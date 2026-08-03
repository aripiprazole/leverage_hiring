use std::{fs, process::Command};

use tempfile::tempdir;

#[test]
fn errors_are_json_on_stderr_while_help_remains_raw() {
    let directory = tempdir().expect("temporary directory");
    let config = directory.path().join("config.json");
    let error = Command::new(env!("CARGO_BIN_EXE_meier"))
        .args([
            "--config",
            config.to_str().expect("config path"),
            "vm",
            "ps",
        ])
        .env("RUST_LOG", "off")
        .output()
        .expect("meier invocation");
    assert!(!error.status.success());
    assert!(error.stdout.is_empty());
    let parsed: serde_json::Value = serde_json::from_slice(&error.stderr).expect("JSON error");
    assert!(parsed["error"].as_str().is_some());

    let help = Command::new(env!("CARGO_BIN_EXE_meier"))
        .arg("--help")
        .env("RUST_LOG", "off")
        .output()
        .expect("help invocation");
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).starts_with("Manage Barbirolli"));
    assert!(help.stderr.is_empty());

    let init = Command::new(env!("CARGO_BIN_EXE_meier"))
        .args([
            "--config",
            config.to_str().expect("config path"),
            "config",
            "init",
        ])
        .env("RUST_LOG", "off")
        .output()
        .expect("init invocation");
    assert!(init.status.success());
    assert!(fs::metadata(config).is_ok());
    assert!(serde_json::from_slice::<serde_json::Value>(&init.stdout).is_ok());
}
