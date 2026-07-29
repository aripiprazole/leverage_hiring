use std::{
    fmt::{self, Display},
    path::PathBuf,
    str::FromStr,
};

use derive_more::Display;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::NetworkSpec;

#[cfg(feature = "linux")]
pub(crate) mod managed;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmInput {
    pub user: UserName,
    pub vcpu_count: VcpuCount,
    pub memory_mib: MemoryMib,
    #[serde(default)]
    pub authorized_keys: Vec<AuthorizedKey>,
    #[serde(default)]
    pub port_bindings: Vec<PortBinding>,
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
    #[serde(default)]
    pub port_bindings: Vec<PortBinding>,
    pub network: NetworkSpec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortBinding {
    pub internal: Port,
    pub external: Port,
}

/// TCP only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Display, Serialize)]
#[serde(transparent)]
#[display("{_0}")]
pub struct Port(u16);

impl TryFrom<u16> for Port {
    type Error = ParseValueError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        (value != 0)
            .then_some(Self(value))
            .ok_or_else(|| ParseValueError::new("network port", value.to_string()))
    }
}

impl From<Port> for u16 {
    fn from(value: Port) -> Self {
        value.0
    }
}

impl<'de> Deserialize<'de> for Port {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_from(u16::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Display, Serialize)]
#[serde(transparent)]
#[display("{_0}")]
pub struct UserName(String);

impl FromStr for UserName {
    type Err = ParseValueError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
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

impl AsRef<str> for UserName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for UserName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Display, Serialize)]
#[serde(transparent)]
#[display("{_0}")]
pub struct AuthorizedKey(String);

impl FromStr for AuthorizedKey {
    type Err = ParseValueError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        (!value.contains(['\n', '\r']) && ssh_key::PublicKey::from_openssh(value).is_ok())
            .then(|| Self(value.to_owned()))
            .ok_or_else(|| ParseValueError::new("authorized key", value))
    }
}

impl AsRef<str> for AuthorizedKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for AuthorizedKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
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

    fn try_from(value: u16) -> Result<Self, Self::Error> {
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

impl Serialize for VmId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for VmId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_from(u16::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

impl Display for VmId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Display, Serialize)]
#[serde(transparent)]
#[display("{_0}")]
pub struct VcpuCount(u8);

impl TryFrom<u8> for VcpuCount {
    type Error = ParseValueError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
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

impl<'de> Deserialize<'de> for VcpuCount {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_from(u8::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Display, Serialize)]
#[serde(transparent)]
#[display("{_0}")]
pub struct MemoryMib(u16);

impl TryFrom<u16> for MemoryMib {
    type Error = ParseValueError;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
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

impl<'de> Deserialize<'de> for MemoryMib {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_from(u16::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind}: {value:?}")]
pub struct ParseValueError {
    kind: &'static str,
    value: String,
}

impl ParseValueError {
    pub(crate) fn new(kind: &'static str, value: impl Into<String>) -> Self {
        Self {
            kind,
            value: value.into(),
        }
    }
}
