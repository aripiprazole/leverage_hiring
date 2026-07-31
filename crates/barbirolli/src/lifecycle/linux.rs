use super::{LifecycleError, Result, VmStatus, VmSummary, WarmupFailure};

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
use tracing::{Instrument as _, info_span};
use validated::Validated;

use crate::{
    StorageError, VmId, VmInput, VmSpec, VmStore,
    idle::{IdleDecision, IdlePolicy, Monitor, Observation},
    vm::managed::{LifecycleError as ManagedLifecycleError, ManagedVm},
};

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum BarbirolliVm {
    Discovered(VmSpec),
    Failed(VmSpec),
    Managed(ManagedVm),
}

pub struct BarbirolliInner {
    pub(crate) vms: DashMap<VmId, BarbirolliVm>,
    pub store: VmStore,
    pub firecracker: PathBuf,
    pub api_socket_dir: PathBuf,
    pub api_socket_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub draining: AtomicBool,
    pub idle_policy: IdlePolicy,
}

#[derive(derive_more::Deref, Clone, derive_more::Debug)]
#[debug("Barbirolli(Arc<BarbirolliInner>)")]
pub struct Barbirolli(Arc<BarbirolliInner>);

impl Barbirolli {
    pub async fn new(
        store: VmStore,
        firecracker: impl Into<PathBuf>,
    ) -> Result<Self, LifecycleError> {
        Self::new_with_idle_policy(store, firecracker, IdlePolicy::default()).await
    }

    pub async fn new_with_idle_policy(
        mut store: VmStore,
        firecracker: impl Into<PathBuf>,
        idle_policy: IdlePolicy,
    ) -> Result<Self, LifecycleError> {
        let firecracker = verify_firecracker(firecracker).await?;
        tracing::info!(?firecracker, "initializing barbirolli");
        let specs = store
            .vm_root
            .by_ref()
            .map(|item| item.map(|item| item.persisted_spec.spec))
            .collect::<Result<Vec<_>, StorageError>>()?;
        let api_socket_dir = store.vm_root.dir.join(".sockets");
        let barbirolli = Self(Arc::new(BarbirolliInner {
            vms: DashMap::default(),
            store,
            firecracker,
            api_socket_dir,
            api_socket_timeout: Duration::from_secs(10),
            shutdown_timeout: Duration::from_secs(10),
            draining: AtomicBool::new(false),
            idle_policy,
        }));
        let mut errors = vec![];
        for spec in specs {
            if spec.deleted {
                continue;
            }
            if let Err(err) = spec.reconcile(&barbirolli).await {
                errors.push(WarmupFailure::Reconcile(err));
                continue;
            }
            barbirolli
                .vms
                .insert(spec.id, BarbirolliVm::Discovered(spec));
        }
        match NEVec::try_from_vec(errors) {
            Some(errors) => Err(LifecycleError::Warmup(Validated::Fail(errors))),
            None => {
                tracing::info!(vm_count = barbirolli.vms.len(), "barbirolli ready");
                Ok(barbirolli)
            }
        }
    }

    pub async fn run_idle_reaper(&self) {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        while self.is_app_alive().is_ok() {
            interval.tick().await;
            for mut vm in self.vms.iter_mut() {
                let vm = vm.value_mut();
                vm.check_idle(self).await;
            }
        }
    }

    pub(crate) fn api_socket(&self, vm_id: VmId) -> PathBuf {
        self.api_socket_dir
            .join(format!("firecracker-{vm_id}.socket"))
    }

    pub(crate) fn legacy_api_socket(&self) -> PathBuf {
        self.api_socket_dir.join("firecracker.socket")
    }

    pub fn vm(&self, vm_id: VmId) -> Result<Ref<'_, VmId, BarbirolliVm>> {
        self.vms.get(&vm_id).ok_or(LifecycleError::NotFound(vm_id))
    }

    pub fn vm_mut(&self, vm_id: VmId) -> Result<RefMut<'_, VmId, BarbirolliVm>> {
        self.vms
            .get_mut(&vm_id)
            .ok_or(LifecycleError::NotFound(vm_id))
    }

    #[tracing::instrument(skip(self, input), err)]
    pub async fn create(&self, input: VmInput) -> Result<VmId, LifecycleError> {
        self.is_app_alive()?;
        let spec = self.store.create(input).await?;
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
            vm.shutdown(&self).await?;
            self.store.delete(&vm.spec())?;
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
        let ids = self.vms.iter().map(|vm| *vm.key()).collect::<Vec<_>>();
        for id in ids {
            tracing::info!(?id, "shutting down");
            let Some(mut vm) = self.vms.get_mut(&id) else {
                continue;
            };
            if let Err(err) = vm.shutdown(self).await {
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

pub async fn verify_firecracker(firecracker: impl Into<PathBuf>) -> Result<PathBuf> {
    let firecracker = firecracker.into();
    let output = tokio::process::Command::new(&firecracker)
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
        Ok(firecracker)
    } else {
        Err(ManagedLifecycleError::UnsupportedFirecracker(version).into())
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
            if !matches!(self, Self::Managed(managed) if managed.pid == expected_pid) {
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
