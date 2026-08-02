use std::path::PathBuf;

use super::{Result, StorageError};

#[derive(Debug, Clone)]
pub struct VmStore;

impl VmStore {
    /// # Errors
    ///
    /// This implementation currently cannot fail.
    pub fn new(_vm_root: PathBuf, _image_root: PathBuf) -> Result<Self, StorageError> {
        Ok(Self)
    }
}
