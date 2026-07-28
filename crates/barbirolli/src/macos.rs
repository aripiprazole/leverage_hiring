use std::{
    fmt::{self, Display},
    io,
    net::Ipv4Addr,
    path::PathBuf,
    str::FromStr,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

pub type Result<T, E = LifecycleError> = std::result::Result<T, E>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind}: {value:?}")]
pub struct ParseValueError {
    kind: &'static str,
    value: String,
}

impl ParseValueError {
    fn new(kind: &'static str, value: impl Into<String>) -> Self {
        Self {
            kind,
            value: value.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UserName(String);

impl FromStr for UserName {
    type Err = ParseValueError;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let is_safe = !value.is_empty()
            && value != "."
            && value != ".."
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte));
        is_safe
            .then(|| Self(value.to_owned()))
            .ok_or_else(|| ParseValueError::new("user name", value))
    }
}

impl Display for UserName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl AsRef<str> for UserName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Serialize for UserName {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for UserName {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArtifactName(String);

impl FromStr for ArtifactName {
    type Err = ParseValueError;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let is_safe = !value.is_empty()
            && value != "."
            && value != ".."
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte));
        is_safe
            .then(|| Self(value.to_owned()))
            .ok_or_else(|| ParseValueError::new("artifact name", value))
    }
}

impl Display for ArtifactName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl AsRef<str> for ArtifactName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Serialize for ArtifactName {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ArtifactName {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AuthorizedKey(String);

impl FromStr for AuthorizedKey {
    type Err = ParseValueError;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let is_safe =
            !value.contains(['\n', '\r']) && ssh_key::PublicKey::from_openssh(value).is_ok();
        is_safe
            .then(|| Self(value.to_owned()))
            .ok_or_else(|| ParseValueError::new("authorized key", value))
    }
}

impl Display for AuthorizedKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl AsRef<str> for AuthorizedKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Serialize for AuthorizedKey {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for AuthorizedKey {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VmId(u16);

impl TryFrom<u16> for VmId {
    type Error = ParseValueError;

    fn try_from(value: u16) -> std::result::Result<Self, Self::Error> {
        (value <= 16_383)
            .then_some(Self(value))
            .ok_or_else(|| ParseValueError::new("VM ID", value.to_string()))
    }
}

impl From<VmId> for u16 {
    fn from(value: VmId) -> Self {
        value.0
    }
}

impl Display for VmId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Serialize for VmId {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for VmId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_from(u16::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VcpuCount(u8);

impl TryFrom<u8> for VcpuCount {
    type Error = ParseValueError;

    fn try_from(value: u8) -> std::result::Result<Self, Self::Error> {
        (1..=31)
            .contains(&value)
            .then_some(Self(value))
            .ok_or_else(|| ParseValueError::new("vCPU count", value.to_string()))
    }
}

impl From<VcpuCount> for u8 {
    fn from(value: VcpuCount) -> Self {
        value.0
    }
}

impl Serialize for VcpuCount {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for VcpuCount {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_from(u8::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MemoryMib(u16);

impl TryFrom<u16> for MemoryMib {
    type Error = ParseValueError;

    fn try_from(value: u16) -> std::result::Result<Self, Self::Error> {
        (1..=2_047)
            .contains(&value)
            .then_some(Self(value))
            .ok_or_else(|| ParseValueError::new("memory MiB", value.to_string()))
    }
}

impl From<MemoryMib> for u16 {
    fn from(value: MemoryMib) -> Self {
        value.0
    }
}

impl Serialize for MemoryMib {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for MemoryMib {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_from(u16::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InterfaceName(String);

impl FromStr for InterfaceName {
    type Err = ParseValueError;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let is_safe = !value.is_empty()
            && value.len() <= 15
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte));
        is_safe
            .then(|| Self(value.to_owned()))
            .ok_or_else(|| ParseValueError::new("network interface", value))
    }
}

impl Display for InterfaceName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl AsRef<str> for InterfaceName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkSpec {
    pub vm_id: VmId,
    pub tap: InterfaceName,
    pub subnet: Ipv4Addr,
    pub host_ip: Ipv4Addr,
    pub guest_ip: Ipv4Addr,
    pub guest_mac: String,
}

impl NetworkSpec {
    pub fn new(vm_id: VmId) -> std::result::Result<Self, ParseValueError> {
        let id = u16::from(vm_id);
        let offset = id * 4;
        let third = (offset / 256) as u8;
        let fourth = (offset % 256) as u8;
        let guest = fourth + 2;
        Ok(Self {
            vm_id,
            tap: format!("fc-tap{id}").parse()?,
            subnet: Ipv4Addr::new(172, 16, third, fourth),
            host_ip: Ipv4Addr::new(172, 16, third, fourth + 1),
            guest_ip: Ipv4Addr::new(172, 16, third, guest),
            guest_mac: format!("06:00:ac:10:{third:02x}:{guest:02x}"),
        })
    }

    pub fn subnet_cidr(&self) -> String {
        format!("{}/30", self.subnet)
    }

    pub fn kernel_boot_args(&self) -> String {
        format!(
            "keep_bootcon console=ttyS0 reboot=k panic=1 ip={}::{}:255.255.255.252::eth0:off nameserver=1.1.1.1",
            self.guest_ip, self.host_ip
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmInput {
    pub user: UserName,
    pub vcpu_count: VcpuCount,
    pub memory_mib: MemoryMib,
    pub kernel: ArtifactName,
    pub rootfs: ArtifactName,
    #[serde(default)]
    pub authorized_keys: Vec<AuthorizedKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmSpec {
    pub id: VmId,
    pub user: UserName,
    pub artifact_dir: PathBuf,
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
    pub vcpu_count: VcpuCount,
    pub memory_mib: MemoryMib,
    pub network: NetworkSpec,
}

#[derive(Debug, Clone)]
pub struct VmStore;

impl VmStore {
    pub fn new(
        _vm_root: PathBuf,
        _image_root: PathBuf,
        _default_authorized_keys: PathBuf,
    ) -> std::result::Result<Self, StorageError> {
        Ok(Self)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("invalid VM input: {0}")]
    InvalidInput(String),
    #[error("user {0} already owns a VM")]
    DuplicateUser(UserName),
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
    pub user: UserName,
    pub status: VmStatus,
}

pub struct BarbirolliVm;

impl BarbirolliVm {
    pub fn spec(&self) -> &VmSpec {
        unreachable!("Barbirolli VMs require the linux feature")
    }

    pub fn summary(&self) -> VmSummary {
        unreachable!("Barbirolli VMs require the linux feature")
    }

    pub async fn start(&mut self, _barbirolli: &Barbirolli) -> Result<()> {
        Err(LifecycleError::UnsupportedPlatform)
    }

    pub async fn shutdown(&mut self, _barbirolli: &Barbirolli) -> Result<()> {
        Err(LifecycleError::UnsupportedPlatform)
    }
}

#[derive(Debug, Clone)]
pub struct Barbirolli;

impl Barbirolli {
    pub async fn new(
        _store: VmStore,
        _firecracker: impl Into<PathBuf>,
    ) -> Result<Self, LifecycleError> {
        Err(LifecycleError::UnsupportedPlatform)
    }

    pub async fn create(&self, _input: VmInput) -> Result<VmId, LifecycleError> {
        Err(LifecycleError::UnsupportedPlatform)
    }

    pub async fn list(&self) -> Vec<VmSummary> {
        Vec::new()
    }

    pub fn vm(&self, _vm_id: VmId) -> Result<BarbirolliVm, LifecycleError> {
        Err(LifecycleError::UnsupportedPlatform)
    }

    pub async fn authorized_keys_path(&self, _user: &str) -> Option<PathBuf> {
        None
    }

    pub async fn ssh_address(&self, _user: &str) -> Option<Ipv4Addr> {
        None
    }

    pub async fn delete(&self, _vm_id: VmId) -> Result<(), LifecycleError> {
        Err(LifecycleError::UnsupportedPlatform)
    }

    pub async fn shutdown_all(&self) -> Result<(), LifecycleError> {
        Err(LifecycleError::UnsupportedPlatform)
    }
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
    #[error("warmup reconciliation failed: {0:?}")]
    Warmup(Vec<String>),
    #[error("application shutdown failed: {0:?}")]
    Shutdown(Vec<String>),
    #[error("Firecracker lifecycle operations require Linux")]
    UnsupportedPlatform,
}

pub mod test_support {
    use std::sync::{
        Mutex, MutexGuard,
        atomic::{AtomicU16, Ordering},
    };

    use super::VmId;

    static FIRECRACKER_TEST_LOCK: Mutex<()> = Mutex::new(());
    static NEXT_FIRECRACKER_VM_ID: AtomicU16 = AtomicU16::new(12_000);

    pub fn lock_firecracker_tests() -> MutexGuard<'static, ()> {
        FIRECRACKER_TEST_LOCK
            .lock()
            .expect("the Firecracker test lock was poisoned")
    }

    pub fn next_firecracker_vm_id() -> VmId {
        VmId::try_from(NEXT_FIRECRACKER_VM_ID.fetch_add(1, Ordering::Relaxed))
            .expect("the Firecracker test VM ID pool was exhausted")
    }
}
