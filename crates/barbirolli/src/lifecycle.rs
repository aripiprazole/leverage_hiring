use std::{
    net::Ipv4Addr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use dashmap::DashMap;
use serde::Serialize;
use tokio::sync::Mutex;

#[cfg(feature = "linux")]
use crate::managed::{LifecycleError, ManagedVm};
use crate::{StorageError, UserName, VmId, VmInput, VmSpec, VmStore};

#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("VM {0} was not found")]
    NotFound(VmId),
    #[error("lifecycle operations are draining")]
    Draining,
    #[error("cannot {operation} VM {vm_id} while it is {status:?}")]
    InvalidTransition {
        vm_id: VmId,
        operation: &'static str,
        status: VmStatus,
    },

    #[error(transparent)]
    Vm(#[from] LifecycleError),
    #[error("warmup reconciliation failed: {0:?}")]
    Warmup(Vec<String>),
    #[error("application shutdown failed: {0:?}")]
    Shutdown(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VmStatus {
    Discovered,
    Starting,
    Running,
    ShuttingDown,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VmSummary {
    pub id: VmId,
    pub user: UserName,
    pub status: VmStatus,
}

enum BarbirolliVm {
    Discovered(VmSpec),
    Failed(VmSpec),
    Managed(ManagedVm),
}

impl BarbirolliVm {
    fn status(&self) -> VmStatus {
        match self {
            Self::Discovered(_) => VmStatus::Discovered,
            Self::Failed(_) => VmStatus::Failed,
            Self::Managed(vm) if vm.failed => VmStatus::Failed,
            Self::Managed(_) => VmStatus::Running,
        }
    }
}

pub struct BarbirolliInner {
    vms: DashMap<VmId, Arc<Mutex<BarbirolliVm>>>,
    store: VmStore,
    #[cfg_attr(not(feature = "linux"), allow(dead_code))]
    firecracker: PathBuf,
    api_socket_timeout: Duration,
    shutdown_timeout: Duration,
    draining: AtomicBool,
}

#[derive(derive_more::Deref, derive_more::DerefMut, Clone)]
pub struct Barbirolli(Arc<BarbirolliInner>);

impl Barbirolli {
    pub async fn new(
        store: VmStore,
        firecracker: impl Into<PathBuf>,
    ) -> Result<Self, LifecycleError> {
        let firecracker = firecracker.into();
        let specs = store.discover().await?;
        crate::managed::verify_firecracker(&firecracker).await?;
        let mut failures = Vec::new();
        for spec in &specs {
            if let Err(error) = crate::managed::reconcile(spec, &firecracker).await {
                tracing::error!(
                    vm_id = %spec.vm_id,
                    user = %spec.user,
                    operation = "warmup_reconciliation",
                    %error,
                    "VM warmup reconciliation failed"
                );
                failures.push(format!("VM {} ({}): {error}", spec.vm_id, spec.user));
            }
        }
        if !failures.is_empty() {
            return Err(LifecycleError::Warmup(failures));
        }
        let vms = specs
            .into_iter()
            .map(|spec| {
                (
                    spec.vm_id,
                    Arc::new(Mutex::new(BarbirolliVm::Discovered(spec))),
                )
            })
            .collect();

        Ok(Self(Arc::new(BarbirolliInner {
            vms,
            store,
            firecracker,
            api_socket_timeout: Duration::from_secs(10),
            shutdown_timeout: Duration::from_secs(10),
            draining: AtomicBool::new(false),
        })))
    }

    fn accepting_operations(&self) -> Result<(), LifecycleError> {
        if self.draining.load(Ordering::Acquire) {
            Err(LifecycleError::Draining)
        } else {
            Ok(())
        }
    }

    fn vm(&self, vm_id: VmId) -> Result<Arc<Mutex<BarbirolliVm>>, LifecycleError> {
        self.vms
            .get(&vm_id)
            .map(|vm| vm.value().clone())
            .ok_or(LifecycleError::NotFound(vm_id))
    }

    pub async fn create(&self, input: VmInput) -> Result<VmId, LifecycleError> {
        self.accepting_operations()?;
        let spec = self.store.create(input).await?;
        let id = spec.vm_id;
        self.vms
            .insert(id, Arc::new(Mutex::new(BarbirolliVm::Discovered(spec))));
        Ok(id)
    }

    pub async fn list(&self) -> Vec<VmSummary> {
        let mut summaries = Vec::with_capacity(self.vms.len());
        for entry in self.vms.iter() {
            let vm = entry.lock().await;
            summaries.push(VmSummary {
                id: *entry.key(),
                user: vm.spec().user.clone(),
                status: vm.status(),
            });
        }
        summaries
    }

    pub async fn status(&self, vm_id: VmId) -> Result<VmSummary, LifecycleError> {
        let vm = self.vm(vm_id)?;
        let vm = vm.lock().await;
        Ok(VmSummary {
            id: vm_id,
            user: vm.spec().user.clone(),
            status: vm.status(),
        })
    }

    pub async fn spec(&self, vm_id: VmId) -> Result<VmSpec, LifecycleError> {
        let vm = self.vm(vm_id)?;
        Ok(vm.lock().await.spec().clone())
    }

    pub async fn authorized_keys_path(&self, user: &str) -> Option<PathBuf> {
        for vm in self.vms.iter() {
            let vm = vm.lock().await;
            if vm.spec().user.as_ref() == user {
                return Some(vm.spec().artifact_dir.join("authorized_keys"));
            }
        }
        None
    }

    pub async fn ssh_address(&self, user: &str) -> Option<Ipv4Addr> {
        for vm in self.vms.iter() {
            let vm = vm.lock().await;
            if vm.user.as_ref() == user {
                return Some(vm.network.guest_ip);
            }
        }
        None
    }

    pub async fn start(self, vm_id: VmId) -> Result<(), LifecycleError> {
        self.accepting_operations()?;
        let vm = self.vm(vm_id)?;
        let mut vm = vm.lock().await;
        let spec = match &*vm {
            BarbirolliVm::Discovered(spec) | BarbirolliVm::Failed(spec) => spec.clone(),
            BarbirolliVm::Managed(managed) if managed.is_failed() => {
                return Err(LifecycleError::InvalidTransition {
                    vm_id,
                    operation: "start",
                    status: VmStatus::Failed,
                });
            }
            BarbirolliVm::Managed(_) => return Ok(()),
        };
        tracing::info!(
            vm_id = %spec.vm_id,
            user = %spec.user,
            operation = "start",
            status = "starting",
            "VM state transition"
        );
        let managed = match ManagedVm::start(spec.clone(), self.clone()).await {
            Ok(managed) => managed,
            Err(error) => {
                tracing::error!(
                    vm_id = %spec.vm_id,
                    user = %spec.user,
                    operation = "start",
                    status = "failed",
                    %error,
                    "VM startup failed"
                );
                *vm = BarbirolliVm::Failed(spec);
                return Err(error.into());
            }
        };
        tracing::info!(
            vm_id = %spec.vm_id,
            user = %spec.user,
            operation = "start",
            status = "running",
            "VM state transition"
        );
        *vm = BarbirolliVm::Managed(managed);
        Ok(())
    }

    pub async fn shutdown(&self, vm_id: VmId) -> Result<(), LifecycleError> {
        self.accepting_operations()?;
        let vm = self.vm(vm_id)?;
        let mut vm = vm.lock().await;
        self.shutdown_locked(&mut vm).await
    }

    async fn shutdown_locked(&self, vm: &mut BarbirolliVm) -> Result<(), LifecycleError> {
        if let BarbirolliVm::Failed(spec) = &*vm {
            let spec = spec.clone();
            crate::managed::reconcile(&spec, &self.firecracker).await?;
            *vm = BarbirolliVm::Discovered(spec);
            return Ok(());
        }
        let BarbirolliVm::Managed(managed) = &mut *vm else {
            return Ok(());
        };
        let spec = managed.spec.clone();
        tracing::info!(
            vm_id = %spec.vm_id,
            user = %spec.user,
            operation = "shutdown",
            status = "shutting_down",
            "VM state transition"
        );
        let shutdown = managed.shutdown().await;
        let cleanup = managed.cleanup().await;
        match (shutdown, cleanup) {
            (Ok(()), Ok(())) => {
                tracing::info!(
                    vm_id = %spec.vm_id,
                    user = %spec.user,
                    operation = "shutdown",
                    status = "discovered",
                    "VM state transition"
                );
                *vm = BarbirolliVm::Discovered(spec);
                Ok(())
            }
            (shutdown, cleanup) => {
                managed.mark_failed();
                let error = crate::managed::LifecycleError::ShutdownCleanup {
                    shutdown: shutdown.err().map(Box::new),
                    cleanup: cleanup.err().map(Box::new),
                };
                tracing::error!(
                    vm_id = %spec.vm_id,
                    user = %spec.user,
                    operation = "shutdown",
                    status = "failed",
                    %error,
                    "VM shutdown failed"
                );
                Err(error.into())
            }
        }
    }

    pub async fn delete(&self, vm_id: VmId) -> Result<(), LifecycleError> {
        self.accepting_operations()?;
        let vm = self.vm(vm_id)?;
        let mut vm = vm.lock().await;
        self.shutdown_locked(&mut vm).await?;
        self.store.delete(vm.spec()).await?;
        let spec = vm.spec().clone();
        drop(vm);
        self.vms.remove(&vm_id);
        tracing::info!(
            vm_id = %spec.vm_id,
            user = %spec.user,
            operation = "delete",
            status = "deleted",
            "VM state transition"
        );
        Ok(())
    }

    pub async fn shutdown_all(&self) -> Result<(), LifecycleError> {
        self.draining.store(true, Ordering::Release);
        let ids = self
            .vms
            .iter()
            .map(|entry| *entry.key())
            .collect::<Vec<_>>();
        let mut failures = Vec::new();

        for id in ids {
            if let Err(error) = self.shutdown_vm(id).await {
                failures.push(error.to_string());
            }
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(LifecycleError::Shutdown(failures))
        }
    }
}
