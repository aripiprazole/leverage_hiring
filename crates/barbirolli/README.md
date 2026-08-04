# barbirolli

Barbirolli provides the lifecycle, storage, networking, and Firecracker
integration used by the local microVM daemon.

The crate is intended for Linux hosts with Firecracker and KVM available. The
`meier` CLI supplies the cross-platform client and Lima transport.
