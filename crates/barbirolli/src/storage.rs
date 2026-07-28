use std::{
    collections::HashSet,
    fs::{
        canonicalize, copy, create_dir, create_dir_all, read_dir, remove_dir_all, rename,
        symlink_metadata,
    },
    io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{ArtifactName, AuthorizedKey, NetworkSpec, UserName, VmId, VmInput, VmSpec};

const CONFIG_VERSION: u16 = 1;
const SOCKET_DIRECTORY: &str = ".sockets";

pub type Result<T, E = StorageError> = std::result::Result<T, E>;

#[derive(Debug)]
pub struct VmStore {
    pub vm_root: VmRootFolder,
    pub image_root: PathBuf,
    pub default_authorized_keys: PathBuf,
}

#[derive(Debug)]
pub struct VmRootFolder {
    pub(crate) dir: PathBuf,
    paths: std::vec::IntoIter<PathDescriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedVmSpec {
    pub version: u16,
    pub spec: VmSpec,
}

#[derive(Debug)]
pub struct PathDescriptor {
    directory: PathBuf,
    config: PathBuf,
}

#[derive(Debug)]
pub struct VmRootFolderItem {
    pub name: String,
    pub directory: PathBuf,
    pub config: PathBuf,
    pub persisted_spec: PersistedVmSpec,
}

impl VmStore {
    pub fn new(
        vm_root: PathBuf,
        image_root: PathBuf,
        default_authorized_keys: PathBuf,
    ) -> Result<Self, StorageError> {
        create_dir_all(&vm_root).map_err(|error| StorageError::io(&vm_root, error))?;
        let vm_root = canonicalize(&vm_root).map_err(|error| StorageError::io(&vm_root, error))?;
        let socket_dir = vm_root.join(SOCKET_DIRECTORY);
        if !socket_dir.exists() {
            create_dir(&socket_dir).map_err(|error| StorageError::io(&socket_dir, error))?;
        }
        let socket_metadata =
            symlink_metadata(&socket_dir).map_err(|error| StorageError::io(&socket_dir, error))?;
        if !socket_metadata.file_type().is_dir() {
            return Err(StorageError::InvalidConfig {
                path: socket_dir,
                message: "socket path must be a real directory".to_owned(),
            });
        }
        Ok(Self {
            vm_root: VmRootFolder::new(vm_root)?,
            image_root: canonicalize(&image_root)
                .map_err(|error| StorageError::io(&image_root, error))?,
            default_authorized_keys: canonicalize(&default_authorized_keys)
                .map_err(|error| StorageError::io(&default_authorized_keys, error))?,
        })
    }

    pub async fn create(&self, input: VmInput) -> Result<VmSpec, StorageError> {
        let final_dir = self.vm_root.dir.join(input.user.as_ref());
        if final_dir.exists() {
            return Err(StorageError::DuplicateUser(input.user));
        }

        let source_kernel = self.resolve_artifact(&input.kernel)?;
        let source_rootfs = self.resolve_artifact(&input.rootfs)?;
        let id = self.vm_root.next_id()?;

        let tmp = tempfile::Builder::new()
            .prefix(".creating-")
            .tempdir_in(&self.vm_root.dir)
            .map_err(|error| StorageError::io(&self.vm_root.dir, error))?;

        let kernel = tmp.path().join("vmlinux");
        let rootfs = tmp.path().join("rootfs.ext4");

        copy(&source_kernel, &kernel).map_err(|error| StorageError::io(&kernel, error))?;
        copy(&source_rootfs, &rootfs).map_err(|error| StorageError::io(&rootfs, error))?;
        self.write_authorized_keys(tmp.path(), &input.authorized_keys)?;

        let persisted = PersistedVmSpec::new(VmSpec {
            id,
            user: input.user,
            artifact_dir: final_dir.clone(),
            kernel: final_dir.join("vmlinux"),
            rootfs: final_dir.join("rootfs.ext4"),
            vcpu_count: input.vcpu_count,
            memory_mib: input.memory_mib,
            network: NetworkSpec::new(id)
                .map_err(|error| StorageError::InvalidInput(error.to_string()))?,
        });
        persisted.write_into(tmp.path().join("config.json"))?;
        rename(tmp.path(), &final_dir).map_err(|error| StorageError::io(&final_dir, error))?;
        tracing::info!(vm_id = %persisted.spec.id, user = %persisted.spec.user, operation = "create", "VM state transition");
        Ok(persisted.spec)
    }

    pub fn resolve_artifact(&self, artifact: &ArtifactName) -> Result<PathBuf> {
        let requested = self.image_root.join(artifact.as_ref());
        let canonical =
            canonicalize(&requested).map_err(|error| StorageError::io(&requested, error))?;
        if !canonical.starts_with(&self.image_root) {
            return Err(StorageError::InvalidInput(format!(
                "artifact {artifact} resolves outside IMAGE_ROOT"
            )));
        }
        if !canonical.is_file() {
            return Err(StorageError::InvalidInput(format!(
                "artifact {artifact} is not a file"
            )));
        }
        Ok(canonical)
    }

    fn write_authorized_keys(&self, directory: &Path, keys: &[AuthorizedKey]) -> Result<()> {
        let destination = directory.join("authorized_keys");
        if keys.is_empty() {
            copy(&self.default_authorized_keys, &destination)
                .map_err(|error| StorageError::io(&destination, error))?;
        } else {
            let mut contents = keys
                .iter()
                .map(AsRef::as_ref)
                .collect::<Vec<_>>()
                .join("\n");
            contents.push('\n');
            std::fs::write(&destination, contents)
                .map_err(|error| StorageError::io(&destination, error))?;
        }
        Ok(())
    }

    pub fn delete(&self, spec: &VmSpec) -> Result<()> {
        remove_dir_all(&spec.artifact_dir)
            .map_err(|error| StorageError::io(&spec.artifact_dir, error))
    }
}

impl PersistedVmSpec {
    pub fn new(spec: VmSpec) -> Self {
        Self {
            version: CONFIG_VERSION,
            spec,
        }
    }

    pub fn write_into(&self, path: PathBuf) -> Result<()> {
        let contents =
            serde_json::to_vec_pretty(&self).map_err(|error| StorageError::InvalidConfig {
                path: path.clone(),
                message: error.to_string(),
            })?;
        std::fs::write(&path, contents).map_err(|error| StorageError::io(&path, error))?;
        Ok(())
    }
}

impl PathDescriptor {
    fn config(&self) -> Result<Vec<u8>> {
        let metadata = std::fs::symlink_metadata(&self.config)
            .map_err(|error| StorageError::io(&self.config, error))?;
        if !metadata.is_file() {
            return Err(StorageError::InvalidConfig {
                path: self.config.clone(),
                message: "config.json must be a real file".to_owned(),
            });
        }

        std::fs::read(&self.config).map_err(|error| StorageError::io(&self.config, error))
    }

    fn vm_name(&self) -> Result<String> {
        let name = self
            .directory
            .file_name()
            .expect("a VM_ROOT directory entry always has a file name");
        if name == SOCKET_DIRECTORY {
            return Err(StorageError::SocketDirectory);
        }
        if name.as_encoded_bytes().starts_with(".creating-".as_bytes()) {
            tracing::warn!(path = %self.directory.display(), "removing stale VM creation directory");
            remove_dir_all(&self.directory)
                .map_err(|error| StorageError::io(&self.directory, error))?;
            return Err(StorageError::CreatingDirectory);
        }
        Ok(name.to_string_lossy().into_owned())
    }
}

impl VmRootFolderItem {
    fn verify(&self) -> Result<()> {
        let directory = &self.directory;
        let spec = &self.persisted_spec.spec;
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
                    path: self.config.clone(),
                    message: format!(
                        "configured path {} does not match {}",
                        actual.display(),
                        expected.display()
                    ),
                });
            }
        }
        let network = NetworkSpec::new(spec.id).map_err(|error| StorageError::InvalidConfig {
            path: self.config.clone(),
            message: error.to_string(),
        })?;
        if spec.network != network {
            return Err(StorageError::InvalidConfig {
                path: self.config.clone(),
                message: "configured network does not match VM ID".to_owned(),
            });
        }
        for required in ["vmlinux", "rootfs.ext4", "authorized_keys"] {
            let path = directory.join(required);
            let is_file = symlink_metadata(&path)
                .map(|metadata| metadata.file_type().is_file())
                .unwrap_or(false);
            if !is_file {
                return Err(StorageError::InvalidConfig {
                    path,
                    message: "required artifact must be a real file".to_owned(),
                });
            }
        }
        Ok(())
    }
}

impl VmRootFolder {
    pub fn new(vm_root: impl AsRef<Path>) -> Result<Self> {
        let vm_root = vm_root.as_ref();
        let directory = read_dir(vm_root).map_err(|error| StorageError::io(vm_root, error))?;
        let mut paths = Vec::new();
        for entry in directory {
            let entry = entry.map_err(|error| StorageError::io(vm_root, error))?;
            let directory = entry.path();
            let metadata = symlink_metadata(&directory)
                .map_err(|error| StorageError::io(&directory, error))?;
            if !metadata.file_type().is_dir() {
                return Err(StorageError::InvalidConfig {
                    path: directory,
                    message: "VM entry must be a real directory".to_owned(),
                });
            }
            paths.push(PathDescriptor {
                config: directory.join("config.json"),
                directory,
            });
        }
        Ok(Self {
            dir: PathBuf::from(vm_root),
            paths: paths.into_iter(),
        })
    }

    fn next_id(&self) -> Result<VmId> {
        let occupied = VmRootFolder::new(&self.dir)?
            .map(|item| item.map(|item| item.persisted_spec.spec.id))
            .collect::<Result<HashSet<_>>>()?;
        (0..=16_383)
            .map(|id| VmId::try_from(id).expect("the VM ID range is valid"))
            .find(|id| !occupied.contains(id))
            .ok_or(StorageError::IdsExhausted)
    }

    fn next_item(&mut self) -> Result<Option<VmRootFolderItem>> {
        loop {
            let Some(descriptor) = self.paths.next() else {
                return Ok(None);
            };
            let name = match descriptor.vm_name() {
                Ok(name) => name,
                Err(StorageError::CreatingDirectory | StorageError::SocketDirectory) => continue,
                Err(err) => return Err(err),
            };
            let config = descriptor.config()?;
            let spec = serde_json::from_slice::<PersistedVmSpec>(&config).map_err(|error| {
                StorageError::InvalidConfig {
                    path: descriptor.config.clone(),
                    message: error.to_string(),
                }
            })?;
            if spec.version != CONFIG_VERSION {
                return Err(StorageError::InvalidConfig {
                    path: descriptor.config.clone(),
                    message: format!("unsupported config version {}", spec.version),
                });
            }

            let item = VmRootFolderItem {
                directory: descriptor.directory,
                config: descriptor.config,
                name,
                persisted_spec: spec,
            };
            item.verify()?;
            return Ok(Some(item));
        }
    }
}

impl ExactSizeIterator for VmRootFolder {
    fn len(&self) -> usize {
        self.paths.len()
    }
}

impl Iterator for VmRootFolder {
    type Item = Result<VmRootFolderItem>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_item() {
            Ok(Some(val)) => Some(Ok(val)),
            Ok(None) => None,
            Err(err) => Some(Err(err)),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("invalid VM input: {0}")]
    InvalidInput(String),
    #[error("user {0} already owns a VM")]
    DuplicateUser(UserName),
    #[error("no VM IDs remain")]
    IdsExhausted,
    #[error("socket directory")]
    SocketDirectory,
    #[error("stale VM creation directory")]
    CreatingDirectory,
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
