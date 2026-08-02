use barbirolli::{AuthorizedKey, Port, PortBinding, VcpuCount, VmInput};

pub struct TestVmConfig {
    vcpu_count: u8,
    authorized_keys: Vec<AuthorizedKey>,
    port_bindings: Vec<PortBinding>,
}

impl TestVmConfig {
    pub fn new(vcpu_count: u8) -> Self {
        Self {
            vcpu_count,
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

    pub fn authorized_key(mut self, key: AuthorizedKey) -> Self {
        self.authorized_keys.push(key);
        self
    }

    pub fn into_input(self) -> VmInput {
        VmInput {
            vcpu_count: VcpuCount::try_from(self.vcpu_count).expect("valid vCPU count"),
            authorized_keys: self.authorized_keys,
            port_bindings: self.port_bindings,
        }
    }
}
