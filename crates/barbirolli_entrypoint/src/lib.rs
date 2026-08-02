use std::{
    fs::File,
    io::BufReader,
    path::{Path, PathBuf},
    process::ExitStatus,
};

use oci_spec::runtime::Process;
use thiserror::Error;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(target_os = "linux"))]
mod unsupported;

#[cfg(target_os = "linux")]
pub use linux::{
    OciChild, mount_userspace_filesystems, power_off, spawn_oci_process, sync_filesystems,
};
#[cfg(not(target_os = "linux"))]
pub use unsupported::{
    OciChild, mount_userspace_filesystems, power_off, spawn_oci_process, sync_filesystems,
};

pub type Result<T, E = EntrypointError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum EntrypointError {
    #[error("barbirolli_entrypoint only runs on Linux")]
    UnsupportedPlatform,

    #[error("failed to open OCI process spec at {path}: {source}")]
    OpenProcessSpec {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to decode OCI process spec at {path}: {source}")]
    DecodeProcessSpec {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("invalid OCI process spec: {0}")]
    InvalidProcessSpec(String),

    #[error("OCI process field {0} is not supported by the VM entrypoint")]
    UnsupportedProcessField(&'static str),

    #[error("failed to create mountpoint {path}: {source}")]
    CreateMountpoint {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to inspect mounted filesystems: {0}")]
    InspectMounts(#[source] std::io::Error),

    #[error("failed to mount {filesystem} at {target}: {source}")]
    MountFilesystem {
        filesystem: &'static str,
        target: &'static str,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to configure PID 1 signal supervision: {0}")]
    SignalSupervision(#[source] std::io::Error),

    #[error("failed to spawn OCI process {program:?}: {source}")]
    SpawnProcess {
        program: String,
        #[source]
        source: std::io::Error,
    },

    #[error("failed while waiting for OCI process descendants: {0}")]
    WaitForProcess(#[source] std::io::Error),

    #[error("failed to forward signal {signal} to OCI process group: {source}")]
    ForwardSignal {
        signal: i32,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to power off after OCI process exited with {exit}: {source}")]
    PowerOff {
        exit: ExitStatus,
        #[source]
        source: std::io::Error,
    },
}

pub fn read_process_spec(path: impl AsRef<Path>) -> Result<Process> {
    let path = path.as_ref();
    let file = File::open(path).map_err(|source| EntrypointError::OpenProcessSpec {
        path: path.to_owned(),
        source,
    })?;
    let process = serde_json::from_reader(BufReader::new(file)).map_err(|source| {
        EntrypointError::DecodeProcessSpec {
            path: path.to_owned(),
            source,
        }
    })?;
    validate_process_spec(&process)?;
    Ok(process)
}

fn validate_process_spec(process: &Process) -> Result<()> {
    let arguments = process.args().as_deref().ok_or_else(|| {
        EntrypointError::InvalidProcessSpec("args must contain an executable".to_owned())
    })?;
    if arguments.first().is_none_or(String::is_empty) {
        return Err(EntrypointError::InvalidProcessSpec(
            "args must contain a non-empty executable".to_owned(),
        ));
    }
    if !process.cwd().is_absolute() {
        return Err(EntrypointError::InvalidProcessSpec(
            "cwd must be an absolute path".to_owned(),
        ));
    }
    if process.terminal().unwrap_or(false) {
        return Err(EntrypointError::UnsupportedProcessField("terminal"));
    }
    if process.command_line().is_some() {
        return Err(EntrypointError::UnsupportedProcessField("commandLine"));
    }
    if process.capabilities().is_some() {
        return Err(EntrypointError::UnsupportedProcessField("capabilities"));
    }
    if process.apparmor_profile().is_some() {
        return Err(EntrypointError::UnsupportedProcessField("apparmorProfile"));
    }
    if process.selinux_label().is_some() {
        return Err(EntrypointError::UnsupportedProcessField("selinuxLabel"));
    }
    if process.oom_score_adj().is_some() {
        return Err(EntrypointError::UnsupportedProcessField("oomScoreAdj"));
    }
    if process.scheduler().is_some() {
        return Err(EntrypointError::UnsupportedProcessField("scheduler"));
    }
    if process.io_priority().is_some() {
        return Err(EntrypointError::UnsupportedProcessField("ioPriority"));
    }
    if process.exec_cpu_affinity().is_some() {
        return Err(EntrypointError::UnsupportedProcessField("execCPUAffinity"));
    }
    if process.user().username().is_some() {
        return Err(EntrypointError::UnsupportedProcessField("user.username"));
    }
    if process.user().umask().is_some_and(|mask| mask > 0o777) {
        return Err(EntrypointError::InvalidProcessSpec(
            "user.umask must fit in 0777".to_owned(),
        ));
    }

    if let Some(environment) = process.env() {
        for variable in environment {
            let Some((name, _)) = variable.split_once('=') else {
                return Err(EntrypointError::InvalidProcessSpec(format!(
                    "environment entry {variable:?} must use NAME=VALUE"
                )));
            };
            if name.is_empty() || name.as_bytes().contains(&0) {
                return Err(EntrypointError::InvalidProcessSpec(format!(
                    "environment entry {variable:?} has an invalid name"
                )));
            }
        }
    }

    let mut rlimit_types = std::collections::BTreeSet::new();
    if let Some(rlimits) = process.rlimits() {
        for rlimit in rlimits {
            if rlimit.soft() > rlimit.hard() {
                return Err(EntrypointError::InvalidProcessSpec(format!(
                    "{} has a soft limit greater than its hard limit",
                    rlimit.typ()
                )));
            }
            if !rlimit_types.insert(rlimit.typ().to_string()) {
                return Err(EntrypointError::InvalidProcessSpec(format!(
                    "{} appears more than once",
                    rlimit.typ()
                )));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::{EntrypointError, read_process_spec};

    #[test]
    fn default_guest_process_is_a_valid_oci_process_document() {
        let spec = Path::new(env!("CARGO_MANIFEST_DIR")).join("default-process.json");

        let process = read_process_spec(spec).expect("default process spec should be valid");

        assert_eq!(
            process
                .args()
                .as_deref()
                .and_then(|args| args.first())
                .map(String::as_str),
            Some("/bin/sh")
        );
    }

    #[test]
    fn reads_an_oci_process_document() {
        let temporary = tempfile::tempdir().expect("temporary directory should be created");
        let spec = temporary.path().join("process.json");
        fs::write(
            &spec,
            r#"{
                "terminal": false,
                "user": { "uid": 1000, "gid": 1000, "additionalGids": [10] },
                "args": ["/bin/echo", "hello"],
                "env": ["PATH=/bin:/usr/bin", "MESSAGE=hello=world"],
                "cwd": "/tmp",
                "rlimits": [{ "type": "RLIMIT_NOFILE", "soft": 128, "hard": 256 }],
                "noNewPrivileges": true
            }"#,
        )
        .expect("process spec should be written");

        let process = read_process_spec(&spec).expect("process spec should be read");

        assert_eq!(
            process.args().as_deref(),
            Some(["/bin/echo".to_owned(), "hello".to_owned()].as_slice())
        );
        assert_eq!(process.user().uid(), 1000);
        assert_eq!(process.user().gid(), 1000);
        assert_eq!(process.cwd().to_string_lossy(), "/tmp");
    }

    #[test]
    fn rejects_a_process_without_an_executable() {
        let temporary = tempfile::tempdir().expect("temporary directory should be created");
        let spec = temporary.path().join("process.json");
        fs::write(
            &spec,
            r#"{
                "user": { "uid": 0, "gid": 0 },
                "args": [],
                "cwd": "/"
            }"#,
        )
        .expect("process spec should be written");

        let error = read_process_spec(&spec).expect_err("empty args must be rejected");

        assert!(matches!(error, EntrypointError::InvalidProcessSpec(_)));
        assert!(error.to_string().contains("executable"));
    }

    #[test]
    fn rejects_relative_working_directories() {
        let temporary = tempfile::tempdir().expect("temporary directory should be created");
        let spec = temporary.path().join("process.json");
        fs::write(
            &spec,
            r#"{
                "user": { "uid": 0, "gid": 0 },
                "args": ["/bin/true"],
                "cwd": "tmp"
            }"#,
        )
        .expect("process spec should be written");

        let error = read_process_spec(&spec).expect_err("relative cwd must be rejected");

        assert!(error.to_string().contains("absolute path"));
    }
}
