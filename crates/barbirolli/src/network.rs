use std::{
    fmt::{self, Display},
    net::Ipv4Addr,
    str::FromStr,
};

use serde::{Deserialize, Serialize};

use crate::{ParseValueError, VmId};

#[cfg(any(target_os = "linux", test))]
use crate::{Port, PortBinding};

#[cfg(any(target_os = "linux", test))]
const VM_NETWORK_POOL: Ipv4Addr = Ipv4Addr::new(172, 16, 0, 0);
#[cfg(any(target_os = "linux", test))]
const VM_NETWORK_POOL_PREFIX: u8 = 16;
const BARBIROLLI_ENTRYPOINT: &str = "/barbirolli_entrypoint";

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ipv4Network {
    pub network: Ipv4Addr,
    pub prefix: u8,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PortForward {
    pub external: Port,
    pub guest_ip: Ipv4Addr,
    pub internal: Port,
    pub dest_exclusion: Ipv4Network,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FirewallPlan {
    pub port_forwards: Vec<PortForward>,
    pub forward_rules: Vec<ForwardRule>,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForwardRule {
    /// Permit packets initiated by this VM to leave its TAP through the host's
    /// default external interface.
    AllowVmEgress,
    /// Permit established reply traffic from the host's external interface
    /// back to this VM's TAP.
    AllowEstablishedToVm,
    /// Permit established reply traffic from the VM address pool after this
    /// VM initiated a connection to a peer's published port.
    AllowEstablishedFromPeer,
    /// Permit TCP traffic to a published internal port from either the host's
    /// external interface or another VM.
    AllowPublishedTcp(PortForward),
    /// Reject every other forwarded packet targeting this VM's TAP.
    DropUnmatchedToVm,
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
    /// Builds the deterministic network allocation for `vm_id`.
    ///
    /// # Errors
    ///
    /// Returns an error if the derived TAP interface name is invalid.
    pub fn new(vm_id: VmId) -> std::result::Result<Self, ParseValueError> {
        let id = u16::from(vm_id);
        let offset = id * 4;
        let [third, fourth] = offset.to_be_bytes();
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

    #[must_use]
    pub fn subnet_cidr(&self) -> String {
        format!("{}/30", self.subnet)
    }

    #[must_use]
    pub fn kernel_boot_args(&self) -> String {
        format!(
            "keep_bootcon console=ttyS0 reboot=k panic=1 init={BARBIROLLI_ENTRYPOINT} ip={}::{}:255.255.255.252::eth0:off nameserver=1.1.1.1",
            self.guest_ip, self.host_ip
        )
    }

    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn port_forward(&self, binding: PortBinding) -> PortForward {
        PortForward {
            internal: binding.internal,
            external: binding.external,
            guest_ip: self.guest_ip,
            dest_exclusion: Ipv4Network {
                network: VM_NETWORK_POOL,
                prefix: VM_NETWORK_POOL_PREFIX,
            },
        }
    }

    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn firewall_plan(&self, bindings: &[PortBinding]) -> FirewallPlan {
        let port_forwards = bindings
            .iter()
            .copied()
            .map(|binding| self.port_forward(binding))
            .collect::<Vec<_>>();
        let mut forward_rules = vec![
            ForwardRule::AllowVmEgress,
            ForwardRule::AllowEstablishedToVm,
            ForwardRule::AllowEstablishedFromPeer,
        ];
        forward_rules.extend(
            port_forwards
                .iter()
                .copied()
                .map(ForwardRule::AllowPublishedTcp),
        );
        forward_rules.push(ForwardRule::DropUnmatchedToVm);
        FirewallPlan {
            port_forwards,
            forward_rules,
        }
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

#[cfg(test)]
mod tests {
    use super::{
        FirewallPlan, ForwardRule, Ipv4Network, NetworkSpec, Port, PortBinding, PortForward,
    };
    use crate::VmId;
    use std::net::Ipv4Addr;

    #[test]
    fn network_allocation_is_stable() {
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
            "keep_bootcon console=ttyS0 reboot=k panic=1 init=/barbirolli_entrypoint ip=172.16.0.2::172.16.0.1:255.255.255.252::eth0:off nameserver=1.1.1.1"
        );

        let last = NetworkSpec::new(VmId::try_from(16_383).expect("valid VM ID"))
            .expect("failed to allocate the last network");
        assert_eq!(last.tap.as_ref(), "fc-tap16383");
        assert_eq!(last.subnet, Ipv4Addr::new(172, 16, 255, 252));
        assert_eq!(last.host_ip, Ipv4Addr::new(172, 16, 255, 253));
        assert_eq!(last.guest_ip, Ipv4Addr::new(172, 16, 255, 254));
        assert_eq!(last.guest_mac, "06:00:ac:10:ff:fe");
    }

    #[test]
    fn external_port_is_forwarded_to_the_internal_guest_port() {
        let network =
            NetworkSpec::new(VmId::try_from(0).expect("valid VM ID")).expect("valid network");
        let binding = PortBinding {
            internal: Port::try_from(22).expect("valid internal port"),
            external: Port::try_from(2222).expect("valid external port"),
        };

        assert_eq!(
            network.port_forward(binding),
            PortForward {
                external: Port::try_from(2222).expect("valid external port"),
                guest_ip: Ipv4Addr::new(172, 16, 0, 2),
                internal: Port::try_from(22).expect("valid internal port"),
                dest_exclusion: Ipv4Network {
                    network: Ipv4Addr::new(172, 16, 0, 0),
                    prefix: 16,
                },
            }
        );
    }

    #[test]
    fn firewall_allows_published_tcp_before_dropping_other_inbound_traffic() {
        let network =
            NetworkSpec::new(VmId::try_from(0).expect("valid VM ID")).expect("valid network");
        let binding = PortBinding {
            internal: Port::try_from(22).expect("valid internal port"),
            external: Port::try_from(2222).expect("valid external port"),
        };
        let forward = PortForward {
            external: Port::try_from(2222).expect("valid external port"),
            guest_ip: Ipv4Addr::new(172, 16, 0, 2),
            internal: Port::try_from(22).expect("valid internal port"),
            dest_exclusion: Ipv4Network {
                network: Ipv4Addr::new(172, 16, 0, 0),
                prefix: 16,
            },
        };

        assert_eq!(
            network.firewall_plan(&[binding]),
            FirewallPlan {
                port_forwards: vec![forward],
                forward_rules: vec![
                    ForwardRule::AllowVmEgress,
                    ForwardRule::AllowEstablishedToVm,
                    ForwardRule::AllowEstablishedFromPeer,
                    ForwardRule::AllowPublishedTcp(forward),
                    ForwardRule::DropUnmatchedToVm,
                ],
            }
        );
    }
}
