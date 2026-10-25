//! Linux descriptor-relative storage. No path-based check-then-open/delete operations.

use crate::contracts::{Error, Result};
use std::{
    ffi::{CStr, CString},
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path},
};

pub(super) struct Directory(File);

fn safe_name(name: &str) -> Result<CString> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(Error::Invalid("invalid model storage basename".into()));
    }
    CString::new(name).map_err(|_| Error::Invalid("invalid model storage basename".into()))
}

fn open_at(parent: &File, name: &CStr, flags: libc::c_int, mode: libc::mode_t) -> io::Result<File> {
    // SAFETY: name is NUL-terminated, parent is live, and the returned descriptor
    // is owned exactly once. mode is supplied even when O_CREAT is absent.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            mode,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful openat returned a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn user_id() -> libc::uid_t {
    // SAFETY: getuid has no pointer arguments or preconditions.
    unsafe { libc::getuid() }
}

fn reject() -> Error {
    Error::Invalid(
        "unsafe model storage: require owned private directories/files, no symlinks or hardlinks"
            .into(),
    )
}

impl Directory {
    pub(super) fn open(path: &Path, create: bool) -> Result<Self> {
        if !path.is_absolute() {
            return Err(reject());
        }
        let mut directory = File::open("/")?;
        let parts: Vec<_> = path.components().collect();
        if parts.len() < 3 {
            return Err(reject());
        }
        for (index, part) in parts.iter().enumerate() {
            let Component::Normal(part) = part else {
                if matches!(part, Component::RootDir) {
                    continue;
                }
                return Err(reject());
            };
            let name = CString::new(part.as_bytes()).map_err(|_| reject())?;
            let next = match open_at(&directory, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0) {
                Ok(file) => file,
                Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                    // SAFETY: directory is live and name is a NUL-terminated single
                    // component. Existing entries are reopened with O_NOFOLLOW.
                    let result =
                        unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) };
                    if result < 0 {
                        let error = io::Error::last_os_error();
                        if error.kind() != io::ErrorKind::AlreadyExists {
                            return Err(error.into());
                        }
                    }
                    open_at(&directory, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?
                }
                Err(error) => return Err(error.into()),
            };
            let metadata = next.metadata()?;
            // The final two directories are Vox-owned private directories. Ancestors
            // may be shared only with sticky protection (e.g. /tmp in offline tests).
            if index + 2 >= parts.len() {
                if metadata.uid() != user_id() || metadata.mode() & 0o077 != 0 {
                    return Err(reject());
                }
            } else if (metadata.uid() != 0 && metadata.uid() != user_id())
                || (metadata.mode() & 0o022 != 0
                    && !(metadata.mode() & libc::S_ISVTX != 0
                        && (metadata.uid() == 0 || metadata.uid() == user_id())))
            {
                return Err(reject());
            }
            directory = next;
        }
        Ok(Self(directory))
    }

    pub(super) fn lock(&self) -> Result<()> {
        // SAFETY: flock operates only on this live directory descriptor. The lock
        // is released when the Directory drops. Nonblocking avoids hidden waits.
        if unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } < 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::WouldBlock {
                Err(Error::Busy)
            } else {
                Err(error.into())
            };
        }
        Ok(())
    }

    pub(super) fn read(&self, name: &str) -> Result<File> {
        let name = safe_name(name)?;
        let file = open_at(&self.0, &name, libc::O_RDONLY | libc::O_NONBLOCK, 0)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != user_id()
            || metadata.nlink() != 1
            || metadata.mode() & 0o077 != 0
        {
            return Err(reject());
        }
        Ok(file)
    }

    pub(super) fn create(&self, name: &str) -> Result<File> {
        let name = safe_name(name)?;
        Ok(open_at(
            &self.0,
            &name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?)
    }

    pub(super) fn publish(&self, temporary: &str, destination: &str) -> Result<()> {
        let temporary = safe_name(temporary)?;
        let destination = safe_name(destination)?;
        // SAFETY: both names are single NUL-terminated components relative to the
        // same live directory descriptor. NOREPLACE never overwrites another writer.
        let result = unsafe {
            libc::renameat2(
                self.0.as_raw_fd(),
                temporary.as_ptr(),
                self.0.as_raw_fd(),
                destination.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }

    pub(super) fn unlink(&self, name: &str) -> Result<()> {
        let name = safe_name(name)?;
        // SAFETY: unlinkat removes this basename itself, never follows symlinks and
        // never recurses. Flags=0 cannot remove a directory.
        if unsafe { libc::unlinkat(self.0.as_raw_fd(), name.as_ptr(), 0) } < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }

    pub(super) fn sync(&self) -> Result<()> {
        Ok(self.0.sync_all()?)
    }
}
