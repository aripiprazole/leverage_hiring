#!/usr/bin/env bash

set -u

TESTS_RUN=0
TESTS_FAILED=0
TEST_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCRIPTS_DIR="$TEST_ROOT/scripts"

fail() {
    printf '    %s\n' "$*" >&2
    return 1
}

run_test() {
    local name="$1"
    shift
    TESTS_RUN=$((TESTS_RUN + 1))
    if "$@"; then
        printf 'ok %d - %s\n' "$TESTS_RUN" "$name"
    else
        printf 'not ok %d - %s\n' "$TESTS_RUN" "$name"
        TESTS_FAILED=$((TESTS_FAILED + 1))
    fi
}

write_executable() {
    local path="$1"
    shift
    printf '%s\n' "$@" >"$path"
    chmod +x "$path"
}

test_dev_prints_help() {
    local output
    output="$("$SCRIPTS_DIR/dev" --help 2>&1)" || return 1
    [[ "$output" == *'Usage: scripts/dev <setup|run|test|smoke>'* ]] || {
        fail "unexpected help output: $output"
        return 1
    }
}

test_dev_rejects_unknown_commands() {
    local output
    if output="$("$SCRIPTS_DIR/dev" deploy 2>&1)"; then
        return 1
    fi
    [[ "$output" == *'unknown command: deploy'* ]] || {
        fail "unexpected error output: $output"
        return 1
    }
}

test_dev_dispatches_and_forwards_arguments() {
    local temporary output
    temporary="$(mktemp -d)"
    cp "$SCRIPTS_DIR/dev" "$temporary/dev" || return 1
    write_executable "$temporary/setup" \
        '#!/usr/bin/env bash' \
        'printf "setup:%s\n" "$*"'

    output="$("$temporary/dev" setup alpha beta 2>&1)"
    local status=$?
    rm -rf -- "$temporary"

    [[ $status -eq 0 ]] || return 1
    [[ "$output" == 'setup:alpha beta' ]] || {
        fail "dispatcher did not forward arguments: $output"
        return 1
    }
}

test_common_paths_default_and_allow_overrides() {
    local common default_paths overridden_paths
    common="$SCRIPTS_DIR/lib/common.sh"

    default_paths="$(
        env \
            -u XDG_DATA_HOME \
            -u XDG_CACHE_HOME \
            -u BARBIROLLI_RUNTIME_DIR \
            -u CARGO_TARGET_DIR \
            HOME=/tmp/barbirolli-test-home \
            bash -e -c 'source "$1"; printf "%s|%s" "$BARBIROLLI_RUNTIME_DIR" "$CARGO_TARGET_DIR"' \
            _ "$common"
    )" || return 1
    [[ "$default_paths" == '/tmp/barbirolli-test-home/.local/share/barbirolli|/tmp/barbirolli-test-home/.cache/barbirolli-target' ]] || {
        fail "unexpected default paths: $default_paths"
        return 1
    }

    overridden_paths="$(
        BARBIROLLI_RUNTIME_DIR=/runtime \
            CARGO_TARGET_DIR=/cargo-target \
            bash -e -c 'source "$1"; printf "%s|%s" "$BARBIROLLI_RUNTIME_DIR" "$CARGO_TARGET_DIR"' \
            _ "$common"
    )" || return 1
    [[ "$overridden_paths" == '/runtime|/cargo-target' ]] || {
        fail "environment overrides were not preserved: $overridden_paths"
        return 1
    }
}

test_common_normalizes_supported_architectures() {
    local common
    common="$SCRIPTS_DIR/lib/common.sh"

    [[ "$(bash -c 'source "$1"; normalize_arch "$2"' _ "$common" arm64)" == 'aarch64' ]] ||
        return 1
    [[ "$(bash -c 'source "$1"; normalize_arch "$2"' _ "$common" aarch64)" == 'aarch64' ]] ||
        return 1
    [[ "$(bash -c 'source "$1"; normalize_arch "$2"' _ "$common" amd64)" == 'x86_64' ]] ||
        return 1
    [[ "$(bash -c 'source "$1"; normalize_arch "$2"' _ "$common" x86_64)" == 'x86_64' ]] ||
        return 1

    local output
    if output="$(bash -c 'source "$1"; normalize_arch "$2"' _ "$common" riscv64 2>&1)"; then
        return 1
    fi
    [[ "$output" == *'unsupported architecture: riscv64'* ]] || {
        fail "unexpected architecture error: $output"
        return 1
    }
}

test_setup_is_idempotent_when_artifacts_exist() {
    local temporary runtime fake_bin calls output
    temporary="$(mktemp -d)"
    runtime="$temporary/runtime"
    fake_bin="$temporary/bin"
    calls="$temporary/calls"
    mkdir -p "$runtime/bin" "$runtime/images" "$runtime/ssh" "$fake_bin"
    : >"$calls"

    write_executable "$runtime/bin/firecracker" \
        '#!/usr/bin/env bash' \
        'printf "Firecracker v1.13.2\n"'
    printf 'kernel\n' >"$runtime/images/vmlinux"
    printf 'rootfs\n' >"$runtime/images/ubuntu-24.04.ext4"
    ln -s ubuntu-24.04.ext4 "$runtime/images/alpine.ext4"
    printf 'private\n' >"$runtime/ssh/id_ed25519"
    printf 'ssh-ed25519 AAAA test\n' >"$runtime/ssh/id_ed25519.pub"
    cp "$runtime/ssh/id_ed25519.pub" \
        "$runtime/images/ubuntu-24.04.ext4.authorized_key"

    write_executable "$fake_bin/uname" \
        '#!/usr/bin/env bash' \
        'case "${1:-}" in -s) echo Linux ;; -m) echo aarch64 ;; *) echo Linux ;; esac'
    local command_name
    for command_name in curl tar sha256sum ssh-keygen unsquashfs truncate mkfs.ext4 install sudo; do
        write_executable "$fake_bin/$command_name" \
            '#!/usr/bin/env bash' \
            'printf "%s\n" "$(basename "$0")" >>"$CALL_LOG"' \
            'exit 99'
    done

    output="$(
        PATH="$fake_bin:/usr/bin:/bin:/usr/sbin" \
            CALL_LOG="$calls" \
            BARBIROLLI_RUNTIME_DIR="$runtime" \
            CARGO_TARGET_DIR="$temporary/target" \
            "$SCRIPTS_DIR/setup" 2>&1
    )"
    local status=$?
    local called
    called="$(<"$calls")"
    rm -rf -- "$temporary"

    [[ $status -eq 0 ]] || {
        fail "setup failed: $output"
        return 1
    }
    [[ -z "$called" ]] || {
        fail "setup invoked external work: $called"
        return 1
    }
    [[ "$output" == *'runtime is already prepared'* ]] || {
        fail "setup did not report idempotency: $output"
        return 1
    }
}

test_common_rejects_rootfs_prepared_for_different_key() {
    local temporary runtime
    temporary="$(mktemp -d)"
    runtime="$temporary/runtime"
    mkdir -p "$runtime/bin" "$runtime/images" "$runtime/ssh"

    write_executable "$runtime/bin/firecracker" \
        '#!/usr/bin/env bash' \
        'printf "Firecracker v1.13.2\n"'
    printf 'kernel\n' >"$runtime/images/vmlinux"
    printf 'rootfs\n' >"$runtime/images/ubuntu-24.04.ext4"
    ln -s ubuntu-24.04.ext4 "$runtime/images/alpine.ext4"
    printf 'private\n' >"$runtime/ssh/id_ed25519"
    printf 'ssh-ed25519 AAAA current\n' >"$runtime/ssh/id_ed25519.pub"
    printf 'ssh-ed25519 AAAA stale\n' >"$runtime/images/ubuntu-24.04.ext4.authorized_key"

    if BARBIROLLI_RUNTIME_DIR="$runtime" \
        bash -c 'source "$1"; runtime_is_prepared' \
        _ "$SCRIPTS_DIR/lib/common.sh"; then
        rm -rf -- "$temporary"
        fail "runtime accepted a rootfs prepared for a different SSH key"
        return 1
    fi

    rm -rf -- "$temporary"
}

test_run_explains_when_setup_is_missing() {
    local temporary fake_bin output
    temporary="$(mktemp -d)"
    fake_bin="$temporary/bin"
    mkdir -p "$fake_bin"

    write_executable "$fake_bin/uname" \
        '#!/usr/bin/env bash' \
        'case "${1:-}" in -s) echo Linux ;; -m) echo aarch64 ;; *) echo Linux ;; esac'
    write_executable "$fake_bin/cargo" '#!/usr/bin/env bash' 'exit 99'
    write_executable "$fake_bin/sudo" '#!/usr/bin/env bash' 'exit 99'

    if output="$(
        PATH="$fake_bin:/usr/bin:/bin:/usr/sbin" \
            BARBIROLLI_RUNTIME_DIR="$temporary/runtime" \
            CARGO_TARGET_DIR="$temporary/target" \
            "$SCRIPTS_DIR/run" 2>&1
    )"; then
        rm -rf -- "$temporary"
        return 1
    fi
    rm -rf -- "$temporary"

    [[ "$output" == *'run scripts/setup first'* ]] || {
        fail "run did not explain the missing setup: $output"
        return 1
    }
}

test_common_reports_command_timeouts() {
    local temporary fake_bin output
    temporary="$(mktemp -d)"
    fake_bin="$temporary/bin"
    mkdir -p "$fake_bin"
    write_executable "$fake_bin/timeout" '#!/usr/bin/env bash' 'exit 124'

    output="$(
        PATH="$fake_bin:/usr/bin:/bin" \
            BARBIROLLI_COMMAND_TIMEOUT=9 \
            bash -c 'source "$1"; run_with_timeout "cargo test" true' \
            _ "$SCRIPTS_DIR/lib/common.sh" 2>&1
    )"
    local status=$?
    if [[ $status -eq 0 ]]; then
        rm -rf -- "$temporary"
        return 1
    fi
    rm -rf -- "$temporary"

    [[ $status -eq 124 ]] || {
        fail "timeout returned $status instead of 124"
        return 1
    }
    [[ "$output" == *'cargo test timed out after 9s'* ]] || {
        fail "timeout was not explained: $output"
        return 1
    }
}

test_test_command_runs_all_phases_in_order() {
    local temporary fake_bin copied_scripts calls output
    temporary="$(mktemp -d)"
    fake_bin="$temporary/bin"
    copied_scripts="$temporary/scripts"
    calls="$temporary/calls"
    mkdir -p "$fake_bin" "$copied_scripts/lib" "$copied_scripts/tests"
    : >"$calls"

    cp "$SCRIPTS_DIR/test" "$copied_scripts/test" || return 1
    cp "$SCRIPTS_DIR/lib/common.sh" "$copied_scripts/lib/common.sh" || return 1
    write_executable "$copied_scripts/tests/test_scripts.sh" \
        '#!/usr/bin/env bash' \
        'printf "shell-tests\n" >>"$CALL_LOG"'
    write_executable "$fake_bin/uname" \
        '#!/usr/bin/env bash' \
        'case "${1:-}" in -s) echo Linux ;; -m) echo aarch64 ;; *) echo Linux ;; esac'
    write_executable "$fake_bin/timeout" \
        '#!/usr/bin/env bash' \
        '[[ "${1:-}" == "--foreground" ]] && shift' \
        'shift' \
        '"$@"'
    write_executable "$fake_bin/cargo" \
        '#!/usr/bin/env bash' \
        'printf "cargo:%s\n" "$*" >>"$CALL_LOG"'

    output="$(
        PATH="$fake_bin:/usr/bin:/bin:/usr/sbin" \
            CALL_LOG="$calls" \
            BARBIROLLI_RUNTIME_DIR="$temporary/runtime" \
            CARGO_TARGET_DIR="$temporary/target" \
            BARBIROLLI_COMMAND_TIMEOUT=9 \
            "$copied_scripts/test" 2>&1
    )"
    local status=$?
    local actual
    actual="$(<"$calls")"
    rm -rf -- "$temporary"

    [[ $status -eq 0 ]] || {
        fail "test command failed: $output"
        return 1
    }
    local expected
    expected=$'shell-tests\ncargo:fmt --all -- --check\ncargo:clippy --workspace --all-targets --features linux -- -D warnings\ncargo:test --workspace --features linux'
    [[ "$actual" == "$expected" ]] || {
        fail "unexpected test phases:"
        printf '%s\n' "$actual" >&2
        return 1
    }
}

test_smoke_exercises_http_and_ssh_lifecycle() {
    local temporary runtime fake_bin calls output
    temporary="$(mktemp -d)"
    runtime="$temporary/runtime"
    fake_bin="$temporary/bin"
    calls="$temporary/calls"
    mkdir -p "$runtime/bin" "$runtime/images" "$runtime/ssh" "$fake_bin"
    : >"$calls"

    write_executable "$runtime/bin/firecracker" \
        '#!/usr/bin/env bash' \
        'printf "Firecracker v1.13.2\n"'
    printf 'kernel\n' >"$runtime/images/vmlinux"
    printf 'rootfs\n' >"$runtime/images/ubuntu-24.04.ext4"
    ln -s ubuntu-24.04.ext4 "$runtime/images/alpine.ext4"
    printf 'private\n' >"$runtime/ssh/id_ed25519"
    printf 'ssh-ed25519 AAAA test\n' >"$runtime/ssh/id_ed25519.pub"
    cp "$runtime/ssh/id_ed25519.pub" \
        "$runtime/images/ubuntu-24.04.ext4.authorized_key"

    write_executable "$fake_bin/uname" \
        '#!/usr/bin/env bash' \
        'case "${1:-}" in -s) echo Linux ;; -m) echo aarch64 ;; *) echo Linux ;; esac'
    write_executable "$fake_bin/curl" \
        '#!/usr/bin/env bash' \
        'method=GET' \
        'url=' \
        'while (($#)); do' \
        '    case "$1" in' \
        '        -X|--request) method="$2"; shift 2 ;;' \
        '        -d|--data|--data-binary|-H|--header|--max-time) shift 2 ;;' \
        '        http://*|https://*) url="$1"; shift ;;' \
        '        *) shift ;;' \
        '    esac' \
        'done' \
        'printf "%s %s\n" "$method" "$url" >>"$CALL_LOG"' \
        'case "$method $url" in' \
        '    "GET "*/vms) printf "[]\n" ;;' \
        '    "POST "*/vms/*/start) printf "{\"id\":7,\"status\":\"running\"}\n" ;;' \
        '    "POST "*/vms/*/shutdown) printf "{\"id\":7,\"status\":\"discovered\"}\n" ;;' \
        '    "POST "*/vms) printf "{\"id\":7}\n" ;;' \
        '    "DELETE "*/vms/*) ;;' \
        '    *) exit 65 ;;' \
        'esac'
    write_executable "$fake_bin/ssh" \
        '#!/usr/bin/env bash' \
        'printf "ssh:%s\n" "$*" >>"$CALL_LOG"' \
        'printf "Linux smoke 6.1.0\n"'
    write_executable "$fake_bin/timeout" \
        '#!/usr/bin/env bash' \
        'printf "timeout:%s\n" "$*" >>"$CALL_LOG"' \
        '[[ "${1:-}" == "--foreground" ]] && shift' \
        'shift' \
        '"$@"'

    output="$(
        PATH="$fake_bin:/usr/bin:/bin:/usr/sbin" \
            CALL_LOG="$calls" \
            BARBIROLLI_RUNTIME_DIR="$runtime" \
            CARGO_TARGET_DIR="$temporary/target" \
            BARBIROLLI_COMMAND_TIMEOUT=9 \
            "$SCRIPTS_DIR/smoke" 2>&1
    )"
    local status=$?
    local actual
    actual="$(<"$calls")"
    rm -rf -- "$temporary"

    [[ $status -eq 0 ]] || {
        fail "smoke command failed: $output"
        return 1
    }
    [[ "$actual" == *'GET http://127.0.0.1:3000/vms'* ]] || return 1
    [[ "$actual" == *'POST http://127.0.0.1:3000/vms'* ]] || return 1
    [[ "$actual" == *'POST http://127.0.0.1:3000/vms/7/start'* ]] || return 1
    [[ "$actual" == *'timeout:--foreground 9 ssh '* ]] || {
        fail "SSH command was not bounded by the configured timeout"
        return 1
    }
    [[ "$actual" == *'ssh:'* ]] || return 1
    [[ "$actual" == *'POST http://127.0.0.1:3000/vms/7/shutdown'* ]] || return 1
    [[ "$actual" == *'DELETE http://127.0.0.1:3000/vms/7'* ]] || return 1
    [[ "$output" == *'smoke test passed'* ]] || {
        fail "smoke success was not reported: $output"
        return 1
    }
}

run_test 'dev prints help' test_dev_prints_help
run_test 'dev rejects unknown commands' test_dev_rejects_unknown_commands
run_test 'dev dispatches and forwards arguments' test_dev_dispatches_and_forwards_arguments
run_test 'common paths default and allow overrides' test_common_paths_default_and_allow_overrides
run_test 'common normalizes supported architectures' test_common_normalizes_supported_architectures
run_test 'setup is idempotent when artifacts exist' test_setup_is_idempotent_when_artifacts_exist
run_test 'common rejects a rootfs prepared for a different key' test_common_rejects_rootfs_prepared_for_different_key
run_test 'run explains when setup is missing' test_run_explains_when_setup_is_missing
run_test 'common reports command timeouts' test_common_reports_command_timeouts
run_test 'test command runs all phases in order' test_test_command_runs_all_phases_in_order
run_test 'smoke exercises HTTP and SSH lifecycle' test_smoke_exercises_http_and_ssh_lifecycle

printf '1..%d\n' "$TESTS_RUN"
if [[ $TESTS_FAILED -ne 0 ]]; then
    printf '%d test(s) failed\n' "$TESTS_FAILED" >&2
    exit 1
fi
