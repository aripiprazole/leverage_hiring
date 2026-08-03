# Leverage Take Home

## Limitations/Known problems

- Unrestricted Firecracker VMM: The VMM runs only trusted binaries and root filesystems. The
  project does not use it in production.
- Secret management with Firecracker MDS: The service does not implement this function.
- UDP port bindings: The service does not implement this function.

## Lima

Run the projects in a Linux VM that has KVM support.

> Tested the projects only with Lima on macOS and nested virtualization.

```bash
limactl start --set '.nestedVirtualization=true' --name=provisioning template://default
```

## Dependencies

```bash
# ubuntu deps
sudo apt-get update
sudo apt-get install -y \
  build-essential \
  libclang-dev \
  coreutils \
  curl \
  e2fsprogs \
  jq \
  openssh-client \
  postgresql-client \
  squashfs-tools

curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs |
  sh -s -- -y --profile default --default-toolchain stable

source "$HOME/.cargo/env"
cargo --version
```

## Booting

On the macOS host, open a shell in the repository inside Lima:

```bash
limactl shell --workdir "$PWD" provisioning
```

In the Lima VM, run these commands:

```bash
# Install the dependencies
./x daemon:setup
./x daemon:run
```

## Example commands

```sh
./x help # Show available VM commands
./x vm:create --publish 2222:22 # Create a VM
./x vm:start 0 # Start a VM
./x vm:pause 0 # Pause a running VM
./x vm:resume 0 # Resume a paused VM
./x vm:shutdown 0 # Stop a VM
./x vm:status 0 # Check the VM status
./x vm:logs 0 # Print retained serial output and follow new output
./x vm:delete 0 # Delete a VM
```

The daemon gives each new VM the default memory size of 256 MiB. The `vm:create` command has no
memory option.

## CLI helpers

```sh
./x vm:ps
./x vm:show 0 # Show the current VM state
./x vm:ssh 0 # Open SSH with the VM's default key
./x vm:logs 0 --pull # Print the stored serial output and exit
./x vm:logs 0 --attach # Use the full form of the default follow mode
./x vm:ssh --identity ~/.ssh/my-key.pem 0 -- "cat /etc/os-release" # Use the private key that matches --authorized-key-file
./x help vm:create # Help for a specific command
./x help vm:pause # Help for pausing a VM
./x help vm:resume # Help for resuming a VM
```

When you use the Nix shell, `x` is also in `PATH`:

```bash
nix develop --command x help
```

## Examples

Run these commands from the repository root in the Linux/Lima VM. Run `./x daemon:run` in another
Linux/Lima terminal.

### OCI image lifecycle

The OCI store uses the image name and tag as its key. This example pulls Alpine and starts its
single VM. It then stops the VM and keeps the image. Finally, it removes the stopped lifecycle and
its artifacts:

```sh
./x oci:pull alpine:latest
./x oci:run alpine:latest
./x oci:stop alpine:latest
./x oci:rm alpine:latest
```

You can use `oci:run` when the image is not in the store. The command pulls the image automatically.
Stop the VM before you use `oci:rm`.

### SQLite

```sh
VM_ID=$(./x vm:create | jq -r .id)

./x vm:start "$VM_ID"

./x vm:ssh "$VM_ID" -- "apt-get update &&
   apt-get install -y sqlite3 &&
   sqlite3 /root/example.db 'CREATE TABLE messages (body TEXT);' &&
   sqlite3 /root/example.db \"INSERT INTO messages VALUES ('hello');\" &&
   sqlite3 /root/example.db 'SELECT * FROM messages;'"

./x vm:ssh "$VM_ID" -- sync
./x vm:shutdown "$VM_ID"

./x vm:start "$VM_ID"

./x vm:ssh "$VM_ID" -- "sqlite3 /root/example.db 'SELECT * FROM messages;'"

./x vm:shutdown "$VM_ID"

./x vm:delete "$VM_ID"
```

### HTTP

```sh
VM_ID=$(./x vm:create --publish 8080:80 | jq -r .id)

./x vm:start "$VM_ID"

./x vm:ssh "$VM_ID" -- "mkdir -p /srv/hello &&
   echo '<h1>Hello from a microVM</h1>' > /srv/hello/index.html &&
   python3 -m http.server 80 --directory /srv/hello"
```

Run this command in another terminal in the same Linux/Lima VM:

```sh
HOST_IP=$(ip -j route get 1.1.1.1 | jq -r '.[0].prefsrc')
curl --fail "http://$HOST_IP:8080"
```

Press `Ctrl-C` in the HTTP server terminal. Then run:

```sh
./x vm:shutdown "$VM_ID"
./x vm:delete "$VM_ID"
```

### PostgreSQL

```sh
VM_ID=$(./x vm:create --publish 5432:5432 | jq -r .id)

./x vm:start "$VM_ID"

./x vm:ssh "$VM_ID" -- "export DEBIAN_FRONTEND=noninteractive &&
   apt-get update &&
   apt-get install -y postgresql &&
   sed -i \"s/^#listen_addresses = 'localhost'/listen_addresses = '*'/\" \
     /etc/postgresql/16/main/postgresql.conf &&
   printf '%s\n' 'host all all 0.0.0.0/0 scram-sha-256' \
     >> /etc/postgresql/16/main/pg_hba.conf &&
   service postgresql restart &&
   su postgres -c \"psql -c \\\"ALTER USER postgres PASSWORD 'postgres';\\\"\" &&
   su postgres -c 'createdb hello' &&
   su postgres -c \"psql -d hello -c \\\"CREATE TABLE messages (body TEXT);
     INSERT INTO messages VALUES ('hello from postgres');\\\"\""

HOST_IP=$(ip -j route get 1.1.1.1 | jq -r '.[0].prefsrc')

env PGPASSWORD=postgres \
  psql --host "$HOST_IP" --port 5432 \
    --username postgres --dbname hello \
    --tuples-only --no-align \
    --command 'SELECT body FROM messages;'

./x vm:shutdown "$VM_ID"

./x vm:start "$VM_ID"

./x vm:ssh "$VM_ID" -- "service postgresql start"

env PGPASSWORD=postgres \
  psql --host "$HOST_IP" --port 5432 \
    --username postgres --dbname hello \
    --tuples-only --no-align \
    --command 'SELECT body FROM messages;'

./x vm:shutdown "$VM_ID"
./x vm:delete "$VM_ID"
```
