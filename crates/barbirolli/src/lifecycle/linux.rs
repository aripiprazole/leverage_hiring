use super::{
    BalloonConfig, BalloonStatistics, LifecycleError, Result, VmStatus, VmSummary, WarmupFailure,
};

use std::{
    fmt::Debug,
    fs::{Permissions, read},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use dashmap::{
    DashMap,
    mapref::{
        entry::Entry,
        one::{Ref, RefMut},
    },
};
use futures::{FutureExt, future::BoxFuture};
use nonempty_collections::NEVec;
use rustix::process::Pid;
use tokio::{
    process::Command,
    time::{MissedTickBehavior, interval},
};
use tracing::{Instrument as _, info_span};
use validated::Validated;

use crate::{
    AuthorizedKey, DaemonConfig, MemoryMib, ProvisioningConfig, Rootfs, StorageError, VmId,
    VmInput, VmSpec, VmStore,
    idle::{IdleDecision, IdlePolicy, Monitor, Observation},
    io_error,
    vm::managed::{LifecycleError as ManagedLifecycleError, ManagedVm},
};

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum BarbirolliVm {
    Discovered(VmSpec),
    Failed(VmSpec),
    Managed(ManagedVm),
}

#[derive(Debug, Clone)]
pub struct Balloon {
    pub mem: MemoryMib,
}

pub struct BarbirolliInner {
    pub(crate) vms: DashMap<VmId, BarbirolliVm>,
    pub balloon: Balloon,
    pub provisioning: ProvisioningConfig,
    pub store: VmStore,
    pub firecracker: Firecracker,
    pub entrypoint: PathBuf,
    pub shutdown_timeout: Duration,
    pub draining: AtomicBool,
    pub idle_policy: IdlePolicy,
}

#[derive(derive_more::Deref, Clone, derive_more::Debug)]
#[debug("Barbirolli(Arc<BarbirolliInner>)")]
pub struct Barbirolli(Arc<BarbirolliInner>);

impl Barbirolli {
    /// Creates a lifecycle manager and reconciles persisted VMs.
    ///
    /// # Errors
    ///
    /// Returns an error if Firecracker cannot be validated, persisted VM state
    /// cannot be loaded, or one or more VMs fail warmup reconciliation.
    #[tracing::instrument]
    pub async fn new(store: VmStore, config: DaemonConfig) -> Result<Self, LifecycleError> {
        tracing::info!("initializing barbirolli");
        let firecracker = Firecracker::new(config.firecracker).await?;
        let specs = store.all()?;
        let barbirolli = Self(Arc::new(BarbirolliInner {
            firecracker,
            vms: DashMap::default(),
            balloon: Balloon {
                mem: config.provisioning.initial_balloon_mem,
            },
            provisioning: config.provisioning,
            store,
            entrypoint: config.entrypoint,
            shutdown_timeout: Duration::from_secs(10),
            draining: AtomicBool::new(false),
            idle_policy: config.idle_policy.unwrap_or_default(),
        }));
        match reconcile(&barbirolli, specs).await {
            Validated::Fail(errors) => Err(LifecycleError::Warmup(Validated::Fail(errors))),
            Validated::Good(()) => {
                tracing::info!("barbirolli ready");
                Ok(barbirolli)
            }
        }
    }

    /// Returns the VM associated with `vm_id`.
    ///
    /// # Errors
    ///
    /// Returns [`LifecycleError::NotFound`] if the VM is not registered.
    pub fn vm(&self, vm_id: VmId) -> Result<Ref<'_, VmId, BarbirolliVm>> {
        self.vms.get(&vm_id).ok_or(LifecycleError::NotFound(vm_id))
    }

    /// Returns mutable access to the VM associated with `vm_id`.
    ///
    /// # Errors
    ///
    /// Returns [`LifecycleError::NotFound`] if the VM is not registered.
    pub fn vm_mut(&self, vm_id: VmId) -> Result<RefMut<'_, VmId, BarbirolliVm>> {
        self.vms
            .get_mut(&vm_id)
            .ok_or(LifecycleError::NotFound(vm_id))
    }

    /// Prune resources at the host machine by stopping idle vms, and by
    /// removing idle vms it does free resources to be allocated to used vms
    #[tracing::instrument]
    pub async fn autoscale(&self) {
        tracing::info!("autoscale");
        let mut interval = interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        while self.is_app_alive().is_ok() {
            interval.tick().await;
            for mut vm in self.vms.iter_mut() {
                let vm = vm.value_mut();
                vm.heartbeat(self).await;
            }
        }
    }

    /// Persists a new discovered VM.
    ///
    /// # Errors
    ///
    /// Returns an error if the manager is draining, capacity is exhausted, or
    /// the VM artifacts cannot be created.
    #[tracing::instrument(skip(self, input), err)]
    pub async fn create(&self, input: VmInput) -> Result<VmId> {
        self.is_app_alive()?;
        let running_vms = self
            .vms
            .iter()
            .filter(|vm| {
                matches!(
                    vm.value(),
                    BarbirolliVm::Managed(managed) if !managed.failed
                )
            })
            .count();
        if running_vms >= self.provisioning.count as usize {
            return Err(LifecycleError::CapacityReached {
                running_vms,
                max_running_vms: self.provisioning.count,
            });
        }
        let authorized_keys = input.authorized_keys.clone();
        let provision_ssh_keys = input.provision_ssh_keys;
        let creation = self
            .store
            .create(input, self.provisioning.default_vm_mem)
            .await?;
        creation
            .rootfs
            .provision_rootfs(&self.entrypoint, &authorized_keys, provision_ssh_keys)?;
        let spec = creation.finish()?;
        let id = spec.id;
        self.vms.insert(id, BarbirolliVm::Discovered(spec));
        tracing::info!(
            vm_id = %id,
            status = "discovered",
            "VM state transition"
        );
        Ok(id)
    }

    #[tracing::instrument(skip(self))]
    pub fn list(&self) -> BoxFuture<'_, Vec<VmSummary>> {
        async move {
            self.vms
                .iter()
                .map(|entry| entry.value().summary())
                .collect()
        }
        .instrument(info_span!("list_vms", operation = "list"))
        .boxed()
    }

    /// Shuts down and soft-deletes a VM.
    ///
    /// # Errors
    ///
    /// Returns an error if the manager is draining, the VM is not registered,
    /// shutdown fails, or the deleted state cannot be persisted.
    #[tracing::instrument(skip(self), fields(%vm_id, operation = "delete"), err)]
    pub async fn delete(&self, vm_id: VmId) -> Result<(), LifecycleError> {
        self.is_app_alive()?;
        let Entry::Occupied(mut vm) = self.vms.entry(vm_id) else {
            return Err(LifecycleError::NotFound(vm_id));
        };
        tracing::info!(status = "deleting", "VM state transition");
        let () = {
            let vm = vm.get_mut();
            vm.shutdown(self).await?;
            self.store.delete(vm.spec())?;
        };
        vm.remove();
        tracing::info!(status = "deleted", "VM state transition");
        Ok(())
    }

    /// Drains the manager and shuts down every registered VM.
    ///
    /// # Errors
    ///
    /// Returns an aggregate error when one or more VMs fail to shut down.
    #[tracing::instrument(skip(self), fields(vm_count = self.vms.len()), err)]
    pub async fn shutdown(&self) -> Result<(), LifecycleError> {
        tracing::info!("draining barbirolli");
        self.draining.store(true, Ordering::Release);
        let mut errors = vec![];
        for mut vm in self.vms.iter_mut() {
            tracing::info!(id = ?vm.key(), "shutting down");
            if let Err(err) = vm.value_mut().shutdown(self).await {
                errors.push(Box::new(err));
            }
        }
        if let Some(errors) = NEVec::try_from_vec(errors) {
            return Err(LifecycleError::Shutdown(Validated::Fail(errors)));
        }
        tracing::info!("barbirolli shutdown complete");
        Ok(())
    }

    fn is_app_alive(&self) -> Result<()> {
        if self.draining.load(Ordering::Acquire) {
            Err(LifecycleError::Draining)
        } else {
            Ok(())
        }
    }
}

impl Rootfs {
    fn provision_rootfs(
        &self,
        entrypoint: &Path,
        authorized_keys: &[AuthorizedKey],
        provision_ssh_keys: bool,
    ) -> Result<(), StorageError> {
        let entrypoint_contents = read(entrypoint).map_err(|source| StorageError::Io {
            path: entrypoint.to_owned(),
            source,
        })?;
        self.provision(|builder| {
            builder.write(
                "barbirolli_entrypoint",
                entrypoint_contents,
                Permissions::from_mode(0o755),
            )?;
            if !authorized_keys.is_empty() && provision_ssh_keys {
                builder.create_folder("root/.ssh", Permissions::from_mode(0o700))?;
                builder.write(
                    "root/.ssh/authorized_keys",
                    authorized_keys_into_bytes(authorized_keys),
                    Permissions::from_mode(0o600),
                )?;
            }
            Ok(())
        })
        .map_err(StorageError::from)
    }
}

fn authorized_keys_into_bytes(authorized_keys: &[AuthorizedKey]) -> Vec<u8> {
    let capacity = authorized_keys
        .iter()
        .map(|authorized_key| authorized_key.as_ref().len() + 1)
        .sum();
    let mut contents = Vec::with_capacity(capacity);
    for authorized_key in authorized_keys {
        contents.extend_from_slice(authorized_key.as_ref().as_bytes());
        contents.push(b'\n');
    }
    contents
}

async fn reconcile(barbirolli: &Barbirolli, specs: Vec<VmSpec>) -> Validated<(), WarmupFailure> {
    let mut errors = vec![];
    for spec in specs {
        if spec.deleted {
            continue;
        }
        match spec.reconcile(barbirolli).await {
            Ok(()) => {
                barbirolli
                    .vms
                    .insert(spec.id, BarbirolliVm::Discovered(spec));
            }
            Err(err) => {
                errors.push(WarmupFailure::Reconcile(err));
            }
        }
    }
    if let Some(errors) = NEVec::try_from_vec(errors) {
        Validated::Fail(errors)
    } else {
        tracing::info!(vm_count = barbirolli.vms.len(), "reconciliation done");
        Validated::Good(())
    }
}

impl BarbirolliVm {
    pub fn id(&self) -> VmId {
        self.spec().id
    }

    pub fn spec(&self) -> &VmSpec {
        match self {
            Self::Discovered(spec) | Self::Failed(spec) => spec,
            Self::Managed(vm) => &vm.spec,
        }
    }

    pub fn summary(&self) -> VmSummary {
        match self {
            Self::Discovered(spec) => VmSummary {
                id: spec.id,
                status: VmStatus::Discovered,
                bindings: spec.bindings.clone(),
                network: spec.network.clone(),
            },
            Self::Failed(spec) => VmSummary {
                id: spec.id,
                status: VmStatus::Failed,
                bindings: spec.bindings.clone(),
                network: spec.network.clone(),
            },
            Self::Managed(vm) if vm.failed => VmSummary {
                id: vm.spec.id,
                status: VmStatus::Failed,
                bindings: vm.spec.bindings.clone(),
                network: vm.spec.network.clone(),
            },
            Self::Managed(vm) => VmSummary {
                id: vm.spec.id,
                status: VmStatus::Running,
                bindings: vm.spec.bindings.clone(),
                network: vm.spec.network.clone(),
            },
        }
    }

    pub fn balloon_config<'a>(
        &'a mut self,
        barbirolli: &'a Barbirolli,
    ) -> BoxFuture<'a, Result<BalloonConfig>> {
        async move {
            barbirolli.is_app_alive()?;
            let config = self
                .managed_for("read balloon config")?
                .balloon_config()
                .await?;
            Ok(BalloonConfig {
                amount_mib: config.amount_mib,
                deflate_on_oom: config.deflate_on_oom,
                stats_polling_interval_s: config.stats_polling_interval_s,
            })
        }
        .boxed()
    }

    pub fn update_balloon<'a>(
        &'a mut self,
        barbirolli: &'a Barbirolli,
        amount_mib: MemoryMib,
    ) -> BoxFuture<'a, Result<()>> {
        async move {
            barbirolli.is_app_alive()?;
            self.managed_for("update balloon")?
                .update_balloon(amount_mib.into())
                .await?;
            Ok(())
        }
        .boxed()
    }

    pub fn balloon_statistics<'a>(
        &'a mut self,
        barbirolli: &'a Barbirolli,
    ) -> BoxFuture<'a, Result<BalloonStatistics>> {
        async move {
            barbirolli.is_app_alive()?;
            let statistics = self
                .managed_for("read balloon statistics")?
                .balloon_statistics()
                .await?;
            Ok(BalloonStatistics {
                target_mib: statistics.target_mib,
                actual_mib: statistics.actual_mib,
            })
        }
        .boxed()
    }

    fn managed_for(&mut self, operation: &'static str) -> Result<&mut ManagedVm> {
        let vm_id = self.id();
        let status = self.summary().status;
        match self {
            Self::Managed(managed) if !managed.failed => Ok(managed),
            _ => Err(LifecycleError::InvalidTransition {
                vm_id,
                operation,
                status,
            }),
        }
    }

    #[tracing::instrument(skip(self, policy), fields(vm_id = %self.id()))]
    fn observation(&self, policy: IdlePolicy) -> Option<Observation> {
        let Self::Managed(managed) = self else {
            return None;
        };
        let sample = match managed.activity_sample() {
            Ok(sample) => sample,
            Err(error) => {
                tracing::debug!(%error, "idle sample unavailable");
                return None;
            }
        };
        let Ok(mut monitor) = managed.monitor.lock() else {
            tracing::error!("VM monitor lock was poisoned");
            return None;
        };
        let Some(monitor) = monitor.as_mut() else {
            *monitor = Some(Monitor::new(sample, policy));
            tracing::debug!("idle baseline established");
            return None;
        };
        Some(monitor.observation(sample, policy))
    }

    #[tracing::instrument(skip(self, barbirolli))]
    fn heartbeat<'a>(&'a mut self, barbirolli: &'a Barbirolli) -> BoxFuture<'a, ()> {
        let vm_id = self.id();
        async move {
            let Some(Observation {
                sample,
                decision,
                activity,
                last_activity,
            }) = self.observation(barbirolli.idle_policy)
            else {
                return;
            };
            match decision {
                IdleDecision::None => {}
                IdleDecision::ActivityReset => tracing::debug!(
                    cpu_percent = activity.cpu_percent,
                    counters_regressed = activity.counters_regressed,
                    memory_bytes = sample.memory_bytes,
                    process_count = sample.process_count,
                    network_bytes = sample.network_bytes,
                    disk_bytes = sample.disk_bytes,
                    established_tcp_connections = sample.established_tcp_connections,
                    total_connections = sample.total_connections,
                    cpu_threshold_percent = barbirolli.idle_policy.cpu_threshold_percent(),
                    "VM idle timer reset"
                ),
                IdleDecision::Strike(strike) => tracing::info!(
                    strike,
                    idle_for_seconds = sample.sampled_at.duration_since(last_activity).as_secs(),
                    "VM idle strike"
                ),
                IdleDecision::Shutdown => {
                    self.final_heartbeat_and_shutdown(sample.instance_pid, barbirolli)
                        .await;
                }
            }
        }
        .instrument(info_span!("heartbeat", %vm_id, operation = "heartbeat"))
        .boxed()
    }

    #[tracing::instrument(skip(self, barbirolli))]
    fn final_heartbeat_and_shutdown<'a>(
        &'a mut self,
        expected_pid: i32,
        barbirolli: &'a Barbirolli,
    ) -> BoxFuture<'a, ()> {
        let vm_id = self.id();
        async move {
            if !matches!(self, Self::Managed(managed) if managed.pid.as_raw_pid() == expected_pid) {
                tracing::debug!("VM instance changed before final idle check");
                return;
            }
            let Some(observation) = self.observation(barbirolli.idle_policy) else {
                tracing::warn!("final idle observation failed. postponing shutdown");
                return;
            };
            if observation.decision != IdleDecision::Shutdown {
                tracing::info!("final idle check observed activity");
                return;
            }
            tracing::info!("VM remained idle. beginning automatic shutdown");
            if let Err(error) = self.shutdown(barbirolli).await {
                tracing::error!(%error, "automatic VM shutdown failed");
            }
        }
        .instrument(info_span!(
            "final_heartbeat_and_shutdown",
            %vm_id,
            operation = "idle_shutdown"
        ))
        .boxed()
    }

    #[tracing::instrument(skip(self, barbirolli))]
    pub fn start<'a>(&'a mut self, barbirolli: &'a Barbirolli) -> BoxFuture<'a, Result<()>> {
        let vm_id = self.id();
        async move {
            barbirolli.is_app_alive()?;
            let spec = match self {
                BarbirolliVm::Discovered(spec) => spec.clone(),
                BarbirolliVm::Failed(spec) => {
                    spec.reconcile(barbirolli).await?;
                    spec.clone()
                }
                BarbirolliVm::Managed(managed) if managed.failed => {
                    return Err(LifecycleError::InvalidTransition {
                        vm_id: managed.spec.id,
                        operation: "start",
                        status: VmStatus::Failed,
                    });
                }
                BarbirolliVm::Managed(_) => return Ok(()),
            };
            tracing::info!(status = "starting", "VM state transition");
            match ManagedVm::start(spec.clone(), barbirolli).await {
                Ok(managed) => {
                    tracing::info!(status = "running", "VM state transition");
                    *self = BarbirolliVm::Managed(managed);
                    Ok(())
                }
                Err(error) => {
                    tracing::error!(
                        status = "failed",
                        %error,
                        "VM startup failed"
                    );
                    *self = BarbirolliVm::Failed(spec);
                    Err(error.into())
                }
            }
        }
        .instrument(info_span!("start_vm", %vm_id, operation = "start"))
        .boxed()
    }

    #[tracing::instrument(skip(self, barbirolli))]
    pub fn shutdown<'a>(&'a mut self, barbirolli: &'a Barbirolli) -> BoxFuture<'a, Result<()>> {
        let vm_id = self.id();
        async move {
            match self {
                Self::Discovered(_) => Ok(()),
                Self::Failed(spec) => {
                    spec.reconcile(barbirolli).await?;
                    *self = Self::Discovered(spec.clone());
                    Ok(())
                }
                Self::Managed(managed) => {
                    tracing::info!(status = "shutting_down", "VM state transition");
                    let shutdown = managed.shutdown(barbirolli.shutdown_timeout).await;
                    let cleanup = managed.cleanup().await;
                    match (shutdown, cleanup) {
                        (Ok(()), Ok(())) => {
                            let spec = managed.spec.clone();
                            tracing::info!(status = "discovered", "VM state transition");
                            *self = Self::Discovered(spec);
                            Ok(())
                        }
                        (shutdown, cleanup) => {
                            managed.failed = true;
                            tracing::error!(status = "failed", "VM shutdown failed");
                            Err(ManagedLifecycleError::ShutdownCleanup {
                                shutdown: shutdown.err().map(Box::new),
                                cleanup: cleanup.err().map(Box::new),
                            }
                            .into())
                        }
                    }
                }
            }
        }
        .instrument(info_span!("shutdown_vm", %vm_id, operation = "shutdown"))
        .boxed()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Firecracker {
    pub bin: PathBuf,
    pub api_socket_timeout: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum PidError {
    #[error("failed to access pid resource {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("the Firecracker process could not be identified")]
    ProcessNotFound,
}

impl crate::IoError for PidError {
    fn from_io_error(path: PathBuf, source: std::io::Error) -> Self {
        Self::Io { path, source }
    }
}

impl Firecracker {
    pub async fn new(firecracker: impl Into<PathBuf>) -> Result<Firecracker> {
        let firecracker = firecracker.into();
        let output = Command::new(&firecracker)
            .arg("--version")
            .output()
            .await
            .map_err(|error| ManagedLifecycleError::UnsupportedFirecracker(error.to_string()))?;
        let version = if output.stdout.is_empty() {
            String::from_utf8_lossy(&output.stderr).trim().to_owned()
        } else {
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        };
        if output.status.success() && version.starts_with("Firecracker v1.13.") {
            tracing::debug!(
                path = %firecracker.display(),
                %version,
                "verified Firecracker executable"
            );
            Ok(Firecracker {
                bin: firecracker,
                api_socket_timeout: Duration::from_secs(10),
            })
        } else {
            Err(ManagedLifecycleError::UnsupportedFirecracker(version).into())
        }
    }

    #[tracing::instrument(err)]
    pub(crate) async fn pid(&self, spec: &VmSpec) -> Result<Pid, PidError> {
        let api_socket: PathBuf = spec.api_socket.clone().into();
        let expected_socket = api_socket.as_os_str().as_encoded_bytes();
        let expected_firecracker = self.bin.as_os_str().as_encoded_bytes();
        let expected_id = format!("barbirolli-{}", spec.id);
        let mut proc = io_error!(
            PidError,
            tokio::fs::read_dir("/proc").await,
            PathBuf::from("/proc")
        )?;
        while let Some(entry) =
            io_error!(PidError, proc.next_entry().await, PathBuf::from("/proc"))?
        {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i32>().ok())
            else {
                continue;
            };
            let Ok(command) = tokio::fs::read(entry.path().join("cmdline")).await else {
                continue;
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
                && arguments
                    .windows(2)
                    .any(|args| args[0] == b"--id" && args[1] == expected_id.as_bytes())
            {
                return Pid::from_raw(pid).ok_or(PidError::ProcessNotFound);
            }
        }
        Err(PidError::ProcessNotFound)
    }
}
