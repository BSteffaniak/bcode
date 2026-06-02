#![allow(clippy::module_name_repetitions)]

//! Switchy-backed session database primitives.
//!
//! This module is the first CQRS/event-store database slice for Bcode sessions. It
//! intentionally keeps Turso-specific details at connection boundaries and uses
//! `switchy` database traits for migrations and repository operations.

use std::{fs, path::Path, sync::Arc};

use bcode_session_models::{SessionEvent, SessionEventKind, SessionId, SessionInputHistoryEntry};
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
    /// Event serialization failed.
    #[error(transparent)]
    Serialize(#[from] serde_json::Error),
    /// A database row did not contain the expected column/type.
    #[error("invalid session database row: missing or invalid column {column}")]
    InvalidRow { column: String },
}

/// Result type for session DB operations.
pub type SessionDbResult<T> = Result<T, SessionDbError>;

/// Return Bcode's default per-session database path for `session_id`.
#[must_use]
pub fn session_db_path(root: &Path, session_id: SessionId) -> std::path::PathBuf {
    root.join(session_id.to_string()).join("session.db")
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
}

impl GlobalSessionDb {
    /// Open the global session catalog database at `path` and apply cheap schema migrations.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * the Turso connection cannot be opened
    /// * schema migrations fail
    pub async fn open_turso(path: &Path) -> SessionDbResult<Self> {
        let db = switchy::database_connection::init_turso_local(Some(path)).await?;
        run_global_migrations(&*db).await?;
        Ok(Self { db: Arc::new(db) })
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
    /// * the Turso connection cannot be opened
    /// * schema migrations fail
    pub async fn open_turso(session_id: SessionId, path: &Path) -> SessionDbResult<Self> {
        let db = switchy::database_connection::init_turso_local(Some(path)).await?;
        run_session_migrations(&*db).await?;
        Ok(Self {
            session_id,
            db: Arc::new(db),
        })
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
    /// Return the last event sequence processed by a projection, if known.
    ///
    /// # Errors
    ///
    /// Returns an error if the checkpoint query fails or returns an invalid row.
    pub async fn projection_checkpoint(
        &self,
        projection_name: &str,
    ) -> SessionDbResult<Option<u64>> {
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
    /// Returns an error if event timestamp queries fail or rows are malformed.
    pub async fn activity_bounds(&self) -> SessionDbResult<Option<(u64, u64)>> {
        let rows = self
            .db
            .select("events")
            .columns(&["created_at_ms"])
            .sort("event_seq", SortDirection::Asc)
            .execute(&**self.db)
            .await?;
        let timestamps = rows
            .iter()
            .filter_map(|row| row.get("created_at_ms").and_then(|value| value.as_i64()))
            .map(i64_to_u64)
            .collect::<Vec<_>>();
        Ok(timestamps.first().copied().zip(timestamps.last().copied()))
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
        let mut items = self
            .latest_transcript_rows(limit)
            .await?
            .iter()
            .map(transcript_item_from_row)
            .collect::<SessionDbResult<Vec<_>>>()?;
        items.sort_by_key(|item| item.transcript_seq);
        Ok(items)
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
    }
}

async fn update_projection_checkpoints(
    db: &dyn Database,
    event: &SessionEvent,
) -> SessionDbResult<()> {
    for projection_name in [
        "session_state",
        "input_history",
        "transcript",
        "tool_runs",
        "model_context",
    ] {
        update_projection_checkpoint(db, projection_name, event).await?;
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

const fn i64_to_u64(value: i64) -> u64 {
    if value.is_negative() {
        0
    } else {
        value.cast_unsigned()
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
            db.projection_checkpoint("input_history")
                .await
                .expect("input checkpoint"),
            Some(2)
        );
        assert_eq!(
            db.projection_checkpoint("transcript")
                .await
                .expect("transcript checkpoint"),
            Some(2)
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
