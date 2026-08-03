//! Feature-gated Linux daemon runner used by the hidden `meier __daemon` mode.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use crate::{
    MeierError,
    config::{self, Config},
};

/// The small, feature-gated settings surface shared by the daemon mode and
/// compatibility launchers. The public CLI keeps that mode hidden behind the
/// feature-enabled executable.
#[derive(Debug, Clone)]
pub struct DaemonSettings {
    pub vm_root: PathBuf,
    pub image_root: PathBuf,
    pub firecracker: PathBuf,
    pub barbirolli_entrypoint: PathBuf,
    pub listen: SocketAddr,
    pub idle_initial_interval: Duration,
    pub idle_strike_interval: Duration,
    pub idle_final_interval: Duration,
    pub idle_cpu_high_percent: f64,
    pub idle_cpu_low_percent: f64,
}

impl Config {
    pub fn daemon_settings(&self) -> Result<DaemonSettings, MeierError> {
        let paths = config::runtime_paths(self)?;
        Ok(DaemonSettings {
            vm_root: paths.vms,
            image_root: paths.images,
            firecracker: paths.firecracker,
            barbirolli_entrypoint: paths.entrypoint,
            listen: self.daemon.listen,
            idle_initial_interval: Duration::from_secs(300),
            idle_strike_interval: Duration::from_secs(60),
            idle_final_interval: Duration::from_secs(30),
            idle_cpu_high_percent: 0.5,
            idle_cpu_low_percent: 3.0,
        })
    }
}

/// Run the daemon using a previously loaded configuration.
pub async fn run(config: Config) -> Result<(), MeierError> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = config;
        Err(MeierError::UnsupportedPlatform(
            "the Meier daemon must execute inside the Linux Lima guest".to_owned(),
        ))
    }

    #[cfg(target_os = "linux")]
    {
        run_with_settings(config.daemon_settings()?).await
    }
}

/// Run the daemon with settings supplied by a caller that retains a
/// compatibility configuration format, such as the root `ssh` binary.
#[tracing::instrument(skip(settings), fields(addr = %settings.listen), err)]
pub async fn run_with_settings(settings: DaemonSettings) -> Result<(), MeierError> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = settings;
        Err(MeierError::UnsupportedPlatform(
            "the Meier daemon must execute inside the Linux Lima guest".to_owned(),
        ))
    }

    #[cfg(target_os = "linux")]
    {
        tracing::info!(addr = %settings.listen, "initializing Meier daemon");
        let standard_rootfs =
            barbirolli::Rootfs::from(settings.image_root.join("ubuntu-24.04.ext4"));
        let store = barbirolli::VmStore::new(settings.vm_root.clone(), settings.image_root.clone())
            .map_err(|error| MeierError::Setup(error.to_string()))?;
        let daemon = barbirolli::Barbirolli::new(
            store,
            barbirolli::DaemonConfig {
                firecracker: settings.firecracker,
                entrypoint: settings.barbirolli_entrypoint,
                provisioning: barbirolli::ProvisioningConfig::default(),
                idle_policy: Some(barbirolli::IdlePolicy {
                    initial_interval: settings.idle_initial_interval,
                    strike_interval: settings.idle_strike_interval,
                    final_interval: settings.idle_final_interval,
                    cpu_idle_high_percent: settings.idle_cpu_high_percent,
                    cpu_active_low_percent: settings.idle_cpu_low_percent,
                }),
            },
        )
        .await
        .map_err(|error| MeierError::Setup(error.to_string()))?;

        let listener = tokio::net::TcpListener::bind(settings.listen)
            .await
            .map_err(|error| {
                MeierError::Setup(format!("could not bind {}: {error}", settings.listen))
            })?;
        let server = axum::serve(listener, elhone::router(daemon.clone(), standard_rootfs));
        tracing::info!(addr = %settings.listen, "Meier HTTP service started");
        let service = tokio::select! {
            result = server => result.map_err(|error| error.to_string()),
            result = tokio::signal::ctrl_c() => result.map_err(|error| error.to_string()),
            () = daemon.autoscale() => Ok(()),
        };
        let shutdown = daemon.shutdown().await.map_err(|error| error.to_string());
        match (service, shutdown) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(service), Ok(())) => Err(MeierError::Setup(format!("service failed: {service}"))),
            (Ok(()), Err(shutdown)) => {
                Err(MeierError::Setup(format!("shutdown failed: {shutdown}")))
            }
            (Err(service), Err(shutdown)) => Err(MeierError::Setup(format!(
                "service failed: {service}; shutdown failed: {shutdown}"
            ))),
        }
    }
}

/// Load the launcher-selected configuration and run the daemon in the
/// foreground. This is called by the hidden command in the same executable as
/// the public client commands.
pub async fn run_from_cli(
    config_path: PathBuf,
    runtime_dir: Option<PathBuf>,
) -> Result<(), MeierError> {
    let mut config = config::load(&config_path)?;
    if let Some(runtime_dir) = runtime_dir {
        config.daemon.runtime_dir = runtime_dir.display().to_string();
    }
    run(config).await
}
