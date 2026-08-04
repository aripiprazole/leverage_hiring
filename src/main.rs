use std::{env, error::Error, net::SocketAddr, path::PathBuf, time::Duration};

use meier::daemon::{DaemonSettings, run_with_settings};
use serde::Deserialize;
use tracing_subscriber::EnvFilter;

#[derive(Deserialize)]
struct Config {
    vm_root: PathBuf,
    image_root: PathBuf,
    firecracker: PathBuf,
    barbirolli_entrypoint: PathBuf,
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
    let filter = match env::var("RUST_LOG") {
        Ok(directives) => match EnvFilter::try_new(directives) {
            Ok(filter) => filter,
            Err(error) => {
                eprintln!("invalid RUST_LOG filter: {error}; using info");
                EnvFilter::new("info")
            }
        },
        Err(env::VarError::NotPresent) => EnvFilter::new("info"),
        Err(error) => {
            eprintln!("could not read RUST_LOG: {error}; using info");
            EnvFilter::new("info")
        }
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let config = envy::from_env::<Config>()?;
    run_with_settings(DaemonSettings {
        vm_root: config.vm_root,
        image_root: config.image_root,
        firecracker: config.firecracker,
        barbirolli_entrypoint: config.barbirolli_entrypoint,
        listen: config.elhone_addr,
        idle_initial_interval: Duration::from_secs(config.idle_initial_interval_seconds),
        idle_strike_interval: Duration::from_secs(config.idle_strike_interval_seconds),
        idle_final_interval: Duration::from_secs(config.idle_final_interval_seconds),
        idle_cpu_high_percent: config.idle_cpu_high_percent,
        idle_cpu_low_percent: config.idle_cpu_low_percent,
    })
    .await?;
    Ok(())
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
            (
                "BARBIROLLI_ENTRYPOINT".to_owned(),
                "/usr/local/bin/barbirolli_entrypoint".to_owned(),
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
            config.barbirolli_entrypoint,
            PathBuf::from("/usr/local/bin/barbirolli_entrypoint")
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
