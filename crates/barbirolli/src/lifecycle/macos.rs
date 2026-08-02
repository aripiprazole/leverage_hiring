use futures::{FutureExt, future::BoxFuture};

use super::{BalloonConfig, BalloonStatistics, DaemonConfig, LifecycleError, Result, VmSummary};
use crate::{MemoryMib, VmId, VmInput, VmSpec, VmStore};

pub struct BarbirolliVm;

// These methods deliberately mirror the stateful Linux API even though the
// unsupported-platform implementation cannot access VM state.
#[allow(clippy::unused_self)]
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

    pub fn balloon_config(
        &mut self,
        _barbirolli: &Barbirolli,
    ) -> BoxFuture<'_, Result<BalloonConfig>> {
        async move { Err(LifecycleError::UnsupportedPlatform) }.boxed()
    }

    pub fn update_balloon(
        &mut self,
        _barbirolli: &Barbirolli,
        _amount_mib: MemoryMib,
    ) -> BoxFuture<'_, Result<()>> {
        async move { Err(LifecycleError::UnsupportedPlatform) }.boxed()
    }

    pub fn balloon_statistics(
        &mut self,
        _barbirolli: &Barbirolli,
    ) -> BoxFuture<'_, Result<BalloonStatistics>> {
        async move { Err(LifecycleError::UnsupportedPlatform) }.boxed()
    }
}

#[derive(Debug, Clone)]
pub struct Barbirolli;

impl Barbirolli {
    /// # Errors
    ///
    /// Always returns [`LifecycleError::UnsupportedPlatform`].
    #[allow(clippy::unused_async)]
    pub async fn new(_store: VmStore, _config: DaemonConfig) -> Result<Self, LifecycleError> {
        Err(LifecycleError::UnsupportedPlatform)
    }

    pub async fn autoscale(&self) {
        std::future::pending::<()>().await;
    }

    /// # Errors
    ///
    /// Always returns [`LifecycleError::UnsupportedPlatform`].
    #[allow(clippy::unused_async)]
    pub async fn create(&self, _input: VmInput) -> Result<VmId, LifecycleError> {
        Err(LifecycleError::UnsupportedPlatform)
    }

    #[must_use]
    pub fn list(&self) -> BoxFuture<'_, Vec<VmSummary>> {
        async move { Vec::new() }.boxed()
    }

    /// # Errors
    ///
    /// Always returns [`LifecycleError::UnsupportedPlatform`].
    pub fn vm(&self, _vm_id: VmId) -> Result<BarbirolliVm, LifecycleError> {
        Err(LifecycleError::UnsupportedPlatform)
    }

    /// # Errors
    ///
    /// Always returns [`LifecycleError::UnsupportedPlatform`].
    pub fn vm_mut(&self, _vm_id: VmId) -> Result<BarbirolliVm, LifecycleError> {
        Err(LifecycleError::UnsupportedPlatform)
    }

    /// # Errors
    ///
    /// Always returns [`LifecycleError::UnsupportedPlatform`].
    #[allow(clippy::unused_async)]
    pub async fn delete(&self, _vm_id: VmId) -> Result<(), LifecycleError> {
        Err(LifecycleError::UnsupportedPlatform)
    }

    /// # Errors
    ///
    /// Always returns [`LifecycleError::UnsupportedPlatform`].
    #[allow(clippy::unused_async)]
    pub async fn shutdown(&self) -> Result<(), LifecycleError> {
        Err(LifecycleError::UnsupportedPlatform)
    }
}
