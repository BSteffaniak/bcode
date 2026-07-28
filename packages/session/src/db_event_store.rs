//! Current canonical event-row reads and writes.

use crate::db::{SessionDbError, SessionDbResult};
use crate::persisted::{decode_session_event, encode_session_event};
use bcode_session_models::{SessionEvent, SessionEventKind, SessionId};
use switchy::database::{Database, DatabaseValue, query::SortDirection};

pub const fn event_kind_name(kind: &SessionEventKind) -> &'static str {
    match kind {
        SessionEventKind::SessionCreated { .. } => "session_created",
        SessionEventKind::ClientAttached { .. } => "client_attached",
        SessionEventKind::ClientDetached { .. } => "client_detached",
        SessionEventKind::UserMessage { .. } => "user_message",
        SessionEventKind::AssistantDelta { .. } => "assistant_delta",
        SessionEventKind::AssistantMessage { .. } => "assistant_message",
        SessionEventKind::AssistantResponseSegment { .. } => "assistant_response_segment",
        SessionEventKind::ToolCallRequested { .. } => "tool_call_requested",
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
        SessionEventKind::ToolInvocationLifecycle { .. } => "tool_invocation_lifecycle",
        SessionEventKind::ToolInvocationResultRecorded { .. } => "tool_invocation_result_recorded",
        SessionEventKind::ToolContribution { .. } => "tool_contribution",
        SessionEventKind::ToolContributionPlaced { .. } => "tool_contribution_placed",
        SessionEventKind::ToolExchangeRequested { .. } => "tool_exchange_requested",
        SessionEventKind::ToolExchangeResolved { .. } => "tool_exchange_resolved",
        SessionEventKind::WorkingDirectoryChanged { .. } => "working_directory_changed",
        SessionEventKind::SessionImported { .. } => "session_imported",
        SessionEventKind::SessionForked { .. } => "session_forked",
        SessionEventKind::ExecutionSessionCreated { .. } => "execution_session_created",
        SessionEventKind::AssistantReasoningActivity { .. } => "assistant_reasoning_activity",
        SessionEventKind::RalphLifecycle { .. } => "ralph_lifecycle",
        SessionEventKind::PluginStatusNote { .. } => "plugin_status_note",
        SessionEventKind::InertHistory { .. } => "inert_history",
    }
}

pub const fn event_created_at_ms(event: &SessionEvent) -> u64 {
    event.timestamp_ms
}

pub fn seq_to_value(sequence: u64) -> DatabaseValue {
    DatabaseValue::Int64(i64::try_from(sequence).unwrap_or(i64::MAX))
}

pub async fn insert_event(
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

pub async fn strict_events(db: &dyn Database) -> SessionDbResult<Vec<SessionEvent>> {
    let rows = db
        .select("events")
        .columns(&["payload"])
        .sort("event_seq", SortDirection::Asc)
        .execute(db)
        .await?;
    rows.into_iter()
        .map(|row| decode_session_event(&required_string(&row, "payload")?).map_err(Into::into))
        .collect()
}

pub fn strict_event_from_row(
    row: &switchy::database::Row,
    session_id: SessionId,
) -> SessionDbResult<SessionEvent> {
    let event_seq = required_non_negative_u64(row, "event_seq")?;
    let payload = required_string(row, "payload")?;
    let event = decode_session_event(&payload)?;
    if event.sequence != event_seq {
        return Err(SessionDbError::InvalidCanonicalSequence {
            expected: event_seq,
            actual: event.sequence,
        });
    }
    if event.session_id != session_id {
        return Err(SessionDbError::InvalidRow {
            column: "events.session_id".to_owned(),
        });
    }
    Ok(event)
}

pub async fn last_sequence(db: &dyn Database) -> SessionDbResult<Option<u64>> {
    let row = db
        .query_raw("SELECT MAX(event_seq) AS event_seq FROM events")
        .await?
        .into_iter()
        .next();
    let value = row
        .as_ref()
        .and_then(|row| row.get("event_seq"))
        .and_then(|value| value.as_i64());
    match value {
        Some(value) if value.is_negative() => Err(SessionDbError::InvalidRow {
            column: "event_seq".to_owned(),
        }),
        Some(value) => Ok(Some(value.cast_unsigned())),
        None => Ok(None),
    }
}

fn required_string(row: &switchy::database::Row, column: &str) -> SessionDbResult<String> {
    row.get(column)
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .ok_or_else(|| SessionDbError::InvalidRow {
            column: column.to_owned(),
        })
}

fn required_non_negative_u64(row: &switchy::database::Row, column: &str) -> SessionDbResult<u64> {
    let value = row
        .get(column)
        .and_then(|value| value.as_i64())
        .ok_or_else(|| SessionDbError::InvalidRow {
            column: column.to_owned(),
        })?;
    if value.is_negative() {
        return Err(SessionDbError::InvalidRow {
            column: column.to_owned(),
        });
    }
    Ok(value.cast_unsigned())
}
