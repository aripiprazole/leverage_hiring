use std::process::ExitStatus;

use crate::{EntrypointError, ProcessSpec, Result};

pub struct OciChild;

impl OciChild {
    /// Waits for the OCI process and supervises its descendants.
    ///
    /// # Errors
    ///
    /// Always returns [`EntrypointError::UnsupportedPlatform`].
    pub fn supervise_and_reap(self) -> Result<ExitStatus> {
        Err(EntrypointError::UnsupportedPlatform)
    }
}

impl ProcessSpec {
    /// Spawns the configured OCI process.
    ///
    /// # Errors
    ///
    /// Always returns [`EntrypointError::UnsupportedPlatform`].
    pub fn spawn(self) -> Result<OciChild> {
        Err(EntrypointError::UnsupportedPlatform)
    }
}

/// Mounts the filesystems required by the VM userspace.
///
/// # Errors
///
/// Always returns [`EntrypointError::UnsupportedPlatform`].
pub fn mount_userspace_filesystems() -> Result<()> {
    Err(EntrypointError::UnsupportedPlatform)
}

pub fn sync_filesystems() {}

/// Powers off the VM after the OCI process exits.
///
/// # Errors
///
/// Always returns [`EntrypointError::UnsupportedPlatform`].
pub fn power_off(_exit: ExitStatus) -> Result<()> {
    Err(EntrypointError::UnsupportedPlatform)
}
