use super::{Result, StorageError};

use std::{
    collections::HashMap,
    fs::{
        canonicalize, copy, create_dir, create_dir_all, read_dir, remove_dir_all, rename,
        set_permissions,
    },
    future::Future,
    io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use nonempty_collections::NEVec;
use rustix::mount;
use serde::{Deserialize, Serialize};
use validated::Validated;

use crate::{ApiSocket, MemoryMib, NetworkSpec, VmId, VmInput, VmSpec};

const CONFIG_VERSION: u16 = 4;
const SOCKET_DIRECTORY: &str = ".sockets";

#[derive(Debug, thiserror::Error)]
pub enum SshAccessError {
    #[error("{operation} failed for {path}: {source}")]
    Operation {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("SSH access setup failed: {setup}; rootfs cleanup also failed: {cleanup}")]
    SetupCleanup {
        setup: Box<Self>,
        cleanup: RootfsCleanupError,
    },
    #[error("rootfs cleanup failed: {0}")]
    Cleanup(#[from] RootfsCleanupError),
}

#[derive(Debug, thiserror::Error)]
pub enum RootfsCleanupError {
    #[error("failed to unmount {mountpoint}: {source}")]
    Unmount {
        mountpoint: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to detach loop device {loop_device}: {source}")]
    Detach {
        loop_device: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "failed to unmount {mountpoint}: {unmount}; \
         failed to detach loop device {loop_device}: {detach}"
    )]
    UnmountAndDetach {
        mountpoint: PathBuf,
        unmount: io::Error,
        loop_device: PathBuf,
        detach: io::Error,
    },
}

#[derive(Debug)]
pub struct VmStore {
    pub vm_root: VmRootFolder,
    pub image_root: PathBuf,
    creation_lock: tokio::sync::Mutex<()>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedVmSpec {
    pub version: u16,
    pub spec: VmSpec,
}

impl VmStore {
    /// Opens the VM store rooted at `vm_root` and validates its image root.
    ///
    /// # Errors
    ///
    /// Returns an error if either root cannot be created, resolved, or read as
    /// a valid VM storage location.
    #[tracing::instrument(
        skip_all,
        fields(
            vm_root = %vm_root.display(),
            image_root = %image_root.display()
        ),
        err
    )]
    pub fn new(vm_root: PathBuf, image_root: PathBuf) -> Result<Self> {
        tracing::info!("creating VM store");
        create_dir_all(&vm_root).map_err(|error| StorageError::io(&vm_root, error))?;
        let vm_root = canonicalize(&vm_root).map_err(|error| StorageError::io(&vm_root, error))?;
        let socket_dir = vm_root.join(SOCKET_DIRECTORY);
        if !socket_dir.exists() {
            create_dir(&socket_dir).map_err(|error| StorageError::io(&socket_dir, error))?;
        }
        Ok(Self {
            vm_root: VmRootFolder::new(vm_root)?,
            image_root: canonicalize(&image_root)
                .map_err(|error| StorageError::io(&image_root, error))?,
            creation_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Creates and persists a VM's artifacts.
    ///
    /// # Errors
    ///
    /// Returns an error if the input is invalid or any artifact, rootfs, or
    /// configuration operation fails.
    #[tracing::instrument(skip(self, input), err)]
    pub async fn create(
        &self,
        input: VmInput,
        memory_mib: MemoryMib,
    ) -> Result<VmSpec, StorageError> {
        let _creation_guard = self.creation_lock.lock().await;
        let source_kernel = self.image_root.join("vmlinux");
        let source_rootfs = self.image_root.join("alpine.ext4");
        let id = self.vm_root.next_id()?;
        let final_dir = self.vm_root.dir.join(id.to_string());

        let tmp = tempfile::Builder::new()
            .prefix(".creating-")
            .tempdir_in(&self.vm_root.dir)
            .map_err(|error| StorageError::io(&self.vm_root.dir, error))?;

        let kernel = tmp.path().join("vmlinux");
        let rootfs = tmp.path().join("rootfs.ext4");

        copy(&source_kernel, &kernel).map_err(|error| StorageError::io(&kernel, error))?;
        copy(&source_rootfs, &rootfs).map_err(|error| StorageError::io(&rootfs, error))?;
        if !input.authorized_keys.is_empty() {
            let mut authorized_keys = input
                .authorized_keys
                .iter()
                .map(AsRef::as_ref)
                .collect::<Vec<_>>()
                .join("\n");
            authorized_keys.push('\n');
            install_authorized_keys_into_rootfs(&rootfs, authorized_keys.as_bytes())?;
        }

        let persisted = PersistedVmSpec::new(VmSpec {
            id,
            deleted: false,
            artifact_dir: final_dir.clone(),
            kernel: final_dir.join("vmlinux"),
            rootfs: final_dir.join("rootfs.ext4"),
            vcpu_count: input.vcpu_count,
            api_socket: ApiSocket::from(
                self.vm_root
                    .dir
                    .join(SOCKET_DIRECTORY)
                    .join(format!("firecracker-{id}.socket")),
            ),
            memory_mib,
            bindings: input.bindings,
            network: NetworkSpec::new(id)
                .map_err(|error| StorageError::InvalidInput(error.to_string()))?,
        });
        persisted.write_into(&tmp.path().join("config.json"))?;
        rename(tmp.path(), &final_dir).map_err(|error| StorageError::io(&final_dir, error))?;
        Ok(persisted.spec)
    }

    /// Loads all persisted VM specifications.
    ///
    /// # Errors
    ///
    /// Returns an error if the VM root cannot be read or a persisted
    /// configuration is invalid.
    pub fn all(&self) -> Result<Vec<VmSpec>> {
        VmRootFolder::new(&self.vm_root.dir)?
            .map(|item| item.map(|item| item.persisted_spec.spec))
            .collect::<Result<Vec<_>, StorageError>>()
    }

    /// Applies `f` to every non-deleted persisted VM.
    ///
    /// # Errors
    ///
    /// Returns an error if persisted VM discovery fails. Callback errors are
    /// accumulated in the returned [`Validated`] value.
    #[tracing::instrument(skip(f))]
    pub async fn collect<T, E, F, Fut>(&self, mut f: F) -> Result<Validated<HashMap<VmId, T>, E>>
    where
        F: FnMut(VmSpec) -> Fut,
        Fut: Future<Output = std::result::Result<T, E>>,
    {
        let mut acc = HashMap::new();
        let mut errors = vec![];
        for spec in self.all()? {
            if spec.deleted {
                continue;
            }

            let id = spec.id;
            match f(spec).await {
                Ok(val) => {
                    acc.insert(id, val);
                }
                Err(err) => {
                    errors.push(err);
                }
            }
        }
        Ok(match NEVec::try_from_vec(errors) {
            Some(errors) => Validated::Fail(errors),
            None => Validated::Good(acc),
        })
    }

    /// Marks a VM as deleted in its persisted configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if the updated configuration cannot be serialized or
    /// written.
    #[tracing::instrument(skip(self, spec), fields(vm_id = %spec.id), err)]
    pub fn delete(&self, spec: &VmSpec) -> Result<()> {
        let config = spec.artifact_dir.join("config.json");
        let mut spec = spec.clone();
        spec.deleted = true;
        PersistedVmSpec::new(spec).write_into(&config)
    }
}

impl PersistedVmSpec {
    #[must_use]
    pub fn new(spec: VmSpec) -> Self {
        Self {
            version: CONFIG_VERSION,
            spec,
        }
    }

    /// Writes this specification to `path` as formatted JSON.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization or writing fails.
    pub fn write_into(&self, path: &Path) -> Result<()> {
        let contents =
            serde_json::to_vec_pretty(self).map_err(|error| StorageError::InvalidConfig {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?;
        std::fs::write(path, contents).map_err(|error| StorageError::io(path, error))?;
        Ok(())
    }
}

impl SshAccessError {
    fn operation(operation: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Operation {
            operation,
            path: path.into(),
            source,
        }
    }
}

#[tracing::instrument(err)]
fn install_authorized_keys(rootfs: &Path, authorized_keys: &[u8]) -> Result<(), SshAccessError> {
    tracing::info!("installing authorized keys");
    let ssh_directory = rootfs.join("root/.ssh");
    create_dir_all(&ssh_directory).map_err(|error| {
        SshAccessError::operation("create root SSH directory", &ssh_directory, error)
    })?;
    set_permissions(&ssh_directory, std::fs::Permissions::from_mode(0o700)).map_err(|error| {
        SshAccessError::operation("set root SSH directory permissions", &ssh_directory, error)
    })?;

    let destination = ssh_directory.join("authorized_keys");
    std::fs::write(&destination, authorized_keys).map_err(|error| {
        SshAccessError::operation("install authorized keys", &destination, error)
    })?;
    set_permissions(&destination, std::fs::Permissions::from_mode(0o600)).map_err(|error| {
        SshAccessError::operation("set authorized keys permissions", &destination, error)
    })?;
    tracing::info!("installed authorized keys");
    Ok(())
}

#[tracing::instrument(skip(authorized_keys), err)]
fn install_authorized_keys_into_rootfs(
    rootfs: &Path,
    authorized_keys: &[u8],
) -> Result<(), SshAccessError> {
    let mounted = MountedRootfs::new(rootfs)?;
    let setup = install_authorized_keys(mounted.path(), authorized_keys);
    let cleanup = mounted.finish();

    match (setup, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(setup), Ok(())) => Err(setup),
        (Ok(()), Err(cleanup)) => Err(cleanup.into()),
        (Err(setup), Err(cleanup)) => Err(SshAccessError::SetupCleanup {
            setup: Box::new(setup),
            cleanup,
        }),
    }
}

#[derive(Debug)]
struct MountedRootfs {
    loop_device: loopdev::LoopDevice,
    loop_path: PathBuf,
    mountpoint: tempfile::TempDir,
    attached: bool,
    mounted: bool,
}

impl MountedRootfs {
    #[tracing::instrument(err)]
    fn new(rootfs: &Path) -> Result<Self, SshAccessError> {
        tracing::info!("attaching rootfs loop device");
        let parent = rootfs.parent().unwrap_or_else(|| Path::new("."));
        let mountpoint = tempfile::Builder::new()
            .prefix(".rootfs-mount-")
            .tempdir_in(parent)
            .map_err(|error| {
                SshAccessError::operation("create rootfs mountpoint", parent, error)
            })?;
        let loop_control = loopdev::LoopControl::open().map_err(|error| {
            SshAccessError::operation("open loop control device", "/dev/loop-control", error)
        })?;
        let loop_device = loop_control.next_free().map_err(|error| {
            SshAccessError::operation("allocate loop device", "/dev/loop-control", error)
        })?;
        let loop_path = loop_device.path().ok_or_else(|| {
            SshAccessError::operation(
                "resolve loop device",
                "/proc/self/fd",
                io::Error::new(io::ErrorKind::NotFound, "loop device path is unavailable"),
            )
        })?;
        loop_device
            .with()
            .autoclear(true)
            .attach(rootfs)
            .map_err(|error| SshAccessError::operation("attach rootfs", rootfs, error))?;

        let mut mounted = Self {
            loop_device,
            loop_path,
            mountpoint,
            attached: true,
            mounted: false,
        };
        let flags = mount::MountFlags::NODEV
            | mount::MountFlags::NOEXEC
            | mount::MountFlags::NOSUID
            | mount::MountFlags::NOATIME
            | mount::MountFlags::NOSYMFOLLOW;
        if let Err(error) = mount::mount(
            &mounted.loop_path,
            mounted.mountpoint.path(),
            "ext4",
            flags,
            None::<&std::ffi::CStr>,
        ) {
            tracing::error!(?error, "failed to mount rootfs");
            let setup =
                SshAccessError::operation("mount rootfs", mounted.mountpoint.path(), error.into());
            return Err(match mounted.cleanup() {
                Ok(()) => setup,
                Err(cleanup) => SshAccessError::SetupCleanup {
                    setup: Box::new(setup),
                    cleanup,
                },
            });
        }
        mounted.mounted = true;
        tracing::info!("rootfs mounted");
        Ok(mounted)
    }

    #[must_use]
    fn path(&self) -> &Path {
        self.mountpoint.path()
    }

    fn finish(mut self) -> Result<(), RootfsCleanupError> {
        self.cleanup()
    }

    #[tracing::instrument(skip(self), err)]
    fn cleanup(&mut self) -> Result<(), RootfsCleanupError> {
        tracing::info!("cleaning up rootfs mount");
        let unmount = if self.mounted {
            match mount::unmount(self.mountpoint.path(), mount::UnmountFlags::NOFOLLOW) {
                Ok(()) => {
                    self.mounted = false;
                    None
                }
                Err(error) => Some(io::Error::from(error)),
            }
        } else {
            None
        };
        let detach = if self.attached {
            match self.loop_device.detach() {
                Ok(()) => {
                    self.attached = false;
                    None
                }
                Err(error) => Some(error),
            }
        } else {
            None
        };

        match (unmount, detach) {
            (None, None) => Ok(()),
            (Some(source), None) => Err(RootfsCleanupError::Unmount {
                mountpoint: self.mountpoint.path().to_owned(),
                source,
            }),
            (None, Some(source)) => Err(RootfsCleanupError::Detach {
                loop_device: self.loop_path.clone(),
                source,
            }),
            (Some(unmount), Some(detach)) => Err(RootfsCleanupError::UnmountAndDetach {
                mountpoint: self.mountpoint.path().to_owned(),
                unmount,
                loop_device: self.loop_path.clone(),
                detach,
            }),
        }
    }
}

impl Drop for MountedRootfs {
    fn drop(&mut self) {
        if let Err(err) = self.cleanup() {
            tracing::error!(?err, "failed to drop mounted rootfs");
        }
    }
}

#[derive(Debug)]
pub struct PathDescriptor {
    directory: PathBuf,
    config: PathBuf,
}

impl PathDescriptor {
    fn config(&self) -> Result<Vec<u8>> {
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

#[derive(Debug)]
pub struct VmRootFolderItem {
    pub name: String,
    pub directory: PathBuf,
    pub config: PathBuf,
    pub persisted_spec: PersistedVmSpec,
}

impl VmRootFolderItem {
    fn new(desc: PathDescriptor, name: String, persisted_spec: PersistedVmSpec) -> Result<Self> {
        let directory = &desc.directory;
        let config = &desc.config;
        let spec = &persisted_spec.spec;
        let expected_name = spec.id.to_string();
        if directory.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str()) {
            return Err(StorageError::InvalidConfig {
                path: directory.to_owned(),
                message: "directory name does not match configured VM ID".to_owned(),
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
                    path: desc.config.clone(),
                    message: format!(
                        "configured path {} does not match {}",
                        actual.display(),
                        expected.display()
                    ),
                });
            }
        }
        let expected_api_socket = ApiSocket::from(
            directory
                .parent()
                .expect("a VM artifact directory always has a parent")
                .join(SOCKET_DIRECTORY)
                .join(format!("firecracker-{}.socket", spec.id)),
        );
        if spec.api_socket != expected_api_socket {
            return Err(StorageError::InvalidConfig {
                path: desc.config.clone(),
                message: "configured API socket does not match VM ID".to_owned(),
            });
        }
        let network = NetworkSpec::new(spec.id).map_err(|error| StorageError::InvalidConfig {
            path: config.clone(),
            message: error.to_string(),
        })?;
        if spec.network != network {
            return Err(StorageError::InvalidConfig {
                path: desc.config.clone(),
                message: "configured network does not match VM ID".to_owned(),
            });
        }
        Ok(VmRootFolderItem {
            directory: desc.directory,
            config: desc.config,
            name,
            persisted_spec,
        })
    }
}

#[derive(Debug)]
pub struct VmRootFolder {
    pub(crate) dir: PathBuf,
    paths: std::vec::IntoIter<PathDescriptor>,
}

impl VmRootFolder {
    pub fn new(vm_root: impl AsRef<Path>) -> Result<Self> {
        let vm_root = vm_root.as_ref();
        let directory = read_dir(vm_root).map_err(|error| StorageError::io(vm_root, error))?;
        let mut paths = Vec::new();
        for entry in directory {
            let entry = entry.map_err(|error| StorageError::io(vm_root, error))?;
            let directory = entry.path();
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
        let len = VmRootFolder::new(&self.dir)?.try_fold(0_u16, |len, item| {
            item?;
            len.checked_add(1).ok_or(StorageError::IdsExhausted)
        })?;
        VmId::try_from(len).map_err(|_| StorageError::IdsExhausted)
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

            return Ok(Some(VmRootFolderItem::new(descriptor, name, spec)?));
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

#[cfg(test)]
mod tests {
    use std::{env, fs, os::unix::fs::PermissionsExt};

    use super::install_authorized_keys;

    use super::{MountedRootfs, VmStore};
    use crate::{AuthorizedKey, MemoryMib, VcpuCount, VmInput};

    #[test]
    fn authorized_keys_are_installed_for_root_with_strict_permissions() {
        let rootfs = tempfile::tempdir().expect("failed to create fake rootfs");

        install_authorized_keys(rootfs.path(), b"ssh-ed25519 test-key root@example.com\n")
            .expect("failed to install authorized keys");

        assert_eq!(
            fs::read(rootfs.path().join("root/.ssh/authorized_keys"))
                .expect("failed to read installed authorized keys"),
            b"ssh-ed25519 test-key root@example.com\n"
        );
        assert_eq!(
            fs::metadata(rootfs.path().join("root/.ssh"))
                .expect("failed to read .ssh metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(rootfs.path().join("root/.ssh/authorized_keys"))
                .expect("failed to read authorized_keys metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[tokio::test]
    #[ignore = "requires root, loop devices, and BARBIROLLI_TEST_ROOTFS"]
    async fn vm_store_writes_explicit_authorized_keys_directly_into_rootfs() {
        let source_rootfs =
            env::var_os("BARBIROLLI_TEST_ROOTFS").expect("BARBIROLLI_TEST_ROOTFS is required");
        let temporary_root =
            env::var_os("BARBIROLLI_TEST_TMPDIR").expect("BARBIROLLI_TEST_TMPDIR is required");
        let temporary =
            tempfile::tempdir_in(temporary_root).expect("failed to create temporary VM directory");
        let vm_root = temporary.path().join("vms");
        let image_root = temporary.path().join("images");
        fs::create_dir(&image_root).expect("failed to create image directory");
        fs::write(image_root.join("vmlinux"), b"kernel").expect("failed to create kernel");
        fs::copy(source_rootfs, image_root.join("alpine.ext4"))
            .expect("failed to copy source rootfs");

        let store = VmStore::new(vm_root, image_root).expect("failed to create VM store");
        let spec = store
            .create(
                VmInput {
                    authorized_keys: vec![
                        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti root@example.com"
                            .parse::<AuthorizedKey>()
                            .expect("valid authorized key"),
                    ],
                    bindings: Vec::new(),
                    vcpu_count: VcpuCount::try_from(1).expect("valid vCPU count"),
                },
                MemoryMib::from(128),
            )
            .await
            .expect("failed to create VM");

        assert!(
            !spec.artifact_dir.join("authorized_keys").exists(),
            "authorized keys must not be persisted beside the VM artifacts"
        );

        let mounted = MountedRootfs::new(&spec.rootfs).expect("failed to remount rootfs");
        assert_eq!(
            fs::read(mounted.path().join("root/.ssh/authorized_keys"))
                .expect("failed to read authorized keys from rootfs"),
            b"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILM+rvN+ot98qgEN796jTiQfZfG1KaT0PtFDJ/XFSqti root@example.com\n"
        );
        mounted.finish().expect("failed to unmount rootfs");
    }
}
