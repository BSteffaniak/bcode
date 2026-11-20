//! Attachments: files and directory trees copied into `<root>/attachments/fb-<n>/`.
//!
//! Sources are checked before anything is copied: the path must be absolute, contain no
//! symbolic links, and stay within [`MAX_ATTACHMENT_BYTES`] and [`MAX_ATTACHMENT_FILES`].
//! The copy is written beside its final name and renamed into place, so a failed copy never
//! leaves a partial attachment, and a copy whose row cannot be recorded is removed.

use std::path::{Path, PathBuf};

use bcode_feedback_models::{
    AttachFeedbackRequest, AttachmentKind, FeedbackAttachment, FeedbackItem, MAX_ATTACHMENT_BYTES,
    MAX_ATTACHMENT_FILES, MAX_ATTACHMENTS,
};
use switchy_database::query::FilterableQuery as _;
use switchy_database::{DatabaseValue, Row, TransactionMode};

use crate::{FeedbackStore, FeedbackStoreError, database, int, optional_text, text, unsigned};

/// Deepest directory nesting accepted in a directory attachment.
const MAX_DEPTH: usize = 8;

impl FeedbackStore {
    /// Copy a file or directory into an item.
    ///
    /// # Errors
    /// * [`FeedbackStoreError::Invalid`] for a malformed request.
    /// * [`FeedbackStoreError::NotFound`] for an unknown item.
    /// * [`FeedbackStoreError::AttachmentRejected`] when the source is missing, a symbolic
    ///   link, contains one, is too large, or the item already has [`MAX_ATTACHMENTS`].
    /// * [`FeedbackStoreError::Path`] when the copy cannot be written, or a database error.
    pub fn attach(
        &self,
        request: &AttachFeedbackRequest,
        now_ms: u64,
    ) -> Result<FeedbackItem, FeedbackStoreError> {
        request.validate()?;
        let item = self.get(request.id)?;
        if item.attachments.len() >= MAX_ATTACHMENTS {
            return Err(FeedbackStoreError::AttachmentRejected(format!(
                "{} already has {MAX_ATTACHMENTS} attachments",
                item.id
            )));
        }
        let source = Path::new(&request.source);
        let (kind, bytes) = measure(source)?;
        let ordinal = item
            .attachments
            .iter()
            .map(|attachment| attachment.ordinal)
            .max()
            .unwrap_or(0)
            + 1;
        let directory = self.root.join("attachments").join(item.id.to_string());
        std::fs::create_dir_all(&directory)
            .map_err(|error| FeedbackStoreError::Path(directory.clone(), error.to_string()))?;
        let stored = directory.join(format!("{ordinal}-{}", request.name));
        let partial = directory.join(format!(".{ordinal}-{}.partial", request.name));
        remove(&partial);
        let copied = match kind {
            AttachmentKind::File => std::fs::copy(source, &partial).map(drop),
            AttachmentKind::Directory => copy_tree(source, &partial),
        }
        .and_then(|()| std::fs::rename(&partial, &stored));
        if let Err(error) = copied {
            remove(&partial);
            return Err(FeedbackStoreError::Path(stored, error.to_string()));
        }
        let recorded = database::block_on(async {
            let tx = self
                .database
                .begin_transaction_with_mode(TransactionMode::Immediate)
                .await?;
            tx.insert("feedback_attachments")
                .value("item_id", int(item.id.0)?)
                .value("ordinal", i64::from(ordinal))
                .value("name", request.name.as_str())
                .value("kind", kind_name(kind))
                .value("media_type", request.media_type.as_str())
                .value("bytes", int(bytes)?)
                .value(
                    "description",
                    request
                        .description
                        .clone()
                        .map_or(DatabaseValue::Null, DatabaseValue::String),
                )
                .value("sensitive", i64::from(request.sensitive))
                .value("stored_path", stored.to_string_lossy().as_ref())
                .value("created_at_ms", int(now_ms)?)
                .execute(&*tx)
                .await?;
            tx.update("feedback_items")
                .where_eq("id", int(item.id.0)?)
                .value("updated_at_ms", int(now_ms)?)
                .execute(&*tx)
                .await?;
            tx.commit().await?;
            Ok::<(), FeedbackStoreError>(())
        });
        if let Err(error) = recorded {
            remove(&stored);
            return Err(error);
        }
        self.get(item.id)
    }
}

/// Decode one `feedback_attachments` row.
pub fn from_row(row: &Row) -> Result<FeedbackAttachment, FeedbackStoreError> {
    Ok(FeedbackAttachment {
        ordinal: u32::try_from(unsigned(row, "ordinal")?)
            .map_err(|_| FeedbackStoreError::Corrupt("attachment ordinal".into()))?,
        name: text(row, "name")?,
        kind: match text(row, "kind")?.as_str() {
            "file" => AttachmentKind::File,
            "directory" => AttachmentKind::Directory,
            other => {
                return Err(FeedbackStoreError::Corrupt(format!(
                    "attachment kind {other:?}"
                )));
            }
        },
        media_type: text(row, "media_type")?,
        bytes: unsigned(row, "bytes")?,
        description: optional_text(row, "description")?,
        sensitive: unsigned(row, "sensitive")? != 0,
        path: text(row, "stored_path")?,
        created_at_ms: unsigned(row, "created_at_ms")?,
    })
}

const fn kind_name(kind: AttachmentKind) -> &'static str {
    match kind {
        AttachmentKind::File => "file",
        AttachmentKind::Directory => "directory",
    }
}

/// The kind and total size of a source, refusing links, special files and oversized trees.
fn measure(source: &Path) -> Result<(AttachmentKind, u64), FeedbackStoreError> {
    let reject = |reason: String| Err(FeedbackStoreError::AttachmentRejected(reason));
    let metadata = match std::fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) => return reject(format!("{}: {error}", source.display())),
    };
    if metadata.is_file() {
        if metadata.len() > MAX_ATTACHMENT_BYTES {
            return reject(format!(
                "{} is larger than {MAX_ATTACHMENT_BYTES} bytes",
                source.display()
            ));
        }
        return Ok((AttachmentKind::File, metadata.len()));
    }
    if !metadata.is_dir() {
        return reject(format!(
            "{} is not a regular file or directory",
            source.display()
        ));
    }
    let mut totals = (0_u64, 0_usize);
    walk(source, 0, &mut totals)?;
    Ok((AttachmentKind::Directory, totals.0))
}

fn walk(
    directory: &Path,
    depth: usize,
    totals: &mut (u64, usize),
) -> Result<(), FeedbackStoreError> {
    let reject = |reason: String| FeedbackStoreError::AttachmentRejected(reason);
    if depth > MAX_DEPTH {
        return Err(reject(format!(
            "{} nests deeper than {MAX_DEPTH} directories",
            directory.display()
        )));
    }
    let entries = std::fs::read_dir(directory)
        .map_err(|error| reject(format!("{}: {error}", directory.display())))?;
    for entry in entries {
        let entry = entry.map_err(|error| reject(format!("{}: {error}", directory.display())))?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| reject(format!("{}: {error}", path.display())))?;
        if metadata.is_dir() {
            walk(&path, depth + 1, totals)?;
        } else if metadata.is_file() {
            totals.0 = totals.0.saturating_add(metadata.len());
            totals.1 += 1;
            if totals.0 > MAX_ATTACHMENT_BYTES || totals.1 > MAX_ATTACHMENT_FILES {
                return Err(reject(format!(
                    "{} holds more than {MAX_ATTACHMENT_FILES} files or {MAX_ATTACHMENT_BYTES} bytes",
                    directory.display()
                )));
            }
        } else {
            return Err(reject(format!(
                "{} is a symbolic link or special file",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Copy a directory tree that [`measure`] accepted.
fn copy_tree(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::create_dir(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let target: PathBuf = destination.join(entry.file_name());
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if metadata.is_file() {
            std::fs::copy(entry.path(), &target)?;
        } else {
            return Err(std::io::Error::other(
                "the source changed while it was copied",
            ));
        }
    }
    Ok(())
}

fn remove(path: &Path) {
    let _ = std::fs::remove_dir_all(path).or_else(|_| std::fs::remove_file(path));
}
