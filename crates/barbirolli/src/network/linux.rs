use super::*;

use nlink::{
    netlink::{
        Connection, Nftables, Route,
        nftables::{Chain, ChainType, CtState, Family, Hook, Policy, Priority, Rule},
    },
    tuntap::{Mode, TunTap},
};

const NETWORK_PREFIX: u8 = 30;
const MAIN_ROUTE_TABLE: u32 = 254;

pub type Result<T, E = NetworkError> = std::result::Result<T, E>;

#[derive(Debug)]
pub struct ManagedNetwork {
    spec: NetworkSpec,
    table: String,
}

impl ManagedNetwork {
    #[tracing::instrument(err)]
    pub async fn cleanup(&self) -> Result<()> {
        self.spec.cleanup(&self.table).await?;
        tracing::info!(
            vm_id = %self.spec.vm_id,
            tap = %self.spec.tap,
            table = %self.table,
            "VM network cleaned up"
        );
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct DefaultRouteCandidate {
    is_ipv4: bool,
    is_default: bool,
    metric: Option<u32>,
    output_interface: Option<u32>,
}

impl NetworkSpec {
    async fn setup(
        &self,
        route: &Connection<Route>,
        nftables_conn: &Connection<Nftables>,
        host: &InterfaceName,
        table: &str,
        bindings: &[PortBinding],
    ) -> Result<()> {
        let tap = route
            .get_link_by_name(self.tap.as_ref())
            .await?
            .ok_or_else(|| NetworkError::InterfaceNotFound(self.tap.to_string()))?;
        route
            .add_address_by_index(tap.ifindex(), self.host_ip.into(), NETWORK_PREFIX)
            .await?;
        route.set_link_up_by_index(tap.ifindex()).await?;
        tokio::fs::write("/proc/sys/net/ipv4/ip_forward", b"1\n").await?;
        self.install_rules(nftables_conn, host, table, bindings)
            .await
    }

    #[tracing::instrument(
        skip(self),
        fields(
            vm_id = %self.vm_id,
            tap = %self.tap,
            subnet = %self.subnet_cidr(),
            host_ip = %self.host_ip,
            guest_ip = %self.guest_ip
        )
    )]
    pub async fn prepare(&self, bindings: &[PortBinding]) -> Result<ManagedNetwork> {
        let table = format!("fc_vm_{}", self.vm_id);
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
        route_conn.del_link_if_exists(self.tap.as_ref()).await?;

        TunTap::builder()
            .name(self.tap.as_ref())
            .mode(Mode::Tap)
            .create_persistent()?;

        match self
            .setup(&route_conn, &nftables_conn, &host, &table, bindings)
            .await
        {
            Ok(()) => {
                tracing::info!(host_interface = %host, "VM network prepared");
                Ok(ManagedNetwork {
                    spec: self.clone(),
                    table,
                })
            }
            Err(err) => {
                match self
                    .cleanup_with_connections(&route_conn, &nftables_conn, &table)
                    .await
                {
                    Ok(()) => Err(err),
                    Err(rollback) => Err(NetworkError::SetupRollback {
                        setup: Box::new(err),
                        rollback: Box::new(rollback),
                    }),
                }
            }
        }
    }

    #[tracing::instrument(
        skip(self, route, nftables),
        fields(vm_id = %self.vm_id, tap = %self.tap, %table)
    )]
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
            .del_link_if_exists(self.tap.as_ref())
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

    #[tracing::instrument(err)]
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
                .del_link_if_exists(self.tap.as_ref())
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

    #[tracing::instrument(err)]
    pub async fn cleanup_stale(&self) -> Result<(), NetworkError> {
        tracing::info!(
            vm_id = %self.vm_id,
            tap = %self.tap,
            "cleaning up stale VM network"
        );
        self.cleanup(&format!("fc_vm_{}", self.vm_id)).await
    }

    async fn install_rules(
        &self,
        conn: &Connection<Nftables>,
        host: &InterfaceName,
        table: &str,
        bindings: &[PortBinding],
    ) -> Result<(), NetworkError> {
        let prerouting_chain = Chain::new(table, "prerouting")?
            .family(Family::Ip)
            .hook(Hook::Prerouting)
            .priority(Priority::DstNat)
            .chain_type(ChainType::Nat)
            .policy(Policy::Accept);
        let output_chain = Chain::new(table, "output")?
            .family(Family::Ip)
            .hook(Hook::Output)
            .priority(Priority::DstNat)
            .chain_type(ChainType::Nat)
            .policy(Policy::Accept);
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
        let masquerade = Rule::new(table, "postrouting")
            .family(Family::Ip)
            .match_saddr_v4(self.subnet, NETWORK_PREFIX)
            .match_oif(host.as_ref())
            .masquerade();

        let plan = self.firewall_plan(bindings);
        let mut transaction = conn
            .transaction()
            .add_table(table, Family::Ip)
            .add_chain(prerouting_chain)
            .add_chain(output_chain)
            .add_chain(forward_chain)
            .add_chain(postrouting_chain)
            .add_rule(masquerade);

        for forward in &plan.port_forwards {
            transaction = transaction.add_rule(
                Rule::new(table, "prerouting")
                    .family(Family::Ip)
                    .match_iif(host.as_ref())
                    .match_daddr_v4_not(
                        forward.dest_exclusion.network,
                        forward.dest_exclusion.prefix,
                    )
                    .match_tcp_dport(forward.external.into())
                    .dnat(forward.guest_ip, Some(forward.internal.into()))
                    .comment("published-tcp-dnat"),
            );
            transaction = transaction.add_rule(
                Rule::new(table, "output")
                    .family(Family::Ip)
                    .match_daddr_v4_not(
                        forward.dest_exclusion.network,
                        forward.dest_exclusion.prefix,
                    )
                    .match_tcp_dport(forward.external.into())
                    .dnat(forward.guest_ip, Some(forward.internal.into()))
                    .comment("published-tcp-output-dnat"),
            );
        }

        for rule in plan.forward_rules {
            let rule = match rule {
                ForwardRule::AllowVmEgress => Rule::new(table, "forward")
                    .family(Family::Ip)
                    .match_iif(self.tap.as_ref())
                    .match_oif(host.as_ref())
                    .accept()
                    .comment("vm-egress"),
                ForwardRule::AllowEstablishedToVm => Rule::new(table, "forward")
                    .family(Family::Ip)
                    .match_iif(host.as_ref())
                    .match_oif(self.tap.as_ref())
                    .match_ct_state(CtState::ESTABLISHED | CtState::RELATED)
                    .accept()
                    .comment("vm-established"),
                ForwardRule::AllowEstablishedFromPeer => Rule::new(table, "forward")
                    .family(Family::Ip)
                    .match_saddr_v4(VM_NETWORK_POOL, VM_NETWORK_POOL_PREFIX)
                    .match_oif(self.tap.as_ref())
                    .match_ct_state(CtState::ESTABLISHED | CtState::RELATED)
                    .accept()
                    .comment("vm-peer-established"),
                ForwardRule::AllowPublishedTcp(forward) => Rule::new(table, "forward")
                    .family(Family::Ip)
                    .match_oif(self.tap.as_ref())
                    .match_daddr_v4(forward.guest_ip, 32)
                    .match_tcp_dport(forward.internal.into())
                    .accept()
                    .comment("published-tcp-forward"),
                ForwardRule::DropUnmatchedToVm => Rule::new(table, "forward")
                    .family(Family::Ip)
                    .match_oif(self.tap.as_ref())
                    .drop()
                    .comment("drop-unpublished-inbound"),
            };
            transaction = transaction.add_rule(rule);
        }

        transaction.commit(conn).await?;
        Ok(())
    }
}

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

#[derive(Debug, thiserror::Error)]
pub enum NetworkError {
    #[error("no IPv4 default route with an output interface exists in the main routing table")]
    MissingDefaultRoute,
    #[error("failed to parse tap name: {0}")]
    ParseValueError(#[from] ParseValueError),
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
    use super::{
        Connection, DefaultRouteCandidate, Family, NetworkError, NetworkSpec, Nftables,
        overlaps_vm_pool, select_default_route,
    };
    use crate::{Port, PortBinding, ProvisioningConfig, support};
    use barbirolli::VmStatus;
    use barbirolli_derive::firecracker_test;
    use nlink::netlink::nftables::{CmpOp, RuleExpr};
    use std::net::Ipv4Addr;

    use support::{
        config::TestVmConfig,
        firecracker::FirecrackerFixture,
        network::{LimaHttpFixture, available_tcp_port},
    };

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

    #[firecracker_test]
    async fn vms_are_isolated_and_can_reach_an_http_server_on_lima() {
        let fixture = FirecrackerFixture::new(ProvisioningConfig::default()).await;
        let vms = fixture
            .create_vms_concurrently::<2>([TestVmConfig::new(1), TestVmConfig::new(1)], |_| {})
            .await;
        let mut running = vms;
        running.sort_by_key(|vm| u16::from(vm.id));
        running[0]
            .lifecycle
            .start(|lifecycle| {
                assert_eq!(lifecycle.status(), VmStatus::Running);
            })
            .await;
        running[1]
            .lifecycle
            .start(|lifecycle| {
                assert_eq!(lifecycle.status(), VmStatus::Running);
            })
            .await;
        let alice = &running[0];
        let bob = &running[1];

        let lima = LimaHttpFixture::start("outside-lima").await;
        alice
            .ssh
            .http_get(&lima.url_for(&alice.network), |output| {
                assert_eq!(output.stdout, "outside-lima");
            })
            .await;

        let isolated_port = 18_080;
        bob.ssh
            .start_http_server(isolated_port, "bob-unpublished")
            .await;
        bob.network
            .http_get(isolated_port, |response| {
                assert_eq!(response.status, 200);
                assert_eq!(response.body, b"bob-unpublished");
            })
            .await;
        let blocked = alice
            .ssh
            .try_http_get(&bob.network.url(isolated_port))
            .await;
        assert!(!blocked.status.success());

        fixture.finish().await;
    }

    #[firecracker_test]
    async fn a_published_binding_permits_one_vm_to_reach_another() {
        let fixture = FirecrackerFixture::new(ProvisioningConfig::default()).await;
        let allowed_port = 18_081;
        let blocked_port = 18_082;
        let external_port = available_tcp_port();
        let vms = fixture
            .create_vms_concurrently::<2>(
                [
                    TestVmConfig::new(1),
                    TestVmConfig::new(1).binding(allowed_port, external_port),
                ],
                |_| {},
            )
            .await;
        vms[0].lifecycle.start(|_| {}).await;
        vms[1].lifecycle.start(|_| {}).await;
        let alice = &vms[0];
        let bob = &vms[1];

        bob.ssh
            .start_http_server(allowed_port, "bob-published")
            .await;
        bob.ssh
            .start_http_server(blocked_port, "bob-unpublished")
            .await;
        bob.network.http_get(allowed_port, |_| {}).await;
        bob.network.http_get(blocked_port, |_| {}).await;

        alice
            .ssh
            .http_get(&bob.network.url(allowed_port), |output| {
                assert_eq!(output.stdout, "bob-published");
            })
            .await;
        let blocked = alice.ssh.try_http_get(&bob.network.url(blocked_port)).await;
        assert!(!blocked.status.success());

        fixture.finish().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires Linux root privileges and nftables"]
    #[allow(clippy::await_holding_lock)]
    async fn installs_published_port_and_default_deny_rules() {
        let _guard = support::lock_firecracker_tests();
        let network = NetworkSpec::new(support::next_firecracker_vm_id()).expect("valid network");
        let binding = PortBinding {
            internal: Port::try_from(22).expect("valid internal port"),
            external: Port::try_from(25_222).expect("valid external port"),
        };
        let table = format!("fc_vm_{}", network.vm_id);
        let managed = network
            .prepare(&[binding])
            .await
            .expect("failed to prepare VM network");

        let inspection = async {
            let connection = Connection::<Nftables>::new()?;
            let chains = connection
                .list_chains_in(table.as_str(), Family::Ip)
                .await?;
            let rules = connection.list_rules(&table, Family::Ip).await?;
            Ok::<_, NetworkError>((chains, rules))
        }
        .await;
        let cleanup = managed.cleanup().await;

        let (chains, rules) = inspection.expect("failed to inspect installed nftables rules");
        cleanup.expect("failed to clean up VM network");

        let mut chain_names = chains
            .iter()
            .map(|chain| chain.name.as_str())
            .collect::<Vec<_>>();
        chain_names.sort_unstable();
        assert_eq!(
            chain_names,
            ["forward", "output", "postrouting", "prerouting"]
        );

        let mut comments = rules
            .iter()
            .filter_map(|rule| rule.comment.as_deref())
            .collect::<Vec<_>>();
        comments.sort_unstable();
        assert_eq!(
            comments,
            [
                "drop-unpublished-inbound",
                "published-tcp-dnat",
                "published-tcp-forward",
                "published-tcp-output-dnat",
                "vm-egress",
                "vm-established",
                "vm-peer-established",
            ]
        );

        let dnat = rules
            .iter()
            .find(|rule| rule.comment.as_deref() == Some("published-tcp-dnat"))
            .expect("missing published TCP DNAT rule");
        assert!(
            dnat.expressions().iter().any(|expression| matches!(
                expression,
                RuleExpr::Cmp {
                    op: CmpOp::Neq,
                    data,
                    ..
                } if data == &[172, 16, 0, 0]
            )),
            "DNAT must exclude destinations already translated into the VM pool"
        );
    }
}
