use super::{
    BalloonConfig, BalloonStatistics, LifecycleError, Result, VmStatus, VmSummary, WarmupFailure,
};

use std::{
    fmt::Debug,
    path::PathBuf,
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
    DaemonConfig, MemoryMib, ProvisioningConfig, VmId, VmInput, VmSpec, VmStore,
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
    pub shutdown_timeout: Duration,
    pub draining: AtomicBool,
    pub idle_policy: IdlePolicy,
}

#[derive(derive_more::Deref, Clone, derive_more::Debug)]
#[debug("Barbirolli(Arc<BarbirolliInner>)")]
pub struct Barbirolli(Arc<BarbirolliInner>);

impl Barbirolli {
    #[tracing::instrument]
    pub async fn new(store: VmStore, config: DaemonConfig) -> Result<Self, LifecycleError> {
        tracing::info!("initializing barbirolli");
        let firecracker = Firecracker::new(config.firecracker).await?;
        let specs = store.all().await?;
        let barbirolli = Self(Arc::new(BarbirolliInner {
            firecracker,
            vms: DashMap::default(),
            balloon: Balloon {
                mem: config.provisioning.initial_balloon_mem,
            },
            provisioning: config.provisioning,
            store,
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

    pub fn vm(&self, vm_id: VmId) -> Result<Ref<'_, VmId, BarbirolliVm>> {
        self.vms.get(&vm_id).ok_or(LifecycleError::NotFound(vm_id))
    }

    pub fn vm_mut(&self, vm_id: VmId) -> Result<RefMut<'_, VmId, BarbirolliVm>> {
        self.vms
            .get_mut(&vm_id)
            .ok_or(LifecycleError::NotFound(vm_id))
    }

    /// Prune resources at the host machine by stopping idle vms, and by
    /// removing idle vms it does free resources to be allocated to used vms
    pub async fn autoscale(&self) {
        let mut interval = interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        while self.is_app_alive().is_ok() {
            interval.tick().await;
            for mut vm in self.vms.iter_mut() {
                let vm = vm.value_mut();
                vm.check_idle(self).await;
            }
        }
    }

    #[tracing::instrument(skip(self, input), err)]
    pub async fn create(&self, input: VmInput) -> Result<VmId, LifecycleError> {
        self.is_app_alive()?;
        let spec = self
            .store
            .create(input, self.provisioning.default_vm_mem)
            .await?;
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

    #[tracing::instrument(skip(self), fields(vm_count = self.vms.len()), err)]
    pub async fn shutdown(&self) -> Result<(), LifecycleError> {
        tracing::info!("draining barbirolli");
        self.draining.store(true, Ordering::Release);
        let mut errors = vec![];
        for mut vm in self.vms.iter_mut() {
            tracing::info!(id = ?vm.key(), "shutting down");
            if let Err(err) = vm.value_mut().shutdown(self).await {
                errors.push(Box::new(err))
            }
        }
        match NEVec::try_from_vec(errors) {
            Some(errors) => Err(LifecycleError::Shutdown(Validated::Fail(errors))),
            None => {
                tracing::info!("barbirolli shutdown complete");
                Ok(())
            }
        }
    }

    fn is_app_alive(&self) -> Result<()> {
        if self.draining.load(Ordering::Acquire) {
            Err(LifecycleError::Draining)
        } else {
            Ok(())
        }
    }
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
    match NEVec::try_from_vec(errors) {
        Some(errors) => Validated::Fail(errors),
        None => {
            tracing::info!(vm_count = barbirolli.vms.len(), "reconciliation done");
            Validated::Good(())
        }
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
                port_bindings: spec.port_bindings.clone(),
                network: spec.network.clone(),
            },
            Self::Failed(spec) => VmSummary {
                id: spec.id,
                status: VmStatus::Failed,
                port_bindings: spec.port_bindings.clone(),
                network: spec.network.clone(),
            },
            Self::Managed(vm) if vm.failed => VmSummary {
                id: vm.spec.id,
                status: VmStatus::Failed,
                port_bindings: vm.spec.port_bindings.clone(),
                network: vm.spec.network.clone(),
            },
            Self::Managed(vm) => VmSummary {
                id: vm.spec.id,
                status: VmStatus::Running,
                port_bindings: vm.spec.port_bindings.clone(),
                network: vm.spec.network.clone(),
            },
        }
    }

    pub fn balloon_config(&mut self) -> BoxFuture<'_, Result<BalloonConfig>> {
        async move {
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

    pub fn update_balloon(&mut self, amount_mib: MemoryMib) -> BoxFuture<'_, Result<()>> {
        async move {
            self.managed_for("update balloon")?
                .update_balloon(amount_mib.into())
                .await?;
            Ok(())
        }
        .boxed()
    }

    pub fn balloon_statistics(&mut self) -> BoxFuture<'_, Result<BalloonStatistics>> {
        async move {
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
    fn check_idle<'a>(&'a mut self, barbirolli: &'a Barbirolli) -> BoxFuture<'a, ()> {
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
                    self.final_idle_check_and_shutdown(sample.instance_pid, barbirolli)
                        .await;
                }
            }
        }
        .instrument(info_span!("check_idle", %vm_id, operation = "idle_check"))
        .boxed()
    }

    #[tracing::instrument(skip(self, barbirolli))]
    fn final_idle_check_and_shutdown<'a>(
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
            "final_idle_check_and_shutdown",
            %vm_id,
            operation = "idle_shutdown"
        ))
        .boxed()
    }

    #[tracing::instrument(skip(self, barbirolli))]
    pub fn start<'a>(&'a mut self, barbirolli: &'a Barbirolli) -> BoxFuture<'a, Result<()>> {
        let vm_id = self.id();
        async move {
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
