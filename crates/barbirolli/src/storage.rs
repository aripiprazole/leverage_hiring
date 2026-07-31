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
    #[error("socket directory")]
    SocketDirectory,
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
    SshAccess(#[from] SshAccessError),
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
    use std::fs;

    use barbirolli::support::{behavior::BehaviorFixture, config::TestVmConfig};
    use barbirolli::{Port, PortBinding};

    #[tokio::test]
    async fn storage_copies_artifacts_without_a_key_sidecar() {
        let fixture = BehaviorFixture::new();
        let store = fixture.storage.open();
        let alice = store.create_vm(TestVmConfig::new(1, 128)).await;

        assert_eq!(u16::from(alice.spec.id), 0);
        assert!(!alice.spec.deleted);
        assert_eq!(alice.spec.artifact_dir, fixture.storage.vm_root.join("0"));
        assert_eq!(
            fs::read(alice.storage.kernel()).expect("failed to read copied kernel"),
            b"kernel"
        );
        assert_eq!(
            fs::read(alice.storage.rootfs()).expect("failed to read copied rootfs"),
            b"rootfs"
        );
        assert!(
            !alice.storage.authorized_keys().exists(),
            "authorized keys must not be copied beside VM artifacts"
        );
    }

    #[tokio::test]
    async fn storage_soft_deletes_and_allocates_monotonic_ids() {
        let fixture = BehaviorFixture::new();
        let store = fixture.storage.open();
        let alice = store.create_vm(TestVmConfig::new(1, 128)).await;
        let bob = store
            .create_vm(TestVmConfig::new(2, 192).binding(80, 8080))
            .await;

        store.delete_vm(&alice);
        assert!(alice.storage.artifact_dir.exists());
        assert!(alice.storage.kernel().exists());
        assert!(alice.storage.rootfs().exists());
        assert_eq!(alice.storage.config()["spec"]["deleted"], true);

        let carol = store.create_vm(TestVmConfig::new(1, 128)).await;
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
        let alice = store.create_vm(TestVmConfig::new(1, 128)).await;
        let bob = store
            .create_vm(TestVmConfig::new(2, 192).binding(80, 8080))
            .await;

        let discovered = fixture.storage.open().discover();
        assert_eq!(discovered.len(), 2);
        let reopened_alice = discovered
            .iter()
            .find(|vm| vm.spec.id == alice.spec.id)
            .expect("Alice's persisted VM was not discovered");
        assert!(!reopened_alice.spec.deleted);
        let reopened_bob = discovered
            .iter()
            .find(|vm| vm.spec.id == bob.spec.id)
            .expect("Bob's persisted VM was not discovered");
        assert!(!reopened_bob.spec.deleted);
        assert_eq!(
            reopened_bob.spec.port_bindings,
            vec![PortBinding {
                internal: Port::try_from(80).expect("valid internal port"),
                external: Port::try_from(8080).expect("valid external port"),
            }]
        );
    }

    #[tokio::test]
    async fn storage_reads_legacy_config_without_deleted() {
        let fixture = BehaviorFixture::new();
        let store = fixture.storage.open();
        let alice = store.create_vm(TestVmConfig::new(1, 128)).await;
        let mut config = alice.storage.config();
        config["spec"]
            .as_object_mut()
            .expect("VM spec should be an object")
            .remove("deleted");
        fs::write(
            alice.storage.artifact_dir.join("config.json"),
            serde_json::to_vec_pretty(&config).expect("failed to encode legacy VM config"),
        )
        .expect("failed to write legacy VM config");

        let discovered = fixture.storage.open().discover();
        assert_eq!(discovered.len(), 1);
        assert!(!discovered[0].spec.deleted);
    }
}
