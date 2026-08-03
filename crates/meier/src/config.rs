use std::{
    env, fs,
    net::SocketAddr,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::MeierError;

const DEFAULT_CONFIG_DIR: &str = ".meier";
const DEFAULT_CONFIG_FILE: &str = "config.json";
const DEFAULT_RUNTIME_DIR: &str = "~/.local/share/barbirolli";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub lima: LimaConfig,
    #[serde(default)]
    pub client: ClientConfig,
    #[serde(default)]
    pub daemon: DaemonConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct LimaConfig {
    pub instance: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ClientConfig {
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    pub listen: SocketAddr,
    pub runtime_dir: String,
}

impl Default for LimaConfig {
    fn default() -> Self {
        Self {
            instance: "provisioning".to_owned(),
        }
    }
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:3000".to_owned(),
        }
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 3000)),
            runtime_dir: DEFAULT_RUNTIME_DIR.to_owned(),
        }
    }
}

#[must_use]
pub fn default_path() -> PathBuf {
    home_dir()
        .map(|home| home.join(DEFAULT_CONFIG_DIR).join(DEFAULT_CONFIG_FILE))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_DIR).join(DEFAULT_CONFIG_FILE))
}

#[must_use]
pub fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

#[tracing::instrument(skip_all, fields(path = %path.display()), err)]
pub fn load(path: &Path) -> Result<Config, MeierError> {
    let bytes = fs::read(path).map_err(|source| MeierError::ConfigRead {
        path: path.to_owned(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| MeierError::ConfigDecode {
        path: path.to_owned(),
        source,
    })
}

#[tracing::instrument(skip_all, fields(path = %path.display()), err)]
pub fn init(path: &Path) -> Result<Config, MeierError> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| MeierError::ConfigWrite {
        path: parent.to_owned(),
        source,
    })?;
    let config = Config::default();
    let encoded = serde_json::to_vec_pretty(&config)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|source| MeierError::ConfigWrite {
            path: path.to_owned(),
            source,
        })?;
    use std::io::Write;
    file.write_all(&encoded)
        .map_err(|source| MeierError::ConfigWrite {
            path: path.to_owned(),
            source,
        })?;
    file.write_all(b"\n")
        .map_err(|source| MeierError::ConfigWrite {
            path: path.to_owned(),
            source,
        })?;
    tracing::info!(path = %path.display(), "initialized Meier configuration");
    Ok(config)
}

impl DaemonConfig {
    fn expand_runtime_dir(&self) -> Result<PathBuf, MeierError> {
        if self.runtime_dir == "~" {
            return home_dir().ok_or_else(|| MeierError::Validation("HOME is not set".to_owned()));
        }
        if let Some(rest) = self.runtime_dir.strip_prefix("~/") {
            return Ok(home_dir()
                .ok_or_else(|| MeierError::Validation("HOME is not set".to_owned()))?
                .join(rest));
        }
        Ok(PathBuf::from(&self.runtime_dir))
    }
}

#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub root: PathBuf,
    pub downloads: PathBuf,
    pub images: PathBuf,
    pub vms: PathBuf,
    pub ssh: PathBuf,
    pub firecracker: PathBuf,
    pub entrypoint: PathBuf,
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
    pub authorized_keys: PathBuf,
    pub private_key: PathBuf,
}

pub fn runtime_paths(config: &Config) -> Result<RuntimePaths, MeierError> {
    let root = config.daemon.expand_runtime_dir()?;
    let downloads = root.join("downloads");
    let images = root.join("images");
    let vms = root.join("vms");
    let ssh = root.join("ssh");
    Ok(RuntimePaths {
        firecracker: root.join("bin/firecracker"),
        entrypoint: root.join("bin/barbirolli_entrypoint"),
        kernel: images.join("vmlinux"),
        rootfs: images.join("ubuntu-24.04.ext4"),
        authorized_keys: ssh.join("id_ed25519.pub"),
        private_key: ssh.join("id_ed25519"),
        root,
        downloads,
        images,
        vms,
        ssh,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_file() {
        let encoded = serde_json::to_value(Config::default()).expect("config serializes");
        assert_eq!(encoded["lima"]["instance"], "provisioning");
        assert_eq!(encoded["client"]["url"], "http://127.0.0.1:3000");
        assert_eq!(encoded["daemon"]["listen"], "127.0.0.1:3000");
    }
}
