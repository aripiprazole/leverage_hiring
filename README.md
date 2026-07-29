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
      "user": "alice",
      "vcpu_count": 1,
      "memory_mib": 256
    }'
```

VM creation always copies `IMAGE_ROOT/vmlinux` and `IMAGE_ROOT/alpine.ext4`.
`GET /vms` includes each VM's allocated network.
