//! `openat`-family backend.

use std::ffi::{CStr, CString, OsStr};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, RawFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Component, Path};

use crate::{
    Creation, Entry, EntryKind, FileIdentity, Metadata, OpenOptions, RenameMode, Timestamp,
};

pub fn validate_non_utf8_name(name: &OsStr) -> Result<(), ()> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(());
    }
    Ok(())
}

fn c_name(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name contains NUL"))
}

fn check(result: libc::c_int) -> io::Result<libc::c_int> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

fn openat(directory: RawFd, name: &CStr, flags: libc::c_int) -> io::Result<File> {
    // SAFETY: name is NUL-terminated and the mode is only consulted for creation.
    let fd = check(unsafe {
        libc::openat(
            directory,
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    })?;
    // SAFETY: openat returned a new descriptor owned exclusively by this File.
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub fn open_dir_path(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

pub fn open_dir_path_following(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)
}

pub fn open_dir_path_strict(path: &Path) -> io::Result<File> {
    let mut components = path.components();
    if components.next() != Some(Component::RootDir) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected an absolute directory path",
        ));
    }
    let mut directory = File::open("/")?;
    for component in components {
        let Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected a normalized directory path",
            ));
        };
        crate::validate_name(name)?;
        directory = open_dir_at(&directory, name)?;
    }
    Ok(directory)
}

pub fn open_dir_at(directory: &File, name: &OsStr) -> io::Result<File> {
    openat(
        directory.as_raw_fd(),
        &c_name(name)?,
        libc::O_RDONLY | libc::O_DIRECTORY,
    )
}

const fn file_flags(options: OpenOptions) -> libc::c_int {
    // Files are always created owner-only (0o600), which already satisfies `private`.
    let _ = options.private;
    let writes = options.write || options.append;
    let mut flags = match (options.read, writes) {
        (true, true) => libc::O_RDWR,
        (false, true) => libc::O_WRONLY,
        _ => libc::O_RDONLY,
    };
    if options.append {
        flags |= libc::O_APPEND;
    }
    if options.truncate {
        flags |= libc::O_TRUNC;
    }
    // O_NONBLOCK keeps a substituted FIFO from blocking before it can be rejected.
    flags
        | libc::O_NONBLOCK
        | match options.creation {
            Creation::Existing => 0,
            Creation::IfMissing => libc::O_CREAT,
            Creation::New => libc::O_CREAT | libc::O_EXCL,
        }
}

fn regular_file(file: File) -> io::Result<File> {
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "confined entry is not a regular file",
        ));
    }
    // SAFETY: fcntl operates on the live descriptor owned by file.
    let status = check(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) })?;
    // SAFETY: as above; only the non-blocking status flag is cleared.
    check(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, status & !libc::O_NONBLOCK) })?;
    Ok(file)
}

pub fn open_file_at(directory: &File, name: &OsStr, options: OpenOptions) -> io::Result<File> {
    regular_file(openat(
        directory.as_raw_fd(),
        &c_name(name)?,
        file_flags(options),
    )?)
}

pub fn open_file_path(path: &Path, options: OpenOptions) -> io::Result<File> {
    let path = c_name(path.as_os_str())?;
    // SAFETY: path is NUL-terminated and the mode is only consulted for creation.
    let fd = check(unsafe {
        libc::open(
            path.as_ptr(),
            file_flags(options) | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    })?;
    // SAFETY: open returned a new descriptor owned exclusively by this File.
    regular_file(unsafe { File::from_raw_fd(fd) })
}

pub fn sync_dir_path(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn kind_of(file_type: std::fs::FileType) -> EntryKind {
    if file_type.is_symlink() {
        EntryKind::Symlink
    } else if file_type.is_dir() {
        EntryKind::Directory
    } else if file_type.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

const fn kind_of_mode(mode: libc::mode_t) -> EntryKind {
    match mode & libc::S_IFMT {
        libc::S_IFLNK => EntryKind::Symlink,
        libc::S_IFDIR => EntryKind::Directory,
        libc::S_IFREG => EntryKind::File,
        _ => EntryKind::Other,
    }
}

pub fn file_metadata(file: &File) -> io::Result<Metadata> {
    let metadata = file.metadata()?;
    Ok(Metadata {
        kind: kind_of(metadata.file_type()),
        len: metadata.len(),
        link_count: metadata.nlink(),
        identity: FileIdentity {
            volume: metadata.dev(),
            file: u128::from(metadata.ino()),
        },
        modified: timestamp(metadata.mtime(), metadata.mtime_nsec()),
        changed: timestamp(metadata.ctime(), metadata.ctime_nsec()),
    })
}

fn timestamp(seconds: i64, nanoseconds: i64) -> Timestamp {
    Timestamp(i128::from(seconds) * 1_000_000_000 + i128::from(nanoseconds))
}

#[allow(
    clippy::useless_conversion,
    clippy::unnecessary_fallible_conversions,
    reason = "stat field widths differ between Unix platforms"
)]
pub fn metadata_at(directory: &File, name: &OsStr) -> io::Result<Metadata> {
    let name = c_name(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: name is NUL-terminated and stat points to writable storage.
    check(unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    })?;
    // SAFETY: successful fstatat initialized the structure.
    let stat = unsafe { stat.assume_init() };
    Ok(Metadata {
        kind: kind_of_mode(stat.st_mode),
        len: u64::try_from(stat.st_size).map_err(invalid_stat)?,
        link_count: u64::try_from(stat.st_nlink).map_err(invalid_stat)?,
        identity: FileIdentity {
            volume: u64::try_from(stat.st_dev).map_err(invalid_stat)?,
            file: u128::from(u64::try_from(stat.st_ino).map_err(invalid_stat)?),
        },
        modified: timestamp(
            i64::try_from(stat.st_mtime).map_err(invalid_stat)?,
            i64::try_from(stat.st_mtime_nsec).map_err(invalid_stat)?,
        ),
        changed: timestamp(
            i64::try_from(stat.st_ctime).map_err(invalid_stat)?,
            i64::try_from(stat.st_ctime_nsec).map_err(invalid_stat)?,
        ),
    })
}

fn invalid_stat<E>(_: E) -> io::Error {
    io::Error::other("invalid stat field")
}

pub fn create_dir_at(directory: &File, name: &OsStr) -> io::Result<()> {
    let name = c_name(name)?;
    // SAFETY: name is NUL-terminated and relative to the live directory descriptor.
    check(unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) }).map(drop)
}

fn unlinkat(directory: &File, name: &OsStr, flags: libc::c_int) -> io::Result<()> {
    let name = c_name(name)?;
    // SAFETY: name is NUL-terminated and relative to the live directory descriptor.
    check(unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), flags) }).map(drop)
}

pub fn remove_file_at(directory: &File, name: &OsStr) -> io::Result<()> {
    unlinkat(directory, name, 0)
}

pub fn remove_dir_at(directory: &File, name: &OsStr) -> io::Result<()> {
    unlinkat(directory, name, libc::AT_REMOVEDIR)
}

pub fn rename_at(
    from_directory: &File,
    from: &OsStr,
    to_directory: &File,
    to: &OsStr,
    mode: RenameMode,
) -> io::Result<()> {
    let from = c_name(from)?;
    let to = c_name(to)?;
    let (from_fd, to_fd) = (from_directory.as_raw_fd(), to_directory.as_raw_fd());
    let result = match mode {
        // SAFETY: both names are NUL-terminated and relative to live descriptors.
        RenameMode::Replace => unsafe {
            libc::renameat(from_fd, from.as_ptr(), to_fd, to.as_ptr())
        },
        RenameMode::NoReplace => rename_no_replace(from_fd, &from, to_fd, &to)?,
    };
    check(result).map(drop)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "same fallible interface as platforms without no-replace renames"
)]
fn rename_no_replace(
    from_fd: RawFd,
    from: &CStr,
    to_fd: RawFd,
    to: &CStr,
) -> io::Result<libc::c_int> {
    // SAFETY: both names are NUL-terminated and relative to live descriptors.
    Ok(unsafe {
        libc::renameat2(
            from_fd,
            from.as_ptr(),
            to_fd,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    })
}

#[cfg(target_vendor = "apple")]
#[allow(
    clippy::unnecessary_wraps,
    reason = "same fallible interface as platforms without no-replace renames"
)]
fn rename_no_replace(
    from_fd: RawFd,
    from: &CStr,
    to_fd: RawFd,
    to: &CStr,
) -> io::Result<libc::c_int> {
    // SAFETY: both names are NUL-terminated and relative to live descriptors.
    Ok(unsafe {
        libc::renameatx_np(
            from_fd,
            from.as_ptr(),
            to_fd,
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    })
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn rename_no_replace(_: RawFd, _: &CStr, _: RawFd, _: &CStr) -> io::Result<libc::c_int> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no-replace rename is unavailable on this platform",
    ))
}

pub fn hard_link_at(
    from_directory: &File,
    from: &OsStr,
    to_directory: &File,
    to: &OsStr,
) -> io::Result<()> {
    let from = c_name(from)?;
    let to = c_name(to)?;
    // SAFETY: both names are NUL-terminated and relative to live descriptors. Flags of
    // zero link the entry itself rather than a symlink target.
    check(unsafe {
        libc::linkat(
            from_directory.as_raw_fd(),
            from.as_ptr(),
            to_directory.as_raw_fd(),
            to.as_ptr(),
            0,
        )
    })
    .map(drop)
}

/// Owns a `DIR` stream over a private duplicate of the directory descriptor.
pub struct Entries {
    stream: std::ptr::NonNull<libc::DIR>,
}

pub fn entries(directory: &File) -> io::Result<Entries> {
    // A fresh descriptor gives this enumeration its own position; a dup would share it.
    let duplicate = openat(
        directory.as_raw_fd(),
        c".",
        libc::O_RDONLY | libc::O_DIRECTORY,
    )?;
    let fd = std::os::fd::IntoRawFd::into_raw_fd(duplicate);
    // SAFETY: fd is an owned directory descriptor; fdopendir takes ownership on success.
    let Some(stream) = std::ptr::NonNull::new(unsafe { libc::fdopendir(fd) }) else {
        let error = io::Error::last_os_error();
        // SAFETY: fdopendir failed, so fd is still owned here.
        unsafe { libc::close(fd) };
        return Err(error);
    };
    Ok(Entries { stream })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn clear_errno() {
    // SAFETY: the thread-local errno location is always valid.
    unsafe { *libc::__errno_location() = 0 };
}

#[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
fn clear_errno() {
    // SAFETY: the thread-local errno location is always valid.
    unsafe { *libc::__error() = 0 };
}

impl Iterator for Entries {
    type Item = io::Result<Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            clear_errno();
            // SAFETY: the stream is live and exclusively owned by this iterator.
            let entry = unsafe { libc::readdir(self.stream.as_ptr()) };
            if entry.is_null() {
                let error = io::Error::last_os_error();
                return (error.raw_os_error() != Some(0)).then_some(Err(error));
            }
            // SAFETY: readdir returned a valid entry that lives until the next call.
            let (name, file_type) =
                unsafe { (CStr::from_ptr((*entry).d_name.as_ptr()), (*entry).d_type) };
            let bytes = name.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            let name = OsStr::from_bytes(bytes);
            let kind = match file_type {
                libc::DT_REG => EntryKind::File,
                libc::DT_DIR => EntryKind::Directory,
                libc::DT_LNK => EntryKind::Symlink,
                libc::DT_UNKNOWN => {
                    // SAFETY: the stream's descriptor stays open while the stream lives and
                    // ManuallyDrop leaves ownership with the stream.
                    let directory = std::mem::ManuallyDrop::new(unsafe {
                        File::from_raw_fd(libc::dirfd(self.stream.as_ptr()))
                    });
                    match metadata_at(&directory, name) {
                        Ok(metadata) => metadata.kind,
                        Err(error) => return Some(Err(error)),
                    }
                }
                _ => EntryKind::Other,
            };
            return Some(Ok(Entry {
                name: name.to_owned(),
                kind,
            }));
        }
    }
}

impl Drop for Entries {
    fn drop(&mut self) {
        // SAFETY: the stream is owned by this iterator and closed exactly once.
        unsafe { libc::closedir(self.stream.as_ptr()) };
    }
}

pub fn sync_dir(directory: &File) -> io::Result<()> {
    directory.sync_all()
}

#[allow(
    clippy::useless_conversion,
    clippy::unnecessary_fallible_conversions,
    reason = "statvfs field widths differ between Unix platforms"
)]
pub fn available_bytes(directory: &File) -> io::Result<u64> {
    let mut statistics = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: the descriptor is live and statistics points to writable storage.
    check(unsafe { libc::fstatvfs(directory.as_raw_fd(), statistics.as_mut_ptr()) })?;
    // SAFETY: successful fstatvfs initialized the structure.
    let statistics = unsafe { statistics.assume_init() };
    let invalid = || io::Error::other("invalid available storage measurement");
    let blocks = u64::try_from(statistics.f_bavail).map_err(|_| invalid())?;
    let size = u64::try_from(statistics.f_frsize).map_err(|_| invalid())?;
    if size == 0 {
        return Err(io::Error::other("unknown filesystem allocation unit"));
    }
    blocks.checked_mul(size).ok_or_else(invalid)
}

pub fn is_private(file: &File) -> io::Result<bool> {
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no preconditions.
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        return Ok(false);
    }
    Ok(true)
}

pub fn create_private_dir(path: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new().mode(0o700).create(path)
}
