//! Current global and per-session SQL schema materialization.
//!
//! This module defines the current database shape. It contains no source-format interpretation or
//! historical migration policy.

use switchy::schema::discovery::code::{CodeMigration, CodeMigrationSource};

pub fn global_migrations() -> CodeMigrationSource<'static> {
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

pub fn session_migrations() -> CodeMigrationSource<'static> {
    let mut source = CodeMigrationSource::new();
    add_session_base_migrations(&mut source);
    add_session_runtime_migrations(&mut source);
    add_session_execution_migrations(&mut source);
    source
}

fn add_session_execution_migrations(source: &mut CodeMigrationSource<'static>) {
    add_sql_migration(
        source,
        "030_session_state_visibility_column",
        "ALTER TABLE session_state ADD COLUMN visibility TEXT",
        "ALTER TABLE session_state DROP COLUMN visibility",
    );
    add_sql_migration(
        source,
        "031_session_state_execution_provenance_column",
        "ALTER TABLE session_state ADD COLUMN execution_provenance TEXT",
        "ALTER TABLE session_state DROP COLUMN execution_provenance",
    );
    add_sql_migration(
        source,
        "032_terminal_tool_lifecycle_projection",
        "UPDATE session_storage_contract SET writer_epoch = writer_epoch WHERE contract_id = 1",
        "UPDATE session_storage_contract SET writer_epoch = writer_epoch WHERE contract_id = 1",
    );
    add_sql_migration(
        source,
        "033_session_migration_receipts_table",
        "CREATE TABLE IF NOT EXISTS session_migration_receipts (\n    operation_id TEXT PRIMARY KEY NOT NULL,\n    session_id TEXT NOT NULL,\n    source_writer_epoch INTEGER NOT NULL,\n    target_writer_epoch INTEGER NOT NULL,\n    receipt TEXT NOT NULL,\n    completed_at_ms INTEGER NOT NULL\n)",
        "DROP TABLE IF EXISTS session_migration_receipts",
    );
    add_sql_migration(
        source,
        "034_session_state_model_selection_source_column",
        "ALTER TABLE session_state ADD COLUMN model_selection_source TEXT",
        "ALTER TABLE session_state DROP COLUMN model_selection_source",
    );
    add_sql_migration(
        source,
        "035_session_state_reasoning_by_model_column",
        "ALTER TABLE session_state ADD COLUMN reasoning_by_model TEXT",
        "ALTER TABLE session_state DROP COLUMN reasoning_by_model",
    );
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

#[allow(clippy::too_many_lines)]
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
    add_session_compatibility_ledger_markers(source);
}

fn add_session_compatibility_ledger_markers(source: &mut CodeMigrationSource<'static>) {
    // Preserve released ledger identities without materializing superseded compatibility tables.
    add_sql_migration(
        source,
        "028_session_compatibility_state",
        "UPDATE session_storage_contract SET contract_id = contract_id WHERE 0",
        "UPDATE session_storage_contract SET contract_id = contract_id WHERE 0",
    );
    add_sql_migration(
        source,
        "029_session_compatibility_issues",
        "UPDATE session_storage_contract SET contract_id = contract_id WHERE 0",
        "UPDATE session_storage_contract SET contract_id = contract_id WHERE 0",
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
