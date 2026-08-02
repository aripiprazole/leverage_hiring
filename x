#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

usage() {
    cat <<'EOF'
Usage: x <command> [args]

Commands:
  setup_daemon     Download and prepare daemon/runtime artifacts.
  run_daemon       Build and run the HTTP daemon with required privileges.
  oci:pull         Pull and retain an OCI image.
  oci:run          Run one VM for an OCI image.
  oci:stop         Stop an OCI image's VM.
  oci:rm           Remove a stopped OCI image and its artifacts.
  create           Create a new VM.
  ps               List VMs.
  show             Show details for a VM.
  status           Show a VM status.
  start            Start a VM.
  shutdown         Shutdown a VM.
  delete           Delete a VM.
  ssh              SSH into a running VM.
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
    setup_daemon
    run_daemon
    oci:pull
    oci:run
    oci:stop
    oci:rm
    create
    ps
    show
    status
    start
    shutdown
    delete
    ssh
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
