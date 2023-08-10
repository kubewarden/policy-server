use clap::builder::TypedValueParser;
use clap::Parser;
use tracing_subscriber::filter::LevelFilter;

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
pub struct Cli {
    // TODO: use the same logging flags as policy-server
    /// Log level
    #[arg(
        long,
        env = "LOG_LEVEL",
        default_value_t = LevelFilter::INFO,
        value_parser = clap::builder::PossibleValuesParser::new(["trace", "debug", "info", "warn", "error"])
            .map(|s| s.parse::<LevelFilter>().unwrap()),
    )]
    pub log_level: LevelFilter,

    /// Policy Server Deployment name
    #[clap(long, env = "KUBEWARDEN_POLICY_SERVER_DEPLOYMENT_NAME")]
    pub policy_server_deployment_name: String,

    /// Kubernetes Namespace where the container is running
    #[clap(long, env = "NAMESPACE")]
    pub namespace: String,

    /// YAML file holding the policies to be loaded and their settings
    #[clap(long, env = "KUBEWARDEN_POLICIES")]
    pub policies: String,

    /// Download path for the policies
    #[clap(long, env = "KUBEWARDEN_POLICIES_DOWNLOAD_DIR")]
    pub policies_download_dir: String,
}
