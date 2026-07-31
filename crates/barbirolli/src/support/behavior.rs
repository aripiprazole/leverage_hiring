use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

use barbirolli::{Barbirolli, LifecycleError, VmId, VmSpec, VmStore, VmSummary};
use tempfile::TempDir;

use super::{config::TestVmConfig, lifecycle::VmLifecycleFixture};

pub struct BehaviorFixture {
    pub storage: StorageEnvironmentFixture,
    firecracker: PathBuf,
    temporary: TempDir,
}

#[derive(Clone)]
pub struct StorageEnvironmentFixture {
    pub vm_root: PathBuf,
    pub image_root: PathBuf,
}

pub struct StoreFixture {
    store: VmStore,
}

pub struct StoredVmFixture {
    pub spec: VmSpec,
    pub storage: VmStorageFixture,
}

pub struct BehaviorManagerFixture {
    manager: Barbirolli,
}

pub struct BehaviorVmFixture {
    pub id: VmId,
    pub lifecycle: VmLifecycleFixture,
    pub storage: VmStorageFixture,
}

pub struct VmStorageFixture {
    pub artifact_dir: PathBuf,
}

impl BehaviorFixture {
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
        let store = self.storage.open().store;
        let manager = Barbirolli::new(store, self.firecracker.clone())
            .await
            .expect("failed to create Barbirolli");
        BehaviorManagerFixture { manager }
    }

    pub fn path(&self) -> &std::path::Path {
        self.temporary.path()
    }
}

impl StorageEnvironmentFixture {
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
            .create(config.into_input())
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
    pub async fn create_vm(&self, config: TestVmConfig) -> BehaviorVmFixture {
        self.try_create_vm(config)
            .await
            .expect("failed to create manager VM")
    }

    pub async fn try_create_vm(
        &self,
        config: TestVmConfig,
    ) -> Result<BehaviorVmFixture, LifecycleError> {
        let id = self.manager.create(config.into_input()).await?;
        self.try_vm(id)
    }

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

    pub async fn try_delete(&self, id: VmId) -> Result<(), LifecycleError> {
        self.manager.delete(id).await
    }

    pub async fn list(&self) -> Vec<VmSummary> {
        self.manager.list().await
    }

    pub async fn finish(&self) {
        self.manager
            .shutdown()
            .await
            .expect("failed to drain Barbirolli");
    }
}

impl VmStorageFixture {
    fn new(spec: &VmSpec) -> Self {
        Self {
            artifact_dir: spec.artifact_dir.clone(),
        }
    }

    pub fn config(&self) -> serde_json::Value {
        let config =
            fs::read(self.artifact_dir.join("config.json")).expect("failed to read VM config");
        serde_json::from_slice(&config).expect("invalid VM config")
    }

    pub fn kernel(&self) -> PathBuf {
        self.artifact_dir.join("vmlinux")
    }

    pub fn rootfs(&self) -> PathBuf {
        self.artifact_dir.join("rootfs.ext4")
    }

    pub fn authorized_keys(&self) -> PathBuf {
        self.artifact_dir.join("authorized_keys")
    }
}
