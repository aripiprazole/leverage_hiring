#![cfg(target_os = "linux")]

use std::{fs, process::Command};

#[test]
#[ignore = "requires root to apply OCI user and supplementary-group settings"]
fn configures_and_executes_the_oci_process() {
    let temporary = tempfile::tempdir().expect("temporary directory should be created");
    let spec = temporary.path().join("process.json");
    fs::write(
        &spec,
        r#"{
            "terminal": false,
            "user": { "uid": 0, "gid": 0 },
            "args": ["/bin/sh", "-c", "exit 23"],
            "env": ["PATH=/bin:/usr/bin"],
            "cwd": "/",
            "noNewPrivileges": true
        }"#,
    )
    .expect("process spec should be written");
    let exit = Command::new(env!("CARGO_BIN_EXE_barbirolli_entrypoint"))
        .env("BARBIROLLI_ENTRYPOINT_RUN_PROCESS_SPEC", spec)
        .env("BARBIROLLI_ENTRYPOINT_RUN_PROCESS_SIGNAL_MASK", "0")
        .status()
        .expect("OCI process should execute through the entrypoint helper");

    assert_eq!(exit.code(), Some(23));
}
