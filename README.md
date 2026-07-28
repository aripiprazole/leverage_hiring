# SSH VM service

This project runs Firecracker VMs behind an HTTP lifecycle API (Elhone) and an
SSH forwarding service (Meier). The application runs on Linux and needs KVM
plus permission to create TAP interfaces and nftables rules.

The development scripts are intended to be run **inside a Linux Lima guest**.
They do not create or enter a Lima instance.

## Prerequisites

The Lima guest needs:

- `aarch64` or `x86_64` Linux with `/dev/kvm`
- Rust and Cargo from `rust-toolchain.toml`
- Passwordless or interactive `sudo`
- `curl`, `jq`, `tar`, `sha256sum`, and GNU `timeout`
- `ssh-keygen` and the OpenSSH client
- `unsquashfs`, `mkfs.ext4`, and `truncate`
- Enough free space for a 1 GiB rootfs and several GiB of Cargo build output

On Ubuntu, the non-Rust tools are available through:

```bash
sudo apt-get update
sudo apt-get install -y \
  coreutils \
  curl \
  e2fsprogs \
  jq \
  openssh-client \
  squashfs-tools
```

Verify the guest before starting:

```bash
uname -sm
test -e /dev/kvm && echo "KVM is available"
sudo -v
```

## Quick start

From the repository root inside Lima, prepare Firecracker and the VM images:

```bash
scripts/dev setup
```

`setup` is idempotent. It downloads Firecracker 1.13.2, a matching v1.13 CI
kernel, an Ubuntu 24.04 rootfs, and creates a dedicated SSH key. The rootfs is
converted into a writable ext4 image with the key installed.

Run the automated checks:

```bash
scripts/dev test
```

Start the service:

```bash
scripts/dev run
```

`run` builds with the `linux` and `local` features, then uses `sudo` to launch
the service. By default:

- Elhone listens at `127.0.0.1:3000`.
- Meier listens at `0.0.0.0:2222`.
- Logs use the `info` filter.

Leave that command running. In another terminal inside the same Lima guest,
run the end-to-end test:

```bash
scripts/dev smoke
```

The smoke test creates and starts a VM, waits for its SSH server, runs
`uname -a` through Meier's SSH forwarding path, then shuts down and deletes the
VM.

## Commands

Each command works directly or through `scripts/dev`:

```bash
scripts/setup
scripts/run
scripts/test
scripts/smoke

scripts/dev setup
scripts/dev run
scripts/dev test
scripts/dev smoke
```

Show dispatcher help with:

```bash
scripts/dev --help
```

### Setup

```bash
scripts/setup
```

Existing valid artifacts are reused. Downloads are staged before being moved
into place, and interrupted rootfs preparation cleans up its temporary files.
If only one half of the SSH key pair exists, setup stops instead of overwriting
it.

The setup downloads come from the official
[Firecracker v1.13.2 release](https://github.com/firecracker-microvm/firecracker/releases/tag/v1.13.2)
and [Firecracker CI artifact bucket](https://github.com/firecracker-microvm/firecracker/blob/main/docs/getting-started.md).

### Run

```bash
scripts/run
```

The development build enables the `local` feature. That disables HTTP bearer
authentication and relaxes Meier's outer authorized-key check for configured
VM users. Keep the HTTP listener on loopback and do not use this mode for
production.

Stop the service with `Ctrl+C`; it attempts to shut down all managed VMs before
exiting.

### Test

```bash
scripts/test
```

This runs:

1. Shell regression tests.
2. `cargo fmt --all -- --check`.
3. `cargo clippy --workspace --all-targets --features linux -- -D warnings`.
4. `cargo test --workspace --features linux`.

Extra arguments are forwarded to the final `cargo test` invocation:

```bash
scripts/test -- --nocapture
```

Every Cargo phase is bounded by `BARBIROLLI_COMMAND_TIMEOUT`, so a deadlock is
reported as a failed timeout instead of hanging indefinitely.

### Smoke test

Start `scripts/run` in another terminal first, then run:

```bash
scripts/smoke
```

The smoke test uses a unique VM user by default. Set
`BARBIROLLI_SMOKE_USER` when a fixed name is useful:

```bash
BARBIROLLI_SMOKE_USER=alice scripts/smoke
```

## Privileged Firecracker test

The ignored Firecracker test changes host networking and must run as root.
Compile it as the development user, then execute only the test binary with
`sudo`:

```bash
source scripts/lib/common.sh

FIRECRACKER_TEST_BIN="$(
  cargo test \
    -p barbirolli \
    --features linux \
    --test firecracker \
    --no-run \
    --message-format=json |
  jq -r '
    select(
      .reason == "compiler-artifact" and
      .target.name == "firecracker" and
      .executable != null
    ) |
    .executable
  ' |
  tail -n 1
)"

test -n "$FIRECRACKER_TEST_BIN"

sudo --preserve-env=FIRECRACKER,IMAGE_ROOT \
  timeout --foreground "$BARBIROLLI_COMMAND_TIMEOUT" \
  "$FIRECRACKER_TEST_BIN" \
  --ignored \
  --nocapture
```

## Runtime layout

The default runtime directory is:

```text
${XDG_DATA_HOME:-$HOME/.local/share}/barbirolli
```

It contains:

```text
bin/firecracker
downloads/
images/vmlinux
images/ubuntu-24.04.ext4
images/ubuntu-24.04.ext4.authorized_key
images/alpine.ext4
ssh/id_ed25519
ssh/id_ed25519.pub
vms/
```

`images/alpine.ext4` is an alias retained for the artifact name currently used
by the privileged test. The `.authorized_key` marker lets `scripts/setup`
rebuild the rootfs when the development SSH key changes.

Cargo output defaults to:

```text
${XDG_CACHE_HOME:-$HOME/.cache}/barbirolli-target
```

The guest's `/tmp` is deliberately not used for Cargo output because Lima often
mounts it as a small tmpfs.

## Environment overrides

The scripts preserve caller-provided values:

| Variable | Default | Purpose |
| --- | --- | --- |
| `BARBIROLLI_RUNTIME_DIR` | `${XDG_DATA_HOME:-$HOME/.local/share}/barbirolli` | Firecracker, images, keys, and VM state |
| `CARGO_TARGET_DIR` | `${XDG_CACHE_HOME:-$HOME/.cache}/barbirolli-target` | Linux Cargo build output |
| `BARBIROLLI_COMMAND_TIMEOUT` | `120` | Maximum seconds for Cargo, HTTP, and SSH operations |
| `ELHONE_ADDR` | `127.0.0.1:3000` | HTTP listener |
| `MEIER_ADDR` | `0.0.0.0:2222` | SSH forwarding listener |
| `RUST_LOG` | `info` | Tracing filter |
| `BARBIROLLI_API_URL` | `http://$ELHONE_ADDR` | Smoke-test API endpoint |
| `BARBIROLLI_SSH_HOST` | `127.0.0.1` | Smoke-test address for Meier |
| `BARBIROLLI_SMOKE_USER` | Unique `smoke-*` name | VM user created by the smoke test |

For example:

```bash
BARBIROLLI_COMMAND_TIMEOUT=300 \
RUST_LOG=debug \
scripts/dev run
```

## Known failure

The current `Barbirolli::delete` implementation keeps a `DashMap` mutable entry
guard while removing that same entry. The complete workspace test and the
delete phase of `scripts/smoke` can therefore time out. The scripts expose this
failure with a bounded, nonzero result; they do not silently skip deletion.
