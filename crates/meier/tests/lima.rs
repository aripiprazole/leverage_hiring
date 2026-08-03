#![cfg(target_os = "macos")]

use std::{fs, os::unix::fs::PermissionsExt, process::Command};

use tempfile::tempdir;

#[test]
fn macos_routes_api_calls_through_running_lima_without_guest_meier() {
    let directory = tempdir().expect("temporary directory");
    let script = fake_limactl(directory.path().join("trace"));
    let config = directory.path().join("config.json");
    let output = Command::new(env!("CARGO_BIN_EXE_meier"))
        .args([
            "--config",
            config.to_str().expect("config path"),
            "config",
            "init",
        ])
        .env("RUST_LOG", "off")
        .output()
        .expect("config init");
    assert!(output.status.success());

    let output = Command::new(env!("CARGO_BIN_EXE_meier"))
        .args([
            "--config",
            config.to_str().expect("config path"),
            "vm",
            "ps",
        ])
        .env("RUST_LOG", "off")
        .env("LIMACTL", &script)
        .output()
        .expect("vm ps");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "[]");
    let trace = fs::read_to_string(directory.path().join("trace")).expect("limactl trace");
    assert!(trace.contains("list --format json provisioning"));
    assert!(trace.contains("shell provisioning -- curl"));
    assert!(!trace.contains("meier "), "the guest must not invoke meier");
}

#[test]
fn macos_rejects_missing_or_stopped_lima_without_starting_it() {
    let directory = tempdir().expect("temporary directory");
    let script = fake_limactl(directory.path().join("trace"));
    let config = directory.path().join("config.json");
    let init = Command::new(env!("CARGO_BIN_EXE_meier"))
        .args([
            "--config",
            config.to_str().expect("config path"),
            "config",
            "init",
        ])
        .env("RUST_LOG", "off")
        .output()
        .expect("config init");
    assert!(init.status.success());

    let output = Command::new(env!("CARGO_BIN_EXE_meier"))
        .args([
            "--config",
            config.to_str().expect("config path"),
            "vm",
            "ps",
        ])
        .env("RUST_LOG", "off")
        .env("LIMACTL", &script)
        .env("FAKE_STOPPED", "1")
        .output()
        .expect("stopped vm ps");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not running"));
    let trace = fs::read_to_string(directory.path().join("trace")).expect("limactl trace");
    assert!(
        !trace.contains("shell provisioning"),
        "stopped Lima must not receive guest work"
    );
}

fn fake_limactl(trace: std::path::PathBuf) -> std::path::PathBuf {
    let script = trace.with_file_name("limactl");
    let contents = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nif [ \"$1\" = list ]; then\n  if [ \"$FAKE_STOPPED\" = 1 ]; then printf '[{{\"status\":\"Stopped\"}}]'; else printf '[{{\"status\":\"Running\"}}]'; fi\n  exit 0\nfi\nif [ \"$1\" = shell ]; then\n  [ \"$4\" = meier ] && exit 99\n  printf '[]\\n__MEIER_HTTP_STATUS__200'\n  exit 0\nfi\nexit 64\n",
        quote(&trace.display().to_string())
    );
    fs::write(&script, contents).expect("fake limactl");
    let mut permissions = fs::metadata(&script).expect("metadata").permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&script, permissions).expect("permissions");
    script
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
