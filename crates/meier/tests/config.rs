use std::{fs, path::Path};

use meier::config::{self, Config};
use tempfile::tempdir;

#[test]
fn init_writes_the_documented_config_and_refuses_overwrite() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("nested/meier/config.json");
    let config = config::init(&path).expect("config should initialize");
    assert_eq!(config, Config::default());
    assert_eq!(
        config::load(&path).expect("config should load"),
        Config::default()
    );
    assert!(config::init(&path).is_err(), "init must use create_new");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn unknown_fields_are_rejected() {
    let directory = tempdir().expect("temporary directory");
    let path = directory.path().join("config.json");
    fs::write(
        &path,
        br#"{"client":{"url":"http://127.0.0.1:3000"},"extra":true}"#,
    )
    .expect("config fixture");
    let error = config::load(Path::new(&path)).expect_err("unknown field should fail");
    assert!(error.to_string().contains("unknown field"));
}
