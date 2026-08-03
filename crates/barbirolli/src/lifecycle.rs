#[cfg(any(target_os = "linux", test))]
use std::path::Path;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JailerConfig {
    pub jailer: PathBuf,
    pub chroot_base: PathBuf,
    uid_base: u32,
    gid_base: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JailerIdentity {
    pub uid: u32,
    pub gid: u32,
}

const MAX_VM_ID: u32 = 16_383;
#[cfg(any(target_os = "linux", test))]
pub(crate) const JAILED_API_SOCKET: &str = "/run/firecracker.socket";

impl JailerConfig {
    /// Creates a stable per-VM jailer identity range.
    ///
    /// # Errors
    ///
    /// Returns an error when either range includes UID/GID 0, overflows, or
    /// reaches the reserved `u32::MAX` sentinel value.
    pub fn new(
        jailer: impl Into<PathBuf>,
        chroot_base: impl Into<PathBuf>,
        uid_base: u32,
        gid_base: u32,
    ) -> Result<Self, JailerConfigError> {
        Ok(Self {
            jailer: jailer.into(),
            chroot_base: chroot_base.into(),
            uid_base: Self::validate_identity_base("UID", uid_base)?,
            gid_base: Self::validate_identity_base("GID", gid_base)?,
        })
    }

    /// Parses the identity bases supplied by configuration.
    ///
    /// Parsing is kept separate from the typed constructor so callers cannot
    /// accidentally treat an unparsed configuration value as a valid UID/GID.
    ///
    /// # Errors
    ///
    /// Returns an error if a value is not a valid identity base.
    pub fn parse(
        jailer: impl Into<PathBuf>,
        chroot_base: impl Into<PathBuf>,
        uid_base: impl AsRef<str>,
        gid_base: impl AsRef<str>,
    ) -> Result<Self, JailerConfigError> {
        Self::new(
            jailer,
            chroot_base,
            Self::parse_identity_base("UID", uid_base.as_ref())?,
            Self::parse_identity_base("GID", gid_base.as_ref())?,
        )
    }

    #[must_use]
    pub fn identity(&self, vm_id: VmId) -> JailerIdentity {
        let offset = u32::from(u16::from(vm_id));
        JailerIdentity {
            uid: self.uid_base + offset,
            gid: self.gid_base + offset,
        }
    }

    #[must_use]
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn jail_root(&self, firecracker: &Path, vm_id: VmId) -> PathBuf {
        self.chroot_base
            .join(
                firecracker
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("firecracker"),
            )
            .join(format!("barbirolli-{vm_id}"))
            .join("root")
    }

    fn parse_identity_base(kind: &'static str, value: &str) -> Result<u32, JailerConfigError> {
        value
            .parse()
            .map_err(|_| JailerConfigError::InvalidIdentityBase {
                kind,
                value: value.to_owned(),
            })
    }

    fn validate_identity_base(kind: &'static str, base: u32) -> Result<u32, JailerConfigError> {
        if base == 0 {
            return Err(JailerConfigError::RootIdentity { kind });
        }
        if base
            .checked_add(MAX_VM_ID)
            .is_none_or(|last| last == u32::MAX)
        {
            return Err(JailerConfigError::IdentityRangeOverflow { kind, base });
        }
        Ok(base)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum JailerConfigError {
    #[error("the jailer {kind} base must be an unsigned integer, got {value:?}")]
    InvalidIdentityBase { kind: &'static str, value: String },
    #[error("the jailer {kind} range must not include root")]
    RootIdentity { kind: &'static str },
    #[error("the jailer {kind} base {base} cannot represent every VM identity")]
    IdentityRangeOverflow { kind: &'static str, base: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FirecrackerExecutorConfig {
    Jailed(JailerConfig),
    Unrestricted,
}

impl FirecrackerExecutorConfig {
    #[must_use]
    #[cfg(target_os = "linux")]
    pub(crate) fn identity(&self, vm_id: VmId) -> Option<JailerIdentity> {
        match self {
            Self::Jailed(config) => Some(config.identity(vm_id)),
            Self::Unrestricted => None,
        }
    }

    #[must_use]
    #[cfg(target_os = "linux")]
    pub(crate) fn jailer(&self) -> Option<&JailerConfig> {
        match self {
            Self::Jailed(config) => Some(config),
            Self::Unrestricted => None,
        }
    }
}

#[derive(Debug)]
pub struct DaemonConfig {
    pub provisioning: ProvisioningConfig,
    pub firecracker: PathBuf,
    pub firecracker_executor: FirecrackerExecutorConfig,
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
    #[cfg(target_os = "linux")]
    use barbirolli::FirecrackerExecutorConfig;
    use barbirolli::support;
    use barbirolli::{
        BalloonConfig, JailerConfig, JailerConfigError, JailerIdentity, LifecycleError,
        ProvisioningConfig, VmId, VmStatus,
    };
    use barbirolli_derive::firecracker_test;

    use super::MAX_VM_ID;
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

    #[test]
    fn jailer_identity_is_stable_and_vm_specific() {
        let config = JailerConfig::new("/jailer", "/srv/jailer/barbirolli", 100_000, 200_000)
            .expect("valid jailer identity range");

        assert_eq!(
            config.identity(VmId::try_from(42).expect("valid VM ID")),
            JailerIdentity {
                uid: 100_042,
                gid: 200_042,
            }
        );
    }

    #[test]
    fn jailer_identity_range_rejects_root_and_reserved_values() {
        assert_eq!(
            JailerConfig::new("/jailer", "/srv/jailer/barbirolli", 0, 1),
            Err(JailerConfigError::RootIdentity { kind: "UID" })
        );
        assert_eq!(
            JailerConfig::new("/jailer", "/srv/jailer/barbirolli", 1, u32::MAX - MAX_VM_ID),
            Err(JailerConfigError::IdentityRangeOverflow {
                kind: "GID",
                base: u32::MAX - MAX_VM_ID,
            })
        );
    }

    #[test]
    fn jailer_identity_bases_parse_before_validation() {
        assert_eq!(
            JailerConfig::parse("/jailer", "/srv/jailer/barbirolli", "not-a-uid", "200000"),
            Err(JailerConfigError::InvalidIdentityBase {
                kind: "UID",
                value: "not-a-uid".to_owned(),
            })
        );
        assert_eq!(
            JailerConfig::parse("/jailer", "/srv/jailer/barbirolli", "0", "200000"),
            Err(JailerConfigError::RootIdentity { kind: "UID" })
        );
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
        assert_eq!(
            first.logical_api_socket,
            first.storage.artifact_dir.join("firecracker.socket")
        );
        first
            .lifecycle
            .start_concurrently(|lifecycle| {
                assert_eq!(lifecycle.status(), VmStatus::Running);
            })
            .await;
        assert!(first.api_socket.exists());

        let second = fixture.create_vm(TestVmConfig::new(2)).await;
        assert_ne!(first.id, second.id);
        assert_eq!(
            second.logical_api_socket,
            second.storage.artifact_dir.join("firecracker.socket")
        );
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

    #[cfg(target_os = "linux")]
    #[firecracker_test]
    async fn jailed_executor_drops_identity_and_uses_the_precreated_cgroup() {
        use std::os::unix::fs::MetadataExt as _;

        let fixture = FirecrackerFixture::new(ProvisioningConfig::default()).await;
        let FirecrackerExecutorConfig::Jailed(config) = &fixture.firecracker_executor else {
            return;
        };
        let vm = fixture.create_vm(TestVmConfig::new(1)).await;
        let identity = config.identity(vm.id);

        vm.lifecycle.start(|_| {}).await;
        let pid = {
            let state = fixture.manager.vm(vm.id).expect("missing running VM");
            let super::BarbirolliVm::Managed(managed) = state.value() else {
                panic!("expected a managed VM");
            };
            managed.pid.as_raw_pid()
        };

        let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .expect("failed to read Firecracker process status");
        assert_eq!(status_identities(&status, "Uid:"), [identity.uid; 4]);
        assert_eq!(status_identities(&status, "Gid:"), [identity.gid; 4]);

        let cgroup = format!("/sys/fs/cgroup/barbirolli/vm-{}/cgroup.procs", vm.id);
        let cgroup_pids = std::fs::read_to_string(cgroup)
            .expect("failed to read the VM cgroup")
            .lines()
            .map(str::parse::<i32>)
            .collect::<Result<Vec<_>, _>>()
            .expect("invalid cgroup PID");
        assert_eq!(cgroup_pids, [pid]);
        assert!(!vm.logical_api_socket.exists());
        assert!(vm.api_socket.starts_with(&config.chroot_base));
        assert_eq!(
            std::fs::metadata(&vm.api_socket)
                .expect("missing effective API socket")
                .uid(),
            identity.uid
        );

        fixture.finish().await;
    }

    #[cfg(target_os = "linux")]
    #[firecracker_test]
    async fn jailed_prepare_failure_rolls_back_the_jail_cgroup_and_network() {
        let fixture = FirecrackerFixture::new(ProvisioningConfig::default()).await;
        let FirecrackerExecutorConfig::Jailed(config) = &fixture.firecracker_executor else {
            return;
        };
        let vm = fixture.create_vm(TestVmConfig::new(1)).await;
        let jail = config
            .jail_root(&fixture.firecracker, vm.id)
            .parent()
            .expect("the jail root has an ID parent")
            .to_owned();
        let cgroup = std::path::PathBuf::from(format!("/sys/fs/cgroup/barbirolli/vm-{}", vm.id));
        let tap = std::path::PathBuf::from("/sys/class/net").join(vm.network.spec.tap.as_ref());
        std::fs::remove_file(vm.storage.kernel()).expect("failed to remove the fixture kernel");

        {
            let mut state = fixture.manager.vm_mut(vm.id).expect("missing fixture VM");
            state
                .start(&fixture.manager)
                .await
                .expect_err("the missing kernel should fail startup");
        }

        assert_eq!(vm.lifecycle.status(), VmStatus::Failed);
        assert!(!cgroup.exists());
        assert!(!jail.exists());
        assert!(!tap.exists());
        assert!(!vm.logical_api_socket.exists());
        assert!(!vm.api_socket.exists());

        fixture.finish().await;
    }

    #[cfg(target_os = "linux")]
    fn status_identities(status: &str, key: &str) -> [u32; 4] {
        status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .expect("missing process identity status")
            .split_ascii_whitespace()
            .map(str::parse::<u32>)
            .collect::<Result<Vec<_>, _>>()
            .expect("invalid process identity status")
            .try_into()
            .expect("expected four process identities")
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
            let manager = fixture.new_manager(provisioning).await;

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
            let manager = fixture.new_manager(ProvisioningConfig::default()).await;
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
            let manager = fixture.new_manager(provisioning).await;

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
            let manager = fixture.new_manager(ProvisioningConfig::default()).await;
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
            let manager = fixture.new_manager(ProvisioningConfig::default()).await;
            let vm = manager
                .try_create_vm(TestVmConfig::new(1))
                .await
                .expect("failed to create manager VM");

            vm.lifecycle.delete_with_timeout();
            assert!(manager.list().await.is_empty());
            assert!(vm.storage.kernel().exists());
            assert!(vm.storage.rootfs().exists());
            assert_eq!(vm.storage.config()["spec"]["deleted"], true);

            let reopened = fixture.new_manager(ProvisioningConfig::default()).await;
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
            let manager = fixture.new_manager(ProvisioningConfig::default()).await;
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
            let manager = fixture.new_manager(ProvisioningConfig::default()).await;
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
