#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

usage() {
    cat <<'EOF'
Usage: x <command> [args]

Commands:
  daemon:setup     Download and prepare daemon/runtime artifacts.
  daemon:run       Build and run the HTTP daemon with required privileges.
  oci:pull         Pull and retain an OCI image.
  oci:run          Run one VM for an OCI image.
  oci:stop         Stop an OCI image's VM.
  oci:rm           Remove a stopped OCI image and its artifacts.
  vm:create        Create a new VM.
  vm:ps            List VMs.
  vm:show          Show details for a VM.
  vm:status        Show a VM status.
  vm:start         Start a VM.
  vm:shutdown      Shutdown a VM.
  vm:delete        Delete a VM.
  vm:ssh           SSH into a running VM.
  vm:logs          Pull or attach to a VM's serial log.
  help             Show command usage.

Run:
  x help <command>
for command-specific help.
EOF
}

is_supported_command() {
    local command="$1"
    shift
    local allowed
    for allowed in "$@"; do
        [[ "$allowed" == "$command" ]] && return 0
    done
    return 1
}

commands=(
    daemon:setup
    daemon:run
    oci:pull
    oci:run
    oci:stop
    oci:rm
    vm:create
    vm:ps
    vm:show
    vm:status
    vm:start
    vm:shutdown
    vm:delete
    vm:ssh
    vm:logs
)

if (($# == 0)); then
    usage
    exit 0
fi

case "$1" in
    -h | --help)
        usage
        exit 0
        ;;
    help)
        shift
        if (($# == 0)); then
            usage
            exit 0
        fi
        command="$1"
        if is_supported_command "$command" "${commands[@]}"; then
            "$SCRIPT_DIR/scripts/$command" --help
            exit 0
        fi
        printf 'error: unknown command: %s\n' "$command" >&2
        printf 'Run `x help` to list supported commands.\n' >&2
        exit 2
        ;;
    *)
        command="$1"
        shift
        if ! is_supported_command "$command" "${commands[@]}"; then
            printf 'error: unknown command: %s\n' "$command" >&2
            printf 'Run `x help` for available commands.\n' >&2
            exit 2
        fi
        "$SCRIPT_DIR/scripts/$command" "$@"
        ;;
esac
