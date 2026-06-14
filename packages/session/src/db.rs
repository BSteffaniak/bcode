#![allow(clippy::module_name_repetitions)]

//! Switchy-backed session database primitives.
//!
//! This module is the first CQRS/event-store database slice for Bcode sessions. It
//! intentionally keeps Turso-specific details at connection boundaries and uses
//! `switchy` database traits for migrations and repository operations.

use std::{fs, path::Path, sync::Arc, time::Duration};

use bcode_session_models::{
    RuntimeWorkId, RuntimeWorkKind, RuntimeWorkStatus, SessionEvent, SessionEventKind,
    SessionHistoryCursor, SessionHistoryDirection, SessionHistoryPage, SessionHistoryQuery,
    SessionId, SessionInputHistoryEntry, SessionSummary, SessionTitleSource,
};
use switchy::{
    database::{
        Database, DatabaseError, DatabaseValue,
        query::{FilterableQuery, SortDirection},
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
const MODEL_CONTEXT_EVENT_LIMIT: usize = 512;
const MODEL_CONTEXT_SCAN_PAGE_LIMIT: usize = 512;
const MODEL_CONTEXT_RAW_SCAN_LIMIT: usize = 8_192;

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
    /// A database row did not contain the expected column/type.
    #[error("invalid session database row: missing or invalid column {column}")]
    InvalidRow { column: String },
}

/// Result type for session DB operations.
pub type SessionDbResult<T> = Result<T, SessionDbError>;

/// Incrementally materialized session DB projections that maintain freshness checkpoints.
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
    /// Runtime-work lifecycle rows.
    RuntimeWork,
}

impl MaterializedProjection {
    const ALL: [Self; 5] = [
        Self::SessionState,
        Self::InputHistory,
        Self::Transcript,
        Self::ToolRuns,
        Self::RuntimeWork,
    ];

    /// Return all checkpointed materialized projections.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &Self::ALL
    }

    /// Return the stable projection checkpoint name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionState => "session_state",
            Self::InputHistory => "input_history",
            Self::Transcript => "transcript",
            Self::ToolRuns => "tool_runs",
            Self::RuntimeWork => "runtime_work",
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
    pub work_id: RuntimeWorkId,
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
    pub parent_work_id: Option<RuntimeWorkId>,
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
    /// Projection-updated timestamp, when known.
    pub updated_at_ms: Option<u64>,
    /// Whether the input-history projection has at least one user message.
    pub has_user_message: bool,
    /// Latest context-compaction event sequence, if any.
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
        let path = global_catalog_db_path(root);
        Self::open_turso(&path).await
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
        let path = namespaced_catalog_db_path(root, namespace);
        Self::open_turso(&path).await
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
        let root = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(root)?;
        let catalog_lock = crate::lease::acquire_catalog_lock(root)?;
        let db = init_turso_local_with_retry(path).await?;
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
    /// Open one session database under `root` using Bcode's default per-session layout.
    ///
    /// # Errors
    ///
    /// Returns an error if the session directory cannot be created, the database cannot be
    /// opened, or schema migrations fail.
    pub async fn open_turso_in_root(session_id: SessionId, root: &Path) -> SessionDbResult<Self> {
        let path = session_db_path(root, session_id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Self::open_turso(session_id, &path).await
    }

    /// Open one session database at `path` and apply cheap schema migrations.
    ///
    /// This must not replay events or rebuild projections. Repair/reprojection belongs behind
    /// explicit repair commands.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * the Turso connection cannot be opened after bounded lock retries
    /// * schema migrations fail
    pub async fn open_turso(session_id: SessionId, path: &Path) -> SessionDbResult<Self> {
        let db = init_turso_local_with_retry(path).await?;
        run_session_migrations(&*db).await?;
        Ok(Self {
            session_id,
            db: Arc::new(db),
        })
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
        let row = self
            .db
            .select("events")
            .columns(&["event_seq"])
            .where_eq("event_type", "context_compacted")
            .sort("event_seq", SortDirection::Desc)
            .limit(1)
            .execute_first(&**self.db)
            .await?;
        row.as_ref()
            .map(|row| required_i64(row, "event_seq").map(i64_to_u64))
            .transpose()
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
    pub async fn append_event_with_activity_timestamp(
        &self,
        event: &SessionEvent,
        activity_timestamp_ms: Option<u64>,
    ) -> SessionDbResult<()> {
        let tx = self.db.begin_transaction().await?;
        insert_event(&*tx, event, activity_timestamp_ms).await?;
        project_event(&*tx, event).await?;
        update_projection_checkpoints(&*tx, event).await?;
        tx.commit().await?;
        Ok(())
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
            .columns(&["event_seq", "text"])
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

    /// Return all canonical events in sequence order.
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

        rows.into_iter()
            .map(|row| {
                let payload = required_string(&row, "payload")?;
                Ok(serde_json::from_str(&payload)?)
            })
            .collect()
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
        let mut events = rows
            .iter()
            .take(limit)
            .map(|row| {
                let payload = required_string(row, "payload")?;
                let event = serde_json::from_str::<SessionEvent>(&payload)?;
                Ok(event)
            })
            .collect::<SessionDbResult<Vec<_>>>()?;
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

    /// Return model-context events from canonical DB events and indexed compaction metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if event queries or deserialization fail.
    pub async fn model_context_events(&self) -> SessionDbResult<Vec<SessionEvent>> {
        let Some(compaction_event) = self.latest_context_compaction_event().await? else {
            return self.latest_model_context_events().await;
        };
        let SessionEventKind::ContextCompacted {
            compacted_through_sequence,
            ..
        } = &compaction_event.kind
        else {
            return Ok(vec![compaction_event]);
        };
        let rows = self
            .db
            .select("events")
            .columns(&["payload"])
            .where_gt("event_seq", seq_to_value(*compacted_through_sequence))
            .sort("event_seq", SortDirection::Asc)
            .execute(&**self.db)
            .await?;
        let mut events = Vec::with_capacity(rows.len().saturating_add(1));
        events.push(compaction_event.clone());
        for row in rows {
            let payload = required_string(&row, "payload")?;
            let event = serde_json::from_str::<SessionEvent>(&payload)?;
            if event.sequence != compaction_event.sequence {
                events.push(event);
            }
        }
        Ok(events)
    }

    async fn latest_model_context_events(&self) -> SessionDbResult<Vec<SessionEvent>> {
        let mut selected_newest_first = Vec::new();
        let mut scanned = 0_usize;
        let mut before_sequence = None;

        while selected_newest_first.len() < MODEL_CONTEXT_EVENT_LIMIT
            && scanned < MODEL_CONTEXT_RAW_SCAN_LIMIT
        {
            let remaining_scan = MODEL_CONTEXT_RAW_SCAN_LIMIT.saturating_sub(scanned);
            let page_limit = MODEL_CONTEXT_SCAN_PAGE_LIMIT.min(remaining_scan).max(1);
            let mut select = self
                .db
                .select("events")
                .columns(&["event_seq", "event_type", "payload"])
                .sort("event_seq", SortDirection::Desc)
                .limit(page_limit);
            if let Some(sequence) = before_sequence {
                select = select.where_lte("event_seq", seq_to_value(sequence));
            }

            let rows = select.execute(&**self.db).await?;
            if rows.is_empty() {
                break;
            }
            scanned = scanned.saturating_add(rows.len());

            let mut oldest_sequence = None;
            for row in &rows {
                let sequence = required_i64(row, "event_seq").map(i64_to_u64)?;
                oldest_sequence =
                    Some(oldest_sequence.map_or(sequence, |oldest: u64| oldest.min(sequence)));
                let event_type = required_string(row, "event_type")?;
                if !is_model_context_event_type(&event_type) {
                    continue;
                }
                let payload = required_string(row, "payload")?;
                selected_newest_first.push(serde_json::from_str::<SessionEvent>(&payload)?);
                if selected_newest_first.len() >= MODEL_CONTEXT_EVENT_LIMIT {
                    break;
                }
            }

            let Some(oldest_sequence) = oldest_sequence else {
                break;
            };
            if oldest_sequence == 0 || rows.len() < page_limit {
                break;
            }
            before_sequence = Some(oldest_sequence.saturating_sub(1));
        }

        selected_newest_first.reverse();
        Ok(selected_newest_first)
    }

    async fn latest_context_compaction_event(&self) -> SessionDbResult<Option<SessionEvent>> {
        let row = self
            .db
            .select("events")
            .columns(&["payload"])
            .where_eq("event_type", "context_compacted")
            .sort("event_seq", SortDirection::Desc)
            .limit(1)
            .execute_first(&**self.db)
            .await?;
        row.as_ref()
            .map(|row| {
                let payload = required_string(row, "payload")?;
                Ok(serde_json::from_str(&payload)?)
            })
            .transpose()
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

        rows.into_iter()
            .map(|row| {
                let payload = required_string(&row, "payload")?;
                Ok(serde_json::from_str(&payload)?)
            })
            .collect()
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
    source
}

fn session_migrations() -> CodeMigrationSource<'static> {
    let mut source = CodeMigrationSource::new();
    add_sql_migration(
        &mut source,
        "001_events_table",
        "CREATE TABLE IF NOT EXISTS events (\n    event_seq INTEGER PRIMARY KEY NOT NULL,\n    event_type TEXT NOT NULL,\n    schema_version INTEGER NOT NULL,\n    created_at_ms INTEGER,\n    causation_id TEXT,\n    correlation_id TEXT,\n    payload TEXT NOT NULL\n)",
        "DROP TABLE IF EXISTS events",
    );
    add_sql_migration(
        &mut source,
        "002_events_event_type_index",
        "CREATE INDEX IF NOT EXISTS idx_events_event_type ON events(event_type)",
        "DROP INDEX IF EXISTS idx_events_event_type",
    );
    add_sql_migration(
        &mut source,
        "003_session_state_table",
        "CREATE TABLE IF NOT EXISTS session_state (\n    session_id TEXT PRIMARY KEY NOT NULL,\n    last_event_seq INTEGER NOT NULL,\n    current_model TEXT,\n    current_provider TEXT,\n    working_directory TEXT,\n    title TEXT,\n    updated_at_ms INTEGER\n)",
        "DROP TABLE IF EXISTS session_state",
    );
    add_sql_migration(
        &mut source,
        "004_input_messages_table",
        "CREATE TABLE IF NOT EXISTS input_messages (\n    input_seq INTEGER PRIMARY KEY NOT NULL,\n    event_seq INTEGER NOT NULL,\n    created_at_ms INTEGER,\n    text TEXT NOT NULL,\n    working_directory TEXT,\n    model TEXT,\n    FOREIGN KEY(event_seq) REFERENCES events(event_seq)\n)",
        "DROP TABLE IF EXISTS input_messages",
    );
    add_sql_migration(
        &mut source,
        "005_input_messages_event_seq_index",
        "CREATE INDEX IF NOT EXISTS idx_input_messages_event_seq ON input_messages(event_seq)",
        "DROP INDEX IF EXISTS idx_input_messages_event_seq",
    );
    add_sql_migration(
        &mut source,
        "006_transcript_items_table",
        "CREATE TABLE IF NOT EXISTS transcript_items (\n    transcript_seq INTEGER PRIMARY KEY NOT NULL,\n    event_seq_start INTEGER NOT NULL,\n    event_seq_end INTEGER NOT NULL,\n    role TEXT NOT NULL,\n    kind TEXT NOT NULL,\n    status TEXT NOT NULL,\n    content TEXT,\n    created_at_ms INTEGER,\n    FOREIGN KEY(event_seq_start) REFERENCES events(event_seq)\n)",
        "DROP TABLE IF EXISTS transcript_items",
    );
    add_sql_migration(
        &mut source,
        "007_transcript_items_event_range_index",
        "CREATE INDEX IF NOT EXISTS idx_transcript_items_event_range ON transcript_items(event_seq_start, event_seq_end)",
        "DROP INDEX IF EXISTS idx_transcript_items_event_range",
    );
    add_sql_migration(
        &mut source,
        "008_tool_runs_table",
        "CREATE TABLE IF NOT EXISTS tool_runs (\n    tool_call_id TEXT PRIMARY KEY NOT NULL,\n    event_seq_start INTEGER NOT NULL,\n    event_seq_end INTEGER,\n    status TEXT NOT NULL,\n    tool_name TEXT,\n    started_at_ms INTEGER,\n    completed_at_ms INTEGER,\n    output_bytes INTEGER,\n    is_error INTEGER,\n    FOREIGN KEY(event_seq_start) REFERENCES events(event_seq)\n)",
        "DROP TABLE IF EXISTS tool_runs",
    );
    add_sql_migration(
        &mut source,
        "009_tool_runs_status_index",
        "CREATE INDEX IF NOT EXISTS idx_tool_runs_status ON tool_runs(status)",
        "DROP INDEX IF EXISTS idx_tool_runs_status",
    );
    add_sql_migration(
        &mut source,
        "010_projection_checkpoints_table",
        "CREATE TABLE IF NOT EXISTS projection_checkpoints (\n    projection_name TEXT PRIMARY KEY NOT NULL,\n    last_event_seq INTEGER NOT NULL,\n    projection_version INTEGER NOT NULL,\n    updated_at_ms INTEGER\n)",
        "DROP TABLE IF EXISTS projection_checkpoints",
    );
    add_sql_migration(
        &mut source,
        "011_snapshots_table",
        "CREATE TABLE IF NOT EXISTS snapshots (\n    snapshot_name TEXT PRIMARY KEY NOT NULL,\n    last_event_seq INTEGER NOT NULL,\n    schema_version INTEGER NOT NULL,\n    payload TEXT NOT NULL,\n    updated_at_ms INTEGER\n)",
        "DROP TABLE IF EXISTS snapshots",
    );
    add_sql_migration(
        &mut source,
        "012_runtime_work_table",
        "CREATE TABLE IF NOT EXISTS runtime_work (\n    work_id TEXT PRIMARY KEY NOT NULL,\n    event_seq_start INTEGER NOT NULL,\n    event_seq_end INTEGER,\n    parent_work_id TEXT,\n    kind TEXT NOT NULL,\n    label TEXT NOT NULL,\n    status TEXT NOT NULL,\n    started_at_ms INTEGER,\n    finished_at_ms INTEGER,\n    message TEXT,\n    cancellable INTEGER NOT NULL DEFAULT 0,\n    FOREIGN KEY(event_seq_start) REFERENCES events(event_seq)\n)",
        "DROP TABLE IF EXISTS runtime_work",
    );
    add_sql_migration(
        &mut source,
        "013_runtime_work_status_index",
        "CREATE INDEX IF NOT EXISTS idx_runtime_work_status ON runtime_work(status)",
        "DROP INDEX IF EXISTS idx_runtime_work_status",
    );
    add_sql_migration(
        &mut source,
        "014_runtime_work_parent_index",
        "CREATE INDEX IF NOT EXISTS idx_runtime_work_parent_work_id ON runtime_work(parent_work_id)",
        "DROP INDEX IF EXISTS idx_runtime_work_parent_work_id",
    );
    source
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
            activity_timestamp_ms
                .or_else(|| event_created_at_ms(event))
                .map(seq_to_value),
        )
        .value("payload", serde_json::to_string(event)?)
        .execute(db)
        .await?;
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
                .value(
                    "updated_at_ms",
                    event_created_at_ms(event).map(seq_to_value),
                )
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
        SessionEventKind::UserMessage { text, .. } => {
            db.insert("input_messages")
                .value("input_seq", seq_to_value(event.sequence))
                .value("event_seq", seq_to_value(event.sequence))
                .value(
                    "created_at_ms",
                    event_created_at_ms(event).map(seq_to_value),
                )
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
            ..
        } => {
            db.update("tool_runs")
                .value("event_seq_end", seq_to_value(event.sequence))
                .value("status", if *is_error { "error" } else { "complete" })
                .value("is_error", bool_to_value(*is_error))
                .where_eq("tool_call_id", tool_call_id.clone())
                .execute(db)
                .await?;
            insert_transcript_item(
                db,
                event,
                "tool",
                "result",
                if *is_error { "error" } else { "complete" },
                None,
            )
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
        .value(
            "updated_at_ms",
            event_created_at_ms(event).map(seq_to_value),
        )
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
    let mut statement = db
        .insert("transcript_items")
        .value("transcript_seq", seq_to_value(event.sequence))
        .value("event_seq_start", seq_to_value(event.sequence))
        .value("event_seq_end", seq_to_value(event.sequence))
        .value("role", role)
        .value("kind", kind)
        .value("status", status)
        .value(
            "created_at_ms",
            event_created_at_ms(event).map(seq_to_value),
        );

    if let Some(content) = content {
        statement = statement.value("content", content);
    }

    statement.execute(db).await?;
    Ok(())
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
        SessionEventKind::PermissionRequested { .. } => "permission_requested",
        SessionEventKind::PermissionResolved { .. } => "permission_resolved",
        SessionEventKind::ModelChanged { .. } => "model_changed",
        SessionEventKind::SystemMessage { .. } => "system_message",
        SessionEventKind::AgentChanged { .. } => "agent_changed",
        SessionEventKind::ModelTurnStarted { .. } => "model_turn_started",
        SessionEventKind::ModelTurnFinished { .. } => "model_turn_finished",
        SessionEventKind::ModelUsage { .. } => "model_usage",
        SessionEventKind::ContextCompacted { .. } => "context_compacted",
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
        SessionEventKind::ToolInvocationPresentation { .. } => "tool_invocation_presentation",
    }
}

async fn update_projection_checkpoints(
    db: &dyn Database,
    event: &SessionEvent,
) -> SessionDbResult<()> {
    for projection in MaterializedProjection::all() {
        update_projection_checkpoint(db, projection.as_str(), event).await?;
    }
    Ok(())
}

async fn update_projection_checkpoint(
    db: &dyn Database,
    projection_name: &str,
    event: &SessionEvent,
) -> SessionDbResult<()> {
    let existing = db
        .select("projection_checkpoints")
        .columns(&["projection_name"])
        .where_eq("projection_name", projection_name)
        .execute_first(db)
        .await?;

    if existing.is_some() {
        db.update("projection_checkpoints")
            .value("last_event_seq", seq_to_value(event.sequence))
            .value("projection_version", DatabaseValue::Int32(1))
            .value(
                "updated_at_ms",
                event_created_at_ms(event).map(seq_to_value),
            )
            .where_eq("projection_name", projection_name)
            .execute(db)
            .await?;
    } else {
        db.insert("projection_checkpoints")
            .value("projection_name", projection_name)
            .value("last_event_seq", seq_to_value(event.sequence))
            .value("projection_version", DatabaseValue::Int32(1))
            .value(
                "updated_at_ms",
                event_created_at_ms(event).map(seq_to_value),
            )
            .execute(db)
            .await?;
    }

    Ok(())
}

fn runtime_work_from_row(row: &switchy::database::Row) -> SessionDbResult<RuntimeWorkProjection> {
    Ok(RuntimeWorkProjection {
        work_id: RuntimeWorkId::new(required_string(row, "work_id")?),
        event_seq_start: required_i64(row, "event_seq_start").map(i64_to_u64)?,
        event_seq_end: optional_i64(row, "event_seq_end").map(i64_to_u64),
        kind: parse_runtime_work_kind(&required_string(row, "kind")?),
        label: required_string(row, "label")?,
        status: parse_runtime_work_status(&required_string(row, "status")?),
        parent_work_id: optional_string(row, "parent_work_id").map(RuntimeWorkId::new),
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
        text: required_string(row, "text")?,
    })
}

const fn is_model_context_event_type(event_type: &str) -> bool {
    matches!(
        event_type.as_bytes(),
        b"user_message"
            | b"assistant_message"
            | b"tool_call_requested"
            | b"tool_call_finished"
            | b"system_message"
            | b"working_directory_changed"
            | b"context_compacted"
    )
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

const fn event_created_at_ms(event: &SessionEvent) -> Option<u64> {
    match &event.kind {
        SessionEventKind::SkillInvoked { invoked_at_ms, .. }
        | SessionEventKind::SkillSuggested {
            suggested_at_ms: invoked_at_ms,
            ..
        }
        | SessionEventKind::SkillActivated {
            activated_at_ms: invoked_at_ms,
            ..
        }
        | SessionEventKind::SkillDeactivated {
            deactivated_at_ms: invoked_at_ms,
            ..
        }
        | SessionEventKind::SkillContextLoaded {
            loaded_at_ms: invoked_at_ms,
            ..
        }
        | SessionEventKind::SkillInvocationFailed {
            failed_at_ms: invoked_at_ms,
            ..
        }
        | SessionEventKind::SessionImported {
            imported_at_ms: invoked_at_ms,
            ..
        } => Some(*invoked_at_ms),
        SessionEventKind::RuntimeWorkStarted { started_at_ms, .. }
        | SessionEventKind::RuntimeWorkCancelRequested {
            requested_at_ms: started_at_ms,
            ..
        }
        | SessionEventKind::RuntimeWorkFinished {
            finished_at_ms: started_at_ms,
            ..
        }
        | SessionEventKind::RuntimeWorkProgress {
            progress_at_ms: started_at_ms,
            ..
        }
        | SessionEventKind::ModelTurnCancelRequested {
            requested_at_ms: started_at_ms,
            ..
        }
        | SessionEventKind::ToolInvocationPresentation {
            finished_at_ms: started_at_ms,
            ..
        } => *started_at_ms,
        _ => None,
    }
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
    use bcode_session_models::{CURRENT_SESSION_EVENT_SCHEMA_VERSION, ClientId};

    fn event(session_id: SessionId, sequence: u64, kind: SessionEventKind) -> SessionEvent {
        SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            session_id,
            provenance: None,
            kind,
        }
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
                tool_name: "shell".to_string(),
                arguments_json: "{}".to_string(),
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
        let work_id = RuntimeWorkId::new("work-1");

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
