use std::{
    collections::HashSet,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use tokio::{fs, sync::Mutex};

use crate::{ArtifactName, AuthorizedKey, NetworkSpec, UserName, VmId, VmInput, VmSpec};

const CONFIG_VERSION: u16 = 1;
const SOCKET_DIRECTORY: &str = ".sockets";
const UNIX_SOCKET_PATH_LIMIT: usize = 108;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("invalid VM input: {0}")]
    InvalidInput(String),
    #[error("user {0} already owns a VM")]
    DuplicateUser(UserName),
    #[error("no VM IDs remain")]
    IdsExhausted,
    #[error("storage operation failed for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid configuration in {path}: {message}")]
    InvalidConfig { path: PathBuf, message: String },
}

impl StorageError {
    fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedVmSpec {
    version: u16,
    spec: VmSpec,
}

#[derive(Debug, Clone)]
pub struct VmStore {
    vm_root: PathBuf,
    image_root: PathBuf,
    default_authorized_keys: PathBuf,
    creation: Arc<Mutex<()>>,
}

impl VmStore {
    pub fn new(
        vm_root: PathBuf,
        image_root: PathBuf,
        default_authorized_keys: PathBuf,
    ) -> Result<Self, StorageError> {
        std::fs::create_dir_all(&vm_root).map_err(|error| StorageError::io(&vm_root, error))?;
        let vm_root =
            std::fs::canonicalize(&vm_root).map_err(|error| StorageError::io(&vm_root, error))?;
        let socket_directory = vm_root.join(SOCKET_DIRECTORY);
        match std::fs::symlink_metadata(&socket_directory) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(StorageError::InvalidConfig {
                    path: socket_directory,
                    message: "socket entry must be a real directory".to_owned(),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                std::fs::create_dir(&socket_directory)
                    .map_err(|error| StorageError::io(&socket_directory, error))?;
            }
            Err(error) => return Err(StorageError::io(&socket_directory, error)),
        }
        let image_root = std::fs::canonicalize(&image_root)
            .map_err(|error| StorageError::io(&image_root, error))?;
        let default_authorized_keys = std::fs::canonicalize(&default_authorized_keys)
            .map_err(|error| StorageError::io(&default_authorized_keys, error))?;
        Ok(Self {
            vm_root,
            image_root,
            default_authorized_keys,
            creation: Arc::new(Mutex::new(())),
        })
    }

    pub fn vm_root(&self) -> &Path {
        &self.vm_root
    }

    pub fn image_root(&self) -> &Path {
        &self.image_root
    }

    pub async fn discover(&self) -> Result<Vec<VmSpec>, StorageError> {
        let _guard = self.creation.lock().await;
        self.discover_unlocked().await
    }

    async fn discover_unlocked(&self) -> Result<Vec<VmSpec>, StorageError> {
        let mut directory = fs::read_dir(&self.vm_root)
            .await
            .map_err(|error| StorageError::io(&self.vm_root, error))?;
        let mut specs = Vec::new();
        let mut ids = HashSet::new();
        let mut users = HashSet::new();

        while let Some(entry) = directory
            .next_entry()
            .await
            .map_err(|error| StorageError::io(&self.vm_root, error))?
        {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == SOCKET_DIRECTORY {
                continue;
            }
            if name.starts_with(".creating-") {
                tracing::warn!(path = %path.display(), "removing stale VM creation directory");
                fs::remove_dir_all(&path)
                    .await
                    .map_err(|error| StorageError::io(&path, error))?;
                continue;
            }
            let metadata = fs::symlink_metadata(&path)
                .await
                .map_err(|error| StorageError::io(&path, error))?;
            if !metadata.is_dir() {
                return Err(StorageError::InvalidConfig {
                    path,
                    message: "VM entry must be a real directory".to_owned(),
                });
            }

            let config_path = path.join("config.json");
            let bytes = fs::read(&config_path)
                .await
                .map_err(|error| StorageError::io(&config_path, error))?;
            let persisted: PersistedVmSpec =
                serde_json::from_slice(&bytes).map_err(|error| StorageError::InvalidConfig {
                    path: config_path.clone(),
                    message: error.to_string(),
                })?;
            if persisted.version != CONFIG_VERSION {
                return Err(StorageError::InvalidConfig {
                    path: config_path,
                    message: format!("unsupported config version {}", persisted.version),
                });
            }
            self.verify_discovered(&path, &persisted.spec).await?;
            if !ids.insert(persisted.spec.vm_id) {
                return Err(StorageError::InvalidConfig {
                    path,
                    message: format!("duplicate VM ID {}", persisted.spec.vm_id),
                });
            }
            if !users.insert(persisted.spec.user.clone()) {
                return Err(StorageError::InvalidConfig {
                    path,
                    message: format!("duplicate user {}", persisted.spec.user),
                });
            }
            specs.push(persisted.spec);
        }

        specs.sort_by_key(|spec| spec.vm_id);
        Ok(specs)
    }

    async fn verify_discovered(&self, directory: &Path, spec: &VmSpec) -> Result<(), StorageError> {
        if directory.file_name().and_then(|name| name.to_str()) != Some(spec.user.as_ref()) {
            return Err(StorageError::InvalidConfig {
                path: directory.to_owned(),
                message: "directory name does not match configured user".to_owned(),
            });
        }
        let expected = [
            (&spec.artifact_dir, directory.to_owned()),
            (&spec.kernel, directory.join("vmlinux")),
            (&spec.rootfs, directory.join("rootfs.ext4")),
        ];
        for (actual, expected) in expected {
            if actual != &expected {
                return Err(StorageError::InvalidConfig {
                    path: directory.to_owned(),
                    message: format!(
                        "configured path {} does not match {}",
                        actual.display(),
                        expected.display()
                    ),
                });
            }
        }
        let expected_network = NetworkSpec::new(spec.vm_id, self.socket_path(spec.vm_id)?)
            .map_err(|error| StorageError::InvalidConfig {
                path: directory.to_owned(),
                message: error.to_string(),
            })?;
        if spec.network != expected_network {
            return Err(StorageError::InvalidConfig {
                path: directory.to_owned(),
                message: "configured network does not match VM ID".to_owned(),
            });
        }
        for required in ["vmlinux", "rootfs.ext4", "authorized_keys"] {
            let path = directory.join(required);
            let metadata = fs::symlink_metadata(&path)
                .await
                .map_err(|error| StorageError::io(&path, error))?;
            if !metadata.is_file() {
                return Err(StorageError::InvalidConfig {
                    path,
                    message: "required artifact must be a real file".to_owned(),
                });
            }
        }
        Ok(())
    }

    pub async fn create(&self, input: VmInput) -> Result<VmSpec, StorageError> {
        let _guard = self.creation.lock().await;
        let final_directory = self.vm_root.join(input.user.as_ref());
        if fs::try_exists(&final_directory)
            .await
            .map_err(|error| StorageError::io(&final_directory, error))?
        {
            return Err(StorageError::DuplicateUser(input.user));
        }

        let discovered = self.discover_unlocked().await?;
        let used: HashSet<_> = discovered.iter().map(|spec| spec.vm_id).collect();
        let vm_id = (0..16_384)
            .find_map(|id| {
                let id = VmId::try_from(id).expect("range is a valid VM ID");
                (!used.contains(&id)).then_some(id)
            })
            .ok_or(StorageError::IdsExhausted)?;

        let source_kernel = self.resolve_artifact(&input.kernel).await?;
        let source_rootfs = self.resolve_artifact(&input.rootfs).await?;
        let temporary = tempfile::Builder::new()
            .prefix(".creating-")
            .tempdir_in(&self.vm_root)
            .map_err(|error| StorageError::io(&self.vm_root, error))?;
        let temporary_path = temporary.path();

        let kernel = temporary_path.join("vmlinux");
        if fs::hard_link(&source_kernel, &kernel).await.is_err() {
            fs::copy(&source_kernel, &kernel)
                .await
                .map_err(|error| StorageError::io(&kernel, error))?;
        }
        let rootfs = temporary_path.join("rootfs.ext4");
        fs::copy(&source_rootfs, &rootfs)
            .await
            .map_err(|error| StorageError::io(&rootfs, error))?;
        self.write_authorized_keys(temporary_path, &input.authorized_keys)
            .await?;

        let spec = VmSpec {
            vm_id,
            user: input.user,
            artifact_dir: final_directory.clone(),
            kernel: final_directory.join("vmlinux"),
            rootfs: final_directory.join("rootfs.ext4"),
            vcpu_count: input.vcpu_count,
            memory_mib: input.memory_mib,
            network: NetworkSpec::new(vm_id, self.socket_path(vm_id)?)
                .map_err(|error| StorageError::InvalidInput(error.to_string()))?,
        };
        let config = serde_json::to_vec_pretty(&PersistedVmSpec {
            version: CONFIG_VERSION,
            spec: spec.clone(),
        })
        .expect("VmSpec serialization is infallible");
        let config_path = temporary_path.join("config.json");
        fs::write(&config_path, config)
            .await
            .map_err(|error| StorageError::io(&config_path, error))?;

        fs::rename(temporary_path, &final_directory)
            .await
            .map_err(|error| StorageError::io(&final_directory, error))?;
        tracing::info!(vm_id = %spec.vm_id, user = %spec.user, operation = "create", "VM state transition");
        Ok(spec)
    }

    async fn resolve_artifact(&self, artifact: &ArtifactName) -> Result<PathBuf, StorageError> {
        let requested = self.image_root.join(artifact.as_ref());
        let canonical = fs::canonicalize(&requested)
            .await
            .map_err(|error| StorageError::io(&requested, error))?;
        if !canonical.starts_with(&self.image_root) {
            return Err(StorageError::InvalidInput(format!(
                "artifact {artifact} resolves outside IMAGE_ROOT"
            )));
        }
        let metadata = fs::metadata(&canonical)
            .await
            .map_err(|error| StorageError::io(&canonical, error))?;
        if !metadata.is_file() {
            return Err(StorageError::InvalidInput(format!(
                "artifact {artifact} is not a file"
            )));
        }
        Ok(canonical)
    }

    async fn write_authorized_keys(
        &self,
        directory: &Path,
        keys: &[AuthorizedKey],
    ) -> Result<(), StorageError> {
        let destination = directory.join("authorized_keys");
        if keys.is_empty() {
            fs::copy(&self.default_authorized_keys, &destination)
                .await
                .map_err(|error| StorageError::io(&destination, error))?;
        } else {
            let mut contents = keys
                .iter()
                .map(AsRef::as_ref)
                .collect::<Vec<_>>()
                .join("\n");
            contents.push('\n');
            fs::write(&destination, contents)
                .await
                .map_err(|error| StorageError::io(&destination, error))?;
        }
        Ok(())
    }

    pub async fn delete(&self, spec: &VmSpec) -> Result<(), StorageError> {
        let _guard = self.creation.lock().await;
        fs::remove_dir_all(&spec.artifact_dir)
            .await
            .map_err(|error| StorageError::io(&spec.artifact_dir, error))
    }

    fn socket_path(&self, vm_id: VmId) -> Result<PathBuf, StorageError> {
        let path = self
            .vm_root
            .join(SOCKET_DIRECTORY)
            .join(format!("{vm_id}.socket"));
        if path.as_os_str().as_encoded_bytes().len() >= UNIX_SOCKET_PATH_LIMIT {
            Err(StorageError::InvalidInput(format!(
                "VM_ROOT is too long for Firecracker API socket {}",
                path.display()
            )))
        } else {
            Ok(path)
        }
    }
}
