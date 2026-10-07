//! Disposable indexed launch previews; never canonical workflow state.

use bcode_workflow::WorkflowLaunchCatalogItem;

use crate::{WorkflowDiscoveryError, index};
use futures::executor::block_on;
use switchy_database::Database;
use switchy_database::query::{
    FilterableQuery, Projection, SortDirection, byte_length, identifier, where_and, where_eq,
    where_gt, where_or,
};
use switchy_database::schema::{DataType, create_index};

/// Maximum encoded JSON bytes decoded by one preview window (1 MiB).
///
/// This bounds decoder input, not exact Rust heap usage. Item count remains bounded too.
pub const MAX_PREVIEW_PAGE_BYTES: usize = 1024 * 1024;

/// A bounded keyset window. Resume strictly after the last returned item's semantic key.
#[derive(Debug)]
pub struct LaunchPreviewPage {
    /// Items admitted under both row and encoded-byte budgets.
    pub items: Vec<WorkflowLaunchCatalogItem>,
    /// Another row exists; it has not been decoded or consumed.
    pub has_more: bool,
}

/// Request-owned preview storage with a bounded `SQLite` page cache.
///
/// Rows are ordered by the application-supplied semantic title and source key. Drop
/// closes the database before removing its private directory. Process death may leave
/// temporary files for operating-system cleanup; they are never reopened.
pub struct LaunchPreviewSpool {
    connection: Box<dyn Database>,
    directory: tempfile::TempDir,
}

impl std::fmt::Debug for LaunchPreviewSpool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LaunchPreviewSpool")
            .field("connection", &self.connection)
            .field("directory", &self.directory)
            .finish()
    }
}

impl LaunchPreviewSpool {
    /// Create an empty disposable preview index.
    ///
    /// # Errors
    /// Returns filesystem or temporary-index errors.
    pub fn new() -> Result<Self, WorkflowDiscoveryError> {
        let directory = tempfile::Builder::new()
            .prefix("bcode-launch-previews-")
            .tempdir()?;
        let connection = index::open(&directory.path().join("previews.sqlite"))?;
        index::table(
            connection.as_ref(),
            "previews",
            &[
                ("title", DataType::Text),
                ("source_key", DataType::Text),
                ("payload", DataType::Text),
            ],
            &["title", "source_key"],
        )?;
        block_on(
            create_index("preview_identity")
                .table("previews")
                .column("source_key")
                .unique(true)
                .execute(connection.as_ref()),
        )?;
        Ok(Self {
            connection,
            directory,
        })
    }

    /// Resolve one retained identity with an indexed lookup, without scanning other previews.
    ///
    /// # Errors
    /// Returns temporary-index or payload decoding errors.
    pub fn find(
        &self,
        source_key: &str,
    ) -> Result<Option<WorkflowLaunchCatalogItem>, WorkflowDiscoveryError> {
        self.find_with_cancellation(source_key, &|| false)
    }

    /// Resolve one identity with bounded decoder input and cooperative cancellation.
    ///
    /// Checks cancellation before querying, before decoding, and before delivery.
    /// A single `SQLite` step or JSON decode remains non-interruptible.
    ///
    /// # Errors
    /// Returns cancellation, oversized-payload, temporary-index, or decoding errors.
    pub fn find_with_cancellation(
        &self,
        source_key: &str,
        cancelled: &impl Fn() -> bool,
    ) -> Result<Option<WorkflowLaunchCatalogItem>, WorkflowDiscoveryError> {
        let check = || {
            if cancelled() {
                Err(WorkflowDiscoveryError::Invalid(
                    "preview lookup cancelled".into(),
                ))
            } else {
                Ok(())
            }
        };
        check()?;
        let row = block_on(
            self.connection
                .select("previews")
                .projections(vec![
                    Projection::new(byte_length(identifier("payload"))).alias("bytes"),
                ])
                .where_eq("source_key", source_key)
                .limit(1)
                .execute_first(self.connection.as_ref()),
        )?;
        let result = if let Some(row) = row {
            check()?;
            if index::integer(&row, "bytes")? > MAX_PREVIEW_PAGE_BYTES {
                return Err(WorkflowDiscoveryError::Invalid(
                    "preview item exceeds page byte budget".into(),
                ));
            }
            let payload = self.payload(source_key, MAX_PREVIEW_PAGE_BYTES)?;
            Some(serde_json::from_str(&payload)?)
        } else {
            None
        };
        check()?;
        Ok(result)
    }

    /// Store one fully projected preview. Duplicate identities fail rather than overwrite.
    ///
    /// # Errors
    /// Returns serialization, oversized-payload, duplicate-identity, or temporary-index errors.
    pub fn insert(
        &self,
        source_key: &str,
        item: &WorkflowLaunchCatalogItem,
    ) -> Result<(), WorkflowDiscoveryError> {
        // Serialize into fixed storage: checking the length after `to_string` would
        // already have allocated an arbitrarily large second copy of the preview.
        let mut buffer = vec![0_u8; MAX_PREVIEW_PAGE_BYTES];
        let mut writer = buffer.as_mut_slice();
        if let Err(error) = serde_json::to_writer(&mut writer, item) {
            if error.io_error_kind() == Some(std::io::ErrorKind::WriteZero) {
                return Err(WorkflowDiscoveryError::Invalid(
                    "preview item exceeds page byte budget".into(),
                ));
            }
            return Err(error.into());
        }
        let length = MAX_PREVIEW_PAGE_BYTES - writer.len();
        let payload = std::str::from_utf8(&buffer[..length]).map_err(|error| {
            WorkflowDiscoveryError::Invalid(format!("invalid preview encoding: {error}"))
        })?;
        self.insert_payload(&item.title, source_key, payload)?;
        Ok(())
    }

    /// Read the first bounded window in semantic order, including optional lookahead.
    ///
    /// # Errors
    /// Rejects zero or oversized limits and returns index or decoding errors.
    pub fn first(
        &self,
        limit: usize,
    ) -> Result<Vec<WorkflowLaunchCatalogItem>, WorkflowDiscoveryError> {
        self.page(None, limit)
    }

    /// Read an indexed window strictly after a semantic cursor, without replaying earlier rows.
    ///
    /// # Errors
    /// Rejects invalid limits and returns temporary-index or decoding errors.
    pub fn page(
        &self,
        after: Option<(&str, &str)>,
        limit: usize,
    ) -> Result<Vec<WorkflowLaunchCatalogItem>, WorkflowDiscoveryError> {
        self.page_with_cancellation(after, limit, &|| false)
    }

    /// Read a keyset window, checking cancellation before each row read and decode.
    ///
    /// Cancellation discards the partial window; it never returns a truncated success.
    /// A single database step or JSON decode is not interruptible by this callback.
    ///
    /// # Errors
    /// Returns an error on cancellation, invalid limits, index access, or decoding failure.
    pub fn page_with_cancellation(
        &self,
        after: Option<(&str, &str)>,
        limit: usize,
        cancelled: &impl Fn() -> bool,
    ) -> Result<Vec<WorkflowLaunchCatalogItem>, WorkflowDiscoveryError> {
        let page = self.bounded_page(after, limit, MAX_PREVIEW_PAGE_BYTES, cancelled)?;
        if page.has_more && page.items.len() < limit {
            return Err(WorkflowDiscoveryError::Invalid(
                "preview window exceeds byte budget; use bounded paging".into(),
            ));
        }
        Ok(page.items)
    }

    /// Read a row- and encoded-byte-bounded window with undecoded lookahead.
    ///
    /// An item exceeding the entire byte budget fails explicitly when it becomes the
    /// first item. No successful page is empty while reporting more items.
    ///
    /// # Errors
    /// Rejects invalid budgets, cancellation, an oversized first item, and index/decode errors.
    pub fn bounded_page(
        &self,
        after: Option<(&str, &str)>,
        limit: usize,
        byte_budget: usize,
        cancelled: &impl Fn() -> bool,
    ) -> Result<LaunchPreviewPage, WorkflowDiscoveryError> {
        let check = || {
            if cancelled() {
                Err(WorkflowDiscoveryError::Invalid(
                    "preview page cancelled".into(),
                ))
            } else {
                Ok(())
            }
        };
        check()?;
        if limit == 0
            || limit > crate::MAX_DISCOVERY_RESULTS
            || byte_budget == 0
            || byte_budget > MAX_PREVIEW_PAGE_BYTES
        {
            return Err(WorkflowDiscoveryError::Invalid(
                "invalid preview window limit".into(),
            ));
        }
        let mut query = self
            .connection
            .select("previews")
            .projections(vec![
                Projection::new(identifier("source_key")),
                Projection::new(byte_length(identifier("payload"))).alias("bytes"),
            ])
            .sort("title", SortDirection::Asc)
            .sort("source_key", SortDirection::Asc)
            .limit(limit + 1);
        if let Some((title, key)) = after {
            query = query.filter(Box::new(where_or(vec![
                Box::new(where_gt("title", title)),
                Box::new(where_and(vec![
                    Box::new(where_eq("title", title)),
                    Box::new(where_gt("source_key", key)),
                ])),
            ])));
        }
        let rows = block_on(query.execute(self.connection.as_ref()))?;
        let mut items = Vec::new();
        let mut remaining = byte_budget;
        let mut has_more = false;
        for row in rows {
            check()?;
            check()?;
            if items.len() == limit {
                has_more = true;
                break;
            }
            let payload_bytes = index::integer(&row, "bytes")?;
            if payload_bytes > remaining {
                if items.is_empty() {
                    return Err(WorkflowDiscoveryError::Invalid(
                        "preview item exceeds page byte budget".into(),
                    ));
                }
                has_more = true;
                break;
            }
            let payload = self.payload(&index::text(&row, "source_key")?, remaining)?;
            remaining -= payload_bytes;
            items.push(serde_json::from_str(&payload)?);
        }
        check()?;
        Ok(LaunchPreviewPage { items, has_more })
    }
    fn insert_payload(
        &self,
        title: &str,
        key: &str,
        payload: &str,
    ) -> Result<(), WorkflowDiscoveryError> {
        block_on(
            self.connection
                .insert("previews")
                .value("title", title)
                .value("source_key", key)
                .value("payload", payload)
                .execute(self.connection.as_ref()),
        )?;
        Ok(())
    }

    // This request-local index has no concurrent writers; preflight byte lengths
    // are therefore valid until the payload read completes.
    fn payload(&self, key: &str, budget: usize) -> Result<String, WorkflowDiscoveryError> {
        let row = block_on(
            self.connection
                .select("previews")
                .columns(&["payload"])
                .where_eq("source_key", key)
                .limit(1)
                .execute_first(self.connection.as_ref()),
        )?
        .ok_or_else(|| WorkflowDiscoveryError::Invalid("preview payload missing".into()))?;
        let payload = index::text(&row, "payload")?;
        if payload.len() > budget {
            return Err(WorkflowDiscoveryError::Invalid(
                "preview payload oversized".into(),
            ));
        }
        Ok(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str) -> WorkflowLaunchCatalogItem {
        WorkflowLaunchCatalogItem {
            source: bcode_workflow::WorkflowLaunchSourceIdentity::Template {
                owner_plugin_id: "test".into(),
                template_id: title.into(),
                template_version: 1,
            },
            source_label: "test".into(),
            precedence: 0,
            title: title.into(),
            description: None,
            readiness: bcode_workflow::WorkflowLaunchReadiness::Ready,
            unavailable_reason: None,
            package_lock_digest_sha256: None,
            publication: None,
            actions: Vec::new(),
            requirements: bcode_workflow::WorkflowRequirementSummary::default(),
            effects: bcode_workflow::WorkflowEffectSummary::default(),
            permissions: bcode_workflow::WorkflowPermissionPreview::default(),
            input_schema: bcode_workflow::ValueSchema::of::<String>(),
            configuration_schema: bcode_workflow::ValueSchema::of::<String>(),
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn insertion_accepts_exact_budget_and_rejects_overflow_without_publishing() {
        let spool = LaunchPreviewSpool::new().unwrap();
        let mut preview = item("bounded");
        preview.description = Some(String::new());
        let overhead = serde_json::to_vec(&preview).unwrap().len();
        preview.description = Some("x".repeat(MAX_PREVIEW_PAGE_BYTES - overhead));
        spool.insert("exact", &preview).unwrap();
        assert_eq!(spool.find("exact").unwrap(), Some(preview.clone()));
        preview.description.as_mut().unwrap().push('x');
        assert!(matches!(
            spool.insert("oversized", &preview),
            Err(WorkflowDiscoveryError::Invalid(message))
                if message == "preview item exceeds page byte budget"
        ));
        assert!(spool.find("oversized").unwrap().is_none());
        // Escaping, rather than the input string's byte length, consumes the budget.
        preview.description = Some("\n".repeat(MAX_PREVIEW_PAGE_BYTES / 2));
        assert!(spool.insert("escaped", &preview).is_err());
        assert!(spool.find("escaped").unwrap().is_none());
        spool.insert("recovery", &item("small")).unwrap();
        assert!(spool.find("recovery").unwrap().is_some());
    }

    #[test]
    fn retained_lookup_bounds_payload_and_checks_cancellation_before_decoding() {
        let spool = LaunchPreviewSpool::new().unwrap();
        let a = item("a");
        spool.insert("a", &a).unwrap();
        assert_eq!(spool.find("a").unwrap(), Some(a));
        assert_eq!(spool.find("missing").unwrap(), None);
        spool.insert_payload("bad", "bad", "invalid json").unwrap();
        let calls = std::cell::Cell::new(0);
        let result = spool.find_with_cancellation("bad", &|| {
            calls.set(calls.get() + 1);
            calls.get() == 2
        });
        assert!(
            matches!(result, Err(WorkflowDiscoveryError::Invalid(message)) if message == "preview lookup cancelled")
        );
        let oversized = "x".repeat(MAX_PREVIEW_PAGE_BYTES + 1);
        spool.insert_payload("large", "large", &oversized).unwrap();
        assert!(
            matches!(spool.find("large"), Err(WorkflowDiscoveryError::Invalid(message)) if message == "preview item exceeds page byte budget")
        );
        assert!(spool.find_with_cancellation("missing", &|| true).is_err());
    }

    #[test]
    fn byte_bounded_windows_resume_without_skipping_and_reject_oversized_items() {
        let spool = LaunchPreviewSpool::new().unwrap();
        let a = item("a");
        let b = item("b");
        let budget = serde_json::to_vec(&a).unwrap().len();
        spool.insert("b", &b).unwrap();
        spool.insert("a", &a).unwrap();
        let first = spool.bounded_page(None, 10, budget, &|| false).unwrap();
        assert_eq!(first.items, vec![a]);
        assert!(first.has_more);
        let second = spool
            .bounded_page(Some(("a", "a")), 10, budget, &|| false)
            .unwrap();
        assert_eq!(second.items, vec![b]);
        assert!(!second.has_more);
        assert!(
            matches!(spool.bounded_page(None, 10, budget - 1, &|| false), Err(WorkflowDiscoveryError::Invalid(message)) if message == "preview item exceeds page byte budget")
        );
        let row_limited = spool
            .bounded_page(None, 1, MAX_PREVIEW_PAGE_BYTES, &|| false)
            .unwrap();
        assert!(row_limited.has_more);
        assert_eq!(row_limited.items.len(), 1);
    }

    #[test]
    fn cancellation_prevents_decoding_the_next_row() {
        let spool = LaunchPreviewSpool::new().unwrap();
        spool
            .insert_payload("title", "key", "invalid json")
            .unwrap();
        let calls = std::cell::Cell::new(0);
        let result = spool.page_with_cancellation(None, 1, &|| {
            calls.set(calls.get() + 1);
            calls.get() == 3
        });
        assert!(
            matches!(result, Err(WorkflowDiscoveryError::Invalid(message)) if message == "preview page cancelled")
        );
        assert!(spool.page(None, 1).is_err());
        assert!(spool.page_with_cancellation(None, 1, &|| true).is_err());
    }

    #[test]
    fn semantic_keyset_preserves_equal_titles_and_unique_source_identity() {
        let spool = LaunchPreviewSpool::new().unwrap();
        let preview = item("same");
        spool.insert("b", &preview).unwrap();
        spool.insert("a", &preview).unwrap();
        assert!(spool.insert("a", &item("different")).is_err());
        let page = spool
            .bounded_page(None, 1, MAX_PREVIEW_PAGE_BYTES, &|| false)
            .unwrap();
        assert_eq!(page.items, vec![preview.clone()]);
        assert!(page.has_more);
        let next = spool
            .bounded_page(Some(("same", "a")), 1, MAX_PREVIEW_PAGE_BYTES, &|| false)
            .unwrap();
        assert_eq!(next.items, vec![preview]);
        assert!(!next.has_more);
        assert!(
            spool
                .bounded_page(Some(("same", "b")), 1, MAX_PREVIEW_PAGE_BYTES, &|| false)
                .unwrap()
                .items
                .is_empty()
        );
    }

    #[test]
    fn preview_spool_is_private_bounded_and_removed_on_drop() {
        let spool = LaunchPreviewSpool::new().unwrap();
        let path = spool.directory.path().to_path_buf();
        assert!(path.join("previews.sqlite").is_file());
        assert!(spool.first(1).unwrap().is_empty());
        assert!(spool.first(0).is_err());
        assert!(spool.first(crate::MAX_DISCOVERY_RESULTS + 1).is_err());
        drop(spool);
        assert!(!path.exists());
    }
}
