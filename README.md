# Leverage Take Home

## Limitations/Known problems

- Unrestricted firecracker vmm: It's only running trusted binary / root fs, and it's not running in production.
- Secret Management using Firecracker MDS: not implemented
- UDP port bindings: not implemented

## Lima

The projects should run inside of a Linux machine with KVM support.

> Only tested using Lima on MacOS with Nested Virtualization.

```bash
limactl start --set '.nestedVirtualization=true' --name=provisioning template://default
```

## Dependencies

Required dependencies on Ubuntu

```bash
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

From the macOS host, open a shell in the repository inside Lima:

```bash
limactl shell --workdir "$PWD" provisioning
```

Then, inside Lima:

```bash
./x setup_daemon
./x run_daemon
```

## Example commands

```sh
# Show available VM commands
./x help

# Create a VM
./x create --publish 2222:22

# Start a VM
./x start 0

# Stop a VM
./x shutdown 0

# Check status
./x status 0

# Delete a VM
./x delete 0
```

New VMs use the daemon provisioning default of 256 MiB. Memory is not selected per create request.

## CLI helpers

```sh
# Inspect the current state
./x ps
./x show 0

# Open SSH using the VM's default key
./x ssh 0

# Use a custom key if you passed --authorized-key-file on create
./x ssh --identity ~/.ssh/my-key.pem 0 -- "cat /etc/os-release"

# Help for a specific command
./x help create
```

If you use the provided Nix shell, `x` is also available in `PATH`:

```bash
nix develop --command x help
```

## Examples

Run these from the repository root inside the Linux/Lima guest. Keep
`./x run_daemon` running in another Linux/Lima terminal.

### SQLite

```sh
VM_ID=$(./x create | jq -r .id)

./x start "$VM_ID"

./x ssh "$VM_ID" -- "apt-get update &&
   apt-get install -y sqlite3 &&
   sqlite3 /root/example.db 'CREATE TABLE messages (body TEXT);' &&
   sqlite3 /root/example.db \"INSERT INTO messages VALUES ('hello');\" &&
   sqlite3 /root/example.db 'SELECT * FROM messages;'"

./x ssh "$VM_ID" -- sync
./x shutdown "$VM_ID"

./x start "$VM_ID"

./x ssh "$VM_ID" -- "sqlite3 /root/example.db 'SELECT * FROM messages;'"

./x shutdown "$VM_ID"

./x delete "$VM_ID"
```

### HTTP

```sh
VM_ID=$(./x create --publish 8080:80 | jq -r .id)

./x start "$VM_ID"

./x ssh "$VM_ID" -- "mkdir -p /srv/hello &&
   echo '<h1>Hello from a microVM</h1>' > /srv/hello/index.html &&
   python3 -m http.server 80 --directory /srv/hello"
```

From another terminal inside the same Linux/Lima guest:

```sh
HOST_IP=$(ip -j route get 1.1.1.1 | jq -r '.[0].prefsrc')
curl --fail "http://$HOST_IP:8080"
```

Press `Ctrl-C` in the HTTP server terminal, then run:

```sh
./x shutdown "$VM_ID"
./x delete "$VM_ID"
```

### PostgreSQL

```sh
VM_ID=$(./x create --publish 5432:5432 | jq -r .id)

./x start "$VM_ID"

./x ssh "$VM_ID" -- "export DEBIAN_FRONTEND=noninteractive &&
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

./x shutdown "$VM_ID"

./x start "$VM_ID"

./x ssh "$VM_ID" -- "service postgresql start"

env PGPASSWORD=postgres \
  psql --host "$HOST_IP" --port 5432 \
    --username postgres --dbname hello \
    --tuples-only --no-align \
    --command 'SELECT body FROM messages;'

./x shutdown "$VM_ID"
./x delete "$VM_ID"
```
