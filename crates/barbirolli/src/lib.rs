#![recursion_limit = "512"]

mod idle;
mod lifecycle;
mod network;
mod storage;
mod vm;

#[cfg(test)]
extern crate self as barbirolli;

#[cfg(test)]
#[allow(
    dead_code,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    reason = "test fixture helpers intentionally fail fast instead of exposing recoverable APIs"
)]
pub mod support;

use std::path::PathBuf;

pub use idle::IdlePolicy;
pub use lifecycle::{
    BalloonConfig, BalloonStatistics, Barbirolli, DaemonConfig, FirecrackerExecutorConfig,
    JailerConfig, JailerConfigError, JailerIdentity, LifecycleError, ProvisioningConfig, VmStatus,
    VmSummary,
};
pub use network::{InterfaceName, NetworkSpec};
#[cfg(target_os = "linux")]
pub use storage::{RootfsBuilder, RootfsProvisionError};
pub use storage::{StorageError, VmStore};
pub use vm::{
    ApiSocket, AuthorizedKey, MemoryMib, ParseValueError, Port, PortBinding, Rootfs, VcpuCount,
    VmId, VmInput, VmSpec,
};

pub trait IoError {
    fn from_io_error(path: PathBuf, err: std::io::Error) -> Self;
}

#[cfg(target_os = "linux")]
macro_rules! io_error {
    ($error:ty, $result:expr, $path:expr) => {
        ($result).map_err(|source| <$error as crate::IoError>::from_io_error($path, source))
    };
}

#[cfg(target_os = "linux")]
pub(crate) use io_error;
