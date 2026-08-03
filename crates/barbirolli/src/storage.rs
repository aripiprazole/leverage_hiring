use std::{io, path::PathBuf};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(target_os = "linux"))]
mod macos;

#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(not(target_os = "linux"))]
pub use macos::*;

pub type Result<T, E = StorageError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("invalid VM input: {0}")]
    InvalidInput(String),
    #[error("no VM IDs remain")]
    IdsExhausted,
    #[error("stale VM creation directory")]
    CreatingDirectory,
    #[error("storage operation failed for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid configuration in {path}: {message}")]
    InvalidConfig { path: PathBuf, message: String },
    #[cfg(target_os = "linux")]
    #[error(transparent)]
    RootfsProvision(#[from] RootfsProvisionError),
}

#[cfg(target_os = "linux")]
impl StorageError {
    fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::{fs, io};

    use barbirolli::support::{behavior::BehaviorFixture, config::TestVmConfig};
    use barbirolli::{ApiSocket, Port, PortBinding, Rootfs};
    use barbirolli_derive::firecracker_test;
    use serde_json::json;

    use barbirolli::support::{firecracker::FirecrackerFixture, ssh::SshKeyFixture};

    use super::RootfsProvisionError;
    use crate::ProvisioningConfig;

    fn read_entrypoint(rootfs: &Rootfs) -> Result<Option<Vec<u8>>, RootfsProvisionError> {
        rootfs.provision(|builder| match builder.read("barbirolli_entrypoint") {
            Ok(contents) => Ok(Some(contents)),
            Err(RootfsProvisionError::Operation { source, .. })
                if source.kind() == io::ErrorKind::NotFound =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        })
    }

    #[firecracker_test]
    async fn daemon_installs_the_entrypoint_and_authorized_key_before_boot() {
        let fixture = FirecrackerFixture::new(ProvisioningConfig::default()).await;
        assert_eq!(
            read_entrypoint(&fixture.source_rootfs()).expect("failed to inspect source rootfs"),
            None,
            "the input rootfs must not already contain the daemon-owned entrypoint"
        );
        let key = SshKeyFixture::generate().await;
        let vm = fixture
            .create_vm(TestVmConfig::new(1).authorized_key(key.public_key.clone()))
            .await;
        assert_eq!(
            read_entrypoint(&Rootfs::from(vm.storage.rootfs()))
                .expect("failed to inspect private rootfs"),
            Some(fs::read(&fixture.entrypoint).expect("failed to read daemon entrypoint")),
            "Barbirolli must install the entrypoint into the copied input rootfs"
        );
        vm.lifecycle.start(|_| {}).await;

        vm.ssh
            .with_private_key(key.private_key.clone())
            .command(
                "printf 'barbirolli-ssh\\n'; readlink /proc/1/exe; \
                 set -- $(cat /proc/1/task/1/children); readlink /proc/$1/exe",
                |output| {
                    assert_eq!(
                        output.stdout,
                        "barbirolli-ssh\n/barbirolli_entrypoint\n/usr/sbin/sshd\n"
                    );
                },
            )
            .await;

        fixture.finish().await;
    }

    #[tokio::test]
    async fn storage_copies_artifacts_without_a_key_sidecar() {
        let fixture = BehaviorFixture::new();
        let store = fixture.storage.open();
        let alice = store.create_vm(TestVmConfig::new(1)).await;

        assert_eq!(u16::from(alice.spec.id), 0);
        assert!(!alice.spec.deleted);
        assert_eq!(alice.spec.artifact_dir, fixture.storage.vm_root.join("0"));
        assert_eq!(
            alice.spec.api_socket,
            ApiSocket::from(alice.storage.artifact_dir.join("firecracker.socket"))
        );
        assert!(!fixture.storage.vm_root.join(".sockets").exists());
        assert_eq!(
            fs::read(alice.storage.kernel()).expect("failed to read copied kernel"),
            b"kernel"
        );
        assert_eq!(
            fs::read(alice.storage.rootfs()).expect("failed to read copied rootfs"),
            b"rootfs"
        );
        assert_eq!(
            fs::read(alice.storage.serial_log()).expect("failed to read serial log"),
            b""
        );
        assert!(
            !alice.storage.authorized_keys().exists(),
            "authorized keys must not be copied beside VM artifacts"
        );
    }

    #[tokio::test]
    async fn storage_copies_the_rootfs_from_vm_input() {
        let fixture = BehaviorFixture::new();
        let selected_rootfs = fixture.temporary.path().join("selected-rootfs.ext4");
        fs::write(&selected_rootfs, b"selected rootfs").expect("failed to create selected rootfs");
        let store = fixture.storage.open();

        let creation = store
            .create(
                TestVmConfig::new(1).into_input(Rootfs::from(selected_rootfs)),
                ProvisioningConfig::default().default_vm_mem,
            )
            .await
            .expect("failed to create stored VM");
        let spec = creation.finish().expect("failed to persist stored VM");

        assert_eq!(
            fs::read(spec.rootfs.as_ref()).expect("failed to read copied rootfs"),
            b"selected rootfs"
        );
    }

    #[tokio::test]
    async fn storage_soft_deletes_and_allocates_monotonic_ids() {
        let fixture = BehaviorFixture::new();
        let store = fixture.storage.open();
        let alice = store.create_vm(TestVmConfig::new(1)).await;
        let bob = store
            .create_vm(TestVmConfig::new(2).binding(80, 8080))
            .await;

        store.delete_vm(&alice);
        assert!(alice.storage.artifact_dir.exists());
        assert!(alice.storage.kernel().exists());
        assert!(alice.storage.rootfs().exists());
        assert_eq!(alice.storage.config()["spec"]["deleted"], true);

        let carol = store.create_vm(TestVmConfig::new(1)).await;
        assert_eq!(u16::from(alice.spec.id), 0);
        assert_eq!(u16::from(bob.spec.id), 1);
        assert_eq!(u16::from(carol.spec.id), 2);
        assert!(!bob.spec.deleted);
        assert!(!carol.spec.deleted);
    }

    #[tokio::test]
    async fn storage_discovers_persisted_vms_after_reopening() {
        let fixture = BehaviorFixture::new();
        let store = fixture.storage.open();
        let alice = store.create_vm(TestVmConfig::new(1)).await;
        let bob = store
            .create_vm(TestVmConfig::new(2).binding(80, 8080))
            .await;

        let bob_config = bob.storage.config();
        assert_eq!(bob_config["version"], 4);
        assert_eq!(
            bob_config["spec"]["bindings"],
            json!([{ "internal": 80, "external": 8080 }])
        );
        assert!(bob_config["spec"].get("port_bindings").is_none());

        let discovered = fixture.storage.open().discover();
        assert_eq!(discovered.len(), 2);
        let reopened_alice = discovered
            .iter()
            .find(|vm| vm.spec.id == alice.spec.id)
            .expect("Alice's persisted VM was not discovered");
        assert!(!reopened_alice.spec.deleted);
        assert_eq!(reopened_alice.spec.api_socket, alice.spec.api_socket);
        assert_eq!(
            reopened_alice.spec.api_socket,
            ApiSocket::from(
                reopened_alice
                    .storage
                    .artifact_dir
                    .join("firecracker.socket")
            )
        );
        let reopened_bob = discovered
            .iter()
            .find(|vm| vm.spec.id == bob.spec.id)
            .expect("Bob's persisted VM was not discovered");
        assert!(!reopened_bob.spec.deleted);
        assert_eq!(reopened_bob.spec.api_socket, bob.spec.api_socket);
        assert_eq!(
            reopened_bob.spec.api_socket,
            ApiSocket::from(reopened_bob.storage.artifact_dir.join("firecracker.socket"))
        );
        assert_eq!(
            reopened_bob.spec.bindings,
            vec![PortBinding {
                internal: Port::try_from(80).expect("valid internal port"),
                external: Port::try_from(8080).expect("valid external port"),
            }]
        );
    }

    #[tokio::test]
    async fn storage_defaults_missing_deleted_to_false() {
        let fixture = BehaviorFixture::new();
        let store = fixture.storage.open();
        let alice = store.create_vm(TestVmConfig::new(1)).await;
        let mut config = alice.storage.config();
        config["spec"]
            .as_object_mut()
            .expect("VM spec should be an object")
            .remove("deleted");
        fs::write(
            alice.storage.artifact_dir.join("config.json"),
            serde_json::to_vec_pretty(&config).expect("failed to encode VM config"),
        )
        .expect("failed to write VM config");

        let discovered = fixture.storage.open().discover();
        assert_eq!(discovered.len(), 1);
        assert!(!discovered[0].spec.deleted);
    }
}
