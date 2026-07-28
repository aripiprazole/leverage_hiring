use std::net::Ipv4Addr;

use barbirolli::{
    ArtifactName, AuthorizedKey, InterfaceName, MemoryMib, NetworkSpec, UserName, VcpuCount, VmId,
};

#[test]
fn parsed_values_reject_inputs_that_could_escape_or_exceed_limits() {
    for invalid in ["", "..", "../root", "/root", "a/b", "a\\b", "é"] {
        assert!(invalid.parse::<UserName>().is_err(), "{invalid:?}");
        assert!(invalid.parse::<ArtifactName>().is_err(), "{invalid:?}");
    }
    assert!("gabrielle_1.test-user".parse::<UserName>().is_ok());
    assert!("alpine.ext4".parse::<ArtifactName>().is_ok());

    for invalid in [0, 32] {
        assert!(VcpuCount::try_from(invalid).is_err());
    }
    for invalid in [0, 2048] {
        assert!(MemoryMib::try_from(invalid).is_err());
    }
    assert!(VmId::try_from(16_384).is_err());
    assert!("1234567890123456".parse::<InterfaceName>().is_err());
    assert!("ssh-rsa".parse::<AuthorizedKey>().is_err());
    assert!(
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBGdsGblMxDzs/KaTt+UP4TagL9vlgW2N9kHCOXVsmdQ\n\
         ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBGdsGblMxDzs/KaTt+UP4TagL9vlgW2N9kHCOXVsmdQ"
            .parse::<AuthorizedKey>()
            .is_err()
    );
}

#[test]
fn network_allocation_uses_stable_non_overlapping_slash_thirties() {
    let first = NetworkSpec::new(
        VmId::try_from(0).unwrap(),
        "/vms/alice/firecracker.socket".into(),
    )
    .unwrap();
    assert_eq!(first.subnet(), Ipv4Addr::new(172, 16, 0, 0));
    assert_eq!(first.host_ip(), Ipv4Addr::new(172, 16, 0, 1));
    assert_eq!(first.guest_ip(), Ipv4Addr::new(172, 16, 0, 2));
    assert_eq!(first.guest_mac(), "06:00:ac:10:00:02");
    assert_eq!(first.subnet_cidr(), "172.16.0.0/30");
    assert!(
        first
            .kernel_boot_args()
            .contains("ip=172.16.0.2::172.16.0.1:255.255.255.252::eth0:off nameserver=1.1.1.1")
    );

    let sixty_four = NetworkSpec::new(
        VmId::try_from(64).unwrap(),
        "/vms/bob/firecracker.socket".into(),
    )
    .unwrap();
    assert_eq!(sixty_four.subnet(), Ipv4Addr::new(172, 16, 1, 0));
    assert_eq!(sixty_four.host_ip(), Ipv4Addr::new(172, 16, 1, 1));
    assert_eq!(sixty_four.guest_ip(), Ipv4Addr::new(172, 16, 1, 2));
    assert_eq!(sixty_four.guest_mac(), "06:00:ac:10:01:02");
}
