use std::{path::PathBuf, sync::Arc, time::Duration};

use barbirolli::{Barbirolli, DaemonConfig, ProvisioningConfig, VmId, VmStore, VmSummary};
use futures::future::try_join_all;
use serde::Deserialize;
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::Barrier,
};

use super::{
    config::TestVmConfig, lifecycle::VmLifecycleFixture, network::VmNetworkFixture,
    ssh::SshFixture, storage::VmStorageFixture,
};

pub struct FirecrackerFixture {
    manager: Barbirolli,
    temporary: TempDir,
    vm_root: PathBuf,
    image_root: PathBuf,
    firecracker: PathBuf,
    ssh_private_key: PathBuf,
}

#[derive(Clone)]
pub struct FirecrackerVmFixture {
    pub id: VmId,
    pub lifecycle: VmLifecycleFixture,
    pub api: FirecrackerApiFixture,
    pub network: VmNetworkFixture,
    pub ssh: SshFixture,
    pub storage: VmStorageFixture,
}

#[derive(Clone)]
pub struct FirecrackerApiFixture {
    pub socket: PathBuf,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct MachineConfig {
    vcpu_count: u8,
    mem_size_mib: u16,
}

impl MachineConfig {
    pub fn new(vcpu_count: u8, memory_mib: u16) -> Self {
        Self {
            vcpu_count,
            mem_size_mib: memory_mib,
        }
    }
}

impl FirecrackerFixture {
    pub async fn new() -> Self {
        let temporary = tempfile::Builder::new()
            .prefix("barbirolli-firecracker-")
            .tempdir_in("/var/tmp")
            .expect("failed to create temporary storage");
        let vm_root = temporary.path().join("vms");
        let image_root =
            PathBuf::from(std::env::var_os("IMAGE_ROOT").expect("IMAGE_ROOT is required"));
        let firecracker =
            PathBuf::from(std::env::var_os("FIRECRACKER").expect("FIRECRACKER is required"));
        let ssh_private_key = std::env::var_os("SSH_PRIVATE_KEY")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                image_root
                    .parent()
                    .expect("IMAGE_ROOT should have a parent")
                    .join("ssh/id_ed25519")
            });
        let store = VmStore::new(vm_root.clone(), image_root.clone())
            .expect("failed to create the VM store");
        let manager = Barbirolli::new(
            store,
            DaemonConfig {
                provisioning: ProvisioningConfig::default(),
                firecracker: firecracker.clone(),
                idle_policy: None,
            },
        )
        .await
        .expect("failed to create Barbirolli");

        Self {
            manager,
            temporary,
            vm_root,
            image_root,
            firecracker,
            ssh_private_key,
        }
    }

    pub async fn create_vm(&self, config: TestVmConfig) -> FirecrackerVmFixture {
        let id = self
            .manager
            .create(config.into_input())
            .await
            .expect("failed to create fixture VM");
        self.vm(id)
    }

    pub async fn create_vms_concurrently<const N: usize>(
        &self,
        configs: [TestVmConfig; N],
        inspect: impl FnOnce(&[FirecrackerVmFixture; N]),
    ) -> [FirecrackerVmFixture; N] {
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

    pub fn vm(&self, id: VmId) -> FirecrackerVmFixture {
        let spec = self
            .manager
            .vm(id)
            .expect("missing fixture VM")
            .spec()
            .clone();
        let socket = spec.api_socket.clone().into();
        FirecrackerVmFixture {
            id,
            lifecycle: VmLifecycleFixture::new(self.manager.clone(), id),
            api: FirecrackerApiFixture { socket },
            network: VmNetworkFixture::new(spec.network.clone()),
            ssh: SshFixture::new(spec.network.guest_ip, self.ssh_private_key.clone()),
            storage: VmStorageFixture::new(&spec),
        }
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

impl FirecrackerApiFixture {
    pub async fn machine_config(&self, inspect: impl FnOnce(&MachineConfig)) -> MachineConfig {
        let mut socket = UnixStream::connect(&self.socket)
            .await
            .expect("failed to connect to the per-VM API socket");
        socket
            .write_all(
                b"GET /machine-config HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("failed to query the VM");

        let response = tokio::time::timeout(Duration::from_secs(5), async {
            let mut response = Vec::new();
            loop {
                let mut buffer = [0; 4096];
                let read = socket
                    .read(&mut buffer)
                    .await
                    .expect("failed to read the Firecracker response");
                if read == 0 {
                    panic!("Firecracker closed an incomplete HTTP response");
                }
                response.extend_from_slice(&buffer[..read]);

                let Some((body_offset, content_length)) = response_body_bounds(&response) else {
                    continue;
                };
                if response.len() >= body_offset + content_length {
                    return response;
                }
            }
        })
        .await
        .expect("the Firecracker API query timed out");
        let body_offset = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|offset| offset + 4)
            .expect("Firecracker returned an invalid HTTP response");
        let headers = std::str::from_utf8(&response[..body_offset])
            .expect("Firecracker returned non-UTF-8 HTTP headers");
        let status = headers
            .lines()
            .next()
            .expect("Firecracker returned an empty HTTP response");
        let mut status_parts = status.split_whitespace();
        if status_parts.next() != Some("HTTP/1.1") || status_parts.next() != Some("200") {
            panic!("Firecracker API query failed with {status}");
        }

        let config = serde_json::from_slice(&response[body_offset..])
            .expect("Firecracker returned an invalid machine config");
        inspect(&config);
        config
    }
}

fn response_body_bounds(response: &[u8]) -> Option<(usize, usize)> {
    let body_offset = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|offset| offset + 4)?;
    let headers = std::str::from_utf8(&response[..body_offset])
        .expect("Firecracker returned invalid headers");
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim())
        })
        .expect("Firecracker response omitted Content-Length")
        .parse::<usize>()
        .expect("Firecracker returned an invalid Content-Length");
    Some((body_offset, content_length))
}
