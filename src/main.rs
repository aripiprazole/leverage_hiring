use std::{env, error::Error, net::SocketAddr, path::PathBuf, sync::Arc};

use barbirolli::{Barbirolli, VmStore};
use serde::Deserialize;
use tracing_subscriber::EnvFilter;

#[derive(Deserialize)]
struct Config {
    vm_root: PathBuf,
    image_root: PathBuf,
    default_authorized_keys: PathBuf,
    firecracker: PathBuf,
    #[serde(default = "default_elhone_address")]
    elhone_addr: SocketAddr,
}

fn default_elhone_address() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 3000))
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
    let store = VmStore::new(
        config.vm_root,
        config.image_root,
        config.default_authorized_keys,
    )?;
    let manager = Arc::new(Barbirolli::new(store, config.firecracker).await?);

    let elhone_address = elhone::validate_address(config.elhone_addr)?;
    let elhone_listener = tokio::net::TcpListener::bind(elhone_address).await?;
    let elhone = axum::serve(
        elhone_listener,
        elhone::router(manager.clone(), elhone::Auth::from_env()?),
    );

    tracing::info!(%elhone_address, "HTTP service started");
    let service = tokio::select! {
        result = elhone => result.map_err(|error| error.to_string()),
        result = tokio::signal::ctrl_c() => result.map_err(|error| error.to_string()),
    };
    let shutdown = manager.shutdown().await.map_err(|error| error.to_string());

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
                "DEFAULT_AUTHORIZED_KEYS".to_owned(),
                "/etc/ssh/authorized_keys".to_owned(),
            ),
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
            config.default_authorized_keys,
            PathBuf::from("/etc/ssh/authorized_keys")
        );
        assert_eq!(
            config.firecracker,
            PathBuf::from("/usr/local/bin/firecracker")
        );
        assert_eq!(
            config.elhone_addr,
            "127.0.0.1:4000".parse().expect("valid Elhone address")
        );
    }
}
