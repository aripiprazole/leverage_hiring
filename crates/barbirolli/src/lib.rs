#![cfg_attr(test, recursion_limit = "512")]

mod idle;
mod lifecycle;
mod network;
mod storage;
mod vm;

#[cfg(test)]
extern crate self as barbirolli;

#[cfg(test)]
#[allow(dead_code)]
pub mod support;

pub use idle::IdlePolicy;
pub use lifecycle::{
    Barbirolli, DaemonConfig, LifecycleError, ProvisioningConfig, VmStatus, VmSummary,
};
pub use network::{InterfaceName, NetworkSpec};
pub use storage::{StorageError, VmStore};
pub use vm::{
    ApiSocket, AuthorizedKey, MemoryMib, ParseValueError, Port, PortBinding, VcpuCount, VmId,
    VmInput, VmSpec,
};
