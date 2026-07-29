use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use derive_more::Debug;
use fctools::{
    extension::metrics::Metrics,
    process_spawner::DirectProcessSpawner,
    runtime::tokio::TokioRuntime,
    vm::{
        Vm, VmError as FctoolsVmError, VmState,
        configuration::{InitMethod, VmConfiguration, VmConfigurationData},
        models::{BootSource, Drive, MachineConfiguration, MetricsSystem, NetworkInterface},
        shutdown::{VmShutdownAction, VmShutdownError, VmShutdownMethod},
    },
    vmm::{
        arguments::{VmmApiSocket, VmmArguments},
        executor::unrestricted::UnrestrictedVmmExecutor,
        id::{VmmId, VmmIdError},
        installation::VmmInstallation,
        ownership::VmmOwnershipModel,
        resource::{
            CreatedResourceType, MovedResourceType, ResourceType,
            system::{ResourceSystem, ResourceSystemError},
        },
    },
};
use futures::{FutureExt, future::BoxFuture};
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{Instrument as _, info_span};
use validated::Validated::{self, Fail, Good};

use crate::{
    Barbirolli, VmSpec,
    idle::{IdleTracker, Sample},
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
    pub(crate) pid: i32,
    pub(crate) idle_tracker: Mutex<Option<IdleTracker>>,
    cgroup: Option<VmCgroup>,
    latest_metrics: Arc<RwLock<Option<Metrics>>>,
    #[debug(skip)]
    metrics_task: tokio::task::JoinHandle<()>,
}

#[derive(Debug)]
struct VmCgroup {
    path: PathBuf,
}

impl VmSpec {
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

    #[tracing::instrument(skip(self, barbirolli))]
    pub(crate) fn reconcile<'a>(
        &'a self,
        barbirolli: &'a Barbirolli,
    ) -> BoxFuture<'a, Result<(), LifecycleError>> {
        let vm_id = self.id;
        async move {
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
            let stale_health = self
                .cleanup_stale_health()
                .map_err(ReconcileFailure::Health);
            let reconciliation = Validated::from(stale_processes).map4(
                Validated::from(stale_network),
                Validated::from(stale_socket),
                Validated::from(stale_health),
                |(), (), (), ()| (),
            );
            match reconciliation {
                Good(()) => Ok(()),
                failures @ Fail(_) => Err(LifecycleError::Reconcile(failures)),
            }
        }
        .instrument(info_span!("reconcile_vm", %vm_id, operation = "reconcile"))
        .boxed()
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

    fn metrics_path(&self) -> PathBuf {
        self.artifact_dir.join("metrics.fifo")
    }

    fn cleanup_stale_health(&self) -> Result<(), HealthError> {
        let metrics = self.metrics_path();
        if let Err(source) = fs::remove_file(&metrics)
            && source.kind() != std::io::ErrorKind::NotFound
        {
            return Err(HealthError::Io {
                path: metrics,
                source,
            });
        }

        let cgroup = PathBuf::from(CGROUP_ROOT).join(format!("vm-{}", self.id));
        if cgroup.exists() {
            let kill = cgroup.join("cgroup.kill");
            if kill.exists() {
                fs::write(&kill, "1").map_err(|source| HealthError::Io { path: kill, source })?;
            }
            fs::remove_dir(&cgroup).map_err(|source| HealthError::Io {
                path: cgroup,
                source,
            })?;
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
    let metrics = resources.create_resource(
        spec.metrics_path(),
        ResourceType::Created(CreatedResourceType::Fifo),
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
            metrics_system: Some(MetricsSystem { metrics }),
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
        let network = spec.network.prepare(&spec.port_bindings).await?;
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

        if let Err(error) = vm.start(barbirolli.api_socket_timeout).await {
            return Err(LifecycleError::StartupRollback {
                startup_error: Box::new(error.into()),
                vm_rollback_error: rollback_vm(&mut vm, barbirolli.shutdown_timeout)
                    .await
                    .err()
                    .map(Box::new),
                network_rollback_error: network.cleanup().await.err().map(Box::new),
            });
        }

        let pid = match find_firecracker_pid(barbirolli).await {
            Ok(pid) => pid,
            Err(error) => {
                return Err(LifecycleError::StartupRollback {
                    startup_error: Box::new(error.into()),
                    vm_rollback_error: rollback_vm(&mut vm, barbirolli.shutdown_timeout)
                        .await
                        .err()
                        .map(Box::new),
                    network_rollback_error: network.cleanup().await.err().map(Box::new),
                });
            }
        };
        let cgroup = match VmCgroup::create(spec.id.to_string(), pid) {
            Ok(cgroup) => cgroup,
            Err(error) => {
                return Err(LifecycleError::StartupRollback {
                    startup_error: Box::new(LifecycleError::Health(error)),
                    vm_rollback_error: rollback_vm(&mut vm, barbirolli.shutdown_timeout)
                        .await
                        .err()
                        .map(Box::new),
                    network_rollback_error: network.cleanup().await.err().map(Box::new),
                });
            }
        };
        let latest_metrics = Arc::new(RwLock::new(None));
        let metrics_task = spawn_metrics_reader(spec.metrics_path(), latest_metrics.clone());

        Ok(Self {
            spec,
            vm,
            network: Some(network),
            failed: false,
            pid,
            idle_tracker: Mutex::new(None),
            cgroup: Some(cgroup),
            latest_metrics,
            metrics_task,
        })
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
        self.metrics_task.abort();
        let vm = self.vm.cleanup().await.map_err(LifecycleError::from);
        let network = match self.network.as_ref() {
            Some(network) => network.cleanup().await,
            None => Ok(()),
        };
        if network.is_ok() {
            self.network = None;
        }

        let cgroup = self
            .cgroup
            .take()
            .map_or(Ok(()), |cgroup| cgroup.cleanup())
            .map_err(LifecycleError::Health);

        match (vm, network, cgroup) {
            (Ok(()), Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(()), Ok(())) => Err(error),
            (Ok(()), Err(error), Ok(())) => Err(error.into()),
            (Ok(()), Ok(()), Err(error)) => Err(error),
            (vm, network, cgroup) => Err(LifecycleError::Cleanup {
                vm: vm.err().map(Box::new),
                network: network.err().map(Box::new),
                health: cgroup.err().map(Box::new),
            }),
        }
    }

    pub(crate) fn activity_sample(&self) -> Result<Sample, HealthError> {
        let cgroup = self.cgroup.as_ref().ok_or(HealthError::MissingCgroup)?;
        let metrics = self
            .latest_metrics
            .read()
            .map_err(|_| HealthError::MetricsLock)?
            .clone()
            .ok_or(HealthError::MissingMetrics)?;
        let connections = connection_counts(self.spec.network.guest_ip);
        Ok(Sample {
            instance_pid: self.pid,
            sampled_at: Instant::now(),
            cpu_usage_usec: cgroup.read_key("cpu.stat", "usage_usec")?,
            memory_bytes: cgroup.read_value("memory.current")?,
            process_count: cgroup.read_value("pids.current")?,
            network_bytes: metrics
                .net
                .rx_bytes_count
                .saturating_add(metrics.net.tx_bytes_count),
            disk_bytes: metrics
                .block
                .read_bytes
                .saturating_add(metrics.block.write_bytes),
            established_tcp_connections: connections.1,
            total_connections: connections.0,
            vcpu_count: self.spec.vcpu_count.into(),
        })
    }
}

const CGROUP_ROOT: &str = "/sys/fs/cgroup/barbirolli";

fn spawn_metrics_reader(
    path: PathBuf,
    latest: Arc<RwLock<Option<Metrics>>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(error) = read_metrics(&path, &latest).await {
            tracing::error!(%error, path = %path.display(), "Firecracker metrics reader stopped");
        }
    })
}

async fn read_metrics(path: &PathBuf, latest: &RwLock<Option<Metrics>>) -> Result<(), HealthError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|source| HealthError::Io {
            path: path.clone(),
            source,
        })?;
    let mut lines = BufReader::new(file).lines();
    while let Some(line) = lines.next_line().await.map_err(|source| HealthError::Io {
        path: path.clone(),
        source,
    })? {
        let metrics = match serde_json::from_str(&line) {
            Ok(metrics) => metrics,
            Err(error) => {
                tracing::warn!(%error, "discarding invalid Firecracker metrics");
                continue;
            }
        };
        *latest.write().map_err(|_| HealthError::MetricsLock)? = Some(metrics);
    }
    Ok(())
}

async fn find_firecracker_pid(barbirolli: &Barbirolli) -> Result<i32, HealthError> {
    let expected_socket = barbirolli.api_socket.as_os_str().as_encoded_bytes();
    let expected_firecracker = barbirolli.firecracker.as_os_str().as_encoded_bytes();
    let mut proc = tokio::fs::read_dir("/proc")
        .await
        .map_err(|source| HealthError::Io {
            path: PathBuf::from("/proc"),
            source,
        })?;
    while let Some(entry) = proc.next_entry().await.map_err(|source| HealthError::Io {
        path: PathBuf::from("/proc"),
        source,
    })? {
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
        if arguments
            .first()
            .is_some_and(|arg| *arg == expected_firecracker)
            && arguments
                .windows(2)
                .any(|args| args[0] == b"--api-sock" && args[1] == expected_socket)
        {
            return Ok(pid);
        }
    }
    Err(HealthError::ProcessNotFound)
}

impl VmCgroup {
    fn create(id: String, pid: i32) -> Result<Self, HealthError> {
        let root = PathBuf::from(CGROUP_ROOT);
        fs::create_dir_all(&root).map_err(|source| HealthError::Io {
            path: root.clone(),
            source,
        })?;
        let controllers = root.join("cgroup.subtree_control");
        fs::write(&controllers, "+cpu +memory +pids").map_err(|source| HealthError::Io {
            path: controllers,
            source,
        })?;
        let path = root.join(format!("vm-{id}"));
        fs::create_dir(&path).map_err(|source| HealthError::Io {
            path: path.clone(),
            source,
        })?;
        let procs = path.join("cgroup.procs");
        if let Err(source) = fs::write(&procs, pid.to_string()) {
            let setup = HealthError::Io {
                path: procs,
                source,
            };
            return match fs::remove_dir(&path) {
                Ok(()) => Err(setup),
                Err(source) => Err(HealthError::CgroupSetupRollback {
                    setup: Box::new(setup),
                    rollback: Box::new(HealthError::Io { path, source }),
                }),
            };
        }
        Ok(Self { path })
    }

    fn read_value(&self, name: &str) -> Result<u64, HealthError> {
        let path = self.path.join(name);
        let value = fs::read_to_string(&path).map_err(|source| HealthError::Io {
            path: path.clone(),
            source,
        })?;
        value
            .trim()
            .parse()
            .map_err(|_| HealthError::InvalidValue(path))
    }

    fn read_key(&self, name: &str, key: &str) -> Result<u64, HealthError> {
        let path = self.path.join(name);
        let value = fs::read_to_string(&path).map_err(|source| HealthError::Io {
            path: path.clone(),
            source,
        })?;
        value
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(' ')?;
                (name == key).then(|| value.parse().ok()).flatten()
            })
            .ok_or(HealthError::InvalidValue(path))
    }

    fn cleanup(self) -> Result<(), HealthError> {
        match fs::remove_dir(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(HealthError::Io {
                path: self.path,
                source,
            }),
        }
    }
}

fn connection_counts(guest_ip: std::net::Ipv4Addr) -> (u64, u64) {
    let Some(contents) = ["/proc/net/nf_conntrack", "/proc/net/ip_conntrack"]
        .into_iter()
        .find_map(|path| fs::read_to_string(path).ok())
    else {
        return (0, 0);
    };
    let address = guest_ip.to_string();
    contents
        .lines()
        .filter(|line| {
            line.split_ascii_whitespace().any(|field| {
                field
                    .strip_prefix("src=")
                    .or_else(|| field.strip_prefix("dst="))
                    .is_some_and(|ip| ip == address)
            })
        })
        .fold((0, 0), |(total, established), line| {
            (
                total + 1,
                established + u64::from(line.split_ascii_whitespace().any(|v| v == "ESTABLISHED")),
            )
        })
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
    #[error("VM cleanup failed (VM: {vm:?}, network: {network:?}, health: {health:?})")]
    Cleanup {
        vm: Option<Box<LifecycleError>>,
        network: Option<Box<NetworkError>>,
        health: Option<Box<LifecycleError>>,
    },
    #[error("VM shutdown/cleanup failed (shutdown: {shutdown:?}, cleanup: {cleanup:?})")]
    ShutdownCleanup {
        shutdown: Option<Box<LifecycleError>>,
        cleanup: Option<Box<LifecycleError>>,
    },
    #[error("warmup reconciliation failed: {0:?}")]
    Reconcile(Validated<(), ReconcileFailure>),
    #[error(transparent)]
    Health(#[from] HealthError),
    #[error("FIRECRACKER must name a Firecracker 1.13 executable, got {0:?}")]
    UnsupportedFirecracker(String),
}

#[derive(Debug, thiserror::Error)]
pub enum HealthError {
    #[error("failed to access health resource {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid health value in {0}")]
    InvalidValue(PathBuf),
    #[error("the Firecracker process could not be identified")]
    ProcessNotFound,
    #[error("the VM cgroup is unavailable")]
    MissingCgroup,
    #[error("Firecracker has not emitted a metrics sample")]
    MissingMetrics,
    #[error("the Firecracker metrics lock was poisoned")]
    MetricsLock,
    #[error("the VM idle tracker lock was poisoned")]
    TrackerLock,
    #[error("cgroup setup failed: {setup}; rollback also failed: {rollback}")]
    CgroupSetupRollback {
        setup: Box<HealthError>,
        rollback: Box<HealthError>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ReconcileFailure {
    #[error("failed to kill stale Firecracker processes")]
    Processes(#[source] std::io::Error),
    #[error("failed to clean up stale network")]
    Network(#[source] NetworkError),
    #[error("failed to remove stale Firecracker API socket")]
    Socket(#[source] std::io::Error),
    #[error("failed to clean up stale health resources")]
    Health(#[source] HealthError),
}
