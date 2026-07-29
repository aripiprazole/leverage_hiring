use serde::Serialize;
use validated::Validated;

use crate::{NetworkSpec, PortBinding, StorageError, UserName, VmId};

#[cfg(feature = "linux")]
mod linux;
#[cfg(not(feature = "linux"))]
mod macos;

#[cfg(feature = "linux")]
pub use linux::*;
#[cfg(not(feature = "linux"))]
pub use macos::*;

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
    pub port_bindings: Vec<PortBinding>,
    pub network: NetworkSpec,
}

#[derive(Debug, thiserror::Error)]
pub enum WarmupFailure {
    #[error("duplicate VM ID {0}")]
    DuplicateVmId(VmId),
    #[cfg(feature = "linux")]
    #[error(transparent)]
    Reconcile(#[from] crate::vm::managed::LifecycleError),
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
    #[cfg(feature = "linux")]
    #[error(transparent)]
    Vm(#[from] crate::vm::managed::LifecycleError),
    #[error("warmup reconciliation failed: {0:?}")]
    Warmup(Validated<(), WarmupFailure>),
    #[error("application shutdown failed: {0:?}")]
    Shutdown(Validated<(), Box<LifecycleError>>),
    #[cfg(not(feature = "linux"))]
    #[error("Firecracker lifecycle operations require Linux")]
    UnsupportedPlatform,
}
