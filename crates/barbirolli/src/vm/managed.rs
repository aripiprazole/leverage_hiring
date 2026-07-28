use std::time::Duration;

use derive_more::Debug;
use fctools::{
    process_spawner::DirectProcessSpawner,
    runtime::tokio::TokioRuntime,
    vm::{
        Vm, VmError as FctoolsVmError, VmState,
        configuration::{InitMethod, VmConfiguration, VmConfigurationData},
        models::{BootSource, Drive, MachineConfiguration, NetworkInterface},
        shutdown::{VmShutdownAction, VmShutdownError, VmShutdownMethod},
    },
    vmm::{
        arguments::{VmmApiSocket, VmmArguments},
        executor::unrestricted::UnrestrictedVmmExecutor,
        id::{VmmId, VmmIdError},
        installation::VmmInstallation,
        ownership::VmmOwnershipModel,
        resource::{
            MovedResourceType, ResourceType,
            system::{ResourceSystem, ResourceSystemError},
        },
    },
};
use validated::Validated::{self, Fail, Good};

use crate::{
    Barbirolli, VmSpec,
    network::{ManagedNetwork, NetworkError},
};

type FirecrackerResourceSystem = ResourceSystem<DirectProcessSpawner, TokioRuntime>;
type FirecrackerVm = Vm<UnrestrictedVmmExecutor, DirectProcessSpawner, TokioRuntime>;

#[derive(Debug)]
pub struct ManagedVm {
    pub spec: VmSpec,
    #[debug(skip)]
    pub vm: FirecrackerVm,
    pub network: Option<ManagedNetwork>,
    pub failed: bool,
}

impl VmSpec {
    #[tracing::instrument(
        skip(self, barbirolli),
        fields(vm_id = %self.id, user = %self.user)
    )]
    async fn prepare_vm(&self, barbirolli: &Barbirolli) -> Result<FirecrackerVm, LifecycleError> {
        let mut resources = FirecrackerResourceSystem::with_capacity(
            DirectProcessSpawner,
            TokioRuntime,
            VmmOwnershipModel::Shared,
            2,
        );
        let configuration = vm_configuration(&mut resources, self)?;
        let arguments = VmmArguments::new(VmmApiSocket::Enabled(barbirolli.api_socket.clone()));
        let executor = UnrestrictedVmmExecutor::new(arguments)
            .id(VmmId::new(format!("barbirolli-{}", self.id))?);
        let installation = VmmInstallation::new(
            barbirolli.firecracker.clone(),
            barbirolli.firecracker.clone(),
            barbirolli.firecracker.clone(),
        );

        Ok(FirecrackerVm::prepare(executor, resources, installation, configuration).await?)
    }

    pub(crate) async fn reconcile(&self, barbirolli: &Barbirolli) -> Result<(), LifecycleError> {
        let stale_processes = self
            .kill_stale_processes(barbirolli)
            .await
            .map_err(ReconcileFailure::Processes);
        let stale_network = self
            .network
            .cleanup_stale()
            .await
            .map_err(ReconcileFailure::Network);
        let stale_socket = match std::fs::remove_file(&barbirolli.api_socket) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(ReconcileFailure::Socket(error)),
        };
        let reconciliation = Validated::from(stale_processes).map3(
            Validated::from(stale_network),
            Validated::from(stale_socket),
            |(), (), ()| (),
        );
        match reconciliation {
            Good(()) => Ok(()),
            failures @ Fail(_) => Err(LifecycleError::Reconcile(failures)),
        }
    }

    async fn kill_stale_processes(&self, barbirolli: &Barbirolli) -> Result<(), std::io::Error> {
        let expected_socket = barbirolli.api_socket.as_os_str().as_encoded_bytes();
        let expected_firecracker = barbirolli.firecracker.as_os_str().as_encoded_bytes();
        let mut proc = tokio::fs::read_dir("/proc").await?;
        while let Some(entry) = proc.next_entry().await? {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i32>().ok())
            else {
                continue;
            };
            let command = match tokio::fs::read(entry.path().join("cmdline")).await {
                Ok(command) => command,
                Err(_) => continue,
            };
            let arguments = command
                .split(|byte| *byte == 0)
                .filter(|argument| !argument.is_empty())
                .collect::<Vec<_>>();
            let is_firecracker = arguments
                .first()
                .is_some_and(|argument| *argument == expected_firecracker);
            let has_socket = arguments
                .windows(2)
                .any(|arguments| arguments[0] == b"--api-sock" && arguments[1] == expected_socket);
            if is_firecracker
                && has_socket
                && let Some(pid) = rustix::process::Pid::from_raw(pid)
            {
                rustix::process::kill_process(pid, rustix::process::Signal::KILL)?;
            }
        }
        Ok(())
    }
}

fn vm_configuration(
    resources: &mut FirecrackerResourceSystem,
    spec: &VmSpec,
) -> Result<VmConfiguration, LifecycleError> {
    let kernel = resources.create_resource(
        &spec.kernel,
        ResourceType::Moved(MovedResourceType::HardLinkedOrCopied),
    )?;
    let rootfs = resources.create_resource(
        &spec.rootfs,
        ResourceType::Moved(MovedResourceType::HardLinkedOrCopied),
    )?;

    Ok(VmConfiguration::New {
        init_method: InitMethod::ViaApiCalls,
        data: VmConfigurationData {
            boot_source: BootSource {
                kernel_image: kernel,
                boot_args: Some(spec.network.kernel_boot_args()),
                initrd: None,
            },
            drives: vec![Drive {
                drive_id: "rootfs".into(),
                is_root_device: true,
                cache_type: None,
                partuuid: None,
                is_read_only: Some(false),
                block: Some(rootfs),
                rate_limiter: None,
                io_engine: None,
                socket: None,
            }],
            machine_configuration: MachineConfiguration {
                vcpu_count: spec.vcpu_count.into(),
                mem_size_mib: u16::from(spec.memory_mib).into(),
                smt: None,
                track_dirty_pages: None,
                huge_pages: None,
            },
            cpu_template: None,
            network_interfaces: vec![NetworkInterface {
                iface_id: "eth0".into(),
                host_dev_name: spec.network.tap.as_ref().to_owned(),
                guest_mac: Some(spec.network.guest_mac.to_owned()),
                rx_rate_limiter: None,
                tx_rate_limiter: None,
            }],
            balloon_device: None,
            vsock_device: None,
            logger_system: None,
            metrics_system: None,
            mmds_configuration: None,
            entropy_device: None,
        },
    })
}

async fn rollback_vm(
    vm: &mut FirecrackerVm,
    shutdown_timeout: Duration,
) -> Result<(), LifecycleError> {
    let shutdown = match vm.get_state() {
        VmState::NotStarted | VmState::Exited | VmState::Crashed(_) => Ok(()),
        VmState::Running | VmState::Paused => vm
            .shutdown([VmShutdownAction {
                method: VmShutdownMethod::Kill,
                timeout: Some(shutdown_timeout),
                graceful: false,
            }])
            .await
            .map(|_| ())
            .map_err(LifecycleError::from),
    };
    let cleanup = vm.cleanup().await.map_err(LifecycleError::from);
    match (shutdown, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(shutdown), Err(cleanup)) => Err(LifecycleError::ShutdownCleanup {
            shutdown: Some(Box::new(shutdown)),
            cleanup: Some(Box::new(cleanup)),
        }),
    }
}

impl ManagedVm {
    pub async fn start(spec: VmSpec, barbirolli: &Barbirolli) -> Result<Self, LifecycleError> {
        let network = spec.network.prepare().await?;
        let mut vm = match spec.prepare_vm(barbirolli).await {
            Ok(vm) => vm,
            Err(startup) => {
                return Err(LifecycleError::StartupRollback {
                    startup_error: Box::new(startup),
                    vm_rollback_error: None,
                    network_rollback_error: network.cleanup().await.err().map(Box::new),
                });
            }
        };

        match vm.start(barbirolli.api_socket_timeout).await {
            Ok(()) => Ok(Self {
                spec,
                vm,
                network: Some(network),
                failed: false,
            }),
            Err(error) => Err(LifecycleError::StartupRollback {
                startup_error: Box::new(error.into()),
                vm_rollback_error: rollback_vm(&mut vm, barbirolli.shutdown_timeout)
                    .await
                    .err()
                    .map(Box::new),
                network_rollback_error: network.cleanup().await.err().map(Box::new),
            }),
        }
    }

    pub async fn shutdown(&mut self, shutdown_timeout: Duration) -> Result<(), LifecycleError> {
        if matches!(self.vm.get_state(), VmState::Exited | VmState::Crashed(_)) {
            return Ok(());
        }
        self.vm
            .shutdown([
                VmShutdownAction {
                    method: if cfg!(target_arch = "x86_64") {
                        VmShutdownMethod::CtrlAltDel
                    } else {
                        VmShutdownMethod::WriteToSerial(b"reboot\n".to_vec())
                    },
                    timeout: Some(shutdown_timeout),
                    graceful: true,
                },
                VmShutdownAction {
                    method: VmShutdownMethod::PauseThenKill,
                    timeout: Some(shutdown_timeout / 2),
                    graceful: false,
                },
                VmShutdownAction {
                    method: VmShutdownMethod::Kill,
                    timeout: Some(shutdown_timeout / 2),
                    graceful: false,
                },
            ])
            .await?;
        Ok(())
    }

    pub async fn cleanup(&mut self) -> Result<(), LifecycleError> {
        let vm = self.vm.cleanup().await.map_err(LifecycleError::from);
        let network = match self.network.as_ref() {
            Some(network) => network.cleanup().await,
            None => Ok(()),
        };
        if network.is_ok() {
            self.network = None;
        }

        match (vm, network) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(vm), Ok(())) => Err(vm),
            (Ok(()), Err(network)) => Err(network.into()),
            (Err(vm), Err(network)) => Err(LifecycleError::Cleanup {
                vm: Some(Box::new(vm)),
                network: Some(Box::new(network)),
            }),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    #[error(transparent)]
    Network(#[from] NetworkError),
    #[error(transparent)]
    ResourceSystem(#[from] ResourceSystemError),
    #[error(transparent)]
    VmmId(#[from] VmmIdError),
    #[error(transparent)]
    Vm(#[from] FctoolsVmError),
    #[error(transparent)]
    Shutdown(#[from] VmShutdownError),
    #[error(
        "startup failed: {startup_error}; rollback: {vm_rollback_error:?}; network rollback: {network_rollback_error:?}"
    )]
    StartupRollback {
        startup_error: Box<LifecycleError>,
        vm_rollback_error: Option<Box<LifecycleError>>,
        network_rollback_error: Option<Box<NetworkError>>,
    },
    #[error("VM cleanup failed (VM: {vm:?}, network: {network:?})")]
    Cleanup {
        vm: Option<Box<LifecycleError>>,
        network: Option<Box<NetworkError>>,
    },
    #[error("VM shutdown/cleanup failed (shutdown: {shutdown:?}, cleanup: {cleanup:?})")]
    ShutdownCleanup {
        shutdown: Option<Box<LifecycleError>>,
        cleanup: Option<Box<LifecycleError>>,
    },
    #[error("warmup reconciliation failed: {0:?}")]
    Reconcile(Validated<(), ReconcileFailure>),
    #[error("FIRECRACKER must name a Firecracker 1.13 executable, got {0:?}")]
    UnsupportedFirecracker(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ReconcileFailure {
    #[error("failed to kill stale Firecracker processes")]
    Processes(#[source] std::io::Error),
    #[error("failed to clean up stale network")]
    Network(#[source] NetworkError),
    #[error("failed to remove stale Firecracker API socket")]
    Socket(#[source] std::io::Error),
}
