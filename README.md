# Leverage Take Home

## Limitations/Known problems

- Unrestricted firecracker vmm: It's only running trusted binary / root fs, and it's not running in production.
- Secret Management using Firecracker MDS: not implemented
- UDP port bindings: not implemented

## Dependencies

Ubuntu dependencies

```bash
sudo apt-get update
sudo apt-get install -y \
  build-essential \
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

```bash
$(pwd)/scripts/setup # or lima limactl shell provisioning ~/ $(pwd)/scripts/setup
$(pwd)/scripts/run
```

## Example commands

```sh
# Start a VM
curl -X POST http://127.0.0.1:3000/vms/0/start

# Stop a VM
curl -X POST http://127.0.0.1:3000/vms/0/shutdown

# Delete a VM
curl  -X DELETE http://127.0.0.1:3000/vms/0

# Create a new VM
curl -X POST http://127.0.0.1:3000/vms \
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

## Examples

Run these from the repository root in a linux machine.

### SQLite

```sh
VM_ID=$(
  curl --fail-with-body --silent --show-error \
    -X POST http://127.0.0.1:3000/vms \
    -H 'Content-Type: application/json' \
    -d '{
      "vcpu_count": 1,
      "memory_mib": 256,
      "authorized_keys": [],
      "port_bindings": []
    }' | jq -r .id
)

VM_IP=$(
  curl --fail-with-body --silent --show-error \
    "http://127.0.0.1:3000/vms/$VM_ID" | jq -r .network.guest_ip
)

curl -X POST "http://127.0.0.1:3000/vms/$VM_ID/start"

ssh \
  -i '~/.local/share/barbirolli/ssh/id_ed25519' \
  -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
  -o ConnectionAttempts=30 root@"$VM_IP" \
  "apt-get update &&
   apt-get install -y sqlite3 &&
   sqlite3 /root/example.db 'CREATE TABLE messages (body TEXT);' &&
   sqlite3 /root/example.db \"INSERT INTO messages VALUES ('hello');\" &&
   sqlite3 /root/example.db 'SELECT * FROM messages;'"

curl -X POST "http://127.0.0.1:3000/vms/$VM_ID/shutdown"

curl -X POST "http://127.0.0.1:3000/vms/$VM_ID/start"

ssh \
  -i '~/.local/share/barbirolli/ssh/id_ed25519' \
  -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
  -o ConnectionAttempts=30 root@"$VM_IP" \
  "sqlite3 /root/example.db 'SELECT * FROM messages;'"

curl -X POST "http://127.0.0.1:3000/vms/$VM_ID/shutdown"

curl -X DELETE "http://127.0.0.1:3000/vms/$VM_ID"
```

### HTTP

```sh
VM_ID=$(
  curl --silent --show-error \
    -X POST http://127.0.0.1:3000/vms \
    -H 'Content-Type: application/json' \
    -d '{
      "vcpu_count": 1,
      "memory_mib": 256,
      "authorized_keys": [],
      "port_bindings": [
        {"internal": 80, "external": 8080}
      ]
    }' | jq -r .id
)

VM_IP=$(
  curl --silent --show-error \
    "http://127.0.0.1:3000/vms/$VM_ID" | jq -r .network.guest_ip
)

curl -X POST \
  "http://127.0.0.1:3000/vms/$VM_ID/start"

ssh \
  -i '~/.local/share/barbirolli/ssh/id_ed25519' \
  -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
  -o ConnectionAttempts=30 root@"$VM_IP" \
  "mkdir -p /srv/hello &&
   echo '<h1>Hello from a microVM</h1>' > /srv/hello/index.html &&
   python3 -m http.server 80 --directory /srv/hello"
```

From another linux terminal:

```sh
curl --fail \
  "http://$(limactl shell provisioning ip -j route get 1.1.1.1 |
    jq -r '.[0].prefsrc'):8080"
```

Press `Ctrl-C` in the HTTP server terminal, then run:

```sh
curl -X POST "http://127.0.0.1:3000/vms/$VM_ID/shutdown"
curl -X DELETE "http://127.0.0.1:3000/vms/$VM_ID"
```

### PostgreSQL

```sh
VM_ID=$(
  curl --silent --show-error \
    -X POST http://127.0.0.1:3000/vms \
    -H 'Content-Type: application/json' \
    -d '{
      "vcpu_count": 1,
      "memory_mib": 512,
      "authorized_keys": [],
      "port_bindings": [
        {"internal": 5432, "external": 5432}
      ]
    }' | jq -r .id
)

VM_IP=$(
  curl --silent --show-error \
    "http://127.0.0.1:3000/vms/$VM_ID" | jq -r .network.guest_ip
)

curl --fail-with-body -X POST "http://127.0.0.1:3000/vms/$VM_ID/start"

ssh \
  -i '~/.local/share/barbirolli/ssh/id_ed25519' \
  -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
  -o ConnectionAttempts=30 root@"$VM_IP" \
  "export DEBIAN_FRONTEND=noninteractive &&
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

LIMA_IP=$(
  limactl shell provisioning ip -j route get 1.1.1.1 |
    jq -r '.[0].prefsrc'
)

env PGPASSWORD=postgres \
  psql --host "$LIMA_IP" --port 5432 \
    --username postgres --dbname hello \
    --tuples-only --no-align \
    --command 'SELECT body FROM messages;'

curl -X POST "http://127.0.0.1:3000/vms/$VM_ID/shutdown"

curl -X POST "http://127.0.0.1:3000/vms/$VM_ID/start"

ssh \
  -i '~/.local/share/barbirolli/ssh/id_ed25519' \
  -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
  -o ConnectionAttempts=30 root@"$VM_IP" \
  "service postgresql start"

env PGPASSWORD=postgres \
  psql --host "$LIMA_IP" --port 5432 \
    --username postgres --dbname hello \
    --tuples-only --no-align \
    --command 'SELECT body FROM messages;'

curl -X POST "http://127.0.0.1:3000/vms/$VM_ID/shutdown"
curl -X DELETE "http://127.0.0.1:3000/vms/$VM_ID"
```

## Lima

The flake sets up the limactl on MacOS targets, you can set it up by using:

```bash
limactl start --set '.nestedVirtualization=true' --name=provisioning template://default
```
