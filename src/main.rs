use std::{env, error::Error, net::SocketAddr, path::PathBuf, time::Duration};

use barbirolli::{Barbirolli, DaemonConfig, IdlePolicy, ProvisioningConfig, VmStore};
use serde::Deserialize;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[derive(Deserialize)]
struct Config {
    vm_root: PathBuf,
    image_root: PathBuf,
    firecracker: PathBuf,
    #[serde(default = "default_elhone_address")]
    elhone_addr: SocketAddr,
    #[serde(default = "default_idle_initial_interval")]
    idle_initial_interval_seconds: u64,
    #[serde(default = "default_idle_strike_interval")]
    idle_strike_interval_seconds: u64,
    #[serde(default = "default_idle_final_interval")]
    idle_final_interval_seconds: u64,
    #[serde(default = "default_idle_cpu_high")]
    idle_cpu_high_percent: f64,
    #[serde(default = "default_idle_cpu_low")]
    idle_cpu_low_percent: f64,
}

fn default_elhone_address() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 3000))
}

fn default_idle_initial_interval() -> u64 {
    300
}
fn default_idle_strike_interval() -> u64 {
    60
}
fn default_idle_final_interval() -> u64 {
    30
}
fn default_idle_cpu_high() -> f64 {
    0.5
}
fn default_idle_cpu_low() -> f64 {
    3.0
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            env::var("RUST_LOG")
                .ok()
                .and_then(|directives| EnvFilter::try_new(directives).ok())
                .unwrap_or_else(|| EnvFilter::new("info")),
        )
        .init();

    let config = envy::from_env::<Config>()?;
    let daemon = Barbirolli::new(
        VmStore::new(config.vm_root, config.image_root)?,
        DaemonConfig {
            firecracker: config.firecracker,
            provisioning: ProvisioningConfig::default(),
            idle_policy: Some(IdlePolicy {
                initial_interval: Duration::from_secs(config.idle_initial_interval_seconds),
                strike_interval: Duration::from_secs(config.idle_strike_interval_seconds),
                final_interval: Duration::from_secs(config.idle_final_interval_seconds),
                cpu_idle_high_percent: config.idle_cpu_high_percent,
                cpu_active_low_percent: config.idle_cpu_low_percent,
            }),
        },
    )
    .await?;

    let listener = TcpListener::bind(config.elhone_addr).await?;
    let elhone = axum::serve(listener, elhone::router(daemon.clone()));

    tracing::info!(addr = %config.elhone_addr, "HTTP service started");
    let service = tokio::select! {
        result = elhone => result.map_err(|error| error.to_string()),
        result = tokio::signal::ctrl_c() => result.map_err(|error| error.to_string()),
        () = daemon.autoscale() => Ok(()),
    };
    let shutdown = daemon.shutdown().await.map_err(|error| error.to_string());

    match (service, shutdown) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(service), Ok(())) => Err(std::io::Error::other(service).into()),
        (Ok(()), Err(shutdown)) => Err(std::io::Error::other(shutdown).into()),
        (Err(service), Err(shutdown)) => Err(std::io::Error::other(format!(
            "service failed: {service}; shutdown failed: {shutdown}"
        ))
        .into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_deserializes_environment_variables() {
        let config = envy::from_iter::<_, Config>([
            ("VM_ROOT".to_owned(), "/var/lib/vms".to_owned()),
            ("IMAGE_ROOT".to_owned(), "/var/lib/images".to_owned()),
            (
                "FIRECRACKER".to_owned(),
                "/usr/local/bin/firecracker".to_owned(),
            ),
            ("ELHONE_ADDR".to_owned(), "127.0.0.1:4000".to_owned()),
        ])
        .expect("configuration should deserialize");

        assert_eq!(config.vm_root, PathBuf::from("/var/lib/vms"));
        assert_eq!(config.image_root, PathBuf::from("/var/lib/images"));
        assert_eq!(
            config.firecracker,
            PathBuf::from("/usr/local/bin/firecracker")
        );
        assert_eq!(
            config.elhone_addr,
            "127.0.0.1:4000".parse().expect("valid Elhone address")
        );
        assert_eq!(config.idle_initial_interval_seconds, 300);
        assert_eq!(config.idle_strike_interval_seconds, 60);
        assert_eq!(config.idle_final_interval_seconds, 30);
        assert!((config.idle_cpu_high_percent - 0.5).abs() < f64::EPSILON);
        assert!((config.idle_cpu_low_percent - 3.0).abs() < f64::EPSILON);
    }
}
