use super::{LifecycleError, Result, VmStatus, VmSummary, WarmupFailure};

use std::{
    collections::HashSet,
    fmt::Debug,
    net::Ipv4Addr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use dashmap::{
    DashMap,
    mapref::one::{Ref, RefMut},
};
use nonempty_collections::NEVec;
use validated::Validated;

use crate::vm::managed::{LifecycleError as ManagedLifecycleError, ManagedVm};
use crate::{StorageError, VmId, VmInput, VmSpec, VmStore};

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
    pub api_socket: PathBuf,
    pub api_socket_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub draining: AtomicBool,
}

#[derive(derive_more::Deref, Clone, derive_more::Debug)]
#[debug("Barbirolli(Arc<BarbirolliInner>)")]
pub struct Barbirolli(Arc<BarbirolliInner>);

impl Barbirolli {
    #[tracing::instrument(skip(firecracker))]
    pub async fn new(
        mut store: VmStore,
        firecracker: impl Into<PathBuf>,
    ) -> Result<Self, LifecycleError> {
        let firecracker = verify_firecracker(firecracker).await?;
        tracing::info!(?firecracker, "initializing barbirolli");
        let specs = store
            .vm_root
            .by_ref()
            .map(|item| item.map(|item| item.persisted_spec.spec))
            .collect::<Result<Vec<_>, StorageError>>()?;
        let api_socket = store.vm_root.dir.join(".sockets/firecracker.socket");
        let barbirolli = Self(Arc::new(BarbirolliInner {
            vms: DashMap::default(),
            store,
            firecracker,
            api_socket,
            api_socket_timeout: Duration::from_secs(10),
            shutdown_timeout: Duration::from_secs(10),
            draining: AtomicBool::new(false),
        }));
        let mut ids = HashSet::new();
        let mut errors = vec![];
        for spec in specs {
            if !ids.insert(spec.id) {
                errors.push(WarmupFailure::DuplicateVmId(spec.id));
                continue;
            };
            if let Err(err) = Box::pin(spec.reconcile(&barbirolli)).await {
                errors.push(WarmupFailure::Reconcile(err));
                continue;
            }
            barbirolli
                .vms
                .insert(spec.id, BarbirolliVm::Discovered(spec));
        }
        match NEVec::try_from_vec(errors) {
            Some(errors) => Err(LifecycleError::Warmup(Validated::Fail(errors))),
            None => Ok(barbirolli),
        }
    }

    fn accepting_operations(&self) -> Result<()> {
        if self.draining.load(Ordering::Acquire) {
            Err(LifecycleError::Draining)
        } else {
            Ok(())
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

    pub async fn create(&self, input: VmInput) -> Result<VmId, LifecycleError> {
        self.accepting_operations()?;
        let spec = self.store.create(input).await?;
        let id = spec.id;
        self.vms.insert(id, BarbirolliVm::Discovered(spec));
        Ok(id)
    }

    pub async fn list(&self) -> Vec<VmSummary> {
        self.vms
            .iter()
            .map(|entry| entry.value().summary())
            .collect()
    }

    pub async fn authorized_keys_path(&self, user: &str) -> Option<PathBuf> {
        self.vms.iter().find_map(|entry| {
            let spec = entry.value().spec();
            (spec.user.as_ref() == user).then(|| spec.artifact_dir.join("authorized_keys"))
        })
    }

    pub async fn ssh_address(&self, user: &str) -> Option<Ipv4Addr> {
        self.vms.iter().find_map(|entry| {
            let BarbirolliVm::Managed(managed) = entry.value() else {
                return None;
            };
            (!managed.failed && managed.spec.user.as_ref() == user)
                .then_some(managed.spec.network.guest_ip)
        })
    }

    pub async fn delete(&self, vm_id: VmId) -> Result<(), LifecycleError> {
        self.accepting_operations()?;
        let mut vm = self.vm_mut(vm_id)?;
        Box::pin(vm.shutdown(self)).await?;
        let spec = vm.spec();
        self.store.delete(spec)?;
        self.vms.remove(&vm_id);
        tracing::info!(
            vm_id = %spec.id,
            user = %spec.user,
            operation = "delete",
            status = "deleted",
            "VM state transition"
        );
        Ok(())
    }

    pub async fn shutdown(&self) -> Result<(), LifecycleError> {
        self.draining.store(true, Ordering::Release);
        let mut errors = vec![];
        for vm in self.vms.iter() {
            let Some(mut vm) = self.vms.get_mut(&vm.key()) else {
                continue;
            };
            if let Err(err) = Box::pin(vm.shutdown(self)).await {
                errors.push(Box::new(err))
            }
        }
        match NEVec::try_from_vec(errors) {
            Some(errors) => Err(LifecycleError::Shutdown(Validated::Fail(errors))),
            None => Ok(()),
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
        Ok(firecracker)
    } else {
        Err(ManagedLifecycleError::UnsupportedFirecracker(version).into())
    }
}

impl BarbirolliVm {
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
                user: spec.user.clone(),
                status: VmStatus::Discovered,
            },
            Self::Failed(spec) => VmSummary {
                id: spec.id,
                user: spec.user.clone(),
                status: VmStatus::Failed,
            },
            Self::Managed(vm) if vm.failed => VmSummary {
                id: vm.spec.id,
                user: vm.spec.user.clone(),
                status: VmStatus::Failed,
            },
            Self::Managed(vm) => VmSummary {
                id: vm.spec.id,
                user: vm.spec.user.clone(),
                status: VmStatus::Running,
            },
        }
    }

    #[tracing::instrument]
    pub async fn start(&mut self, barbirolli: &Barbirolli) -> Result<()> {
        tracing::info!("starting barbirolli vm");
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
        tracing::info!(
            vm_id = %spec.id,
            user = %spec.user,
            operation = "start",
            status = "starting",
            "VM state transition"
        );
        match ManagedVm::start(spec.clone(), barbirolli).await {
            Ok(managed) => {
                tracing::info!(
                    vm_id = %spec.id,
                    user = %spec.user,
                    operation = "start",
                    status = "running",
                    "VM state transition"
                );
                *self = BarbirolliVm::Managed(managed);
                Ok(())
            }
            Err(error) => {
                tracing::error!(
                    vm_id = %spec.id,
                    user = %spec.user,
                    operation = "start",
                    status = "failed",
                    %error,
                    "VM startup failed"
                );
                *self = BarbirolliVm::Failed(spec);
                Err(error.into())
            }
        }
    }

    #[tracing::instrument]
    pub async fn shutdown(&mut self, barbirolli: &Barbirolli) -> Result<()> {
        tracing::info!("shutting downs barbirolli vm");
        match self {
            Self::Discovered(_) => Ok(()),
            Self::Failed(spec) => {
                spec.reconcile(barbirolli).await?;
                *self = Self::Discovered(spec.clone());
                Ok(())
            }
            Self::Managed(managed) => {
                tracing::info!(
                    vm_id = %managed.spec.id,
                    user = %managed.spec.user,
                    operation = "shutdown",
                    status = "shutting_down",
                    "VM state transition"
                );
                let shutdown = managed.shutdown(barbirolli.shutdown_timeout).await;
                let cleanup = managed.cleanup().await;
                if shutdown.is_ok() && cleanup.is_ok() {
                    let spec = managed.spec.clone();
                    tracing::info!(
                        vm_id = %spec.id,
                        user = %spec.user,
                        operation = "shutdown",
                        status = "discovered",
                        "VM state transition"
                    );
                    *self = Self::Discovered(spec);
                    Ok(())
                } else {
                    managed.failed = true;
                    tracing::error!(
                        vm_id = %managed.spec.id,
                        user = %managed.spec.user,
                        operation = "shutdown",
                        status = "failed",
                        "VM shutdown failed"
                    );
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
