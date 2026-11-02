//! Handle-relative, no-follow filesystem access for confining untrusted names.
//!
//! A [`Dir`] retains an open directory handle. Every operation resolves exactly one
//! name component relative to that handle and never follows a link in that
//! component, so renaming or substituting the directory's ancestors after it was
//! opened cannot redirect an operation outside of it. This pins the opened directory
//! object, not its continuing location in the namespace; callers that need to bind a
//! location must still compare [`FileIdentity`] values.
//!
//! Unix uses `openat`-family calls with `O_NOFOLLOW`. Windows uses `NtCreateFile`
//! relative to the retained handle with `FILE_OPEN_REPARSE_POINT`. On Windows, only
//! name-surrogate reparse points (symbolic links, junctions, and mount points) are
//! treated as links; other reparse points, such as cloud-file placeholders, are
//! ordinary files and directories.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::Path;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as sys;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as sys;

#[cfg(test)]
mod tests;

/// The type of a directory entry, observed without following links.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// A symbolic link, or on Windows a name-surrogate reparse point.
    Symlink,
    /// Any other object, such as a device, FIFO, or socket.
    Other,
}

/// Stable identity of a filesystem object while it exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileIdentity {
    volume: u64,
    file: u128,
}

impl FileIdentity {
    /// Returns the device or volume serial number containing the object.
    #[must_use]
    pub const fn volume(&self) -> u64 {
        self.volume
    }

    /// Returns the inode or file ID within its volume.
    #[must_use]
    pub const fn file(&self) -> u128 {
        self.file
    }
}

/// A file timestamp for change detection on this host. Values are comparable with each
/// other but have a platform-defined epoch and resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Timestamp(i128);

/// Metadata observed through a handle or without following a final link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metadata {
    kind: EntryKind,
    len: u64,
    link_count: u64,
    identity: FileIdentity,
    modified: Timestamp,
    changed: Timestamp,
}

impl Metadata {
    /// Returns the entry type.
    #[must_use]
    pub const fn kind(&self) -> EntryKind {
        self.kind
    }

    /// Returns the size in bytes.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.len
    }

    /// Returns whether the size is zero.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the number of hard links to the object.
    #[must_use]
    pub const fn link_count(&self) -> u64 {
        self.link_count
    }

    /// Returns the object's identity.
    #[must_use]
    pub const fn identity(&self) -> FileIdentity {
        self.identity
    }

    /// Returns when the contents were last written.
    #[must_use]
    pub const fn modified(&self) -> Timestamp {
        self.modified
    }

    /// Returns when the contents or metadata last changed (Unix `ctime`, Windows
    /// `ChangeTime`).
    #[must_use]
    pub const fn changed(&self) -> Timestamp {
        self.changed
    }
}

/// Whether opening a file may create it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Creation {
    /// Open only an existing file.
    #[default]
    Existing,
    /// Open an existing file or create a new one.
    IfMissing,
    /// Create a new file, failing with [`io::ErrorKind::AlreadyExists`] if one exists.
    New,
}

/// Options for [`Dir::open_file`]. Files are always opened without following links
/// and must be regular files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "mirrors the independent access flags of std::fs::OpenOptions"
)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    private: bool,
    creation: Creation,
}

impl OpenOptions {
    /// Creates options that open nothing until access is selected.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            read: false,
            write: false,
            append: false,
            truncate: false,
            private: false,
            creation: Creation::Existing,
        }
    }

    /// Sets read access.
    #[must_use]
    pub const fn read(mut self, read: bool) -> Self {
        self.read = read;
        self
    }

    /// Sets write access.
    #[must_use]
    pub const fn write(mut self, write: bool) -> Self {
        self.write = write;
        self
    }

    /// Sets append-only write access.
    #[must_use]
    pub const fn append(mut self, append: bool) -> Self {
        self.append = append;
        self
    }

    /// Truncates an existing file to zero length. Requires write access.
    #[must_use]
    pub const fn truncate(mut self, truncate: bool) -> Self {
        self.truncate = truncate;
        self
    }

    /// Creates new files accessible only to the current user and the platform's
    /// administrators. Unix always creates files owner-only; on Windows, files otherwise
    /// inherit their directory's access control. Existing files are unaffected.
    #[must_use]
    pub const fn private(mut self, private: bool) -> Self {
        self.private = private;
        self
    }

    /// Selects whether the open may create the file.
    #[must_use]
    pub const fn creation(mut self, creation: Creation) -> Self {
        self.creation = creation;
        self
    }

    fn validate(self) -> io::Result<Self> {
        // Read-only opens cannot modify or create; append cannot be combined with truncation.
        let valid = if self.write || self.append {
            !(self.append && self.truncate)
        } else {
            self.read && !self.truncate && self.creation == Creation::Existing
        };
        if !valid {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid confined open options",
            ));
        }
        Ok(self)
    }
}

/// Whether a rename may replace an existing destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameMode {
    /// Atomically replace an existing destination file.
    Replace,
    /// Fail with [`io::ErrorKind::AlreadyExists`] if the destination exists.
    NoReplace,
}

/// One entry returned by [`Dir::entries`]. `.` and `..` are never returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    name: OsString,
    kind: EntryKind,
}

impl Entry {
    /// Returns the entry name, a single path component.
    #[must_use]
    pub fn name(&self) -> &OsStr {
        &self.name
    }

    /// Consumes the entry and returns its name.
    #[must_use]
    pub fn into_name(self) -> OsString {
        self.name
    }

    /// Returns the entry type, observed without following links.
    #[must_use]
    pub const fn kind(&self) -> EntryKind {
        self.kind
    }
}

/// Incremental directory enumeration. Callers bound their own work by how many
/// entries they consume.
pub struct Entries(sys::Entries);

impl Iterator for Entries {
    type Item = io::Result<Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

/// A retained directory handle that confines operations to single names within it.
#[derive(Debug)]
pub struct Dir {
    file: File,
}

impl Dir {
    /// Opens a directory without following a link in its final component.
    /// Ancestor components are resolved normally and must be controlled by the caller.
    ///
    /// # Errors
    ///
    /// * The path does not exist or cannot be opened.
    /// * The final component is a link or is not a directory.
    pub fn open(path: &Path) -> io::Result<Self> {
        sys::open_dir_path(path).map(|file| Self { file })
    }

    /// Opens a trusted root directory, following links in every component. Use only
    /// for caller-controlled roots; confine untrusted names beneath it with [`Self::open_dir`]
    /// and [`Self::open_file`].
    ///
    /// # Errors
    ///
    /// Returns an error if the path cannot be opened or is not a directory.
    pub fn open_following_links(path: &Path) -> io::Result<Self> {
        sys::open_dir_path_following(path).map(|file| Self { file })
    }

    /// Opens an absolute directory path, refusing links in every component.
    ///
    /// # Errors
    ///
    /// * The path is relative or contains `.`/`..` components.
    /// * Any component is missing, is a link, or is not a directory.
    pub fn open_strict(path: &Path) -> io::Result<Self> {
        sys::open_dir_path_strict(path).map(|file| Self { file })
    }

    /// Opens a child directory without following links.
    ///
    /// # Errors
    ///
    /// * The name is not a single valid component.
    /// * The entry is missing, is a link, or is not a directory.
    pub fn open_dir(&self, name: &OsStr) -> io::Result<Self> {
        validate_name(name)?;
        sys::open_dir_at(&self.file, name).map(|file| Self { file })
    }

    /// Opens a child regular file without following links.
    ///
    /// # Errors
    ///
    /// * The options or name are invalid.
    /// * The entry is missing (unless created), already exists for [`Creation::New`],
    ///   is a link, or is not a regular file.
    pub fn open_file(&self, name: &OsStr, options: OpenOptions) -> io::Result<File> {
        validate_name(name)?;
        sys::open_file_at(&self.file, name, options.validate()?)
    }

    /// Returns this directory's metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the handle cannot be queried.
    pub fn metadata(&self) -> io::Result<Metadata> {
        sys::file_metadata(&self.file)
    }

    /// Returns a child entry's metadata without following a link.
    ///
    /// # Errors
    ///
    /// * The name is not a single valid component.
    /// * The entry is missing or cannot be queried.
    pub fn metadata_at(&self, name: &OsStr) -> io::Result<Metadata> {
        validate_name(name)?;
        sys::metadata_at(&self.file, name)
    }

    /// Creates a child directory accessible only to its owner on Unix. On Windows
    /// the new directory inherits its parent's access control.
    ///
    /// # Errors
    ///
    /// * The name is not a single valid component.
    /// * The entry already exists or cannot be created.
    pub fn create_dir(&self, name: &OsStr) -> io::Result<()> {
        validate_name(name)?;
        sys::create_dir_at(&self.file, name)
    }

    /// Creates a child directory accessible only to the current user and the
    /// platform's administrators. Files created inside it inherit that restriction
    /// on Windows.
    ///
    /// # Errors
    ///
    /// * The name is not a single valid component.
    /// * The entry already exists, or the directory or its access control cannot be
    ///   created.
    pub fn create_private_dir(&self, name: &OsStr) -> io::Result<()> {
        validate_name(name)?;
        sys::create_private_dir_at(&self.file, name)
    }

    /// Removes a child file or link without following it.
    ///
    /// # Errors
    ///
    /// * The name is not a single valid component.
    /// * The entry is missing, is a directory, or cannot be removed.
    pub fn remove_file(&self, name: &OsStr) -> io::Result<()> {
        validate_name(name)?;
        sys::remove_file_at(&self.file, name)
    }

    /// Removes an empty child directory without following links.
    ///
    /// # Errors
    ///
    /// * The name is not a single valid component.
    /// * The entry is missing, is not a directory, is not empty, or cannot be removed.
    pub fn remove_dir(&self, name: &OsStr) -> io::Result<()> {
        validate_name(name)?;
        sys::remove_dir_at(&self.file, name)
    }

    /// Renames a child entry, without following links, to a name in `destination`.
    ///
    /// # Errors
    ///
    /// * Either name is not a single valid component.
    /// * The source is missing or cannot be renamed.
    /// * The destination exists for [`RenameMode::NoReplace`].
    /// * No-replace renames are unsupported by this platform.
    pub fn rename(
        &self,
        from: &OsStr,
        destination: &Self,
        to: &OsStr,
        mode: RenameMode,
    ) -> io::Result<()> {
        validate_name(from)?;
        validate_name(to)?;
        sys::rename_at(&self.file, from, &destination.file, to, mode)
    }

    /// Creates a hard link in `destination` to a child file, without following links.
    ///
    /// # Errors
    ///
    /// * Either name is not a single valid component.
    /// * The source is missing, the destination exists, or linking is unsupported.
    pub fn hard_link(&self, from: &OsStr, destination: &Self, to: &OsStr) -> io::Result<()> {
        validate_name(from)?;
        validate_name(to)?;
        sys::hard_link_at(&self.file, from, &destination.file, to)
    }

    /// Enumerates this directory from its beginning.
    ///
    /// # Errors
    ///
    /// Returns an error if enumeration cannot start. Per-entry failures are yielded.
    pub fn entries(&self) -> io::Result<Entries> {
        sys::entries(&self.file).map(Entries)
    }

    /// Durably records prior changes to this directory's entries where the platform
    /// supports it. Windows file systems journal entry metadata, so this is a no-op.
    ///
    /// # Errors
    ///
    /// Returns an error if the platform synchronization fails.
    pub fn sync(&self) -> io::Result<()> {
        sys::sync_dir(&self.file)
    }

    /// Returns the bytes available to the current user on this directory's volume.
    ///
    /// # Errors
    ///
    /// Returns an error if the volume cannot be queried or reports an invalid size.
    pub fn available_bytes(&self) -> io::Result<u64> {
        sys::available_bytes(&self.file)
    }

    /// Returns whether only the current user (and the platform's administrators)
    /// can access this directory.
    ///
    /// # Errors
    ///
    /// Returns an error if ownership or access control cannot be read.
    pub fn is_private(&self) -> io::Result<bool> {
        sys::is_private(&self.file)
    }

    /// Returns another handle to the same directory.
    ///
    /// # Errors
    ///
    /// Returns an error if the handle cannot be duplicated.
    pub fn try_clone(&self) -> io::Result<Self> {
        self.file.try_clone().map(|file| Self { file })
    }

    /// Borrows the underlying directory handle.
    #[must_use]
    pub const fn as_file(&self) -> &File {
        &self.file
    }

    /// Consumes this value and returns the underlying directory handle.
    #[must_use]
    pub fn into_file(self) -> File {
        self.file
    }
}

/// Opens a regular file by path without following a link in its final component.
/// Ancestor components are resolved normally and must be controlled by the caller.
///
/// # Errors
///
/// * The options are invalid.
/// * The file is missing (unless created), already exists for [`Creation::New`], is a
///   link, or is not a regular file.
pub fn open_file_path(path: &Path, options: OpenOptions) -> io::Result<File> {
    sys::open_file_path(path, options.validate()?)
}

/// Durably records prior changes to the entries of the directory at `path`, following
/// links. Windows file systems journal entry metadata, so this is a no-op there.
///
/// # Errors
///
/// Returns an error if the directory cannot be opened or synchronized.
pub fn sync_directory_path(path: &Path) -> io::Result<()> {
    sys::sync_dir_path(path)
}

/// Atomically publishes a fully written temporary file, replacing `destination`.
///
/// Use instead of [`tempfile::NamedTempFile::persist`], which on Windows uses
/// `MoveFileExW` alone and fails while another process holds `destination` open with
/// delete sharing. This falls back to POSIX replacement semantics there, and is an
/// ordinary `rename` elsewhere. Returns the published file's handle.
///
/// # Errors
///
/// Returns an error if the file cannot be published; the temporary file is then removed.
pub fn persist(temporary: tempfile::NamedTempFile, destination: &Path) -> io::Result<File> {
    let (file, path) = temporary.into_parts();
    // Disarm cleanup first; on Windows this also clears the temporary-file attribute so
    // the published file has ordinary caching and backup semantics.
    let path = path.keep().map_err(|error| error.error)?;
    if let Err(error) = std::fs::rename(&path, destination) {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(error);
    }
    Ok(file)
}

/// Returns metadata for an open file or directory handle.
///
/// # Errors
///
/// Returns an error if the handle cannot be queried.
pub fn file_metadata(file: &File) -> io::Result<Metadata> {
    sys::file_metadata(file)
}

/// Returns the bytes available to the current user on the volume of an open file or
/// directory handle. The handle pins the volume, avoiding path re-resolution.
///
/// # Errors
///
/// Returns an error if the volume cannot be queried or reports an invalid size.
pub fn available_bytes(file: &File) -> io::Result<u64> {
    sys::available_bytes(file)
}

/// Returns whether only the current user (and the platform's administrators) can
/// access an open file or directory.
///
/// # Errors
///
/// Returns an error if ownership or access control cannot be read.
pub fn is_private(file: &File) -> io::Result<bool> {
    sys::is_private(file)
}

/// Creates a new directory accessible only to the current user and the platform's
/// administrators. Files created inside it inherit that restriction on Windows.
///
/// # Errors
///
/// * The path exists or its parent is missing.
/// * The directory or its access control cannot be created.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    sys::create_private_dir(path)
}

fn validate_name(name: &OsStr) -> io::Result<()> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a single confined path component",
        )
    };
    let Some(text) = name.to_str() else {
        return sys::validate_non_utf8_name(name).map_err(|()| invalid());
    };
    let separator = |character: char| {
        character == '/' || character == '\0' || (cfg!(windows) && "\\:".contains(character))
    };
    // Windows path normalization strips trailing dots and spaces, so such names would
    // be unreachable through ordinary paths.
    let trailing = cfg!(windows) && (text.ends_with('.') || text.ends_with(' '));
    if text.is_empty() || text == "." || text == ".." || text.contains(separator) || trailing {
        return Err(invalid());
    }
    Ok(())
}
