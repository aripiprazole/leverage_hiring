#![doc = "Cross-platform command-line access to the Barbirolli VM service."]

pub mod cli;
mod client;
pub mod config;
mod error;
mod lima;
mod setup;

#[cfg(feature = "daemon")]
pub mod daemon;

pub use error::MeierError;

/// Run the command-line application.
pub async fn run() -> Result<(), MeierError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .try_init()
        .ok();

    let cli = cli::parse_args()?;
    let config_path = cli.config.clone().unwrap_or_else(config::default_path);

    #[cfg(feature = "daemon")]
    if let Some(args) = cli.internal_daemon_args() {
        return daemon::run_from_cli(config_path, args.runtime_dir.clone()).await;
    }

    if cli.is_bootstrap_command() {
        return cli.dispatch(&config_path).await;
    }

    let config = config::load(&config_path)?;
    cli.dispatch_with_config(&config_path, &config).await
}
