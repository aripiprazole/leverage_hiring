use std::path::PathBuf;

use serde::Serialize;
use validated::Validated;

use crate::{MemoryMib, NetworkSpec, PortBinding, StorageError, VmId, idle::IdlePolicy};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(target_os = "linux"))]
mod macos;

#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(not(target_os = "linux"))]
pub use macos::*;

pub type Result<T, E = LifecycleError> = std::result::Result<T, E>;

/// The maximum daemon memory is calculated as
/// `max_daemon_mem = x + (x / 100 * extra_percentage)`, where
/// `x = default_vm_mem * count`.
#[derive(Debug)]
pub struct ProvisioningConfig {
    pub default_vm_mem: MemoryMib,
    pub max_daemon_mem: MemoryMib,
    pub initial_balloon_mem: MemoryMib,
    pub count: u32,
    pub extra_percentage: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BalloonConfig {
    pub amount_mib: i32,
    pub deflate_on_oom: bool,
    pub stats_polling_interval_s: Option<i32>,
}

impl BalloonConfig {
    #[must_use]
    pub fn new(amount_mib: i32, deflate_on_oom: bool, stats_polling_interval_s: i32) -> Self {
        Self {
            amount_mib,
            deflate_on_oom,
            stats_polling_interval_s: Some(stats_polling_interval_s),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BalloonStatistics {
    pub target_mib: u32,
    pub actual_mib: u32,
}

impl Default for ProvisioningConfig {
    fn default() -> Self {
        const VM_MEM_MIB: u16 = 256;
        const DAEMON_MEM_MIB: u16 = 2048;
        const EXTRA_PERCENTAGE: u8 = 20;

        let mem_mib = u32::from(DAEMON_MEM_MIB) * 100 / (100 + u32::from(EXTRA_PERCENTAGE));
        let count = mem_mib / u32::from(VM_MEM_MIB);
        let balloon_pool_mib = u32::from(DAEMON_MEM_MIB) - mem_mib;
        let initial_balloon_mem = u16::try_from(balloon_pool_mib / count)
            .expect("default balloon target fits in MemoryMib");

        Self {
            default_vm_mem: MemoryMib::from(VM_MEM_MIB),
            max_daemon_mem: MemoryMib::from(DAEMON_MEM_MIB),
            initial_balloon_mem: MemoryMib::from(initial_balloon_mem),
            extra_percentage: EXTRA_PERCENTAGE,
            count,
        }
    }
}

#[derive(Debug)]
pub struct DaemonConfig {
    pub provisioning: ProvisioningConfig,
    pub firecracker: PathBuf,
    pub entrypoint: PathBuf,
    pub idle_policy: Option<IdlePolicy>,
}

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
    pub bindings: Vec<PortBinding>,
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
    #[error(
        "cannot create VM: {running_vms} running VMs reached the configured limit of {max_running_vms}"
    )]
    CapacityReached {
        running_vms: usize,
        max_running_vms: u32,
    },
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
    use barbirolli::support;
    use barbirolli::{BalloonConfig, LifecycleError, ProvisioningConfig, VmStatus};
    use barbirolli_derive::firecracker_test;

    use support::{config::TestVmConfig, firecracker::FirecrackerFixture};

    #[test]
    fn default_provisioning_calculates_a_nonzero_balloon_target() {
        let provisioning = ProvisioningConfig::default();

        assert_eq!(u16::from(provisioning.default_vm_mem), 256);
        assert_eq!(u16::from(provisioning.max_daemon_mem), 2048);
        assert_eq!(provisioning.extra_percentage, 20);
        assert_eq!(provisioning.count, 6);
        assert_eq!(u16::from(provisioning.initial_balloon_mem), 57);
    }

    #[firecracker_test]
    async fn creation_rejects_when_running_vm_capacity_is_reached() {
        let provisioning = ProvisioningConfig {
            count: 1,
            ..ProvisioningConfig::default()
        };
        let fixture = FirecrackerFixture::new(provisioning).await;
        let vm = fixture.create_vm(TestVmConfig::new(1)).await;
        vm.lifecycle.start(|_| {}).await;

        let result = fixture.try_create_vm(TestVmConfig::new(1)).await;

        assert!(matches!(
            result,
            Err(LifecycleError::CapacityReached {
                running_vms: 1,
                max_running_vms: 1,
            })
        ));
        fixture.finish().await;
    }

    #[firecracker_test]
    async fn guest_balloon_inflates_and_deflates() {
        let fixture = FirecrackerFixture::new(ProvisioningConfig::default()).await;
        let vm = fixture.create_vm(TestVmConfig::new(1)).await;

        vm.lifecycle.start(|_| {}).await;
        vm.lifecycle
            .balloon_config(|config| {
                assert_eq!(config, &BalloonConfig::new(57, true, 10));
            })
            .await;
        let inflated = vm.lifecycle.wait_for_balloon(57).await;
        assert_eq!(inflated.target_mib, 57);
        assert_eq!(inflated.actual_mib, 57);

        vm.lifecycle.update_balloon(0).await;
        let deflated = vm.lifecycle.wait_for_balloon(0).await;
        assert_eq!(deflated.target_mib, 0);
        assert_eq!(deflated.actual_mib, 0);

        fixture.finish().await;
    }

    #[firecracker_test]
    async fn per_vm_api_sockets_support_concurrent_vms_and_repeated_lifecycle_calls() {
        let fixture = FirecrackerFixture::new(ProvisioningConfig::default()).await;

        let first = fixture.create_vm(TestVmConfig::new(1)).await;
        first
            .lifecycle
            .start_concurrently(|lifecycle| {
                assert_eq!(lifecycle.status(), VmStatus::Running);
            })
            .await;
        assert!(first.api_socket.exists());

        let second = fixture.create_vm(TestVmConfig::new(2)).await;
        assert_ne!(first.id, second.id);
        second
            .lifecycle
            .start(|lifecycle| {
                assert_eq!(lifecycle.status(), VmStatus::Running);
            })
            .await;
        assert_ne!(first.api_socket, second.api_socket);
        assert!(second.api_socket.exists());

        first
            .lifecycle
            .shutdown_concurrently(|lifecycle| {
                assert_eq!(lifecycle.status(), VmStatus::Discovered);
            })
            .await;
        assert!(!first.api_socket.exists());
        assert!(second.api_socket.exists());

        first
            .lifecycle
            .start(|lifecycle| {
                assert_eq!(lifecycle.status(), VmStatus::Running);
            })
            .await;
        assert!(first.api_socket.exists());
        assert!(second.api_socket.exists());

        fixture.finish().await;
    }

    #[firecracker_test]
    async fn concurrent_vms_keep_private_disks_across_manager_restart_and_delete_together() {
        let mut fixture = FirecrackerFixture::new(ProvisioningConfig::default()).await;
        let mut vms = fixture
            .create_vms_concurrently::<2>([TestVmConfig::new(1), TestVmConfig::new(1)], |vms| {
                assert_eq!(vms.len(), 2);
                assert_ne!(vms[0].id, vms[1].id);
            })
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
        use std::fs;

        use barbirolli::support::{behavior::BehaviorFixture, config::TestVmConfig};
        use barbirolli::{
            LifecycleError, MemoryMib, NetworkSpec, ProvisioningConfig, VmId, VmStatus,
        };

        #[tokio::test]
        async fn manager_rejects_creation_when_running_vm_capacity_is_reached() {
            let fixture = BehaviorFixture::new();
            let provisioning = ProvisioningConfig {
                count: 0,
                ..ProvisioningConfig::default()
            };
            let manager = fixture.manager_with_provisioning(provisioning).await;

            let result = manager.try_create_vm(TestVmConfig::new(1)).await;

            assert!(matches!(
                result,
                Err(LifecycleError::CapacityReached {
                    running_vms: 0,
                    max_running_vms: 0,
                })
            ));
            assert!(manager.list().await.is_empty());
            assert!(fixture.storage.open().discover().is_empty());
            manager.finish().await;
        }

        #[tokio::test]
        async fn serial_log_reconciliation_preserves_and_repairs_logs() {
            let fixture = BehaviorFixture::new();
            let manager = fixture.manager().await;
            let retained = manager
                .try_create_vm(TestVmConfig::new(1))
                .await
                .expect("failed to create retained-log VM");
            let legacy = manager
                .try_create_vm(TestVmConfig::new(1))
                .await
                .expect("failed to create legacy VM");
            fs::write(retained.storage.serial_log(), b"first boot\n")
                .expect("failed to seed retained serial log");
            fs::remove_file(legacy.storage.serial_log())
                .expect("failed to remove legacy serial log");
            let retained_spec = barbirolli::Barbirolli::vm(&manager, retained.id)
                .expect("missing retained-log VM")
                .spec()
                .clone();
            let legacy_spec = barbirolli::Barbirolli::vm(&manager, legacy.id)
                .expect("missing legacy VM")
                .spec()
                .clone();

            retained_spec
                .ensure_serial_log()
                .expect("failed to reconcile retained serial log");
            legacy_spec
                .ensure_serial_log()
                .expect("failed to reconcile legacy serial log");

            assert_eq!(
                fs::read(retained.storage.serial_log())
                    .expect("failed to read retained serial log"),
                b"first boot\n"
            );
            assert_eq!(
                fs::read(legacy.storage.serial_log()).expect("failed to read repaired serial log"),
                b""
            );
            manager.finish().await;
        }

        #[tokio::test]
        async fn manager_capacity_does_not_count_discovered_or_failed_vms() {
            let fixture = BehaviorFixture::new();
            let provisioning = ProvisioningConfig {
                count: 1,
                ..ProvisioningConfig::default()
            };
            let manager = fixture.manager_with_provisioning(provisioning).await;

            let discovered = manager
                .try_create_vm(TestVmConfig::new(1))
                .await
                .expect("discovered VM should not consume running capacity");
            manager
                .try_create_vm(TestVmConfig::new(1))
                .await
                .expect("another discovered VM should not consume running capacity");
            let failed_spec = barbirolli::Barbirolli::vm(&manager, discovered.id)
                .expect("missing discovered VM")
                .spec()
                .clone();
            manager.vms.insert(
                discovered.id,
                super::super::BarbirolliVm::Failed(failed_spec.clone()),
            );

            manager
                .try_create_vm(TestVmConfig::new(1))
                .await
                .expect("failed VM should not consume running capacity");
            assert_eq!(manager.list().await.len(), 3);
            manager.vms.insert(
                discovered.id,
                super::super::BarbirolliVm::Discovered(failed_spec),
            );
            manager.finish().await;
        }

        #[tokio::test]
        async fn manager_exposes_discovered_state_and_missing_vm_errors() {
            let fixture = BehaviorFixture::new();
            let manager = fixture.manager().await;
            let vm = manager
                .try_create_vm(TestVmConfig::new(1))
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
                .try_create_vm(TestVmConfig::new(1))
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
                        TestVmConfig::new(1),
                        TestVmConfig::new(1),
                        TestVmConfig::new(1),
                        TestVmConfig::new(1),
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
            let next = fixture.storage.open().create_vm(TestVmConfig::new(1)).await;
            assert_eq!(u16::from(next.spec.id), 4);
        }

        #[tokio::test]
        async fn manager_draining_rejects_lifecycle_operations_but_keeps_reads_and_shutdown() {
            let fixture = BehaviorFixture::new();
            let manager = fixture.manager().await;
            let vm = manager
                .try_create_vm(TestVmConfig::new(1))
                .await
                .expect("failed to create manager VM");

            manager.finish().await;

            assert!(matches!(
                manager.try_create_vm(TestVmConfig::new(1)).await,
                Err(LifecycleError::Draining)
            ));
            assert!(matches!(
                manager.delete(vm.id).await,
                Err(LifecycleError::Draining)
            ));

            let start = {
                let mut lifecycle = manager.vm_mut(vm.id).expect("missing manager VM");
                lifecycle.start(&manager).await
            };
            assert!(matches!(start, Err(LifecycleError::Draining)));

            let balloon_config = {
                let mut lifecycle = manager.vm_mut(vm.id).expect("missing manager VM");
                lifecycle.balloon_config(&manager).await
            };
            assert!(matches!(balloon_config, Err(LifecycleError::Draining)));

            let update_balloon = {
                let mut lifecycle = manager.vm_mut(vm.id).expect("missing manager VM");
                lifecycle.update_balloon(&manager, MemoryMib::from(1)).await
            };
            assert!(matches!(update_balloon, Err(LifecycleError::Draining)));

            let balloon_statistics = {
                let mut lifecycle = manager.vm_mut(vm.id).expect("missing manager VM");
                lifecycle.balloon_statistics(&manager).await
            };
            assert!(matches!(balloon_statistics, Err(LifecycleError::Draining)));

            let listed = manager.list().await;
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].id, vm.id);
            assert_eq!(listed[0].status, VmStatus::Discovered);
            assert_eq!(
                barbirolli::Barbirolli::vm(&manager, vm.id)
                    .expect("missing manager VM")
                    .summary()
                    .status,
                VmStatus::Discovered
            );

            let shutdown = {
                let mut lifecycle = manager.vm_mut(vm.id).expect("missing manager VM");
                lifecycle.shutdown(&manager).await
            };
            assert!(shutdown.is_ok());
        }
    }
}
