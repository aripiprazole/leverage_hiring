use std::{fs, path::PathBuf};

use barbirolli::VmSpec;

#[derive(Clone)]
pub struct VmStorageFixture {
    pub artifact_dir: PathBuf,
}

impl VmStorageFixture {
    pub fn new(spec: &VmSpec) -> Self {
        Self {
            artifact_dir: spec.artifact_dir.clone(),
        }
    }

    pub fn config(&self) -> serde_json::Value {
        let config =
            fs::read(self.artifact_dir.join("config.json")).expect("failed to read VM config");
        serde_json::from_slice(&config).expect("invalid VM config")
    }

    pub fn kernel(&self) -> PathBuf {
        self.artifact_dir.join("vmlinux")
    }

    pub fn rootfs(&self) -> PathBuf {
        self.artifact_dir.join("rootfs.ext4")
    }

    pub fn authorized_keys(&self) -> PathBuf {
        self.artifact_dir.join("authorized_keys")
    }
}
