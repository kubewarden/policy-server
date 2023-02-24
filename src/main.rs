extern crate k8s_openapi;
extern crate policy_evaluator;

use anyhow::{anyhow, Result};
use lazy_static::lazy_static;
use opentelemetry::global::shutdown_tracer_provider;
use policy_evaluator::policy_fetcher::sigstore;
use policy_evaluator::policy_fetcher::verify::FulcioAndRekorData;
use policy_evaluator::{callback_handler::CallbackHandlerBuilder, kube};
use std::{fs, path::PathBuf, process, sync::RwLock, thread};
use tokio::{runtime::Runtime, sync::mpsc, sync::oneshot};
use tracing::{debug, error, info};

mod admission_review;
mod api;
mod cli;
mod metrics;
mod server;
mod settings;
mod worker;

mod policy_downloader;
use policy_downloader::Downloader;

mod worker_pool;
use worker_pool::WorkerPool;

mod communication;
use communication::{EvalRequest, WorkerPoolBootRequest};

lazy_static! {
    static ref TRACE_SYSTEM_INITIALIZED: RwLock<bool> = RwLock::new(false);
}

fn main() -> Result<()> {
    let matches = cli::build_cli().get_matches();

    // init some variables based on the cli parameters
    let addr = cli::api_bind_address(&matches)?;
    let (cert_file, key_file) = cli::tls_files(&matches)?;
    let policies = cli::policies(&matches)?;
    let sources = cli::remote_server_options(&matches)?;
    let pool_size = matches
        .get_one::<String>("workers")
        .map_or_else(num_cpus::get, |v| {
            v.parse::<usize>()
                .expect("error parsing the number of workers")
        });
    let always_accept_admission_reviews_on_namespace = matches
        .get_one::<String>("always-accept-admission-reviews-on-namespace")
        .map(|s| s.to_owned());

    let metrics_enabled = matches.contains_id("enable-metrics");
    let ignore_kubernetes_connection_failure =
        matches.contains_id("ignore-kubernetes-connection-failure");
    let verification_config = cli::verification_config(&matches).unwrap_or_else(|e| {
        fatal_error(format!("Cannot create sigstore verification config: {e:?}"));
        unreachable!()
    });
    let sigstore_cache_dir = matches
        .get_one::<String>("sigstore-cache-dir")
        .map(PathBuf::from)
        .expect("This should not happen, there's a default value for sigstore-cache-dir");

    let policy_evaluation_limit = if matches.contains_id("disable-timeout-protection") {
        None
    } else {
        match matches
            .get_one::<String>("policy-timeout")
            .expect("policy-timeout should always be set")
            .parse::<u64>()
        {
            Ok(v) => Some(v),
            Err(e) => {
                fatal_error(format!(
                    "'policy-timeout' value cannot be converted to unsigned int: {e}"
                ));
                unreachable!()
            }
        }
    };

    // Run in daemon mode if specified by the user
    if matches.contains_id("daemon") {
        println!("Running instance as a daemon");

        let mut daemonize = daemonize::Daemonize::new().pid_file(
            matches
                .get_one::<String>("daemon-pid-file")
                .expect("pid-file should always have a value"),
        );
        if let Some(stdout_file) = matches.get_one::<String>("daemon-stdout-file") {
            let file = fs::File::create(stdout_file)
                .map_err(|e| anyhow!("Cannot create file for daemon stdout: {}", e))?;
            daemonize = daemonize.stdout(file);
        }
        if let Some(stderr_file) = matches.get_one::<String>("daemon-stderr-file") {
            let file = fs::File::create(stderr_file)
                .map_err(|e| anyhow!("Cannot create file for daemon stderr: {}", e))?;
            daemonize = daemonize.stderr(file);
        }
        match daemonize.start() {
            Ok(_) => println!("Detached from shell, now running in background."),
            Err(e) => fatal_error(format!("Something went wrong while daemonizing: {e}")),
        }
    }

    ////////////////////////////////////////////////////////////////////////////
    //                                                                        //
    // Phase 1: setup the CallbackHandler. This is used by the synchronous    //
    // world (the waPC host_callback) to request the execution of code that   //
    // can be run only inside of asynchronous world.                          //
    // An example of that, is a policy that changes the container image       //
    // references to ensure they use immutable shasum instead of tags.   The  //
    // act of retrieving the container image manifest digest requires a       //
    // network request, which is fulfilled using asynchronous code.           //
    //                                                                        //
    // The communication between the two worlds happens via tokio channels.   //
    //                                                                        //
    ////////////////////////////////////////////////////////////////////////////

    // This is a channel used to stop the tokio task that is run
    // inside of the CallbackHandler
    let (callback_handler_shutdown_channel_tx, callback_handler_shutdown_channel_rx) =
        oneshot::channel();

    let fulcio_and_rekor_data = match sigstore::tuf::SigstoreRepository::fetch(None) {
        Ok(repo) => Some(FulcioAndRekorData::FromTufRepository { repo }),
        Err(e) => {
            // We cannot rely on `tracing` yet, because the tracing system has not
            // been initialized, this has to be done inside of an async block, which
            // we cannot use yet
            eprintln!("Cannot fetch TUF repository: {e:?}");
            eprintln!("Sigstore Verifier created without Fulcio data: keyless signatures are going to be discarded because they cannot be verified");
            eprintln!(
                "Sigstore Verifier created without Rekor data: transparency log data won't be used"
            );
            eprintln!("Sigstore capabilities are going to be limited");
            None
        }
    };

    let mut callback_handler_builder =
        CallbackHandlerBuilder::new(callback_handler_shutdown_channel_rx)
            .registry_config(sources.clone())
            .fulcio_and_rekor_data(fulcio_and_rekor_data.as_ref());

    // Attempt to build kube::Client instance, this unfortunately needs an async context
    // for a really limited amount of time.
    //
    // Important: the tokio runtime used to create the `kube::Client` **must**
    // be the very same one used later on when the client is used.
    let rt = match Runtime::new() {
        Ok(r) => r,
        Err(error) => {
            fatal_error(format!("error initializing tokio runtime: {error}"));
            unreachable!();
        }
    };

    let kube_client: Option<kube::Client> = rt.block_on(async {
        match kube::Client::try_default().await {
            Ok(client) => Some(client),
            Err(e) => {
                // We cannot rely on `tracing` yet, because the tracing system has not
                // been initialized yet
                eprintln!("Cannot connect to Kubernetes cluster: {e}");
                None
            }
        }
    });

    match kube_client {
        Some(client) => {
            callback_handler_builder = callback_handler_builder.kube_client(client);
        }
        None => {
            if ignore_kubernetes_connection_failure {
                // We cannot rely on `tracing` yet, because the tracing system has not
                // been initialized yet
                eprintln!(
                    "Cannot connect to Kubernetes, context aware policies will not work properly"
                );
            } else {
                return Err(anyhow!(
                    "Cannot connect to Kubernetes, context aware policies would not work properly"
                ));
            }
        }
    };

    let mut callback_handler = callback_handler_builder.build()?;
    let callback_sender_channel = callback_handler.sender_channel();

    ////////////////////////////////////////////////////////////////////////////
    //                                                                        //
    // Phase 2: setup the Wasm worker pool, this "lives" inside of a          //
    // dedicated system thread.                                               //
    //                                                                        //
    // The communication between the "synchronous world" (aka the Wasm worker //
    // pool) and the "asynchronous world" (aka the http server) happens via   //
    // tokio channels.                                                        //
    //                                                                        //
    ////////////////////////////////////////////////////////////////////////////

    // This is the channel used by the http server to communicate with the
    // Wasm workers
    let (api_tx, api_rx) = mpsc::channel::<EvalRequest>(32);

    // This is the channel used to have the asynchronous code trigger the
    // bootstrap of the worker pool. The bootstrap must be triggered
    // from within the asynchronous code because some of the tracing collectors
    // (e.g. OpenTelemetry) require a tokio::Runtime to be available.
    let (worker_pool_bootstrap_req_tx, worker_pool_bootstrap_req_rx) =
        oneshot::channel::<WorkerPoolBootRequest>();

    // Spawn the system thread that runs the main loop of the worker pool manager
    let wasm_thread = thread::spawn(move || {
        let worker_pool = WorkerPool::new(
            worker_pool_bootstrap_req_rx,
            api_rx,
            callback_sender_channel,
            always_accept_admission_reviews_on_namespace,
            policy_evaluation_limit,
        );
        worker_pool.run();
    });

    ////////////////////////////////////////////////////////////////////////////
    //                                                                        //
    // Phase 3: setup the asynchronous world.                                 //
    //                                                                        //
    // We setup the tokio Runtime manually, instead of relying on the the     //
    // `tokio::main` macro, because we don't want the "synchronous" world to  //
    // be spawned inside of one of the threads owned by the runtime.          //
    //                                                                        //
    ////////////////////////////////////////////////////////////////////////////

    rt.block_on(async {
        // Setup the tracing system. This MUST be done inside of a tokio Runtime
        // because some collectors rely on it and would panic otherwise.
        match cli::setup_tracing(&matches) {
            Err(err) => {
                fatal_error(err.to_string());
                unreachable!();
            }
            Ok(_) => {
                debug!("tracing system ready");
                let mut w = TRACE_SYSTEM_INITIALIZED.write().unwrap();
                *w = true;
            }
        };

        // The unused variable is required so the meter is not dropped early and
        // lives for the whole block lifetime, exporting metrics
        let _meter = if metrics_enabled {
            Some(metrics::init_meter())
        } else {
            None
        };

        // Download policies
        let mut downloader = match Downloader::new(
            sources,
            verification_config.is_some(),
            Some(sigstore_cache_dir),
        )
        .await
        {
            Ok(d) => d,
            Err(e) => {
                fatal_error(e.to_string());
                unreachable!()
            }
        };

        let policies_download_dir = matches.get_one::<String>("policies-download-dir").unwrap();
        let fetched_policies = match downloader
            .download_policies(
                &policies,
                policies_download_dir,
                verification_config.as_ref(),
            )
            .await
        {
            Ok(fp) => fp,
            Err(e) => {
                fatal_error(e.to_string());
                unreachable!()
            }
        };

        // Spawn the tokio task used by the CallbackHandler
        let callback_handle = tokio::spawn(async move {
            info!(status = "init", "CallbackHandler task");
            callback_handler.loop_eval().await;
            info!(status = "exit", "CallbackHandler task");
        });

        // Bootstrap the worker pool
        info!(status = "init", "worker pool bootstrap");
        let (worker_pool_bootstrap_res_tx, mut worker_pool_bootstrap_res_rx) =
            oneshot::channel::<Result<()>>();
        let bootstrap_data = WorkerPoolBootRequest {
            policies,
            fetched_policies,
            pool_size,
            resp_chan: worker_pool_bootstrap_res_tx,
        };
        if worker_pool_bootstrap_req_tx.send(bootstrap_data).is_err() {
            fatal_error("Cannot send bootstrap data to worker pool".to_string());
        }

        // Wait for the worker pool to be fully bootstraped before moving on.
        //
        // It's really important to NOT start the web server before the workers are ready.
        // Our Kubernetes deployment exposes a readiness probe that relies on the web server
        // to be listening. The API server will start hitting the policy server as soon as the
        // readiness probe marks the instance as ready.
        // We don't want Kubernetes API server to send admission reviews before ALL the workers
        // are ready.
        loop {
            match worker_pool_bootstrap_res_rx.try_recv() {
                Ok(res) => match res {
                    Ok(_) => break,
                    Err(e) => fatal_error(e.to_string()),
                },
                Err(oneshot::error::TryRecvError::Empty) => {
                    // the channel is empty, keep waiting
                }
                _ => {
                    fatal_error("Cannot receive worker pool bootstrap result".to_string());
                    return;
                }
            }
        }
        info!(status = "done", "worker pool bootstrap");

        // All is good, we can start listening for incoming requests through the
        // web server
        let tls_config = if cert_file.is_empty() {
            None
        } else {
            Some(crate::server::TlsConfig {
                cert_file: cert_file.to_string(),
                key_file: key_file.to_string(),
            })
        };
        server::run_server(&addr, tls_config, api_tx).await;

        // The evaluation is done, we can shutdown the tokio task that is running
        // the CallbackHandler
        if callback_handler_shutdown_channel_tx.send(()).is_err() {
            error!("Cannot shut down the CallbackHandler task");
        } else if let Err(e) = callback_handle.await {
            error!(
                error = e.to_string().as_str(),
                "Error waiting for the CallbackHandler task"
            );
        }
    });

    if let Err(e) = wasm_thread.join() {
        fatal_error(format!("error while waiting for worker threads: {e:?}"));
    };

    Ok(())
}

fn fatal_error(msg: String) {
    let trace_system_ready = TRACE_SYSTEM_INITIALIZED.read().unwrap();
    if *trace_system_ready {
        error!("{}", msg);
        shutdown_tracer_provider();
    } else {
        eprintln!("{msg}");
    }

    process::exit(1);
}
