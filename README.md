# SSH VM service

```sh
limactl start --set '.nestedVirtualization=true' --name=provisioning template://default

limactl shell --workdir ~/ provisioning $(pwd)/scripts/run

# Start a VM
limactl shell provisioning curl --fail-with-body -X POST http://127.0.0.1:3000/vms/0/start

# Stop a VM
limactl shell provisioning curl --fail-with-body -X POST http://127.0.0.1:3000/vms/0/shutdown

# Delete a VM
limactl shell provisioning curl --fail-with-body -X DELETE http://127.0.0.1:3000/vms/0

# Create a new VM
limactl shell provisioning \
  curl --fail-with-body \
    -X POST http://127.0.0.1:3000/vms \
    -H 'Content-Type: application/json' \
    -d '{
      "vcpu_count": 1,
      "memory_mib": 256,
      "authorized_keys": [],
      "port_bindings": [
        {
          "internal": 22,
          "external": 2222
        }
      ]
    }'
```

VM creation always copies `IMAGE_ROOT/vmlinux` and `IMAGE_ROOT/alpine.ext4`.
`GET /vms` includes each VM's allocated network and port bindings.

A port binding is `internal:external`: the example forwards TCP traffic from
port 2222 on the host's default external interface to port 22 inside the VM.
Both ports must be in `1..=65535`. External-port uniqueness is an invariant of
the provisioning design and is not checked again by this service.

Forwarded inbound traffic is denied unless it matches a port binding. Reply
traffic for VM-initiated connections remains allowed, VM internet egress
remains unrestricted, and direct VM-to-VM traffic is denied.

When `authorized_keys` is nonempty, those keys are written directly to
`/root/.ssh/authorized_keys` inside the VM's private `rootfs.ext4`. No
`authorized_keys` sidecar is stored in the VM directory. An empty list keeps
the default key that `scripts/setup` installed in the source ext4 image.
