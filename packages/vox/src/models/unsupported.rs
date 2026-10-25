//! Other operating systems fail closed until a native no-follow storage adapter exists.
use crate::contracts::{Error, Result};
use std::{fs::File, path::Path};

pub(super) struct Directory;

fn unavailable<T>() -> Result<T> {
    Err(Error::Unsupported(
        "secure model storage is currently implemented for Linux only".into(),
    ))
}

impl Directory {
    pub(super) fn open(_path: &Path, _create: bool) -> Result<Self> {
        unavailable()
    }
    pub(super) fn lock(&self) -> Result<()> {
        unavailable()
    }
    pub(super) fn read(&self, _name: &str) -> Result<File> {
        unavailable()
    }
    pub(super) fn create(&self, _name: &str) -> Result<File> {
        unavailable()
    }
    pub(super) fn publish(&self, _temporary: &str, _destination: &str) -> Result<()> {
        unavailable()
    }
    pub(super) fn unlink(&self, _name: &str) -> Result<()> {
        unavailable()
    }
    pub(super) fn sync(&self) -> Result<()> {
        unavailable()
    }
}
