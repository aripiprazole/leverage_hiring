use std::process::ExitStatus;

use crate::{EntrypointError, ProcessSpec, Result};

pub struct OciChild;

impl OciChild {
    pub fn supervise_and_reap(self) -> Result<ExitStatus> {
        Err(EntrypointError::UnsupportedPlatform)
    }
}

impl ProcessSpec {
    pub fn spawn(self) -> Result<OciChild> {
        Err(EntrypointError::UnsupportedPlatform)
    }
}

pub fn mount_userspace_filesystems() -> Result<()> {
    Err(EntrypointError::UnsupportedPlatform)
}

pub fn sync_filesystems() {}

pub fn power_off(_exit: ExitStatus) -> Result<()> {
    Err(EntrypointError::UnsupportedPlatform)
}
