use serde::Serialize;
use validated::Validated;

use crate::{NetworkSpec, PortBinding, StorageError, VmId};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(target_os = "linux"))]
mod macos;

#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(not(target_os = "linux"))]
pub use macos::*;

pub type Result<T, E = LifecycleError> = std::result::Result<T, E>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VmStatus {
    Discovered,
    Starting,
    Running,
    ShuttingDown,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VmSummary {
    pub id: VmId,
    pub status: VmStatus,
    pub port_bindings: Vec<PortBinding>,
    pub network: NetworkSpec,
}

#[derive(Debug, thiserror::Error)]
pub enum WarmupFailure {
    #[cfg(target_os = "linux")]
    #[error(transparent)]
    Reconcile(#[from] crate::vm::managed::LifecycleError),
}

#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("VM {0} was not found")]
    NotFound(VmId),
    #[error("lifecycle operations are draining")]
    Draining,
    #[error("cannot {operation} VM {vm_id} while it is {status:?}")]
    InvalidTransition {
        vm_id: VmId,
        operation: &'static str,
        status: VmStatus,
    },
    #[cfg(target_os = "linux")]
    #[error(transparent)]
    Vm(#[from] crate::vm::managed::LifecycleError),
    #[error("warmup reconciliation failed: {0:?}")]
    Warmup(Validated<(), WarmupFailure>),
    #[error("application shutdown failed: {0:?}")]
    Shutdown(Validated<(), Box<LifecycleError>>),
    #[cfg(not(target_os = "linux"))]
    #[error("Firecracker lifecycle operations require Linux")]
    UnsupportedPlatform,
}

#[cfg(test)]
mod tests {
    use barbirolli::VmStatus;
    use barbirolli::support;
    use barbirolli_derive::firecracker_test;

    use support::{
        config::TestVmConfig,
        firecracker::{FirecrackerFixture, MachineConfig},
    };

    #[firecracker_test]
    async fn per_vm_api_sockets_support_concurrent_vms_and_repeated_lifecycle_calls() {
        let fixture = FirecrackerFixture::new().await;

        let first = fixture.create_vm(TestVmConfig::new(1, 128)).await;
        first
            .lifecycle
            .start_concurrently(|lifecycle| {
                assert_eq!(lifecycle.status(), VmStatus::Running);
            })
            .await;
        first
            .api
            .machine_config(|config| {
                assert_eq!(config, &MachineConfig::new(1, 128));
            })
            .await;

        let second = fixture.create_vm(TestVmConfig::new(2, 192)).await;
        assert_ne!(first.id, second.id);
        second
            .lifecycle
            .start(|lifecycle| {
                assert_eq!(lifecycle.status(), VmStatus::Running);
            })
            .await;
        assert_ne!(first.api.socket, second.api.socket);
        second
            .api
            .machine_config(|config| {
                assert_eq!(config, &MachineConfig::new(2, 192));
            })
            .await;

        first
            .lifecycle
            .shutdown_concurrently(|lifecycle| {
                assert_eq!(lifecycle.status(), VmStatus::Discovered);
            })
            .await;
        assert!(!first.api.socket.exists());
        second
            .api
            .machine_config(|config| {
                assert_eq!(config, &MachineConfig::new(2, 192));
            })
            .await;

        first
            .lifecycle
            .start(|lifecycle| {
                assert_eq!(lifecycle.status(), VmStatus::Running);
            })
            .await;
        first
            .api
            .machine_config(|config| {
                assert_eq!(config, &MachineConfig::new(1, 128));
            })
            .await;
        second
            .api
            .machine_config(|config| {
                assert_eq!(config, &MachineConfig::new(2, 192));
            })
            .await;

        fixture.finish().await;
    }

    #[firecracker_test]
    async fn concurrent_vms_keep_private_disks_across_manager_restart_and_delete_together() {
        let mut fixture = FirecrackerFixture::new().await;
        let mut vms = fixture
            .create_vms_concurrently::<2>(
                [TestVmConfig::new(1, 128), TestVmConfig::new(1, 128)],
                |vms| {
                    assert_eq!(vms.len(), 2);
                    assert_ne!(vms[0].id, vms[1].id);
                },
            )
            .await;
        vms.sort_by_key(|vm| u16::from(vm.id));
        let alice_id = vms[0].id;
        let bob_id = vms[1].id;

        vms[0]
            .lifecycle
            .start(|lifecycle| {
                assert_eq!(lifecycle.status(), VmStatus::Running);
            })
            .await;
        vms[1]
            .lifecycle
            .start(|lifecycle| {
                assert_eq!(lifecycle.status(), VmStatus::Running);
            })
            .await;
        let alice = &vms[0];
        let bob = &vms[1];
        alice
            .ssh
            .command("printf alice > /root/barbirolli-identity && sync", |_| {})
            .await;
        bob.ssh
            .command("printf bob > /root/barbirolli-identity && sync", |_| {})
            .await;

        alice
            .lifecycle
            .shutdown(|lifecycle| {
                assert_eq!(lifecycle.status(), VmStatus::Discovered);
            })
            .await;
        bob.lifecycle
            .shutdown(|lifecycle| {
                assert_eq!(lifecycle.status(), VmStatus::Discovered);
            })
            .await;
        fixture
            .restart_manager(|fixture| {
                assert_eq!(
                    fixture.vm(alice_id).lifecycle.status(),
                    VmStatus::Discovered
                );
                assert_eq!(fixture.vm(bob_id).lifecycle.status(), VmStatus::Discovered);
            })
            .await;

        let alice = fixture.vm(alice_id);
        let bob = fixture.vm(bob_id);
        alice.lifecycle.start(|_| {}).await;
        bob.lifecycle.start(|_| {}).await;
        alice
            .ssh
            .command("cat /root/barbirolli-identity", |output| {
                assert_eq!(output.stdout, "alice");
            })
            .await;
        bob.ssh
            .command("cat /root/barbirolli-identity", |output| {
                assert_eq!(output.stdout, "bob");
            })
            .await;

        let alice_storage = alice.storage.clone();
        let bob_storage = bob.storage.clone();
        fixture
            .delete_vms_concurrently(&[alice_id, bob_id], |remaining| {
                assert!(remaining.is_empty());
            })
            .await;
        assert_eq!(alice_storage.config()["spec"]["deleted"], true);
        assert_eq!(bob_storage.config()["spec"]["deleted"], true);

        fixture.restart_manager(|_| {}).await;
        assert!(fixture.list().await.is_empty());
        fixture.finish().await;
    }

    #[cfg(target_os = "linux")]
    mod behavior {
        use barbirolli::support::{behavior::BehaviorFixture, config::TestVmConfig};
        use barbirolli::{LifecycleError, NetworkSpec, VmId, VmStatus};

        #[tokio::test]
        async fn manager_exposes_discovered_state_and_missing_vm_errors() {
            let fixture = BehaviorFixture::new();
            let manager = fixture.manager().await;
            let vm = manager
                .try_create_vm(TestVmConfig::new(1, 128))
                .await
                .expect("failed to create manager VM");

            let listed = manager.list().await;
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].id, vm.id);
            assert_eq!(listed[0].status, VmStatus::Discovered);
            assert_eq!(
                listed[0].network,
                NetworkSpec::new(vm.id).expect("valid network")
            );
            let listed_json = serde_json::to_value(&listed[0]).expect("summary should serialize");
            assert_eq!(listed_json["network"]["tap"], "fc-tap0");
            assert_eq!(listed_json["network"]["guest_ip"], "172.16.0.2");
            assert_eq!(vm.lifecycle.status(), VmStatus::Discovered);
            assert_eq!(manager.vm(vm.id).id, vm.id);
            vm.lifecycle
                .shutdown(|lifecycle| {
                    assert_eq!(lifecycle.status(), VmStatus::Discovered);
                })
                .await;

            let missing = VmId::try_from(10).expect("valid VM ID");
            assert!(matches!(
                manager.try_vm(missing),
                Err(LifecycleError::NotFound(id)) if id == missing
            ));
            assert!(matches!(
                manager.delete(missing).await,
                Err(LifecycleError::NotFound(id)) if id == missing
            ));

            manager
                .delete(vm.id)
                .await
                .expect("failed to delete manager VM");
            manager.finish().await;
        }

        #[tokio::test]
        async fn manager_deletion_is_deadlock_free_and_deleted_vms_stay_filtered() {
            let fixture = BehaviorFixture::new();
            assert!(fixture.temporary.path().is_dir());
            let manager = fixture.manager().await;
            let vm = manager
                .try_create_vm(TestVmConfig::new(1, 128))
                .await
                .expect("failed to create manager VM");

            vm.lifecycle.delete_with_timeout();
            assert!(manager.list().await.is_empty());
            assert!(vm.storage.kernel().exists());
            assert!(vm.storage.rootfs().exists());
            assert_eq!(vm.storage.config()["spec"]["deleted"], true);

            let reopened = fixture.manager().await;
            assert!(reopened.list().await.is_empty());
            assert!(matches!(
                reopened.try_vm(vm.id),
                Err(LifecycleError::NotFound(id)) if id == vm.id
            ));

            reopened.finish().await;
            manager.finish().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn manager_concurrent_creation_deletion_and_reopen_are_persistent() {
            let fixture = BehaviorFixture::new();
            let manager = fixture.manager().await;
            let mut vms = manager
                .create_vms_concurrently::<4>(
                    [
                        TestVmConfig::new(1, 128),
                        TestVmConfig::new(1, 129),
                        TestVmConfig::new(1, 130),
                        TestVmConfig::new(1, 131),
                    ],
                    |vms| {
                        let ids = vms
                            .iter()
                            .map(|vm| vm.id)
                            .collect::<std::collections::HashSet<_>>();
                        assert_eq!(ids.len(), 4);
                    },
                )
                .await;
            vms.sort_by_key(|vm| u16::from(vm.id));
            assert_eq!(
                vms.iter().map(|vm| u16::from(vm.id)).collect::<Vec<_>>(),
                [0, 1, 2, 3]
            );

            let deleted = [vms[0].id, vms[2].id];
            manager
                .delete_vms_concurrently(&deleted, |remaining| {
                    assert_eq!(remaining.len(), 2);
                    assert!(remaining.iter().all(|vm| !deleted.contains(&vm.id)));
                })
                .await;
            assert_eq!(vms[0].storage.config()["spec"]["deleted"], true);
            assert_eq!(vms[2].storage.config()["spec"]["deleted"], true);

            manager.finish().await;
            let discovered = fixture.storage.open().discover();
            let mut active = discovered
                .iter()
                .filter(|vm| !vm.spec.deleted)
                .map(|vm| u16::from(vm.spec.id))
                .collect::<Vec<_>>();
            active.sort_unstable();
            assert_eq!(active, [1, 3]);
            let next = fixture
                .storage
                .open()
                .create_vm(TestVmConfig::new(1, 128))
                .await;
            assert_eq!(u16::from(next.spec.id), 4);
        }

        #[tokio::test]
        async fn manager_draining_rejects_new_vms() {
            let fixture = BehaviorFixture::new();
            let manager = fixture.manager().await;

            manager.finish().await;
            assert!(matches!(
                manager.try_create_vm(TestVmConfig::new(1, 128)).await,
                Err(LifecycleError::Draining)
            ));
        }
    }
}
