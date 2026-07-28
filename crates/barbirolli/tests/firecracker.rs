use std::{env, path::PathBuf};

use barbirolli::{
    ArtifactName, Barbirolli, MemoryMib, UserName, VcpuCount, VmInput, VmStatus, VmStore,
};
use barbirolli_derive::firecracker_test;

#[firecracker_test]
#[ignore = "requires Linux, KVM, Firecracker 1.13, and VM artifacts"]
async fn complete_firecracker_lifecycle(test_id: barbirolli::VmId) {
    let vm_root = PathBuf::from(env::var_os("VM_ROOT").expect("VM_ROOT is required"));
    let image_root = PathBuf::from(env::var_os("IMAGE_ROOT").expect("IMAGE_ROOT is required"));
    let default_keys = PathBuf::from(
        env::var_os("DEFAULT_AUTHORIZED_KEYS").expect("DEFAULT_AUTHORIZED_KEYS is required"),
    );
    let firecracker = PathBuf::from(env::var_os("FIRECRACKER").expect("FIRECRACKER is required"));
    let store = VmStore::new(vm_root, image_root, default_keys).unwrap();
    let manager = Barbirolli::new(store, firecracker).await.unwrap();
    let user_name = format!("firecracker-test-{test_id}");
    let user = user_name.parse::<UserName>().unwrap();
    let id = manager
        .create(VmInput {
            user,
            vcpu_count: VcpuCount::try_from(2).unwrap(),
            memory_mib: MemoryMib::try_from(512).unwrap(),
            kernel: env::var("FIRECRACKER_TEST_KERNEL")
                .unwrap_or_else(|_| "vmlinux".to_owned())
                .parse::<ArtifactName>()
                .unwrap(),
            rootfs: env::var("FIRECRACKER_TEST_ROOTFS")
                .unwrap_or_else(|_| "alpine.ext4".to_owned())
                .parse::<ArtifactName>()
                .unwrap(),
            authorized_keys: Vec::new(),
        })
        .await
        .unwrap();

    let (first_start, second_start) = tokio::join!(manager.start(id), manager.start(id));
    first_start.unwrap();
    second_start.unwrap();
    assert_eq!(manager.status(id).await.unwrap().status, VmStatus::Running);
    assert_eq!(
        manager.ssh_address(&user_name).await,
        Some(manager.spec(id).await.unwrap().network.guest_ip())
    );

    #[cfg(feature = "linux")]
    {
        use nlink::netlink::{Connection, Nftables, nftables::Family};

        let table = format!("fc_vm_{id}");
        let nftables = Connection::<Nftables>::new().unwrap();
        let rules = nftables.list_rules(&table, Family::Ip).await.unwrap();
        assert_eq!(
            rules.iter().filter(|rule| rule.chain == "forward").count(),
            3
        );
        assert_eq!(
            rules
                .iter()
                .filter(|rule| rule.chain == "postrouting")
                .count(),
            1
        );
        assert!(rules.iter().any(|rule| {
            rule.userdata_raw.as_ref().is_some_and(|data| {
                data.windows("vm-isolation".len())
                    .any(|window| window == b"vm-isolation")
            })
        }));
    }

    manager.shutdown(id).await.unwrap();
    assert_eq!(
        manager.status(id).await.unwrap().status,
        VmStatus::Discovered
    );

    #[cfg(feature = "linux")]
    {
        use nlink::netlink::{Connection, Nftables, nftables::Family};

        let tables = Connection::<Nftables>::new()
            .unwrap()
            .list_tables_in(Family::Ip)
            .await
            .unwrap();
        assert!(
            tables
                .iter()
                .all(|table| table.name != format!("fc_vm_{id}"))
        );
    }

    manager.delete(id).await.unwrap();
}
