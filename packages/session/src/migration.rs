use bcode_session_models::SessionId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::Path;
use thiserror::Error;

/// Action a migration plan would perform for session persistence metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMigrationAction {
    /// No action is needed.
    None,
    /// Rebuild derived session indexes from canonical event logs.
    RebuildDerivedIndex,
}

/// How a migration handles backups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMigrationBackupPolicy {
    /// No backup is needed because canonical data is not rewritten.
    NotRequired,
    /// A backup must exist before canonical data can be rewritten.
    Required,
}

/// How a migration may be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMigrationApplyPolicy {
    /// The migration is safe for automatic/background execution.
    Automatic,
    /// The migration requires explicit user action.
    Manual,
}

/// Registered session migration metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionMigrationDefinition {
    pub id: &'static str,
    pub domain: &'static str,
    pub from_version: u16,
    pub to_version: u16,
    pub action: SessionMigrationAction,
    pub backup_policy: SessionMigrationBackupPolicy,
    pub apply_policy: SessionMigrationApplyPolicy,
}

impl SessionMigrationDefinition {
    #[must_use]
    pub const fn automatic(&self) -> bool {
        matches!(self.apply_policy, SessionMigrationApplyPolicy::Automatic)
    }
}

const SESSION_INDEX_REBUILD_MIGRATION: SessionMigrationDefinition = SessionMigrationDefinition {
    id: "sessions-index-rebuild-v2",
    domain: "sessions/index",
    from_version: 0,
    to_version: crate::index::SESSION_INDEX_VERSION,
    action: SessionMigrationAction::RebuildDerivedIndex,
    backup_policy: SessionMigrationBackupPolicy::NotRequired,
    apply_policy: SessionMigrationApplyPolicy::Automatic,
};

const SESSION_MIGRATIONS: &[SessionMigrationDefinition] = &[SESSION_INDEX_REBUILD_MIGRATION];

/// Registry of session persistence migrations.
#[derive(Debug, Clone, Copy)]
pub struct SessionMigrationRegistry {
    migrations: &'static [SessionMigrationDefinition],
}

impl SessionMigrationRegistry {
    /// Return the built-in session migration registry.
    #[must_use]
    pub const fn builtin() -> Self {
        Self {
            migrations: SESSION_MIGRATIONS,
        }
    }

    /// Return all registered migrations.
    #[must_use]
    pub const fn migrations(&self) -> &'static [SessionMigrationDefinition] {
        self.migrations
    }

    /// Return the migration definition for an action.
    #[must_use]
    pub fn migration_for_action(
        &self,
        action: SessionMigrationAction,
    ) -> Option<SessionMigrationDefinition> {
        self.migrations
            .iter()
            .copied()
            .find(|migration| migration.action == action)
    }

    pub(crate) fn required_migration_for_action(
        &self,
        action: SessionMigrationAction,
    ) -> Result<SessionMigrationDefinition, SessionMigrationRegistryError> {
        self.migration_for_action(action)
            .ok_or(SessionMigrationRegistryError::MissingRequiredAction(action))
    }

    /// Validate registry invariants.
    ///
    /// # Errors
    ///
    /// Returns an error when migration definitions are duplicated or unsafe.
    pub fn validate(&self) -> Result<(), SessionMigrationRegistryError> {
        let mut ids = BTreeSet::new();
        let mut edges = BTreeSet::new();
        for migration in self.migrations {
            if migration.id.is_empty() {
                return Err(SessionMigrationRegistryError::EmptyId);
            }
            if !ids.insert(migration.id) {
                return Err(SessionMigrationRegistryError::DuplicateId(migration.id));
            }
            let edge = (
                migration.domain,
                migration.from_version,
                migration.to_version,
            );
            if !edges.insert(edge) {
                return Err(SessionMigrationRegistryError::DuplicateVersionEdge {
                    domain: migration.domain,
                    from_version: migration.from_version,
                    to_version: migration.to_version,
                });
            }
            if migration.from_version >= migration.to_version {
                return Err(SessionMigrationRegistryError::InvalidVersionEdge {
                    id: migration.id,
                    from_version: migration.from_version,
                    to_version: migration.to_version,
                });
            }
            if migration.backup_policy == SessionMigrationBackupPolicy::Required
                && migration.apply_policy == SessionMigrationApplyPolicy::Automatic
            {
                return Err(
                    SessionMigrationRegistryError::UnsafeAutomaticCanonicalRewrite(migration.id),
                );
            }
        }
        Ok(())
    }
}

/// Session migration registry validation error.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SessionMigrationRegistryError {
    #[error("session migration id cannot be empty")]
    EmptyId,
    #[error("duplicate session migration id: {0}")]
    DuplicateId(&'static str),
    #[error("duplicate session migration version edge for {domain}: {from_version}->{to_version}")]
    DuplicateVersionEdge {
        domain: &'static str,
        from_version: u16,
        to_version: u16,
    },
    #[error("invalid session migration version edge for {id}: {from_version}->{to_version}")]
    InvalidVersionEdge {
        id: &'static str,
        from_version: u16,
        to_version: u16,
    },
    #[error("automatic canonical rewrite migration must not require backup: {0}")]
    UnsafeAutomaticCanonicalRewrite(&'static str),
    #[error("missing required session migration action: {0:?}")]
    MissingRequiredAction(SessionMigrationAction),
}

/// Migration status for a single session persistence target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMigrationPlanItem {
    pub migration_id: &'static str,
    pub session_id: SessionId,
    pub current_version: u16,
    pub found_version: Option<u16>,
    pub action: SessionMigrationAction,
    pub reason: String,
    pub automatic: bool,
    pub backup_policy: SessionMigrationBackupPolicy,
}

/// Migration plan for session persistence metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMigrationPlan {
    pub domain: &'static str,
    pub items: Vec<SessionMigrationPlanItem>,
}

impl SessionMigrationPlan {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Options for applying session persistence migrations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SessionMigrationOptions {
    /// Report actions without modifying files.
    pub dry_run: bool,
    /// Back up canonical event logs before applying migrations.
    pub backup: bool,
}

/// Result status for one migration item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMigrationApplyStatus {
    /// The item would be applied, but dry-run mode was requested.
    Planned,
    /// The item was applied.
    Applied,
    /// The item was skipped because it requires manual handling.
    Skipped,
}

/// Result for one migration item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMigrationReportItem {
    pub migration_id: &'static str,
    pub session_id: SessionId,
    pub action: SessionMigrationAction,
    pub status: SessionMigrationApplyStatus,
    pub message: String,
}

/// Result from applying a session migration plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMigrationReport {
    pub domain: &'static str,
    pub dry_run: bool,
    pub backup_dir: Option<std::path::PathBuf>,
    pub items: Vec<SessionMigrationReportItem>,
}

/// Journal entry status for migration attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMigrationJournalStatus {
    /// Migration apply began.
    Started,
    /// Migration apply completed successfully.
    Completed,
    /// Migration apply failed.
    Failed,
}

/// Durable migration journal entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMigrationJournalEntry {
    pub run_id: String,
    pub domain: &'static str,
    pub status: SessionMigrationJournalStatus,
    pub dry_run: bool,
    pub backup: bool,
    pub backup_dir: Option<String>,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub migration_ids: Vec<&'static str>,
    pub session_ids: Vec<SessionId>,
    pub error: Option<String>,
}

pub(crate) fn append_journal_entry(
    root: &Path,
    entry: &SessionMigrationJournalEntry,
) -> Result<(), crate::SessionStoreError> {
    fs::create_dir_all(root)?;
    let path = root.join("migrations.jsonl");
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, entry).map_err(crate::SessionStoreError::Index)?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}
