//! No-follow descriptor traversal for canonical multi-edit targets.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Component, Path};

/// Open a canonical absolute file without following any symlink components.
///
/// This pins the opened object, not the directory's continuing namespace location.
/// Callers must still bind and recheck authorized identities before publication.
pub fn open_target(path: &Path) -> io::Result<(File, File)> {
    let mut components = path.components().peekable();
    if components.next() != Some(Component::RootDir) || components.peek().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected absolute file path",
        ));
    }
    let mut handle = File::open("/")?;
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid target component",
            ));
        };
        let name = CString::new(name.as_bytes())?;
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if components.peek().is_some() {
                libc::O_DIRECTORY
            } else {
                0
            };
        // SAFETY: handle owns the directory descriptor throughout the call and
        // name is a valid NUL-terminated string. No creation flags are supplied.
        let descriptor = unsafe { libc::openat(handle.as_raw_fd(), name.as_ptr(), flags) };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat returned a fresh owned descriptor, transferred once.
        let child = unsafe { File::from_raw_fd(descriptor) };
        if components.peek().is_none() {
            return Ok((handle, child));
        }
        handle = child;
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "missing target component",
    ))
}

/// Same-directory staging owned by a pinned directory handle.
/// Drop removes only the private staging name, never the published target.
pub struct StagedFile<'a> {
    parent: &'a File,
    name: CString,
    file: File,
    permissions: std::fs::Permissions,
    published: bool,
}

/// Result after a rename was attempted. An error may be ambiguous on remote filesystems.
#[derive(Debug)]
pub enum Publication {
    Committed,
    Unknown(io::Error),
}

impl<'a> StagedFile<'a> {
    pub fn create(
        parent: &'a File,
        bytes: &[u8],
        permissions: std::fs::Permissions,
        cancelled: &impl Fn() -> bool,
    ) -> io::Result<Self> {
        use std::io::Write as _;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        for _ in 0..128 {
            if cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "staging cancelled",
                ));
            }
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let name = CString::new(format!(".bcode-edit-{}-{id}", std::process::id()))?;
            // SAFETY: parent and name remain valid; exclusive creation never follows a link.
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDWR
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if fd < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::AlreadyExists {
                    continue;
                }
                return Err(error);
            }
            // SAFETY: fresh owned descriptor from openat.
            let file = unsafe { File::from_raw_fd(fd) };
            let mut staged = Self {
                parent,
                name,
                file,
                permissions: permissions.clone(),
                published: false,
            };
            for chunk in bytes.chunks(8192) {
                if cancelled() {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "staging cancelled",
                    ));
                }
                staged.file.write_all(chunk)?;
            }
            staged.file.set_permissions(permissions)?;
            staged.file.sync_all()?;
            return Ok(staged);
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "staging name budget exhausted",
        ))
    }

    // A pathname is not ownership. Keep the descriptor alive so inode reuse cannot
    // make an unrelated replacement appear to be our staging file.
    fn owns_name(&self) -> io::Result<bool> {
        use std::os::unix::fs::MetadataExt as _;
        let owned = self.file.metadata()?;
        let mut named = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: parent/name are live and named points to writable stat storage.
        if unsafe {
            libc::fstatat(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                named.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful fstatat initialized the stat value.
        let named = unsafe { named.assume_init() };
        Ok(i128::from(named.st_dev) == i128::from(owned.dev()) && named.st_ino == owned.ino())
    }

    pub fn publish(
        mut self,
        name: &std::ffi::OsStr,
        expected: &[u8],
        cancelled: &impl Fn() -> bool,
    ) -> io::Result<Publication> {
        use std::os::unix::fs::{FileExt as _, MetadataExt as _, PermissionsExt as _};
        let metadata = self.file.metadata()?;
        if metadata.mode() & 0o7777 != self.permissions.mode() & 0o7777 {
            return Err(io::Error::other("staging permissions changed"));
        }
        if metadata.len() != expected.len() as u64 || metadata.nlink() != 1 {
            return Err(io::Error::other("staging size or links changed"));
        }
        let mut chunk = [0u8; 8192];
        for (index, expected_chunk) in expected.chunks(chunk.len()).enumerate() {
            if cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "publication cancelled",
                ));
            }
            let buffer = &mut chunk[..expected_chunk.len()];
            self.file.read_exact_at(buffer, (index * 8192) as u64)?;
            if buffer != expected_chunk {
                return Err(io::Error::other("staging content changed"));
            }
        }
        let after = self.file.metadata()?;
        if after.len() != metadata.len()
            || after.nlink() != 1
            || after.mode() != metadata.mode()
            || after.mtime() != metadata.mtime()
            || after.mtime_nsec() != metadata.mtime_nsec()
            || after.ctime() != metadata.ctime()
            || after.ctime_nsec() != metadata.ctime_nsec()
        {
            return Err(io::Error::other(
                "staging metadata changed during verification",
            ));
        }
        if cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "publication cancelled",
            ));
        }
        if !self.owns_name()? {
            return Err(io::Error::other("staging file identity changed"));
        }
        if Path::new(name).components().count() != 1
            || !matches!(
                Path::new(name).components().next(),
                Some(Component::Normal(_))
            )
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid publication leaf",
            ));
        }
        let name = CString::new(name.as_bytes())?;
        // SAFETY: both names and the directory descriptor remain live through renameat.
        if unsafe {
            libc::renameat(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                self.parent.as_raw_fd(),
                name.as_ptr(),
            )
        } != 0
        {
            return Ok(Publication::Unknown(io::Error::last_os_error()));
        }
        self.published = true;
        Ok(Publication::Committed)
    }
}

impl Drop for StagedFile<'_> {
    fn drop(&mut self) {
        if !self.published && self.owns_name().unwrap_or(false) {
            // SAFETY: parent and name remain live. Identity was checked above;
            // external namespace writers can still race this best-effort cleanup.
            unsafe {
                libc::unlinkat(self.parent.as_raw_fd(), self.name.as_ptr(), 0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_publication_preserves_bytes_and_permissions_and_cleans_abandoned_stage() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = crate::tests::temp_dir("staged-publication")
            .canonicalize()
            .unwrap();
        let target = root.join("target");
        std::fs::write(&target, "original").unwrap();
        let (parent, _) = open_target(&target).unwrap();
        let bytes = b"\xef\xbb\xbfnew\r\nlast";
        let staged = StagedFile::create(
            &parent,
            bytes,
            std::fs::Permissions::from_mode(0o640),
            &|| false,
        )
        .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
        staged
            .publish(std::ffi::OsStr::new("target"), bytes, &|| false)
            .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), bytes);
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        drop(
            StagedFile::create(
                &parent,
                b"abandoned",
                std::fs::Permissions::from_mode(0o600),
                &|| false,
            )
            .unwrap(),
        );
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_file(target).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn staging_content_tampering_and_cancellation_leave_target_unchanged() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = crate::tests::temp_dir("staging-content")
            .canonicalize()
            .unwrap();
        let target = root.join("target");
        std::fs::write(&target, b"original").unwrap();
        let (parent, _) = open_target(&target).unwrap();
        for cancel in [false, true] {
            let staged = StagedFile::create(
                &parent,
                b"expected",
                std::fs::Permissions::from_mode(0o600),
                &|| false,
            )
            .unwrap();
            if !cancel {
                let path = root.join(std::ffi::OsStr::from_bytes(staged.name.as_bytes()));
                std::fs::write(path, b"tampered").unwrap();
            }
            assert!(
                staged
                    .publish(std::ffi::OsStr::new("target"), b"expected", &|| cancel)
                    .is_err()
            );
            assert_eq!(std::fs::read(&target).unwrap(), b"original");
            assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        }
        std::fs::remove_file(target).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn staging_permission_tampering_is_rejected_before_publication() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = crate::tests::temp_dir("staging-permissions")
            .canonicalize()
            .unwrap();
        let target = root.join("target");
        std::fs::write(&target, b"original").unwrap();
        let original_permissions = std::fs::metadata(&target).unwrap().permissions();
        let (parent, _) = open_target(&target).unwrap();
        let staged = StagedFile::create(
            &parent,
            b"expected",
            std::fs::Permissions::from_mode(0o600),
            &|| false,
        )
        .unwrap();
        staged
            .file
            .set_permissions(std::fs::Permissions::from_mode(0o644))
            .unwrap();
        let error = staged
            .publish(std::ffi::OsStr::new("target"), b"expected", &|| false)
            .unwrap_err();
        assert_eq!(error.to_string(), "staging permissions changed");
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions(),
            original_permissions
        );
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_file(target).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn rename_error_reports_uncertainty_and_cleans_stage() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = crate::tests::temp_dir("rename-uncertainty")
            .canonicalize()
            .unwrap();
        let target = root.join("target");
        std::fs::write(&target, b"original").unwrap();
        let (parent, _) = open_target(&target).unwrap();
        let destination = root.join("directory");
        std::fs::create_dir(&destination).unwrap();
        let staged = StagedFile::create(
            &parent,
            b"expected",
            std::fs::Permissions::from_mode(0o600),
            &|| false,
        )
        .unwrap();
        assert!(matches!(
            staged
                .publish(std::ffi::OsStr::new("directory"), b"expected", &|| false)
                .unwrap(),
            Publication::Unknown(_)
        ));
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
        std::fs::remove_file(target).unwrap();
        std::fs::remove_dir(destination).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn replaced_staging_name_is_neither_published_nor_cleaned_up() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = crate::tests::temp_dir("batch-staging-replaced")
            .canonicalize()
            .unwrap();
        let target = root.join("target");
        std::fs::write(&target, b"original").unwrap();
        let (parent, _) = open_target(&target).unwrap();
        let staged = StagedFile::create(
            &parent,
            b"replacement",
            std::fs::Permissions::from_mode(0o600),
            &|| false,
        )
        .unwrap();
        let staging_path = root.join(std::ffi::OsStr::from_bytes(staged.name.as_bytes()));
        std::fs::remove_file(&staging_path).unwrap();
        std::fs::write(&staging_path, b"foreign").unwrap();
        assert!(
            staged
                .publish(std::ffi::OsStr::new("target"), b"replacement", &|| false)
                .is_err()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
        assert_eq!(std::fs::read(&staging_path).unwrap(), b"foreign");
        std::fs::remove_file(staging_path).unwrap();
        std::fs::remove_file(target).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn rejects_symlinks_in_parent_and_leaf_without_following_them() {
        let root = crate::tests::temp_dir("batch-confined")
            .canonicalize()
            .unwrap();
        let directory = root.join("directory");
        std::fs::create_dir(&directory).unwrap();
        let target = directory.join("target");
        std::fs::write(&target, "original").unwrap();
        let (parent, file) = open_target(&target).unwrap();
        assert!(file.metadata().unwrap().is_file());
        assert!(parent.metadata().unwrap().is_dir());
        let parent_alias = root.join("parent-alias");
        let leaf_alias = root.join("leaf-alias");
        std::os::unix::fs::symlink(&directory, &parent_alias).unwrap();
        std::os::unix::fs::symlink(&target, &leaf_alias).unwrap();
        assert!(open_target(&parent_alias.join("target")).is_err());
        assert!(open_target(&leaf_alias).is_err());
        assert!(open_target(Path::new("relative")).is_err());
        assert!(open_target(&directory.join("../directory/target")).is_err());
        std::fs::remove_file(parent_alias).unwrap();
        std::fs::remove_file(leaf_alias).unwrap();
        std::fs::remove_file(target).unwrap();
        std::fs::remove_dir(directory).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
