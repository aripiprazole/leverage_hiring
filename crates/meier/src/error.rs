use std::{io, path::PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum MeierError {
    #[error("{0}")]
    Cli(String),
    #[error("configuration file {path} could not be read: {source}")]
    ConfigRead { path: PathBuf, source: io::Error },
    #[error("configuration file {path} is invalid: {source}")]
    ConfigDecode {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("configuration file {path} could not be written: {source}")]
    ConfigWrite { path: PathBuf, source: io::Error },
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    #[error("HTTP request failed with status {status}: {message}")]
    Http { status: u16, message: String },
    #[error("{0}")]
    Request(#[from] reqwest::Error),
    #[error("Lima command failed: {message}")]
    Lima { message: String },
    #[error("command `{program}` failed: {message}")]
    Command { program: String, message: String },
    #[error("command `{program}` exited with status {code}")]
    ProcessExit { program: String, code: i32 },
    #[error("setup failed: {0}")]
    Setup(String),
    #[error("unsupported platform: {0}")]
    UnsupportedPlatform(String),
    #[error("{0}")]
    Validation(String),
}

impl MeierError {
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Cli(_) => 2,
            Self::ProcessExit { code, .. } if *code > 0 => (*code).min(255),
            _ => 1,
        }
    }
}

pub(crate) fn process_exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .unwrap_or_else(|| status.signal().map_or(1, |signal| 128 + signal))
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(1)
    }
}

impl From<clap::Error> for MeierError {
    fn from(error: clap::Error) -> Self {
        Self::Cli(error.to_string())
    }
}
