#![allow(clippy::module_name_repetitions)]

//! Switchy-backed session database primitives.
//!
//! This module is the first CQRS/event-store database slice for Bcode sessions. It
//! intentionally keeps Turso-specific details at connection boundaries and uses
//! `switchy` database traits for migrations and repository operations.

use std::{path::Path, sync::Arc};

use bcode_session_models::{SessionEvent, SessionEventKind, SessionId};
use switchy::{
    database::{Database, DatabaseError, DatabaseValue, query::FilterableQuery},
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
    /// Event serialization failed.
    #[error(transparent)]
    Serialize(#[from] serde_json::Error),
}

/// Result type for session DB operations.
pub type SessionDbResult<T> = Result<T, SessionDbError>;

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
        let tx = self.db.begin_transaction().await?;
        insert_event(&*tx, event).await?;
        project_event(&*tx, event).await?;
        tx.commit().await?;
        Ok(())
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
    source.add_migration(CodeMigration::new(
        "001_global_catalog".to_string(),
        Box::new(
            r"
CREATE TABLE IF NOT EXISTS sessions (
    session_id TEXT PRIMARY KEY NOT NULL,
    db_path TEXT NOT NULL,
    title TEXT,
    working_directory TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    state TEXT NOT NULL DEFAULT 'active',
    projection_status TEXT NOT NULL DEFAULT 'fresh'
);
CREATE INDEX IF NOT EXISTS idx_sessions_updated_at_ms ON sessions(updated_at_ms);
"
            .to_string(),
        ),
        Some(Box::new("DROP TABLE IF EXISTS sessions".to_string())),
    ));
    source
}

fn session_migrations() -> CodeMigrationSource<'static> {
    let mut source = CodeMigrationSource::new();
    source.add_migration(CodeMigration::new(
        "001_session_event_store_and_projections".to_string(),
        Box::new(
            r"
CREATE TABLE IF NOT EXISTS events (
    event_seq INTEGER PRIMARY KEY NOT NULL,
    event_type TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    created_at_ms INTEGER,
    causation_id TEXT,
    correlation_id TEXT,
    payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_events_event_type ON events(event_type);

CREATE TABLE IF NOT EXISTS session_state (
    session_id TEXT PRIMARY KEY NOT NULL,
    last_event_seq INTEGER NOT NULL,
    current_model TEXT,
    current_provider TEXT,
    working_directory TEXT,
    title TEXT,
    updated_at_ms INTEGER
);

CREATE TABLE IF NOT EXISTS input_messages (
    input_seq INTEGER PRIMARY KEY NOT NULL,
    event_seq INTEGER NOT NULL,
    created_at_ms INTEGER,
    text TEXT NOT NULL,
    working_directory TEXT,
    model TEXT,
    FOREIGN KEY(event_seq) REFERENCES events(event_seq)
);
CREATE INDEX IF NOT EXISTS idx_input_messages_event_seq ON input_messages(event_seq);

CREATE TABLE IF NOT EXISTS transcript_items (
    transcript_seq INTEGER PRIMARY KEY NOT NULL,
    event_seq_start INTEGER NOT NULL,
    event_seq_end INTEGER NOT NULL,
    role TEXT NOT NULL,
    kind TEXT NOT NULL,
    status TEXT NOT NULL,
    content TEXT,
    created_at_ms INTEGER,
    FOREIGN KEY(event_seq_start) REFERENCES events(event_seq)
);
CREATE INDEX IF NOT EXISTS idx_transcript_items_event_range ON transcript_items(event_seq_start, event_seq_end);

CREATE TABLE IF NOT EXISTS tool_runs (
    tool_call_id TEXT PRIMARY KEY NOT NULL,
    event_seq_start INTEGER NOT NULL,
    event_seq_end INTEGER,
    status TEXT NOT NULL,
    tool_name TEXT,
    started_at_ms INTEGER,
    completed_at_ms INTEGER,
    output_bytes INTEGER,
    is_error INTEGER,
    FOREIGN KEY(event_seq_start) REFERENCES events(event_seq)
);
CREATE INDEX IF NOT EXISTS idx_tool_runs_status ON tool_runs(status);

CREATE TABLE IF NOT EXISTS projection_checkpoints (
    projection_name TEXT PRIMARY KEY NOT NULL,
    last_event_seq INTEGER NOT NULL,
    projection_version INTEGER NOT NULL,
    updated_at_ms INTEGER
);

CREATE TABLE IF NOT EXISTS snapshots (
    snapshot_name TEXT PRIMARY KEY NOT NULL,
    last_event_seq INTEGER NOT NULL,
    schema_version INTEGER NOT NULL,
    payload TEXT NOT NULL,
    updated_at_ms INTEGER
);
"
            .to_string(),
        ),
        Some(Box::new(
            r"
DROP TABLE IF EXISTS snapshots;
DROP TABLE IF EXISTS projection_checkpoints;
DROP TABLE IF EXISTS tool_runs;
DROP TABLE IF EXISTS transcript_items;
DROP TABLE IF EXISTS input_messages;
DROP TABLE IF EXISTS session_state;
DROP TABLE IF EXISTS events;
"
            .to_string(),
        )),
    ));
    source
}

async fn insert_event(db: &dyn Database, event: &SessionEvent) -> SessionDbResult<()> {
    db.insert("events")
        .value("event_seq", seq_to_value(event.sequence))
        .value("event_type", event_kind_name(&event.kind))
        .value(
            "schema_version",
            DatabaseValue::Int32(i32::from(event.schema_version)),
        )
        .value("payload", serde_json::to_string(event)?)
        .execute(db)
        .await?;
    Ok(())
}

async fn project_event(db: &dyn Database, event: &SessionEvent) -> SessionDbResult<()> {
    update_session_state(db, event).await?;
    match &event.kind {
        SessionEventKind::UserMessage { text, .. } => {
            db.insert("input_messages")
                .value("input_seq", seq_to_value(event.sequence))
                .value("event_seq", seq_to_value(event.sequence))
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
        .value("status", status);

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
