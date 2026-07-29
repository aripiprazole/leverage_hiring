use super::{Result, StorageError};

use std::{
    collections::HashSet,
    fs::{
        canonicalize, copy, create_dir, create_dir_all, read_dir, remove_dir_all, rename,
        set_permissions, symlink_metadata,
    },
    io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{ArtifactName, AuthorizedKey, NetworkSpec, VmId, VmInput, VmSpec};

const CONFIG_VERSION: u16 = 1;
const SOCKET_DIRECTORY: &str = ".sockets";

#[cfg(target_os = "linux")]
use rustix::mount;

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

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct MountedRootfs {
    loop_device: loopdev::LoopDevice,
    loop_path: PathBuf,
    mountpoint: tempfile::TempDir,
    attached: bool,
    mounted: bool,
}

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
    #[tracing::instrument(
        skip_all,
        fields(
            vm_root = %vm_root.display(),
            image_root = %image_root.display()
        ),
        err
    )]
    pub fn new(
        vm_root: PathBuf,
        image_root: PathBuf,
        default_authorized_keys: PathBuf,
    ) -> Result<Self> {
        tracing::info!("creating new vm store");
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

    #[tracing::instrument(
        skip(self, input),
        fields(user = %input.user),
        err
    )]
    pub async fn create(&self, input: VmInput) -> Result<VmSpec, StorageError> {
        tracing::info!("creating info");
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

    #[tracing::instrument(
        skip(self, spec),
        fields(vm_id = %spec.id, user = %spec.user),
        err
    )]
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

    #[cfg(target_os = "linux")]
    #[tracing::instrument(
        skip(self),
        fields(vm_id = %self.spec.id, user = %self.spec.user, rootfs = %self.spec.rootfs.display()),
        err
    )]
    pub fn setup_ssh_access(&self) -> std::result::Result<(), SshAccessError> {
        let mounted = MountedRootfs::open(&self.spec.rootfs)?;
        let setup = install_authorized_keys(
            mounted.path(),
            &self.spec.artifact_dir.join("authorized_keys"),
        );
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
fn install_authorized_keys(rootfs: &Path, authorized_keys: &Path) -> Result<(), SshAccessError> {
    tracing::info!("installing authorized keys");
    let ssh_directory = rootfs.join("root/.ssh");
    create_dir_all(&ssh_directory).map_err(|error| {
        SshAccessError::operation("create root SSH directory", &ssh_directory, error)
    })?;
    set_permissions(&ssh_directory, std::fs::Permissions::from_mode(0o700)).map_err(|error| {
        SshAccessError::operation("set root SSH directory permissions", &ssh_directory, error)
    })?;

    let destination = ssh_directory.join("authorized_keys");
    copy(authorized_keys, &destination).map_err(|error| {
        SshAccessError::operation("install authorized keys", &destination, error)
    })?;
    set_permissions(&destination, std::fs::Permissions::from_mode(0o600)).map_err(|error| {
        SshAccessError::operation("set authorized keys permissions", &destination, error)
    })?;
    tracing::info!("installed authorized keys");
    Ok(())
}

#[cfg(target_os = "linux")]
impl MountedRootfs {
    #[tracing::instrument(err)]
    fn open(rootfs: &Path) -> Result<Self, SshAccessError> {
        tracing::info!("opening rootfs");
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
            tracing::error!(?error, "failed to mount");
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
        tracing::info!("mounted");
        Ok(mounted)
    }

    fn path(&self) -> &Path {
        self.mountpoint.path()
    }

    fn finish(mut self) -> Result<(), RootfsCleanupError> {
        self.cleanup()
    }

    #[tracing::instrument(skip(self), err)]
    fn cleanup(&mut self) -> Result<(), RootfsCleanupError> {
        tracing::info!("cleanup");
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

#[cfg(target_os = "linux")]
impl Drop for MountedRootfs {
    fn drop(&mut self) {
        let _ = self.cleanup();
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
    fn new(
        descriptor: PathDescriptor,
        name: String,
        persisted_spec: PersistedVmSpec,
    ) -> Result<Self> {
        let directory = &descriptor.directory;
        let config = &descriptor.config;
        let spec = &persisted_spec.spec;
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
                    path: descriptor.config.clone(),
                    message: format!(
                        "configured path {} does not match {}",
                        actual.display(),
                        expected.display()
                    ),
                });
            }
        }
        let network = NetworkSpec::new(spec.id).map_err(|error| StorageError::InvalidConfig {
            path: config.clone(),
            message: error.to_string(),
        })?;
        if spec.network != network {
            return Err(StorageError::InvalidConfig {
                path: descriptor.config.clone(),
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
        Ok(VmRootFolderItem {
            directory: descriptor.directory,
            config: descriptor.config,
            name,
            persisted_spec,
        })
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
    #[cfg(target_os = "linux")]
    use super::{MountedRootfs, PersistedVmSpec};
    #[cfg(target_os = "linux")]
    use crate::{MemoryMib, NetworkSpec, UserName, VcpuCount, VmId, VmSpec};

    #[test]
    fn authorized_keys_are_installed_for_root_with_strict_permissions() {
        let rootfs = tempfile::tempdir().expect("failed to create fake rootfs");
        let authorized_keys = rootfs.path().parent().unwrap().join("authorized_keys");
        fs::write(&authorized_keys, b"ssh-ed25519 test-key root@example.com\n")
            .expect("failed to create authorized keys");

        install_authorized_keys(rootfs.path(), &authorized_keys)
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

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires root, loop devices, and BARBIROLLI_TEST_ROOTFS"]
    fn persisted_vm_spec_mounts_rootfs_and_installs_authorized_keys() {
        let source_rootfs =
            env::var_os("BARBIROLLI_TEST_ROOTFS").expect("BARBIROLLI_TEST_ROOTFS is required");
        let temporary_root =
            env::var_os("BARBIROLLI_TEST_TMPDIR").expect("BARBIROLLI_TEST_TMPDIR is required");
        let temporary =
            tempfile::tempdir_in(temporary_root).expect("failed to create temporary VM directory");
        let artifact_dir = temporary.path().join("vm");
        fs::create_dir(&artifact_dir).expect("failed to create VM artifact directory");
        let rootfs = artifact_dir.join("rootfs.ext4");
        fs::copy(source_rootfs, &rootfs).expect("failed to copy rootfs");
        fs::write(
            artifact_dir.join("authorized_keys"),
            b"ssh-ed25519 test-key root@example.com\n",
        )
        .expect("failed to create authorized keys");

        let persisted = PersistedVmSpec::new(VmSpec {
            id: VmId::try_from(1).expect("valid VM ID"),
            user: "root-vm".parse::<UserName>().expect("valid user"),
            artifact_dir: artifact_dir.clone(),
            kernel: artifact_dir.join("vmlinux"),
            rootfs: rootfs.clone(),
            vcpu_count: VcpuCount::try_from(1).expect("valid vCPU count"),
            memory_mib: MemoryMib::try_from(128).expect("valid memory"),
            network: NetworkSpec::new(VmId::try_from(1).expect("valid VM ID"))
                .expect("valid network"),
        });

        persisted
            .setup_ssh_access()
            .expect("failed to set up SSH access");

        let mounted = MountedRootfs::open(&rootfs).expect("failed to remount rootfs");
        assert_eq!(
            fs::read(mounted.path().join("root/.ssh/authorized_keys"))
                .expect("failed to read authorized keys from rootfs"),
            b"ssh-ed25519 test-key root@example.com\n"
        );
        mounted.finish().expect("failed to unmount rootfs");
    }
}
