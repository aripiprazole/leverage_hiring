use std::{
    fs,
    net::Ipv4Addr,
    os::unix::{fs::PermissionsExt, net::UnixStream},
    sync::mpsc,
    thread,
    time::Duration,
};

use barbirolli::{
    AuthorizedKey, Barbirolli, LifecycleError, MemoryMib, NetworkSpec, StorageError, UserName,
    VcpuCount, VmId, VmInput, VmStatus, VmStore,
};

const AUTHORIZED_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti user@example.com";

#[test]
fn network_allocation_and_authorized_key_parsing_are_stable() {
    let first = NetworkSpec::new(VmId::try_from(0).expect("valid VM ID"))
        .expect("failed to allocate the first network");
    assert_eq!(first.tap.as_ref(), "fc-tap0");
    assert_eq!(first.subnet, Ipv4Addr::new(172, 16, 0, 0));
    assert_eq!(first.host_ip, Ipv4Addr::new(172, 16, 0, 1));
    assert_eq!(first.guest_ip, Ipv4Addr::new(172, 16, 0, 2));
    assert_eq!(first.guest_mac, "06:00:ac:10:00:02");
    assert_eq!(first.subnet_cidr(), "172.16.0.0/30");
    assert_eq!(
        first.kernel_boot_args(),
        "keep_bootcon console=ttyS0 reboot=k panic=1 ip=172.16.0.2::172.16.0.1:255.255.255.252::eth0:off nameserver=1.1.1.1"
    );

    let last = NetworkSpec::new(VmId::try_from(16_383).expect("valid VM ID"))
        .expect("failed to allocate the last network");
    assert_eq!(last.tap.as_ref(), "fc-tap16383");
    assert_eq!(last.subnet, Ipv4Addr::new(172, 16, 255, 252));
    assert_eq!(last.host_ip, Ipv4Addr::new(172, 16, 255, 253));
    assert_eq!(last.guest_ip, Ipv4Addr::new(172, 16, 255, 254));
    assert_eq!(last.guest_mac, "06:00:ac:10:ff:fe");

    assert!(AUTHORIZED_KEY.parse::<AuthorizedKey>().is_ok());
    assert!(
        "not an OpenSSH public key"
            .parse::<AuthorizedKey>()
            .is_err()
    );
    assert!(
        format!("{AUTHORIZED_KEY}\n")
            .parse::<AuthorizedKey>()
            .is_err()
    );
}

#[tokio::test]
async fn storage_persists_keys_reuses_ids_and_follows_fixed_artifact_symlinks() {
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
    let default_authorized_keys = temporary.path().join("default_authorized_keys");
    fs::write(&default_authorized_keys, format!("{AUTHORIZED_KEY}\n"))
        .expect("failed to create default authorized keys");

    let store = VmStore::new(
        vm_root.clone(),
        image_root.clone(),
        default_authorized_keys.clone(),
    )
    .expect("failed to create the VM store");
    let alice = store
        .create(VmInput {
            user: "alice".parse::<UserName>().expect("valid user"),
            vcpu_count: VcpuCount::try_from(1).expect("valid vCPU count"),
            memory_mib: MemoryMib::try_from(128).expect("valid memory"),
            authorized_keys: Vec::new(),
        })
        .await
        .expect("failed to create Alice's VM");
    assert_eq!(u16::from(alice.id), 0);
    assert_eq!(
        fs::read(&alice.kernel).expect("failed to read copied kernel"),
        b"kernel"
    );
    assert_eq!(
        fs::read(&alice.rootfs).expect("failed to read copied rootfs"),
        b"rootfs"
    );
    assert_eq!(
        fs::read_to_string(alice.artifact_dir.join("authorized_keys"))
            .expect("failed to read copied authorized keys"),
        format!("{AUTHORIZED_KEY}\n")
    );

    let duplicate = store
        .create(VmInput {
            user: "alice".parse::<UserName>().expect("valid user"),
            vcpu_count: VcpuCount::try_from(1).expect("valid vCPU count"),
            memory_mib: MemoryMib::try_from(128).expect("valid memory"),
            authorized_keys: Vec::new(),
        })
        .await;
    assert!(matches!(duplicate, Err(StorageError::DuplicateUser(_))));

    let bob = store
        .create(VmInput {
            user: "bob".parse::<UserName>().expect("valid user"),
            vcpu_count: VcpuCount::try_from(2).expect("valid vCPU count"),
            memory_mib: MemoryMib::try_from(192).expect("valid memory"),
            authorized_keys: vec![
                AUTHORIZED_KEY
                    .parse::<AuthorizedKey>()
                    .expect("valid authorized key"),
            ],
        })
        .await
        .expect("failed to create Bob's VM");
    assert_eq!(u16::from(bob.id), 1);
    assert_eq!(
        fs::read_to_string(bob.artifact_dir.join("authorized_keys"))
            .expect("failed to read explicit authorized keys"),
        format!("{AUTHORIZED_KEY}\n")
    );

    store.delete(&alice).expect("failed to delete Alice's VM");
    let carol = store
        .create(VmInput {
            user: "carol".parse::<UserName>().expect("valid user"),
            vcpu_count: VcpuCount::try_from(1).expect("valid vCPU count"),
            memory_mib: MemoryMib::try_from(128).expect("valid memory"),
            authorized_keys: Vec::new(),
        })
        .await
        .expect("failed to create Carol's VM");
    assert_eq!(u16::from(carol.id), 0);

    let discovered = VmStore::new(vm_root, image_root, default_authorized_keys)
        .expect("failed to reopen the VM store")
        .vm_root
        .map(|item| {
            u16::from(
                item.expect("failed to discover a persisted VM")
                    .persisted_spec
                    .spec
                    .id,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(discovered.len(), 2);
    assert!(discovered.contains(&0));
    assert!(discovered.contains(&1));
}

#[tokio::test]
async fn manager_exposes_discovered_state_and_drains_operations() {
    let temporary = tempfile::tempdir().expect("failed to create temporary storage");
    let vm_root = temporary.path().join("vms");
    let image_root = temporary.path().join("images");
    fs::create_dir(&image_root).expect("failed to create IMAGE_ROOT");
    fs::write(image_root.join("vmlinux"), b"kernel").expect("failed to create kernel");
    fs::write(image_root.join("alpine.ext4"), b"rootfs").expect("failed to create rootfs");
    let default_authorized_keys = temporary.path().join("default_authorized_keys");
    fs::write(&default_authorized_keys, format!("{AUTHORIZED_KEY}\n"))
        .expect("failed to create default authorized keys");
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

    let store = VmStore::new(vm_root, image_root, default_authorized_keys)
        .expect("failed to create the VM store");
    let manager = Barbirolli::new(store, firecracker)
        .await
        .expect("failed to create Barbirolli");
    let id = manager
        .create(VmInput {
            user: "alice".parse::<UserName>().expect("valid user"),
            vcpu_count: VcpuCount::try_from(1).expect("valid vCPU count"),
            memory_mib: MemoryMib::try_from(128).expect("valid memory"),
            authorized_keys: Vec::new(),
        })
        .await
        .expect("failed to create Alice's VM");

    let listed = manager.list().await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, id);
    assert_eq!(listed[0].user.as_ref(), "alice");
    assert_eq!(listed[0].status, VmStatus::Discovered);
    assert_eq!(
        listed[0].network,
        NetworkSpec::new(id).expect("valid network")
    );
    let listed_json = serde_json::to_value(&listed[0]).expect("summary should serialize");
    assert_eq!(listed_json["network"]["tap"], "fc-tap0");
    assert_eq!(listed_json["network"]["guest_ip"], "172.16.0.2");
    {
        let vm = manager.vm_mut(id).expect("missing VM");
        assert_eq!(vm.summary().status, VmStatus::Discovered);
        assert_eq!(vm.spec().id, id);
    }
    assert_eq!(
        manager
            .authorized_keys_path("alice")
            .await
            .expect("missing authorized_keys path")
            .file_name()
            .and_then(|name| name.to_str()),
        Some("authorized_keys")
    );
    assert!(manager.authorized_keys_path("missing").await.is_none());
    assert!(manager.ssh_address("alice").await.is_none());
    {
        let mut vm = manager.vm_mut(id).expect("missing VM");
        Box::pin(vm.shutdown(&manager))
            .await
            .expect("discovered shutdown should be idempotent");
    }

    let missing = VmId::try_from(10).expect("valid VM ID");
    assert!(matches!(
        manager.vm_mut(missing),
        Err(LifecycleError::NotFound(id)) if id == missing
    ));
    assert!(matches!(
        manager.delete(missing).await,
        Err(LifecycleError::NotFound(id)) if id == missing
    ));

    let (delete_result_tx, delete_result_rx) = mpsc::sync_channel(1);
    let delete_manager = manager.clone();
    let delete_thread = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to create DELETE test runtime");
        let result = runtime
            .block_on(delete_manager.delete(id))
            .map_err(|error| error.to_string());
        delete_result_tx
            .send(result)
            .expect("DELETE test receiver dropped");
    });
    delete_result_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("VM deletion deadlocked")
        .expect("failed to delete VM");
    delete_thread.join().expect("VM deletion thread panicked");
    assert!(manager.list().await.is_empty());
    manager
        .shutdown()
        .await
        .expect("failed to drain an empty manager");
    let drained = manager
        .create(VmInput {
            user: "after-drain".parse::<UserName>().expect("valid user"),
            vcpu_count: VcpuCount::try_from(1).expect("valid vCPU count"),
            memory_mib: MemoryMib::try_from(128).expect("valid memory"),
            authorized_keys: Vec::new(),
        })
        .await;
    assert!(matches!(drained, Err(LifecycleError::Draining)));

    assert!(UnixStream::connect(temporary.path().join("unused.socket")).is_err());
}
