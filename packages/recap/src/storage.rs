//! Bounded JSON document storage with optimistic revisions and atomic replacement.
//!
//! The caller supplies a private, application-owned root, not a directory writable
//! by untrusted processes. Writers must use this store's lock protocol. Asset reads
//! reject symlinks and verify canonical confinement, but cannot defend against a
//! hostile process concurrently renaming entries in that private root using only
//! portable standard-library filesystem APIs.
//!
//! Documents live at `<root>/<id>.json`, media at `<root>/<id>.assets/`.
//! An interrupted writer may leave `<id>.lock`. Recovery is explicit: verify that
//! the writer is no longer active before removing the lock; reads never repair it.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::contracts::{Asset, Error, RecapDocument, Result, validate_id};

/// Persistence for independent recap documents. Clones share the same on-disk locks.
#[derive(Clone, Debug)]
pub struct DocumentStore {
    root: PathBuf,
    max_document_bytes: u64,
}

impl DocumentStore {
    /// Configure a store without accessing or creating the filesystem.
    /// The byte budget applies to both serialized documents and document reads.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>, max_document_bytes: u64) -> Self {
        Self {
            root: root.into(),
            max_document_bytes,
        }
    }

    /// Return the owned media directory for an ID. This does not create it or
    /// authorize access; media producers must preserve the private-root boundary.
    ///
    /// # Errors
    /// Returns a validation error for an unsafe document identity.
    pub fn asset_directory(&self, id: &str) -> Result<PathBuf> {
        validate_id(id)?;
        Ok(self.root.join(format!("{id}.assets")))
    }

    /// Load a bounded, compatible, fully validated document without mutation.
    ///
    /// # Errors
    /// Returns I/O errors (including not-found), byte-limit errors, unsafe-path,
    /// unsupported-version, malformed-JSON, and document validation errors.
    pub fn load(&self, id: &str) -> Result<RecapDocument> {
        validate_id(id)?;
        let root = self.existing_root()?;
        let path = root.join(format!("{id}.json"));
        let bytes = read_owned_file(&root, &path, self.max_document_bytes)?;
        let document = RecapDocument::from_json_slice(&bytes)?;
        if document.id != id {
            return Err(Error::InvalidDocument(
                "stored document identity does not match its location".into(),
            ));
        }
        Ok(document)
    }

    /// Atomically create or update a document under a cross-process writer lock.
    /// `None` is create-only and requires revision 1; `Some(n)` requires stored
    /// revision `n` and document revision `n + 1`. Invalid/future existing content
    /// is never overwritten. Assets are not copied or modified by this operation.
    ///
    /// The temporary file and containing directory are synchronized. If rename
    /// succeeds but directory synchronization fails, [`Error::DurabilityUncertain`]
    /// indicates a possibly committed operation: reload before retrying.
    ///
    /// # Errors
    /// Returns validation, serialization, byte-limit, lock, filesystem, revision
    /// conflict, or durability-uncertain errors. Overflowing revisions are invalid.
    pub fn save(&self, document: &RecapDocument, expected_revision: Option<u64>) -> Result<()> {
        document.validate()?;
        let next = expected_revision.map_or(Some(1), |revision| revision.checked_add(1));
        if next != Some(document.revision) {
            return Err(Error::InvalidDocument(
                "save requires the next consecutive revision".into(),
            ));
        }
        let bytes = serde_json::to_vec_pretty(document)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > self.max_document_bytes {
            return Err(Error::TooLarge {
                limit: self.max_document_bytes,
            });
        }
        fs::create_dir_all(&self.root)?;
        let root = self.existing_root()?;
        let _lock = WriterLock::acquire(root.join(format!("{}.lock", document.id)))?;
        let actual = match self.load(&document.id) {
            Ok(current) => Some(current.revision),
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        if actual != expected_revision {
            return Err(Error::RevisionConflict {
                expected: expected_revision,
                actual,
            });
        }
        let destination = root.join(format!("{}.json", document.id));
        let mut temporary = tempfile::NamedTempFile::new_in(&root)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(destination)
            .map_err(|error| Error::Io(error.error))?;
        File::open(&root)
            .and_then(|directory| directory.sync_all())
            .map_err(Error::DurabilityUncertain)?;
        Ok(())
    }

    /// Read an asset registered in the current persisted document, with an
    /// independent byte budget. No URLs are fetched and no paths are repaired.
    /// The asset must exactly match the persisted metadata, including provenance.
    ///
    /// # Errors
    /// Returns document load errors, an invalid reference error, confinement or
    /// symlink errors, I/O errors, or [`Error::TooLarge`].
    pub fn read_asset(&self, document_id: &str, asset: &Asset, max_bytes: u64) -> Result<Vec<u8>> {
        let document = self.load(document_id)?;
        if !document.assets.iter().any(|registered| registered == asset) {
            return Err(Error::InvalidDocument(
                "asset is not registered in the current document".into(),
            ));
        }
        let root = self.existing_root()?;
        let asset_root = root.join(format!("{document_id}.assets"));
        let metadata = fs::symlink_metadata(&asset_root)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(Error::UnsafeStoragePath);
        }
        let asset_root = asset_root.canonicalize()?;
        if !asset_root.starts_with(&root) {
            return Err(Error::UnsafeStoragePath);
        }
        let path = asset_root.join(asset.path.as_str());
        read_owned_file(&asset_root, &path, max_bytes)
    }

    fn existing_root(&self) -> Result<PathBuf> {
        let metadata = fs::symlink_metadata(&self.root)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(Error::UnsafeStoragePath);
        }
        Ok(self.root.canonicalize()?)
    }
}

fn read_owned_file(root: &Path, path: &Path, max_bytes: u64) -> Result<Vec<u8>> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| Error::UnsafeStoragePath)?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(Error::UnsafeStoragePath);
        }
        current.push(component);
        if fs::symlink_metadata(&current)?.file_type().is_symlink() {
            return Err(Error::UnsafeStoragePath);
        }
    }
    // Check before opening too: opening a FIFO can otherwise block indefinitely.
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(Error::UnsafeStoragePath);
    }
    let canonical = path.canonicalize()?;
    if !canonical.starts_with(root) {
        return Err(Error::UnsafeStoragePath);
    }
    let file = File::open(canonical)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(Error::UnsafeStoragePath);
    }
    if metadata.len() > max_bytes {
        return Err(Error::TooLarge { limit: max_bytes });
    }
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
        return Err(Error::TooLarge { limit: max_bytes });
    }
    Ok(bytes)
}

struct WriterLock {
    path: PathBuf,
    // Keep the handle alive until this writer finishes, including on Windows.
    file: Option<File>,
}

impl WriterLock {
    fn acquire(path: PathBuf) -> Result<Self> {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => Ok(Self {
                path,
                file: Some(file),
            }),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Err(Error::Locked),
            Err(error) => Err(Error::Io(error)),
        }
    }
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        drop(self.file.take());
        // Failure leaves a conservative, explicit-recovery lock rather than
        // pretending another writer may safely proceed.
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::tests::document;

    #[test]
    fn create_update_and_conflict_preserve_committed_content() {
        let directory = tempfile::tempdir().unwrap();
        let store = DocumentStore::new(directory.path(), 100_000);
        let mut value = document();
        store.save(&value, None).unwrap();
        assert_eq!(store.load(&value.id).unwrap(), value);
        assert!(matches!(
            store.save(&value, None),
            Err(Error::RevisionConflict {
                actual: Some(1),
                ..
            })
        ));
        value.revision = 2;
        value.title = "Refined".into();
        store.save(&value, Some(1)).unwrap();
        let committed = value.clone();
        value.title = "Stale writer".into();
        assert!(matches!(
            store.save(&value, Some(1)),
            Err(Error::RevisionConflict {
                actual: Some(2),
                ..
            })
        ));
        assert_eq!(store.load(&value.id).unwrap(), committed);
        value.revision = 4;
        assert!(store.save(&value, Some(2)).is_err());
        assert_eq!(store.load(&value.id).unwrap(), committed);
    }

    #[test]
    fn reads_and_writes_are_bounded_and_reads_do_not_create() {
        let directory = tempfile::tempdir().unwrap();
        let absent = directory.path().join("absent");
        let store = DocumentStore::new(&absent, 8);
        assert!(store.load("recap-1").is_err());
        assert!(!absent.exists());
        assert!(matches!(
            store.save(&document(), None),
            Err(Error::TooLarge { .. })
        ));
        assert!(!absent.exists());
        fs::create_dir(&absent).unwrap();
        fs::write(absent.join("recap-1.json"), [b' '; 9]).unwrap();
        assert!(matches!(
            store.load("recap-1"),
            Err(Error::TooLarge { limit: 8 })
        ));
        assert!(store.load("../escape").is_err());
    }

    #[test]
    fn future_and_corrupt_documents_are_not_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        let store = DocumentStore::new(directory.path(), 100_000);
        let path = directory.path().join("recap-1.json");
        for bytes in [
            br#"{"schema_version":999,"unknown":true}"#.as_slice(),
            b"not json",
        ] {
            fs::write(&path, bytes).unwrap();
            assert!(store.load("recap-1").is_err());
            assert!(store.save(&document(), None).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn lock_contention_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let store = DocumentStore::new(directory.path(), 100_000);
        let lock = WriterLock::acquire(directory.path().join("recap-1.lock")).unwrap();
        assert!(matches!(store.save(&document(), None), Err(Error::Locked)));
        assert!(!directory.path().join("recap-1.json").exists());
        drop(lock);
        store.save(&document(), None).unwrap();
    }

    #[test]
    fn asset_reads_require_registration_and_enforce_the_budget() {
        let directory = tempfile::tempdir().unwrap();
        let store = DocumentStore::new(directory.path(), 100_000);
        let value = document();
        store.save(&value, None).unwrap();
        let assets = store.asset_directory(&value.id).unwrap();
        fs::create_dir_all(assets.join("frames")).unwrap();
        fs::write(assets.join("frames/first.png"), b"image bytes").unwrap();
        assert_eq!(
            store.read_asset(&value.id, &value.assets[0], 11).unwrap(),
            b"image bytes"
        );
        assert!(matches!(
            store.read_asset(&value.id, &value.assets[0], 10),
            Err(Error::TooLarge { .. })
        ));
        let mut forged = value.assets[0].clone();
        forged.id = "unregistered".into();
        assert!(store.read_asset(&value.id, &forged, 100).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_documents_and_asset_components_are_rejected() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let store = DocumentStore::new(directory.path(), 100_000);
        let value = document();
        fs::write(
            outside.path().join("outside.json"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        symlink(
            outside.path().join("outside.json"),
            directory.path().join("recap-1.json"),
        )
        .unwrap();
        assert!(matches!(
            store.load(&value.id),
            Err(Error::UnsafeStoragePath)
        ));
        assert!(matches!(
            store.save(&value, None),
            Err(Error::UnsafeStoragePath)
        ));
        fs::remove_file(directory.path().join("recap-1.json")).unwrap();
        store.save(&value, None).unwrap();
        let assets = store.asset_directory(&value.id).unwrap();
        fs::create_dir(&assets).unwrap();
        symlink(outside.path(), assets.join("frames")).unwrap();
        assert!(matches!(
            store.read_asset(&value.id, &value.assets[0], 100),
            Err(Error::UnsafeStoragePath)
        ));
    }
}
