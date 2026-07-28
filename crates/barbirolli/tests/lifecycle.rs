use std::fs;
#[cfg(not(feature = "linux"))]
use std::path::Path;

use barbirolli::{
    ArtifactName, Barbirolli, MemoryMib, UserName, VcpuCount, VmInput, VmStatus, VmStore,
};
use tempfile::TempDir;

const AUTHORIZED_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBGdsGblMxDzs/KaTt+UP4TagL9vlgW2N9kHCOXVsmdQ test";

fn store(temporary: &TempDir) -> VmStore {
    let images = temporary.path().join("images");
    fs::create_dir(&images).unwrap();
    fs::write(images.join("vmlinux"), b"kernel").unwrap();
    fs::write(images.join("alpine.ext4"), b"rootfs").unwrap();
    let keys = temporary.path().join("authorized_keys");
    fs::write(&keys, format!("{AUTHORIZED_KEY}\n")).unwrap();
    VmStore::new(temporary.path().join("vms"), images, keys).unwrap()
}

fn input(user: &str) -> VmInput {
    VmInput {
        user: user.parse::<UserName>().unwrap(),
        vcpu_count: VcpuCount::try_from(2).unwrap(),
        memory_mib: MemoryMib::try_from(1024).unwrap(),
        kernel: "vmlinux".parse::<ArtifactName>().unwrap(),
        rootfs: "alpine.ext4".parse::<ArtifactName>().unwrap(),
        authorized_keys: Vec::new(),
    }
}

async fn manager(store: VmStore) -> Barbirolli {
    #[cfg(feature = "linux")]
    let firecracker = {
        use std::os::unix::fs::PermissionsExt;

        let firecracker = store.vm_root.parent().unwrap().join("firecracker");
        fs::write(&firecracker, "#!/bin/sh\necho 'Firecracker v1.13.0'\n").unwrap();
        fs::set_permissions(&firecracker, fs::Permissions::from_mode(0o755)).unwrap();
        firecracker
    };
    #[cfg(not(feature = "linux"))]
    let firecracker = Path::new("/usr/bin/firecracker").to_owned();

    Barbirolli::new(store, firecracker).await.unwrap()
}

#[tokio::test]
async fn create_list_status_and_delete_use_the_concrete_vm_state() {
    let temporary = TempDir::new().unwrap();
    let manager = manager(store(&temporary)).await;
    let id = manager.create(input("alice")).await.unwrap();

    assert_eq!(
        manager.status(id).await.unwrap().status,
        VmStatus::Discovered
    );
    assert_eq!(manager.list().await.len(), 1);
    assert_eq!(manager.spec(id).await.unwrap().user.as_ref(), "alice");
    assert_eq!(
        manager.authorized_keys_path("alice").await.unwrap(),
        manager
            .spec(id)
            .await
            .unwrap()
            .artifact_dir
            .join("authorized_keys")
    );
    assert!(manager.authorized_keys_path("bob").await.is_none());
    assert_eq!(
        manager.ssh_address("alice").await,
        Some(manager.spec(id).await.unwrap().network.guest_ip())
    );
    assert!(manager.ssh_address("bob").await.is_none());

    manager.shutdown(id).await.unwrap();
    manager.delete(id).await.unwrap();
    assert!(manager.status(id).await.is_err());
}

#[tokio::test]
async fn shutdown_all_stops_accepting_lifecycle_operations() {
    let temporary = TempDir::new().unwrap();
    let manager = manager(store(&temporary)).await;
    manager.create(input("alice")).await.unwrap();

    manager.shutdown_all().await.unwrap();
    assert!(manager.create(input("bob")).await.is_err());
}

#[tokio::test]
async fn concurrent_deletes_are_serialized() {
    let temporary = TempDir::new().unwrap();
    let manager = manager(store(&temporary)).await;
    let id = manager.create(input("alice")).await.unwrap();

    let (first, second) = tokio::join!(manager.delete(id), manager.delete(id));
    assert_ne!(first.is_ok(), second.is_ok());
    assert!(manager.status(id).await.is_err());
}

#[cfg(not(feature = "linux"))]
#[tokio::test]
async fn warmup_discovers_persisted_vms() {
    let temporary = TempDir::new().unwrap();
    let store = store(&temporary);
    let first = manager(store.clone()).await;
    let id = first.create(input("alice")).await.unwrap();
    drop(first);

    let restarted = manager(store).await;
    assert_eq!(restarted.status(id).await.unwrap().user.as_ref(), "alice");
}

#[cfg(not(feature = "linux"))]
#[tokio::test]
async fn start_requires_linux_support() {
    let temporary = TempDir::new().unwrap();
    let manager = manager(store(&temporary)).await;
    let id = manager.create(input("alice")).await.unwrap();

    assert!(
        manager
            .start(id)
            .await
            .unwrap_err()
            .to_string()
            .contains("linux")
    );
    assert_eq!(
        manager.status(id).await.unwrap().status,
        VmStatus::Discovered
    );
}
