use std::{net::Ipv4Addr, path::PathBuf};

use serde::Serialize;

use crate::{StorageError, UserName, VmId, VmInput, VmSpec, VmStore};

pub type Result<T, E = LifecycleError> = std::result::Result<T, E>;

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

pub struct Barbirolli;

impl Barbirolli {
    pub async fn new(
        _store: VmStore,
        _firecracker: impl Into<PathBuf>,
    ) -> Result<Self, LifecycleError> {
        Err(LifecycleError::UnsupportedPlatform)
    }

    pub async fn create(&self, _input: VmInput) -> Result<VmId, LifecycleError> {
        Err(LifecycleError::UnsupportedPlatform)
    }

    pub async fn list(&self) -> Vec<VmSummary> {
        Vec::new()
    }

    pub async fn status(&self, _vm_id: VmId) -> Result<VmSummary, LifecycleError> {
        Err(LifecycleError::UnsupportedPlatform)
    }

    pub async fn spec(&self, _vm_id: VmId) -> Result<VmSpec, LifecycleError> {
        Err(LifecycleError::UnsupportedPlatform)
    }

    pub async fn authorized_keys_path(&self, _user: &str) -> Option<PathBuf> {
        None
    }

    pub async fn ssh_address(&self, _user: &str) -> Option<Ipv4Addr> {
        None
    }

    pub async fn start(&self, _vm_id: VmId) -> Result<(), LifecycleError> {
        Err(LifecycleError::UnsupportedPlatform)
    }

    pub async fn shutdown(&self, _vm_id: VmId) -> Result<(), LifecycleError> {
        Err(LifecycleError::UnsupportedPlatform)
    }

    pub async fn delete(&self, _vm_id: VmId) -> Result<(), LifecycleError> {
        Err(LifecycleError::UnsupportedPlatform)
    }

    pub async fn shutdown_all(&self) -> Result<(), LifecycleError> {
        Err(LifecycleError::UnsupportedPlatform)
    }
}

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
    #[error("warmup reconciliation failed: {0:?}")]
    Warmup(Vec<String>),
    #[error("application shutdown failed: {0:?}")]
    Shutdown(Vec<String>),
    #[error("Firecracker lifecycle operations require Linux")]
    UnsupportedPlatform,
}
