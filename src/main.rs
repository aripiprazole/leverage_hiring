use std::{env, error::Error, net::SocketAddr, path::PathBuf, sync::Arc};

use barbirolli::{Barbirolli, VmStore};
use tracing_subscriber::EnvFilter;

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

    let store = VmStore::new(
        required_path("VM_ROOT")?,
        required_path("IMAGE_ROOT")?,
        required_path("DEFAULT_AUTHORIZED_KEYS")?,
    )?;
    let manager = Arc::new(Barbirolli::new(store, required_path("FIRECRACKER")?).await?);

    let elhone_address = elhone::address_from_env()?;
    let elhone_listener = tokio::net::TcpListener::bind(elhone_address).await?;
    let elhone = axum::serve(
        elhone_listener,
        elhone::router(manager.clone(), elhone::Auth::from_env()?),
    );

    let meier_address = env::var("MEIER_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:2222".to_owned())
        .parse::<SocketAddr>()?;
    let meier = meier::serve(manager.clone(), meier_address);

    tracing::info!(
        %elhone_address,
        %meier_address,
        "SSH and HTTP services started"
    );
    let service = tokio::select! {
        result = elhone => result.map_err(|error| error.to_string()),
        result = meier => result.map_err(|error| error.to_string()),
        result = tokio::signal::ctrl_c() => result.map_err(|error| error.to_string()),
    };
    let shutdown = manager
        .shutdown_all()
        .await
        .map_err(|error| error.to_string());

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

fn required_path(name: &'static str) -> Result<PathBuf, Box<dyn Error>> {
    Ok(PathBuf::from(env::var(name).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("required environment variable {name} is not set"),
        )
    })?))
}
