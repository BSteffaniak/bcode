use bcode_session_models::SessionId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::Path;
use thiserror::Error;

/// Action a migration plan would perform for session persistence metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMigrationAction {
    /// No action is needed.
    None,
    /// Rebuild derived session indexes from canonical event logs.
    RebuildDerivedIndex,
    /// Rewrite canonical session event logs to a newer schema.
    RewriteCanonicalEvents,
}

/// How a migration handles backups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMigrationBackupPolicy {
    /// No backup is needed because canonical data is not rewritten.
    NotRequired,
    /// A backup must exist before canonical data can be rewritten.
    Required,
}

/// How a migration may be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMigrationApplyPolicy {
    /// The migration is safe for automatic/background execution.
    Automatic,
    /// The migration requires explicit user action.
    Manual,
}

/// Registered session migration metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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

/// Canonical session event-log migration authoring interface.
///
/// Implementations provide only event transformation metadata/logic; the
/// session store owns backup, journaling, atomic replacement, validation, and
/// derived-index rebuilds when canonical migrations are introduced.
pub trait SessionEventLogMigration {
    /// Stable migration identifier.
    const ID: &'static str;
    /// Source event schema version.
    const FROM_SCHEMA: u16;
    /// Target event schema version.
    const TO_SCHEMA: u16;

    /// Migrate one event to the target schema.
    ///
    /// # Errors
    ///
    /// Returns an error when the event cannot be represented in the target schema.
    fn migrate_event(
        &self,
        event: bcode_session_models::SessionEvent,
    ) -> Result<bcode_session_models::SessionEvent, SessionEventLogMigrationError>;
}

/// Error returned by canonical event-log migrations.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SessionEventLogMigrationError {
    /// The event cannot be migrated.
    #[error("session event migration failed: {0}")]
    Message(String),
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct SessionMigrationOptions {
    /// Report actions without modifying files.
    pub dry_run: bool,
    /// Back up canonical event logs before applying migrations.
    pub backup: bool,
}

/// Result status for one migration item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMigrationApplyStatus {
    /// The item would be applied, but dry-run mode was requested.
    Planned,
    /// The item was applied.
    Applied,
    /// The item was skipped because it requires manual handling.
    Skipped,
}

/// Result for one migration item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionMigrationReportItem {
    pub migration_id: &'static str,
    pub session_id: SessionId,
    pub action: SessionMigrationAction,
    pub status: SessionMigrationApplyStatus,
    pub message: String,
}

/// Result from applying a session migration plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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
    pub domain: String,
    pub status: SessionMigrationJournalStatus,
    pub dry_run: bool,
    pub backup: bool,
    pub backup_dir: Option<String>,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub migration_ids: Vec<String>,
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

/// Recovery status derived from the migration journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMigrationRecoveryStatus {
    /// No incomplete or failed migration runs were found.
    Clean,
    /// One or more migration runs need attention.
    NeedsAttention(Vec<SessionMigrationRecoveryItem>),
}

/// Migration run requiring operator attention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionMigrationRecoveryItem {
    pub run_id: String,
    pub domain: String,
    pub status: SessionMigrationJournalStatus,
    pub backup_dir: Option<String>,
    pub migration_ids: Vec<String>,
    pub session_ids: Vec<SessionId>,
    pub error: Option<String>,
}

/// Read the session migration journal.
///
/// # Errors
///
/// Returns an error if the journal exists but cannot be read or decoded.
pub fn read_journal_entries(
    root: &Path,
) -> Result<Vec<SessionMigrationJournalEntry>, crate::SessionStoreError> {
    let path = root.join("migrations.jsonl");
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(crate::SessionStoreError::Io(error)),
    };
    let mut entries = Vec::new();
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        entries.push(serde_json::from_str(line).map_err(crate::SessionStoreError::Index)?);
    }
    Ok(entries)
}

/// Derive recovery status from journal entries.
#[must_use]
pub fn recovery_status_from_entries(
    entries: &[SessionMigrationJournalEntry],
) -> SessionMigrationRecoveryStatus {
    let mut runs = BTreeMap::new();
    for entry in entries {
        runs.insert(entry.run_id.clone(), entry.clone());
    }
    let items = runs
        .into_values()
        .filter(|entry| entry.status != SessionMigrationJournalStatus::Completed)
        .map(|entry| SessionMigrationRecoveryItem {
            run_id: entry.run_id,
            domain: entry.domain.clone(),
            status: entry.status,
            backup_dir: entry.backup_dir,
            migration_ids: entry.migration_ids,
            session_ids: entry.session_ids,
            error: entry.error,
        })
        .collect::<Vec<_>>();
    if items.is_empty() {
        SessionMigrationRecoveryStatus::Clean
    } else {
        SessionMigrationRecoveryStatus::NeedsAttention(items)
    }
}

/// Root-relative fixture path for session migration tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionMigrationFixture {
    /// Human-readable fixture name.
    pub name: &'static str,
    /// Path to the fixture directory or file relative to the session crate.
    pub path: &'static str,
}

/// Built-in session migration fixture declarations.
#[must_use]
pub const fn session_migration_fixtures() -> &'static [SessionMigrationFixture] {
    &[SessionMigrationFixture {
        name: "session-migration-fixture-readme",
        path: "fixtures/migrations/README.md",
    }]
}

/// Derive recovery status from journal entries and leftover migration temp files.
///
/// # Errors
///
/// Returns an error if the session directory cannot be scanned.
pub fn recovery_status(
    root: &Path,
    entries: &[SessionMigrationJournalEntry],
) -> Result<SessionMigrationRecoveryStatus, crate::SessionStoreError> {
    let mut items = match recovery_status_from_entries(entries) {
        SessionMigrationRecoveryStatus::Clean => Vec::new(),
        SessionMigrationRecoveryStatus::NeedsAttention(items) => items,
    };
    if root.exists() {
        for entry in fs::read_dir(root)? {
            let path = entry?.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("tmp")
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(".events.tmp"))
            {
                items.push(SessionMigrationRecoveryItem {
                    run_id: "leftover-temp-file".to_string(),
                    domain: "sessions/events".to_string(),
                    status: SessionMigrationJournalStatus::Failed,
                    backup_dir: None,
                    migration_ids: Vec::new(),
                    session_ids: Vec::new(),
                    error: Some(format!("leftover migration temp file: {}", path.display())),
                });
            }
        }
    }
    if items.is_empty() {
        Ok(SessionMigrationRecoveryStatus::Clean)
    } else {
        Ok(SessionMigrationRecoveryStatus::NeedsAttention(items))
    }
}

/// Register a derived session rebuild migration definition.
#[macro_export]
macro_rules! register_session_derived_rebuild {
    (id = $id:literal, domain = $domain:literal, version = $version:expr $(,)?) => {
        $crate::SessionMigrationDefinition {
            id: $id,
            domain: $domain,
            from_version: 0,
            to_version: $version,
            action: $crate::SessionMigrationAction::RebuildDerivedIndex,
            backup_policy: $crate::SessionMigrationBackupPolicy::NotRequired,
            apply_policy: $crate::SessionMigrationApplyPolicy::Automatic,
        }
    };
}

/// Register a canonical session event migration definition.
#[macro_export]
macro_rules! register_session_event_migration {
    (id = $id:literal, from = $from:expr, to = $to:expr $(,)?) => {
        $crate::SessionMigrationDefinition {
            id: $id,
            domain: "sessions/events",
            from_version: $from,
            to_version: $to,
            action: $crate::SessionMigrationAction::RewriteCanonicalEvents,
            backup_policy: $crate::SessionMigrationBackupPolicy::Required,
            apply_policy: $crate::SessionMigrationApplyPolicy::Manual,
        }
    };
}
