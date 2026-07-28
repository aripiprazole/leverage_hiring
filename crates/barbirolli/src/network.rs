use std::{
    fmt::{self, Display},
    net::Ipv4Addr,
    str::FromStr,
};

use serde::{Deserialize, Serialize};

use crate::{ParseValueError, VmId};

#[cfg(feature = "linux")]
mod linux;

#[cfg(feature = "linux")]
pub use linux::*;

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

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct InterfaceName(String);

impl FromStr for InterfaceName {
    type Err = ParseValueError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let valid = !value.is_empty()
            && value.len() <= 15
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte));
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(ParseValueError::new("network interface", value))
        }
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
