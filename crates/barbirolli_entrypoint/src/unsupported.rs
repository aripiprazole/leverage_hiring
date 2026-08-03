use std::{env, process::ExitStatus};

use crate::{EntrypointError, ProcessSpec, RUN_PROCESS_SPEC_ENV, Result};

pub struct OciChild;

impl OciChild {
    /// Waits for the OCI process while forwarding signals to its process group.
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

/// Runs the OCI process when this entrypoint was relaunched as its safe process
/// configuration helper.
///
/// # Errors
///
/// Returns [`EntrypointError::UnsupportedPlatform`] when helper mode was
/// requested on a non-Linux platform.
pub fn run_oci_process_if_requested() -> Result<()> {
    if env::var_os(RUN_PROCESS_SPEC_ENV).is_some() {
        Err(EntrypointError::UnsupportedPlatform)
    } else {
        Ok(())
    }
}

/// Mounts procfs before PID 1 supervision starts.
///
/// # Errors
///
/// Always returns [`EntrypointError::UnsupportedPlatform`].
pub fn mount_proc_filesystem() -> Result<()> {
    Err(EntrypointError::UnsupportedPlatform)
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
