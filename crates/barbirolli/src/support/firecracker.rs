use std::{path::PathBuf, sync::Arc};

use barbirolli::{
    Barbirolli, DaemonConfig, FirecrackerExecutorConfig, JailerConfig, ProvisioningConfig, Rootfs,
    VmId, VmStore, VmSummary,
};
use futures::future::try_join_all;
use tempfile::TempDir;
use tokio::sync::Barrier;

use super::{
    config::TestVmConfig, lifecycle::VmLifecycleFixture, network::VmNetworkFixture,
    ssh::SshFixture, storage::VmStorageFixture,
};

pub struct FirecrackerFixture {
    pub manager: Barbirolli,
    pub temp: TempDir,
    pub vm_root: PathBuf,
    pub image_root: PathBuf,
    pub firecracker: PathBuf,
    pub firecracker_executor: FirecrackerExecutorConfig,
    pub entrypoint: PathBuf,
    pub ssh_private_key: PathBuf,
}

#[derive(Clone)]
pub struct FirecrackerVmFixture {
    pub id: VmId,
    pub lifecycle: VmLifecycleFixture,
    pub api_socket: PathBuf,
    pub logical_api_socket: PathBuf,
    pub network: VmNetworkFixture,
    pub ssh: SshFixture,
    pub storage: VmStorageFixture,
}

impl FirecrackerFixture {
    pub async fn new(provisioning: ProvisioningConfig) -> Self {
        let temp = tempfile::Builder::new()
            .prefix("barbirolli-firecracker-")
            .tempdir_in("/var/tmp")
            .expect("failed to create temporary storage");
        let vm_root = temp.path().join("vms");
        let image_root =
            PathBuf::from(std::env::var_os("IMAGE_ROOT").expect("IMAGE_ROOT is required"));
        let firecracker =
            PathBuf::from(std::env::var_os("FIRECRACKER").expect("FIRECRACKER is required"));
        let firecracker_executor = executor_config();
        let entrypoint = PathBuf::from(
            std::env::var_os("BARBIROLLI_ENTRYPOINT").expect("BARBIROLLI_ENTRYPOINT is required"),
        );
        let ssh_private_key = std::env::var_os("SSH_PRIVATE_KEY").map_or_else(
            || {
                image_root
                    .parent()
                    .expect("IMAGE_ROOT should have a parent")
                    .join("ssh/id_ed25519")
            },
            PathBuf::from,
        );
        let store = VmStore::new(vm_root.clone(), image_root.clone())
            .expect("failed to create the VM store");
        let manager = Barbirolli::new(
            store,
            DaemonConfig {
                provisioning,
                firecracker: firecracker.clone(),
                firecracker_executor: firecracker_executor.clone(),
                entrypoint: entrypoint.clone(),
                idle_policy: None,
            },
        )
        .await
        .expect("failed to create Barbirolli");

        Self {
            manager,
            temp,
            vm_root,
            image_root,
            firecracker,
            firecracker_executor,
            entrypoint,
            ssh_private_key,
        }
    }

    pub async fn create_vm(&self, config: TestVmConfig) -> FirecrackerVmFixture {
        self.try_create_vm(config)
            .await
            .expect("failed to create fixture VM")
    }

    #[must_use]
    pub fn source_rootfs(&self) -> Rootfs {
        Rootfs::from(self.image_root.join("ubuntu-24.04.ext4"))
    }

    pub async fn try_create_vm(
        &self,
        config: TestVmConfig,
    ) -> Result<FirecrackerVmFixture, barbirolli::LifecycleError> {
        let id = self
            .manager
            .create(config.into_input(Rootfs::from(self.image_root.join("ubuntu-24.04.ext4"))))
            .await?;
        Ok(self.vm(id))
    }

    pub async fn create_vms_concurrently<const N: usize>(
        &self,
        configs: [TestVmConfig; N],
        inspect: impl FnOnce(&[FirecrackerVmFixture; N]),
    ) -> [FirecrackerVmFixture; N] {
        let barrier = Arc::new(Barrier::new(N.max(1)));
        let rootfs = Rootfs::from(self.image_root.join("ubuntu-24.04.ext4"));
        let ids = try_join_all(configs.map(|config| {
            let manager = self.manager.clone();
            let barrier = barrier.clone();
            let rootfs = rootfs.clone();
            async move {
                barrier.wait().await;
                manager.create(config.into_input(rootfs)).await
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
    pub fn vm(&self, id: VmId) -> FirecrackerVmFixture {
        let spec = self
            .manager
            .vm(id)
            .expect("missing fixture VM")
            .spec()
            .clone();
        let logical_api_socket: PathBuf = spec.api_socket.clone().into();
        FirecrackerVmFixture {
            id,
            lifecycle: VmLifecycleFixture::new(self.manager.clone(), id),
            api_socket: self.effective_api_socket(id, &logical_api_socket),
            logical_api_socket,
            network: VmNetworkFixture::new(spec.network.clone()),
            ssh: SshFixture::new(spec.network.guest_ip, self.ssh_private_key.clone()),
            storage: VmStorageFixture::new(&spec),
        }
    }

    fn effective_api_socket(&self, id: VmId, local_path: &std::path::Path) -> PathBuf {
        let FirecrackerExecutorConfig::Jailed(config) = &self.firecracker_executor else {
            return local_path.to_owned();
        };
        config
            .jail_root(&self.firecracker, id)
            .join(crate::lifecycle::JAILED_API_SOCKET.trim_start_matches('/'))
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
        let remaining = self.manager.list().await;
        inspect(&remaining);
        remaining
    }

    pub async fn restart_manager(&mut self, inspect: impl FnOnce(&Self)) {
        self.manager
            .shutdown()
            .await
            .expect("failed to stop Barbirolli before restart");
        let store = VmStore::new(self.vm_root.clone(), self.image_root.clone())
            .expect("failed to reopen the VM store");
        self.manager = Barbirolli::new(
            store,
            DaemonConfig {
                provisioning: ProvisioningConfig::default(),
                firecracker: self.firecracker.clone(),
                firecracker_executor: self.firecracker_executor.clone(),
                entrypoint: self.entrypoint.clone(),
                idle_policy: None,
            },
        )
        .await
        .expect("failed to restart Barbirolli");
        inspect(self);
    }

    pub async fn list(&self) -> Vec<VmSummary> {
        self.manager.list().await
    }

    pub async fn finish(self) {
        let vms = self.manager.list().await;
        for vm in vms {
            self.manager
                .delete(vm.id)
                .await
                .expect("failed to delete fixture VM");
        }
        self.manager
            .shutdown()
            .await
            .expect("failed to drain Barbirolli");
    }
}

fn executor_config() -> FirecrackerExecutorConfig {
    match std::env::var("FIRECRACKER_EXECUTOR").as_deref() {
        Ok("unrestricted") => return FirecrackerExecutorConfig::Unrestricted,
        Ok("jailed") | Err(std::env::VarError::NotPresent) => {}
        Ok(value) => panic!("invalid FIRECRACKER_EXECUTOR: {value}"),
        Err(error) => panic!("invalid FIRECRACKER_EXECUTOR: {error}"),
    }
    let jailer = std::env::var_os("JAILER").expect("JAILER is required for jailed tests");
    let chroot_base = std::env::var_os("JAILER_CHROOT_BASE")
        .map_or_else(|| PathBuf::from("/srv/jailer/barbirolli"), PathBuf::from);
    let uid_base = required_u32("JAILER_UID_BASE");
    let gid_base = required_u32("JAILER_GID_BASE");
    FirecrackerExecutorConfig::Jailed(
        JailerConfig::new(jailer, chroot_base, uid_base, gid_base)
            .expect("invalid jailer identity range"),
    )
}

fn required_u32(name: &'static str) -> u32 {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} is required for jailed tests"))
        .parse()
        .unwrap_or_else(|_| panic!("{name} must be an unsigned 32-bit integer"))
}
