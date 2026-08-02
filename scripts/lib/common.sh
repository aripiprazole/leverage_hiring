#!/usr/bin/env bash

COMMON_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$COMMON_DIR/.." && pwd)"
REPO_ROOT="$(cd "$SCRIPTS_DIR/.." && pwd)"

: "${BARBIROLLI_RUNTIME_DIR:=${XDG_DATA_HOME:-$HOME/.local/share}/barbirolli}"
: "${CARGO_TARGET_DIR:=${XDG_CACHE_HOME:-$HOME/.cache}/barbirolli-target}"
: "${ELHONE_ADDR:=127.0.0.1:3000}"
: "${ELHONE_URL:=http://$ELHONE_ADDR}"
: "${RUST_LOG:=info}"

FIRECRACKER_VERSION="v1.13.2"
FIRECRACKER_CI_VERSION="v1.13"
KERNEL_VERSION="6.1.141"
ROOTFS_VERSION="24.04"

DOWNLOAD_DIR="$BARBIROLLI_RUNTIME_DIR/downloads"
IMAGE_ROOT="$BARBIROLLI_RUNTIME_DIR/images"
VM_ROOT="$BARBIROLLI_RUNTIME_DIR/vms"
SSH_DIR="$BARBIROLLI_RUNTIME_DIR/ssh"
FIRECRACKER="$BARBIROLLI_RUNTIME_DIR/bin/firecracker"
DEFAULT_AUTHORIZED_KEYS="$SSH_DIR/id_ed25519.pub"
SSH_PRIVATE_KEY="$SSH_DIR/id_ed25519"
KERNEL_IMAGE="$IMAGE_ROOT/vmlinux"
ROOTFS_IMAGE="$IMAGE_ROOT/alpine.ext4"
ROOTFS_SQUASHFS="$DOWNLOAD_DIR/ubuntu-$ROOTFS_VERSION.squashfs"
ROOTFS_KEY_MARKER="$ROOTFS_IMAGE.authorized_key"
ROOTFS_RESOLVER_MARKER="$ROOTFS_IMAGE.resolver"

export BARBIROLLI_RUNTIME_DIR
export CARGO_TARGET_DIR
export VM_ROOT
export IMAGE_ROOT
export DEFAULT_AUTHORIZED_KEYS
export FIRECRACKER
export ELHONE_URL
export ELHONE_ADDR
export RUST_LOG

info() {
    printf '==> %s\n' "$*"
}

error() {
    printf 'error: %s\n' "$*" >&2
}

die() {
    error "$*"
    exit 1
}

usage_error() {
    error "$*"
    exit 2
}

normalize_arch() {
    case "$1" in
        aarch64 | arm64)
            printf 'aarch64\n'
            ;;
        x86_64 | amd64)
            printf 'x86_64\n'
            ;;
        *)
            error "unsupported architecture: $1"
            return 1
            ;;
    esac
}

require_linux() {
    [[ "$(uname -s)" == "Linux" ]] ||
        die "these scripts must run inside a Linux Lima guest"
}

require_commands() {
    local missing=()
    local command_name
    for command_name in "$@"; do
        command -v "$command_name" >/dev/null 2>&1 || missing+=("$command_name")
    done
    if ((${#missing[@]} > 0)); then
        die "missing required command(s): ${missing[*]}"
    fi
}

firecracker_is_compatible() {
    [[ -x "$FIRECRACKER" ]] &&
        "$FIRECRACKER" --version 2>/dev/null | grep -q '^Firecracker v1\.13\.'
}

rootfs_key_matches() {
    [[ -s "$ROOTFS_KEY_MARKER" ]] &&
        cmp -s "$DEFAULT_AUTHORIZED_KEYS" "$ROOTFS_KEY_MARKER"
}

rootfs_resolver_matches() {
    [[ -s "$ROOTFS_RESOLVER_MARKER" ]] &&
        grep -qx 'nameserver 1.1.1.1' "$ROOTFS_RESOLVER_MARKER"
}

runtime_is_prepared() {
    firecracker_is_compatible &&
        [[ -s "$KERNEL_IMAGE" ]] &&
        [[ -s "$ROOTFS_IMAGE" ]] &&
        [[ -s "$SSH_PRIVATE_KEY" ]] &&
        [[ -s "$DEFAULT_AUTHORIZED_KEYS" ]] &&
        rootfs_key_matches &&
        rootfs_resolver_matches
}

require_runtime_artifacts() {
    runtime_is_prepared ||
        die "runtime artifacts are missing or invalid; run x setup_daemon first"
}

validate_integer() {
    local label="$1"
    local value="$2"
    local minimum="$3"
    local maximum="$4"
    local canonical
    local normalized

    [[ "$value" =~ ^[0-9]+$ ]] ||
        usage_error "invalid $label: expected an integer, got $value"

    canonical="${value#"${value%%[!0]*}"}"
    [[ -n "$canonical" ]] || canonical=0

    ((${#canonical} <= ${#maximum})) ||
        usage_error "invalid $label: expected a value from $minimum to $maximum, got $value"

    normalized=$((10#$canonical))
    ((normalized >= minimum && normalized <= maximum)) ||
        usage_error "invalid $label: expected a value from $minimum to $maximum, got $value"

    printf '%d\n' "$normalized"
}

require_vm_id() {
    validate_integer "VM ID" "$1" 0 16383
}

api_request() {
    local method="$1"
    local path="$2"
    shift 2

    require_commands curl jq

    if (($# > 1)); then
        usage_error "api_request accepts at most one JSON body argument"
    fi

    local response
    local status=0
    local url="${ELHONE_URL%/}$path"
    if (($# == 1)); then
        if response="$(
            curl \
                --fail-with-body \
                --silent \
                --show-error \
                --request "$method" \
                --header 'Accept: application/json' \
                --header 'Content-Type: application/json' \
                --data "$1" \
                "$url"
        )"; then
            :
        else
            status=$?
        fi
    else
        if response="$(
            curl \
                --fail-with-body \
                --silent \
                --show-error \
                --request "$method" \
                --header 'Accept: application/json' \
                "$url"
        )"; then
            :
        else
            status=$?
        fi
    fi

    if ((status != 0)); then
        if [[ -n "$response" ]]; then
            printf '%s\n' "$response" | jq .
        fi
        return "$status"
    fi

    if [[ -n "$response" ]]; then
        printf '%s\n' "$response" | jq .
    fi
}
