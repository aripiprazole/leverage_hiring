# Leverage Take Home

## Base testing

```sh
limactl start --set '.nestedVirtualization=true' --name=provisioning template://default

limactl shell provisioning -- bash -lc '                                  nix-shell-env
  set -euo pipefail

  sudo apt-get update
  sudo apt-get install -y \
    build-essential \
    coreutils \
    curl \
    e2fsprogs \
    jq \
    openssh-client \
    squashfs-tools

  curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs |
    sh -s -- -y --profile default --default-toolchain stable

  source "$HOME/.cargo/env"
  cargo --version
'

limactl shell --workdir ~/ provisioning $(pwd)/scripts/setup
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

## Examples

Run these from the repository root in a macOS zsh terminal.

### SQLite

```sh
VM_ID=$(
  limactl shell provisioning \
    curl --fail-with-body --silent --show-error \
      -X POST http://127.0.0.1:3000/vms \
      -H 'Content-Type: application/json' \
      -d '{
        "vcpu_count": 1,
        "memory_mib": 256,
        "authorized_keys": [],
        "port_bindings": [{"internal": 22, "external": 2201}]
      }' |
    jq -r .id
)

limactl shell provisioning curl --fail-with-body -X POST \
  "http://127.0.0.1:3000/vms/$VM_ID/start"

limactl shell provisioning ssh \
  -i '~/.local/share/barbirolli/ssh/id_ed25519' \
  -p 2201 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
  root@127.0.0.1

apt-get update && apt-get install -y sqlite3
sqlite3 /root/example.db
CREATE TABLE messages (body TEXT);
INSERT INTO messages VALUES ('hello');
SELECT * FROM messages;
.quit
exit

limactl shell provisioning curl --fail-with-body -X POST \
  "http://127.0.0.1:3000/vms/$VM_ID/shutdown"
```

### HTTP

```sh
VM_ID=$(
  limactl shell provisioning \
    curl --fail-with-body --silent --show-error \
      -X POST http://127.0.0.1:3000/vms \
      -H 'Content-Type: application/json' \
      -d '{
        "vcpu_count": 1,
        "memory_mib": 256,
        "authorized_keys": [],
        "port_bindings": [
          {"internal": 22, "external": 2202},
          {"internal": 80, "external": 8080}
        ]
      }' |
    jq -r .id
)

limactl shell provisioning curl --fail-with-body -X POST \
  "http://127.0.0.1:3000/vms/$VM_ID/start"

limactl shell provisioning ssh \
  -i '~/.local/share/barbirolli/ssh/id_ed25519' \
  -p 2202 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
  root@127.0.0.1

mkdir -p /srv/hello
echo '<h1>Hello from a microVM</h1>' > /srv/hello/index.html
python3 -m http.server 80 --directory /srv/hello
```

Open <http://127.0.0.1:8080>. Press `Ctrl-C`, then run:

```sh
exit
limactl shell provisioning curl --fail-with-body -X POST \
  "http://127.0.0.1:3000/vms/$VM_ID/shutdown"
```

### PostgreSQL

```sh
VM_ID=$(
  limactl shell provisioning \
    curl --fail-with-body --silent --show-error \
      -X POST http://127.0.0.1:3000/vms \
      -H 'Content-Type: application/json' \
      -d '{
        "vcpu_count": 1,
        "memory_mib": 512,
        "authorized_keys": [],
        "port_bindings": [
          {"internal": 22, "external": 2203},
          {"internal": 5432, "external": 5432}
        ]
      }' |
    jq -r .id
)

limactl shell provisioning curl --fail-with-body -X POST \
  "http://127.0.0.1:3000/vms/$VM_ID/start"

limactl shell provisioning ssh \
  -i '~/.local/share/barbirolli/ssh/id_ed25519' \
  -p 2203 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
  root@127.0.0.1

apt-get update && apt-get install -y postgresql
service postgresql start
su postgres -c "createdb hello"
su postgres -c "psql -l"
exit

limactl shell provisioning curl --fail-with-body -X POST \
  "http://127.0.0.1:3000/vms/$VM_ID/shutdown"
```
