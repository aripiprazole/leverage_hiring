use std::{
    collections::BTreeSet,
    fs::File,
    io::BufReader,
    path::{Path, PathBuf},
    process::ExitStatus,
};

use oci_spec::runtime::{PosixRlimitType, Process};
use thiserror::Error;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(target_os = "linux"))]
mod unsupported;

#[cfg(target_os = "linux")]
pub use linux::{OciChild, mount_userspace_filesystems, power_off, sync_filesystems};
#[cfg(not(target_os = "linux"))]
pub use unsupported::{OciChild, mount_userspace_filesystems, power_off, sync_filesystems};

pub type Result<T, E = EntrypointError> = std::result::Result<T, E>;

#[derive(Debug)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct ProcessSpec {
    program: String,
    arguments: Vec<String>,
    environment: Vec<EnvironmentVariable>,
    working_directory: PathBuf,
    uid: u32,
    gid: u32,
    additional_gids: Vec<u32>,
    umask: Option<u32>,
    rlimits: Vec<ProcessRlimit>,
    no_new_privileges: bool,
}

impl ProcessSpec {
    pub fn new(path: impl AsRef<Path>) -> Result<ProcessSpec> {
        let path = path.as_ref();
        let file = File::open(path).map_err(|source| EntrypointError::OpenProcessSpec {
            path: path.to_owned(),
            source,
        })?;
        let process: Process = serde_json::from_reader(BufReader::new(file)).map_err(|source| {
            EntrypointError::DecodeProcessSpec {
                path: path.to_owned(),
                source,
            }
        })?;
        ProcessSpec::try_from(process).map_err(|source| EntrypointError::ParseProcessSpec {
            path: path.to_owned(),
            source,
        })
    }
}

#[derive(Debug)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct EnvironmentVariable {
    name: String,
    value: String,
}

#[derive(Debug)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct ProcessRlimit {
    resource: PosixRlimitType,
    soft: u64,
    hard: u64,
}

impl TryFrom<Process> for ProcessSpec {
    type Error = ParseProcessSpecError;

    fn try_from(process: Process) -> std::result::Result<Self, Self::Error> {
        let (program, arguments) = process
            .args()
            .as_deref()
            .and_then(|arguments| arguments.split_first())
            .filter(|(program, _)| !program.is_empty())
            .ok_or(ParseProcessSpecError::MissingExecutable)?;
        if !process.cwd().is_absolute() {
            return Err(ParseProcessSpecError::RelativeWorkingDirectory);
        }
        if process.terminal().unwrap_or(false) {
            return Err(ParseProcessSpecError::UnsupportedField("terminal"));
        }
        if process.command_line().is_some() {
            return Err(ParseProcessSpecError::UnsupportedField("commandLine"));
        }
        if process.capabilities().is_some() {
            return Err(ParseProcessSpecError::UnsupportedField("capabilities"));
        }
        if process.apparmor_profile().is_some() {
            return Err(ParseProcessSpecError::UnsupportedField("apparmorProfile"));
        }
        if process.selinux_label().is_some() {
            return Err(ParseProcessSpecError::UnsupportedField("selinuxLabel"));
        }
        if process.oom_score_adj().is_some() {
            return Err(ParseProcessSpecError::UnsupportedField("oomScoreAdj"));
        }
        if process.scheduler().is_some() {
            return Err(ParseProcessSpecError::UnsupportedField("scheduler"));
        }
        if process.io_priority().is_some() {
            return Err(ParseProcessSpecError::UnsupportedField("ioPriority"));
        }
        if process.exec_cpu_affinity().is_some() {
            return Err(ParseProcessSpecError::UnsupportedField("execCPUAffinity"));
        }
        if process.user().username().is_some() {
            return Err(ParseProcessSpecError::UnsupportedField("user.username"));
        }
        if process.user().umask().is_some_and(|mask| mask > 0o777) {
            return Err(ParseProcessSpecError::InvalidUmask);
        }

        let environment = process
            .env()
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|variable| {
                let (name, value) = variable.split_once('=').ok_or_else(|| {
                    ParseProcessSpecError::InvalidEnvironmentEntry {
                        entry: variable.clone(),
                        reason: "must use NAME=VALUE",
                    }
                })?;
                if name.is_empty() || name.as_bytes().contains(&0) {
                    return Err(ParseProcessSpecError::InvalidEnvironmentEntry {
                        entry: variable.clone(),
                        reason: "has an invalid name",
                    });
                }
                Ok(EnvironmentVariable {
                    name: name.to_owned(),
                    value: value.to_owned(),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut rlimit_types = BTreeSet::new();
        let rlimits = process
            .rlimits()
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|rlimit| {
                let resource = rlimit.typ();
                if rlimit.soft() > rlimit.hard() {
                    return Err(ParseProcessSpecError::RlimitSoftExceedsHard(resource));
                }
                if !rlimit_types.insert(resource.to_string()) {
                    return Err(ParseProcessSpecError::DuplicateRlimit(resource));
                }
                Ok(ProcessRlimit {
                    resource,
                    soft: rlimit.soft(),
                    hard: rlimit.hard(),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            program: program.clone(),
            arguments: arguments.to_vec(),
            environment,
            working_directory: process.cwd().clone(),
            uid: process.user().uid(),
            gid: process.user().gid(),
            additional_gids: process.user().additional_gids().clone().unwrap_or_default(),
            umask: process.user().umask(),
            rlimits,
            no_new_privileges: process.no_new_privileges().unwrap_or(false),
        })
    }
}

#[derive(Debug, Error)]
pub enum ParseProcessSpecError {
    #[error("args must contain a non-empty executable")]
    MissingExecutable,

    #[error("cwd must be an absolute path")]
    RelativeWorkingDirectory,

    #[error("OCI process field {0} is not supported by the VM entrypoint")]
    UnsupportedField(&'static str),

    #[error("user.umask must fit in 0777")]
    InvalidUmask,

    #[error("environment entry {entry:?} {reason}")]
    InvalidEnvironmentEntry { entry: String, reason: &'static str },

    #[error("{0} has a soft limit greater than its hard limit")]
    RlimitSoftExceedsHard(PosixRlimitType),

    #[error("{0} appears more than once")]
    DuplicateRlimit(PosixRlimitType),
}

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

    #[error("failed to parse OCI process spec at {path}: {source}")]
    ParseProcessSpec {
        path: PathBuf,
        #[source]
        source: ParseProcessSpecError,
    },

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

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use crate::ProcessSpec;

    use super::{EntrypointError, ParseProcessSpecError, PosixRlimitType};

    #[test]
    fn parses_the_default_guest_process() {
        let spec = Path::new(env!("CARGO_MANIFEST_DIR")).join("default-process.json");
        let process = ProcessSpec::new(spec).expect("default process spec should parse");

        assert_eq!(process.program, "/bin/sh");
    }

    #[test]
    fn parses_an_oci_process_document() {
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

        let process = ProcessSpec::new(&spec).expect("process spec should be read");

        assert_eq!(process.program, "/bin/echo");
        assert_eq!(process.arguments, ["hello"]);
        assert_eq!(process.environment.len(), 2);
        assert_eq!(process.environment[1].name, "MESSAGE");
        assert_eq!(process.environment[1].value, "hello=world");
        assert_eq!(process.uid, 1000);
        assert_eq!(process.gid, 1000);
        assert_eq!(process.additional_gids, [10]);
        assert_eq!(process.working_directory.to_string_lossy(), "/tmp");
        assert_eq!(process.rlimits.len(), 1);
        assert_eq!(process.rlimits[0].resource, PosixRlimitType::RlimitNofile);
        assert_eq!(process.rlimits[0].soft, 128);
        assert_eq!(process.rlimits[0].hard, 256);
        assert!(process.no_new_privileges);
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

        let error = ProcessSpec::new(&spec).expect_err("empty args must be rejected");

        assert!(matches!(
            error,
            EntrypointError::ParseProcessSpec {
                source: ParseProcessSpecError::MissingExecutable,
                ..
            }
        ));
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

        let error = ProcessSpec::new(&spec).expect_err("relative cwd must be rejected");

        assert!(error.to_string().contains("absolute path"));
    }
}
