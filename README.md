# SSH VM service

```sh
limactl start --set '.nestedVirtualization=true' --name=provisioning template://default

limactl shell --workdir ~/ provisioning $(pwd)/scripts/run

limactl shellctl
```
