use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, sync::Arc};

use barbirolli::{
    Barbirolli, DaemonConfig, LifecycleError, ProvisioningConfig, VmId, VmSpec, VmStore, VmSummary,
};
use derive_more::{Deref, DerefMut};
use futures::future::try_join_all;
use tempfile::TempDir;
use tokio::sync::Barrier;

use super::{config::TestVmConfig, lifecycle::VmLifecycleFixture};

pub use super::storage::VmStorageFixture;

pub struct BehaviorFixture {
    pub storage: StorageEnvironmentFixture,
    pub temporary: TempDir,
    firecracker: PathBuf,
}

#[derive(Clone)]
pub struct StorageEnvironmentFixture {
    pub vm_root: PathBuf,
    pub image_root: PathBuf,
}

#[derive(Deref, DerefMut)]
pub struct StoreFixture {
    #[deref]
    #[deref_mut]
    store: VmStore,
}

pub struct StoredVmFixture {
    pub spec: VmSpec,
    pub storage: VmStorageFixture,
}

#[derive(Deref, DerefMut)]
pub struct BehaviorManagerFixture {
    #[deref]
    #[deref_mut]
    manager: Barbirolli,
}

pub struct BehaviorVmFixture {
    pub id: VmId,
    pub lifecycle: VmLifecycleFixture,
    pub storage: VmStorageFixture,
}

impl Default for BehaviorFixture {
    fn default() -> Self {
        Self::new()
    }
}

impl BehaviorFixture {
    #[must_use]
    pub fn new() -> Self {
        let temporary = tempfile::tempdir().expect("failed to create temporary storage");
        let vm_root = temporary.path().join("vms");
        let image_root = temporary.path().join("images");
        fs::create_dir(&image_root).expect("failed to create IMAGE_ROOT");

        let source_kernel = temporary.path().join("source-vmlinux");
        let source_rootfs = temporary.path().join("source-alpine.ext4");
        fs::write(&source_kernel, b"kernel").expect("failed to create kernel");
        fs::write(&source_rootfs, b"rootfs").expect("failed to create rootfs");
        std::os::unix::fs::symlink(&source_kernel, image_root.join("vmlinux"))
            .expect("failed to link kernel");
        std::os::unix::fs::symlink(&source_rootfs, image_root.join("alpine.ext4"))
            .expect("failed to link rootfs");

        let firecracker = temporary.path().join("firecracker");
        fs::write(
            &firecracker,
            b"#!/bin/sh\nprintf 'Firecracker v1.13.0\\n'\n",
        )
        .expect("failed to create fake Firecracker");
        let mut permissions = fs::metadata(&firecracker)
            .expect("failed to read fake Firecracker metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&firecracker, permissions)
            .expect("failed to make fake Firecracker executable");

        Self {
            storage: StorageEnvironmentFixture {
                vm_root,
                image_root,
            },
            firecracker,
            temporary,
        }
    }

    pub async fn manager(&self) -> BehaviorManagerFixture {
        self.manager_with_provisioning(ProvisioningConfig::default())
            .await
    }

    pub async fn manager_with_provisioning(
        &self,
        provisioning: ProvisioningConfig,
    ) -> BehaviorManagerFixture {
        let store = self.storage.open().store;
        let manager = Barbirolli::new(
            store,
            DaemonConfig {
                provisioning,
                firecracker: self.firecracker.clone(),
                idle_policy: None,
            },
        )
        .await
        .expect("failed to create Barbirolli");
        BehaviorManagerFixture { manager }
    }
}

impl StorageEnvironmentFixture {
    #[must_use]
    pub fn open(&self) -> StoreFixture {
        StoreFixture {
            store: VmStore::new(self.vm_root.clone(), self.image_root.clone())
                .expect("failed to create the VM store"),
        }
    }
}

impl StoreFixture {
    pub async fn create_vm(&self, config: TestVmConfig) -> StoredVmFixture {
        let spec = self
            .store
            .create(
                config.into_input(),
                ProvisioningConfig::default().default_vm_mem,
            )
            .await
            .expect("failed to create stored VM");
        StoredVmFixture::new(spec)
    }

    pub fn delete_vm(&self, vm: &StoredVmFixture) {
        self.store
            .delete(&vm.spec)
            .expect("failed to delete stored VM");
    }

    pub fn discover(self) -> Vec<StoredVmFixture> {
        self.store
            .vm_root
            .map(|item| {
                let spec = item
                    .expect("failed to discover a persisted VM")
                    .persisted_spec
                    .spec;
                StoredVmFixture::new(spec)
            })
            .collect()
    }
}

impl StoredVmFixture {
    fn new(spec: VmSpec) -> Self {
        Self {
            storage: VmStorageFixture::new(&spec),
            spec,
        }
    }
}

impl BehaviorManagerFixture {
    pub async fn try_create_vm(
        &self,
        config: TestVmConfig,
    ) -> Result<BehaviorVmFixture, LifecycleError> {
        let id = self.manager.create(config.into_input()).await?;
        self.try_vm(id)
    }

    pub async fn create_vms_concurrently<const N: usize>(
        &self,
        configs: [TestVmConfig; N],
        inspect: impl FnOnce(&[BehaviorVmFixture; N]),
    ) -> [BehaviorVmFixture; N] {
        let barrier = Arc::new(Barrier::new(N.max(1)));
        let ids = try_join_all(configs.map(|config| {
            let manager = self.manager.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                manager.create(config.into_input()).await
            }
        }))
        .await
        .expect("a concurrent create failed");
        let ids: [VmId; N] = ids
            .try_into()
            .unwrap_or_else(|_| unreachable!("one VM ID is returned per config"));
        let vms = ids.map(|id| self.vm(id));
        inspect(&vms);
        vms
    }

    #[must_use]
    pub fn vm(&self, id: VmId) -> BehaviorVmFixture {
        self.try_vm(id).expect("missing manager VM")
    }

    pub fn try_vm(&self, id: VmId) -> Result<BehaviorVmFixture, LifecycleError> {
        let spec = self.manager.vm(id)?.spec().clone();
        Ok(BehaviorVmFixture {
            id,
            lifecycle: VmLifecycleFixture::new(self.manager.clone(), id),
            storage: VmStorageFixture::new(&spec),
        })
    }

    pub async fn delete_vms_concurrently(
        &self,
        ids: &[VmId],
        inspect: impl FnOnce(&[VmSummary]),
    ) -> Vec<VmSummary> {
        let barrier = Arc::new(Barrier::new(ids.len().max(1)));
        try_join_all(ids.iter().copied().map(|id| {
            let manager = self.manager.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                manager.delete(id).await
            }
        }))
        .await
        .expect("a concurrent delete failed");
        let remaining = self.list().await;
        inspect(&remaining);
        remaining
    }

    pub async fn finish(&self) {
        self.manager
            .shutdown()
            .await
            .expect("failed to drain Barbirolli");
    }
}
