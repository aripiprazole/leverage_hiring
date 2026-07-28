use std::{
    fmt::{self, Display},
    net::Ipv4Addr,
    path::PathBuf,
    str::FromStr,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

#[cfg(feature = "linux")]
use nlink::{
    netlink::{
        Connection, Nftables, Route,
        nftables::{Chain, ChainType, CtState, Family, Hook, Policy, Priority, Rule},
    },
    tuntap::{Mode, TunTap},
};

use crate::{ParseValueError, VmId};

const NETWORK_PREFIX: u8 = 30;
const MAIN_ROUTE_TABLE: u32 = 254;
const VM_NETWORK_POOL: Ipv4Addr = Ipv4Addr::new(172, 16, 0, 0);
const VM_NETWORK_POOL_PREFIX: u8 = 16;

pub type Result<T, E = NetworkError> = std::result::Result<T, E>;

#[cfg(feature = "linux")]
pub struct ManagedNetwork {
    spec: NetworkSpec,
    table: String,
}

#[cfg(feature = "linux")]
impl ManagedNetwork {
    pub async fn cleanup(&self) -> Result<()> {
        self.spec.cleanup(&self.table).await
    }
}

#[cfg(any(feature = "linux", test))]
#[derive(Clone, Copy)]
struct DefaultRouteCandidate {
    is_ipv4: bool,
    is_default: bool,
    metric: Option<u32>,
    output_interface: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkSpec {
    vm_id: VmId,
    tap: InterfaceName,
    subnet: Ipv4Addr,
    host_ip: Ipv4Addr,
    guest_ip: Ipv4Addr,
    guest_mac: String,
}

impl NetworkSpec {
    pub fn new(vm_id: VmId) -> Result<Self> {
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

    async fn setup(
        &self,
        route: &Connection<Route>,
        nftables_conn: &Connection<Nftables>,
        host: &InterfaceName,
        table: &str,
    ) -> Result<()> {
        let tap = route_conn
            .get_link_by_name(spec.tap_name().as_ref())
            .await?
            .ok_or_else(|| NetworkError::InterfaceNotFound(spec.tap_name.to_string()))?;
        route_conn
            .add_address_by_index(tap.ifindex(), spec.host_ip().into(), NETWORK_PREFIX)
            .await?;
        route_conn.set_link_up_by_index(tap.ifindex()).await?;
        tokio::fs::write("/proc/sys/net/ipv4/ip_forward", b"1\n").await?;
        self.install_rules(&nftables_conn, &host, &table).await
    }

    #[cfg(feature = "linux")]
    pub async fn prepare(&self) -> Result<ManagedNetwork> {
        let table = format!("fc_vm_{}", spec.vm_id);
        let route_conn = Connection::<Route>::new()?;

        ensure_pool_available(&route_conn).await?;

        let routes = route_conn.get_routes_for_table(MAIN_ROUTE_TABLE).await?;
        let candidates = routes
            .iter()
            .map(|route| DefaultRouteCandidate {
                is_ipv4: route.is_ipv4(),
                is_default: route.is_default(),
                metric: route.priority(),
                output_interface: route.oif(),
            })
            .collect::<Vec<_>>();
        let output = select_default_route(&candidates).ok_or(NetworkError::MissingDefaultRoute)?;
        let link = route_conn
            .get_link_by_index(output)
            .await?
            .ok_or_else(|| NetworkError::InterfaceNotFound(format!("ifindex {output}")))?;
        let host = link
            .name()
            .ok_or_else(|| NetworkError::InterfaceNotFound(format!("ifindex {output}")))?
            .parse::<InterfaceName>()
            .map_err(|error| NetworkError::InterfaceNotFound(error.to_string()))?;

        let nftables_conn = Connection::<Nftables>::new()?;
        nftables_conn
            .del_table_if_exists(table.as_str(), Family::Ip)
            .await?;
        route_conn.del_link_if_exists(&self.tap).await?;

        TunTap::builder()
            .name(&self.tap)
            .mode(Mode::Tap)
            .create_persistent()?;

        match self.setup(&route_conn, &nftables_conn, &host, &table) {
            Ok(()) => Ok(ManagedNetwork {
                spec: spec.clone(),
                table,
            }),
            Err(err) => {
                match cleanup_with_connections(&route_conn, &nftables_conn, spec, &table).await {
                    Ok(()) => Err(err),
                    Err(rollback) => Err(NetworkError::SetupRollback {
                        setup: Box::new(err),
                        rollback: Box::new(rollback),
                    }),
                }
            }
        }
    }

    #[cfg(feature = "linux")]
    async fn cleanup_with_connections(
        &self,
        route: &Connection<Route>,
        nftables: &Connection<Nftables>,
        table: &str,
    ) -> Result<(), NetworkError> {
        let nftables = nftables
            .del_table_if_exists(table, Family::Ip)
            .await
            .map(|_| ())
            .map_err(NetworkError::from);
        let tap = route
            .del_link_if_exists(&self.tap)
            .await
            .map(|_| ())
            .map_err(NetworkError::from);
        match (nftables, tap) {
            (Ok(()), Ok(())) => Ok(()),
            (nftables, tap) => Err(NetworkError::Cleanup {
                nftables: nftables.err().map(Box::new),
                tap: tap.err().map(Box::new),
            }),
        }
    }

    #[cfg(feature = "linux")]
    async fn cleanup(&self, table: &str) -> Result<(), NetworkError> {
        let nftables = match Connection::<Nftables>::new() {
            Ok(connection) => connection
                .del_table_if_exists(table, Family::Ip)
                .await
                .map(|_| ())
                .map_err(NetworkError::from),
            Err(error) => Err(NetworkError::from(error)),
        };
        let tap = match Connection::<Route>::new() {
            Ok(connection) => connection
                .del_link_if_exists(&self.tap)
                .await
                .map(|_| ())
                .map_err(NetworkError::from),
            Err(error) => Err(NetworkError::from(error)),
        };
        match (nftables, tap) {
            (Ok(()), Ok(())) => Ok(()),
            (nftables, tap) => Err(NetworkError::Cleanup {
                nftables: nftables.err().map(Box::new),
                tap: tap.err().map(Box::new),
            }),
        }
    }

    async fn cleanup_stale(&self) -> Result<(), NetworkError> {
        self.cleanup(&format!("fc_vm_{}", spec.vm_id)).await
    }

    async fn install_rules(
        &self,
        conn: &Connection<Nftables>,
        host: &InterfaceName,
        table: &str,
    ) -> Result<(), NetworkError> {
        let forward_chain = Chain::new(table, "forward")?
            .family(Family::Ip)
            .hook(Hook::Forward)
            .priority(Priority::Filter)
            .chain_type(ChainType::Filter)
            .policy(Policy::Accept);
        let postrouting_chain = Chain::new(table, "postrouting")?
            .family(Family::Ip)
            .hook(Hook::Postrouting)
            .priority(Priority::SrcNat)
            .chain_type(ChainType::Nat)
            .policy(Policy::Accept);
        let isolate = Rule::new(table, "forward")
            .family(Family::Ip)
            .match_iif(spec.tap_name().as_ref())
            .match_daddr_v4(VM_NETWORK_POOL, VM_NETWORK_POOL_PREFIX)
            .drop()
            .comment("vm-isolation");
        let outbound = Rule::new(table, "forward")
            .family(Family::Ip)
            .match_iif(spec.tap_name().as_ref())
            .match_oif(host.as_ref())
            .accept();
        let established = Rule::new(table, "forward")
            .family(Family::Ip)
            .match_iif(host.as_ref())
            .match_oif(spec.tap_name().as_ref())
            .match_ct_state(CtState::ESTABLISHED | CtState::RELATED)
            .accept();
        let masquerade = Rule::new(table, "postrouting")
            .family(Family::Ip)
            .match_saddr_v4(spec.subnet(), NETWORK_PREFIX)
            .match_oif(host.as_ref())
            .masquerade();

        conn.transaction()
            .add_table(table, Family::Ip)
            .add_chain(forward_chain)
            .add_chain(postrouting_chain)
            .add_rule(isolate)
            .add_rule(outbound)
            .add_rule(established)
            .add_rule(masquerade)
            .commit(conn)
            .await?;
        Ok(())
    }
}

#[cfg(any(feature = "linux", test))]
fn select_default_route(routes: &[DefaultRouteCandidate]) -> Option<u32> {
    routes
        .iter()
        .filter(|route| route.is_ipv4 && route.is_default)
        .filter_map(|route| {
            route
                .output_interface
                .map(|interface| (route.metric.unwrap_or(0), interface))
        })
        .min_by_key(|(metric, _)| *metric)
        .map(|(_, interface)| interface)
}

#[cfg(feature = "linux")]
async fn ensure_pool_available(connection: &Connection<Route>) -> Result<(), NetworkError> {
    for route in connection.get_routes().await? {
        let Some(std::net::IpAddr::V4(destination)) = route.destination() else {
            continue;
        };
        if route.dst_len() == 0 {
            continue;
        }
        if let Some(interface) = route.oif()
            && connection
                .get_link_by_index(interface)
                .await?
                .and_then(|link| link.name().map(str::to_owned))
                .is_some_and(|name| name.starts_with("fc-tap"))
        {
            continue;
        }

        let prefix = route.dst_len();
        if overlaps_vm_pool(*destination, prefix) {
            return Err(NetworkError::PoolOverlap(format!("{destination}/{prefix}")));
        }
    }
    Ok(())
}

#[cfg(any(feature = "linux", test))]
fn overlaps_vm_pool(destination: Ipv4Addr, prefix: u8) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    };
    let route_start = u32::from(destination) & mask;
    let route_end = route_start | !mask;
    let pool_start = u32::from(Ipv4Addr::new(172, 16, 0, 0));
    let pool_end = pool_start | 0xffff;
    route_start <= pool_end && pool_start <= route_end
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

#[cfg(feature = "linux")]
#[derive(Debug, thiserror::Error)]
pub enum NetworkError {
    #[error("no IPv4 default route with an output interface exists in the main routing table")]
    MissingDefaultRoute,
    #[error("failed to parse tap name: {0}")]
    ParseValueError(ParseValueError),
    #[error("network interface {0:?} was not found")]
    InterfaceNotFound(String),
    #[error("172.16.0.0/16 overlaps existing host route {0}")]
    PoolOverlap(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Nlink(#[from] nlink::netlink::Error),
    #[error(transparent)]
    TunTap(#[from] nlink::tuntap::Error),
    #[error("network cleanup failed (nftables: {nftables:?}, TAP: {tap:?})")]
    Cleanup {
        nftables: Option<Box<NetworkError>>,
        tap: Option<Box<NetworkError>>,
    },
    #[error("network setup failed: {setup}; rollback also failed: {rollback}")]
    SetupRollback {
        setup: Box<NetworkError>,
        rollback: Box<NetworkError>,
    },
}

#[cfg(test)]
mod tests {
    use super::{DefaultRouteCandidate, overlaps_vm_pool, select_default_route};
    use std::net::Ipv4Addr;

    #[test]
    fn selects_lowest_metric_ipv4_default_route() {
        let routes = [
            DefaultRouteCandidate {
                is_ipv4: true,
                is_default: true,
                metric: Some(100),
                output_interface: Some(4),
            },
            DefaultRouteCandidate {
                is_ipv4: false,
                is_default: true,
                metric: Some(1),
                output_interface: Some(5),
            },
            DefaultRouteCandidate {
                is_ipv4: true,
                is_default: true,
                metric: None,
                output_interface: Some(6),
            },
        ];

        assert_eq!(select_default_route(&routes), Some(6));
    }

    #[test]
    fn detects_routes_that_overlap_the_vm_pool() {
        assert!(overlaps_vm_pool(Ipv4Addr::new(172, 16, 0, 0), 16));
        assert!(overlaps_vm_pool(Ipv4Addr::new(172, 16, 0, 0), 12));
        assert!(!overlaps_vm_pool(Ipv4Addr::new(10, 0, 0, 0), 8));
    }
}
