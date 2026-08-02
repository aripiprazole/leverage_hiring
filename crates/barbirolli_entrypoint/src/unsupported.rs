use std::process::ExitStatus;

use oci_spec::runtime::Process;

use crate::{EntrypointError, Result};

pub struct OciChild;

pub fn mount_userspace_filesystems() -> Result<()> {
    Err(EntrypointError::UnsupportedPlatform)
}

pub fn spawn_oci_process(_process: Process) -> Result<OciChild> {
    Err(EntrypointError::UnsupportedPlatform)
}

pub fn sync_filesystems() {}

pub fn power_off(_exit: ExitStatus) -> Result<()> {
    Err(EntrypointError::UnsupportedPlatform)
}
