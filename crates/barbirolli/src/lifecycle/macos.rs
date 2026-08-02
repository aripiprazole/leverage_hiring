use futures::{FutureExt, future::BoxFuture};

use super::{DaemonConfig, LifecycleError, Result, VmSummary};
use crate::{VmId, VmInput, VmSpec, VmStore};

pub struct BarbirolliVm;

impl BarbirolliVm {
    pub fn id(&self) -> VmId {
        self.spec().id
    }

    pub fn spec(&self) -> &VmSpec {
        unreachable!("Barbirolli VMs require Linux")
    }

    pub fn summary(&self) -> VmSummary {
        unreachable!("Barbirolli VMs require Linux")
    }

    pub fn start(&mut self, _barbirolli: &Barbirolli) -> BoxFuture<'_, Result<()>> {
        async move { Err(LifecycleError::UnsupportedPlatform) }.boxed()
    }

    pub fn shutdown(&mut self, _barbirolli: &Barbirolli) -> BoxFuture<'_, Result<()>> {
        async move { Err(LifecycleError::UnsupportedPlatform) }.boxed()
    }
}

#[derive(Debug, Clone)]
pub struct Barbirolli;

impl Barbirolli {
    pub async fn new(_store: VmStore, _config: DaemonConfig) -> Result<Self, LifecycleError> {
        Err(LifecycleError::UnsupportedPlatform)
    }

    pub async fn autoscale(&self) {
        std::future::pending::<()>().await;
    }

    pub async fn create(&self, _input: VmInput) -> Result<VmId, LifecycleError> {
        Err(LifecycleError::UnsupportedPlatform)
    }

    pub fn list(&self) -> BoxFuture<'_, Vec<VmSummary>> {
        async move { Vec::new() }.boxed()
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
