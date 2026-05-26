#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! `OpenCode` `SQLite` session import provider plugin.

use bcode_config::SessionImportPathMode;
use bcode_plugin_sdk::prelude::*;
use bcode_session_import::{
    DiscoverImportableSessionsRequest, DiscoverImportableSessionsResponse, ImportSourceInfo,
    ImportWarning, ImportableSession, ImportableSessionEvent, ImportableSessionEventKind,
    ImportableSessionStatus, ImportableSessionSummary, ListImportSourcesResponse,
    LoadImportableSessionRequest, OP_DISCOVER_IMPORTABLE_SESSIONS, OP_LIST_IMPORT_SOURCES,
    OP_LOAD_IMPORTABLE_SESSION, SESSION_IMPORT_INTERFACE_ID,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension as _, Row};
use serde_json::Value;
use std::path::{Path, PathBuf};

const MANIFEST: &str = include_str!("../bcode-plugin.toml");
const SOURCE_ID: &str = "opencode";
const SOURCE_DISPLAY_NAME: &str = "OpenCode";
const TITLE_MAX_CHARS: usize = 120;

/// `OpenCode` session import provider plugin.
#[derive(Default)]
pub struct OpenCodeSessionImportPlugin;

impl RustPlugin for OpenCodeSessionImportPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id != SESSION_IMPORT_INTERFACE_ID {
            return ServiceResponse::error(
                "unsupported_interface",
                "unsupported session import service interface",
            );
        }
        match context.request.operation.as_str() {
            OP_LIST_IMPORT_SOURCES => json_response(&ListImportSourcesResponse {
                sources: vec![ImportSourceInfo {
                    source_id: SOURCE_ID.to_string(),
                    display_name: SOURCE_DISPLAY_NAME.to_string(),
                    description: Some("OpenCode SQLite session history".to_string()),
                }],
            }),
            OP_DISCOVER_IMPORTABLE_SESSIONS => discover_sessions(&context.request),
            OP_LOAD_IMPORTABLE_SESSION => load_session(&context.request),
            _ => ServiceResponse::error(
                "unsupported_operation",
                "unsupported session import operation",
            ),
        }
    }
}

fn discover_sessions(request: &ServiceRequest) -> ServiceResponse {
    if !opencode_import_enabled() {
        return json_response(&DiscoverImportableSessionsResponse {
            sessions: Vec::new(),
        });
    }
    let request = match request.payload_json::<DiscoverImportableSessionsRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    let sessions = discover_opencode_sessions(
        request.working_directory.as_deref(),
        request.include_diagnostics,
    );
    json_response(&DiscoverImportableSessionsResponse { sessions })
}

fn load_session(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<LoadImportableSessionRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    if request.source_id != SOURCE_ID {
        return ServiceResponse::error("unsupported_source", "unsupported import source");
    }
    match load_opencode_session(&request.locator, &request.external_session_id) {
        Ok(session) => json_response(&session),
        Err(error) => ServiceResponse::error("load_failed", error.to_string()),
    }
}

fn discover_opencode_sessions(
    working_directory: Option<&Path>,
    include_diagnostics: bool,
) -> Vec<ImportableSessionSummary> {
    let mut sessions = Vec::new();
    let paths = opencode_database_paths();
    if paths.is_empty() && include_diagnostics {
        sessions.push(diagnostic_summary(
            "opencode:no-paths",
            "No OpenCode database paths are configured or present",
        ));
    }
    for path in paths {
        if !path.exists() {
            if include_diagnostics {
                sessions.push(diagnostic_summary(
                    format!("opencode:{}", path.display()),
                    format!("OpenCode database not found: {}", path.display()),
                ));
            }
            continue;
        }
        match discover_database_sessions(&path, working_directory) {
            Ok(mut discovered) => sessions.append(&mut discovered),
            Err(error) => {
                if include_diagnostics {
                    sessions.push(unavailable_summary(
                        format!("opencode:{}", path.display()),
                        format!(
                            "Could not scan OpenCode database {}: {error}",
                            path.display()
                        ),
                    ));
                }
            }
        }
    }
    sessions.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| right.created_at_ms.cmp(&left.created_at_ms))
            .then_with(|| left.external_session_id.cmp(&right.external_session_id))
    });
    sessions
}

fn discover_database_sessions(
    path: &Path,
    working_directory: Option<&Path>,
) -> Result<Vec<ImportableSessionSummary>, rusqlite::Error> {
    let connection = open_read_only(path)?;
    let mut statement = connection.prepare(
        "select s.id, s.title, s.directory, s.time_created, s.time_updated, \
         count(m.id) as message_count \
         from session s left join message m on m.session_id = s.id \
         where s.time_archived is null \
         group by s.id \
         order by s.time_updated desc",
    )?;
    let summaries = statement.query_map([], |row| summary_from_row(path, row))?;
    let mut sessions = Vec::new();
    for summary in summaries {
        let summary = summary?;
        if let Some(working_directory) = working_directory
            && summary.working_directory.as_deref() != Some(working_directory)
        {
            continue;
        }
        sessions.push(summary);
    }
    Ok(sessions)
}

fn summary_from_row(
    path: &Path,
    row: &Row<'_>,
) -> Result<ImportableSessionSummary, rusqlite::Error> {
    let external_session_id: String = row.get(0)?;
    let title: String = row.get(1)?;
    let directory: String = row.get(2)?;
    let created_at_ms = i64_to_u64(row.get(3)?);
    let updated_at_ms = i64_to_u64(row.get(4)?);
    let message_count = i64_to_u64(row.get(5)?);
    Ok(ImportableSessionSummary {
        source_id: SOURCE_ID.to_string(),
        source_display_name: SOURCE_DISPLAY_NAME.to_string(),
        external_session_id,
        locator: path.to_string_lossy().into_owned(),
        title: clean_title(&title),
        working_directory: Some(PathBuf::from(directory)),
        created_at_ms,
        updated_at_ms,
        message_count,
        status: ImportableSessionStatus::Available,
        warnings: Vec::new(),
    })
}

#[allow(clippy::too_many_lines)]
fn load_opencode_session(
    locator: &str,
    external_session_id: &str,
) -> Result<ImportableSession, rusqlite::Error> {
    let path = PathBuf::from(locator);
    let connection = open_read_only(&path)?;
    let summary = load_summary(&connection, &path, external_session_id)?;
    let mut events = Vec::new();
    let mut warnings = Vec::new();
    let mut malformed_messages = 0_u64;
    let mut malformed_parts = 0_u64;
    let mut unknown_parts = 0_u64;

    let mut statement = connection.prepare(
        "select m.id, m.time_created, m.data, p.id, p.time_created, p.data \
         from message m left join part p on p.message_id = m.id \
         where m.session_id = ?1 \
         order by m.time_created, m.id, p.time_created, p.id",
    )?;
    let rows = statement.query_map([external_session_id], |row| {
        Ok(MessagePartRow {
            message_id: row.get(0)?,
            message_time: row.get(1)?,
            message_data: row.get(2)?,
            part_id: row.get(3)?,
            part_time: row.get(4)?,
            part_data: row.get(5)?,
        })
    })?;

    let mut current_message_id: Option<String> = None;
    let mut current_message: Option<Value> = None;
    let mut emitted_message = false;
    for row in rows {
        let row = row?;
        if current_message_id.as_deref() != Some(row.message_id.as_str()) {
            if let Some(message) = current_message.take()
                && !emitted_message
            {
                emit_message_fallback(&mut events, &message, current_message_id.as_deref());
            }
            current_message_id = Some(row.message_id.clone());
            current_message = serde_json::from_str::<Value>(&row.message_data).map_or_else(
                |_| {
                    malformed_messages = malformed_messages.saturating_add(1);
                    None
                },
                Some,
            );
            emitted_message = false;
        }
        if let (Some(part_id), Some(part_data)) = (row.part_id.as_deref(), row.part_data.as_deref())
        {
            let Ok(part) = serde_json::from_str::<Value>(part_data) else {
                malformed_parts = malformed_parts.saturating_add(1);
                continue;
            };
            if emit_part(
                &mut events,
                &part,
                part_id,
                row.part_time.or(Some(row.message_time)),
            ) {
                emitted_message = true;
            } else {
                unknown_parts = unknown_parts.saturating_add(1);
            }
        }
    }
    if let Some(message) = current_message
        && !emitted_message
    {
        emit_message_fallback(&mut events, &message, current_message_id.as_deref());
    }

    add_count_warning(
        &mut warnings,
        "malformed_messages",
        "malformed OpenCode message rows were skipped",
        malformed_messages,
    );
    add_count_warning(
        &mut warnings,
        "malformed_parts",
        "malformed OpenCode part rows were skipped",
        malformed_parts,
    );
    add_count_warning(
        &mut warnings,
        "unknown_parts",
        "unknown OpenCode part rows were skipped",
        unknown_parts,
    );

    Ok(ImportableSession {
        summary,
        events,
        warnings,
    })
}

fn load_summary(
    connection: &Connection,
    path: &Path,
    external_session_id: &str,
) -> Result<ImportableSessionSummary, rusqlite::Error> {
    connection
        .query_row(
            "select s.id, s.title, s.directory, s.time_created, s.time_updated, \
             count(m.id) as message_count \
             from session s left join message m on m.session_id = s.id \
             where s.id = ?1 group by s.id",
            [external_session_id],
            |row| summary_from_row(path, row),
        )
        .optional()?
        .ok_or(rusqlite::Error::QueryReturnedNoRows)
}

#[derive(Debug)]
struct MessagePartRow {
    message_id: String,
    message_time: i64,
    message_data: String,
    part_id: Option<String>,
    part_time: Option<i64>,
    part_data: Option<String>,
}

fn emit_message_fallback(
    events: &mut Vec<ImportableSessionEvent>,
    message: &Value,
    message_id: Option<&str>,
) {
    let role = message.get("role").and_then(Value::as_str);
    let timestamp_ms = timestamp_from_message(message);
    if let Some(model_event) = maybe_model_changed(message, message_id, timestamp_ms) {
        events.push(model_event);
    }
    if let Some(agent_id) = message
        .get("agent")
        .and_then(Value::as_str)
        .filter(|agent| !agent.is_empty())
    {
        events.push(import_event(
            message_id,
            timestamp_ms,
            ImportableSessionEventKind::AgentChanged {
                agent_id: agent_id.to_string(),
            },
        ));
    }
    let Some(text) = message
        .get("text")
        .or_else(|| message.get("content"))
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
    else {
        return;
    };
    let kind = match role {
        Some("assistant") => ImportableSessionEventKind::AssistantMessage {
            text: text.to_string(),
        },
        Some("system") => ImportableSessionEventKind::SystemMessage {
            text: text.to_string(),
        },
        _ => ImportableSessionEventKind::UserMessage {
            text: text.to_string(),
        },
    };
    events.push(import_event(message_id, timestamp_ms, kind));
}

#[allow(clippy::too_many_lines)]
fn emit_part(
    events: &mut Vec<ImportableSessionEvent>,
    part: &Value,
    part_id: &str,
    timestamp: Option<i64>,
) -> bool {
    let timestamp_ms = timestamp.and_then(i64_to_u64);
    match part.get("type").and_then(Value::as_str) {
        Some("text") => {
            let Some(text) = part
                .get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
            else {
                return true;
            };
            events.push(import_event(
                Some(part_id),
                timestamp_ms,
                ImportableSessionEventKind::AssistantMessage {
                    text: text.to_string(),
                },
            ));
            true
        }
        Some("reasoning") => {
            let Some(text) = part
                .get("text")
                .or_else(|| part.get("thinking"))
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
            else {
                return true;
            };
            events.push(import_event(
                Some(part_id),
                timestamp_ms,
                ImportableSessionEventKind::AssistantReasoningMessage {
                    text: text.to_string(),
                },
            ));
            true
        }
        Some("tool") => {
            let call_id = string_field(part, "callID")
                .or_else(|| string_field(part, "callId"))
                .or_else(|| string_field(part, "id"))
                .unwrap_or_else(|| part_id.to_string());
            let tool_name = string_field(part, "tool").unwrap_or_else(|| "unknown".to_string());
            let state = part.get("state");
            let arguments_json = state
                .and_then(|state| state.get("input"))
                .or_else(|| part.get("input"))
                .map_or_else(|| "{}".to_string(), ToString::to_string);
            events.push(import_event(
                Some(part_id),
                timestamp_ms,
                ImportableSessionEventKind::ToolCallRequested {
                    tool_call_id: call_id.clone(),
                    tool_name,
                    arguments_json,
                },
            ));
            if let Some(output) = state
                .and_then(|state| state.get("output"))
                .or_else(|| part.get("output"))
            {
                let result = output
                    .as_str()
                    .map_or_else(|| output.to_string(), ToString::to_string);
                let is_error = state
                    .and_then(|state| state.get("status"))
                    .and_then(Value::as_str)
                    .is_some_and(|status| matches!(status, "error" | "failed"));
                events.push(import_event(
                    Some(part_id),
                    timestamp_ms,
                    ImportableSessionEventKind::ToolCallFinished {
                        tool_call_id: call_id,
                        result,
                        is_error,
                    },
                ));
            }
            true
        }
        Some("step-finish") => {
            if let Some(usage) = usage_event(part, Some(part_id), timestamp_ms) {
                events.push(usage);
            }
            true
        }
        Some("agent") => {
            if let Some(agent_id) =
                string_field(part, "agent").or_else(|| string_field(part, "name"))
            {
                events.push(import_event(
                    Some(part_id),
                    timestamp_ms,
                    ImportableSessionEventKind::AgentChanged { agent_id },
                ));
            }
            true
        }
        Some("snapshot") => true,
        Some(_) | None => false,
    }
}

fn maybe_model_changed(
    message: &Value,
    message_id: Option<&str>,
    timestamp_ms: Option<u64>,
) -> Option<ImportableSessionEvent> {
    let provider = string_field(message, "providerID").or_else(|| {
        message
            .pointer("/model/providerID")
            .and_then(Value::as_str)
            .map(ToString::to_string)
    });
    let model = string_field(message, "modelID").or_else(|| {
        message
            .pointer("/model/modelID")
            .and_then(Value::as_str)
            .map(ToString::to_string)
    });
    provider.zip(model).map(|(provider, model)| {
        import_event(
            message_id,
            timestamp_ms,
            ImportableSessionEventKind::ModelChanged { provider, model },
        )
    })
}

fn usage_event(
    value: &Value,
    event_id: Option<&str>,
    timestamp_ms: Option<u64>,
) -> Option<ImportableSessionEvent> {
    let tokens = value.get("tokens")?;
    Some(import_event(
        event_id,
        timestamp_ms,
        ImportableSessionEventKind::ModelUsage {
            input_tokens: u64_to_u32(tokens.get("input").and_then(Value::as_u64)),
            output_tokens: u64_to_u32(tokens.get("output").and_then(Value::as_u64)),
            total_tokens: u64_to_u32(tokens.get("total").and_then(Value::as_u64)),
            cached_input_tokens: u64_to_u32(tokens.pointer("/cache/read").and_then(Value::as_u64)),
            cache_write_input_tokens: u64_to_u32(
                tokens.pointer("/cache/write").and_then(Value::as_u64),
            ),
            reasoning_tokens: u64_to_u32(tokens.get("reasoning").and_then(Value::as_u64)),
        },
    ))
}

fn timestamp_from_message(message: &Value) -> Option<u64> {
    message
        .pointer("/time/created")
        .and_then(Value::as_i64)
        .and_then(i64_to_u64)
}

fn import_event(
    external_event_id: Option<&str>,
    timestamp_ms: Option<u64>,
    kind: ImportableSessionEventKind,
) -> ImportableSessionEvent {
    ImportableSessionEvent {
        external_event_id: external_event_id.map(ToString::to_string),
        timestamp_ms,
        kind,
    }
}

fn opencode_import_enabled() -> bool {
    bcode_config::load_config().map_or(true, |config| {
        config.session_import.enabled && config.session_import.opencode.enabled
    })
}

fn opencode_database_paths() -> Vec<PathBuf> {
    let config = bcode_config::load_config().unwrap_or_default();
    let opencode = config.session_import.opencode;
    let mut paths = Vec::new();
    if matches!(
        opencode.path_mode,
        SessionImportPathMode::DefaultsOnly | SessionImportPathMode::DefaultsAndCustom
    ) && let Some(home) = std::env::var_os("HOME")
    {
        let share = PathBuf::from(home).join(".local/share/opencode");
        paths.push(share.join("opencode.db"));
        paths.push(share.join("opencode-stable.db"));
    }
    if matches!(
        opencode.path_mode,
        SessionImportPathMode::CustomOnly | SessionImportPathMode::DefaultsAndCustom
    ) {
        paths.extend(opencode.paths);
    }
    dedupe_paths(paths)
}

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut deduped = Vec::new();
    for path in paths {
        if !deduped.iter().any(|existing| existing == &path) {
            deduped.push(path);
        }
    }
    deduped
}

fn open_read_only(path: &Path) -> Result<Connection, rusqlite::Error> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
}

fn clean_title(title: &str) -> Option<String> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut title = trimmed.chars().take(TITLE_MAX_CHARS).collect::<String>();
    if trimmed.chars().count() > TITLE_MAX_CHARS {
        title.push('…');
    }
    Some(title)
}

fn diagnostic_summary(
    external_session_id: impl Into<String>,
    message: impl Into<String>,
) -> ImportableSessionSummary {
    let external_session_id = external_session_id.into();
    ImportableSessionSummary {
        source_id: SOURCE_ID.to_string(),
        source_display_name: SOURCE_DISPLAY_NAME.to_string(),
        external_session_id: external_session_id.clone(),
        locator: external_session_id,
        title: Some(message.into()),
        working_directory: None,
        created_at_ms: None,
        updated_at_ms: None,
        message_count: None,
        status: ImportableSessionStatus::Diagnostic,
        warnings: Vec::new(),
    }
}

fn unavailable_summary(
    external_session_id: impl Into<String>,
    message: impl Into<String>,
) -> ImportableSessionSummary {
    let mut summary = diagnostic_summary(external_session_id, message);
    summary.status = ImportableSessionStatus::Unavailable;
    summary
}

fn add_count_warning(
    warnings: &mut Vec<ImportWarning>,
    code: &'static str,
    message: &'static str,
    count: u64,
) {
    if count > 0 {
        warnings.push(ImportWarning::counted(code, message, count));
    }
}

fn string_field(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn i64_to_u64(value: i64) -> Option<u64> {
    u64::try_from(value).ok()
}

fn u64_to_u32(value: Option<u64>) -> Option<u32> {
    value.and_then(|value| u32::try_from(value).ok())
}

fn json_response<T: serde::Serialize>(payload: &T) -> ServiceResponse {
    ServiceResponse::json(payload)
        .unwrap_or_else(|error| ServiceResponse::error("serialization_failed", error.to_string()))
}

fn invalid_request(error: &serde_json::Error) -> ServiceResponse {
    ServiceResponse::error("invalid_request", error.to_string())
}

export_plugin!(OpenCodeSessionImportPlugin, MANIFEST);

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(
        OpenCodeSessionImportPlugin,
        include_str!("../bcode-plugin.toml")
    )
}
