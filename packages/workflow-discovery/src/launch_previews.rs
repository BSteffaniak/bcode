//! Disposable indexed launch previews; never canonical workflow state.

use bcode_workflow::WorkflowLaunchCatalogItem;

use crate::WorkflowDiscoveryError;

/// Request-owned preview storage with a bounded `SQLite` page cache.
///
/// Rows are ordered by the application-supplied semantic title and source key. Drop
/// closes the database before removing its private directory. Process death may leave
/// temporary files for operating-system cleanup; they are never reopened.
#[derive(Debug)]
pub struct LaunchPreviewSpool {
    connection: rusqlite::Connection,
    _directory: tempfile::TempDir,
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
        let connection = rusqlite::Connection::open(directory.path().join("previews.sqlite"))?;
        connection.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA cache_size=-1024; PRAGMA mmap_size=0; PRAGMA temp_store=FILE; CREATE TABLE previews(title TEXT NOT NULL, source_key TEXT NOT NULL, payload TEXT NOT NULL, PRIMARY KEY(title, source_key)) WITHOUT ROWID; CREATE UNIQUE INDEX preview_identity ON previews(source_key);")?;
        Ok(Self {
            connection,
            _directory: directory,
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
        use rusqlite::OptionalExtension as _;
        let payload: Option<String> = self
            .connection
            .query_row(
                "SELECT payload FROM previews WHERE source_key = ?1",
                [source_key],
                |row| row.get(0),
            )
            .optional()?;
        payload
            .map(|payload| serde_json::from_str(&payload).map_err(Into::into))
            .transpose()
    }

    /// Store one fully projected preview. Duplicate identities fail rather than overwrite.
    ///
    /// # Errors
    /// Returns serialization, duplicate-identity, or temporary-index errors.
    pub fn insert(
        &self,
        source_key: &str,
        item: &WorkflowLaunchCatalogItem,
    ) -> Result<(), WorkflowDiscoveryError> {
        let payload = serde_json::to_string(item)?;
        self.connection.execute(
            "INSERT INTO previews(title, source_key, payload) VALUES (?1, ?2, ?3)",
            rusqlite::params![item.title, source_key, payload],
        )?;
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
        if limit == 0 || limit > crate::MAX_DISCOVERY_RESULTS {
            return Err(WorkflowDiscoveryError::Invalid(
                "invalid preview window limit".into(),
            ));
        }
        let (sql, title, key) = after.map_or(
            ("SELECT payload FROM previews ORDER BY title, source_key LIMIT ?3", "", ""),
            |(title, key)| ("SELECT payload FROM previews WHERE (title, source_key) > (?1, ?2) ORDER BY title, source_key LIMIT ?3", title, key),
        );
        let mut statement = self.connection.prepare(sql)?;
        let rows = statement.query_map(rusqlite::params![title, key, limit], |row| {
            row.get::<_, String>(0)
        })?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_spool_is_private_bounded_and_removed_on_drop() {
        let spool = LaunchPreviewSpool::new().unwrap();
        let path = std::path::PathBuf::from(spool.connection.path().unwrap())
            .parent()
            .unwrap()
            .to_path_buf();
        assert!(path.join("previews.sqlite").is_file());
        assert!(spool.first(1).unwrap().is_empty());
        assert!(spool.first(0).is_err());
        assert!(spool.first(crate::MAX_DISCOVERY_RESULTS + 1).is_err());
        drop(spool);
        assert!(!path.exists());
    }
}
