use std::{fs, path::PathBuf};

use barbirolli::VmSpec;

#[derive(Clone)]
pub struct VmStorageFixture {
    pub artifact_dir: PathBuf,
}

impl VmStorageFixture {
    #[must_use]
    pub fn new(spec: &VmSpec) -> Self {
        Self {
            artifact_dir: spec.artifact_dir.clone(),
        }
    }

    #[must_use]
    pub fn config(&self) -> serde_json::Value {
        let config =
            fs::read(self.artifact_dir.join("config.json")).expect("failed to read VM config");
        serde_json::from_slice(&config).expect("invalid VM config")
    }

    #[must_use]
    pub fn kernel(&self) -> PathBuf {
        self.artifact_dir.join("vmlinux")
    }

    #[must_use]
    pub fn rootfs(&self) -> PathBuf {
        self.artifact_dir.join("rootfs.ext4")
    }

    #[must_use]
    pub fn serial_log(&self) -> PathBuf {
        self.artifact_dir.join("serial.log")
    }

    #[must_use]
    pub fn authorized_keys(&self) -> PathBuf {
        self.artifact_dir.join("authorized_keys")
    }
}
