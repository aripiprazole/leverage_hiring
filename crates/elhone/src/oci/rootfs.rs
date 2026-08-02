use std::{
    collections::HashSet,
    ffi::OsString,
    fs::{self, File},
    io::{self, BufReader, BufWriter, Read, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::MetadataExt,
    },
    path::{Component, Path, PathBuf},
    process::{Command, ExitStatus},
    sync::{Arc, Mutex},
};

use flate2::read::MultiGzDecoder;
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use tokio::task::spawn_blocking;

use super::{digest, media};

const MIB: u64 = 1024 * 1024;
const MINIMUM_CAPACITY: u64 = 256 * MIB;
const MINIMUM_DATA_HEADROOM: u64 = 64 * MIB;
const CAPACITY_ALIGNMENT: u64 = 64 * MIB;
const BYTES_PER_INODE: u64 = 16 * 1024;
const MINIMUM_INODES: u64 = 8192;

#[derive(Clone)]
pub struct ArtifactStore {
    retained: Arc<Mutex<Vec<TempDir>>>,
    builder: FilesystemBuilder,
    preserve_ownerships: bool,
}

impl Default for ArtifactStore {
    fn default() -> Self {
        Self {
            retained: Arc::new(Mutex::new(Vec::new())),
            builder: FilesystemBuilder::Ext4,
            preserve_ownerships: true,
        }
    }
}

impl ArtifactStore {
    #[tracing::instrument(
        skip(self, workspace, index, media_kind, expected_diff_id),
        fields(
            root = %workspace.root.display(),
            layer_index = index,
            media_kind = %media_kind.as_ref(),
            layer = %expected_diff_id
        ),
        err
    )]
    pub async fn apply_layer(
        &self,
        workspace: &Workspace,
        index: usize,
        media_kind: &media::LayerMediaKind,
        expected_diff_id: digest::Sha256Digest,
    ) -> Result<AppliedLayer, Error> {
        let root = workspace.root.clone();
        let blob = workspace.layer_blob(index);
        let archive = workspace.layer_archive(index);
        let media_kind = media_kind.as_ref().to_owned();
        let preserve_ownerships = self.preserve_ownerships;
        let layer = expected_diff_id.to_string();

        spawn_blocking(move || {
            archive.apply_layer_sync(
                &root,
                &blob,
                &media_kind,
                expected_diff_id,
                preserve_ownerships,
            )
        })
        .await
        .map_err(|source| Error::Task {
            operation: format!("apply OCI layer {layer}"),
            source,
        })?
    }

    pub async fn finish(&self, workspace: Workspace) -> Result<Filesystem, Error> {
        let root = workspace.root.clone();
        let path = workspace.filesystem();
        let builder = self.builder;
        let result_path = path.clone();

        let built = spawn_blocking(move || {
            let built = builder.build(&root, &path)?;
            fs::remove_dir_all(&root)
                .map_err(|source| Error::io("remove merged rootfs", &root, source))?;
            Ok::<_, Error>(built)
        })
        .await
        .map_err(|source| Error::Task {
            operation: "build ext4 filesystem".to_owned(),
            source,
        })??;

        self.retained
            .lock()
            .map_err(|_| Error::RetentionLock)?
            .push(workspace.temp);

        Ok(Filesystem {
            path: result_path,
            size: built.size,
            digest: built.digest,
        })
    }

    #[cfg(test)]
    pub fn retained_count(&self) -> usize {
        self.retained
            .lock()
            .expect("test artifact retention lock should not be poisoned")
            .len()
    }

    #[cfg(test)]
    pub fn for_test(fail: bool) -> Self {
        Self {
            retained: Arc::new(Mutex::new(Vec::new())),
            builder: if fail {
                FilesystemBuilder::TestFailure
            } else {
                FilesystemBuilder::Test
            },
            preserve_ownerships: false,
        }
    }
}

pub struct Workspace {
    temp: TempDir,
    root: PathBuf,
}

impl Workspace {
    pub fn new() -> Result<Self, Error> {
        let temp = tempfile::Builder::new()
            .prefix("elhone-oci-")
            .tempdir()
            .map_err(|source| {
                Error::io(
                    "create OCI artifact workspace",
                    std::env::temp_dir(),
                    source,
                )
            })?;
        let root = temp.path().join("merged-rootfs");
        fs::create_dir(&root).map_err(|source| Error::io("create merged rootfs", &root, source))?;
        Ok(Self { temp, root })
    }

    pub fn layer_blob(&self, index: usize) -> PathBuf {
        self.temp.path().join(format!("layer-{index}.blob"))
    }

    fn layer_archive(&self, index: usize) -> Archive {
        Archive(self.temp.path().join(format!("layer-{index}.tar")))
    }

    fn filesystem(&self) -> PathBuf {
        self.temp.path().join("rootfs.ext4")
    }

    #[cfg(test)]
    fn root(&self) -> &Path {
        &self.root
    }
}

#[derive(Debug)]
struct Archive(PathBuf);

impl AsRef<Path> for Archive {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

#[derive(Debug)]
pub struct AppliedLayer {
    pub diff_id: digest::Sha256Digest,
    pub uncompressed_size: u64,
}

#[derive(Debug)]
pub struct Filesystem {
    pub path: PathBuf,
    pub size: u64,
    pub digest: digest::Sha256Digest,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid OCI layer {layer}: {message}")]
    InvalidLayer { layer: String, message: String },
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("mkfs.ext4 exited with {status}: {stderr}")]
    Mkfs { status: ExitStatus, stderr: String },
    #[error("{operation} task failed: {source}")]
    Task {
        operation: String,
        #[source]
        source: tokio::task::JoinError,
    },
    #[error("OCI artifact retention lock was poisoned")]
    RetentionLock,
    #[cfg(test)]
    #[error("test filesystem builder failed")]
    TestBuilder,
}

impl Error {
    pub fn is_local(&self) -> bool {
        !matches!(self, Self::InvalidLayer { .. })
    }

    pub fn io(operation: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }

    fn invalid(layer: &digest::Sha256Digest, message: impl Into<String>) -> Self {
        Self::InvalidLayer {
            layer: layer.to_string(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum FilesystemBuilder {
    Ext4,
    #[cfg(test)]
    Test,
    #[cfg(test)]
    TestFailure,
}

impl FilesystemBuilder {
    fn build(self, root: &Path, output: &Path) -> Result<BuiltFilesystem, Error> {
        match self {
            Self::Ext4 => build_ext4(root, output),
            #[cfg(test)]
            Self::Test => build_test_filesystem(root, output),
            #[cfg(test)]
            Self::TestFailure => Err(Error::TestBuilder),
        }
    }
}

#[derive(Debug)]
struct BuiltFilesystem {
    size: u64,
    digest: digest::Sha256Digest,
}

#[derive(Debug, Clone, Copy)]
enum Compression {
    Tar,
    Gzip,
    Zstd,
}

impl Compression {
    fn from_media_type(media_type: &str, layer: &digest::Sha256Digest) -> Result<Self, Error> {
        match media_type {
            "application/vnd.oci.image.layer.v1.tar" => Ok(Self::Tar),
            "application/vnd.oci.image.layer.v1.tar+gzip"
            | "application/vnd.docker.image.rootfs.diff.tar.gzip" => Ok(Self::Gzip),
            "application/vnd.oci.image.layer.v1.tar+zstd" => Ok(Self::Zstd),
            value => Err(Error::invalid(
                layer,
                format!("unsupported layer media type {value:?}"),
            )),
        }
    }
}

#[derive(Debug)]
enum Whiteout {
    Remove(PathBuf),
    Opaque(PathBuf),
}

#[derive(Debug)]
struct ArchivePlan {
    entries: Vec<ArchiveEntry>,
    markers: Vec<PathBuf>,
    whiteouts: Vec<Whiteout>,
}

#[derive(Debug)]
struct ArchiveEntry {
    path: PathBuf,
    directory: bool,
}

impl Archive {
    #[tracing::instrument(
        skip(self, expected_diff_id),
        fields(archive = %self.0.display(), layer = %expected_diff_id),
        err
    )]
    fn apply_layer_sync(
        &self,
        root: &Path,
        blob: &Path,
        media_type: &str,
        expected_diff_id: digest::Sha256Digest,
        preserve_ownerships: bool,
    ) -> Result<AppliedLayer, Error> {
        let compression = Compression::from_media_type(media_type, &expected_diff_id)?;
        let (actual_diff_id, uncompressed_size) =
            self.decompress_layer(blob, compression, &expected_diff_id)?;
        if actual_diff_id != expected_diff_id {
            return Err(Error::invalid(
                &expected_diff_id,
                format!("diff ID mismatch: expected {expected_diff_id}, got {actual_diff_id}"),
            ));
        }

        self.apply(root, &expected_diff_id, preserve_ownerships)?;
        fs::remove_file(blob)
            .map_err(|source| Error::io("remove downloaded layer", blob, source))?;
        fs::remove_file(self.as_ref())
            .map_err(|source| Error::io("remove uncompressed layer", self.as_ref(), source))?;

        Ok(AppliedLayer {
            diff_id: actual_diff_id,
            uncompressed_size,
        })
    }

    #[tracing::instrument(
        skip(self),
        fields(archive = %self.0.display()),
        err
    )]
    fn decompress_layer(
        &self,
        blob: &Path,
        compression: Compression,
        layer: &digest::Sha256Digest,
    ) -> Result<(digest::Sha256Digest, u64), Error> {
        let input =
            File::open(blob).map_err(|source| Error::io("open downloaded layer", blob, source))?;
        let output = File::create(self.as_ref())
            .map_err(|source| Error::io("create uncompressed layer", self.as_ref(), source))?;
        let output = BufWriter::new(output);

        match compression {
            Compression::Tar => self.copy_layer(BufReader::new(input), output, layer),
            Compression::Gzip => {
                self.copy_layer(MultiGzDecoder::new(BufReader::new(input)), output, layer)
            }
            Compression::Zstd => {
                let decoder =
                    zstd::stream::read::Decoder::new(BufReader::new(input)).map_err(|source| {
                        Error::invalid(layer, format!("invalid zstd stream: {source}"))
                    })?;
                self.copy_layer(decoder, output, layer)
            }
        }
    }

    #[tracing::instrument(
        skip(self, input, output),
        fields(archive = %self.0.display()),
        err
    )]
    fn copy_layer(
        &self,
        mut input: impl Read,
        mut output: BufWriter<File>,
        layer: &digest::Sha256Digest,
    ) -> Result<(digest::Sha256Digest, u64), Error> {
        let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
        let mut hasher = Sha256::new();
        let mut size = 0_u64;

        loop {
            let count = input.read(&mut buffer).map_err(|source| {
                Error::invalid(layer, format!("decompression failed: {source}"))
            })?;
            if count == 0 {
                break;
            }
            output
                .write_all(&buffer[..count])
                .map_err(|source| Error::io("write uncompressed layer", self.as_ref(), source))?;
            hasher.update(&buffer[..count]);
            size = size
                .checked_add(u64::try_from(count).map_err(|_| {
                    Error::invalid(layer, "uncompressed layer chunk length does not fit in u64")
                })?)
                .ok_or_else(|| Error::invalid(layer, "uncompressed layer size overflowed u64"))?;
        }
        output
            .flush()
            .map_err(|source| Error::io("flush uncompressed layer", self.as_ref(), source))?;

        let digest = digest::format(&hasher.finalize())
            .parse()
            .expect("a computed SHA-256 digest should parse");
        Ok((digest, size))
    }

    #[tracing::instrument(
        skip(self),
        fields(archive = %self.0.display()),
        err
    )]
    fn apply(
        &self,
        root: &Path,
        layer: &digest::Sha256Digest,
        preserve_ownerships: bool,
    ) -> Result<(), Error> {
        let plan = self.inspect(layer)?;
        apply_whiteouts(root, &plan.whiteouts, layer)?;
        prepare_replacements(root, &plan.entries, layer)?;

        let file = File::open(self.as_ref())
            .map_err(|source| Error::io("open uncompressed layer", self.as_ref(), source))?;
        let mut unpacker = tar::Archive::new(BufReader::new(file));
        unpacker.set_overwrite(true);
        unpacker.set_preserve_permissions(true);
        unpacker.set_preserve_ownerships(preserve_ownerships);
        unpacker.set_preserve_mtime(true);
        unpacker.set_unpack_xattrs(true);
        unpacker
            .unpack(root)
            .map_err(|source| Error::invalid(layer, format!("tar extraction failed: {source}")))?;

        for marker in plan.markers {
            ensure_no_symlink_parents(root, &marker, layer)?;
            remove_path(&root.join(marker), "remove extracted whiteout")?;
        }
        Ok(())
    }

    #[tracing::instrument(
        skip(self),
        fields(archive = %self.0.display()),
        err
    )]
    fn inspect(&self, layer: &digest::Sha256Digest) -> Result<ArchivePlan, Error> {
        let file = File::open(self.as_ref())
            .map_err(|source| Error::io("open uncompressed layer", self.as_ref(), source))?;
        let mut reader = tar::Archive::new(BufReader::new(file));
        let entries = reader
            .entries()
            .map_err(|source| Error::invalid(layer, format!("invalid tar archive: {source}")))?;
        let mut seen = HashSet::new();
        let mut planned_entries = Vec::new();
        let mut markers = Vec::new();
        let mut whiteouts = Vec::new();

        for entry in entries {
            let mut entry = entry
                .map_err(|source| Error::invalid(layer, format!("invalid tar entry: {source}")))?;
            let raw_path = entry
                .path()
                .map_err(|source| Error::invalid(layer, format!("invalid tar path: {source}")))?;
            let path = normalize_relative(&raw_path)
                .map_err(|message| Error::invalid(layer, format!("unsafe tar path: {message}")))?;
            if path.as_os_str().is_empty() {
                if !entry.header().entry_type().is_dir() {
                    return Err(Error::invalid(
                        layer,
                        "the archive root entry is not a directory",
                    ));
                }
                continue;
            }
            if !seen.insert(path.clone()) {
                return Err(Error::invalid(
                    layer,
                    format!("duplicate tar entry {}", path.display()),
                ));
            }

            validate_link(&mut entry, layer)?;
            planned_entries.push(ArchiveEntry {
                path: path.clone(),
                directory: entry.header().entry_type().is_dir(),
            });

            let Some(name) = path.file_name() else {
                continue;
            };
            let name = name.as_bytes();
            if name == b".wh..wh..opq" {
                validate_whiteout(&entry, &path, layer)?;
                markers.push(path.clone());
                whiteouts.push(Whiteout::Opaque(
                    path.parent().unwrap_or_else(|| Path::new("")).to_path_buf(),
                ));
            } else if let Some(target) = name.strip_prefix(b".wh.") {
                validate_whiteout(&entry, &path, layer)?;
                if target.is_empty() {
                    return Err(Error::invalid(
                        layer,
                        "whiteout .wh. has no target basename",
                    ));
                }
                if matches!(target, b"." | b"..") {
                    return Err(Error::invalid(
                        layer,
                        format!("whiteout {} has an unsafe target basename", path.display()),
                    ));
                }
                let mut target_path = path.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
                target_path.push(OsString::from_vec(target.to_vec()));
                markers.push(path);
                whiteouts.push(Whiteout::Remove(target_path));
            }
        }

        Ok(ArchivePlan {
            entries: planned_entries,
            markers,
            whiteouts,
        })
    }
}

fn prepare_replacements(
    root: &Path,
    entries: &[ArchiveEntry],
    layer: &digest::Sha256Digest,
) -> Result<(), Error> {
    let mut directories = entries
        .iter()
        .filter(|entry| entry.directory)
        .collect::<Vec<_>>();
    directories.sort_by_key(|entry| entry.path.components().count());
    for entry in directories {
        ensure_no_symlink_parents(root, &entry.path, layer)?;
        let target = root.join(&entry.path);
        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                remove_path(&target, "replace layer target with directory")?;
                fs::create_dir_all(&target).map_err(|source| {
                    Error::io("create replacement layer directory", &target, source)
                })?;
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                fs::create_dir_all(&target).map_err(|source| {
                    Error::io("create replacement layer directory", &target, source)
                })?;
            }
            Err(source) => return Err(Error::io("inspect layer directory", &target, source)),
        }
    }

    for entry in entries.iter().filter(|entry| !entry.directory) {
        ensure_no_symlink_parents(root, &entry.path, layer)?;
        remove_path(&root.join(&entry.path), "replace layer target")?;
    }
    Ok(())
}

fn validate_link<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    layer: &digest::Sha256Digest,
) -> Result<(), Error> {
    let entry_type = entry.header().entry_type();
    if !entry_type.is_hard_link() && !entry_type.is_symlink() {
        return Ok(());
    }
    let target = entry
        .link_name()
        .map_err(|source| Error::invalid(layer, format!("invalid link target: {source}")))?
        .ok_or_else(|| Error::invalid(layer, "link entry has no target"))?;
    if entry_type.is_hard_link() {
        normalize_relative(&target).map_err(|message| {
            Error::invalid(layer, format!("unsafe hardlink target: {message}"))
        })?;
    }
    Ok(())
}

fn validate_whiteout<R: Read>(
    entry: &tar::Entry<'_, R>,
    path: &Path,
    layer: &digest::Sha256Digest,
) -> Result<(), Error> {
    let size = entry
        .header()
        .size()
        .map_err(|source| Error::invalid(layer, format!("invalid whiteout size: {source}")))?;
    if !entry.header().entry_type().is_file() || size != 0 {
        return Err(Error::invalid(
            layer,
            format!("whiteout {} must be an empty regular file", path.display()),
        ));
    }
    Ok(())
}

fn normalize_relative(path: &Path) -> Result<PathBuf, String> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => normalized.push(value),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(format!("{} contains a parent component", path.display()));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!("{} is absolute", path.display()));
            }
        }
    }
    Ok(normalized)
}

fn apply_whiteouts(
    root: &Path,
    whiteouts: &[Whiteout],
    layer: &digest::Sha256Digest,
) -> Result<(), Error> {
    for whiteout in whiteouts {
        let Whiteout::Opaque(directory) = whiteout else {
            continue;
        };
        ensure_no_symlink_parents(root, directory, layer)?;
        let directory = root.join(directory);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                remove_path(&directory, "replace opaque whiteout target")?;
                fs::create_dir_all(&directory).map_err(|source| {
                    Error::io("create opaque whiteout directory", &directory, source)
                })?;
            }
            Ok(_) => {
                for entry in fs::read_dir(&directory).map_err(|source| {
                    Error::io("read opaque whiteout directory", &directory, source)
                })? {
                    let entry = entry.map_err(|source| {
                        Error::io("read opaque whiteout entry", &directory, source)
                    })?;
                    remove_path(&entry.path(), "apply opaque whiteout")?;
                }
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                fs::create_dir_all(&directory).map_err(|source| {
                    Error::io("create opaque whiteout directory", &directory, source)
                })?;
            }
            Err(source) => {
                return Err(Error::io(
                    "inspect opaque whiteout directory",
                    &directory,
                    source,
                ));
            }
        }
    }
    for whiteout in whiteouts {
        if let Whiteout::Remove(path) = whiteout {
            ensure_no_symlink_parents(root, path, layer)?;
            remove_path(&root.join(path), "apply whiteout")?;
        }
    }
    Ok(())
}

fn ensure_no_symlink_parents(
    root: &Path,
    path: &Path,
    layer: &digest::Sha256Digest,
) -> Result<(), Error> {
    let mut current = root.to_path_buf();
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    for component in parent.components() {
        let Component::Normal(component) = component else {
            continue;
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(Error::invalid(
                    layer,
                    format!("path {} traverses a symlink", path.display()),
                ));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(Error::invalid(
                    layer,
                    format!("path {} traverses a non-directory", path.display()),
                ));
            }
            Ok(_) => {}
            Err(source) if source.kind() == io::ErrorKind::NotFound => break,
            Err(source) => return Err(Error::io("inspect layer path", &current, source)),
        }
    }
    Ok(())
}

fn remove_path(path: &Path, operation: &'static str) -> Result<(), Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path).map_err(|source| Error::io(operation, path, source))
        }
        Ok(_) => fs::remove_file(path).map_err(|source| Error::io(operation, path, source)),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::io(operation, path, source)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FilesystemLayout {
    capacity: u64,
    inodes: u64,
}

fn build_ext4(root: &Path, output: &Path) -> Result<BuiltFilesystem, Error> {
    let layout = filesystem_layout(root)?;
    let file = File::create(output)
        .map_err(|source| Error::io("create ext4 filesystem", output, source))?;
    file.set_len(layout.capacity)
        .map_err(|source| Error::io("size ext4 filesystem", output, source))?;
    drop(file);

    let result = Command::new("mkfs.ext4")
        .args(["-F", "-m", "0", "-L", "rootfs", "-N"])
        .arg(layout.inodes.to_string())
        .arg("-d")
        .arg(root)
        .arg(output)
        .output()
        .map_err(|source| Error::io("execute mkfs.ext4", output, source))?;
    if !result.status.success() {
        return Err(Error::Mkfs {
            status: result.status,
            stderr: String::from_utf8_lossy(&result.stderr).trim().to_owned(),
        });
    }

    Ok(BuiltFilesystem {
        size: fs::metadata(output)
            .map_err(|source| Error::io("inspect ext4 filesystem", output, source))?
            .len(),
        digest: hash_file(output)?,
    })
}

fn filesystem_layout(root: &Path) -> Result<FilesystemLayout, Error> {
    let mut seen = HashSet::new();
    let mut bytes = 0_u64;
    let mut inodes = 0_u64;
    measure_tree(root, &mut seen, &mut bytes, &mut inodes)?;

    let inode_headroom = inodes.div_ceil(4);
    let inode_budget = inodes
        .checked_add(inode_headroom)
        .ok_or_else(|| Error::io("calculate inode budget", root, overflow_error()))?
        .max(MINIMUM_INODES);
    let data_headroom = bytes.div_ceil(4).max(MINIMUM_DATA_HEADROOM);
    let data_budget = bytes
        .checked_add(data_headroom)
        .ok_or_else(|| Error::io("calculate data budget", root, overflow_error()))?;
    let inode_capacity = inode_budget
        .checked_mul(BYTES_PER_INODE)
        .ok_or_else(|| Error::io("calculate inode capacity", root, overflow_error()))?;
    let capacity = data_budget.max(inode_capacity).max(MINIMUM_CAPACITY);
    let capacity = round_up(capacity, CAPACITY_ALIGNMENT)
        .ok_or_else(|| Error::io("align ext4 capacity", root, overflow_error()))?;

    Ok(FilesystemLayout {
        capacity,
        inodes: inode_budget,
    })
}

fn measure_tree(
    path: &Path,
    seen: &mut HashSet<(u64, u64)>,
    bytes: &mut u64,
    inodes: &mut u64,
) -> Result<(), Error> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| Error::io("inspect merged rootfs", path, source))?;
    if seen.insert((metadata.dev(), metadata.ino())) {
        *inodes = inodes
            .checked_add(1)
            .ok_or_else(|| Error::io("count rootfs inodes", path, overflow_error()))?;
        if metadata.is_file() {
            *bytes = bytes
                .checked_add(metadata.len())
                .ok_or_else(|| Error::io("count rootfs bytes", path, overflow_error()))?;
        }
    }
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        for entry in
            fs::read_dir(path).map_err(|source| Error::io("read merged rootfs", path, source))?
        {
            let entry =
                entry.map_err(|source| Error::io("read merged rootfs entry", path, source))?;
            measure_tree(&entry.path(), seen, bytes, inodes)?;
        }
    }
    Ok(())
}

fn round_up(value: u64, alignment: u64) -> Option<u64> {
    value
        .checked_add(alignment.checked_sub(1)?)
        .map(|value| value / alignment * alignment)
}

fn overflow_error() -> io::Error {
    io::Error::other("filesystem size calculation overflowed")
}

fn hash_file(path: &Path) -> Result<digest::Sha256Digest, Error> {
    let file = File::open(path)
        .map_err(|source| Error::io("open filesystem for hashing", path, source))?;
    let mut reader = BufReader::new(file);
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    let mut hasher = Sha256::new();
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|source| Error::io("hash ext4 filesystem", path, source))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(digest::format(&hasher.finalize())
        .parse()
        .expect("a computed SHA-256 digest should parse"))
}

#[cfg(test)]
fn build_test_filesystem(root: &Path, output: &Path) -> Result<BuiltFilesystem, Error> {
    let layout = filesystem_layout(root)?;
    let body = format!("test ext4 capacity={}\n", layout.capacity);
    fs::write(output, body.as_bytes())
        .map_err(|source| Error::io("write test filesystem", output, source))?;
    Ok(BuiltFilesystem {
        size: u64::try_from(body.len()).expect("test filesystem length should fit in u64"),
        digest: hash_file(output)?,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::Write,
        os::unix::fs::{MetadataExt as _, PermissionsExt as _},
        path::Path,
    };

    #[cfg(target_os = "linux")]
    use std::process::Command;

    use flate2::{Compression as GzipCompression, write::GzEncoder};
    use tar::{Builder, EntryType, Header};

    use super::{
        Archive, ArtifactStore, Error, FilesystemLayout, MIB, Workspace, filesystem_layout,
    };
    use crate::oci::{digest, media};

    #[derive(Debug, Clone, Copy)]
    enum Encoding {
        Tar,
        Gzip,
        Zstd,
    }

    struct TestLayer {
        body: Vec<u8>,
        diff_id: digest::Sha256Digest,
        media_kind: media::LayerMediaKind,
    }

    impl TestLayer {
        fn new(archive: Vec<u8>, encoding: Encoding) -> Self {
            let diff_id = digest::Sha256Digest::from(archive.as_slice());
            let (body, media_type) = match encoding {
                Encoding::Tar => (archive, "application/vnd.oci.image.layer.v1.tar"),
                Encoding::Gzip => {
                    let mut encoder = GzEncoder::new(Vec::new(), GzipCompression::default());
                    encoder
                        .write_all(&archive)
                        .expect("test gzip encoder should write");
                    (
                        encoder.finish().expect("test gzip encoder should finish"),
                        "application/vnd.oci.image.layer.v1.tar+gzip",
                    )
                }
                Encoding::Zstd => (
                    zstd::stream::encode_all(archive.as_slice(), 0)
                        .expect("test zstd encoder should finish"),
                    "application/vnd.oci.image.layer.v1.tar+zstd",
                ),
            };
            Self {
                body,
                diff_id,
                media_kind: media_type.parse().expect("test media type should parse"),
            }
        }

        fn install(&self, workspace: &Workspace, index: usize) {
            fs::write(workspace.layer_blob(index), &self.body)
                .expect("test layer should be written");
        }
    }

    fn archive(build: impl FnOnce(&mut Builder<Vec<u8>>)) -> Vec<u8> {
        let mut builder = Builder::new(Vec::new());
        build(&mut builder);
        builder.finish().expect("test tar should finish");
        builder
            .into_inner()
            .expect("test tar body should be returned")
    }

    fn header(size: u64, mode: u32, entry_type: EntryType) -> Header {
        let mut header = Header::new_gnu();
        header.set_size(size);
        header.set_mode(mode);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(1);
        header.set_entry_type(entry_type);
        header.set_cksum();
        header
    }

    fn append_file(builder: &mut Builder<Vec<u8>>, path: &str, body: &[u8], mode: u32) {
        let mut header = header(
            u64::try_from(body.len()).expect("test body length should fit in u64"),
            mode,
            EntryType::Regular,
        );
        header.set_path(path).expect("test path should be valid");
        header.set_cksum();
        builder
            .append(&header, body)
            .expect("test file should append");
    }

    fn append_empty_file(builder: &mut Builder<Vec<u8>>, path: &str) {
        append_file(builder, path, &[], 0o000);
    }

    fn append_directory(builder: &mut Builder<Vec<u8>>, path: &str, mode: u32) {
        let mut header = header(0, mode, EntryType::Directory);
        header.set_path(path).expect("test path should be valid");
        header.set_cksum();
        builder
            .append(&header, std::io::empty())
            .expect("test directory should append");
    }

    fn append_link(
        builder: &mut Builder<Vec<u8>>,
        path: &str,
        target: impl AsRef<Path>,
        entry_type: EntryType,
    ) {
        let mut header = header(0, 0o777, entry_type);
        builder
            .append_link(&mut header, path, target)
            .expect("test link should append");
    }

    async fn apply(store: &ArtifactStore, workspace: &Workspace, index: usize, layer: &TestLayer) {
        layer.install(workspace, index);
        let applied = store
            .apply_layer(workspace, index, &layer.media_kind, layer.diff_id.clone())
            .await
            .expect("test layer should apply");
        assert_eq!(applied.diff_id, layer.diff_id);
        assert!(applied.uncompressed_size > 0);
    }

    #[tokio::test]
    async fn layers_merge_whiteouts_links_permissions_and_xattrs() {
        let store = ArtifactStore::for_test(false);
        let workspace = Workspace::new().expect("test workspace should be created");
        let base = TestLayer::new(
            archive(|builder| {
                append_file(builder, "remove", b"lower", 0o644);
                append_file(builder, "opaque/old", b"old", 0o644);
                append_file(builder, "becomes-file/child", b"child", 0o644);
                append_file(builder, "becomes-directory", b"file", 0o644);
                builder
                    .append_pax_extensions([("SCHILY.xattr.user.elhone", b"present".as_slice())])
                    .expect("test xattr should append");
                append_file(builder, "bin/tool", b"tool", 0o755);
                append_link(builder, "bin/link", "tool", EntryType::Symlink);
                append_link(builder, "bin/hard", "bin/tool", EntryType::Link);
            }),
            Encoding::Gzip,
        );
        let upper = TestLayer::new(
            archive(|builder| {
                append_file(builder, "remove", b"upper", 0o600);
                append_file(builder, "opaque/new", b"new", 0o644);
                append_file(builder, "becomes-file", b"now a file", 0o644);
                append_directory(builder, "becomes-directory", 0o755);
                append_file(
                    builder,
                    "becomes-directory/child",
                    b"now a directory",
                    0o644,
                );
                append_empty_file(builder, "remove-marker/.keep");
                append_empty_file(builder, ".wh.remove");
                append_empty_file(builder, "opaque/.wh..wh..opq");
            }),
            Encoding::Zstd,
        );

        apply(&store, &workspace, 0, &base).await;
        apply(&store, &workspace, 1, &upper).await;

        assert_eq!(
            fs::read(workspace.root().join("remove")).expect("replacement should be readable"),
            b"upper"
        );
        assert!(!workspace.root().join("opaque/old").exists());
        assert_eq!(
            fs::read(workspace.root().join("opaque/new")).expect("new file should be readable"),
            b"new"
        );
        assert!(!workspace.root().join(".wh.remove").exists());
        assert!(!workspace.root().join("opaque/.wh..wh..opq").exists());
        assert_eq!(
            fs::read(workspace.root().join("becomes-file"))
                .expect("directory should be replaced by a file"),
            b"now a file"
        );
        assert_eq!(
            fs::read(workspace.root().join("becomes-directory/child"))
                .expect("file should be replaced by a directory"),
            b"now a directory"
        );
        assert_eq!(
            fs::symlink_metadata(workspace.root().join("bin/tool"))
                .expect("tool metadata should exist")
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            fs::symlink_metadata(workspace.root().join("bin/tool"))
                .expect("tool metadata should exist")
                .mtime(),
            1
        );
        assert_eq!(
            fs::read_link(workspace.root().join("bin/link")).expect("symlink should be readable"),
            Path::new("tool")
        );
        let tool =
            fs::metadata(workspace.root().join("bin/tool")).expect("tool metadata should exist");
        let hard = fs::metadata(workspace.root().join("bin/hard"))
            .expect("hardlink metadata should exist");
        assert_eq!(tool.ino(), hard.ino());
        assert_eq!(
            xattr::get(workspace.root().join("bin/tool"), "user.elhone")
                .expect("xattr should be readable")
                .as_deref(),
            Some(b"present".as_slice())
        );
    }

    #[tokio::test]
    async fn uncompressed_layers_are_supported() {
        let store = ArtifactStore::for_test(false);
        let workspace = Workspace::new().expect("test workspace should be created");
        let layer = TestLayer::new(
            archive(|builder| append_file(builder, "etc/plain", b"plain", 0o644)),
            Encoding::Tar,
        );

        apply(&store, &workspace, 0, &layer).await;

        assert_eq!(
            fs::read(workspace.root().join("etc/plain")).expect("plain file should exist"),
            b"plain"
        );
    }

    #[tokio::test]
    async fn docker_gzip_layers_are_supported() {
        let store = ArtifactStore::for_test(false);
        let workspace = Workspace::new().expect("test workspace should be created");
        let mut layer = TestLayer::new(
            archive(|builder| append_file(builder, "etc/docker", b"docker", 0o644)),
            Encoding::Gzip,
        );
        layer.media_kind = "application/vnd.docker.image.rootfs.diff.tar.gzip"
            .parse()
            .expect("Docker gzip media type should parse");

        apply(&store, &workspace, 0, &layer).await;

        assert_eq!(
            fs::read(workspace.root().join("etc/docker")).expect("Docker layer file should exist"),
            b"docker"
        );
    }

    #[tokio::test]
    async fn duplicate_paths_and_invalid_whiteouts_are_rejected() {
        let store = ArtifactStore::for_test(false);
        let duplicate_workspace = Workspace::new().expect("test workspace should be created");
        let duplicate = TestLayer::new(
            archive(|builder| {
                append_file(builder, "same", b"first", 0o644);
                append_file(builder, "same", b"second", 0o644);
            }),
            Encoding::Gzip,
        );
        duplicate.install(&duplicate_workspace, 0);

        let duplicate_error = store
            .apply_layer(
                &duplicate_workspace,
                0,
                &duplicate.media_kind,
                duplicate.diff_id,
            )
            .await
            .expect_err("duplicate paths should fail");
        assert!(matches!(duplicate_error, Error::InvalidLayer { .. }));
        assert!(duplicate_error.to_string().contains("duplicate tar entry"));

        let whiteout_workspace = Workspace::new().expect("test workspace should be created");
        let whiteout = TestLayer::new(
            archive(|builder| append_file(builder, ".wh.target", b"not empty", 0o644)),
            Encoding::Gzip,
        );
        whiteout.install(&whiteout_workspace, 0);
        let whiteout_error = store
            .apply_layer(
                &whiteout_workspace,
                0,
                &whiteout.media_kind,
                whiteout.diff_id,
            )
            .await
            .expect_err("invalid whiteout should fail");
        assert!(whiteout_error.to_string().contains("empty regular file"));

        for marker in [".wh..", ".wh..."] {
            let workspace = Workspace::new().expect("test workspace should be created");
            let layer = TestLayer::new(
                archive(|builder| append_empty_file(builder, marker)),
                Encoding::Gzip,
            );
            layer.install(&workspace, 0);

            let error = store
                .apply_layer(&workspace, 0, &layer.media_kind, layer.diff_id)
                .await
                .expect_err("whiteout dot targets should fail");
            assert!(error.to_string().contains("unsafe target basename"));
        }
    }

    #[tokio::test]
    async fn traversal_and_symlink_parent_escapes_are_rejected() {
        let store = ArtifactStore::for_test(false);
        let traversal_workspace = Workspace::new().expect("test workspace should be created");
        let traversal = TestLayer::new(
            archive(|builder| {
                let body = b"escape";
                let mut header = header(
                    u64::try_from(body.len()).expect("test body should fit in u64"),
                    0o644,
                    EntryType::Regular,
                );
                let raw = header.as_old_mut();
                raw.name[..9].copy_from_slice(b"../escape");
                header.set_cksum();
                builder
                    .append(&header, body.as_slice())
                    .expect("unsafe test entry should append");
            }),
            Encoding::Tar,
        );
        traversal.install(&traversal_workspace, 0);
        let traversal_error = store
            .apply_layer(
                &traversal_workspace,
                0,
                &traversal.media_kind,
                traversal.diff_id,
            )
            .await
            .expect_err("parent traversal should fail");
        assert!(traversal_error.to_string().contains("parent component"));

        let outside = tempfile::tempdir().expect("outside directory should be created");
        let symlink_workspace = Workspace::new().expect("test workspace should be created");
        let base = TestLayer::new(
            archive(|builder| {
                append_link(builder, "escape", outside.path(), EntryType::Symlink);
            }),
            Encoding::Tar,
        );
        apply(&store, &symlink_workspace, 0, &base).await;
        let upper = TestLayer::new(
            archive(|builder| append_file(builder, "escape/pwned", b"no", 0o644)),
            Encoding::Tar,
        );
        upper.install(&symlink_workspace, 1);
        let symlink_error = store
            .apply_layer(&symlink_workspace, 1, &upper.media_kind, upper.diff_id)
            .await
            .expect_err("symlink parent should fail");
        assert!(symlink_error.to_string().contains("traverses a symlink"));
        assert!(!outside.path().join("pwned").exists());

        let same_layer_workspace = Workspace::new().expect("test workspace should be created");
        let same_layer = TestLayer::new(
            archive(|builder| {
                append_link(builder, "same-escape", outside.path(), EntryType::Symlink);
                append_file(builder, "same-escape/pwned", b"no", 0o644);
            }),
            Encoding::Tar,
        );
        same_layer.install(&same_layer_workspace, 0);
        let same_layer_error = store
            .apply_layer(
                &same_layer_workspace,
                0,
                &same_layer.media_kind,
                same_layer.diff_id,
            )
            .await
            .expect_err("same-layer symlink traversal should fail");
        assert!(
            same_layer_error
                .to_string()
                .contains("outside of destination")
        );
        assert!(!outside.path().join("pwned").exists());
    }

    #[tokio::test]
    async fn malformed_compression_and_tar_are_rejected_as_invalid_layers() {
        let store = ArtifactStore::for_test(false);
        let workspace = Workspace::new().expect("test workspace should be created");
        fs::write(workspace.layer_blob(0), b"not a gzip stream")
            .expect("invalid layer should be written");
        let diff_id = digest::Sha256Digest::from(b"expected".as_slice());
        let media_kind = "application/vnd.oci.image.layer.v1.tar+gzip"
            .parse()
            .expect("test media type should parse");

        let error = store
            .apply_layer(&workspace, 0, &media_kind, diff_id)
            .await
            .expect_err("invalid gzip should fail");

        assert!(matches!(error, Error::InvalidLayer { .. }));
        assert!(error.to_string().contains("decompression failed"));

        let tar_workspace = Workspace::new().expect("test workspace should be created");
        let invalid_tar = vec![b'x'; 512];
        fs::write(tar_workspace.layer_blob(0), &invalid_tar)
            .expect("invalid tar layer should be written");
        let diff_id = digest::Sha256Digest::from(invalid_tar.as_slice());
        let media_kind = "application/vnd.oci.image.layer.v1.tar"
            .parse()
            .expect("test media type should parse");

        let tar_error = store
            .apply_layer(&tar_workspace, 0, &media_kind, diff_id)
            .await
            .expect_err("invalid tar should fail");

        assert!(matches!(tar_error, Error::InvalidLayer { .. }));
        assert!(tar_error.to_string().contains("invalid tar entry"));
    }

    #[test]
    fn filesystem_capacity_uses_dynamic_data_and_inode_headroom() {
        let temporary = tempfile::tempdir().expect("test directory should be created");
        fs::write(temporary.path().join("small"), b"small").expect("small file should be written");
        assert_eq!(
            filesystem_layout(temporary.path()).expect("small layout should calculate"),
            FilesystemLayout {
                capacity: 256 * MIB,
                inodes: 8192,
            }
        );

        let large = temporary.path().join("large");
        fs::File::create(&large)
            .expect("large file should be created")
            .set_len(300 * MIB)
            .expect("large file should be sized");
        assert_eq!(
            filesystem_layout(temporary.path())
                .expect("large layout should calculate")
                .capacity,
            384 * MIB
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn real_ext4_builder_produces_a_clean_inspectable_filesystem() {
        let temporary = tempfile::tempdir().expect("test directory should be created");
        let root = temporary.path().join("root");
        fs::create_dir_all(root.join("etc")).expect("test etc directory should be created");
        fs::write(root.join("etc/os-release"), b"NAME=Elhone\n")
            .expect("test os-release should be written");
        let output = temporary.path().join("rootfs.ext4");

        let built = super::build_ext4(&root, &output).expect("ext4 should build");

        assert_eq!(built.size, 256 * MIB);
        let fsck = Command::new("e2fsck")
            .args(["-f", "-n"])
            .arg(&output)
            .output()
            .expect("e2fsck should execute");
        assert!(
            matches!(fsck.status.code(), Some(0 | 1)),
            "e2fsck failed: {}",
            String::from_utf8_lossy(&fsck.stderr)
        );
        let debugfs = Command::new("debugfs")
            .args(["-R", "cat /etc/os-release"])
            .arg(&output)
            .output()
            .expect("debugfs should execute");
        assert!(debugfs.status.success());
        assert_eq!(debugfs.stdout, b"NAME=Elhone\n");
    }

    #[test]
    fn synchronous_layer_application_reports_diff_id_mismatches() {
        let temporary = tempfile::tempdir().expect("test directory should be created");
        let root = temporary.path().join("root");
        fs::create_dir(&root).expect("test root should be created");
        let blob = temporary.path().join("layer.blob");
        let uncompressed = Archive(temporary.path().join("layer.tar"));
        let layer = TestLayer::new(
            archive(|builder| append_file(builder, "file", b"body", 0o644)),
            Encoding::Tar,
        );
        fs::write(&blob, layer.body).expect("test layer should be written");
        let wrong = digest::Sha256Digest::from(b"wrong".as_slice());

        let error = uncompressed
            .apply_layer_sync(&root, &blob, layer.media_kind.as_ref(), wrong, false)
            .expect_err("wrong diff ID should fail");

        assert!(error.to_string().contains("diff ID mismatch"));
        assert!(
            fs::read_dir(&root)
                .expect("test root should be readable")
                .next()
                .is_none()
        );
    }
}
