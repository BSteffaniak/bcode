//! Handle-relative private files: never follow links or truncate existing files.
use crate::Failure;
use bcode_confined_fs::{Creation, Dir, EntryKind, FileIdentity, OpenOptions};
use std::{
    ffi::OsStr,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub(super) struct Directory {
    directory: Dir,
}
/// Owned by and accessible only to the current user; files must have a single link.
fn owned(file: &File, directory: bool) -> bool {
    let (Ok(metadata), Ok(private)) = (
        bcode_confined_fs::file_metadata(file),
        bcode_confined_fs::is_private(file),
    ) else {
        return false;
    };
    private
        && if directory {
            metadata.kind() == EntryKind::Directory
        } else {
            metadata.kind() == EntryKind::File && metadata.link_count() == 1
        }
}
impl Directory {
    pub(super) fn open(path: &Path) -> Result<Self, Failure> {
        if !path.is_absolute()
            || path
                .to_str()
                .is_none_or(|value| value.chars().any(char::is_control))
            || std::fs::canonicalize(path).map_err(|_| "invalid_private_directory")? != path
        {
            return Err("invalid_private_directory");
        }
        let directory = Dir::open(path).map_err(|_| "invalid_private_directory")?;
        if !owned(directory.as_file(), true) {
            return Err("invalid_private_directory");
        }
        Ok(Self { directory })
    }
    fn open_file(&self, name: &str, create: bool) -> Result<File, Failure> {
        let options = if create {
            OpenOptions::new()
                .write(true)
                .private(true)
                .creation(Creation::New)
        } else {
            OpenOptions::new().read(true)
        };
        let file = self
            .directory
            .open_file(OsStr::new(name), options)
            .map_err(|_| "private_file_unavailable")?;
        if !owned(&file, false) {
            return Err("invalid_private_file");
        }
        Ok(file)
    }
    pub(super) fn publish(&self, token: &str, expiry: u64, config: &str) -> Result<(), Failure> {
        let mut created = Created {
            directory: self,
            files: Vec::new(),
            committed: false,
        };
        for (name, contents) in [
            ("broker-token", format!("{token}\n")),
            ("broker-expiry", format!("{expiry}\n")),
            ("broker.toml", config.to_owned()),
        ] {
            let mut file = self.open_file(name, true)?;
            let metadata =
                bcode_confined_fs::file_metadata(&file).map_err(|_| "private_file_unavailable")?;
            created.files.push((name, metadata.identity()));
            file.write_all(contents.as_bytes())
                .map_err(|_| "private_file_write_failed")?;
            file.sync_all().map_err(|_| "private_file_write_failed")?;
        }
        created.committed = true;
        Ok(())
    }
    fn read(&self, name: &str, limit: u64) -> Result<String, Failure> {
        let mut value = String::new();
        self.open_file(name, false)?
            .take(limit + 1)
            .read_to_string(&mut value)
            .map_err(|_| "invalid_private_file")?;
        if value.len() as u64 > limit {
            return Err("invalid_private_file");
        }
        Ok(value)
    }
}
struct Created<'a> {
    directory: &'a Directory,
    files: Vec<(&'static str, FileIdentity)>,
    committed: bool,
}
impl Drop for Created<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for (name, identity) in &self.files {
            let Ok(file) = self.directory.open_file(name, false) else {
                continue;
            };
            let Ok(metadata) = bcode_confined_fs::file_metadata(&file) else {
                continue;
            };
            drop(file);
            if metadata.identity() != *identity {
                continue;
            }
            let _ = self.directory.directory.remove_file(OsStr::new(name));
        }
    }
}
pub(super) fn read_lease(lease: &str, expiry: &str) -> Result<(String, u64), Failure> {
    let lease = PathBuf::from(lease);
    let expiry = PathBuf::from(expiry);
    if lease.file_name().is_none_or(|name| name != "broker-token")
        || expiry
            .file_name()
            .is_none_or(|name| name != "broker-expiry")
        || lease.parent() != expiry.parent()
    {
        return Err("invalid_private_file");
    }
    let directory = Directory::open(lease.parent().ok_or("invalid_private_directory")?)?;
    let value = directory.read("broker-token", 16_385)?;
    let value = value.strip_suffix('\n').unwrap_or(&value);
    super::token(value)?;
    let expiry = directory.read("broker-expiry", 32)?;
    let expiry = expiry
        .strip_suffix('\n')
        .unwrap_or(&expiry)
        .parse()
        .map_err(|_| "invalid_lease_expiry")?;
    Ok((value.to_owned(), expiry))
}
