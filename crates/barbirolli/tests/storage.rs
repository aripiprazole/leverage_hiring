use std::fs;

use barbirolli::{ArtifactName, MemoryMib, UserName, VcpuCount, VmInput, VmStore};
use tempfile::TempDir;

const AUTHORIZED_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBGdsGblMxDzs/KaTt+UP4TagL9vlgW2N9kHCOXVsmdQ test";

fn input(user: &str, keys: &[&str]) -> VmInput {
    VmInput {
        user: user.parse::<UserName>().unwrap(),
        vcpu_count: VcpuCount::try_from(2).unwrap(),
        memory_mib: MemoryMib::try_from(1024).unwrap(),
        kernel: "vmlinux".parse::<ArtifactName>().unwrap(),
        rootfs: "alpine.ext4".parse::<ArtifactName>().unwrap(),
        authorized_keys: keys.iter().map(|key| key.parse().unwrap()).collect(),
    }
}

fn fixture() -> (TempDir, VmStore) {
    let temporary = TempDir::new().unwrap();
    let image_root = temporary.path().join("images");
    let vm_root = temporary.path().join("vms");
    fs::create_dir(&image_root).unwrap();
    fs::write(image_root.join("vmlinux"), b"kernel").unwrap();
    fs::write(image_root.join("alpine.ext4"), b"rootfs").unwrap();
    let defaults = temporary.path().join("authorized_keys");
    fs::write(&defaults, format!("{AUTHORIZED_KEY}\n")).unwrap();
    let store = VmStore::new(vm_root, image_root, defaults).unwrap();
    (temporary, store)
}

#[tokio::test]
async fn create_publishes_complete_vm_and_discovery_preserves_its_id() {
    let (_temporary, store) = fixture();
    let created = store
        .create(input("gabrielle", &[AUTHORIZED_KEY]))
        .await
        .unwrap();
    assert_eq!(u16::from(created.id), 0);
    assert_eq!(fs::read(&created.kernel).unwrap(), b"kernel");
    assert_eq!(fs::read(&created.rootfs).unwrap(), b"rootfs");
    assert_eq!(
        fs::read_to_string(created.artifact_dir.join("authorized_keys")).unwrap(),
        format!("{AUTHORIZED_KEY}\n")
    );
    assert!(created.artifact_dir.join("config.json").is_file());
    assert_eq!(
        created.network.api_socket().parent().unwrap(),
        store.vm_root.join(".sockets")
    );
    fs::write(created.network.api_socket(), b"stale socket placeholder").unwrap();

    let discovered = store.discover().await.unwrap();
    assert_eq!(discovered, vec![created]);
}

#[tokio::test]
async fn create_uses_default_keys_and_reuses_only_deleted_ids() {
    let (_temporary, store) = fixture();
    let alice = store.create(input("alice", &[])).await.unwrap();
    let bob = store.create(input("bob", &[])).await.unwrap();
    assert_eq!(u16::from(alice.id), 0);
    assert_eq!(u16::from(bob.id), 1);
    assert_eq!(
        fs::read_to_string(alice.artifact_dir.join("authorized_keys")).unwrap(),
        format!("{AUTHORIZED_KEY}\n")
    );

    store.delete(&alice).unwrap();
    let carol = store.create(input("carol", &[])).await.unwrap();
    assert_eq!(u16::from(carol.id), 0);
}

#[tokio::test]
async fn create_rejects_duplicate_users_and_artifacts_outside_image_root() {
    let (temporary, store) = fixture();
    store.create(input("alice", &[])).await.unwrap();
    assert!(store.create(input("alice", &[])).await.is_err());

    let outside = temporary.path().join("outside.ext4");
    fs::write(&outside, b"outside").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, store.image_root.join("escape.ext4")).unwrap();
    let mut escaped = input("eve", &[]);
    escaped.rootfs = "escape.ext4".parse().unwrap();
    assert!(store.create(escaped).await.is_err());
}

#[tokio::test]
async fn discovery_rejects_symlinked_vm_directories_and_duplicate_ids() {
    let (temporary, store) = fixture();
    let alice = store.create(input("alice", &[])).await.unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&alice.artifact_dir, store.vm_root.join("link")).unwrap();
    assert!(store.discover().await.is_err());

    drop(store);
    drop(temporary);
    let (_temporary, store) = fixture();
    let alice = store.create(input("alice", &[])).await.unwrap();
    let bob = store.create(input("bob", &[])).await.unwrap();
    let config_path = bob.artifact_dir.join("config.json");
    let mut config: serde_json::Value =
        serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["spec"]["vm_id"] = serde_json::json!(u16::from(alice.id));
    config["spec"]["network"]["vm_id"] = serde_json::json!(u16::from(alice.id));
    config["spec"]["network"]["tap_name"] = serde_json::json!("fc-tap0");
    config["spec"]["network"]["subnet"] = serde_json::json!("172.16.0.0");
    config["spec"]["network"]["host_ip"] = serde_json::json!("172.16.0.1");
    config["spec"]["network"]["guest_ip"] = serde_json::json!("172.16.0.2");
    config["spec"]["network"]["guest_mac"] = serde_json::json!("06:00:ac:10:00:02");
    fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    assert!(store.discover().await.is_err());
}

#[tokio::test]
async fn discovery_rejects_persisted_paths_that_escape_the_vm_directory() {
    let (_temporary, store) = fixture();
    let alice = store.create(input("alice", &[])).await.unwrap();
    let config_path = alice.artifact_dir.join("config.json");
    let mut config: serde_json::Value =
        serde_json::from_slice(&fs::read(&config_path).unwrap()).unwrap();
    config["spec"]["kernel"] = serde_json::json!("/tmp/vmlinux");
    fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();

    assert!(store.discover().await.is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn discovery_rejects_a_symlinked_config_file() {
    let (_temporary, store) = fixture();
    let alice = store.create(input("alice", &[])).await.unwrap();
    let config = alice.artifact_dir.join("config.json");
    let target = alice.artifact_dir.join("real-config.json");
    fs::rename(&config, &target).unwrap();
    std::os::unix::fs::symlink(&target, &config).unwrap();

    assert!(store.discover().await.is_err());
}
