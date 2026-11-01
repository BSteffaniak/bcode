//! No-follow handle traversal for canonical multi-edit targets.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::{Component, Path};

use bcode_confined_fs::{Creation, Dir, OpenOptions, RenameMode};

/// Open a canonical absolute file without following any symlink components.
///
/// This pins the opened object, not the directory's continuing namespace location.
/// Callers must still bind and recheck authorized identities before publication.
pub fn open_target(path: &Path) -> io::Result<(Dir, File)> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected absolute file path",
        ));
    };
    if !matches!(path.components().next_back(), Some(Component::Normal(_))) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid target component",
        ));
    }
    let parent = Dir::open_strict(parent)?;
    let file = parent.open_file(name, OpenOptions::new().read(true))?;
    Ok((parent, file))
}

/// Open a canonical directory with no-follow traversal, including the final component.
/// Does not open regular files. Like `open_target`, pins the object, not its namespace.
pub fn open_directory(path: &Path) -> io::Result<Dir> {
    Dir::open_strict(path)
}

/// Same-directory staging owned by a pinned directory handle.
/// Drop removes only the private staging name, never the published target.
pub struct StagedFile<'a> {
    parent: &'a Dir,
    name: OsString,
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

/// Read exactly `buffer.len()` bytes at `offset` without moving a shared cursor.
fn read_exact_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_exact_at(file, buffer, offset)
    }
    #[cfg(windows)]
    {
        let mut filled = 0;
        while filled < buffer.len() {
            let read = std::os::windows::fs::FileExt::seek_read(
                file,
                &mut buffer[filled..],
                offset + filled as u64,
            )?;
            if read == 0 {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
            filled += read;
        }
        Ok(())
    }
}

/// Whether staged metadata changed between verification reads. Unix compares mode and
/// change time; Windows compares the attributes and last-write time it exposes.
fn staged_metadata_changed(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    if after.len() != before.len() || after.permissions() != before.permissions() {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        after.mode() != before.mode()
            || after.mtime() != before.mtime()
            || after.mtime_nsec() != before.mtime_nsec()
            || after.ctime() != before.ctime()
            || after.ctime_nsec() != before.ctime_nsec()
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        after.file_attributes() != before.file_attributes()
            || after.last_write_time() != before.last_write_time()
    }
}

impl<'a> StagedFile<'a> {
    pub fn create(
        parent: &'a Dir,
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
            let name = OsString::from(format!(".bcode-edit-{}-{id}", std::process::id()));
            // Exclusive creation never follows a link.
            let file = match parent.open_file(
                &name,
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .creation(Creation::New),
            ) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            };
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

    // A pathname is not ownership. Keep the handle alive so identity reuse cannot
    // make an unrelated replacement appear to be our staging file.
    fn owns_name(&self) -> io::Result<bool> {
        let owned = bcode_confined_fs::file_metadata(&self.file)?;
        let named = self.parent.metadata_at(&self.name)?;
        Ok(named.identity() == owned.identity())
    }

    pub fn publish(
        mut self,
        name: &OsStr,
        expected: &[u8],
        cancelled: &impl Fn() -> bool,
    ) -> io::Result<Publication> {
        let metadata = self.file.metadata()?;
        if !same_permissions(&metadata.permissions(), &self.permissions) {
            return Err(io::Error::other("staging permissions changed"));
        }
        let links = bcode_confined_fs::file_metadata(&self.file)?.link_count();
        if metadata.len() != expected.len() as u64 || links != 1 {
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
            read_exact_at(&self.file, buffer, (index * 8192) as u64)?;
            if buffer != expected_chunk {
                return Err(io::Error::other("staging content changed"));
            }
        }
        let after = self.file.metadata()?;
        if staged_metadata_changed(&metadata, &after)
            || bcode_confined_fs::file_metadata(&self.file)?.link_count() != 1
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
        if let Err(error) = self
            .parent
            .rename(&self.name, self.parent, name, RenameMode::Replace)
        {
            return Ok(Publication::Unknown(error));
        }
        self.published = true;
        Ok(Publication::Committed)
    }
}

/// Compare the permission bits each platform preserves on publication.
fn same_permissions(left: &std::fs::Permissions, right: &std::fs::Permissions) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        left.mode() & 0o7777 == right.mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        left.readonly() == right.readonly()
    }
}

impl Drop for StagedFile<'_> {
    fn drop(&mut self) {
        if !self.published && self.owns_name().unwrap_or(false) {
            // Identity was checked above; external namespace writers can still race this
            // best-effort cleanup.
            let _ = self.parent.remove_file(&self.name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portable_staging_publishes_verified_bytes_and_cleans_abandoned_stages() {
        let root = crate::tests::temp_dir("portable-staging")
            .canonicalize()
            .unwrap();
        let target = root.join("target");
        std::fs::write(&target, b"original").unwrap();
        let permissions = std::fs::metadata(&target).unwrap().permissions();
        let (parent, _) = open_target(&target).unwrap();
        let tampered =
            StagedFile::create(&parent, b"expected", permissions.clone(), &|| false).unwrap();
        std::fs::write(root.join(&tampered.name), b"tampered").unwrap();
        assert!(
            tampered
                .publish(OsStr::new("target"), b"expected", &|| false)
                .is_err()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"original");
        let staged =
            StagedFile::create(&parent, b"new\r\ncontents", permissions, &|| false).unwrap();
        assert!(matches!(
            staged
                .publish(OsStr::new("target"), b"new\r\ncontents", &|| false)
                .unwrap(),
            Publication::Committed
        ));
        assert_eq!(std::fs::read(&target).unwrap(), b"new\r\ncontents");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        assert!(open_target(Path::new("relative")).is_err());
        assert!(open_directory(&root).unwrap().metadata().is_ok());
        std::fs::remove_file(target).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[cfg(unix)]
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

    #[cfg(unix)]
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
                let path = root.join(&staged.name);
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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
        let staging_path = root.join(&staged.name);
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

    #[cfg(unix)]
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
        assert!(parent.as_file().metadata().unwrap().is_dir());
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
