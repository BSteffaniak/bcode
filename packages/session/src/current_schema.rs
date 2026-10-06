//! Current global and per-session SQL schema materialization.
//!
//! This module defines the current database shape. It contains no source-format interpretation or
//! historical migration policy.

#[cfg(test)]
mod tests;

use std::sync::Arc;
use switchy::database::query::{FilterableQuery, delete, update};
use switchy::database::schema::{
    Column, DataType, alter_table, create_index, create_table, drop_index, drop_table,
};
use switchy::database::{Database, DatabaseError, DatabaseValue, Executable};
use switchy::schema::discovery::code::CodeMigration;
use switchy::schema::migration::{Migration, MigrationSource};

/// Current schema operations in append-only contract order, independent of identifier spelling.
#[derive(Default)]
pub struct CurrentMigrationSource {
    migrations: Vec<Arc<dyn Migration<'static>>>,
}

impl CurrentMigrationSource {
    fn new() -> Self {
        Self::default()
    }

    fn add_migration(&mut self, migration: CodeMigration<'static>) {
        self.migrations.push(Arc::new(migration));
    }
}

#[async_trait::async_trait]
impl MigrationSource<'static> for CurrentMigrationSource {
    async fn migrations(&self) -> switchy::schema::Result<Vec<Arc<dyn Migration<'static>>>> {
        Ok(self.migrations.clone())
    }
}

pub fn global_migrations() -> CurrentMigrationSource {
    let mut source = CurrentMigrationSource::new();
    add_migration(
        &mut source,
        "001_global_sessions_table",
        create_table("sessions")
            .if_not_exists(true)
            .column(schema_column("session_id", DataType::Text, false))
            .primary_key("session_id")
            .column(schema_column("db_path", DataType::Text, false))
            .column(schema_column("title", DataType::Text, true))
            .column(schema_column("working_directory", DataType::Text, true))
            .column(schema_column("created_at_ms", DataType::Int, false))
            .column(schema_column("updated_at_ms", DataType::Int, false))
            .column(Column {
                name: "state".into(),
                data_type: DataType::Text,
                nullable: false,
                auto_increment: false,
                default: Some("active".into()),
            })
            .column(Column {
                name: "projection_status".into(),
                data_type: DataType::Text,
                nullable: false,
                auto_increment: false,
                default: Some("fresh".into()),
            }),
        drop_table("sessions").if_exists(true),
    );
    add_migration(
        &mut source,
        "002_global_sessions_updated_at_index",
        create_index("idx_sessions_updated_at_ms")
            .table("sessions")
            .if_not_exists(true)
            .columns(vec!["updated_at_ms"]),
        drop_index("idx_sessions_updated_at_ms", "sessions").if_exists(),
    );
    add_migration(
        &mut source,
        "003_global_composer_drafts_table",
        create_table("composer_drafts")
            .if_not_exists(true)
            .column(schema_column("scope_kind", DataType::Text, false))
            .column(schema_column("scope_key", DataType::Text, false))
            .column(schema_column(
                "launch_working_directory",
                DataType::Text,
                true,
            ))
            .column(schema_column("session_id", DataType::Text, true))
            .column(schema_column("text", DataType::Text, false))
            .column(schema_column("updated_at_ms", DataType::Int, false))
            .primary_key_columns(["scope_kind", "scope_key"]),
        drop_table("composer_drafts").if_exists(true),
    );
    add_migration(
        &mut source,
        "004_global_session_parent_column",
        alter_table("sessions").add_column("parent_id".into(), DataType::Text, true, None),
        alter_table("sessions").drop_column("parent_id".into()),
    );
    add_migration(
        &mut source,
        "005_global_session_parent_index",
        create_index("idx_sessions_parent_id")
            .if_not_exists(true)
            .table("sessions")
            .columns(vec!["parent_id", "session_id"]),
        drop_index("idx_sessions_parent_id", "sessions"),
    );
    source
}

pub fn session_migrations() -> CurrentMigrationSource {
    let mut source = CurrentMigrationSource::new();
    add_session_base_migrations(&mut source);
    add_session_runtime_migrations(&mut source);
    add_session_execution_migrations(&mut source);
    // Preserve the established ledger prefix before appending newer schema operations.
    // Identifiers are durable: sorting the entire catalog would insert submissions ahead
    // of the already released terminal-evidence index.
    source.migrations.sort_by(|a, b| a.id().cmp(b.id()));
    add_migration(
        &mut source,
        "043_submissions_table",
        create_table("submissions")
            .if_not_exists(true)
            .column(schema_column("submission_id", DataType::Text, false))
            .primary_key("submission_id")
            .column(schema_column("accepted_seq", DataType::Int, false))
            .column(schema_column("target_turn_id", DataType::Text, false))
            .column(schema_column("pending", DataType::Int, false))
            .column(schema_column("record", DataType::Text, false)),
        drop_table("submissions").if_exists(true),
    );
    add_migration(
        &mut source,
        "044_submissions_pending_index",
        create_index("idx_submissions_pending")
            .if_not_exists(true)
            .table("submissions")
            .columns(vec!["pending", "accepted_seq"]),
        drop_index("idx_submissions_pending", "submissions"),
    );
    add_migration(
        &mut source,
        "045_submissions_target_index",
        create_index("idx_submissions_target_pending")
            .if_not_exists(true)
            .table("submissions")
            .columns(vec!["target_turn_id", "pending", "accepted_seq"]),
        drop_index("idx_submissions_target_pending", "submissions"),
    );
    add_migration(
        &mut source,
        "046_session_state_parent_column",
        alter_table("session_state").add_column("parent_id".into(), DataType::Text, true, None),
        alter_table("session_state").drop_column("parent_id".into()),
    );
    source
}

fn add_session_execution_migrations(source: &mut CurrentMigrationSource) {
    add_migration(
        source,
        "030_session_state_visibility_column",
        alter_table("session_state").add_column("visibility".into(), DataType::Text, true, None),
        alter_table("session_state").drop_column("visibility".into()),
    );
    add_migration(
        source,
        "031_session_state_execution_provenance_column",
        alter_table("session_state").add_column(
            "execution_provenance".into(),
            DataType::Text,
            true,
            None,
        ),
        alter_table("session_state").drop_column("execution_provenance".into()),
    );
    add_migration(
        source,
        "032_terminal_tool_lifecycle_projection",
        update("session_storage_contract")
            .value(
                "writer_epoch",
                switchy::database::query::identifier("writer_epoch"),
            )
            .where_eq("contract_id", 1_i64),
        update("session_storage_contract")
            .value(
                "writer_epoch",
                switchy::database::query::identifier("writer_epoch"),
            )
            .where_eq("contract_id", 1_i64),
    );
    add_migration(
        source,
        "033_session_migration_receipts_table",
        create_table("session_migration_receipts")
            .if_not_exists(true)
            .column(schema_column("operation_id", DataType::Text, false))
            .primary_key("operation_id")
            .column(schema_column("session_id", DataType::Text, false))
            .column(schema_column("source_writer_epoch", DataType::Int, false))
            .column(schema_column("target_writer_epoch", DataType::Int, false))
            .column(schema_column("receipt", DataType::Text, false))
            .column(schema_column("completed_at_ms", DataType::Int, false)),
        drop_table("session_migration_receipts").if_exists(true),
    );
    add_migration(
        source,
        "034_session_state_model_selection_source_column",
        alter_table("session_state").add_column(
            "model_selection_source".into(),
            DataType::Text,
            true,
            None,
        ),
        alter_table("session_state").drop_column("model_selection_source".into()),
    );
    add_migration(
        source,
        "035_session_state_reasoning_by_model_column",
        alter_table("session_state").add_column(
            "reasoning_by_model".into(),
            DataType::Text,
            true,
            None,
        ),
        alter_table("session_state").drop_column("reasoning_by_model".into()),
    );
    add_session_usage_migrations(source);
}

fn add_session_usage_migrations(source: &mut CurrentMigrationSource) {
    add_migration(
        source,
        "036_session_usage_projection_table",
        create_table("session_usage_projection")
            .if_not_exists(true)
            .column(schema_column("projection_id", DataType::Int, false))
            .primary_key("projection_id")
            .column(schema_column("summary_json", DataType::Text, false)),
        drop_table("session_usage_projection").if_exists(true),
    );
    add_migration(
        source,
        "037_session_usage_requests_table",
        create_table("session_usage_requests")
            .if_not_exists(true)
            .column(schema_column("request_key", DataType::Text, false))
            .primary_key("request_key")
            .column(schema_column("usage_json", DataType::Text, false))
            .column(schema_column("event_seq", DataType::Int, false))
            .foreign_key_column("event_seq", "events", "event_seq"),
        drop_table("session_usage_requests").if_exists(true),
    );
    add_migration(
        source,
        "038_session_usage_request_timestamp",
        alter_table("session_usage_requests").add_column(
            "first_observed_at_ms".into(),
            DataType::Int,
            false,
            Some(0_i64.into()),
        ),
        alter_table("session_usage_requests").drop_column("first_observed_at_ms".into()),
    );
    add_migration(
        source,
        "039_session_usage_derived_cost",
        alter_table("session_usage_requests").add_column(
            "cost_json".into(),
            DataType::Text,
            true,
            None,
        ),
        alter_table("session_usage_requests").drop_column("cost_json".into()),
    );
    add_migration(
        source,
        "040_session_usage_timestamp_index",
        create_index("idx_session_usage_timestamp")
            .table("session_usage_requests")
            .if_not_exists(false)
            .columns(vec!["first_observed_at_ms", "request_key"]),
        drop_index("idx_session_usage_timestamp", "session_usage_requests"),
    );
    add_migration(
        source,
        "041_usage_valuation_staging",
        create_table("usage_valuation_staging")
            .if_not_exists(false)
            .column(schema_column("request_key", DataType::Text, true))
            .primary_key("request_key")
            .column(schema_column("usage_json", DataType::Text, false))
            .column(schema_column("cost_json", DataType::Text, false)),
        drop_table("usage_valuation_staging").if_exists(false),
    );
    add_migration(
        source,
        "042_turn_evidence",
        create_table("turn_evidence")
            .if_not_exists(false)
            .column(schema_column("turn_id", DataType::Text, false))
            .column(schema_column("evidence_kind", DataType::Text, false))
            .column(schema_column("segment_order", DataType::Int, false))
            .column(schema_column("event_seq", DataType::Int, false))
            .primary_key_columns(["turn_id", "evidence_kind", "segment_order", "event_seq"])
            .foreign_key_column("event_seq", "events", "event_seq"),
        drop_table("turn_evidence").if_exists(false),
    );
    add_migration(
        source,
        "043_turn_evidence_terminal_index",
        create_index("idx_turn_evidence_sequence")
            .table("turn_evidence")
            .if_not_exists(false)
            .columns(vec!["turn_id", "evidence_kind", "event_seq"]),
        drop_index("idx_turn_evidence_sequence", "turn_evidence"),
    );
}

fn add_session_base_migrations(source: &mut CurrentMigrationSource) {
    add_migration(
        source,
        "001_events_table",
        create_table("events")
            .if_not_exists(true)
            .column(schema_column("event_seq", DataType::Int, false))
            .primary_key("event_seq")
            .column(schema_column("event_type", DataType::Text, false))
            .column(schema_column("schema_version", DataType::Int, false))
            .column(schema_column("created_at_ms", DataType::Int, true))
            .column(schema_column("causation_id", DataType::Text, true))
            .column(schema_column("correlation_id", DataType::Text, true))
            .column(schema_column("payload", DataType::Text, false)),
        drop_table("events").if_exists(true),
    );
    add_migration(
        source,
        "002_events_event_type_index",
        create_index("idx_events_event_type")
            .table("events")
            .if_not_exists(true)
            .columns(vec!["event_type"]),
        drop_index("idx_events_event_type", "events").if_exists(),
    );
    add_migration(
        source,
        "003_session_state_table",
        create_table("session_state")
            .if_not_exists(true)
            .column(schema_column("session_id", DataType::Text, false))
            .primary_key("session_id")
            .column(schema_column("last_event_seq", DataType::Int, false))
            .column(schema_column("current_model", DataType::Text, true))
            .column(schema_column("current_provider", DataType::Text, true))
            .column(schema_column("working_directory", DataType::Text, true))
            .column(schema_column("title", DataType::Text, true))
            .column(schema_column("updated_at_ms", DataType::Int, true)),
        drop_table("session_state").if_exists(true),
    );
    add_migration(
        source,
        "004_input_messages_table",
        create_table("input_messages")
            .if_not_exists(true)
            .column(schema_column("input_seq", DataType::Int, false))
            .primary_key("input_seq")
            .column(schema_column("event_seq", DataType::Int, false))
            .column(schema_column("created_at_ms", DataType::Int, true))
            .column(schema_column("text", DataType::Text, false))
            .column(schema_column("working_directory", DataType::Text, true))
            .column(schema_column("model", DataType::Text, true))
            .foreign_key_column("event_seq", "events", "event_seq"),
        drop_table("input_messages").if_exists(true),
    );
    add_migration(
        source,
        "005_input_messages_event_seq_index",
        create_index("idx_input_messages_event_seq")
            .table("input_messages")
            .if_not_exists(true)
            .columns(vec!["event_seq"]),
        drop_index("idx_input_messages_event_seq", "input_messages").if_exists(),
    );
    add_migration(
        source,
        "006_transcript_items_table",
        create_table("transcript_items")
            .if_not_exists(true)
            .column(schema_column("transcript_seq", DataType::Int, false))
            .primary_key("transcript_seq")
            .column(schema_column("event_seq_start", DataType::Int, false))
            .column(schema_column("event_seq_end", DataType::Int, false))
            .column(schema_column("role", DataType::Text, false))
            .column(schema_column("kind", DataType::Text, false))
            .column(schema_column("status", DataType::Text, false))
            .column(schema_column("content", DataType::Text, true))
            .column(schema_column("created_at_ms", DataType::Int, true))
            .foreign_key_column("event_seq_start", "events", "event_seq"),
        drop_table("transcript_items").if_exists(true),
    );
    add_session_projection_migrations(source);
}

fn add_session_projection_migrations(source: &mut CurrentMigrationSource) {
    add_migration(
        source,
        "007_transcript_items_event_range_index",
        create_index("idx_transcript_items_event_range")
            .table("transcript_items")
            .if_not_exists(true)
            .columns(vec!["event_seq_start", "event_seq_end"]),
        drop_index("idx_transcript_items_event_range", "transcript_items").if_exists(),
    );
    add_migration(
        source,
        "008_tool_runs_table",
        create_table("tool_runs")
            .if_not_exists(true)
            .column(schema_column("tool_call_id", DataType::Text, false))
            .primary_key("tool_call_id")
            .column(schema_column("event_seq_start", DataType::Int, false))
            .column(schema_column("event_seq_end", DataType::Int, true))
            .column(schema_column("status", DataType::Text, false))
            .column(schema_column("tool_name", DataType::Text, true))
            .column(schema_column("started_at_ms", DataType::Int, true))
            .column(schema_column("completed_at_ms", DataType::Int, true))
            .column(schema_column("output_bytes", DataType::Int, true))
            .column(schema_column("is_error", DataType::Int, true))
            .foreign_key_column("event_seq_start", "events", "event_seq"),
        drop_table("tool_runs").if_exists(true),
    );
    add_migration(
        source,
        "009_tool_runs_status_index",
        create_index("idx_tool_runs_status")
            .table("tool_runs")
            .if_not_exists(true)
            .columns(vec!["status"]),
        drop_index("idx_tool_runs_status", "tool_runs").if_exists(),
    );
    add_migration(
        source,
        "010_projection_checkpoints_table",
        create_table("projection_checkpoints")
            .if_not_exists(true)
            .column(schema_column("projection_name", DataType::Text, false))
            .primary_key("projection_name")
            .column(schema_column("last_event_seq", DataType::Int, false))
            .column(schema_column("projection_version", DataType::Int, false))
            .column(schema_column("updated_at_ms", DataType::Int, true)),
        drop_table("projection_checkpoints").if_exists(true),
    );
    add_migration(
        source,
        "011_snapshots_table",
        create_table("snapshots")
            .if_not_exists(true)
            .column(schema_column("snapshot_name", DataType::Text, false))
            .primary_key("snapshot_name")
            .column(schema_column("last_event_seq", DataType::Int, false))
            .column(schema_column("schema_version", DataType::Int, false))
            .column(schema_column("payload", DataType::Text, false))
            .column(schema_column("updated_at_ms", DataType::Int, true)),
        drop_table("snapshots").if_exists(true),
    );
}

#[allow(clippy::too_many_lines)]
fn add_session_runtime_migrations(source: &mut CurrentMigrationSource) {
    add_migration(
        source,
        "012_runtime_work_table",
        create_table("runtime_work")
            .if_not_exists(true)
            .column(schema_column("work_id", DataType::Text, false))
            .primary_key("work_id")
            .column(schema_column("event_seq_start", DataType::Int, false))
            .column(schema_column("event_seq_end", DataType::Int, true))
            .column(schema_column("parent_work_id", DataType::Text, true))
            .column(schema_column("kind", DataType::Text, false))
            .column(schema_column("label", DataType::Text, false))
            .column(schema_column("status", DataType::Text, false))
            .column(schema_column("started_at_ms", DataType::Int, true))
            .column(schema_column("finished_at_ms", DataType::Int, true))
            .column(schema_column("message", DataType::Text, true))
            .column(Column {
                name: "cancellable".into(),
                data_type: DataType::Int,
                nullable: false,
                auto_increment: false,
                default: Some(0_i64.into()),
            })
            .foreign_key_column("event_seq_start", "events", "event_seq"),
        drop_table("runtime_work").if_exists(true),
    );
    add_migration(
        source,
        "013_runtime_work_status_index",
        create_index("idx_runtime_work_status")
            .table("runtime_work")
            .if_not_exists(true)
            .columns(vec!["status"]),
        drop_index("idx_runtime_work_status", "runtime_work").if_exists(),
    );
    add_migration(
        source,
        "014_runtime_work_parent_index",
        create_index("idx_runtime_work_parent_work_id")
            .table("runtime_work")
            .if_not_exists(true)
            .columns(vec!["parent_work_id"]),
        drop_index("idx_runtime_work_parent_work_id", "runtime_work").if_exists(),
    );
    add_migration(
        source,
        "015_session_drafts_table",
        create_table("session_drafts")
            .if_not_exists(true)
            .column(schema_column("session_id", DataType::Text, false))
            .primary_key("session_id")
            .column(schema_column("text", DataType::Text, false))
            .column(schema_column("updated_at_ms", DataType::Int, false)),
        drop_table("session_drafts").if_exists(true),
    );
    add_migration(
        source,
        "016_session_state_reasoning_effort_column",
        alter_table("session_state").add_column(
            "reasoning_effort".into(),
            DataType::Text,
            true,
            None,
        ),
        alter_table("session_state").drop_column("reasoning_effort".into()),
    );
    add_migration(
        source,
        "017_session_state_reasoning_summary_column",
        alter_table("session_state").add_column(
            "reasoning_summary".into(),
            DataType::Text,
            true,
            None,
        ),
        alter_table("session_state").drop_column("reasoning_summary".into()),
    );
    add_migration(
        source,
        "018_model_context_projection_state_table",
        create_table("model_context_projection_state")
            .if_not_exists(true)
            .column(schema_column("projection_id", DataType::Int, false))
            .primary_key("projection_id")
            .column(schema_column("schema_version", DataType::Int, false))
            .column(schema_column("last_event_seq", DataType::Int, false)),
        drop_table("model_context_projection_state").if_exists(true),
    );
    add_migration(
        source,
        "019_model_context_entries_table",
        create_table("model_context_entries")
            .if_not_exists(true)
            .column(schema_column("event_seq", DataType::Int, false))
            .primary_key("event_seq")
            .column(schema_column("event_type", DataType::Text, false))
            .column(schema_column("payload", DataType::Text, false))
            .foreign_key_column("event_seq", "events", "event_seq"),
        drop_table("model_context_entries").if_exists(true),
    );
    add_migration(
        source,
        "020_model_context_entries_event_type_index",
        create_index("idx_model_context_entries_event_type")
            .table("model_context_entries")
            .if_not_exists(true)
            .columns(vec!["event_type"]),
        drop_index(
            "idx_model_context_entries_event_type",
            "model_context_entries",
        )
        .if_exists(),
    );
    add_migration(
        source,
        "021_artifact_references_table",
        create_table("artifact_references")
            .if_not_exists(true)
            .column(schema_column("artifact_id", DataType::Text, false))
            .column(schema_column("reference_key", DataType::Text, false))
            .column(schema_column("producer_plugin_id", DataType::Text, false))
            .column(schema_column("schema", DataType::Text, false))
            .column(schema_column("schema_version", DataType::Int, false))
            .column(schema_column("storage_uri", DataType::Text, true))
            .column(schema_column("content_type", DataType::Text, true))
            .column(schema_column("byte_len", DataType::Int, true))
            .column(schema_column("availability", DataType::Text, true))
            .column(schema_column("complete", DataType::Int, true))
            .column(schema_column("checksum_sha256", DataType::Text, true))
            .column(schema_column("finalized_event_seq", DataType::Int, false))
            .primary_key_columns(["artifact_id", "reference_key"])
            .foreign_key_column("finalized_event_seq", "events", "event_seq"),
        drop_table("artifact_references").if_exists(true),
    );
    add_migration(
        source,
        "022_context_occupancy_projection_table",
        InitializeOccupancy,
        drop_table("context_occupancy_projection").if_exists(true),
    );
    add_migration(
        source,
        "023_reset_legacy_context_occupancy_projection",
        update("context_occupancy_projection")
            .value("schema_version", 3_i64)
            .value("occupancy_json", DatabaseValue::Null)
            .where_lt("schema_version", 3_i64),
        update("context_occupancy_projection")
            .value("schema_version", 2_i64)
            .value("occupancy_json", DatabaseValue::Null)
            .where_eq("schema_version", 3_i64),
    );
    add_migration(
        source,
        "024_reset_request_context_occupancy_projection",
        update("context_occupancy_projection")
            .value("schema_version", 4_i64)
            .value("occupancy_json", DatabaseValue::Null)
            .where_lt("schema_version", 4_i64),
        update("context_occupancy_projection")
            .value("schema_version", 3_i64)
            .value("occupancy_json", DatabaseValue::Null)
            .where_eq("schema_version", 4_i64),
    );
    add_migration(
        source,
        "025_turn_receipts_table",
        create_table("turn_receipts")
            .if_not_exists(true)
            .column(schema_column("producer", DataType::Text, false))
            .column(schema_column("idempotency_key", DataType::Text, false))
            .column(schema_column("accepted_event_seq", DataType::Int, false))
            .column(schema_column("turn_id", DataType::Text, false))
            .column(schema_column("work_id", DataType::Text, false))
            .primary_key_columns(["producer", "idempotency_key"])
            .foreign_key_column("accepted_event_seq", "events", "event_seq"),
        drop_table("turn_receipts").if_exists(true),
    );
    add_migration(
        source,
        "026_session_storage_contract_table",
        create_table("session_storage_contract")
            .if_not_exists(true)
            .column(schema_column("contract_id", DataType::Int, false))
            .primary_key("contract_id")
            .column(schema_column("schema_version", DataType::Int, false))
            .column(schema_column("writer_epoch", DataType::Int, false))
            .column(schema_column("updated_by_build", DataType::Text, true)),
        drop_table("session_storage_contract").if_exists(true),
    );
    add_migration(
        source,
        "027_initialize_session_storage_contract",
        InitializeStorageContract,
        delete("session_storage_contract").where_eq("contract_id", 1_i64),
    );
    add_session_compatibility_ledger_markers(source);
}

fn add_session_compatibility_ledger_markers(source: &mut CurrentMigrationSource) {
    // Preserve released ledger identities without materializing superseded compatibility tables.
    add_migration(
        source,
        "028_session_compatibility_state",
        update("session_storage_contract")
            .value(
                "contract_id",
                switchy::database::query::identifier("contract_id"),
            )
            .where_eq("contract_id", 0_i64)
            .where_eq("contract_id", 1_i64),
        update("session_storage_contract")
            .value(
                "contract_id",
                switchy::database::query::identifier("contract_id"),
            )
            .where_eq("contract_id", 0_i64)
            .where_eq("contract_id", 1_i64),
    );
    add_migration(
        source,
        "029_session_compatibility_issues",
        update("session_storage_contract")
            .value(
                "contract_id",
                switchy::database::query::identifier("contract_id"),
            )
            .where_eq("contract_id", 0_i64)
            .where_eq("contract_id", 1_i64),
        update("session_storage_contract")
            .value(
                "contract_id",
                switchy::database::query::identifier("contract_id"),
            )
            .where_eq("contract_id", 0_i64)
            .where_eq("contract_id", 1_i64),
    );
}

fn add_migration(
    source: &mut CurrentMigrationSource,
    id: &str,
    up: impl switchy::database::Executable + 'static,
    down: impl switchy::database::Executable + 'static,
) {
    source.add_migration(CodeMigration::new(
        id.to_string(),
        Box::new(up),
        Some(Box::new(down)),
    ));
}
#[derive(Debug)]
struct InitializeStorageContract;

#[async_trait::async_trait]
impl Executable for InitializeStorageContract {
    async fn execute(&self, db: &dyn Database) -> Result<(), DatabaseError> {
        if db
            .select("session_storage_contract")
            .where_eq("contract_id", 1_i64)
            .execute_first(db)
            .await?
            .is_none()
        {
            db.insert("session_storage_contract")
                .value("contract_id", 1_i64)
                .value("schema_version", 1_i64)
                .value("writer_epoch", 1_i64)
                .value("updated_by_build", DatabaseValue::Null)
                .execute(db)
                .await?;
        }
        Ok(())
    }
}

#[derive(Debug)]
struct InitializeOccupancy;

#[async_trait::async_trait]
impl Executable for InitializeOccupancy {
    async fn execute(&self, db: &dyn Database) -> Result<(), DatabaseError> {
        create_table("context_occupancy_projection")
            .if_not_exists(true)
            .column(schema_column("projection_id", DataType::Int, false))
            .column(schema_column("schema_version", DataType::Int, false))
            .column(schema_column("context_epoch", DataType::Int, false))
            .column(schema_column("occupancy_json", DataType::Text, true))
            .primary_key("projection_id")
            .execute(db)
            .await?;
        if db
            .select("context_occupancy_projection")
            .where_eq("projection_id", 1_i64)
            .execute_first(db)
            .await?
            .is_none()
        {
            let row = db
                .select("events")
                .project_as(
                    switchy::database::query::max(switchy::database::query::identifier(
                        "event_seq",
                    )),
                    "sequence",
                )
                .where_in(
                    "event_type",
                    vec![
                        "model_changed",
                        "context_compacted",
                        "provider_context_compacted",
                    ],
                )
                .execute_first(db)
                .await?;
            let sequence = row
                .and_then(|row| row.get("sequence"))
                .filter(|v| *v != DatabaseValue::Null)
                .unwrap_or(DatabaseValue::Int64(0));
            db.insert("context_occupancy_projection")
                .value("projection_id", 1_i64)
                .value("schema_version", 1_i64)
                .value("context_epoch", sequence)
                .value("occupancy_json", DatabaseValue::Null)
                .execute(db)
                .await?;
        }
        if db
            .select("projection_checkpoints")
            .where_eq("projection_name", "context_occupancy")
            .execute_first(db)
            .await?
            .is_none()
        {
            let row = db
                .select("events")
                .project_as(
                    switchy::database::query::max(switchy::database::query::identifier(
                        "event_seq",
                    )),
                    "sequence",
                )
                .execute_first(db)
                .await?;
            let sequence = row
                .and_then(|row| row.get("sequence"))
                .filter(|v| *v != DatabaseValue::Null)
                .unwrap_or(DatabaseValue::Int64(0));
            db.insert("projection_checkpoints")
                .value("projection_name", "context_occupancy")
                .value("last_event_seq", sequence)
                .value("projection_version", 1_i64)
                .value("updated_at_ms", 0_i64)
                .execute(db)
                .await?;
        }
        Ok(())
    }
}

fn schema_column(name: &str, data_type: DataType, nullable: bool) -> Column {
    Column {
        name: name.into(),
        data_type,
        nullable,
        auto_increment: false,
        default: None,
    }
}
