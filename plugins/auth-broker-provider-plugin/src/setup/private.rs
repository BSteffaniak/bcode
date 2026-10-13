//! Descriptor-relative private files: never follow links or truncate existing files.
use crate::Failure;
use std::{
    ffi::CString,
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::MetadataExt,
    },
    path::{Path, PathBuf},
};

pub(super) struct Directory {
    file: File,
}
fn owned(metadata: &std::fs::Metadata, directory: bool) -> bool {
    // SAFETY: geteuid has no preconditions and does not mutate process state.
    let uid = unsafe { libc::geteuid() };
    metadata.uid() == uid
        && metadata.mode().trailing_zeros() >= 6
        && if directory {
            metadata.is_dir()
        } else {
            metadata.is_file() && metadata.nlink() == 1
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
        let path = CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| "invalid_private_directory")?;
        // SAFETY: path is NUL terminated; returned descriptor is exclusively owned below.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err("invalid_private_directory");
        }
        // SAFETY: successful open returned an owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        if !owned(
            &file.metadata().map_err(|_| "invalid_private_directory")?,
            true,
        ) {
            return Err("invalid_private_directory");
        }
        Ok(Self { file })
    }
    fn open_file(&self, name: &str, create: bool) -> Result<File, Failure> {
        let name = CString::new(name).map_err(|_| "invalid_private_file")?;
        let flags = libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if create {
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
            } else {
                libc::O_RDONLY
            };
        // SAFETY: directory descriptor remains live, name is NUL terminated, mode is supplied.
        let fd = unsafe { libc::openat(self.file.as_raw_fd(), name.as_ptr(), flags, 0o600) };
        if fd < 0 {
            return Err("private_file_unavailable");
        }
        // SAFETY: successful openat returned an owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        if !owned(&file.metadata().map_err(|_| "invalid_private_file")?, false) {
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
            let metadata = file.metadata().map_err(|_| "private_file_unavailable")?;
            created.files.push((name, metadata.dev(), metadata.ino()));
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
    files: Vec<(&'static str, u64, u64)>,
    committed: bool,
}
impl Drop for Created<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for (name, device, inode) in &self.files {
            let Ok(file) = self.directory.open_file(name, false) else {
                continue;
            };
            let Ok(metadata) = file.metadata() else {
                continue;
            };
            if metadata.dev() != *device || metadata.ino() != *inode {
                continue;
            }
            let Ok(name) = CString::new(*name) else {
                continue;
            };
            // SAFETY: anchored live directory and constant single-component file name.
            unsafe {
                libc::unlinkat(self.directory.file.as_raw_fd(), name.as_ptr(), 0);
            }
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
