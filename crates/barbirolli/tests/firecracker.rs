use std::{path::PathBuf, sync::Arc, time::Duration};

use barbirolli::{Barbirolli, MemoryMib, VcpuCount, VmInput, VmStatus, VmStore};
use barbirolli_derive::firecracker_test;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::Barrier,
};

#[firecracker_test]
#[ignore = "requires Linux, KVM, Firecracker, and VM artifacts"]
async fn global_api_socket_supports_repeated_and_concurrent_lifecycle_calls() {
    let temporary = tempfile::tempdir().expect("failed to create temporary storage");
    let vm_root = temporary.path().join("vms");
    let image_root = PathBuf::from(std::env::var_os("IMAGE_ROOT").expect("IMAGE_ROOT is required"));
    let firecracker =
        PathBuf::from(std::env::var_os("FIRECRACKER").expect("FIRECRACKER is required"));
    let api_socket = vm_root.join(".sockets/firecracker.socket");
    let store = VmStore::new(vm_root, image_root).expect("failed to create the VM store");
    let manager = Barbirolli::new(store, firecracker)
        .await
        .expect("failed to create Barbirolli");

    let first = manager
        .create(VmInput {
            vcpu_count: VcpuCount::try_from(1).expect("valid vCPU count"),
            memory_mib: MemoryMib::try_from(128).expect("valid memory"),
            authorized_keys: Vec::new(),
            port_bindings: Vec::new(),
        })
        .await
        .expect("failed to create the first VM");
    let start_barrier = Arc::new(Barrier::new(3));
    let first_start = {
        let manager = manager.clone();
        let start_barrier = start_barrier.clone();
        tokio::spawn(async move {
            start_barrier.wait().await;
            let mut vm = manager.vm_mut(first)?;
            Box::pin(vm.start(&manager)).await
        })
    };
    let duplicate_start = {
        let manager = manager.clone();
        let start_barrier = start_barrier.clone();
        tokio::spawn(async move {
            start_barrier.wait().await;
            let mut vm = manager.vm_mut(first)?;
            Box::pin(vm.start(&manager)).await
        })
    };
    start_barrier.wait().await;
    first_start
        .await
        .expect("the first start task panicked")
        .expect("the first start failed");
    duplicate_start
        .await
        .expect("the duplicate start task panicked")
        .expect("the duplicate start failed");
    assert_eq!(
        manager
            .vm_mut(first)
            .expect("missing first VM")
            .summary()
            .status,
        VmStatus::Running
    );

    let mut socket = UnixStream::connect(&api_socket)
        .await
        .expect("failed to connect to the global API socket");
    socket
        .write_all(b"GET /machine-config HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("failed to query the first VM");
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), socket.read_to_end(&mut response))
        .await
        .expect("the first Firecracker API query timed out")
        .expect("failed to read the first Firecracker response");
    let response = String::from_utf8(response).expect("Firecracker returned non-UTF-8 HTTP");
    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(response.contains("\"vcpu_count\":1"));
    assert!(response.contains("\"mem_size_mib\":128"));

    let shutdown_barrier = Arc::new(Barrier::new(3));
    let first_shutdown = {
        let manager = manager.clone();
        let shutdown_barrier = shutdown_barrier.clone();
        tokio::spawn(async move {
            shutdown_barrier.wait().await;
            let mut vm = manager.vm_mut(first)?;
            Box::pin(vm.shutdown(&manager)).await
        })
    };
    let duplicate_shutdown = {
        let manager = manager.clone();
        let shutdown_barrier = shutdown_barrier.clone();
        tokio::spawn(async move {
            shutdown_barrier.wait().await;
            let mut vm = manager.vm_mut(first)?;
            Box::pin(vm.shutdown(&manager)).await
        })
    };
    shutdown_barrier.wait().await;
    first_shutdown
        .await
        .expect("the first shutdown task panicked")
        .expect("the first shutdown failed");
    duplicate_shutdown
        .await
        .expect("the duplicate shutdown task panicked")
        .expect("the duplicate shutdown failed");
    assert_eq!(
        manager
            .vm_mut(first)
            .expect("missing first VM")
            .summary()
            .status,
        VmStatus::Discovered
    );

    let second = manager
        .create(VmInput {
            vcpu_count: VcpuCount::try_from(2).expect("valid vCPU count"),
            memory_mib: MemoryMib::try_from(192).expect("valid memory"),
            authorized_keys: Vec::new(),
            port_bindings: Vec::new(),
        })
        .await
        .expect("failed to create the second VM");
    {
        let mut vm = manager.vm_mut(second).expect("missing second VM");
        Box::pin(vm.start(&manager))
            .await
            .expect("failed to start the second VM");
    }

    let mut socket = UnixStream::connect(&api_socket)
        .await
        .expect("failed to reconnect to the global API socket");
    socket
        .write_all(b"GET /machine-config HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("failed to query the second VM");
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), socket.read_to_end(&mut response))
        .await
        .expect("the second Firecracker API query timed out")
        .expect("failed to read the second Firecracker response");
    let response = String::from_utf8(response).expect("Firecracker returned non-UTF-8 HTTP");
    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(response.contains("\"vcpu_count\":2"));
    assert!(response.contains("\"mem_size_mib\":192"));

    {
        let mut vm = manager.vm_mut(second).expect("missing second VM");
        Box::pin(vm.shutdown(&manager))
            .await
            .expect("failed to shut down the second VM");
    }
    manager
        .delete(first)
        .await
        .expect("failed to delete the first VM");
    manager
        .delete(second)
        .await
        .expect("failed to delete the second VM");
}
