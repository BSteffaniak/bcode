//! Other operating systems fail closed until a native no-follow storage adapter exists.
use crate::contracts::{Error, Result};
use std::{fs::File, path::Path};

pub(super) enum Directory {
    Unsupported,
}

impl Directory {
    fn unavailable<T>(&self) -> Result<T> {
        match self {
            Self::Unsupported => Err(Error::Unsupported(
                "secure model storage is currently implemented for Linux only".into(),
            )),
        }
    }

    pub(super) fn open(_path: &Path, _create: bool) -> Result<Self> {
        Self::Unsupported.unavailable()
    }
    pub(super) fn lock(&self) -> Result<()> {
        self.unavailable()
    }
    pub(super) fn read(&self, _name: &str) -> Result<File> {
        self.unavailable()
    }
    pub(super) fn create(&self, _name: &str) -> Result<File> {
        self.unavailable()
    }
    pub(super) fn publish(&self, _temporary: &str, _destination: &str) -> Result<()> {
        self.unavailable()
    }
    pub(super) fn unlink(&self, _name: &str) -> Result<()> {
        self.unavailable()
    }
    pub(super) fn sync(&self) -> Result<()> {
        self.unavailable()
    }
}
