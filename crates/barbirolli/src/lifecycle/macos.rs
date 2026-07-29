use std::path::PathBuf;

use super::{LifecycleError, Result, VmSummary};
use crate::{VmId, VmInput, VmSpec, VmStore};

pub struct BarbirolliVm;

impl BarbirolliVm {
    pub fn spec(&self) -> &VmSpec {
        unreachable!("Barbirolli VMs require the linux feature")
    }

    pub fn summary(&self) -> VmSummary {
        unreachable!("Barbirolli VMs require the linux feature")
    }

    pub async fn start(&mut self, _barbirolli: &Barbirolli) -> Result<()> {
        Err(LifecycleError::UnsupportedPlatform)
    }

    pub async fn shutdown(&mut self, _barbirolli: &Barbirolli) -> Result<()> {
        Err(LifecycleError::UnsupportedPlatform)
    }
}

#[derive(Debug, Clone)]
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

    pub fn vm(&self, _vm_id: VmId) -> Result<BarbirolliVm, LifecycleError> {
        Err(LifecycleError::UnsupportedPlatform)
    }

    pub fn vm_mut(&self, _vm_id: VmId) -> Result<BarbirolliVm, LifecycleError> {
        Err(LifecycleError::UnsupportedPlatform)
    }

    pub async fn delete(&self, _vm_id: VmId) -> Result<(), LifecycleError> {
        Err(LifecycleError::UnsupportedPlatform)
    }

    pub async fn shutdown(&self) -> Result<(), LifecycleError> {
        Err(LifecycleError::UnsupportedPlatform)
    }
}
