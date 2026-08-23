//! Current projection row decoders.

use crate::db::{RuntimeWorkProjection, SessionDbResult, ToolRun, TranscriptItem};
use crate::db_row::{i64_to_u64, optional_i64, optional_string, required_i64, required_string};
use crate::db_runtime_work::{parse_runtime_work_kind, parse_runtime_work_status};
use bcode_session_models::{SessionInputHistoryEntry, SessionSummary, SessionTitleSource, WorkId};

pub fn runtime_work_from_row(
    row: &switchy::database::Row,
) -> SessionDbResult<RuntimeWorkProjection> {
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

pub fn tool_run_from_row(row: &switchy::database::Row) -> SessionDbResult<ToolRun> {
    Ok(ToolRun {
        tool_call_id: required_string(row, "tool_call_id")?,
        event_seq_start: required_i64(row, "event_seq_start").map(i64_to_u64)?,
        event_seq_end: optional_i64(row, "event_seq_end").map(i64_to_u64),
        status: required_string(row, "status")?,
        tool_name: optional_string(row, "tool_name"),
        started_at_ms: optional_i64(row, "started_at_ms").map(i64_to_u64),
        completed_at_ms: optional_i64(row, "completed_at_ms").map(i64_to_u64),
        output_bytes: optional_i64(row, "output_bytes").map(i64_to_u64),
        is_error: optional_i64(row, "is_error").map(|value| value != 0),
    })
}

pub fn session_summary_from_catalog_row(
    row: &switchy::database::Row,
) -> SessionDbResult<SessionSummary> {
    let session_id = required_string(row, "session_id")?.parse().map_err(|_| {
        crate::db::SessionDbError::InvalidRow {
            column: "session_id".to_owned(),
        }
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
        execution: None,
        location: None,
    })
}

pub fn transcript_item_from_row(row: &switchy::database::Row) -> SessionDbResult<TranscriptItem> {
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

pub fn input_history_entry_from_row(
    row: &switchy::database::Row,
) -> SessionDbResult<SessionInputHistoryEntry> {
    Ok(SessionInputHistoryEntry {
        sequence: required_i64(row, "event_seq").map(i64_to_u64)?,
        timestamp_ms: optional_i64(row, "created_at_ms").map_or(0, i64_to_u64),
        text: required_string(row, "text")?,
    })
}
