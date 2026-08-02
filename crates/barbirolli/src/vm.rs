use std::{
    fmt::{self, Display},
    path::PathBuf,
    str::FromStr,
};

use derive_more::{Display, From, Into};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::NetworkSpec;

#[cfg(target_os = "linux")]
pub(crate) mod managed;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmInput {
    pub vcpu_count: VcpuCount,
    #[serde(default)]
    pub authorized_keys: Vec<AuthorizedKey>,
    #[serde(default)]
    pub bindings: Vec<PortBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmSpec {
    pub id: VmId,
    #[serde(default)]
    pub deleted: bool,
    pub artifact_dir: PathBuf,
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
    pub vcpu_count: VcpuCount,
    pub api_socket: ApiSocket,
    pub memory_mib: MemoryMib,
    #[serde(default)]
    pub bindings: Vec<PortBinding>,
    pub network: NetworkSpec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortBinding {
    pub internal: Port,
    pub external: Port,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Into, From,
)]
#[serde(transparent)]
pub struct ApiSocket(PathBuf);

impl ApiSocket {
    fn remove(&self) -> Result<(), std::io::Error> {
        let api_socket: PathBuf = self.0.clone().into();
        match std::fs::remove_file(api_socket) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        Ok(())
    }
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

#[derive(
    Debug, Clone, Copy, PartialEq, From, Into, Eq, Hash, PartialOrd, Ord, Display, Serialize,
)]
#[serde(transparent)]
#[display("{_0}")]
pub struct MemoryMib(u16);

impl<'de> Deserialize<'de> for MemoryMib {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self::from(u16::deserialize(deserializer)?))
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

#[cfg(test)]
mod tests {
    use barbirolli::AuthorizedKey;

    const AUTHORIZED_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti user@example.com";

    #[test]
    fn authorized_key_parsing_is_strict() {
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
}
