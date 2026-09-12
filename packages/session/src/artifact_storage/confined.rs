//! Descriptor-relative artifact opens. Never follows tool-controlled symlink components.

use std::ffi::{CStr, CString, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, IntoRawFd as _};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Component, Path};

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "invalid confined artifact path",
    )
}

pub fn open_child(parent: &File, name: &CStr, directory: bool) -> io::Result<File> {
    let flags = libc::O_RDONLY
        | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: name is NUL-terminated and parent remains owned for the entire syscall.
    let descriptor = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a fresh descriptor transferred exactly once into File.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

pub fn open_relative(root: &Path, relative: &Path) -> io::Result<File> {
    let mut directory = File::open(root)?;
    if !directory.metadata()?.is_dir() {
        return Err(invalid());
    }
    let mut parts = relative.components().peekable();
    if parts.peek().is_none() {
        return Err(invalid());
    }
    while let Some(part) = parts.next() {
        let Component::Normal(name) = part else {
            return Err(invalid());
        };
        let name = CString::new(name.as_bytes()).map_err(|_| invalid())?;
        directory = open_child(&directory, &name, parts.peek().is_some())?;
    }
    Ok(directory)
}

struct Directory(*mut libc::DIR);
impl Drop for Directory {
    fn drop(&mut self) {
        // SAFETY: this guard owns the live DIR and closes it exactly once.
        unsafe {
            libc::closedir(self.0);
        }
    }
}

/// Read at most two container entry names; directory handles pin the object during enumeration.
pub fn container_names(directory: &File) -> io::Result<Vec<OsString>> {
    // Open a fresh description rather than dup: directory offsets must not be shared with callers.
    let descriptor = open_child(directory, c".", true)?;
    let raw = descriptor.into_raw_fd();
    // SAFETY: raw is an owned directory descriptor; fdopendir assumes ownership only on success.
    let stream = unsafe { libc::fdopendir(raw) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: fdopendir failed, so raw still belongs to this function.
        drop(unsafe { File::from_raw_fd(raw) });
        return Err(error);
    }
    let stream = Directory(stream);
    let mut names = Vec::new();
    // The enclosing reader rejects missing or extra payload names.
    loop {
        // SAFETY: the DIR is live, exclusively owned and not shared between threads.
        let result = unsafe { libc::readdir(stream.0) };
        if result.is_null() {
            break;
        }
        // SAFETY: successful readdir returned a live entry and NUL-terminated d_name.
        let name = unsafe { CStr::from_ptr((*result).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        names.push(OsString::from_vec(name.to_vec()));
        if names.len() == 2 {
            break;
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_traversal_rejects_symlinks_in_every_component() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().expect("root");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret"), b"private").expect("secret");
        symlink(outside.path(), root.path().join("escape")).expect("directory link");
        symlink(outside.path().join("secret"), root.path().join("file")).expect("file link");
        assert!(open_relative(root.path(), Path::new("escape/secret")).is_err());
        assert!(open_relative(root.path(), Path::new("file")).is_err());
        assert!(open_relative(root.path(), Path::new("../secret")).is_err());
        std::fs::create_dir(root.path().join("container")).expect("container");
        let directory = open_relative(root.path(), Path::new("container")).expect("directory");
        symlink(
            outside.path().join("secret"),
            root.path().join("container/content.v1.zstd"),
        )
        .expect("payload link");
        assert!(open_child(&directory, c"content.v1.zstd", false).is_err());
    }
}
