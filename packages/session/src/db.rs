#![allow(clippy::module_name_repetitions)]

//! Switchy-backed session database primitives.
//!
//! This module is the first CQRS/event-store database slice for Bcode sessions. It
//! intentionally keeps Turso-specific details at connection boundaries and uses
//! `switchy` database traits for migrations and repository operations.

use std::{fs, path::Path, sync::Arc, time::Duration};

use crate::persisted::{
    PersistedSessionEventError, decode_session_event, decode_session_event_degraded,
    encode_session_event,
};

use bcode_database_observability::ObservedDatabase;
use bcode_metrics::{DatabaseMetrics, DatabaseOperation, MetricsRegistry};
use bcode_session_models::{
    RuntimeWorkKind, RuntimeWorkStatus, SessionEvent, SessionEventKind, SessionHistoryCursor,
    SessionHistoryDirection, SessionHistoryPage, SessionHistoryQuery, SessionId,
    SessionInputHistoryEntry, SessionSummary, SessionTitleSource, ToolInvocationResult,
    ToolInvocationStreamEvent, WorkId,
};
use switchy::{
    database::{
        Database, DatabaseError, DatabaseValue,
        query::{FilterableQuery, SelectQuery, SortDirection},
    },
    schema::{
        MigrationError,
        discovery::code::{CodeMigration, CodeMigrationSource},
        runner::MigrationRunner,
    },
};
use thiserror::Error;

const GLOBAL_MIGRATIONS_TABLE: &str = "__bcode_global_migrations";
const SESSION_MIGRATIONS_TABLE: &str = "__bcode_session_migrations";
const DATABASE_OPEN_RETRY_ATTEMPTS: u32 = 7;
const DATABASE_OPEN_INITIAL_RETRY_DELAY: Duration = Duration::from_millis(25);
const DATABASE_OPEN_MAX_RETRY_DELAY: Duration = Duration::from_secs(2);
const DATABASE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Durable storage writer epoch understood by this session database implementation.
pub const CURRENT_SESSION_STORAGE_WRITER_EPOCH: u32 =
    crate::lease::CURRENT_SESSION_STORAGE_WRITER_EPOCH;
const LEGACY_SESSION_STORAGE_WRITER_EPOCH: u32 = 1;
const SESSION_STORAGE_CONTRACT_ID: i32 = 1;
const SESSION_STORAGE_CONTRACT_SCHEMA_VERSION: u32 = 1;
const MODEL_CONTEXT_PROJECTION_SCHEMA_VERSION: u32 = 2;
const MODEL_CONTEXT_PROJECTION_ID: i32 = 1;
const CONTEXT_OCCUPANCY_PROJECTION_SCHEMA_VERSION: u32 = 4;
const CONTEXT_OCCUPANCY_PROJECTION_ID: i32 = 1;

/// Errors returned by Switchy-backed session database operations.
#[derive(Debug, Error)]
pub enum SessionDbError {
    /// Database connection initialization failed.
    #[error("failed to initialize database connection: {0}")]
    Connection(#[from] switchy::database_connection::InitTursoError),
    /// Database operation failed.
    #[error(transparent)]
    Database(#[from] DatabaseError),
    /// Schema migration failed.
    #[error(transparent)]
    Migration(#[from] MigrationError),
    /// A filesystem operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Cross-process lease operation failed.
    #[error(transparent)]
    Lease(#[from] crate::lease::SessionLeaseError),
    /// Event serialization failed.
    #[error(transparent)]
    Serialize(#[from] serde_json::Error),
    /// Strict persisted event decode failed.
    #[error(transparent)]
    PersistedEvent(#[from] PersistedSessionEventError),
    /// The durable model-context projection exists but is not current.
    #[error(
        "model-context projection is stale: checkpoint #{checkpoint}, canonical history ends at #{expected}"
    )]
    ModelContextProjectionStale { checkpoint: u64, expected: u64 },
    /// The durable model-context projection schema is unsupported.
    #[error("unsupported model-context projection schema version {actual}; expected {expected}")]
    ModelContextProjectionVersion { actual: u64, expected: u64 },
    /// The durable session storage writer contract is absent or unsupported.
    #[error(
        "unsupported session storage writer epoch: actual={actual:?} expected={expected}; explicit migration is required"
    )]
    WriterIncompatible { actual: Option<u64>, expected: u64 },
    /// Canonical append attempted a sequence other than the next contiguous event.
    #[error("invalid canonical append sequence: expected #{expected}, found #{actual}")]
    InvalidCanonicalAppendSequence { expected: u64, actual: u64 },
    /// Canonical event sequences cannot produce a trustworthy incremental projection.
    #[error("invalid canonical event sequence for reindex: expected #{expected}, found #{actual}")]
    InvalidCanonicalSequence { expected: u64, actual: u64 },
    /// A materialized projection does not match the canonical event tail.
    #[error(
        "session DB projection is stale: {projection} checkpoint={checkpoint:?} expected={expected}"
    )]
    ProjectionStale {
        projection: &'static str,
        checkpoint: Option<u64>,
        expected: u64,
    },
    /// A materialized projection uses an unsupported schema.
    #[error(
        "unsupported session DB projection schema: {projection} actual={actual} expected={expected}"
    )]
    ProjectionIncompatible {
        projection: &'static str,
        actual: u64,
        expected: u64,
    },
    /// A compaction marker is malformed or internally inconsistent.
    #[error("invalid context compaction marker at event #{sequence}: {message}")]
    InvalidCompactionMarker { sequence: u64, message: String },
    /// A database row did not contain the expected column/type.
    #[error("invalid session database row: missing or invalid column {column}")]
    InvalidRow { column: String },
}

/// Result type for session DB operations.
pub type SessionDbResult<T> = Result<T, SessionDbError>;

/// Diagnostic state for the durable model-context projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelContextProjectionStatus {
    /// No projection state exists, so exact compatibility reads remain active.
    Missing,
    /// Projection state matches canonical history and the current schema.
    Fresh { checkpoint: u64 },
    /// Projection state trails or exceeds canonical history.
    Stale { checkpoint: u64, expected: u64 },
    /// Projection state uses an unsupported schema.
    Incompatible { actual: u64, expected: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaterializedProjection {
    /// Projected current session state.
    SessionState,
    /// User-authored input history.
    InputHistory,
    /// Transcript item spans for UI/history windows.
    Transcript,
    /// Active and completed tool-call rows.
    ToolRuns,
    /// Generic references from finalized plugin artifacts.
    ArtifactReferences,
    /// Runtime-work lifecycle rows.
    RuntimeWork,
    /// Authoritative current context occupancy.
    RequestContextOccupancy,
}

impl MaterializedProjection {
    const ALL: [Self; 7] = [
        Self::SessionState,
        Self::InputHistory,
        Self::Transcript,
        Self::ToolRuns,
        Self::ArtifactReferences,
        Self::RuntimeWork,
        Self::RequestContextOccupancy,
    ];

    /// Return all checkpointed materialized projections.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &Self::ALL
    }

    /// Return the schema version stored with this projection's checkpoint.
    #[must_use]
    pub const fn schema_version(self) -> u32 {
        match self {
            Self::RequestContextOccupancy => CONTEXT_OCCUPANCY_PROJECTION_SCHEMA_VERSION,
            Self::SessionState
            | Self::InputHistory
            | Self::Transcript
            | Self::ToolRuns
            | Self::ArtifactReferences
            | Self::RuntimeWork => 1,
        }
    }

    /// Return the stable projection checkpoint name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionState => "session_state",
            Self::InputHistory => "input_history",
            Self::Transcript => "transcript",
            Self::ToolRuns => "tool_runs",
            Self::ArtifactReferences => "artifact_references",
            Self::RuntimeWork => "runtime_work",
            Self::RequestContextOccupancy => "context_occupancy",
        }
    }
}

/// Return Bcode's legacy global catalog database path under `root`.
#[must_use]
pub fn global_catalog_db_path(root: &Path) -> std::path::PathBuf {
    root.join("catalog.db")
}

/// Return a build/compatibility-scoped catalog database path under `root`.
#[must_use]
pub fn namespaced_catalog_db_path(root: &Path, namespace: &str) -> std::path::PathBuf {
    root.join("catalogs").join(namespace).join("catalog.db")
}

/// Return Bcode's default per-session database path for `session_id`.
#[must_use]
pub fn session_db_path(root: &Path, session_id: SessionId) -> std::path::PathBuf {
    root.join(session_id.to_string()).join("session.db")
}

/// Typed tool-run projection row stored in a per-session database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRun {
    /// Provider/tool-call identifier.
    pub tool_call_id: String,
    /// Event sequence that requested the tool call.
    pub event_seq_start: u64,
    /// Event sequence that finished the tool call, when complete.
    pub event_seq_end: Option<u64>,
    /// Projection status, for example `running`, `complete`, or `error`.
    pub status: String,
    /// Tool name, when known.
    pub tool_name: Option<String>,
    /// Whether the completed tool call ended in error.
    pub is_error: Option<bool>,
}

/// Projected runtime-work row stored in a per-session database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeWorkProjection {
    /// Runtime work identifier.
    pub work_id: WorkId,
    /// Event sequence that started the work.
    pub event_seq_start: u64,
    /// Event sequence that finished the work, when terminal.
    pub event_seq_end: Option<u64>,
    /// Runtime work category.
    pub kind: RuntimeWorkKind,
    /// Display label.
    pub label: String,
    /// Current status.
    pub status: RuntimeWorkStatus,
    /// Parent runtime work id, when nested.
    pub parent_work_id: Option<WorkId>,
    /// Start timestamp, when known.
    pub started_at_ms: Option<u64>,
    /// Finish timestamp, when terminal and known.
    pub finished_at_ms: Option<u64>,
    /// Latest progress or terminal message, when known.
    pub message: Option<String>,
    /// Whether the work advertised cancellation support.
    pub cancellable: bool,
}

/// Typed session-state projection row stored in a per-session database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDbState {
    /// Session id owned by this state row.
    pub session_id: SessionId,
    /// Last durable event sequence applied to the state projection.
    pub last_event_seq: u64,
    /// Current explicit/display title, if any.
    pub title: Option<String>,
    /// Current working directory.
    pub working_directory: std::path::PathBuf,
    /// Current provider selection, if any.
    pub current_provider: Option<String>,
    /// Current model selection, if any.
    pub current_model: Option<String>,
    /// Current reasoning effort selection, if any.
    pub reasoning_effort: Option<String>,
    /// Current reasoning summary selection, if any.
    pub reasoning_summary: Option<String>,
    /// Projection-updated timestamp, when known.
    pub updated_at_ms: Option<u64>,
    /// Whether the input-history projection has at least one user message.
    pub has_user_message: bool,
    /// Latest compacted-through canonical sequence, if any.
    pub latest_compaction_sequence: Option<u64>,
}

/// Typed transcript projection row stored in a per-session database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptItem {
    /// Monotonic transcript sequence.
    pub transcript_seq: u64,
    /// First durable event sequence covered by this item.
    pub event_seq_start: u64,
    /// Last durable event sequence covered by this item.
    pub event_seq_end: u64,
    /// UI role, for example `user`, `assistant`, or `tool`.
    pub role: String,
    /// Transcript item kind.
    pub kind: String,
    /// Projection status, for example `complete`, `running`, or `error`.
    pub status: String,
    /// Optional display content.
    pub content: Option<String>,
}

/// Generic finalized artifact reference stored in a bounded session projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizedArtifactReference {
    /// Stable artifact identifier within the session.
    pub artifact_id: String,
    /// Plugin-owned reference key.
    pub reference_key: String,
    /// Plugin that produced the artifact.
    pub producer_plugin_id: String,
    /// Plugin-owned artifact schema identifier.
    pub schema: String,
    /// Artifact schema version.
    pub schema_version: u32,
    /// Storage URI, when the artifact is externally readable.
    pub storage_uri: Option<String>,
    /// Media type, when known.
    pub content_type: Option<String>,
    /// Referenced byte length, when known.
    pub byte_len: Option<u64>,
    /// Generic availability state supplied by the producer, when known.
    pub availability: Option<String>,
    /// Whether the producer marked this reference complete.
    pub complete: Option<bool>,
    /// Generic SHA-256 integrity digest, when supplied by the producer.
    pub checksum_sha256: Option<String>,
    /// Canonical event that finalized this reference.
    pub finalized_event_seq: u64,
}

/// Backend-agnostic handle for Bcode's global session catalog database.
#[derive(Debug, Clone)]
pub struct GlobalSessionDb {
    db: Arc<Box<dyn Database>>,
    _catalog_lock: Option<Arc<crate::lease::CatalogLockGuard>>,
}

impl GlobalSessionDb {
    /// Open the global session catalog database under `root` and apply cheap schema migrations.
    ///
    /// # Errors
    ///
    /// Returns an error if the catalog DB cannot be opened or migrated.
    pub async fn open_turso_in_root(root: &Path) -> SessionDbResult<Self> {
        Self::open_turso_in_root_observed(root, MetricsRegistry::disabled()).await
    }

    /// Open the global catalog under `root` with centralized observability.
    ///
    /// # Errors
    ///
    /// Returns an error if the catalog DB cannot be opened or migrated.
    pub async fn open_turso_in_root_observed(
        root: &Path,
        metrics: MetricsRegistry,
    ) -> SessionDbResult<Self> {
        let path = global_catalog_db_path(root);
        Self::open_turso_observed(&path, metrics).await
    }

    /// Open a build/compatibility-scoped session catalog database under `root`.
    ///
    /// # Errors
    ///
    /// Returns an error if the catalog DB cannot be opened or migrated.
    pub async fn open_turso_in_root_namespace(
        root: &Path,
        namespace: &str,
    ) -> SessionDbResult<Self> {
        Self::open_turso_in_root_namespace_observed(root, namespace, MetricsRegistry::disabled())
            .await
    }

    /// Open a build-scoped global catalog with centralized observability.
    ///
    /// # Errors
    ///
    /// Returns an error if the catalog DB cannot be opened or migrated.
    pub async fn open_turso_in_root_namespace_observed(
        root: &Path,
        namespace: &str,
        metrics: MetricsRegistry,
    ) -> SessionDbResult<Self> {
        let path = namespaced_catalog_db_path(root, namespace);
        Self::open_turso_observed(&path, metrics).await
    }

    /// Open the global session catalog database at `path` and apply cheap schema migrations.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * the Turso connection cannot be opened after bounded lock retries
    /// * schema migrations fail
    pub async fn open_turso(path: &Path) -> SessionDbResult<Self> {
        Self::open_turso_observed(path, MetricsRegistry::disabled()).await
    }

    /// Open the global catalog with centralized database observability.
    ///
    /// # Errors
    ///
    /// Returns an error if the catalog DB cannot be opened or migrated.
    pub async fn open_turso_observed(
        path: &Path,
        metrics: MetricsRegistry,
    ) -> SessionDbResult<Self> {
        let root = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(root)?;
        let catalog_lock = crate::lease::acquire_catalog_lock(root)?;
        let open_started = std::time::Instant::now();
        let db = init_turso_local_with_retry(path).await;
        DatabaseMetrics::new(metrics.clone(), "session_catalog", "turso").record(
            DatabaseOperation::Open,
            None,
            "none",
            db.is_ok(),
            u64::try_from(open_started.elapsed().as_millis()).unwrap_or(u64::MAX),
        );
        let db = db?;
        let db: Box<dyn Database> = Box::new(ObservedDatabase::new(
            db,
            metrics,
            "session_catalog",
            "turso",
        ));
        run_global_migrations(&*db).await?;
        Ok(Self {
            db: Arc::new(db),
            _catalog_lock: Some(Arc::new(catalog_lock)),
        })
    }

    /// Open the global session catalog database at `path` without acquiring the catalog lock.
    ///
    /// This is intended for maintenance code that already holds the catalog lock or is running in
    /// dry-run diagnostic mode.
    ///
    /// # Errors
    ///
    /// Returns an error if the Turso connection cannot be opened or schema migrations fail.
    pub async fn open_turso_without_catalog_lock(path: &Path) -> SessionDbResult<Self> {
        let db = init_turso_local_with_retry(path).await?;
        run_global_migrations(&*db).await?;
        Ok(Self {
            db: Arc::new(db),
            _catalog_lock: None,
        })
    }

    /// Upsert one session catalog row.
    ///
    /// # Errors
    ///
    /// Returns an error if the catalog write fails.
    pub async fn upsert_session(
        &self,
        summary: &SessionSummary,
        db_path: &Path,
    ) -> SessionDbResult<()> {
        let existing = self
            .db
            .select("sessions")
            .columns(&["session_id"])
            .where_eq("session_id", summary.id.to_string())
            .execute_first(&**self.db)
            .await?;
        let title = summary.name.clone();
        let working_directory = summary.working_directory.to_string_lossy().to_string();
        let db_path = db_path.to_string_lossy().to_string();
        if existing.is_some() {
            self.db
                .update("sessions")
                .value("db_path", db_path)
                .value("title", title)
                .value("working_directory", working_directory)
                .value("created_at_ms", seq_to_value(summary.created_at_ms))
                .value("updated_at_ms", seq_to_value(summary.updated_at_ms))
                .value("state", "active")
                .value("projection_status", "fresh")
                .where_eq("session_id", summary.id.to_string())
                .execute(&**self.db)
                .await?;
        } else {
            self.db
                .insert("sessions")
                .value("session_id", summary.id.to_string())
                .value("db_path", db_path)
                .value("title", title)
                .value("working_directory", working_directory)
                .value("created_at_ms", seq_to_value(summary.created_at_ms))
                .value("updated_at_ms", seq_to_value(summary.updated_at_ms))
                .value("state", "active")
                .value("projection_status", "fresh")
                .execute(&**self.db)
                .await?;
        }
        Ok(())
    }

    /// Remove a session catalog row.
    ///
    /// # Errors
    ///
    /// Returns an error if the delete fails.
    pub async fn delete_session(&self, session_id: SessionId) -> SessionDbResult<()> {
        self.db
            .delete("sessions")
            .where_eq("session_id", session_id.to_string())
            .execute(&**self.db)
            .await?;
        Ok(())
    }

    /// Read all global catalog session rows.
    ///
    /// # Errors
    ///
    /// Returns an error if the query or row conversion fails.
    pub async fn list_sessions(&self) -> SessionDbResult<Vec<SessionSummary>> {
        self.db
            .select("sessions")
            .columns(&[
                "session_id",
                "title",
                "working_directory",
                "created_at_ms",
                "updated_at_ms",
            ])
            .sort("updated_at_ms", SortDirection::Desc)
            .execute(&**self.db)
            .await?
            .iter()
            .map(session_summary_from_catalog_row)
            .collect()
    }

    /// Return a launch-directory-scoped draft-session composer draft.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails or the row is malformed.
    pub async fn draft_session_composer_draft(
        &self,
        launch_working_directory: &Path,
    ) -> SessionDbResult<Option<String>> {
        let scope_key = launch_working_directory.to_string_lossy().to_string();
        let row = self
            .db
            .select("composer_drafts")
            .columns(&["text"])
            .where_eq("scope_kind", "draft_session")
            .where_eq("scope_key", scope_key)
            .execute_first(&**self.db)
            .await?;
        row.as_ref()
            .map(|row| required_string(row, "text"))
            .transpose()
    }

    /// Upsert or clear a launch-directory-scoped draft-session composer draft.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    pub async fn set_draft_session_composer_draft(
        &self,
        launch_working_directory: &Path,
        text: &str,
        updated_at_ms: u64,
    ) -> SessionDbResult<()> {
        let scope_key = launch_working_directory.to_string_lossy().to_string();
        if text.is_empty() {
            self.db
                .delete("composer_drafts")
                .where_eq("scope_kind", "draft_session")
                .where_eq("scope_key", scope_key)
                .execute(&**self.db)
                .await?;
            return Ok(());
        }
        let existing = self
            .db
            .select("composer_drafts")
            .columns(&["scope_key"])
            .where_eq("scope_kind", "draft_session")
            .where_eq("scope_key", scope_key.clone())
            .execute_first(&**self.db)
            .await?;
        if existing.is_some() {
            self.db
                .update("composer_drafts")
                .value("launch_working_directory", scope_key.clone())
                .value("text", text.to_owned())
                .value("updated_at_ms", seq_to_value(updated_at_ms))
                .where_eq("scope_kind", "draft_session")
                .where_eq("scope_key", scope_key)
                .execute(&**self.db)
                .await?;
        } else {
            self.db
                .insert("composer_drafts")
                .value("scope_kind", "draft_session")
                .value("scope_key", scope_key.clone())
                .value("launch_working_directory", scope_key)
                .value("text", text.to_owned())
                .value("updated_at_ms", seq_to_value(updated_at_ms))
                .execute(&**self.db)
                .await?;
        }
        Ok(())
    }

    /// Return the underlying database trait object.
    #[must_use]
    pub fn database(&self) -> &dyn Database {
        &**self.db
    }
}

/// Backend-agnostic handle for one isolated session database.
#[derive(Debug, Clone)]
pub struct SessionDb {
    session_id: SessionId,
    db: Arc<Box<dyn Database>>,
}

impl SessionDb {
    /// Open an existing session database without applying migrations or rebuilding projections.
    ///
    /// This is the normal runtime/read path. It fails when the database file does not exist and
    /// never creates parent directories, runs DDL, or replays canonical events.
    ///
    /// # Errors
    ///
    /// Returns an error if the database does not exist or cannot be opened.
    pub async fn open_existing_turso_in_root(
        session_id: SessionId,
        root: &Path,
    ) -> SessionDbResult<Self> {
        Self::open_existing_turso_in_root_observed(session_id, root, MetricsRegistry::disabled())
            .await
    }

    /// Open an existing session database without mutation, with observability.
    ///
    /// # Errors
    ///
    /// Returns an error if the database does not exist or cannot be opened.
    pub async fn open_existing_turso_in_root_observed(
        session_id: SessionId,
        root: &Path,
        metrics: MetricsRegistry,
    ) -> SessionDbResult<Self> {
        let path = session_db_path(root, session_id);
        Self::open_existing_turso_observed(session_id, &path, metrics).await
    }

    /// Initialize a new session database with the complete current schema.
    ///
    /// # Errors
    ///
    /// Returns an error if the database already exists, cannot be created, or migrations fail.
    pub async fn initialize_turso_in_root(
        session_id: SessionId,
        root: &Path,
    ) -> SessionDbResult<Self> {
        let path = session_db_path(root, session_id);
        if path.exists() {
            return Err(SessionDbError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("session database already exists at {}", path.display()),
            )));
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Self::initialize_turso_observed(session_id, &path, MetricsRegistry::disabled()).await
    }

    /// Explicitly migrate an existing database while exclusive maintenance and write guards are
    /// held.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened, migrated, or reprojected.
    pub async fn migrate_turso_in_root(
        session_id: SessionId,
        root: &Path,
        _maintenance: &crate::lease::SessionMaintenanceGuard,
        _write: &crate::lease::SessionWriteGuard,
    ) -> SessionDbResult<Self> {
        let path = session_db_path(root, session_id);
        let db = Self::open_existing_turso_observed(session_id, &path, MetricsRegistry::disabled())
            .await?;
        run_session_migrations(&**db.db).await?;
        migrate_session_storage(&**db.db).await?;
        Ok(db)
    }

    /// Open one session database under `root`, initializing only when it does not yet exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the session directory cannot be created, the database cannot be
    /// opened, or schema migrations fail.
    pub async fn open_turso_in_root(session_id: SessionId, root: &Path) -> SessionDbResult<Self> {
        Self::open_turso_in_root_observed(session_id, root, MetricsRegistry::disabled()).await
    }

    /// Open one session database under `root` with centralized observability.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened or migrated.
    pub async fn open_turso_in_root_observed(
        session_id: SessionId,
        root: &Path,
        metrics: MetricsRegistry,
    ) -> SessionDbResult<Self> {
        let path = session_db_path(root, session_id);
        if path.exists() {
            Self::open_existing_turso_observed(session_id, &path, metrics).await
        } else {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            Self::initialize_turso_observed(session_id, &path, metrics).await
        }
    }

    /// Open an existing database at `path` without applying migrations, or initialize a new one.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened or initialized.
    pub async fn open_turso(session_id: SessionId, path: &Path) -> SessionDbResult<Self> {
        if path.exists() {
            Self::open_existing_turso_observed(session_id, path, MetricsRegistry::disabled()).await
        } else {
            Self::initialize_turso_observed(session_id, path, MetricsRegistry::disabled()).await
        }
    }

    async fn open_existing_turso_observed(
        session_id: SessionId,
        path: &Path,
        metrics: MetricsRegistry,
    ) -> SessionDbResult<Self> {
        if !path.exists() {
            return Err(SessionDbError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("session database does not exist at {}", path.display()),
            )));
        }
        Self::connect_turso_observed(session_id, path, metrics).await
    }

    async fn initialize_turso_observed(
        session_id: SessionId,
        path: &Path,
        metrics: MetricsRegistry,
    ) -> SessionDbResult<Self> {
        let db = Self::connect_turso_observed(session_id, path, metrics).await?;
        run_session_migrations(&**db.db).await?;
        initialize_current_storage_contract(&**db.db).await?;
        Ok(db)
    }

    async fn connect_turso_observed(
        session_id: SessionId,
        path: &Path,
        metrics: MetricsRegistry,
    ) -> SessionDbResult<Self> {
        let open_started = std::time::Instant::now();
        let db = init_turso_local_with_retry(path).await;
        DatabaseMetrics::new(metrics.clone(), "session", "turso").record(
            DatabaseOperation::Open,
            None,
            "none",
            db.is_ok(),
            u64::try_from(open_started.elapsed().as_millis()).unwrap_or(u64::MAX),
        );
        let db = db?;
        let db: Box<dyn Database> =
            Box::new(ObservedDatabase::new(db, metrics, "session", "turso"));
        Ok(Self {
            session_id,
            db: Arc::new(db),
        })
    }

    /// Return the current composer draft, if one is persisted for this session.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails or the row is malformed.
    pub async fn session_composer_draft(&self) -> SessionDbResult<Option<String>> {
        let row = self
            .db
            .select("session_drafts")
            .columns(&["text"])
            .where_eq("session_id", self.session_id.to_string())
            .execute_first(&**self.db)
            .await?;
        row.as_ref()
            .map(|row| required_string(row, "text"))
            .transpose()
    }

    /// Upsert or clear this session's composer draft.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    pub async fn set_session_composer_draft(
        &self,
        text: &str,
        updated_at_ms: u64,
    ) -> SessionDbResult<()> {
        validate_storage_writer_contract(&**self.db).await?;
        if text.is_empty() {
            self.db
                .delete("session_drafts")
                .where_eq("session_id", self.session_id.to_string())
                .execute(&**self.db)
                .await?;
            return Ok(());
        }
        let existing = self
            .db
            .select("session_drafts")
            .columns(&["session_id"])
            .where_eq("session_id", self.session_id.to_string())
            .execute_first(&**self.db)
            .await?;
        if existing.is_some() {
            self.db
                .update("session_drafts")
                .value("text", text.to_owned())
                .value("updated_at_ms", seq_to_value(updated_at_ms))
                .where_eq("session_id", self.session_id.to_string())
                .execute(&**self.db)
                .await?;
        } else {
            self.db
                .insert("session_drafts")
                .value("session_id", self.session_id.to_string())
                .value("text", text.to_owned())
                .value("updated_at_ms", seq_to_value(updated_at_ms))
                .execute(&**self.db)
                .await?;
        }
        Ok(())
    }

    /// Return tool-run projection rows with `running` status.
    ///
    /// # Errors
    ///
    /// Returns an error if the projection query fails or a row is malformed.
    pub async fn active_tool_runs(&self) -> SessionDbResult<Vec<ToolRun>> {
        self.tool_runs_by_status("running").await
    }

    /// Return tool-run projection rows matching `status`.
    ///
    /// # Errors
    ///
    /// Returns an error if the projection query fails or a row is malformed.
    pub async fn tool_runs_by_status(&self, status: &str) -> SessionDbResult<Vec<ToolRun>> {
        let rows = self
            .db
            .select("tool_runs")
            .columns(&[
                "tool_call_id",
                "event_seq_start",
                "event_seq_end",
                "status",
                "tool_name",
                "is_error",
            ])
            .where_eq("status", status)
            .sort("event_seq_start", SortDirection::Asc)
            .execute(&**self.db)
            .await?;
        rows.iter().map(tool_run_from_row).collect()
    }

    /// Return active runtime-work projection rows.
    ///
    /// # Errors
    ///
    /// Returns an error if the projection query fails or a row is malformed.
    pub async fn active_runtime_work(&self) -> SessionDbResult<Vec<RuntimeWorkProjection>> {
        let rows = self
            .db
            .select("runtime_work")
            .columns(&[
                "work_id",
                "event_seq_start",
                "event_seq_end",
                "kind",
                "label",
                "status",
                "parent_work_id",
                "started_at_ms",
                "finished_at_ms",
                "message",
                "cancellable",
            ])
            .where_eq(
                "status",
                runtime_work_status_name(RuntimeWorkStatus::Running),
            )
            .sort("event_seq_start", SortDirection::Asc)
            .execute(&**self.db)
            .await?;
        rows.iter().map(runtime_work_from_row).collect()
    }

    /// Return latest runtime-work projection rows.
    ///
    /// # Errors
    ///
    /// Returns an error if the projection query fails or a row is malformed.
    pub async fn runtime_work_history(
        &self,
        limit: usize,
    ) -> SessionDbResult<Vec<RuntimeWorkProjection>> {
        let mut query = self
            .db
            .select("runtime_work")
            .columns(&[
                "work_id",
                "event_seq_start",
                "event_seq_end",
                "kind",
                "label",
                "status",
                "parent_work_id",
                "started_at_ms",
                "finished_at_ms",
                "message",
                "cancellable",
            ])
            .sort("event_seq_start", SortDirection::Desc);
        if limit > 0 {
            query = query.limit(limit);
        }
        let mut work = query
            .execute(&**self.db)
            .await?
            .iter()
            .map(runtime_work_from_row)
            .collect::<SessionDbResult<Vec<_>>>()?;
        work.reverse();
        Ok(work)
    }

    /// Return the current projected session state row.
    ///
    /// # Errors
    ///
    /// Returns an error if the state query fails or the row is malformed.
    pub async fn session_state(&self) -> SessionDbResult<Option<SessionDbState>> {
        let Some(row) = self
            .db
            .select("session_state")
            .columns(&[
                "session_id",
                "last_event_seq",
                "current_model",
                "current_provider",
                "working_directory",
                "title",
                "reasoning_effort",
                "reasoning_summary",
                "updated_at_ms",
            ])
            .where_eq("session_id", self.session_id.to_string())
            .execute_first(&**self.db)
            .await?
        else {
            return Ok(None);
        };
        let row_session_id = required_string(&row, "session_id")?.parse().map_err(|_| {
            SessionDbError::InvalidRow {
                column: "session_id".to_string(),
            }
        })?;
        let has_user_message = self.input_message_count().await? > 0;
        let latest_compaction_sequence = self.latest_context_compaction_sequence().await?;
        Ok(Some(SessionDbState {
            session_id: row_session_id,
            last_event_seq: required_i64(&row, "last_event_seq").map(i64_to_u64)?,
            title: optional_string(&row, "title"),
            working_directory: std::path::PathBuf::from(required_string(
                &row,
                "working_directory",
            )?),
            current_provider: optional_string(&row, "current_provider"),
            current_model: optional_string(&row, "current_model"),
            reasoning_effort: optional_string(&row, "reasoning_effort"),
            reasoning_summary: optional_string(&row, "reasoning_summary"),
            updated_at_ms: row
                .get("updated_at_ms")
                .and_then(|value| value.as_i64())
                .map(i64_to_u64),
            has_user_message,
            latest_compaction_sequence,
        }))
    }

    async fn input_message_count(&self) -> SessionDbResult<usize> {
        Ok(self
            .db
            .select("input_messages")
            .columns(&["event_seq"])
            .execute(&**self.db)
            .await?
            .len())
    }

    async fn latest_context_compaction_sequence(&self) -> SessionDbResult<Option<u64>> {
        Ok(self
            .latest_context_compaction_event()
            .await?
            .and_then(|event| match event.kind {
                SessionEventKind::ContextCompacted {
                    compacted_through_sequence,
                    ..
                }
                | SessionEventKind::ProviderContextCompacted {
                    compacted_through_sequence,
                    ..
                } => Some(compacted_through_sequence),
                _ => None,
            }))
    }

    /// Return the session id owned by this database.
    #[must_use]
    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Return the underlying database trait object.
    #[must_use]
    pub fn database(&self) -> &dyn Database {
        &**self.db
    }

    /// Return the durable storage writer epoch recorded by this session database.
    ///
    /// # Errors
    ///
    /// Returns an error if the contract row is malformed. A missing row is reported as the known
    /// legacy writer epoch so diagnostics can distinguish migration-required legacy state from an
    /// unknown future writer.
    pub async fn storage_writer_epoch(&self) -> SessionDbResult<u64> {
        let row = self
            .db
            .select("session_storage_contract")
            .columns(&["writer_epoch"])
            .where_eq("contract_id", SESSION_STORAGE_CONTRACT_ID)
            .execute_first(&**self.db)
            .await?;
        row.as_ref().map_or_else(
            || Ok(u64::from(LEGACY_SESSION_STORAGE_WRITER_EPOCH)),
            |row| required_i64(row, "writer_epoch").map(i64_to_u64),
        )
    }

    /// Return a turn receipt by producer-scoped idempotency identity.
    ///
    /// # Errors
    ///
    /// Returns an error if the bounded indexed query fails or contains malformed identity data.
    pub async fn turn_receipt(
        &self,
        producer: &str,
        idempotency_key: &str,
    ) -> SessionDbResult<Option<bcode_session_models::TurnReceipt>> {
        let row = self
            .db
            .select("turn_receipts")
            .columns(&["accepted_event_seq", "turn_id", "work_id"])
            .where_eq("producer", producer.to_owned())
            .where_eq("idempotency_key", idempotency_key.to_owned())
            .execute_first(&**self.db)
            .await?;
        row.as_ref()
            .map(|row| {
                Ok(bcode_session_models::TurnReceipt {
                    accepted_event_sequence: i64_to_u64(required_i64(row, "accepted_event_seq")?),
                    turn_id: bcode_session_models::TurnId(required_string(row, "turn_id")?),
                    work_id: WorkId::new(required_string(row, "work_id")?),
                })
            })
            .transpose()
    }

    /// Append an event and update first-class projections in one transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if event serialization, event insertion, projection updates, or commit
    /// fail.
    pub async fn append_event(&self, event: &SessionEvent) -> SessionDbResult<()> {
        self.append_event_with_activity_timestamp(event, None).await
    }

    /// Append an event with the manager-assigned activity timestamp.
    ///
    /// # Errors
    ///
    /// Returns an error if event serialization, event insertion, projection updates, or commit
    /// fail.
    async fn append_event_for_writer_epoch(
        &self,
        event: &SessionEvent,
        activity_timestamp_ms: Option<u64>,
        writer_epoch: u32,
    ) -> SessionDbResult<()> {
        let tx = self.db.begin_transaction().await?;
        validate_storage_writer_contract_for_epoch(&*tx, writer_epoch).await?;
        validate_append_preconditions_without_writer(&*tx, event).await?;
        insert_event(&*tx, event, activity_timestamp_ms).await?;
        project_materialized_event(&*tx, event).await?;
        project_model_context_event(&*tx, event).await?;
        project_context_occupancy_event(&*tx, event).await?;
        project_turn_receipt(&*tx, event).await?;
        validate_append_postconditions(&*tx, event).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Append an event with the manager-assigned activity timestamp.
    ///
    /// # Errors
    ///
    /// Returns an error if writer compatibility, event insertion, projection updates, or commit
    /// fail.
    pub async fn append_event_with_activity_timestamp(
        &self,
        event: &SessionEvent,
        activity_timestamp_ms: Option<u64>,
    ) -> SessionDbResult<()> {
        self.append_event_for_writer_epoch(
            event,
            activity_timestamp_ms,
            CURRENT_SESSION_STORAGE_WRITER_EPOCH,
        )
        .await
    }
    /// Return the last event sequence processed by a materialized projection, if known.
    ///
    /// # Errors
    ///
    /// Returns an error if the checkpoint query fails or returns an invalid row.
    pub async fn materialized_projection_checkpoint(
        &self,
        projection: MaterializedProjection,
    ) -> SessionDbResult<Option<u64>> {
        self.projection_checkpoint(projection.as_str()).await
    }

    async fn projection_checkpoint(&self, projection_name: &str) -> SessionDbResult<Option<u64>> {
        let row = self
            .db
            .select("projection_checkpoints")
            .columns(&["last_event_seq"])
            .where_eq("projection_name", projection_name)
            .execute_first(&**self.db)
            .await?;

        row.as_ref()
            .map(|row| required_i64(row, "last_event_seq").map(i64_to_u64))
            .transpose()
    }

    /// Return input history from the indexed projection table.
    ///
    /// # Errors
    ///
    /// Returns an error if the projection query fails.
    pub async fn input_history(&self) -> SessionDbResult<Vec<SessionInputHistoryEntry>> {
        let rows = self
            .db
            .select("input_messages")
            .columns(&["event_seq", "created_at_ms", "text"])
            .sort("input_seq", SortDirection::Asc)
            .execute(&**self.db)
            .await?;

        rows.iter().map(input_history_entry_from_row).collect()
    }

    /// Return `(created_at_ms, updated_at_ms)` from stored event activity timestamps.
    ///
    /// # Errors
    ///
    /// Returns an error if bounded event timestamp queries fail or rows are malformed.
    pub async fn activity_bounds(&self) -> SessionDbResult<Option<(u64, u64)>> {
        let first = self
            .db
            .select("events")
            .columns(&["created_at_ms"])
            .sort("event_seq", SortDirection::Asc)
            .limit(1)
            .execute_first(&**self.db)
            .await?
            .and_then(|row| row.get("created_at_ms").and_then(|value| value.as_i64()))
            .map(i64_to_u64);
        let last = self
            .db
            .select("events")
            .columns(&["created_at_ms"])
            .sort("event_seq", SortDirection::Desc)
            .limit(1)
            .execute_first(&**self.db)
            .await?
            .and_then(|row| row.get("created_at_ms").and_then(|value| value.as_i64()))
            .map(i64_to_u64);
        Ok(first.zip(last))
    }

    /// Return all canonical events in sequence order, skipping unsupported or
    /// corrupt persisted records for normal user-facing history reads.
    ///
    /// # Errors
    ///
    /// Returns an error if the query or event deserialization fails.
    pub async fn all_events(&self) -> SessionDbResult<Vec<SessionEvent>> {
        let rows = self
            .db
            .select("events")
            .columns(&["payload"])
            .sort("event_seq", SortDirection::Asc)
            .execute(&**self.db)
            .await?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let payload = required_string(&row, "payload")?;
            if let Some(event) = decode_session_event_degraded(&payload) {
                events.push(event);
            }
        }
        Ok(events)
    }

    /// Return all canonical events in sequence order using strict persisted DTO
    /// decoding.
    ///
    /// Repair, doctor, reindex, and migration flows should use this instead of
    /// [`Self::all_events`] so damaged records are surfaced rather than skipped.
    ///
    /// # Errors
    ///
    /// Returns an error if the query or event deserialization fails.
    pub async fn all_events_strict(&self) -> SessionDbResult<Vec<SessionEvent>> {
        strict_events_from_database(&**self.db).await
    }

    /// Return a bounded history page from canonical DB events.
    ///
    /// # Errors
    ///
    /// Returns an error if event queries or deserialization fail.
    pub async fn history_page(
        &self,
        query: SessionHistoryQuery,
    ) -> SessionDbResult<SessionHistoryPage> {
        let limit = query.limit.max(1);
        let fetch_limit = limit.saturating_add(1);
        let mut select = self
            .db
            .select("events")
            .columns(&["event_seq", "payload"])
            .limit(fetch_limit);
        select = match query.direction {
            SessionHistoryDirection::Forward => {
                let select = if let Some(cursor) = query.cursor {
                    select.where_gte("event_seq", seq_to_value(cursor.sequence))
                } else {
                    select
                };
                select.sort("event_seq", SortDirection::Asc)
            }
            SessionHistoryDirection::Backward => {
                let select = if let Some(cursor) = query.cursor {
                    select.where_lte("event_seq", seq_to_value(cursor.sequence))
                } else {
                    select
                };
                select.sort("event_seq", SortDirection::Desc)
            }
        };

        let rows = select.execute(&**self.db).await?;
        let has_more = rows.len() > limit;
        let mut events = Vec::with_capacity(limit.min(rows.len()));
        for row in rows.iter().take(limit) {
            let payload = required_string(row, "payload")?;
            if let Some(event) = decode_session_event_degraded(&payload) {
                events.push(event);
            }
        }
        if matches!(query.direction, SessionHistoryDirection::Backward) {
            events.reverse();
        }
        let next_cursor = if has_more {
            events.first().map(|event| SessionHistoryCursor {
                sequence: event.sequence,
            })
        } else {
            None
        };
        Ok(SessionHistoryPage {
            session_id: self.session_id,
            events,
            next_cursor,
            has_more,
        })
    }

    /// Return canonical generic plugin automation events for one operation.
    ///
    /// # Errors
    ///
    /// Returns an error if event queries or deserialization fail.
    pub async fn plugin_automation_operation_events(
        &self,
        plugin_id: &str,
        operation_id: &str,
    ) -> SessionDbResult<Vec<SessionEvent>> {
        let rows = self
            .db
            .select("events")
            .columns(&["event_seq", "payload"])
            .where_in(
                "event_type",
                vec![
                    DatabaseValue::String("plugin_automation_turn_started".to_owned()),
                    DatabaseValue::String("plugin_automation_turn_finished".to_owned()),
                ],
            )
            .sort("event_seq", SortDirection::Asc)
            .execute(&**self.db)
            .await?;
        let mut events = Vec::new();
        for row in rows {
            let payload = required_string(&row, "payload")?;
            let Some(event) = decode_session_event_degraded(&payload) else {
                continue;
            };
            let matches_operation = match &event.kind {
                SessionEventKind::PluginAutomationTurnStarted {
                    plugin_id: event_plugin_id,
                    operation_id: event_operation_id,
                    ..
                }
                | SessionEventKind::PluginAutomationTurnFinished {
                    plugin_id: event_plugin_id,
                    operation_id: event_operation_id,
                    ..
                } => event_plugin_id == plugin_id && event_operation_id == operation_id,
                _ => false,
            };
            if matches_operation {
                events.push(event);
            }
        }
        Ok(events)
    }

    /// Return canonical plugin status-note events for one stable note identity.
    ///
    /// # Errors
    ///
    /// Returns an error if event queries or deserialization fail.
    pub async fn plugin_status_note_events(
        &self,
        plugin_id: &str,
        note_id: &str,
    ) -> SessionDbResult<Vec<SessionEvent>> {
        let rows = self
            .db
            .select("events")
            .columns(&["event_seq", "payload"])
            .where_eq(
                "event_type",
                DatabaseValue::String("plugin_status_note".to_owned()),
            )
            .sort("event_seq", SortDirection::Asc)
            .execute(&**self.db)
            .await?;
        let mut events = Vec::new();
        for row in rows {
            let payload = required_string(&row, "payload")?;
            let Some(event) = decode_session_event_degraded(&payload) else {
                continue;
            };
            if matches!(
                &event.kind,
                SessionEventKind::PluginStatusNote {
                    plugin_id: event_plugin_id,
                    note_id: event_note_id,
                    ..
                } if event_plugin_id == plugin_id && event_note_id == note_id
            ) {
                events.push(event);
            }
        }
        Ok(events)
    }

    /// Return model-context events from canonical DB events and indexed compaction metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if event queries or deserialization fail.
    pub async fn model_context_events(&self) -> SessionDbResult<Vec<SessionEvent>> {
        if let Some(events) = self.projected_model_context_events().await? {
            return Ok(events);
        }
        self.compatibility_model_context_events().await
    }

    /// Inspect model-context projection freshness without rebuilding or mutating it.
    ///
    /// # Errors
    ///
    /// Returns an error if projection state or canonical sequence queries fail.
    pub async fn model_context_projection_status(
        &self,
    ) -> SessionDbResult<ModelContextProjectionStatus> {
        let Some(state) = self
            .db
            .select("model_context_projection_state")
            .columns(&["schema_version", "last_event_seq"])
            .where_eq("projection_id", MODEL_CONTEXT_PROJECTION_ID)
            .execute_first(&**self.db)
            .await?
        else {
            return Ok(ModelContextProjectionStatus::Missing);
        };
        let schema_version = required_i64(&state, "schema_version").map(i64_to_u64)?;
        if schema_version != u64::from(MODEL_CONTEXT_PROJECTION_SCHEMA_VERSION) {
            return Ok(ModelContextProjectionStatus::Incompatible {
                actual: schema_version,
                expected: u64::from(MODEL_CONTEXT_PROJECTION_SCHEMA_VERSION),
            });
        }
        let checkpoint = required_i64(&state, "last_event_seq").map(i64_to_u64)?;
        let expected = self.last_event_sequence().await?.unwrap_or_default();
        if checkpoint == expected {
            Ok(ModelContextProjectionStatus::Fresh { checkpoint })
        } else {
            Ok(ModelContextProjectionStatus::Stale {
                checkpoint,
                expected,
            })
        }
    }

    /// Explicitly rebuild the model-context projection from strict canonical history.
    ///
    /// This maintenance operation is never called by normal read or append paths.
    ///
    /// # Errors
    ///
    /// Returns an error if canonical history is invalid or projection replacement fails.
    pub async fn reindex_model_context(
        &self,
        _maintenance: &crate::lease::SessionMaintenanceGuard,
        _write: &crate::lease::SessionWriteGuard,
    ) -> SessionDbResult<usize> {
        let tx = self.db.begin_transaction().await?;
        let event_count = rebuild_model_context_projection(&*tx).await?;
        tx.commit().await?;
        Ok(event_count)
    }

    async fn projected_model_context_events(&self) -> SessionDbResult<Option<Vec<SessionEvent>>> {
        let Some(state) = self
            .db
            .select("model_context_projection_state")
            .columns(&["schema_version", "last_event_seq"])
            .where_eq("projection_id", MODEL_CONTEXT_PROJECTION_ID)
            .execute_first(&**self.db)
            .await?
        else {
            return Ok(None);
        };
        let schema_version = required_i64(&state, "schema_version").map(i64_to_u64)?;
        if schema_version != u64::from(MODEL_CONTEXT_PROJECTION_SCHEMA_VERSION) {
            return Err(SessionDbError::ModelContextProjectionVersion {
                actual: schema_version,
                expected: u64::from(MODEL_CONTEXT_PROJECTION_SCHEMA_VERSION),
            });
        }
        let checkpoint = required_i64(&state, "last_event_seq").map(i64_to_u64)?;
        let expected = self.last_event_sequence().await?.unwrap_or_default();
        if checkpoint != expected {
            return Err(SessionDbError::ModelContextProjectionStale {
                checkpoint,
                expected,
            });
        }
        let rows = self
            .db
            .select("model_context_entries")
            .columns(&["event_seq", "event_type", "payload"])
            .sort("event_seq", SortDirection::Asc)
            .execute(&**self.db)
            .await?;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let event_seq = required_i64(&row, "event_seq").map(i64_to_u64)?;
            let event_type = required_string(&row, "event_type")?;
            let payload = required_string(&row, "payload")?;
            let event = decode_session_event(&payload)?;
            if event.sequence != event_seq
                || model_context_event_kind_name(&event.kind) != event_type
                || !is_model_context_event_type(&event_type)
            {
                return Err(SessionDbError::InvalidRow {
                    column: "model_context_entries".to_string(),
                });
            }
            events.push(event);
        }
        Ok(Some(canonical_model_context_from_events(events)))
    }

    async fn compatibility_model_context_events(&self) -> SessionDbResult<Vec<SessionEvent>> {
        let compaction_event = self.latest_context_compaction_event().await?;
        let compacted_through_sequence =
            compaction_event
                .as_ref()
                .and_then(|event| match &event.kind {
                    SessionEventKind::ContextCompacted {
                        compacted_through_sequence,
                        ..
                    }
                    | SessionEventKind::ProviderContextCompacted {
                        compacted_through_sequence,
                        ..
                    } => Some(*compacted_through_sequence),
                    _ => None,
                });
        let rows = model_context_events_query(&**self.db, compacted_through_sequence)
            .execute(&**self.db)
            .await?;
        let mut candidates = Vec::with_capacity(
            rows.len()
                .saturating_add(usize::from(compaction_event.is_some())),
        );
        if let Some(event) = compaction_event {
            candidates.push(event);
        }
        for row in rows {
            let payload = required_string(&row, "payload")?;
            if let Some(event) = decode_session_event_degraded(&payload) {
                candidates.push(event);
            }
        }
        Ok(canonical_model_context_from_events(candidates))
    }

    /// Return the authoritative current context generation.
    ///
    /// # Errors
    ///
    /// Returns an error when the projection row is missing or malformed.
    pub async fn current_context_epoch(&self) -> SessionDbResult<u64> {
        let row = self
            .db
            .select("context_occupancy_projection")
            .columns(&["schema_version", "context_epoch"])
            .where_eq("projection_id", CONTEXT_OCCUPANCY_PROJECTION_ID)
            .execute_first(&**self.db)
            .await?;
        let Some(row) = row.as_ref() else {
            return Ok(0);
        };
        let schema_version = required_i64(row, "schema_version").map(i64_to_u64)?;
        if schema_version != u64::from(CONTEXT_OCCUPANCY_PROJECTION_SCHEMA_VERSION) {
            return Err(SessionDbError::ProjectionIncompatible {
                projection: MaterializedProjection::RequestContextOccupancy.as_str(),
                actual: schema_version,
                expected: u64::from(CONTEXT_OCCUPANCY_PROJECTION_SCHEMA_VERSION),
            });
        }
        required_i64(row, "context_epoch").map(i64_to_u64)
    }

    /// Return the authoritative current context occupancy.
    ///
    /// # Errors
    ///
    /// Returns an error when the projection is stale, incompatible, or malformed.
    pub async fn current_context_occupancy(
        &self,
    ) -> SessionDbResult<Option<bcode_session_models::RequestContextOccupancy>> {
        let expected = self.last_event_sequence().await?.unwrap_or_default();
        let checkpoint = self
            .materialized_projection_checkpoint(MaterializedProjection::RequestContextOccupancy)
            .await?;
        if checkpoint != Some(expected) {
            return Err(SessionDbError::ProjectionStale {
                projection: MaterializedProjection::RequestContextOccupancy.as_str(),
                checkpoint,
                expected,
            });
        }
        let row = self
            .db
            .select("context_occupancy_projection")
            .columns(&["schema_version", "occupancy_json"])
            .where_eq("projection_id", CONTEXT_OCCUPANCY_PROJECTION_ID)
            .execute_first(&**self.db)
            .await?;
        let Some(row) = row.as_ref() else {
            return Ok(None);
        };
        let schema_version = required_i64(row, "schema_version").map(i64_to_u64)?;
        if schema_version != u64::from(CONTEXT_OCCUPANCY_PROJECTION_SCHEMA_VERSION) {
            return Err(SessionDbError::ProjectionIncompatible {
                projection: MaterializedProjection::RequestContextOccupancy.as_str(),
                actual: schema_version,
                expected: u64::from(CONTEXT_OCCUPANCY_PROJECTION_SCHEMA_VERSION),
            });
        }
        optional_string(row, "occupancy_json")
            .map(|json| serde_json::from_str(&json).map_err(SessionDbError::from))
            .transpose()
    }

    async fn latest_context_compaction_event(&self) -> SessionDbResult<Option<SessionEvent>> {
        let mut latest = None;
        for event_type in ["context_compacted", "provider_context_compacted"] {
            let row = self
                .db
                .select("events")
                .columns(&["payload"])
                .where_eq("event_type", event_type)
                .sort("event_seq", SortDirection::Desc)
                .limit(1)
                .execute_first(&**self.db)
                .await?;
            let Some(row) = row.as_ref() else {
                continue;
            };
            let payload = required_string(row, "payload")?;
            let event = decode_session_event(&payload)?;
            let boundary = match (&event.kind, event_type) {
                (
                    SessionEventKind::ContextCompacted {
                        compacted_through_sequence,
                        ..
                    },
                    "context_compacted",
                )
                | (
                    SessionEventKind::ProviderContextCompacted {
                        compacted_through_sequence,
                        ..
                    },
                    "provider_context_compacted",
                ) => *compacted_through_sequence,
                _ => {
                    return Err(SessionDbError::InvalidCompactionMarker {
                        sequence: event.sequence,
                        message: format!(
                            "event_type {event_type:?} does not match the persisted event kind"
                        ),
                    });
                }
            };
            if boundary > event.sequence {
                return Err(SessionDbError::InvalidCompactionMarker {
                    sequence: event.sequence,
                    message: format!("compacted boundary #{boundary} is later than its marker"),
                });
            }
            if latest
                .as_ref()
                .is_none_or(|current: &SessionEvent| event.sequence > current.sequence)
            {
                latest = Some(event);
            }
        }
        Ok(latest)
    }

    /// Return events from the canonical event table for the inclusive sequence range.
    ///
    /// # Errors
    ///
    /// Returns an error if the query or event deserialization fails.
    pub async fn events_range(
        &self,
        start_sequence: u64,
        end_sequence: u64,
        max_events: usize,
    ) -> SessionDbResult<Vec<SessionEvent>> {
        let rows = self
            .db
            .select("events")
            .columns(&["payload"])
            .where_gte("event_seq", seq_to_value(start_sequence))
            .where_lte("event_seq", seq_to_value(end_sequence))
            .sort("event_seq", SortDirection::Asc)
            .limit(max_events)
            .execute(&**self.db)
            .await?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let payload = required_string(&row, "payload")?;
            if let Some(event) = decode_session_event_degraded(&payload) {
                events.push(event);
            }
        }
        Ok(events)
    }

    /// Return latest transcript projection items in chronological order.
    ///
    /// # Errors
    ///
    /// Returns an error if the projection query fails or a row is malformed.
    pub async fn latest_transcript_items(
        &self,
        limit: usize,
    ) -> SessionDbResult<Vec<TranscriptItem>> {
        self.transcript_items_for_latest_window(limit, limit, usize::MAX)
            .await
    }

    /// Return enough latest transcript projection items to satisfy bounded window targets.
    ///
    /// # Errors
    ///
    /// Returns an error if the projection query fails or a row is malformed.
    pub async fn transcript_items_for_latest_window(
        &self,
        min_items: usize,
        max_items: usize,
        max_bytes: usize,
    ) -> SessionDbResult<Vec<TranscriptItem>> {
        let fetch_limit = max_items.max(min_items).max(1);
        let rows = self
            .latest_transcript_rows(fetch_limit)
            .await?
            .into_iter()
            .map(|row| transcript_item_from_row(&row))
            .collect::<SessionDbResult<Vec<_>>>()?;
        let mut items = Vec::new();
        let mut bytes = 0usize;
        for item in rows {
            let item_bytes = item.content.as_ref().map_or(0, String::len);
            if items.len() >= min_items
                && (items.len() >= max_items || bytes.saturating_add(item_bytes) > max_bytes)
            {
                break;
            }
            bytes = bytes.saturating_add(item_bytes);
            items.push(item);
            if items.len() >= max_items {
                break;
            }
        }
        items.sort_by_key(|item| item.transcript_seq);
        Ok(items)
    }

    /// Return the first canonical event sequence, if any.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails or the row is malformed.
    pub async fn first_event_sequence(&self) -> SessionDbResult<Option<u64>> {
        let row = self
            .db
            .select("events")
            .columns(&["event_seq"])
            .sort("event_seq", SortDirection::Asc)
            .limit(1)
            .execute_first(&**self.db)
            .await?;
        row.as_ref()
            .map(|row| required_i64(row, "event_seq").map(i64_to_u64))
            .transpose()
    }

    /// Return the last canonical event sequence, if any.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails or the row is malformed.
    pub async fn last_event_sequence(&self) -> SessionDbResult<Option<u64>> {
        let row = self
            .db
            .select("events")
            .columns(&["event_seq"])
            .sort("event_seq", SortDirection::Desc)
            .limit(1)
            .execute_first(&**self.db)
            .await?;
        row.as_ref()
            .map(|row| required_i64(row, "event_seq").map(i64_to_u64))
            .transpose()
    }

    /// Resolve one finalized artifact reference from the bounded materialized projection.
    ///
    /// # Errors
    ///
    /// Returns an error if the projection query fails, projection rows are malformed, or the
    /// artifact projection does not match the canonical event tail.
    pub async fn finalized_artifact_reference(
        &self,
        artifact_id: &str,
        reference_key: &str,
    ) -> SessionDbResult<Option<FinalizedArtifactReference>> {
        let expected = self.last_event_sequence().await?.unwrap_or_default();
        let checkpoint = self
            .materialized_projection_checkpoint(MaterializedProjection::ArtifactReferences)
            .await?;
        if checkpoint != Some(expected) {
            return Err(SessionDbError::ProjectionStale {
                projection: MaterializedProjection::ArtifactReferences.as_str(),
                checkpoint,
                expected,
            });
        }
        let row = self
            .db
            .select("artifact_references")
            .columns(&[
                "artifact_id",
                "reference_key",
                "producer_plugin_id",
                "schema",
                "schema_version",
                "storage_uri",
                "content_type",
                "byte_len",
                "availability",
                "complete",
                "checksum_sha256",
                "finalized_event_seq",
            ])
            .where_eq("artifact_id", artifact_id)
            .where_eq("reference_key", reference_key)
            .execute_first(&**self.db)
            .await?;
        row.as_ref()
            .map(finalized_artifact_reference_from_row)
            .transpose()
    }

    /// Return the latest transcript projection rows as generic database rows for callers that
    /// need a lightweight window before typed projection models are finalized.
    ///
    /// # Errors
    ///
    /// Returns an error if the projection query fails.
    pub async fn latest_transcript_rows(
        &self,
        limit: usize,
    ) -> Result<Vec<switchy::database::Row>, DatabaseError> {
        self.db
            .select("transcript_items")
            .columns(&[
                "transcript_seq",
                "event_seq_start",
                "event_seq_end",
                "role",
                "kind",
                "status",
                "content",
            ])
            .sort("transcript_seq", SortDirection::Desc)
            .limit(limit)
            .execute(&**self.db)
            .await
    }
}

async fn init_turso_local_with_retry(
    path: &Path,
) -> Result<Box<dyn Database>, switchy::database_connection::InitTursoError> {
    let mut attempt = 0_u32;
    let mut delay = DATABASE_OPEN_INITIAL_RETRY_DELAY;
    loop {
        match switchy::database_connection::builder()
            .turso()
            .with_path(path)
            .with_busy_timeout(DATABASE_BUSY_TIMEOUT)
            // Turso's multi-process WAL mode is still experimental and has produced stale
            // WAL-index sidecars after daemon lifecycle churn. Bcode serializes writes with
            // database transactions and its session access guard instead of relying on that
            // experimental sidecar format for correctness.
            .with_multiprocess_wal(false)
            .build()
            .await
        {
            Ok(db) => return Ok(db),
            Err(error)
                if is_database_lock_error(&error) && attempt < DATABASE_OPEN_RETRY_ATTEMPTS =>
            {
                attempt = attempt.saturating_add(1);
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2).min(DATABASE_OPEN_MAX_RETRY_DELAY);
            }
            Err(error) => return Err(error),
        }
    }
}

fn is_database_lock_error(error: &switchy::database_connection::InitTursoError) -> bool {
    is_database_lock_error_message(&error.to_string())
}

fn is_database_lock_error_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("locking error")
        || message.contains("failed locking file")
        || message.contains("database is locked")
        || message.contains("busy")
}

async fn run_global_migrations(db: &dyn Database) -> Result<(), MigrationError> {
    let runner = MigrationRunner::new(Box::new(global_migrations()))
        .with_table_name(GLOBAL_MIGRATIONS_TABLE.to_string());
    runner.run(db).await
}

async fn run_session_migrations(db: &dyn Database) -> Result<(), MigrationError> {
    let runner = MigrationRunner::new(Box::new(session_migrations()))
        .with_table_name(SESSION_MIGRATIONS_TABLE.to_string());
    runner.run(db).await
}

async fn set_storage_writer_contract(db: &dyn Database, writer_epoch: u32) -> SessionDbResult<()> {
    db.upsert("session_storage_contract")
        .value("contract_id", SESSION_STORAGE_CONTRACT_ID)
        .value(
            "schema_version",
            DatabaseValue::Int64(i64::from(SESSION_STORAGE_CONTRACT_SCHEMA_VERSION)),
        )
        .value(
            "writer_epoch",
            DatabaseValue::Int64(i64::from(writer_epoch)),
        )
        .value(
            "updated_by_build",
            option_env!("BCODE_BUILD_FINGERPRINT").map(str::to_owned),
        )
        .execute(db)
        .await?;
    Ok(())
}

async fn initialize_current_storage_contract(db: &dyn Database) -> SessionDbResult<()> {
    let tx = db.begin_transaction().await?;
    set_storage_writer_contract(&*tx, CURRENT_SESSION_STORAGE_WRITER_EPOCH).await?;
    tx.commit().await?;
    Ok(())
}

async fn validate_all_projection_checkpoints_at_tail(
    db: &dyn Database,
    expected: Option<u64>,
) -> SessionDbResult<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    for projection in MaterializedProjection::all() {
        validate_materialized_projection_version(db, *projection).await?;
        let checkpoint = projection_checkpoint_from_database(db, *projection).await?;
        if checkpoint != Some(expected) {
            return Err(SessionDbError::ProjectionStale {
                projection: projection.as_str(),
                checkpoint,
                expected,
            });
        }
    }
    Ok(())
}

async fn migrate_session_storage(db: &dyn Database) -> SessionDbResult<()> {
    let tx = db.begin_transaction().await?;
    let canonical_tail = last_event_sequence_from_database(&*tx).await?;
    rebuild_model_context_projection(&*tx).await?;
    if let Some(expected) = canonical_tail {
        let model_state = tx
            .select("model_context_projection_state")
            .columns(&["schema_version", "last_event_seq"])
            .where_eq("projection_id", MODEL_CONTEXT_PROJECTION_ID)
            .execute_first(&*tx)
            .await?
            .ok_or(SessionDbError::ProjectionStale {
                projection: "model_context",
                checkpoint: None,
                expected,
            })?;
        let schema_version = required_i64(&model_state, "schema_version").map(i64_to_u64)?;
        if schema_version != u64::from(MODEL_CONTEXT_PROJECTION_SCHEMA_VERSION) {
            return Err(SessionDbError::ModelContextProjectionVersion {
                actual: schema_version,
                expected: u64::from(MODEL_CONTEXT_PROJECTION_SCHEMA_VERSION),
            });
        }
        let checkpoint = required_i64(&model_state, "last_event_seq").map(i64_to_u64)?;
        if checkpoint != expected {
            return Err(SessionDbError::ModelContextProjectionStale {
                checkpoint,
                expected,
            });
        }
    }
    validate_all_projection_checkpoints_at_tail(&*tx, canonical_tail).await?;
    set_storage_writer_contract(&*tx, CURRENT_SESSION_STORAGE_WRITER_EPOCH).await?;
    tx.commit().await?;
    Ok(())
}

async fn strict_events_from_database(db: &dyn Database) -> SessionDbResult<Vec<SessionEvent>> {
    let rows = db
        .select("events")
        .columns(&["payload"])
        .sort("event_seq", SortDirection::Asc)
        .execute(db)
        .await?;
    rows.into_iter()
        .map(|row| {
            let payload = required_string(&row, "payload")?;
            Ok(decode_session_event(&payload)?)
        })
        .collect()
}

async fn rebuild_model_context_projection(db: &dyn Database) -> SessionDbResult<usize> {
    let events = strict_events_from_database(db).await?;
    for (index, event) in events.iter().enumerate() {
        let expected = u64::try_from(index).unwrap_or(u64::MAX);
        if event.sequence != expected {
            return Err(SessionDbError::InvalidCanonicalSequence {
                expected,
                actual: event.sequence,
            });
        }
    }

    db.delete("model_context_entries").execute(db).await?;
    db.delete("model_context_projection_state")
        .execute(db)
        .await?;
    for event in &events {
        project_model_context_event(db, event).await?;
    }
    Ok(events.len())
}

fn global_migrations() -> CodeMigrationSource<'static> {
    let mut source = CodeMigrationSource::new();
    add_sql_migration(
        &mut source,
        "001_global_sessions_table",
        "CREATE TABLE IF NOT EXISTS sessions (\n    session_id TEXT PRIMARY KEY NOT NULL,\n    db_path TEXT NOT NULL,\n    title TEXT,\n    working_directory TEXT,\n    created_at_ms INTEGER NOT NULL,\n    updated_at_ms INTEGER NOT NULL,\n    state TEXT NOT NULL DEFAULT 'active',\n    projection_status TEXT NOT NULL DEFAULT 'fresh'\n)",
        "DROP TABLE IF EXISTS sessions",
    );
    add_sql_migration(
        &mut source,
        "002_global_sessions_updated_at_index",
        "CREATE INDEX IF NOT EXISTS idx_sessions_updated_at_ms ON sessions(updated_at_ms)",
        "DROP INDEX IF EXISTS idx_sessions_updated_at_ms",
    );
    add_sql_migration(
        &mut source,
        "003_global_composer_drafts_table",
        "CREATE TABLE IF NOT EXISTS composer_drafts (\n    scope_kind TEXT NOT NULL,\n    scope_key TEXT NOT NULL,\n    launch_working_directory TEXT,\n    session_id TEXT,\n    text TEXT NOT NULL,\n    updated_at_ms INTEGER NOT NULL,\n    PRIMARY KEY(scope_kind, scope_key)\n)",
        "DROP TABLE IF EXISTS composer_drafts",
    );
    source
}

fn session_migrations() -> CodeMigrationSource<'static> {
    let mut source = CodeMigrationSource::new();
    add_session_base_migrations(&mut source);
    add_session_runtime_migrations(&mut source);
    source
}

fn add_session_base_migrations(source: &mut CodeMigrationSource<'static>) {
    add_sql_migration(
        source,
        "001_events_table",
        "CREATE TABLE IF NOT EXISTS events (\n    event_seq INTEGER PRIMARY KEY NOT NULL,\n    event_type TEXT NOT NULL,\n    schema_version INTEGER NOT NULL,\n    created_at_ms INTEGER,\n    causation_id TEXT,\n    correlation_id TEXT,\n    payload TEXT NOT NULL\n)",
        "DROP TABLE IF EXISTS events",
    );
    add_sql_migration(
        source,
        "002_events_event_type_index",
        "CREATE INDEX IF NOT EXISTS idx_events_event_type ON events(event_type)",
        "DROP INDEX IF EXISTS idx_events_event_type",
    );
    add_sql_migration(
        source,
        "003_session_state_table",
        "CREATE TABLE IF NOT EXISTS session_state (\n    session_id TEXT PRIMARY KEY NOT NULL,\n    last_event_seq INTEGER NOT NULL,\n    current_model TEXT,\n    current_provider TEXT,\n    working_directory TEXT,\n    title TEXT,\n    updated_at_ms INTEGER\n)",
        "DROP TABLE IF EXISTS session_state",
    );
    add_sql_migration(
        source,
        "004_input_messages_table",
        "CREATE TABLE IF NOT EXISTS input_messages (\n    input_seq INTEGER PRIMARY KEY NOT NULL,\n    event_seq INTEGER NOT NULL,\n    created_at_ms INTEGER,\n    text TEXT NOT NULL,\n    working_directory TEXT,\n    model TEXT,\n    FOREIGN KEY(event_seq) REFERENCES events(event_seq)\n)",
        "DROP TABLE IF EXISTS input_messages",
    );
    add_sql_migration(
        source,
        "005_input_messages_event_seq_index",
        "CREATE INDEX IF NOT EXISTS idx_input_messages_event_seq ON input_messages(event_seq)",
        "DROP INDEX IF EXISTS idx_input_messages_event_seq",
    );
    add_sql_migration(
        source,
        "006_transcript_items_table",
        "CREATE TABLE IF NOT EXISTS transcript_items (\n    transcript_seq INTEGER PRIMARY KEY NOT NULL,\n    event_seq_start INTEGER NOT NULL,\n    event_seq_end INTEGER NOT NULL,\n    role TEXT NOT NULL,\n    kind TEXT NOT NULL,\n    status TEXT NOT NULL,\n    content TEXT,\n    created_at_ms INTEGER,\n    FOREIGN KEY(event_seq_start) REFERENCES events(event_seq)\n)",
        "DROP TABLE IF EXISTS transcript_items",
    );
    add_sql_migration(
        source,
        "007_transcript_items_event_range_index",
        "CREATE INDEX IF NOT EXISTS idx_transcript_items_event_range ON transcript_items(event_seq_start, event_seq_end)",
        "DROP INDEX IF EXISTS idx_transcript_items_event_range",
    );
    add_sql_migration(
        source,
        "008_tool_runs_table",
        "CREATE TABLE IF NOT EXISTS tool_runs (\n    tool_call_id TEXT PRIMARY KEY NOT NULL,\n    event_seq_start INTEGER NOT NULL,\n    event_seq_end INTEGER,\n    status TEXT NOT NULL,\n    tool_name TEXT,\n    started_at_ms INTEGER,\n    completed_at_ms INTEGER,\n    output_bytes INTEGER,\n    is_error INTEGER,\n    FOREIGN KEY(event_seq_start) REFERENCES events(event_seq)\n)",
        "DROP TABLE IF EXISTS tool_runs",
    );
    add_sql_migration(
        source,
        "009_tool_runs_status_index",
        "CREATE INDEX IF NOT EXISTS idx_tool_runs_status ON tool_runs(status)",
        "DROP INDEX IF EXISTS idx_tool_runs_status",
    );
    add_sql_migration(
        source,
        "010_projection_checkpoints_table",
        "CREATE TABLE IF NOT EXISTS projection_checkpoints (\n    projection_name TEXT PRIMARY KEY NOT NULL,\n    last_event_seq INTEGER NOT NULL,\n    projection_version INTEGER NOT NULL,\n    updated_at_ms INTEGER\n)",
        "DROP TABLE IF EXISTS projection_checkpoints",
    );
    add_sql_migration(
        source,
        "011_snapshots_table",
        "CREATE TABLE IF NOT EXISTS snapshots (\n    snapshot_name TEXT PRIMARY KEY NOT NULL,\n    last_event_seq INTEGER NOT NULL,\n    schema_version INTEGER NOT NULL,\n    payload TEXT NOT NULL,\n    updated_at_ms INTEGER\n)",
        "DROP TABLE IF EXISTS snapshots",
    );
}

fn add_session_runtime_migrations(source: &mut CodeMigrationSource<'static>) {
    add_sql_migration(
        source,
        "012_runtime_work_table",
        "CREATE TABLE IF NOT EXISTS runtime_work (\n    work_id TEXT PRIMARY KEY NOT NULL,\n    event_seq_start INTEGER NOT NULL,\n    event_seq_end INTEGER,\n    parent_work_id TEXT,\n    kind TEXT NOT NULL,\n    label TEXT NOT NULL,\n    status TEXT NOT NULL,\n    started_at_ms INTEGER,\n    finished_at_ms INTEGER,\n    message TEXT,\n    cancellable INTEGER NOT NULL DEFAULT 0,\n    FOREIGN KEY(event_seq_start) REFERENCES events(event_seq)\n)",
        "DROP TABLE IF EXISTS runtime_work",
    );
    add_sql_migration(
        source,
        "013_runtime_work_status_index",
        "CREATE INDEX IF NOT EXISTS idx_runtime_work_status ON runtime_work(status)",
        "DROP INDEX IF EXISTS idx_runtime_work_status",
    );
    add_sql_migration(
        source,
        "014_runtime_work_parent_index",
        "CREATE INDEX IF NOT EXISTS idx_runtime_work_parent_work_id ON runtime_work(parent_work_id)",
        "DROP INDEX IF EXISTS idx_runtime_work_parent_work_id",
    );
    add_sql_migration(
        source,
        "015_session_drafts_table",
        "CREATE TABLE IF NOT EXISTS session_drafts (\n    session_id TEXT PRIMARY KEY NOT NULL,\n    text TEXT NOT NULL,\n    updated_at_ms INTEGER NOT NULL\n)",
        "DROP TABLE IF EXISTS session_drafts",
    );
    add_sql_migration(
        source,
        "016_session_state_reasoning_effort_column",
        "ALTER TABLE session_state ADD COLUMN reasoning_effort TEXT",
        "ALTER TABLE session_state DROP COLUMN reasoning_effort",
    );
    add_sql_migration(
        source,
        "017_session_state_reasoning_summary_column",
        "ALTER TABLE session_state ADD COLUMN reasoning_summary TEXT",
        "ALTER TABLE session_state DROP COLUMN reasoning_summary",
    );
    add_sql_migration(
        source,
        "018_model_context_projection_state_table",
        "CREATE TABLE IF NOT EXISTS model_context_projection_state (\n    projection_id INTEGER PRIMARY KEY NOT NULL,\n    schema_version INTEGER NOT NULL,\n    last_event_seq INTEGER NOT NULL\n)",
        "DROP TABLE IF EXISTS model_context_projection_state",
    );
    add_sql_migration(
        source,
        "019_model_context_entries_table",
        "CREATE TABLE IF NOT EXISTS model_context_entries (\n    event_seq INTEGER PRIMARY KEY NOT NULL,\n    event_type TEXT NOT NULL,\n    payload TEXT NOT NULL,\n    FOREIGN KEY(event_seq) REFERENCES events(event_seq)\n)",
        "DROP TABLE IF EXISTS model_context_entries",
    );
    add_sql_migration(
        source,
        "020_model_context_entries_event_type_index",
        "CREATE INDEX IF NOT EXISTS idx_model_context_entries_event_type ON model_context_entries(event_type)",
        "DROP INDEX IF EXISTS idx_model_context_entries_event_type",
    );
    add_sql_migration(
        source,
        "021_artifact_references_table",
        "CREATE TABLE IF NOT EXISTS artifact_references (\n    artifact_id TEXT NOT NULL,\n    reference_key TEXT NOT NULL,\n    producer_plugin_id TEXT NOT NULL,\n    schema TEXT NOT NULL,\n    schema_version INTEGER NOT NULL,\n    storage_uri TEXT,\n    content_type TEXT,\n    byte_len INTEGER,\n    availability TEXT,\n    complete INTEGER,\n    checksum_sha256 TEXT,\n    finalized_event_seq INTEGER NOT NULL,\n    PRIMARY KEY(artifact_id, reference_key),\n    FOREIGN KEY(finalized_event_seq) REFERENCES events(event_seq)\n)",
        "DROP TABLE IF EXISTS artifact_references",
    );
    add_sql_migration(
        source,
        "022_context_occupancy_projection_table",
        "CREATE TABLE IF NOT EXISTS context_occupancy_projection (\n    projection_id INTEGER PRIMARY KEY NOT NULL,\n    schema_version INTEGER NOT NULL,\n    context_epoch INTEGER NOT NULL,\n    occupancy_json TEXT\n);\nINSERT OR IGNORE INTO context_occupancy_projection (projection_id, schema_version, context_epoch, occupancy_json) SELECT 1, 1, COALESCE(MAX(event_seq), 0), NULL FROM events WHERE event_type IN ('model_changed', 'context_compacted', 'provider_context_compacted');\nINSERT OR IGNORE INTO projection_checkpoints (projection_name, last_event_seq, projection_version, updated_at_ms) SELECT 'context_occupancy', COALESCE(MAX(event_seq), 0), 1, 0 FROM events",
        "DROP TABLE IF EXISTS context_occupancy_projection",
    );
    add_sql_migration(
        source,
        "023_reset_legacy_context_occupancy_projection",
        "UPDATE context_occupancy_projection SET schema_version = 3, occupancy_json = NULL WHERE schema_version < 3",
        "UPDATE context_occupancy_projection SET schema_version = 2, occupancy_json = NULL WHERE schema_version = 3",
    );
    add_sql_migration(
        source,
        "024_reset_request_context_occupancy_projection",
        "UPDATE context_occupancy_projection SET schema_version = 4, occupancy_json = NULL WHERE schema_version < 4",
        "UPDATE context_occupancy_projection SET schema_version = 3, occupancy_json = NULL WHERE schema_version = 4",
    );
    add_sql_migration(
        source,
        "025_turn_receipts_table",
        "CREATE TABLE IF NOT EXISTS turn_receipts (\n    producer TEXT NOT NULL,\n    idempotency_key TEXT NOT NULL,\n    accepted_event_seq INTEGER NOT NULL,\n    turn_id TEXT NOT NULL,\n    work_id TEXT NOT NULL,\n    PRIMARY KEY(producer, idempotency_key),\n    FOREIGN KEY(accepted_event_seq) REFERENCES events(event_seq)\n)",
        "DROP TABLE IF EXISTS turn_receipts",
    );
    add_sql_migration(
        source,
        "026_session_storage_contract_table",
        "CREATE TABLE IF NOT EXISTS session_storage_contract (\n    contract_id INTEGER PRIMARY KEY NOT NULL,\n    schema_version INTEGER NOT NULL,\n    writer_epoch INTEGER NOT NULL,\n    updated_by_build TEXT\n)",
        "DROP TABLE IF EXISTS session_storage_contract",
    );
    add_sql_migration(
        source,
        "027_initialize_session_storage_contract",
        "INSERT OR IGNORE INTO session_storage_contract (contract_id, schema_version, writer_epoch, updated_by_build) VALUES (1, 1, 1, NULL)",
        "DELETE FROM session_storage_contract WHERE contract_id = 1",
    );
}

fn add_sql_migration(
    source: &mut CodeMigrationSource<'static>,
    id: &str,
    up_sql: &str,
    down_sql: &str,
) {
    source.add_migration(CodeMigration::new(
        id.to_string(),
        Box::new(up_sql.to_string()),
        Some(Box::new(down_sql.to_string())),
    ));
}

async fn validate_storage_writer_contract_for_epoch(
    db: &dyn Database,
    expected_writer_epoch: u32,
) -> SessionDbResult<()> {
    let row = db
        .select("session_storage_contract")
        .columns(&["schema_version", "writer_epoch"])
        .where_eq("contract_id", SESSION_STORAGE_CONTRACT_ID)
        .execute_first(db)
        .await?;
    let Some(row) = row.as_ref() else {
        return Err(SessionDbError::WriterIncompatible {
            actual: Some(u64::from(LEGACY_SESSION_STORAGE_WRITER_EPOCH)),
            expected: u64::from(expected_writer_epoch),
        });
    };
    let schema_version = required_i64(row, "schema_version").map(i64_to_u64)?;
    if schema_version != u64::from(SESSION_STORAGE_CONTRACT_SCHEMA_VERSION) {
        return Err(SessionDbError::ProjectionIncompatible {
            projection: "session_storage_contract",
            actual: schema_version,
            expected: u64::from(SESSION_STORAGE_CONTRACT_SCHEMA_VERSION),
        });
    }
    let actual = required_i64(row, "writer_epoch").map(i64_to_u64)?;
    let expected = u64::from(expected_writer_epoch);
    if actual != expected {
        return Err(SessionDbError::WriterIncompatible {
            actual: Some(actual),
            expected,
        });
    }
    Ok(())
}

async fn validate_storage_writer_contract(db: &dyn Database) -> SessionDbResult<()> {
    validate_storage_writer_contract_for_epoch(db, CURRENT_SESSION_STORAGE_WRITER_EPOCH).await
}

async fn last_event_sequence_from_database(db: &dyn Database) -> SessionDbResult<Option<u64>> {
    let row = db
        .select("events")
        .columns(&["event_seq"])
        .sort("event_seq", SortDirection::Desc)
        .limit(1)
        .execute_first(db)
        .await?;
    row.as_ref()
        .map(|row| required_i64(row, "event_seq").map(i64_to_u64))
        .transpose()
}

async fn projection_checkpoint_from_database(
    db: &dyn Database,
    projection: MaterializedProjection,
) -> SessionDbResult<Option<u64>> {
    let row = db
        .select("projection_checkpoints")
        .columns(&["last_event_seq"])
        .where_eq("projection_name", projection.as_str())
        .execute_first(db)
        .await?;
    row.as_ref()
        .map(|row| required_i64(row, "last_event_seq").map(i64_to_u64))
        .transpose()
}

async fn projection_version_from_database(
    db: &dyn Database,
    projection: MaterializedProjection,
) -> SessionDbResult<Option<u64>> {
    let row = db
        .select("projection_checkpoints")
        .columns(&["projection_version"])
        .where_eq("projection_name", projection.as_str())
        .execute_first(db)
        .await?;
    row.as_ref()
        .map(|row| required_i64(row, "projection_version").map(i64_to_u64))
        .transpose()
}

async fn validate_materialized_projection_version(
    db: &dyn Database,
    projection: MaterializedProjection,
) -> SessionDbResult<()> {
    let actual = projection_version_from_database(db, projection).await?;
    let expected = u64::from(projection.schema_version());
    if let Some(actual) = actual
        && actual != expected
    {
        return Err(SessionDbError::ProjectionIncompatible {
            projection: projection.as_str(),
            actual,
            expected,
        });
    }
    Ok(())
}

async fn validate_model_context_precondition(
    db: &dyn Database,
    event: &SessionEvent,
) -> SessionDbResult<()> {
    let state = db
        .select("model_context_projection_state")
        .columns(&["schema_version", "last_event_seq"])
        .where_eq("projection_id", MODEL_CONTEXT_PROJECTION_ID)
        .execute_first(db)
        .await?;
    let Some(state) = state.as_ref() else {
        if event.sequence == 0 {
            return Ok(());
        }
        return Err(SessionDbError::ProjectionStale {
            projection: "model_context",
            checkpoint: None,
            expected: event.sequence.saturating_sub(1),
        });
    };
    let schema_version = required_i64(state, "schema_version").map(i64_to_u64)?;
    if schema_version != u64::from(MODEL_CONTEXT_PROJECTION_SCHEMA_VERSION) {
        return Err(SessionDbError::ModelContextProjectionVersion {
            actual: schema_version,
            expected: u64::from(MODEL_CONTEXT_PROJECTION_SCHEMA_VERSION),
        });
    }
    let checkpoint = required_i64(state, "last_event_seq").map(i64_to_u64)?;
    let expected = event.sequence.saturating_sub(1);
    if event.sequence == 0 || checkpoint != expected {
        return Err(SessionDbError::ModelContextProjectionStale {
            checkpoint,
            expected,
        });
    }
    Ok(())
}

async fn validate_context_occupancy_precondition(
    db: &dyn Database,
    event: &SessionEvent,
) -> SessionDbResult<()> {
    let row = db
        .select("context_occupancy_projection")
        .columns(&["schema_version"])
        .where_eq("projection_id", CONTEXT_OCCUPANCY_PROJECTION_ID)
        .execute_first(db)
        .await?;
    let Some(row) = row.as_ref() else {
        if event.sequence == 0 {
            return Ok(());
        }
        return Err(SessionDbError::ProjectionStale {
            projection: MaterializedProjection::RequestContextOccupancy.as_str(),
            checkpoint: None,
            expected: event.sequence.saturating_sub(1),
        });
    };
    let actual = required_i64(row, "schema_version").map(i64_to_u64)?;
    let expected = u64::from(CONTEXT_OCCUPANCY_PROJECTION_SCHEMA_VERSION);
    if actual != expected {
        return Err(SessionDbError::ProjectionIncompatible {
            projection: MaterializedProjection::RequestContextOccupancy.as_str(),
            actual,
            expected,
        });
    }
    Ok(())
}

async fn validate_append_preconditions_without_writer(
    db: &dyn Database,
    event: &SessionEvent,
) -> SessionDbResult<()> {
    let canonical_tail = last_event_sequence_from_database(db).await?;
    let expected_sequence = canonical_tail.map_or(0, |tail| tail.saturating_add(1));
    if event.sequence != expected_sequence {
        return Err(SessionDbError::InvalidCanonicalAppendSequence {
            expected: expected_sequence,
            actual: event.sequence,
        });
    }
    validate_model_context_precondition(db, event).await?;
    validate_context_occupancy_precondition(db, event).await?;
    if let Some(expected) = canonical_tail {
        for projection in MaterializedProjection::all() {
            validate_materialized_projection_version(db, *projection).await?;
            let checkpoint = projection_checkpoint_from_database(db, *projection).await?;
            if checkpoint != Some(expected) {
                return Err(SessionDbError::ProjectionStale {
                    projection: projection.as_str(),
                    checkpoint,
                    expected,
                });
            }
        }
    }
    Ok(())
}

async fn validate_append_postconditions(
    db: &dyn Database,
    event: &SessionEvent,
) -> SessionDbResult<()> {
    let canonical_tail = last_event_sequence_from_database(db).await?;
    if canonical_tail != Some(event.sequence) {
        return Err(SessionDbError::InvalidCanonicalAppendSequence {
            expected: event.sequence,
            actual: canonical_tail.unwrap_or_default(),
        });
    }
    for projection in MaterializedProjection::all() {
        validate_materialized_projection_version(db, *projection).await?;
        let checkpoint = projection_checkpoint_from_database(db, *projection).await?;
        if checkpoint != Some(event.sequence) {
            return Err(SessionDbError::ProjectionStale {
                projection: projection.as_str(),
                checkpoint,
                expected: event.sequence,
            });
        }
    }
    let model_context = db
        .select("model_context_projection_state")
        .columns(&["schema_version", "last_event_seq"])
        .where_eq("projection_id", MODEL_CONTEXT_PROJECTION_ID)
        .execute_first(db)
        .await?
        .ok_or(SessionDbError::ProjectionStale {
            projection: "model_context",
            checkpoint: None,
            expected: event.sequence,
        })?;
    let schema_version = required_i64(&model_context, "schema_version").map(i64_to_u64)?;
    if schema_version != u64::from(MODEL_CONTEXT_PROJECTION_SCHEMA_VERSION) {
        return Err(SessionDbError::ModelContextProjectionVersion {
            actual: schema_version,
            expected: u64::from(MODEL_CONTEXT_PROJECTION_SCHEMA_VERSION),
        });
    }
    let checkpoint = required_i64(&model_context, "last_event_seq").map(i64_to_u64)?;
    if checkpoint != event.sequence {
        return Err(SessionDbError::ModelContextProjectionStale {
            checkpoint,
            expected: event.sequence,
        });
    }
    let occupancy_checkpoint =
        projection_checkpoint_from_database(db, MaterializedProjection::RequestContextOccupancy)
            .await?;
    if occupancy_checkpoint != Some(event.sequence) {
        return Err(SessionDbError::ProjectionStale {
            projection: MaterializedProjection::RequestContextOccupancy.as_str(),
            checkpoint: occupancy_checkpoint,
            expected: event.sequence,
        });
    }
    validate_context_occupancy_precondition(db, event).await
}

async fn insert_event(
    db: &dyn Database,
    event: &SessionEvent,
    activity_timestamp_ms: Option<u64>,
) -> SessionDbResult<()> {
    db.insert("events")
        .value("event_seq", seq_to_value(event.sequence))
        .value("event_type", event_kind_name(&event.kind))
        .value(
            "schema_version",
            DatabaseValue::Int32(i32::from(event.schema_version)),
        )
        .value(
            "created_at_ms",
            seq_to_value(activity_timestamp_ms.unwrap_or_else(|| event_created_at_ms(event))),
        )
        .value("payload", encode_session_event(event)?)
        .execute(db)
        .await?;
    Ok(())
}

fn compaction_boundary(event: &SessionEvent) -> SessionDbResult<Option<u64>> {
    let boundary = match &event.kind {
        SessionEventKind::ContextCompacted {
            compacted_through_sequence,
            ..
        }
        | SessionEventKind::ProviderContextCompacted {
            compacted_through_sequence,
            ..
        } => *compacted_through_sequence,
        _ => return Ok(None),
    };
    if boundary > event.sequence {
        return Err(SessionDbError::InvalidCompactionMarker {
            sequence: event.sequence,
            message: format!("compacted boundary #{boundary} is later than its marker"),
        });
    }
    Ok(Some(boundary))
}

async fn project_model_context_event(
    db: &dyn Database,
    event: &SessionEvent,
) -> SessionDbResult<()> {
    let state = db
        .select("model_context_projection_state")
        .columns(&["schema_version", "last_event_seq"])
        .where_eq("projection_id", MODEL_CONTEXT_PROJECTION_ID)
        .execute_first(db)
        .await?;
    match state.as_ref() {
        None if event.sequence == 0 => {}
        None => {
            return Err(SessionDbError::ProjectionStale {
                projection: "model_context",
                checkpoint: None,
                expected: event.sequence.saturating_sub(1),
            });
        }
        Some(row) => {
            let schema_version = required_i64(row, "schema_version").map(i64_to_u64)?;
            if schema_version != u64::from(MODEL_CONTEXT_PROJECTION_SCHEMA_VERSION) {
                return Err(SessionDbError::ModelContextProjectionVersion {
                    actual: schema_version,
                    expected: u64::from(MODEL_CONTEXT_PROJECTION_SCHEMA_VERSION),
                });
            }
            let checkpoint = required_i64(row, "last_event_seq").map(i64_to_u64)?;
            let expected = event.sequence.saturating_sub(1);
            if event.sequence == 0 || checkpoint != expected {
                return Err(SessionDbError::ModelContextProjectionStale {
                    checkpoint,
                    expected,
                });
            }
        }
    }

    let event_type = model_context_event_kind_name(&event.kind);
    if !matches!(
        context_history_role(&event.kind),
        ContextHistoryRole::Excluded
    ) {
        if let Some(boundary) = compaction_boundary(event)? {
            db.delete("model_context_entries")
                .where_lte("event_seq", seq_to_value(boundary))
                .execute(db)
                .await?;
            db.delete("model_context_entries")
                .where_in(
                    "event_type",
                    vec![
                        DatabaseValue::String("context_compacted".to_string()),
                        DatabaseValue::String("provider_context_compacted".to_string()),
                    ],
                )
                .execute(db)
                .await?;
        }
        db.insert("model_context_entries")
            .value("event_seq", seq_to_value(event.sequence))
            .value("event_type", event_type)
            .value("payload", encode_session_event(event)?)
            .execute(db)
            .await?;
    }

    db.upsert("model_context_projection_state")
        .value("projection_id", MODEL_CONTEXT_PROJECTION_ID)
        .value(
            "schema_version",
            DatabaseValue::Int64(i64::from(MODEL_CONTEXT_PROJECTION_SCHEMA_VERSION)),
        )
        .value("last_event_seq", seq_to_value(event.sequence))
        .execute(db)
        .await?;
    Ok(())
}

async fn project_context_occupancy_event(
    db: &dyn Database,
    event: &SessionEvent,
) -> SessionDbResult<()> {
    use bcode_session_models::{RequestContextOccupancy, SessionEventKind};

    let row = db
        .select("context_occupancy_projection")
        .columns(&["schema_version", "context_epoch", "occupancy_json"])
        .where_eq("projection_id", CONTEXT_OCCUPANCY_PROJECTION_ID)
        .execute_first(db)
        .await?;
    let (context_epoch, current) = if let Some(row) = row.as_ref() {
        let schema_version = required_i64(row, "schema_version").map(i64_to_u64)?;
        if schema_version != u64::from(CONTEXT_OCCUPANCY_PROJECTION_SCHEMA_VERSION) {
            return Err(SessionDbError::ProjectionIncompatible {
                projection: MaterializedProjection::RequestContextOccupancy.as_str(),
                actual: schema_version,
                expected: u64::from(CONTEXT_OCCUPANCY_PROJECTION_SCHEMA_VERSION),
            });
        }
        let context_epoch = required_i64(row, "context_epoch").map(i64_to_u64)?;
        let current = optional_string(row, "occupancy_json")
            .map(|json| serde_json::from_str::<RequestContextOccupancy>(&json))
            .transpose()?;
        (context_epoch, current)
    } else {
        (0, None)
    };

    let (context_epoch, occupancy) = match &event.kind {
        SessionEventKind::ModelChanged { .. }
        | SessionEventKind::ContextCompacted { .. }
        | SessionEventKind::ProviderContextCompacted { .. } => (event.sequence, None),
        SessionEventKind::RequestContextObserved { observation } => (
            context_epoch,
            RequestContextOccupancy::reconcile(
                current.as_ref(),
                context_epoch,
                event.sequence,
                observation.clone(),
            ),
        ),
        _ => (context_epoch, current),
    };
    db.upsert("context_occupancy_projection")
        .value("projection_id", CONTEXT_OCCUPANCY_PROJECTION_ID)
        .value(
            "schema_version",
            DatabaseValue::Int64(i64::from(CONTEXT_OCCUPANCY_PROJECTION_SCHEMA_VERSION)),
        )
        .value("context_epoch", seq_to_value(context_epoch))
        .value(
            "occupancy_json",
            occupancy.as_ref().map(serde_json::to_string).transpose()?,
        )
        .execute(db)
        .await?;
    update_projection_checkpoint(db, MaterializedProjection::RequestContextOccupancy, event)
        .await?;
    Ok(())
}

async fn project_turn_receipt(db: &dyn Database, event: &SessionEvent) -> SessionDbResult<()> {
    let SessionEventKind::UserMessage { admission, .. } = &event.kind else {
        return Ok(());
    };
    let Some((producer, idempotency_key)) = admission.idempotency_identity() else {
        return Ok(());
    };
    let receipt =
        bcode_session_models::TurnReceipt::from_accepted_event(event.session_id, event.sequence);
    db.insert("turn_receipts")
        .value("producer", producer.to_owned())
        .value("idempotency_key", idempotency_key.to_owned())
        .value(
            "accepted_event_seq",
            seq_to_value(receipt.accepted_event_sequence),
        )
        .value("turn_id", receipt.turn_id.to_string())
        .value("work_id", receipt.work_id.to_string())
        .execute(db)
        .await?;
    Ok(())
}

const BASE_MATERIALIZED_PROJECTIONS: [MaterializedProjection; 6] = [
    MaterializedProjection::SessionState,
    MaterializedProjection::InputHistory,
    MaterializedProjection::Transcript,
    MaterializedProjection::ToolRuns,
    MaterializedProjection::ArtifactReferences,
    MaterializedProjection::RuntimeWork,
];

async fn project_materialized_event(
    db: &dyn Database,
    event: &SessionEvent,
) -> SessionDbResult<()> {
    project_event(db, event).await?;
    for projection in BASE_MATERIALIZED_PROJECTIONS {
        update_projection_checkpoint(db, projection, event).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn project_event(db: &dyn Database, event: &SessionEvent) -> SessionDbResult<()> {
    update_session_state(db, event).await?;
    match &event.kind {
        SessionEventKind::SessionCreated {
            name,
            working_directory,
        } => {
            db.upsert("session_state")
                .value("session_id", event.session_id.to_string())
                .value("last_event_seq", seq_to_value(event.sequence))
                .value("title", name.clone())
                .value(
                    "working_directory",
                    working_directory.to_string_lossy().to_string(),
                )
                .value("updated_at_ms", seq_to_value(event_created_at_ms(event)))
                .execute(db)
                .await?;
        }
        SessionEventKind::SessionRenamed { name } => {
            db.update("session_state")
                .value("title", name.clone())
                .where_eq("session_id", event.session_id.to_string())
                .execute(db)
                .await?;
        }
        SessionEventKind::ModelChanged { provider, model } => {
            db.update("session_state")
                .value("current_provider", provider.clone())
                .value("current_model", model.clone())
                .where_eq("session_id", event.session_id.to_string())
                .execute(db)
                .await?;
        }
        SessionEventKind::ReasoningChanged { effort, summary } => {
            db.update("session_state")
                .value("reasoning_effort", effort.clone())
                .value("reasoning_summary", summary.clone())
                .where_eq("session_id", event.session_id.to_string())
                .execute(db)
                .await?;
        }
        SessionEventKind::UserMessage { text, .. } => {
            db.insert("input_messages")
                .value("input_seq", seq_to_value(event.sequence))
                .value("event_seq", seq_to_value(event.sequence))
                .value("created_at_ms", seq_to_value(event_created_at_ms(event)))
                .value("text", text.clone())
                .execute(db)
                .await?;
            if db
                .select("session_state")
                .columns(&["title"])
                .where_eq("session_id", event.session_id.to_string())
                .execute_first(db)
                .await?
                .as_ref()
                .and_then(|row| optional_string(row, "title"))
                .is_none()
            {
                db.update("session_state")
                    .value("title", crate::title_from_first_prompt(text))
                    .where_eq("session_id", event.session_id.to_string())
                    .execute(db)
                    .await?;
            }
            insert_transcript_item(db, event, "user", "message", "complete", Some(text.clone()))
                .await?;
        }
        SessionEventKind::AssistantMessage { text } => {
            insert_transcript_item(
                db,
                event,
                "assistant",
                "message",
                "complete",
                Some(text.clone()),
            )
            .await?;
        }
        SessionEventKind::ToolCallRequested {
            tool_call_id,
            tool_name,
            ..
        } => {
            db.insert("tool_runs")
                .value("tool_call_id", tool_call_id.clone())
                .value("event_seq_start", seq_to_value(event.sequence))
                .value("status", "running")
                .value("tool_name", tool_name.clone())
                .execute(db)
                .await?;
            insert_transcript_item(db, event, "tool", "invocation", "running", None).await?;
        }
        SessionEventKind::ToolCallFinished {
            tool_call_id,
            is_error,
            semantic_result,
            ..
        } => {
            db.update("tool_runs")
                .value("event_seq_end", seq_to_value(event.sequence))
                .value("status", if *is_error { "error" } else { "complete" })
                .value("is_error", bool_to_value(*is_error))
                .where_eq("tool_call_id", tool_call_id.clone())
                .execute(db)
                .await?;
            if let Some(ToolInvocationResult::Artifact { artifact }) = semantic_result {
                project_artifact_references(db, event.sequence, artifact).await?;
            }
            insert_tool_result_transcript_item(db, event, tool_call_id, *is_error).await?;
        }
        SessionEventKind::ToolInvocationStream { event: stream } => {
            update_tool_transcript_item_end(db, tool_stream_tool_call_id(stream), event.sequence)
                .await?;
        }
        SessionEventKind::RuntimeWorkStarted {
            work_id,
            kind,
            label,
            parent_work_id,
            started_at_ms,
            cancellable,
            ..
        } => {
            db.upsert("runtime_work")
                .value("work_id", work_id.to_string())
                .value("event_seq_start", seq_to_value(event.sequence))
                .value("kind", runtime_work_kind_name(*kind))
                .value("label", label.clone())
                .value(
                    "status",
                    runtime_work_status_name(RuntimeWorkStatus::Running),
                )
                .value(
                    "parent_work_id",
                    parent_work_id.as_ref().map(ToString::to_string),
                )
                .value("started_at_ms", started_at_ms.map(seq_to_value))
                .value("cancellable", bool_to_value(*cancellable))
                .execute(db)
                .await?;
        }
        SessionEventKind::RuntimeWorkCancelRequested { work_id, .. } => {
            db.update("runtime_work")
                .value(
                    "status",
                    runtime_work_status_name(RuntimeWorkStatus::Cancelling),
                )
                .where_eq("work_id", work_id.to_string())
                .execute(db)
                .await?;
        }
        SessionEventKind::RuntimeWorkFinished {
            work_id,
            status,
            finished_at_ms,
            message,
        } => {
            db.update("runtime_work")
                .value("event_seq_end", seq_to_value(event.sequence))
                .value("status", runtime_work_status_name(*status))
                .value("finished_at_ms", finished_at_ms.map(seq_to_value))
                .value("message", message.clone())
                .where_eq("work_id", work_id.to_string())
                .execute(db)
                .await?;
        }
        SessionEventKind::RuntimeWorkProgress {
            work_id, message, ..
        } => {
            db.update("runtime_work")
                .value("message", message.clone())
                .where_eq("work_id", work_id.to_string())
                .execute(db)
                .await?;
        }
        SessionEventKind::WorkingDirectoryChanged {
            new_working_directory,
            ..
        } => {
            db.update("session_state")
                .value(
                    "working_directory",
                    new_working_directory.to_string_lossy().to_string(),
                )
                .where_eq("session_id", event.session_id.to_string())
                .execute(db)
                .await?;
        }
        _ => {}
    }

    Ok(())
}

async fn update_session_state(db: &dyn Database, event: &SessionEvent) -> SessionDbResult<()> {
    db.upsert("session_state")
        .value("session_id", event.session_id.to_string())
        .value("last_event_seq", seq_to_value(event.sequence))
        .value("updated_at_ms", seq_to_value(event_created_at_ms(event)))
        .execute(db)
        .await?;
    Ok(())
}

async fn insert_transcript_item(
    db: &dyn Database,
    event: &SessionEvent,
    role: &str,
    kind: &str,
    status: &str,
    content: Option<String>,
) -> SessionDbResult<()> {
    insert_transcript_item_with_range(
        db,
        event.sequence,
        event.sequence,
        event.sequence,
        event_created_at_ms(event),
        role,
        kind,
        status,
        content,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn insert_transcript_item_with_range(
    db: &dyn Database,
    transcript_seq: u64,
    event_seq_start: u64,
    event_seq_end: u64,
    created_at_ms: u64,
    role: &str,
    kind: &str,
    status: &str,
    content: Option<String>,
) -> SessionDbResult<()> {
    let mut statement = db
        .insert("transcript_items")
        .value("transcript_seq", seq_to_value(transcript_seq))
        .value("event_seq_start", seq_to_value(event_seq_start))
        .value("event_seq_end", seq_to_value(event_seq_end))
        .value("role", role)
        .value("kind", kind)
        .value("status", status)
        .value("created_at_ms", seq_to_value(created_at_ms));

    if let Some(content) = content {
        statement = statement.value("content", content);
    }

    statement.execute(db).await?;
    Ok(())
}

async fn insert_tool_result_transcript_item(
    db: &dyn Database,
    event: &SessionEvent,
    tool_call_id: &str,
    is_error: bool,
) -> SessionDbResult<()> {
    let row = db
        .select("tool_runs")
        .columns(&["event_seq_start"])
        .where_eq("tool_call_id", tool_call_id.to_owned())
        .execute_first(db)
        .await?;
    let event_seq_start = row
        .as_ref()
        .map(|row| required_i64(row, "event_seq_start").map(i64_to_u64))
        .transpose()?
        .unwrap_or(event.sequence);
    insert_transcript_item_with_range(
        db,
        event.sequence,
        event_seq_start,
        event.sequence,
        event_created_at_ms(event),
        "tool",
        "result",
        if is_error { "error" } else { "complete" },
        None,
    )
    .await
}

async fn update_tool_transcript_item_end(
    db: &dyn Database,
    tool_call_id: &str,
    event_sequence: u64,
) -> SessionDbResult<()> {
    let row = db
        .select("tool_runs")
        .columns(&["event_seq_start"])
        .where_eq("tool_call_id", tool_call_id.to_owned())
        .execute_first(db)
        .await?;
    let Some(event_seq_start) = row
        .as_ref()
        .map(|row| required_i64(row, "event_seq_start").map(i64_to_u64))
        .transpose()?
    else {
        return Ok(());
    };
    db.update("transcript_items")
        .value("event_seq_end", seq_to_value(event_sequence))
        .where_eq("transcript_seq", seq_to_value(event_seq_start))
        .execute(db)
        .await?;
    Ok(())
}

fn tool_stream_tool_call_id(event: &ToolInvocationStreamEvent) -> &str {
    match event {
        ToolInvocationStreamEvent::Started { tool_call_id, .. }
        | ToolInvocationStreamEvent::OutputDelta { tool_call_id, .. }
        | ToolInvocationStreamEvent::VisualUpdate { tool_call_id, .. }
        | ToolInvocationStreamEvent::ArtifactUpdate { tool_call_id, .. }
        | ToolInvocationStreamEvent::Status { tool_call_id, .. }
        | ToolInvocationStreamEvent::LegacyPresentation { tool_call_id, .. }
        | ToolInvocationStreamEvent::LegacyTransientPruned { tool_call_id, .. }
        | ToolInvocationStreamEvent::Finished { tool_call_id, .. } => tool_call_id,
    }
}

const fn event_kind_name(kind: &SessionEventKind) -> &'static str {
    match kind {
        SessionEventKind::SessionCreated { .. } => "session_created",
        SessionEventKind::ClientAttached { .. } => "client_attached",
        SessionEventKind::ClientDetached { .. } => "client_detached",
        SessionEventKind::UserMessage { .. } => "user_message",
        SessionEventKind::AssistantDelta { .. } => "assistant_delta",
        SessionEventKind::AssistantMessage { .. } => "assistant_message",
        SessionEventKind::ToolCallRequested { .. } => "tool_call_requested",
        SessionEventKind::ToolCallFinished { .. } => "tool_call_finished",
        SessionEventKind::InteractiveToolRequestCreated { .. } => {
            "interactive_tool_request_created"
        }
        SessionEventKind::InteractiveToolRequestResolved { .. } => {
            "interactive_tool_request_resolved"
        }
        SessionEventKind::PermissionRequested { .. } => "permission_requested",
        SessionEventKind::PermissionResolved { .. } => "permission_resolved",
        SessionEventKind::ModelChanged { .. } => "model_changed",
        SessionEventKind::ReasoningChanged { .. } => "reasoning_changed",
        SessionEventKind::SystemMessage { .. } => "system_message",
        SessionEventKind::AgentChanged { .. } => "agent_changed",
        SessionEventKind::ModelTurnStarted { .. } => "model_turn_started",
        SessionEventKind::ModelTurnFinished { .. } => "model_turn_finished",
        SessionEventKind::ModelUsage { .. } => "model_usage",
        SessionEventKind::ContextCompacted { .. } => "context_compacted",
        SessionEventKind::ProviderContextCompacted { .. } => "provider_context_compacted",
        SessionEventKind::RequestContextObserved { .. } => "request_context_observed",
        SessionEventKind::SessionRenamed { .. } => "session_renamed",
        SessionEventKind::TraceEvent { .. } => "trace_event",
        SessionEventKind::SkillInvoked { .. } => "skill_invoked",
        SessionEventKind::SkillSuggested { .. } => "skill_suggested",
        SessionEventKind::SkillActivated { .. } => "skill_activated",
        SessionEventKind::SkillDeactivated { .. } => "skill_deactivated",
        SessionEventKind::SkillContextLoaded { .. } => "skill_context_loaded",
        SessionEventKind::SkillInvocationFailed { .. } => "skill_invocation_failed",
        SessionEventKind::AssistantReasoningDelta { .. } => "assistant_reasoning_delta",
        SessionEventKind::AssistantReasoningMessage { .. } => "assistant_reasoning_message",
        SessionEventKind::RuntimeWorkStarted { .. } => "runtime_work_started",
        SessionEventKind::RuntimeWorkFinished { .. } => "runtime_work_finished",
        SessionEventKind::RuntimeWorkProgress { .. } => "runtime_work_progress",
        SessionEventKind::RuntimeWorkCancelRequested { .. } => "runtime_work_cancel_requested",
        SessionEventKind::ModelTurnCancelRequested { .. } => "model_turn_cancel_requested",
        SessionEventKind::ToolInvocationStream { .. } => "tool_invocation_stream",
        SessionEventKind::WorkingDirectoryChanged { .. } => "working_directory_changed",
        SessionEventKind::SessionImported { .. } => "session_imported",
        SessionEventKind::SessionForked { .. } => "session_forked",
        SessionEventKind::RalphLifecycle { .. } => "ralph_lifecycle",
        SessionEventKind::PluginStatusNote { .. } => "plugin_status_note",
        SessionEventKind::PluginAutomationTurnStarted { .. } => "plugin_automation_turn_started",
        SessionEventKind::PluginAutomationTurnFinished { .. } => "plugin_automation_turn_finished",
    }
}

fn generic_artifact_reference_metadata(
    reference: &bcode_session_models::ToolArtifactRef,
) -> (Option<String>, Option<bool>, Option<String>) {
    let metadata = reference.metadata.as_ref();
    let availability = metadata
        .and_then(|metadata| metadata.get("availability"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    let complete = metadata
        .and_then(|metadata| metadata.get("complete"))
        .and_then(serde_json::Value::as_bool);
    let checksum_sha256 = metadata
        .and_then(|metadata| metadata.get("checksum_sha256"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    (availability, complete, checksum_sha256)
}

async fn project_artifact_references(
    db: &dyn Database,
    finalized_event_seq: u64,
    artifact: &bcode_session_models::ToolArtifact,
) -> SessionDbResult<()> {
    for reference in &artifact.refs {
        let (availability, complete, checksum_sha256) =
            generic_artifact_reference_metadata(reference);
        db.upsert("artifact_references")
            .value("artifact_id", artifact.artifact_id.clone())
            .value("reference_key", reference.key.clone())
            .value("producer_plugin_id", artifact.producer_plugin_id.clone())
            .value("schema", artifact.schema.clone())
            .value(
                "schema_version",
                DatabaseValue::Int64(i64::from(artifact.schema_version)),
            )
            .value("storage_uri", reference.storage_uri.clone())
            .value("content_type", reference.content_type.clone())
            .value("byte_len", reference.byte_len.map(seq_to_value))
            .value("availability", availability)
            .value("complete", complete.map(bool_to_value))
            .value("checksum_sha256", checksum_sha256)
            .value("finalized_event_seq", seq_to_value(finalized_event_seq))
            .execute(db)
            .await?;
    }
    Ok(())
}

fn finalized_artifact_reference_from_row(
    row: &switchy::database::Row,
) -> SessionDbResult<FinalizedArtifactReference> {
    Ok(FinalizedArtifactReference {
        artifact_id: required_string(row, "artifact_id")?,
        reference_key: required_string(row, "reference_key")?,
        producer_plugin_id: required_string(row, "producer_plugin_id")?,
        schema: required_string(row, "schema")?,
        schema_version: u32::try_from(required_i64(row, "schema_version")?).map_err(|_| {
            SessionDbError::InvalidRow {
                column: "schema_version".to_owned(),
            }
        })?,
        storage_uri: optional_string(row, "storage_uri"),
        content_type: optional_string(row, "content_type"),
        byte_len: optional_i64(row, "byte_len").map(i64_to_u64),
        availability: optional_string(row, "availability"),
        complete: optional_i64(row, "complete").map(|value| value != 0),
        checksum_sha256: optional_string(row, "checksum_sha256"),
        finalized_event_seq: required_i64(row, "finalized_event_seq").map(i64_to_u64)?,
    })
}

async fn update_projection_checkpoint(
    db: &dyn Database,
    projection: MaterializedProjection,
    event: &SessionEvent,
) -> SessionDbResult<()> {
    let projection_name = projection.as_str();
    let projection_version = projection.schema_version();
    let existing = db
        .select("projection_checkpoints")
        .columns(&["projection_name"])
        .where_eq("projection_name", projection_name)
        .execute_first(db)
        .await?;

    if existing.is_some() {
        db.update("projection_checkpoints")
            .value("last_event_seq", seq_to_value(event.sequence))
            .value(
                "projection_version",
                DatabaseValue::Int64(i64::from(projection_version)),
            )
            .value("updated_at_ms", seq_to_value(event_created_at_ms(event)))
            .where_eq("projection_name", projection_name)
            .execute(db)
            .await?;
    } else {
        db.insert("projection_checkpoints")
            .value("projection_name", projection_name)
            .value("last_event_seq", seq_to_value(event.sequence))
            .value(
                "projection_version",
                DatabaseValue::Int64(i64::from(projection_version)),
            )
            .value("updated_at_ms", seq_to_value(event_created_at_ms(event)))
            .execute(db)
            .await?;
    }

    Ok(())
}

fn runtime_work_from_row(row: &switchy::database::Row) -> SessionDbResult<RuntimeWorkProjection> {
    Ok(RuntimeWorkProjection {
        work_id: WorkId::new(required_string(row, "work_id")?),
        event_seq_start: required_i64(row, "event_seq_start").map(i64_to_u64)?,
        event_seq_end: optional_i64(row, "event_seq_end").map(i64_to_u64),
        kind: parse_runtime_work_kind(&required_string(row, "kind")?),
        label: required_string(row, "label")?,
        status: parse_runtime_work_status(&required_string(row, "status")?),
        parent_work_id: optional_string(row, "parent_work_id").map(WorkId::new),
        started_at_ms: optional_i64(row, "started_at_ms").map(i64_to_u64),
        finished_at_ms: optional_i64(row, "finished_at_ms").map(i64_to_u64),
        message: optional_string(row, "message"),
        cancellable: optional_i64(row, "cancellable").is_some_and(|value| value != 0),
    })
}

fn tool_run_from_row(row: &switchy::database::Row) -> SessionDbResult<ToolRun> {
    Ok(ToolRun {
        tool_call_id: required_string(row, "tool_call_id")?,
        event_seq_start: required_i64(row, "event_seq_start").map(i64_to_u64)?,
        event_seq_end: optional_i64(row, "event_seq_end").map(i64_to_u64),
        status: required_string(row, "status")?,
        tool_name: optional_string(row, "tool_name"),
        is_error: optional_i64(row, "is_error").map(|value| value != 0),
    })
}

fn session_summary_from_catalog_row(
    row: &switchy::database::Row,
) -> SessionDbResult<SessionSummary> {
    let session_id =
        required_string(row, "session_id")?
            .parse()
            .map_err(|_| SessionDbError::InvalidRow {
                column: "session_id".to_string(),
            })?;
    let working_directory = std::path::PathBuf::from(required_string(row, "working_directory")?);
    let name = optional_string(row, "title");
    Ok(SessionSummary {
        id: session_id,
        name: name.clone(),
        explicit_name: name,
        derived_title: None,
        title_source: SessionTitleSource::Explicit,
        client_count: 0,
        created_at_ms: required_i64(row, "created_at_ms").map(i64_to_u64)?,
        updated_at_ms: required_i64(row, "updated_at_ms").map(i64_to_u64)?,
        working_directory,
        import: None,
        fork: None,
    })
}

fn transcript_item_from_row(row: &switchy::database::Row) -> SessionDbResult<TranscriptItem> {
    Ok(TranscriptItem {
        transcript_seq: required_i64(row, "transcript_seq").map(i64_to_u64)?,
        event_seq_start: required_i64(row, "event_seq_start").map(i64_to_u64)?,
        event_seq_end: required_i64(row, "event_seq_end").map(i64_to_u64)?,
        role: required_string(row, "role")?,
        kind: required_string(row, "kind")?,
        status: required_string(row, "status")?,
        content: optional_string(row, "content"),
    })
}

fn input_history_entry_from_row(
    row: &switchy::database::Row,
) -> SessionDbResult<SessionInputHistoryEntry> {
    Ok(SessionInputHistoryEntry {
        sequence: required_i64(row, "event_seq").map(i64_to_u64)?,
        timestamp_ms: optional_i64(row, "created_at_ms").map_or(0, i64_to_u64),
        text: required_string(row, "text")?,
    })
}

fn model_context_events_query(
    db: &dyn Database,
    compacted_through_sequence: Option<u64>,
) -> SelectQuery<'static> {
    let mut select = db
        .select("events")
        .columns(&["payload"])
        .where_in(
            "event_type",
            MODEL_CONTEXT_EVENT_TYPES
                .iter()
                .map(|event_type| DatabaseValue::String((*event_type).to_string()))
                .collect::<Vec<_>>(),
        )
        .sort("event_seq", SortDirection::Asc);
    if let Some(boundary) = compacted_through_sequence {
        select = select.where_gt("event_seq", seq_to_value(boundary));
    }
    select
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextHistoryRole {
    ModelVisible,
    Structural,
    Excluded,
}

const MODEL_CONTEXT_EVENT_TYPES: &[&str] = &[
    "user_message",
    "assistant_message",
    "tool_call_requested",
    "tool_call_finished",
    "system_message",
    "working_directory_changed",
    "context_compacted",
    "provider_context_compacted",
    "model_turn_started",
    "model_turn_finished",
];

const fn context_history_role(kind: &SessionEventKind) -> ContextHistoryRole {
    match kind {
        SessionEventKind::UserMessage { .. }
        | SessionEventKind::AssistantMessage { .. }
        | SessionEventKind::ToolCallRequested { .. }
        | SessionEventKind::ToolCallFinished { .. }
        | SessionEventKind::SystemMessage { .. }
        | SessionEventKind::WorkingDirectoryChanged { .. }
        | SessionEventKind::ContextCompacted { .. }
        | SessionEventKind::ProviderContextCompacted { .. } => ContextHistoryRole::ModelVisible,
        SessionEventKind::ModelTurnStarted { .. } | SessionEventKind::ModelTurnFinished { .. } => {
            ContextHistoryRole::Structural
        }
        _ => ContextHistoryRole::Excluded,
    }
}

const fn context_history_role_from_name(event_type: &str) -> ContextHistoryRole {
    match event_type.as_bytes() {
        b"user_message"
        | b"assistant_message"
        | b"tool_call_requested"
        | b"tool_call_finished"
        | b"system_message"
        | b"working_directory_changed"
        | b"context_compacted"
        | b"provider_context_compacted" => ContextHistoryRole::ModelVisible,
        b"model_turn_started" | b"model_turn_finished" => ContextHistoryRole::Structural,
        _ => ContextHistoryRole::Excluded,
    }
}

const fn is_model_context_event_type(event_type: &str) -> bool {
    !matches!(
        context_history_role_from_name(event_type),
        ContextHistoryRole::Excluded
    )
}

fn canonical_model_context_from_events(
    events: impl IntoIterator<Item = SessionEvent>,
) -> Vec<SessionEvent> {
    let events = events.into_iter().collect::<Vec<_>>();
    let Some(marker) = events
        .iter()
        .filter(|event| {
            matches!(
                event.kind,
                SessionEventKind::ContextCompacted { .. }
                    | SessionEventKind::ProviderContextCompacted { .. }
            )
        })
        .max_by_key(|event| event.sequence)
        .cloned()
    else {
        return events
            .into_iter()
            .filter(|event| is_model_context_event_type(model_context_event_kind_name(&event.kind)))
            .collect();
    };
    let boundary = match &marker.kind {
        SessionEventKind::ContextCompacted {
            compacted_through_sequence,
            ..
        }
        | SessionEventKind::ProviderContextCompacted {
            compacted_through_sequence,
            ..
        } => *compacted_through_sequence,
        _ => unreachable!("marker selection accepts only compaction events"),
    };
    let mut retained = events
        .into_iter()
        .filter(|event| {
            event.sequence > boundary
                && event.sequence != marker.sequence
                && is_model_context_event_type(model_context_event_kind_name(&event.kind))
                && !matches!(
                    event.kind,
                    SessionEventKind::ContextCompacted { .. }
                        | SessionEventKind::ProviderContextCompacted { .. }
                )
        })
        .collect::<Vec<_>>();
    retained.sort_by_key(|event| event.sequence);
    let mut context = Vec::with_capacity(retained.len().saturating_add(1));
    context.push(marker);
    context.extend(retained);
    context
}

const fn model_context_event_kind_name(kind: &SessionEventKind) -> &'static str {
    match kind {
        SessionEventKind::UserMessage { .. } => "user_message",
        SessionEventKind::AssistantMessage { .. } => "assistant_message",
        SessionEventKind::ToolCallRequested { .. } => "tool_call_requested",
        SessionEventKind::ToolCallFinished { .. } => "tool_call_finished",
        SessionEventKind::SystemMessage { .. } => "system_message",
        SessionEventKind::WorkingDirectoryChanged { .. } => "working_directory_changed",
        SessionEventKind::ContextCompacted { .. } => "context_compacted",
        SessionEventKind::ProviderContextCompacted { .. } => "provider_context_compacted",
        SessionEventKind::ModelTurnStarted { .. } => "model_turn_started",
        SessionEventKind::ModelTurnFinished { .. } => "model_turn_finished",
        SessionEventKind::RequestContextObserved { .. } => "request_context_observed",
        _ => "non_model_context",
    }
}

fn required_string(row: &switchy::database::Row, column: &str) -> SessionDbResult<String> {
    row.get(column)
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .ok_or_else(|| SessionDbError::InvalidRow {
            column: column.to_string(),
        })
}

fn optional_string(row: &switchy::database::Row, column: &str) -> Option<String> {
    row.get(column)
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
}

fn required_i64(row: &switchy::database::Row, column: &str) -> SessionDbResult<i64> {
    row.get(column)
        .and_then(|value| value.as_i64())
        .ok_or_else(|| SessionDbError::InvalidRow {
            column: column.to_string(),
        })
}

fn optional_i64(row: &switchy::database::Row, column: &str) -> Option<i64> {
    row.get(column).and_then(|value| value.as_i64())
}

const fn i64_to_u64(value: i64) -> u64 {
    if value.is_negative() {
        0
    } else {
        value.cast_unsigned()
    }
}

const fn runtime_work_kind_name(kind: RuntimeWorkKind) -> &'static str {
    match kind {
        RuntimeWorkKind::Tool => "tool",
        RuntimeWorkKind::PluginInvocation => "plugin_invocation",
        RuntimeWorkKind::ModelTurn => "model_turn",
        RuntimeWorkKind::EventDelivery => "event_delivery",
    }
}

fn parse_runtime_work_kind(value: &str) -> RuntimeWorkKind {
    match value {
        "plugin_invocation" => RuntimeWorkKind::PluginInvocation,
        "model_turn" => RuntimeWorkKind::ModelTurn,
        "event_delivery" => RuntimeWorkKind::EventDelivery,
        _ => RuntimeWorkKind::Tool,
    }
}

const fn runtime_work_status_name(status: RuntimeWorkStatus) -> &'static str {
    match status {
        RuntimeWorkStatus::Queued => "queued",
        RuntimeWorkStatus::Running => "running",
        RuntimeWorkStatus::Cancelling => "cancelling",
        RuntimeWorkStatus::Completed => "completed",
        RuntimeWorkStatus::Failed => "failed",
        RuntimeWorkStatus::TimedOut => "timed_out",
        RuntimeWorkStatus::Cancelled => "cancelled",
    }
}

fn parse_runtime_work_status(value: &str) -> RuntimeWorkStatus {
    match value {
        "queued" => RuntimeWorkStatus::Queued,
        "cancelling" => RuntimeWorkStatus::Cancelling,
        "completed" => RuntimeWorkStatus::Completed,
        "failed" => RuntimeWorkStatus::Failed,
        "timed_out" => RuntimeWorkStatus::TimedOut,
        "cancelled" => RuntimeWorkStatus::Cancelled,
        _ => RuntimeWorkStatus::Running,
    }
}

const fn event_created_at_ms(event: &SessionEvent) -> u64 {
    event.timestamp_ms
}

fn seq_to_value(sequence: u64) -> DatabaseValue {
    DatabaseValue::Int64(i64::try_from(sequence).unwrap_or(i64::MAX))
}

#[allow(dead_code)]
fn usize_to_value(value: usize) -> DatabaseValue {
    DatabaseValue::Int64(i64::try_from(value).unwrap_or(i64::MAX))
}

const fn bool_to_value(value: bool) -> DatabaseValue {
    DatabaseValue::Int32(if value { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::{
        CURRENT_SESSION_EVENT_SCHEMA_VERSION, ClientId, RequestContextObservation,
        RequestContextTokenCount,
    };
    use std::collections::BTreeMap;

    fn session_storage_files(root: &Path, session_id: SessionId) -> BTreeMap<String, Vec<u8>> {
        let session_dir = root.join(session_id.to_string());
        std::fs::read_dir(session_dir)
            .expect("session directory")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let bytes = std::fs::read(entry.path()).expect("read session storage file");
                (name, bytes)
            })
            .collect()
    }

    async fn reindex_model_context_for_test(
        db: &SessionDb,
        root: &Path,
        session_id: SessionId,
    ) -> usize {
        let maintenance = crate::lease::acquire_session_maintenance_guard(root, session_id)
            .expect("maintenance guard");
        let write =
            crate::lease::acquire_maintenance_session_write_lock(&maintenance, root, session_id)
                .expect("write guard");
        db.reindex_model_context(&maintenance, &write)
            .await
            .expect("explicit reindex")
    }

    fn request_context_event(session_id: SessionId, sequence: u64) -> SessionEvent {
        event(
            session_id,
            sequence,
            SessionEventKind::RequestContextObserved {
                observation: RequestContextObservation {
                    request: bcode_session_models::ModelRequestIdentity {
                        provider_plugin_id: "provider".to_string(),
                        requested_model_id: Some("alias".to_string()),
                        effective_model_id: "model".to_string(),
                        request_id: format!("request-{sequence}"),
                        model_turn_id: format!("turn-{sequence}"),
                        round: 0,
                        request_fingerprint: format!("fingerprint-{sequence}"),
                        effective_auth_profile: None,
                        context_format_version: None,
                        compatibility_key: None,
                        context_epoch: 0,
                    },
                    context_through_sequence: sequence.saturating_sub(1),
                    context_tokens: RequestContextTokenCount::Estimated(sequence),
                    local_estimate: bcode_session_models::LocalContextEstimate {
                        tokens: sequence,
                        algorithm_version: 1,
                    },
                },
            },
        )
    }

    fn event(session_id: SessionId, sequence: u64, kind: SessionEventKind) -> SessionEvent {
        SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind,
        }
    }

    fn local_marker(session_id: SessionId, sequence: u64, boundary: u64) -> SessionEvent {
        event(
            session_id,
            sequence,
            SessionEventKind::ContextCompacted {
                summary: format!("local-{sequence}"),
                compacted_through_sequence: boundary,
            },
        )
    }

    fn provider_marker(session_id: SessionId, sequence: u64, boundary: u64) -> SessionEvent {
        event(
            session_id,
            sequence,
            SessionEventKind::ProviderContextCompacted {
                snapshot: bcode_session_models::ProviderContextSnapshot {
                    format_version: 1,
                    request_fingerprint: None,
                    request_id: None,
                    provider_plugin_id: "provider".to_string(),
                    model_id: "model".to_string(),
                    compatibility_key: "surface".to_string(),
                    auth_profile: None,
                    origin: bcode_session_models::ProviderContextSnapshotOrigin::Explicit,
                    messages_json: "[]".to_string(),
                    portable_summary: format!("provider-{sequence}"),
                },
                compacted_through_sequence: boundary,
            },
        )
    }

    fn message(session_id: SessionId, sequence: u64, text: &str) -> SessionEvent {
        event(
            session_id,
            sequence,
            SessionEventKind::AssistantMessage {
                text: text.to_string(),
            },
        )
    }

    fn assert_marker_transition(first: SessionEvent, second: SessionEvent) {
        let session_id = first.session_id;
        let events = canonical_model_context_from_events(vec![
            message(session_id, 1, "old"),
            first,
            message(session_id, 4, "retained-before-marker"),
            second,
            message(session_id, 7, "retained-after-marker"),
        ]);
        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![6, 4, 7]
        );
    }

    #[test]
    fn local_to_local_uses_newest_marker_boundary() {
        let id = SessionId::new();
        assert_marker_transition(local_marker(id, 3, 1), local_marker(id, 6, 2));
    }

    #[test]
    fn local_to_provider_uses_newest_marker_boundary() {
        let id = SessionId::new();
        assert_marker_transition(local_marker(id, 3, 1), provider_marker(id, 6, 2));
    }

    #[test]
    fn provider_to_local_uses_newest_marker_boundary() {
        let id = SessionId::new();
        assert_marker_transition(provider_marker(id, 3, 1), local_marker(id, 6, 2));
    }

    #[test]
    fn provider_to_provider_uses_newest_marker_boundary() {
        let id = SessionId::new();
        assert_marker_transition(provider_marker(id, 3, 1), provider_marker(id, 6, 2));
    }

    #[test]
    fn retained_event_before_marker_sequence_survives_boundary() {
        let id = SessionId::new();
        let events = canonical_model_context_from_events(vec![
            message(id, 4, "retained"),
            local_marker(id, 10, 2),
        ]);
        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![10, 4]
        );
    }

    #[test]
    fn retained_event_after_marker_insertion_survives_boundary() {
        let id = SessionId::new();
        let events = canonical_model_context_from_events(vec![
            local_marker(id, 3, 2),
            message(id, 4, "after"),
        ]);
        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![3, 4]
        );
    }

    #[test]
    fn older_markers_are_excluded_from_canonical_context() {
        let id = SessionId::new();
        let events = canonical_model_context_from_events(vec![
            local_marker(id, 3, 1),
            provider_marker(id, 5, 2),
            local_marker(id, 7, 4),
        ]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].sequence, 7);
    }

    #[tokio::test]
    async fn db_projection_matches_in_memory_boundary_projection() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let id = SessionId::new();
        let db = SessionDb::open_turso_in_root(id, temp_dir.path())
            .await
            .expect("db");
        let source = vec![
            message(id, 0, "old"),
            local_marker(id, 1, 0),
            message(id, 2, "retained"),
            provider_marker(id, 3, 1),
            message(id, 4, "after"),
        ];
        for item in &source {
            db.append_event(item).await.expect("append");
        }
        assert_eq!(
            db.model_context_events().await.expect("db context"),
            canonical_model_context_from_events(source)
        );
    }

    #[test]
    fn legacy_local_compaction_fixture_decodes() {
        let id = SessionId::new();
        let payload = serde_json::json!({
            "schema_version": 1, "sequence": 3, "timestamp_ms": 1, "session_id": id,
            "kind": {"context_compacted": {"summary": "legacy", "compacted_through_sequence": 2}}
        });
        let decoded = decode_session_event_degraded(&payload.to_string()).expect("legacy local");
        assert!(matches!(
            decoded.kind,
            SessionEventKind::ContextCompacted { .. }
        ));
    }

    #[test]
    fn legacy_provider_snapshot_fixture_defaults_new_identity_fields() {
        let id = SessionId::new();
        let payload = serde_json::json!({
            "schema_version": 1, "sequence": 3, "timestamp_ms": 1, "session_id": id,
            "kind": {"provider_context_compacted": {
                "snapshot": {"provider_plugin_id": "p", "model_id": "m", "messages_json": "[]", "portable_summary": "portable"},
                "compacted_through_sequence": 2
            }}
        });
        let decoded = decode_session_event_degraded(&payload.to_string()).expect("legacy provider");
        let SessionEventKind::ProviderContextCompacted { snapshot, .. } = decoded.kind else {
            panic!("provider marker")
        };
        assert_eq!(snapshot.format_version, 1);
        assert!(snapshot.compatibility_key.is_empty());
        assert_eq!(
            snapshot.origin,
            bcode_session_models::ProviderContextSnapshotOrigin::Explicit
        );
    }

    #[tokio::test]
    async fn observed_session_open_records_initialization_and_migrations() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let metrics = MetricsRegistry::in_memory();
        let session_id = SessionId::new();
        let db =
            SessionDb::open_turso_in_root_observed(session_id, temp_dir.path(), metrics.clone())
                .await
                .expect("observed session database should open");
        drop(db);

        let report = metrics.report();
        assert!(
            report
                .snapshot
                .counters
                .get("database.operation.total")
                .is_some_and(|count| *count > 1)
        );
        assert!(
            report.descriptors["database.operation.total"]
                .label_keys
                .contains(&"database_role".to_owned())
        );
    }

    #[test]
    fn identifies_database_lock_errors() {
        assert!(is_database_lock_error_message(
            "Locking error: Failed locking file '/tmp/session.db-wal'"
        ));
        assert!(is_database_lock_error_message("database is locked"));
        assert!(is_database_lock_error_message("database busy"));
    }

    #[test]
    fn ignores_non_lock_database_errors() {
        assert!(!is_database_lock_error_message("permission denied"));
    }

    #[tokio::test]
    async fn normal_history_reads_skip_corrupt_and_future_persisted_events_without_repair() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");

        db.append_event(&event(
            session_id,
            0,
            SessionEventKind::SessionCreated {
                name: Some("test".to_string()),
                working_directory: temp_dir.path().to_path_buf(),
            },
        ))
        .await
        .expect("append valid event");

        db.database()
            .insert("events")
            .value("event_seq", seq_to_value(1))
            .value("event_type", "future_event_kind")
            .value(
                "schema_version",
                DatabaseValue::Int32(i32::from(CURRENT_SESSION_EVENT_SCHEMA_VERSION)),
            )
            .value("created_at_ms", seq_to_value(1))
            .value(
                "payload",
                serde_json::json!({
                    "schema_version": CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                    "sequence": 1,
                    "session_id": session_id,
                    "kind": { "future_event_kind": { "value": true } }
                })
                .to_string(),
            )
            .execute(db.database())
            .await
            .expect("insert future event");
        db.database()
            .insert("events")
            .value("event_seq", seq_to_value(2))
            .value("event_type", "tool_call_finished")
            .value(
                "schema_version",
                DatabaseValue::Int32(i32::from(CURRENT_SESSION_EVENT_SCHEMA_VERSION)),
            )
            .value("created_at_ms", seq_to_value(2))
            .value(
                "payload",
                serde_json::json!({
                    "schema_version": CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                    "sequence": 2,
                    "session_id": session_id,
                    "kind": { "tool_call_finished": { "result": "missing call id" } }
                })
                .to_string(),
            )
            .execute(db.database())
            .await
            .expect("insert corrupt event");
        insert_event(
            db.database(),
            &event(
                session_id,
                3,
                SessionEventKind::AssistantMessage {
                    text: "still readable".to_string(),
                },
            ),
            None,
        )
        .await
        .expect("insert second valid event");

        let history = db.all_events().await.expect("history should degrade");
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].sequence, 0);
        assert_eq!(history[1].sequence, 3);

        let page = db
            .history_page(SessionHistoryQuery {
                cursor: None,
                direction: SessionHistoryDirection::Forward,
                limit: 8,
            })
            .await
            .expect("page should degrade");
        assert_eq!(page.events.len(), 2);
        assert_eq!(page.events[0].sequence, 0);
        assert_eq!(page.events[1].sequence, 3);

        let raw_rows = db
            .database()
            .select("events")
            .columns(&["event_seq"])
            .sort("event_seq", SortDirection::Asc)
            .execute(db.database())
            .await
            .expect("raw rows should remain");
        assert_eq!(raw_rows.len(), 4);
    }

    #[tokio::test]
    async fn append_event_updates_input_history_and_transcript_projections() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");

        db.append_event(&event(
            session_id,
            0,
            SessionEventKind::SessionCreated {
                name: Some("test".to_string()),
                working_directory: temp_dir.path().to_path_buf(),
            },
        ))
        .await
        .expect("append session created");
        db.append_event(&event(
            session_id,
            1,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "hello".to_string(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        ))
        .await
        .expect("append user message");
        db.append_event(&event(
            session_id,
            2,
            SessionEventKind::AssistantMessage {
                text: "hi there".to_string(),
            },
        ))
        .await
        .expect("append assistant message");

        let input_history = db.input_history().await.expect("input history");
        assert_eq!(
            input_history,
            vec![SessionInputHistoryEntry {
                sequence: 1,
                timestamp_ms: 1,
                text: "hello".to_string(),
            }]
        );

        let transcript = db
            .latest_transcript_items(10)
            .await
            .expect("transcript items");
        assert_eq!(transcript.len(), 2);
        assert_eq!(transcript[0].role, "user");
        assert_eq!(transcript[0].content.as_deref(), Some("hello"));
        assert_eq!(transcript[1].role, "assistant");
        assert_eq!(transcript[1].content.as_deref(), Some("hi there"));

        assert_eq!(
            db.materialized_projection_checkpoint(MaterializedProjection::InputHistory)
                .await
                .expect("input checkpoint"),
            Some(2)
        );
        assert_eq!(
            db.materialized_projection_checkpoint(MaterializedProjection::Transcript)
                .await
                .expect("transcript checkpoint"),
            Some(2)
        );
        for projection in MaterializedProjection::all() {
            assert_eq!(
                db.materialized_projection_checkpoint(*projection)
                    .await
                    .expect("materialized projection checkpoint"),
                Some(2),
                "projection {} should be checkpointed",
                projection.as_str()
            );
        }
        assert_eq!(
            db.projection_checkpoint("model_context")
                .await
                .expect("model context should not be checkpointed"),
            None
        );
    }

    #[tokio::test]
    async fn active_tool_runs_reflect_running_projection_rows() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");

        db.append_event(&event(
            session_id,
            0,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "tool-1".to_string(),
                producer_plugin_id: None,
                tool_name: "shell".to_string(),
                arguments_json: "{}".to_string(),
                working_directory: None,
                request_visual: None,
                legacy_request_presentation: None,
            },
        ))
        .await
        .expect("append tool request");

        let active = db.active_tool_runs().await.expect("active tool runs");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].tool_call_id, "tool-1");
        assert_eq!(active[0].tool_name.as_deref(), Some("shell"));
        assert_eq!(active[0].status, "running");

        db.append_event(&event(
            session_id,
            1,
            SessionEventKind::ToolCallFinished {
                tool_call_id: "tool-1".to_string(),
                result: "done".to_string(),
                is_error: false,
                output: None,
                semantic_result: None,
            },
        ))
        .await
        .expect("append tool finish");

        assert!(
            db.active_tool_runs()
                .await
                .expect("active after finish")
                .is_empty()
        );
        let completed = db
            .tool_runs_by_status("complete")
            .await
            .expect("complete tool runs");
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].event_seq_end, Some(1));
        assert_eq!(completed[0].is_error, Some(false));
    }

    #[tokio::test]
    async fn active_runtime_work_uses_projection_rows() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");
        let work_id = WorkId::new("work-1");

        db.append_event(&event(
            session_id,
            0,
            SessionEventKind::RuntimeWorkStarted {
                work_id: work_id.clone(),
                kind: RuntimeWorkKind::Tool,
                label: "shell".to_string(),
                tool_call_id: None,
                plugin_id: None,
                service_interface: None,
                operation: None,
                parent_work_id: None,
                started_at_ms: Some(123),
                cancellable: true,
            },
        ))
        .await
        .expect("append runtime work start");

        let active = db.active_runtime_work().await.expect("active runtime work");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].work_id, work_id);
        assert_eq!(active[0].label, "shell");
        assert_eq!(active[0].status, RuntimeWorkStatus::Running);
        assert!(active[0].cancellable);

        db.append_event(&event(
            session_id,
            1,
            SessionEventKind::RuntimeWorkFinished {
                work_id: work_id.clone(),
                status: RuntimeWorkStatus::Failed,
                finished_at_ms: Some(456),
                message: Some("daemon stopped".to_string()),
            },
        ))
        .await
        .expect("append runtime work finish");

        assert!(
            db.active_runtime_work()
                .await
                .expect("active after finish")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn provider_snapshot_opaque_json_survives_exact_db_persistence_and_replay() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open db");
        let opaque = r#"[{"type":"provider_state","nested":{"escaped":"a\\nb","order":[3,2,1]},"unicode":"λ"}]"#;
        db.append_event(&event(
            session_id,
            0,
            SessionEventKind::ProviderContextCompacted {
                snapshot: bcode_session_models::ProviderContextSnapshot {
                    format_version: 7,
                    request_fingerprint: None,
                    request_id: None,
                    provider_plugin_id: "provider".to_string(),
                    model_id: "model".to_string(),
                    compatibility_key: "surface".to_string(),
                    auth_profile: Some("profile".to_string()),
                    origin: bcode_session_models::ProviderContextSnapshotOrigin::Explicit,
                    messages_json: opaque.to_string(),
                    portable_summary: "portable".to_string(),
                },
                compacted_through_sequence: 0,
            },
        ))
        .await
        .expect("append snapshot");
        drop(db);

        let reopened = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("reopen db");
        let context = reopened.model_context_events().await.expect("context");
        let SessionEventKind::ProviderContextCompacted { snapshot, .. } = &context[0].kind else {
            panic!("expected provider snapshot");
        };
        assert_eq!(snapshot.messages_json.as_bytes(), opaque.as_bytes());
        assert_eq!(snapshot.format_version, 7);
        assert_eq!(snapshot.compatibility_key, "surface");
        assert_eq!(snapshot.auth_profile.as_deref(), Some("profile"));
    }

    #[tokio::test]
    async fn normal_model_context_read_is_non_mutating_and_does_not_replay_replaced_prefix() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open db");
        db.append_event(&message(session_id, 0, "old"))
            .await
            .expect("old");
        db.append_event(&local_marker(session_id, 1, 0))
            .await
            .expect("marker");
        db.append_event(&message(session_id, 2, "tail"))
            .await
            .expect("tail");
        db.database()
            .update("events")
            .value("payload", "not valid json")
            .where_eq("event_seq", seq_to_value(0))
            .execute(db.database())
            .await
            .expect("corrupt replaced prefix");
        let rows_before = db
            .database()
            .select("events")
            .columns(&["event_seq", "payload"])
            .sort("event_seq", SortDirection::Asc)
            .execute(db.database())
            .await
            .expect("before");

        let context = db
            .model_context_events()
            .await
            .expect("bounded context read");

        assert_eq!(
            context
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        let rows_after = db
            .database()
            .select("events")
            .columns(&["event_seq", "payload"])
            .sort("event_seq", SortDirection::Asc)
            .execute(db.database())
            .await
            .expect("after");
        assert_eq!(rows_before, rows_after);
        assert_eq!(db.last_event_sequence().await.expect("last"), Some(2));
    }

    #[tokio::test]
    async fn model_context_query_selects_only_payload_after_filtering_semantic_types() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");
        let query = model_context_events_query(db.database(), Some(42));

        assert_eq!(query.columns, &["payload"]);
        assert_eq!(query.sorts.as_ref().map(Vec::len), Some(1));
        assert_eq!(query.filters.as_ref().map(Vec::len), Some(2));
        let values = query
            .filters
            .as_ref()
            .expect("semantic and boundary filters")
            .iter()
            .flat_map(|filter| filter.values().unwrap_or_default())
            .collect::<Vec<_>>();
        assert_eq!(values.len(), MODEL_CONTEXT_EVENT_TYPES.len() + 1);
        for event_type in MODEL_CONTEXT_EVENT_TYPES {
            assert!(values.contains(&&DatabaseValue::String((*event_type).to_string())));
        }
        assert!(values.contains(&&seq_to_value(42)));
    }

    #[test]
    fn context_history_roles_separate_model_visible_structural_and_excluded_events() {
        assert!(is_model_context_event_type("model_turn_started"));
        assert!(is_model_context_event_type("model_turn_finished"));
        assert!(!is_model_context_event_type("context_usage_observed"));
        assert!(!is_model_context_event_type("request_context_observed"));
    }

    #[tokio::test]
    #[ignore = "requires BCODE_MODEL_CONTEXT_BENCHMARK_DB and BCODE_MODEL_CONTEXT_BENCHMARK_SESSION_ID"]
    async fn benchmark_model_context_events_from_database_copy() {
        let path = std::env::var_os("BCODE_MODEL_CONTEXT_BENCHMARK_DB")
            .map(std::path::PathBuf::from)
            .expect("BCODE_MODEL_CONTEXT_BENCHMARK_DB must name a disposable database copy");
        let session_id = std::env::var("BCODE_MODEL_CONTEXT_BENCHMARK_SESSION_ID")
            .expect("BCODE_MODEL_CONTEXT_BENCHMARK_SESSION_ID must be set")
            .parse::<SessionId>()
            .expect("benchmark session ID must be valid");
        let db = SessionDb::open_turso(session_id, &path)
            .await
            .expect("open benchmark database copy");
        let compatibility_started_at = std::time::Instant::now();
        let compatibility = db
            .model_context_events()
            .await
            .expect("compatibility context");
        let compatibility_elapsed_ms = compatibility_started_at.elapsed().as_millis();
        if matches!(
            db.model_context_projection_status()
                .await
                .expect("projection status"),
            ModelContextProjectionStatus::Missing
        ) {
            let root = path
                .parent()
                .and_then(Path::parent)
                .expect("benchmark DB must use sessions/<id>/session.db layout");
            reindex_model_context_for_test(&db, root, session_id).await;
        }
        let _ = db
            .model_context_events()
            .await
            .expect("warm projected context");
        let mut projected_samples_ms = Vec::with_capacity(20);
        let mut projected = Vec::new();
        for _ in 0..20 {
            let projected_started_at = std::time::Instant::now();
            projected = db.model_context_events().await.expect("projected context");
            projected_samples_ms.push(projected_started_at.elapsed().as_millis());
        }
        projected_samples_ms.sort_unstable();
        let projected_p95_ms = projected_samples_ms[18];
        eprintln!(
            "model_context_events: events={}, compatibility_elapsed_ms={}, projected_samples_ms={:?}, projected_p95_ms={}",
            projected.len(),
            compatibility_elapsed_ms,
            projected_samples_ms,
            projected_p95_ms
        );
        assert_eq!(projected, compatibility);
    }

    #[tokio::test]
    async fn reopening_legacy_database_keeps_projection_missing_until_explicit_reindex() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        {
            let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
                .await
                .expect("open session db");
            insert_event(
                db.database(),
                &event(
                    session_id,
                    0,
                    SessionEventKind::UserMessage {
                        client_id: ClientId::new(),
                        text: "legacy".to_string(),
                        admission: bcode_session_models::TurnAdmissionMetadata::default(),
                    },
                ),
                None,
            )
            .await
            .expect("insert legacy canonical event");
        }

        let reopened = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("reopen migrated session db");
        assert_eq!(
            reopened
                .model_context_projection_status()
                .await
                .expect("projection status"),
            ModelContextProjectionStatus::Missing
        );
        let context = reopened
            .model_context_events()
            .await
            .expect("exact compatibility context");
        assert_eq!(context.len(), 1);
        assert_eq!(context[0].sequence, 0);
    }

    #[tokio::test]
    async fn projected_context_matches_canonical_across_compaction_transitions() {
        let scenarios = [
            (
                "uncompacted",
                vec![
                    message(SessionId::new(), 0, "first"),
                    message(SessionId::new(), 1, "second"),
                ],
            ),
            (
                "local-to-local",
                vec![
                    message(SessionId::new(), 0, "old"),
                    local_marker(SessionId::new(), 1, 0),
                    message(SessionId::new(), 2, "middle"),
                    local_marker(SessionId::new(), 3, 2),
                    message(SessionId::new(), 4, "new"),
                ],
            ),
            (
                "local-to-provider",
                vec![
                    message(SessionId::new(), 0, "old"),
                    local_marker(SessionId::new(), 1, 0),
                    message(SessionId::new(), 2, "middle"),
                    provider_marker(SessionId::new(), 3, 2),
                    message(SessionId::new(), 4, "new"),
                ],
            ),
            (
                "provider-to-local",
                vec![
                    message(SessionId::new(), 0, "old"),
                    provider_marker(SessionId::new(), 1, 0),
                    message(SessionId::new(), 2, "middle"),
                    local_marker(SessionId::new(), 3, 2),
                    message(SessionId::new(), 4, "new"),
                ],
            ),
            (
                "provider-to-provider",
                vec![
                    message(SessionId::new(), 0, "old"),
                    provider_marker(SessionId::new(), 1, 0),
                    message(SessionId::new(), 2, "middle"),
                    provider_marker(SessionId::new(), 3, 2),
                    message(SessionId::new(), 4, "new"),
                ],
            ),
        ];

        for (name, template) in scenarios {
            let temp_dir = tempfile::tempdir().expect("temp dir");
            let session_id = SessionId::new();
            let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
                .await
                .expect("open session db");
            let events = template
                .into_iter()
                .map(|mut event| {
                    event.session_id = session_id;
                    event
                })
                .collect::<Vec<_>>();
            for event in &events {
                insert_event(db.database(), event, None)
                    .await
                    .unwrap_or_else(|error| panic!("insert {name} canonical event: {error}"));
            }
            let compatibility = db
                .model_context_events()
                .await
                .unwrap_or_else(|error| panic!("load {name} compatibility context: {error}"));
            reindex_model_context_for_test(&db, temp_dir.path(), session_id).await;
            let projected = db
                .model_context_events()
                .await
                .unwrap_or_else(|error| panic!("load {name} projected context: {error}"));
            assert_eq!(projected, compatibility, "scenario {name}");
        }
    }

    #[tokio::test]
    async fn reindex_rejects_canonical_sequence_gaps_without_replacing_projection() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");
        db.append_event(&event(
            session_id,
            0,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "projected".to_string(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        ))
        .await
        .expect("append projected event");
        insert_event(
            db.database(),
            &event(
                session_id,
                2,
                SessionEventKind::AssistantMessage {
                    text: "gap".to_string(),
                },
            ),
            None,
        )
        .await
        .expect("insert gapped canonical event");

        assert!(matches!(
            {
                let maintenance =
                    crate::lease::acquire_session_maintenance_guard(temp_dir.path(), session_id)
                        .expect("maintenance guard");
                let write = crate::lease::acquire_maintenance_session_write_lock(
                    &maintenance,
                    temp_dir.path(),
                    session_id,
                )
                .expect("write guard");
                db.reindex_model_context(&maintenance, &write)
                    .await
                    .expect_err("gap must reject reindex")
            },
            SessionDbError::InvalidCanonicalSequence {
                expected: 1,
                actual: 2
            }
        ));
        let rows = db
            .database()
            .select("model_context_entries")
            .columns(&["event_seq"])
            .execute(db.database())
            .await
            .expect("existing projection entries");
        assert_eq!(rows.len(), 1);
        assert_eq!(required_i64(&rows[0], "event_seq").expect("event seq"), 0);
    }

    #[tokio::test]
    async fn explicit_reindex_builds_missing_projection_from_canonical_history() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");
        for (sequence, kind) in [
            (
                0,
                SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: "old".to_string(),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            ),
            (
                1,
                SessionEventKind::ContextCompacted {
                    summary: "summary".to_string(),
                    compacted_through_sequence: 0,
                },
            ),
            (
                2,
                SessionEventKind::AssistantMessage {
                    text: "new".to_string(),
                },
            ),
        ] {
            insert_event(db.database(), &event(session_id, sequence, kind), None)
                .await
                .expect("insert canonical event");
        }
        assert_eq!(
            db.model_context_projection_status()
                .await
                .expect("missing status"),
            ModelContextProjectionStatus::Missing
        );
        let compatibility = db
            .model_context_events()
            .await
            .expect("compatibility context");

        assert_eq!(
            reindex_model_context_for_test(&db, temp_dir.path(), session_id).await,
            3
        );
        assert_eq!(
            db.model_context_projection_status()
                .await
                .expect("fresh status"),
            ModelContextProjectionStatus::Fresh { checkpoint: 2 }
        );
        assert_eq!(
            db.model_context_events().await.expect("projected context"),
            compatibility
        );
    }

    #[tokio::test]
    async fn fresh_model_context_projection_tracks_semantic_entries_and_compaction() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");

        for (sequence, kind) in [
            (
                0,
                SessionEventKind::SessionCreated {
                    name: Some("projection".to_string()),
                    working_directory: temp_dir.path().to_path_buf(),
                },
            ),
            (
                1,
                SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: "old".to_string(),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            ),
            (
                2,
                SessionEventKind::AssistantMessage {
                    text: "old response".to_string(),
                },
            ),
            (
                3,
                SessionEventKind::ContextCompacted {
                    summary: "summary".to_string(),
                    compacted_through_sequence: 2,
                },
            ),
            (
                4,
                SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: "new".to_string(),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            ),
            (
                5,
                SessionEventKind::ModelTurnStarted {
                    turn_id: "turn-1".to_string(),
                },
            ),
            (
                6,
                SessionEventKind::ModelTurnFinished {
                    turn_id: "turn-1".to_string(),
                    outcome: bcode_session_models::ModelTurnOutcome::Completed,
                    message: None,
                },
            ),
        ] {
            db.append_event(&event(session_id, sequence, kind))
                .await
                .expect("append projected event");
        }
        db.append_event(&request_context_event(session_id, 7))
            .await
            .expect("append excluded occupancy event");

        let state = db
            .database()
            .select("model_context_projection_state")
            .columns(&["schema_version", "last_event_seq"])
            .execute_first(db.database())
            .await
            .expect("projection state")
            .expect("fresh projection state");
        assert_eq!(
            required_i64(&state, "schema_version").expect("version"),
            i64::from(MODEL_CONTEXT_PROJECTION_SCHEMA_VERSION)
        );
        assert_eq!(required_i64(&state, "last_event_seq").expect("last"), 7);

        let context = db.model_context_events().await.expect("projected context");
        assert_eq!(context.len(), 4);
        assert_eq!(context[0].sequence, 3);
        assert_eq!(context[1].sequence, 4);
        assert!(matches!(
            context[2].kind,
            SessionEventKind::ModelTurnStarted { .. }
        ));
        assert!(matches!(
            context[3].kind,
            SessionEventKind::ModelTurnFinished { .. }
        ));

        db.database()
            .delete("model_context_projection_state")
            .execute(db.database())
            .await
            .expect("remove projection state for compatibility comparison");
        let compatibility = db
            .model_context_events()
            .await
            .expect("compatibility context");
        assert_eq!(context, compatibility);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn mixed_version_incident_is_blocked_before_canonical_divergence() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("initialize session db");
        db.append_event(&event(
            session_id,
            0,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "legacy history through N".to_string(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        ))
        .await
        .expect("append history through N");
        db.database()
            .update("session_storage_contract")
            .value(
                "writer_epoch",
                DatabaseValue::Int64(i64::from(LEGACY_SESSION_STORAGE_WRITER_EPOCH)),
            )
            .where_eq("contract_id", SESSION_STORAGE_CONTRACT_ID)
            .execute(db.database())
            .await
            .expect("set legacy writer epoch");
        db.database()
            .update("model_context_projection_state")
            .value("schema_version", DatabaseValue::Int64(1))
            .execute(db.database())
            .await
            .expect("set legacy model-context schema");
        drop(db);

        let maintenance =
            crate::lease::acquire_session_maintenance_guard(temp_dir.path(), session_id)
                .expect("maintenance guard");
        let write = crate::lease::acquire_maintenance_session_write_lock(
            &maintenance,
            temp_dir.path(),
            session_id,
        )
        .expect("write guard");
        let migrated =
            SessionDb::migrate_turso_in_root(session_id, temp_dir.path(), &maintenance, &write)
                .await
                .expect("exclusive storage migration");
        assert_eq!(
            migrated.storage_writer_epoch().await.expect("writer epoch"),
            u64::from(CURRENT_SESSION_STORAGE_WRITER_EPOCH)
        );
        assert_eq!(
            migrated
                .model_context_projection_status()
                .await
                .expect("model-context status"),
            ModelContextProjectionStatus::Fresh { checkpoint: 0 }
        );
        drop(write);
        drop(maintenance);

        let rejected = event(
            session_id,
            1,
            SessionEventKind::AssistantMessage {
                text: "incompatible writer must not commit".to_string(),
            },
        );
        let error = migrated
            .append_event_for_writer_epoch(&rejected, None, LEGACY_SESSION_STORAGE_WRITER_EPOCH)
            .await
            .expect_err("legacy writer must be fenced before canonical insert");
        assert!(matches!(
            error,
            SessionDbError::WriterIncompatible {
                actual: Some(actual),
                expected
            } if actual == u64::from(CURRENT_SESSION_STORAGE_WRITER_EPOCH)
                && expected == u64::from(LEGACY_SESSION_STORAGE_WRITER_EPOCH)
        ));
        assert_eq!(
            migrated
                .last_event_sequence()
                .await
                .expect("canonical tail"),
            Some(0)
        );
        for projection in MaterializedProjection::all() {
            assert_eq!(
                migrated
                    .materialized_projection_checkpoint(*projection)
                    .await
                    .expect("projection checkpoint"),
                Some(0),
                "projection {} must remain fresh at N",
                projection.as_str()
            );
        }
        assert_eq!(
            migrated
                .model_context_events()
                .await
                .expect("model context")
                .len(),
            1
        );

        migrated
            .append_event(&event(
                session_id,
                1,
                SessionEventKind::AssistantMessage {
                    text: "compatible append".to_string(),
                },
            ))
            .await
            .expect("compatible writer should append N+1");
        assert_eq!(
            migrated
                .last_event_sequence()
                .await
                .expect("canonical tail"),
            Some(1)
        );
        for projection in MaterializedProjection::all() {
            assert_eq!(
                migrated
                    .materialized_projection_checkpoint(*projection)
                    .await
                    .expect("projection checkpoint"),
                Some(1),
                "projection {} must advance atomically to N+1",
                projection.as_str()
            );
        }
        assert_eq!(
            migrated
                .model_context_projection_status()
                .await
                .expect("model-context status"),
            ModelContextProjectionStatus::Fresh { checkpoint: 1 }
        );
        assert_eq!(
            migrated
                .model_context_events()
                .await
                .expect("model context")
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn incompatible_storage_writer_contract_rejects_mutations_after_reopen() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");
        db.append_event(&event(
            session_id,
            0,
            SessionEventKind::SessionCreated {
                name: Some("writer contract".to_string()),
                working_directory: temp_dir.path().to_path_buf(),
            },
        ))
        .await
        .expect("append initial event");
        let future_epoch = u64::from(CURRENT_SESSION_STORAGE_WRITER_EPOCH).saturating_add(1);
        db.database()
            .update("session_storage_contract")
            .value("writer_epoch", seq_to_value(future_epoch))
            .where_eq("contract_id", SESSION_STORAGE_CONTRACT_ID)
            .execute(db.database())
            .await
            .expect("set future writer epoch");
        drop(db);

        let reopened = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("runtime open may inspect incompatible contract");
        assert_eq!(
            reopened.storage_writer_epoch().await.expect("writer epoch"),
            future_epoch
        );
        let error = reopened
            .append_event(&event(
                session_id,
                1,
                SessionEventKind::AssistantMessage {
                    text: "must not commit".to_string(),
                },
            ))
            .await
            .expect_err("future writer contract must reject append");
        assert!(matches!(
            error,
            SessionDbError::WriterIncompatible {
                actual: Some(actual),
                expected
            } if actual == future_epoch
                && expected == u64::from(CURRENT_SESSION_STORAGE_WRITER_EPOCH)
        ));
        assert!(matches!(
            reopened.set_session_composer_draft("blocked", 1).await,
            Err(SessionDbError::WriterIncompatible { .. })
        ));
        assert_eq!(
            reopened
                .last_event_sequence()
                .await
                .expect("canonical tail"),
            Some(0)
        );
        assert_eq!(
            reopened.session_composer_draft().await.expect("draft read"),
            None
        );
    }

    #[tokio::test]
    async fn missing_storage_writer_contract_fails_closed_for_mutation() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");
        db.database()
            .delete("session_storage_contract")
            .execute(db.database())
            .await
            .expect("remove writer contract");

        let error = db
            .append_event(&event(
                session_id,
                0,
                SessionEventKind::SessionCreated {
                    name: Some("missing contract".to_string()),
                    working_directory: temp_dir.path().to_path_buf(),
                },
            ))
            .await
            .expect_err("missing contract must reject append");
        assert!(matches!(
            error,
            SessionDbError::WriterIncompatible {
                actual: Some(actual),
                expected
            } if actual == u64::from(LEGACY_SESSION_STORAGE_WRITER_EPOCH)
                && expected == u64::from(CURRENT_SESSION_STORAGE_WRITER_EPOCH)
        ));
        assert_eq!(
            db.storage_writer_epoch()
                .await
                .expect("legacy writer epoch"),
            u64::from(LEGACY_SESSION_STORAGE_WRITER_EPOCH)
        );
        assert_eq!(
            db.last_event_sequence().await.expect("canonical tail"),
            None
        );
    }

    #[tokio::test]
    async fn stale_model_context_projection_rejects_append_atomically() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");
        db.append_event(&event(
            session_id,
            0,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "projected".to_string(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        ))
        .await
        .expect("append projected event");
        db.database()
            .update("model_context_projection_state")
            .value("last_event_seq", seq_to_value(1))
            .execute(db.database())
            .await
            .expect("make model context checkpoint invalid");

        let error = db
            .append_event(&event(
                session_id,
                1,
                SessionEventKind::AssistantMessage {
                    text: "must roll back".to_string(),
                },
            ))
            .await
            .expect_err("stale projection must reject append");
        assert!(matches!(
            error,
            SessionDbError::ModelContextProjectionStale {
                checkpoint: 1,
                expected: 0
            }
        ));
        assert_eq!(
            db.last_event_sequence().await.expect("canonical tail"),
            Some(0)
        );
        for projection in MaterializedProjection::all() {
            assert_eq!(
                db.materialized_projection_checkpoint(*projection)
                    .await
                    .expect("projection checkpoint"),
                Some(0)
            );
        }
    }

    #[tokio::test]
    async fn incompatible_context_occupancy_rejects_append_atomically() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");
        db.append_event(&event(
            session_id,
            0,
            SessionEventKind::SessionCreated {
                name: Some("projection".to_string()),
                working_directory: temp_dir.path().to_path_buf(),
            },
        ))
        .await
        .expect("append initial event");
        db.database()
            .update("context_occupancy_projection")
            .value("schema_version", DatabaseValue::Int64(3))
            .execute(db.database())
            .await
            .expect("make occupancy schema incompatible");

        let error = db
            .append_event(&event(
                session_id,
                1,
                SessionEventKind::AssistantMessage {
                    text: "must roll back".to_string(),
                },
            ))
            .await
            .expect_err("incompatible projection must reject append");
        assert!(matches!(
            error,
            SessionDbError::ProjectionIncompatible {
                projection: "context_occupancy",
                actual: 3,
                expected: 4
            }
        ));
        assert_eq!(
            db.last_event_sequence().await.expect("canonical tail"),
            Some(0)
        );
    }

    #[tokio::test]
    async fn duplicate_turn_receipt_rolls_back_canonical_and_projection_updates() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");
        let admission = bcode_session_models::TurnAdmissionMetadata {
            origin: Some(bcode_session_models::TurnOrigin {
                producer: "test.producer".to_string(),
                correlation_id: None,
                display_label: None,
            }),
            idempotency_key: Some("same-key".to_string()),
            ..bcode_session_models::TurnAdmissionMetadata::default()
        };
        db.append_event(&event(
            session_id,
            0,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "accepted".to_string(),
                admission: admission.clone(),
            },
        ))
        .await
        .expect("append accepted turn");

        db.append_event(&event(
            session_id,
            1,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "duplicate".to_string(),
                admission,
            },
        ))
        .await
        .expect_err("duplicate receipt must roll back append");
        assert_eq!(
            db.last_event_sequence().await.expect("canonical tail"),
            Some(0)
        );
        assert_eq!(db.input_history().await.expect("input history").len(), 1);
        for projection in MaterializedProjection::all() {
            assert_eq!(
                db.materialized_projection_checkpoint(*projection)
                    .await
                    .expect("projection checkpoint"),
                Some(0)
            );
        }
    }

    #[tokio::test]
    async fn invalid_compaction_does_not_advance_canonical_or_projected_state() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");
        db.append_event(&event(
            session_id,
            0,
            SessionEventKind::SessionCreated {
                name: Some("projection".to_string()),
                working_directory: temp_dir.path().to_path_buf(),
            },
        ))
        .await
        .expect("append projected event");
        let error = db
            .append_event(&event(
                session_id,
                1,
                SessionEventKind::ContextCompacted {
                    summary: "invalid".to_string(),
                    compacted_through_sequence: 2,
                },
            ))
            .await
            .expect_err("invalid compaction must roll back");
        assert!(matches!(
            error,
            SessionDbError::InvalidCompactionMarker { sequence: 1, .. }
        ));
        assert_eq!(db.last_event_sequence().await.expect("last"), Some(0));
        let state = db
            .database()
            .select("model_context_projection_state")
            .columns(&["last_event_seq"])
            .execute_first(db.database())
            .await
            .expect("projection state")
            .expect("projection state row");
        assert_eq!(required_i64(&state, "last_event_seq").expect("last"), 0);
    }

    #[tokio::test]
    async fn existing_open_and_reads_leave_database_and_sidecars_byte_identical() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("initialize session db");
        db.append_event(&event(
            session_id,
            0,
            SessionEventKind::SessionCreated {
                name: Some("read immutability".to_string()),
                working_directory: temp_dir.path().to_path_buf(),
            },
        ))
        .await
        .expect("append event");
        drop(db);
        let before = session_storage_files(temp_dir.path(), session_id);

        let reopened = SessionDb::open_existing_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open existing session");
        assert_eq!(reopened.last_event_sequence().await.expect("tail"), Some(0));
        assert_eq!(reopened.all_events().await.expect("history").len(), 1);
        assert_eq!(
            reopened
                .model_context_projection_status()
                .await
                .expect("projection status"),
            ModelContextProjectionStatus::Fresh { checkpoint: 0 }
        );
        drop(reopened);

        let after = session_storage_files(temp_dir.path(), session_id);
        assert_eq!(
            after, before,
            "existing open/read paths must not mutate DB or sidecars"
        );
    }

    #[tokio::test]
    async fn normal_existing_open_does_not_run_pending_projection_migrations() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("initialize session db");
        db.append_event(&event(
            session_id,
            0,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "projected".to_string(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        ))
        .await
        .expect("append projected event");
        db.database()
            .update("model_context_projection_state")
            .value("schema_version", DatabaseValue::Int64(1))
            .execute(db.database())
            .await
            .expect("set legacy projection version");
        drop(db);

        let reopened = SessionDb::open_existing_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open existing session without migration");
        assert_eq!(
            reopened
                .model_context_projection_status()
                .await
                .expect("projection status"),
            ModelContextProjectionStatus::Incompatible {
                actual: 1,
                expected: 2,
            }
        );
        let state = reopened
            .database()
            .select("model_context_projection_state")
            .columns(&["schema_version", "last_event_seq"])
            .where_eq("projection_id", MODEL_CONTEXT_PROJECTION_ID)
            .execute_first(reopened.database())
            .await
            .expect("projection query")
            .expect("projection row");
        assert_eq!(required_i64(&state, "schema_version").expect("schema"), 1);
        assert_eq!(required_i64(&state, "last_event_seq").expect("tail"), 0);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn explicit_legacy_model_context_migration_is_atomic() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");
        db.append_event(&event(
            session_id,
            0,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "projected".to_string(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        ))
        .await
        .expect("append projected event");
        db.database()
            .update("session_storage_contract")
            .value(
                "writer_epoch",
                DatabaseValue::Int64(i64::from(LEGACY_SESSION_STORAGE_WRITER_EPOCH)),
            )
            .where_eq("contract_id", SESSION_STORAGE_CONTRACT_ID)
            .execute(db.database())
            .await
            .expect("set legacy writer epoch");
        db.database()
            .update("model_context_projection_state")
            .value("schema_version", DatabaseValue::Int64(1))
            .execute(db.database())
            .await
            .expect("set legacy version");
        insert_event(
            db.database(),
            &event(
                session_id,
                1,
                SessionEventKind::AssistantMessage {
                    text: "canonical tail".to_string(),
                },
            ),
            None,
        )
        .await
        .expect("append canonical tail without model-context projection");
        let tail_event = event(
            session_id,
            1,
            SessionEventKind::AssistantMessage {
                text: "canonical tail".to_string(),
            },
        );
        project_materialized_event(db.database(), &tail_event)
            .await
            .expect("project non-model read models");
        project_context_occupancy_event(db.database(), &tail_event)
            .await
            .expect("project occupancy read model");
        drop(db);

        let maintenance =
            crate::lease::acquire_session_maintenance_guard(temp_dir.path(), session_id)
                .expect("maintenance guard");
        let write = crate::lease::acquire_maintenance_session_write_lock(
            &maintenance,
            temp_dir.path(),
            session_id,
        )
        .expect("write guard");
        let migrated =
            SessionDb::migrate_turso_in_root(session_id, temp_dir.path(), &maintenance, &write)
                .await
                .expect("explicitly migrate session db");
        assert_eq!(
            migrated
                .storage_writer_epoch()
                .await
                .expect("migrated writer epoch"),
            u64::from(CURRENT_SESSION_STORAGE_WRITER_EPOCH)
        );
        assert_eq!(
            migrated
                .model_context_projection_status()
                .await
                .expect("projection status"),
            ModelContextProjectionStatus::Fresh { checkpoint: 1 }
        );
        let context = migrated
            .model_context_events()
            .await
            .expect("migrated context");
        assert_eq!(context.len(), 2);
        assert_eq!(context[1].sequence, 1);

        drop(migrated);
        let reopened = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("reopen migrated session db");
        assert_eq!(
            reopened
                .model_context_projection_status()
                .await
                .expect("projection status after reopen"),
            ModelContextProjectionStatus::Fresh { checkpoint: 1 }
        );
    }

    #[tokio::test]
    async fn explicit_migration_refuses_malformed_canonical_history_without_mutation() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");
        db.append_event(&event(
            session_id,
            0,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "canonical".to_string(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        ))
        .await
        .expect("append canonical event");
        db.database()
            .update("session_storage_contract")
            .value(
                "writer_epoch",
                DatabaseValue::Int64(i64::from(LEGACY_SESSION_STORAGE_WRITER_EPOCH)),
            )
            .where_eq("contract_id", SESSION_STORAGE_CONTRACT_ID)
            .execute(db.database())
            .await
            .expect("set legacy writer epoch");
        db.database()
            .update("model_context_projection_state")
            .value("schema_version", DatabaseValue::Int64(1))
            .execute(db.database())
            .await
            .expect("set legacy projection schema");
        db.database()
            .update("events")
            .value("payload", "{not-valid-json")
            .where_eq("event_seq", seq_to_value(0))
            .execute(db.database())
            .await
            .expect("corrupt canonical payload");
        drop(db);

        let maintenance =
            crate::lease::acquire_session_maintenance_guard(temp_dir.path(), session_id)
                .expect("maintenance guard");
        let write = crate::lease::acquire_maintenance_session_write_lock(
            &maintenance,
            temp_dir.path(),
            session_id,
        )
        .expect("write guard");
        SessionDb::migrate_turso_in_root(session_id, temp_dir.path(), &maintenance, &write)
            .await
            .expect_err("malformed canonical history must reject migration");
        drop(write);
        drop(maintenance);

        let reopened = SessionDb::open_existing_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("reopen rejected migration");
        assert_eq!(
            reopened.storage_writer_epoch().await.expect("writer epoch"),
            u64::from(LEGACY_SESSION_STORAGE_WRITER_EPOCH)
        );
        assert!(matches!(
            reopened
                .model_context_projection_status()
                .await
                .expect("projection status"),
            ModelContextProjectionStatus::Incompatible { actual: 1, .. }
        ));
        reopened
            .all_events_strict()
            .await
            .expect_err("malformed canonical payload must remain visible");
    }

    #[tokio::test]
    async fn failed_explicit_migration_preserves_projection_and_writer_contract() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");
        db.append_event(&event(
            session_id,
            0,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "projected".to_string(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        ))
        .await
        .expect("append projected event");
        db.database()
            .update("session_storage_contract")
            .value(
                "writer_epoch",
                DatabaseValue::Int64(i64::from(LEGACY_SESSION_STORAGE_WRITER_EPOCH)),
            )
            .where_eq("contract_id", SESSION_STORAGE_CONTRACT_ID)
            .execute(db.database())
            .await
            .expect("set legacy writer epoch");
        db.database()
            .update("model_context_projection_state")
            .value("schema_version", DatabaseValue::Int64(1))
            .execute(db.database())
            .await
            .expect("set legacy projection schema");
        insert_event(
            db.database(),
            &event(
                session_id,
                2,
                SessionEventKind::AssistantMessage {
                    text: "gapped tail".to_string(),
                },
            ),
            None,
        )
        .await
        .expect("insert gapped canonical tail");
        drop(db);

        let maintenance =
            crate::lease::acquire_session_maintenance_guard(temp_dir.path(), session_id)
                .expect("maintenance guard");
        let write = crate::lease::acquire_maintenance_session_write_lock(
            &maintenance,
            temp_dir.path(),
            session_id,
        )
        .expect("write guard");
        assert!(matches!(
            SessionDb::migrate_turso_in_root(session_id, temp_dir.path(), &maintenance, &write)
                .await
                .expect_err("gapped migration must fail"),
            SessionDbError::InvalidCanonicalSequence {
                expected: 1,
                actual: 2
            }
        ));

        let reopened = SessionDb::open_existing_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("reopen failed migration");
        assert_eq!(
            reopened.storage_writer_epoch().await.expect("writer epoch"),
            u64::from(LEGACY_SESSION_STORAGE_WRITER_EPOCH)
        );
        let state = reopened
            .database()
            .select("model_context_projection_state")
            .columns(&["schema_version", "last_event_seq"])
            .where_eq("projection_id", MODEL_CONTEXT_PROJECTION_ID)
            .execute_first(reopened.database())
            .await
            .expect("projection query")
            .expect("projection state");
        assert_eq!(required_i64(&state, "schema_version").expect("schema"), 1);
        assert_eq!(required_i64(&state, "last_event_seq").expect("tail"), 0);
    }

    #[tokio::test]
    async fn incompatible_or_corrupt_model_context_projection_is_visible() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");
        db.append_event(&event(
            session_id,
            0,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "projected".to_string(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        ))
        .await
        .expect("append projected event");

        db.database()
            .update("model_context_projection_state")
            .value("schema_version", DatabaseValue::Int64(1))
            .execute(db.database())
            .await
            .expect("set incompatible version");
        assert!(matches!(
            db.model_context_events()
                .await
                .expect_err("incompatible version must surface"),
            SessionDbError::ModelContextProjectionVersion {
                actual: 1,
                expected: 2,
            }
        ));

        db.database()
            .update("model_context_projection_state")
            .value(
                "schema_version",
                DatabaseValue::Int64(i64::from(MODEL_CONTEXT_PROJECTION_SCHEMA_VERSION)),
            )
            .execute(db.database())
            .await
            .expect("restore version");
        db.database()
            .update("model_context_entries")
            .value("event_type", "assistant_message")
            .where_eq("event_seq", seq_to_value(0))
            .execute(db.database())
            .await
            .expect("corrupt entry identity");
        assert!(matches!(
            db.model_context_events()
                .await
                .expect_err("corrupt entry must surface"),
            SessionDbError::InvalidRow { .. }
        ));
    }

    #[tokio::test]
    async fn stale_model_context_projection_is_visible_and_never_falls_back() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");
        db.append_event(&event(
            session_id,
            0,
            SessionEventKind::SessionCreated {
                name: Some("projection".to_string()),
                working_directory: temp_dir.path().to_path_buf(),
            },
        ))
        .await
        .expect("append projected event");
        insert_event(
            db.database(),
            &event(
                session_id,
                1,
                SessionEventKind::UserMessage {
                    client_id: ClientId::new(),
                    text: "unprojected tail".to_string(),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            ),
            None,
        )
        .await
        .expect("append canonical tail only");

        let error = db
            .model_context_events()
            .await
            .expect_err("stale projection must be visible");
        assert!(matches!(
            error,
            SessionDbError::ModelContextProjectionStale {
                checkpoint: 0,
                expected: 1
            }
        ));
    }

    #[tokio::test]
    async fn uncompacted_model_context_is_not_limited_by_event_count_or_irrelevant_history() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");
        let tx = db.database().begin_transaction().await.expect("begin");

        for sequence in 0..513 {
            insert_event(
                &*tx,
                &event(
                    session_id,
                    sequence,
                    SessionEventKind::UserMessage {
                        client_id: ClientId::new(),
                        text: format!("semantic-{sequence}"),
                        admission: bcode_session_models::TurnAdmissionMetadata::default(),
                    },
                ),
                None,
            )
            .await
            .expect("insert semantic event");
        }
        for sequence in 513..8_706 {
            insert_event(
                &*tx,
                &event(
                    session_id,
                    sequence,
                    SessionEventKind::SessionCreated {
                        name: Some(format!("irrelevant-{sequence}")),
                        working_directory: temp_dir.path().to_path_buf(),
                    },
                ),
                None,
            )
            .await
            .expect("insert irrelevant event");
        }
        insert_event(
            &*tx,
            &event(
                session_id,
                8_706,
                SessionEventKind::AssistantMessage {
                    text: "newest semantic event".to_string(),
                },
            ),
            None,
        )
        .await
        .expect("insert newest semantic event");
        tx.commit().await.expect("commit");

        let events = db.model_context_events().await.expect("model context");
        assert_eq!(events.len(), 514);
        assert_eq!(events.first().map(|event| event.sequence), Some(0));
        assert_eq!(events.last().map(|event| event.sequence), Some(8_706));
    }

    #[tokio::test]
    async fn context_occupancy_projection_reconciles_requests_and_boundaries() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");

        db.append_event(&event(
            session_id,
            0,
            SessionEventKind::ModelChanged {
                provider: "provider".to_string(),
                model: "model".to_string(),
            },
        ))
        .await
        .expect("append model boundary");
        db.append_event(&request_context_event(session_id, 1))
            .await
            .expect("append estimate");
        let occupancy = db
            .current_context_occupancy()
            .await
            .expect("occupancy")
            .expect("estimate should project");
        assert_eq!(occupancy.observation_sequence, 1);

        db.append_event(&event(
            session_id,
            2,
            SessionEventKind::ContextCompacted {
                summary: "summary".to_string(),
                compacted_through_sequence: 1,
            },
        ))
        .await
        .expect("append compaction boundary");
        assert_eq!(
            db.current_context_occupancy().await.expect("occupancy"),
            None
        );
    }

    #[tokio::test]
    async fn explicit_schema_v3_context_occupancy_migration_resets_incompatible_derived_state() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");
        db.append_event(&event(
            session_id,
            0,
            SessionEventKind::ModelChanged {
                provider: "provider".to_string(),
                model: "model".to_string(),
            },
        ))
        .await
        .expect("append model boundary");
        db.database()
            .update("context_occupancy_projection")
            .value("schema_version", DatabaseValue::Int32(3))
            .value(
                "occupancy_json",
                r#"{"context_epoch":0,"observation_sequence":1,"snapshot":{"invocation":{},"context_through_sequence":0}}"#,
            )
            .where_eq("projection_id", CONTEXT_OCCUPANCY_PROJECTION_ID)
            .execute(db.database())
            .await
            .expect("install schema v3 occupancy");
        db.database()
            .delete(SESSION_MIGRATIONS_TABLE)
            .where_eq("id", "024_reset_request_context_occupancy_projection")
            .execute(db.database())
            .await
            .expect("mark compatibility migration pending");
        drop(db);

        let maintenance =
            crate::lease::acquire_session_maintenance_guard(temp_dir.path(), session_id)
                .expect("maintenance guard");
        let write = crate::lease::acquire_maintenance_session_write_lock(
            &maintenance,
            temp_dir.path(),
            session_id,
        )
        .expect("write guard");
        let db =
            SessionDb::migrate_turso_in_root(session_id, temp_dir.path(), &maintenance, &write)
                .await
                .expect("explicitly migrate legacy session db");
        assert_eq!(db.current_context_epoch().await.expect("context epoch"), 0);
        assert_eq!(
            db.current_context_occupancy().await.expect("occupancy"),
            None
        );

        db.append_event(&request_context_event(session_id, 1))
            .await
            .expect("append current usage");
        assert!(
            db.current_context_occupancy()
                .await
                .expect("current occupancy")
                .is_some()
        );
    }

    #[tokio::test]
    async fn malformed_compaction_marker_surfaces_an_error_without_mutation() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");
        db.database()
            .insert("events")
            .value("event_seq", seq_to_value(0))
            .value("event_type", "context_compacted")
            .value(
                "schema_version",
                DatabaseValue::Int32(i32::from(CURRENT_SESSION_EVENT_SCHEMA_VERSION)),
            )
            .value("created_at_ms", seq_to_value(1))
            .value("payload", "{not valid json")
            .execute(db.database())
            .await
            .expect("insert malformed marker");

        let error = db
            .model_context_events()
            .await
            .expect_err("malformed marker must not be ignored");
        assert!(matches!(error, SessionDbError::PersistedEvent(_)));
        assert_eq!(
            db.last_event_sequence().await.expect("last sequence"),
            Some(0)
        );
    }

    #[tokio::test]
    async fn model_context_events_start_at_latest_compaction() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");

        db.append_event(&event(
            session_id,
            0,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "old".to_string(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        ))
        .await
        .expect("append old event");
        db.append_event(&event(
            session_id,
            1,
            SessionEventKind::ContextCompacted {
                summary: "summary".to_string(),
                compacted_through_sequence: 1,
            },
        ))
        .await
        .expect("append compaction");
        db.append_event(&event(
            session_id,
            2,
            SessionEventKind::UserMessage {
                client_id: ClientId::new(),
                text: "new".to_string(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        ))
        .await
        .expect("append new event");

        let events = db.model_context_events().await.expect("model context");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].sequence, 1);
        assert_eq!(events[1].sequence, 2);
    }

    #[tokio::test]
    async fn transcript_window_query_uses_bounded_latest_projection_rows() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");

        for sequence in 0..5 {
            db.append_event(&event(
                session_id,
                sequence,
                SessionEventKind::AssistantMessage {
                    text: format!("message {sequence}"),
                },
            ))
            .await
            .expect("append event");
        }

        let items = db
            .transcript_items_for_latest_window(2, 3, usize::MAX)
            .await
            .expect("transcript window items");
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].event_seq_start, 2);
        assert_eq!(items[1].event_seq_start, 3);
        assert_eq!(items[2].event_seq_start, 4);
        assert_eq!(
            db.first_event_sequence().await.expect("first sequence"),
            Some(0)
        );
        assert_eq!(
            db.last_event_sequence().await.expect("last sequence"),
            Some(4)
        );
    }

    #[tokio::test]
    async fn transcript_window_tool_result_spans_raw_pty_stream_events() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");

        db.append_event(&event(
            session_id,
            0,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "shell-1".to_owned(),
                producer_plugin_id: Some("bcode.shell".to_owned()),
                tool_name: "shell.run".to_owned(),
                arguments_json: r#"{"command":"printf hi"}"#.to_owned(),
                working_directory: None,
                request_visual: None,
                legacy_request_presentation: None,
            },
        ))
        .await
        .expect("append request");
        db.append_event(&event(
            session_id,
            1,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::Started {
                    tool_call_id: "shell-1".to_owned(),
                    tool_name: "shell.run".to_owned(),
                    sequence: 0,
                    terminal: true,
                    columns: Some(80),
                    rows: Some(24),
                    started_at_ms: Some(1),
                },
            },
        ))
        .await
        .expect("append stream start");
        db.append_event(&event(
            session_id,
            2,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::OutputDelta {
                    tool_call_id: "shell-1".to_owned(),
                    stream: bcode_session_models::ToolOutputStream::Pty,
                    sequence: 1,
                    text: "\u{1b}[31mhi\u{1b}[0m\n".to_owned(),
                    byte_len: 12,
                },
            },
        ))
        .await
        .expect("append pty output");
        db.append_event(&event(
            session_id,
            3,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::Finished {
                    tool_call_id: "shell-1".to_owned(),
                    sequence: 2,
                    is_error: false,
                    finished_at_ms: Some(3),
                },
            },
        ))
        .await
        .expect("append stream finish");
        db.append_event(&event(
            session_id,
            4,
            SessionEventKind::ToolCallFinished {
                tool_call_id: "shell-1".to_owned(),
                result: "done".to_owned(),
                is_error: false,
                output: None,
                semantic_result: None,
            },
        ))
        .await
        .expect("append tool finish");

        let items = db
            .transcript_items_for_latest_window(1, 1, usize::MAX)
            .await
            .expect("transcript window items");

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "result");
        assert_eq!(items[0].event_seq_start, 0);
        assert_eq!(items[0].event_seq_end, 4);
        let events = db
            .events_range(items[0].event_seq_start, items[0].event_seq_end, 10)
            .await
            .expect("events range");
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::OutputDelta { stream, .. }
            } if *stream == bcode_session_models::ToolOutputStream::Pty
        )));
    }

    #[tokio::test]
    async fn finalized_artifact_references_are_projected_for_bounded_lookup() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");
        db.append_event(&event(
            session_id,
            0,
            SessionEventKind::SessionCreated {
                name: Some("artifact projection".to_owned()),
                working_directory: std::path::PathBuf::from("/tmp"),
            },
        ))
        .await
        .expect("append session creation");
        db.append_event(&event(
            session_id,
            1,
            SessionEventKind::ToolCallFinished {
                tool_call_id: "call-1".to_owned(),
                result: "done".to_owned(),
                is_error: false,
                output: None,
                semantic_result: Some(ToolInvocationResult::Artifact {
                    artifact: Box::new(bcode_session_models::ToolArtifact {
                        artifact_id: "artifact-1".to_owned(),
                        producer_plugin_id: "fixture.plugin".to_owned(),
                        schema: "fixture.recording".to_owned(),
                        schema_version: 3,
                        tool_call_id: Some("call-1".to_owned()),
                        title: None,
                        metadata: serde_json::Value::Null,
                        refs: vec![bcode_session_models::ToolArtifactRef {
                            key: "recording".to_owned(),
                            content_type: Some("application/octet-stream".to_owned()),
                            storage_uri: Some("file:///tmp/recording".to_owned()),
                            byte_len: Some(42),
                            metadata: Some(serde_json::json!({
                                "availability": "complete",
                                "complete": true,
                                "checksum_sha256": "abc123",
                                "plugin_only": "must not be projected"
                            })),
                        }],
                    }),
                }),
            },
        ))
        .await
        .expect("append finalized artifact");

        let reference = db
            .finalized_artifact_reference("artifact-1", "recording")
            .await
            .expect("projected lookup")
            .expect("reference");
        assert_eq!(reference.producer_plugin_id, "fixture.plugin");
        assert_eq!(reference.schema, "fixture.recording");
        assert_eq!(reference.schema_version, 3);
        assert_eq!(reference.byte_len, Some(42));
        assert_eq!(reference.finalized_event_seq, 1);
        assert_eq!(reference.availability.as_deref(), Some("complete"));
        assert_eq!(reference.complete, Some(true));
        assert_eq!(reference.checksum_sha256.as_deref(), Some("abc123"));
        assert!(
            db.finalized_artifact_reference("artifact-1", "missing")
                .await
                .expect("missing lookup")
                .is_none()
        );
    }

    #[tokio::test]
    async fn corrupt_finalized_artifact_projection_row_is_rejected() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");
        db.append_event(&event(
            session_id,
            0,
            SessionEventKind::SessionCreated {
                name: Some("artifact projection".to_owned()),
                working_directory: std::path::PathBuf::from("/tmp"),
            },
        ))
        .await
        .expect("append session creation");
        db.database()
            .insert("artifact_references")
            .value("artifact_id", "artifact")
            .value("reference_key", "recording")
            .value("producer_plugin_id", "plugin")
            .value("schema", "schema")
            .value(
                "schema_version",
                DatabaseValue::String("invalid".to_owned()),
            )
            .value("finalized_event_seq", seq_to_value(0))
            .execute(db.database())
            .await
            .expect("insert corrupt projection row");

        let error = db
            .finalized_artifact_reference("artifact", "recording")
            .await
            .expect_err("corrupt projection row must be rejected");
        assert!(matches!(error, SessionDbError::InvalidRow { .. }));
    }

    #[tokio::test]
    async fn legacy_session_rejects_append_before_creating_partial_artifact_projection() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");
        db.database()
            .insert("events")
            .value("event_seq", seq_to_value(0))
            .value("event_type", "session_created")
            .value(
                "schema_version",
                DatabaseValue::Int32(i32::from(CURRENT_SESSION_EVENT_SCHEMA_VERSION)),
            )
            .value("created_at_ms", seq_to_value(0))
            .value(
                "payload",
                encode_session_event(&event(
                    session_id,
                    0,
                    SessionEventKind::SessionCreated {
                        name: Some("legacy".to_owned()),
                        working_directory: std::path::PathBuf::from("/tmp"),
                    },
                ))
                .expect("encode legacy event"),
            )
            .execute(db.database())
            .await
            .expect("insert legacy canonical event");
        let error = db
            .append_event(&event(
                session_id,
                1,
                SessionEventKind::AssistantMessage {
                    text: "new tail".to_owned(),
                },
            ))
            .await
            .expect_err("append must reject incomplete legacy projections");
        assert!(matches!(
            error,
            SessionDbError::ProjectionStale {
                projection: "model_context",
                checkpoint: None,
                expected: 0
            }
        ));
        assert_eq!(
            db.last_event_sequence().await.expect("canonical tail"),
            Some(0)
        );
        assert_eq!(
            db.materialized_projection_checkpoint(MaterializedProjection::ArtifactReferences)
                .await
                .expect("artifact checkpoint"),
            None
        );
    }

    #[tokio::test]
    async fn events_range_reads_canonical_db_events() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let session_id = SessionId::new();
        let db = SessionDb::open_turso_in_root(session_id, temp_dir.path())
            .await
            .expect("open session db");

        for sequence in 0..3 {
            db.append_event(&event(
                session_id,
                sequence,
                SessionEventKind::AssistantMessage {
                    text: format!("message {sequence}"),
                },
            ))
            .await
            .expect("append event");
        }

        let events = db.events_range(1, 2, 10).await.expect("events range");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].sequence, 1);
        assert_eq!(events[1].sequence, 2);
    }
}
