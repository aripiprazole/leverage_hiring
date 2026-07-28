use std::{io, path::PathBuf};

use crate::UserName;

#[cfg(feature = "linux")]
mod linux;
#[cfg(not(feature = "linux"))]
mod macos;

#[cfg(feature = "linux")]
pub use linux::*;
#[cfg(not(feature = "linux"))]
pub use macos::*;

pub type Result<T, E = StorageError> = std::result::Result<T, E>;

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

#[cfg(feature = "linux")]
impl StorageError {
    fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
