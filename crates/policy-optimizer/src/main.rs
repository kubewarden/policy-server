// The code dealing with the Lease is based on https://github.com/linkerd/linkerd2/blob/3d601c2ed4382802340c92bcd97a9bae41747958/policy-controller/src/main.rs#L318

use anyhow::{anyhow, Result};
use clap::Parser;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::coordination::v1 as coordv1;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::Resource;
use kube::{api::PatchParams, Client};
use kubert::lease::LeaseManager;
use tokio::time::Duration;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{filter::EnvFilter, fmt};

const LEASE_NAME_PREFIX: &str = "policy-optimizer";
const LEASE_DURATION: Duration = Duration::from_secs(30);
const RENEW_GRACE_PERIOD: Duration = Duration::from_secs(1);

mod cli;

#[tokio::main]
async fn main() -> Result<()> {
    let claimant = std::env::var("HOSTNAME")
        .map_err(|e| anyhow!("Can access `HOSTNAME` environment variable: {e:?}"))?;

    let cli = cli::Cli::parse();
    // setup logging
    let level_filter = cli.log_level;
    let filter_layer = EnvFilter::from_default_env()
        .add_directive(level_filter.into())
        .add_directive("rustls=off".parse().unwrap()) // this crate generates tracing events we don't care about
        .add_directive("hyper=off".parse().unwrap()) // this crate generates tracing events we don't care about
        .add_directive("tower=off".parse().unwrap()); // this crate generates tracing events we don't care about
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt::layer().with_writer(std::io::stderr))
        .init();

    let client = Client::try_default().await?;

    let policy_server_deployment_name =
        format!("policy-server-{}", cli.policy_server_deployment_name);

    let lease_manager = init_lease(
        client.clone(),
        &cli.namespace,
        &policy_server_deployment_name,
    )
    .await?;

    let params = kubert::lease::ClaimParams {
        lease_duration: LEASE_DURATION,
        renew_grace_period: RENEW_GRACE_PERIOD,
    };

    let (mut claims, _task) = lease_manager.spawn(&claimant, params).await?;

    tracing::info!("waiting to be leader");
    claims
        .wait_for(|receiver| receiver.is_current_for(&claimant))
        .await
        .unwrap();

    // TODO: this is some code faking the policy download & optimize process
    // replace it with the actual code later on
    let worker = tokio::spawn(async move {
        tracing::info!("starting job");
        for i in 0..10 {
            tracing::info!("{i} awake");
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });

    worker.await.unwrap();

    Ok(())
}

async fn init_lease(client: Client, ns: &str, deployment_name: &str) -> Result<LeaseManager> {
    // Fetch the policy-server deployment so that we can use it as an owner
    // reference of the Lease.
    let api = kube::Api::<Deployment>::namespaced(client.clone(), ns);
    let deployment = api.get(deployment_name).await?;

    let api = kube::Api::namespaced(client, ns);
    let params = PatchParams {
        field_manager: Some("policy-server".to_string()),
        ..Default::default()
    };

    let lease_name = format!("{LEASE_NAME_PREFIX}-{deployment_name}");

    match api
        .patch(
            &lease_name,
            &params,
            &kube::api::Patch::Apply(coordv1::Lease {
                metadata: ObjectMeta {
                    name: Some(lease_name.clone()),
                    namespace: Some(ns.to_string()),
                    // Specifying a resource version of "0" means that we will
                    // only create the Lease if it does not already exist.
                    resource_version: Some("0".to_string()),
                    owner_references: Some(vec![deployment.controller_owner_ref(&()).unwrap()]),
                    labels: Some(
                        [(
                            "kubewarden.io/policy-server".to_string(),
                            deployment_name.to_string(),
                        )]
                        .into_iter()
                        .collect(),
                    ),
                    ..Default::default()
                },
                spec: None,
            }),
        )
        .await
    {
        Ok(lease) => tracing::info!(?lease, "created Lease resource"),
        Err(kube::Error::Api(_)) => tracing::info!("Lease already exists, no need to create it"),
        Err(error) => {
            tracing::error!(%error, "error creating Lease resource");
            return Err(error.into());
        }
    };
    // Create the lease manager used for trying to claim the policy
    // controller write lease.
    // todo: Do we need to use LeaseManager::field_manager here?
    kubert::lease::LeaseManager::init(api, lease_name)
        .await
        .map_err(Into::into)
}
