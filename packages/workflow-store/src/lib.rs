#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Durable workflow persistence owned independently from session transcript storage.

use bcode_workflow::WorkflowDefinition;
use rusqlite::{Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use thiserror::Error;

const DATABASE_FILE: &str = "workflow.db";
const SCHEMA_VERSION: u32 = 1;
const MAX_ID_BYTES: usize = 512;

/// Errors returned by durable workflow persistence.
#[derive(Debug, Error)]
pub enum WorkflowStoreError {
    /// Database operation failed.
    #[error("workflow database error: {0}")]
    Database(#[from] rusqlite::Error),
    /// Filesystem operation failed.
    #[error("workflow store I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Definition serialization failed.
    #[error("workflow definition serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    /// Persisted data violated the storage contract.
    #[error("invalid workflow store data: {0}")]
    InvalidData(String),
}

/// Canonical persisted definition identity and content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredWorkflowDefinition {
    /// Stable definition identity.
    pub definition_id: String,
    /// Positive definition version.
    pub version: u32,
    /// SHA-256 of canonical serialized definition JSON.
    pub checksum_sha256: String,
    /// Canonical serialized definition.
    pub definition_json: String,
}

/// Durable workflow database.
#[derive(Debug)]
pub struct WorkflowStore {
    path: PathBuf,
    connection: Connection,
}

impl WorkflowStore {
    /// Open or create the canonical workflow database below an explicit Bcode state directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory/database cannot be opened or migrations fail.
    pub fn open_in_state_dir(state_dir: &Path) -> Result<Self, WorkflowStoreError> {
        Self::open_at(&workflow_database_path(state_dir))
    }

    /// Open the production-default workflow database.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory/database cannot be opened or migrations fail.
    pub fn open_default() -> Result<Self, WorkflowStoreError> {
        Self::open_in_state_dir(&bcode_config::default_state_dir())
    }

    fn open_at(path: &Path) -> Result<Self, WorkflowStoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut connection = Connection::open(path)?;
        connection.pragma_update(None, "foreign_keys", true)?;
        migrate(&mut connection)?;
        Ok(Self {
            path: path.to_path_buf(),
            connection,
        })
    }

    /// Return the canonical database path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Persist an immutable normalized definition and checksum.
    ///
    /// Re-persisting byte-identical content is idempotent. Reusing one definition/version for
    /// different content fails closed.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identity/version, serialization failure, checksum conflict, or
    /// database failure.
    pub fn persist_definition(
        &mut self,
        definition_id: &str,
        version: u32,
        definition: &WorkflowDefinition,
    ) -> Result<StoredWorkflowDefinition, WorkflowStoreError> {
        validate_id("definition_id", definition_id)?;
        if version == 0 {
            return Err(WorkflowStoreError::InvalidData(
                "definition version must be positive".to_string(),
            ));
        }
        let definition_json = serde_json::to_string(definition)?;
        let checksum_sha256 = sha256_hex(definition_json.as_bytes());
        let stored = StoredWorkflowDefinition {
            definition_id: definition_id.to_string(),
            version,
            checksum_sha256,
            definition_json,
        };
        let transaction = self.connection.transaction()?;
        persist_definition_transaction(&transaction, &stored)?;
        transaction.commit()?;
        Ok(stored)
    }

    /// Load one exact definition version with checksum verification.
    ///
    /// # Errors
    ///
    /// Returns an error when the query fails or persisted content does not match its checksum.
    pub fn definition(
        &self,
        definition_id: &str,
        version: u32,
    ) -> Result<Option<StoredWorkflowDefinition>, WorkflowStoreError> {
        let stored = self
            .connection
            .query_row(
                "SELECT definition_id, version, checksum_sha256, definition_json \
                 FROM workflow_definitions WHERE definition_id = ?1 AND version = ?2",
                (definition_id, version),
                |row| {
                    Ok(StoredWorkflowDefinition {
                        definition_id: row.get(0)?,
                        version: row.get(1)?,
                        checksum_sha256: row.get(2)?,
                        definition_json: row.get(3)?,
                    })
                },
            )
            .optional()?;
        if let Some(stored) = &stored
            && sha256_hex(stored.definition_json.as_bytes()) != stored.checksum_sha256
        {
            return Err(WorkflowStoreError::InvalidData(format!(
                "definition checksum mismatch: {} v{}",
                stored.definition_id, stored.version
            )));
        }
        Ok(stored)
    }
}

/// Return the canonical workflow database path under one Bcode state directory.
#[must_use]
pub fn workflow_database_path(state_dir: &Path) -> PathBuf {
    state_dir.join("workflows").join(DATABASE_FILE)
}

fn persist_definition_transaction(
    transaction: &Transaction<'_>,
    stored: &StoredWorkflowDefinition,
) -> Result<(), WorkflowStoreError> {
    let existing = transaction
        .query_row(
            "SELECT checksum_sha256 FROM workflow_definitions \
             WHERE definition_id = ?1 AND version = ?2",
            (&stored.definition_id, stored.version),
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        if existing == stored.checksum_sha256 {
            return Ok(());
        }
        return Err(WorkflowStoreError::InvalidData(format!(
            "definition identity conflict: {} v{}",
            stored.definition_id, stored.version
        )));
    }
    transaction.execute(
        "INSERT INTO workflow_definitions \
         (definition_id, version, checksum_sha256, definition_json) VALUES (?1, ?2, ?3, ?4)",
        (
            &stored.definition_id,
            stored.version,
            &stored.checksum_sha256,
            &stored.definition_json,
        ),
    )?;
    Ok(())
}

fn migrate(connection: &mut Connection) -> Result<(), WorkflowStoreError> {
    let transaction = connection.transaction()?;
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS workflow_store_contract (\
             contract_id INTEGER PRIMARY KEY CHECK (contract_id = 1),\
             schema_version INTEGER NOT NULL\
         );\
         INSERT OR IGNORE INTO workflow_store_contract (contract_id, schema_version) VALUES (1, 1);\
         CREATE TABLE IF NOT EXISTS workflow_definitions (\
             definition_id TEXT NOT NULL,\
             version INTEGER NOT NULL CHECK (version > 0),\
             checksum_sha256 TEXT NOT NULL,\
             definition_json TEXT NOT NULL,\
             PRIMARY KEY (definition_id, version)\
         );",
    )?;
    let actual: u32 = transaction.query_row(
        "SELECT schema_version FROM workflow_store_contract WHERE contract_id = 1",
        [],
        |row| row.get(0),
    )?;
    if actual != SCHEMA_VERSION {
        return Err(WorkflowStoreError::InvalidData(format!(
            "unsupported workflow schema version {actual}; expected {SCHEMA_VERSION}"
        )));
    }
    transaction.commit()?;
    Ok(())
}

fn validate_id(label: &str, value: &str) -> Result<(), WorkflowStoreError> {
    if value.trim().is_empty() || value.len() > MAX_ID_BYTES {
        return Err(WorkflowStoreError::InvalidData(format!(
            "{label} must contain 1..={MAX_ID_BYTES} bytes"
        )));
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().fold(
        String::with_capacity(digest.len() * 2),
        |mut encoded, byte| {
            write!(encoded, "{byte:02x}").expect("writing to a string cannot fail");
            encoded
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_workflow::{Step, WorkflowBuilder};

    fn definition(name: &str) -> WorkflowDefinition {
        WorkflowBuilder::new(
            name,
            Step::task(
                "increment",
                |value: u32, _context| async move { Ok(value + 1) },
            ),
        )
        .build()
        .expect("workflow")
        .definition()
        .clone()
    }

    #[test]
    fn canonical_path_is_below_workflows_directory() {
        assert_eq!(
            workflow_database_path(Path::new("/state")),
            Path::new("/state/workflows/workflow.db")
        );
    }

    #[test]
    fn definitions_persist_idempotently_and_verify_checksum() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        let definition = definition("example");
        let first = store
            .persist_definition("example", 1, &definition)
            .expect("persist");
        let second = store
            .persist_definition("example", 1, &definition)
            .expect("idempotent");
        assert_eq!(first, second);
        assert_eq!(store.definition("example", 1).expect("load"), Some(first));
    }

    #[test]
    fn definition_identity_conflicts_fail_closed() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("example", 1, &definition("first"))
            .expect("first");
        let error = store
            .persist_definition("example", 1, &definition("second"))
            .expect_err("conflict");
        assert!(error.to_string().contains("identity conflict"));
    }
}
