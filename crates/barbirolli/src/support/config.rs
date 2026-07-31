use barbirolli::{AuthorizedKey, MemoryMib, Port, PortBinding, VcpuCount, VmInput};

pub struct TestVmConfig {
    vcpu_count: u8,
    memory_mib: u16,
    authorized_keys: Vec<AuthorizedKey>,
    port_bindings: Vec<PortBinding>,
}

impl TestVmConfig {
    pub fn new(vcpu_count: u8, memory_mib: u16) -> Self {
        Self {
            vcpu_count,
            memory_mib,
            authorized_keys: Vec::new(),
            port_bindings: Vec::new(),
        }
    }

    pub fn binding(mut self, internal: u16, external: u16) -> Self {
        self.port_bindings.push(PortBinding {
            internal: Port::try_from(internal).expect("valid internal port"),
            external: Port::try_from(external).expect("valid external port"),
        });
        self
    }

    pub fn into_input(self) -> VmInput {
        VmInput {
            vcpu_count: VcpuCount::try_from(self.vcpu_count).expect("valid vCPU count"),
            memory_mib: MemoryMib::try_from(self.memory_mib).expect("valid memory"),
            authorized_keys: self.authorized_keys,
            port_bindings: self.port_bindings,
        }
    }
}
