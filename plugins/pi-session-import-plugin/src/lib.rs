#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Pi JSONL session import provider plugin.

use bcode_config::SessionImportPathMode;
use bcode_plugin_sdk::prelude::*;
use bcode_session_import::{
    DiscoverImportableSessionsRequest, DiscoverImportableSessionsResponse, ImportSourceInfo,
    ImportWarning, ImportableSession, ImportableSessionEvent, ImportableSessionEventKind,
    ImportableSessionSummary, ListImportSourcesResponse, LoadImportableSessionRequest,
    OP_DISCOVER_IMPORTABLE_SESSIONS, OP_LIST_IMPORT_SOURCES, OP_LOAD_IMPORTABLE_SESSION,
    SESSION_IMPORT_INTERFACE_ID,
};
use serde_json::Value;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MANIFEST: &str = include_str!("../bcode-plugin.toml");
const SOURCE_ID: &str = "pi";
const SOURCE_DISPLAY_NAME: &str = "Pi";
const DISCOVERY_LINE_BUDGET: usize = 2_000;
const TITLE_MAX_CHARS: usize = 120;

/// Pi session import provider plugin.
#[derive(Default)]
pub struct PiSessionImportPlugin;

impl RustPlugin for PiSessionImportPlugin {
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
                    description: Some("Pi JSONL session history".to_string()),
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
    if !pi_import_enabled() {
        return json_response(&DiscoverImportableSessionsResponse {
            sessions: Vec::new(),
        });
    }
    let request = match request.payload_json::<DiscoverImportableSessionsRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    match discover_pi_sessions(request.working_directory.as_deref()) {
        Ok(sessions) => json_response(&DiscoverImportableSessionsResponse { sessions }),
        Err(error) => ServiceResponse::error("discover_failed", error.to_string()),
    }
}

fn load_session(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<LoadImportableSessionRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    if request.source_id != SOURCE_ID {
        return ServiceResponse::error("unsupported_source", "unsupported import source");
    }
    match load_pi_session(&PathBuf::from(request.locator)) {
        Ok(session) => json_response(&session),
        Err(error) => ServiceResponse::error("load_failed", error.to_string()),
    }
}

fn discover_pi_sessions(
    working_directory: Option<&Path>,
) -> Result<Vec<ImportableSessionSummary>, std::io::Error> {
    let mut sessions = Vec::new();
    for root in configured_session_roots() {
        if !root.exists() {
            continue;
        }
        discover_root_sessions(&root, working_directory, &mut sessions)?;
    }
    sessions.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| right.created_at_ms.cmp(&left.created_at_ms))
            .then_with(|| left.external_session_id.cmp(&right.external_session_id))
    });
    Ok(sessions)
}

fn discover_root_sessions(
    root: &Path,
    working_directory: Option<&Path>,
    sessions: &mut Vec<ImportableSessionSummary>,
) -> Result<(), std::io::Error> {
    for entry in fs::read_dir(root)? {
        let project_dir = entry?.path();
        if !project_dir.is_dir()
            || project_dir.file_name().and_then(|name| name.to_str()) == Some("subagent-artifacts")
        {
            continue;
        }
        for file in fs::read_dir(project_dir)? {
            let path = file?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(summary) = read_summary(&path) else {
                continue;
            };
            if let Some(working_directory) = working_directory
                && summary.working_directory.as_deref() != Some(working_directory)
            {
                continue;
            }
            sessions.push(summary);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn load_pi_session(path: &Path) -> Result<ImportableSession, std::io::Error> {
    let summary = read_summary(path)?;
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    let mut warnings = Vec::new();
    let mut skipped_thinking = 0_u64;
    let mut skipped_images = 0_u64;
    let mut malformed_lines = 0_u64;

    for line in reader.lines() {
        let line = line?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            malformed_lines = malformed_lines.saturating_add(1);
            continue;
        };
        if let Some(usage_event) = maybe_usage_event(&value) {
            events.push(usage_event);
        }
        match value.get("type").and_then(Value::as_str) {
            Some("model_change") => {
                if let (Some(provider), Some(model)) = (
                    string_field(&value, "provider"),
                    string_field(&value, "modelId"),
                ) {
                    events.push(import_event(
                        &value,
                        ImportableSessionEventKind::ModelChanged { provider, model },
                    ));
                }
            }
            Some("custom") => {
                if value.get("customType").and_then(Value::as_str) == Some("agent-mode")
                    && let Some(mode) = value.pointer("/data/mode").and_then(Value::as_str)
                {
                    events.push(import_event(
                        &value,
                        ImportableSessionEventKind::AgentChanged {
                            agent_id: mode.to_string(),
                        },
                    ));
                }
            }
            Some("compaction") => {
                if let Some(summary) = string_field(&value, "summary") {
                    events.push(import_event(
                        &value,
                        ImportableSessionEventKind::ContextCompacted { summary },
                    ));
                }
            }
            Some("message") => {
                let Some(message) = value.get("message") else {
                    continue;
                };
                match message.get("role").and_then(Value::as_str) {
                    Some("user") => {
                        let (text, images) = content_text_and_images(message.get("content"));
                        skipped_images = skipped_images.saturating_add(images);
                        if !text.trim().is_empty() {
                            events.push(import_event(
                                &value,
                                ImportableSessionEventKind::UserMessage { text },
                            ));
                        }
                    }
                    Some("assistant") => {
                        if let Some(blocks) = message.get("content").and_then(Value::as_array) {
                            let mut text = String::new();
                            for block in blocks {
                                match block.get("type").and_then(Value::as_str) {
                                    Some("text") => append_text(
                                        &mut text,
                                        block
                                            .get("text")
                                            .and_then(Value::as_str)
                                            .unwrap_or_default(),
                                    ),
                                    Some("thinking") => {
                                        if let Some(thinking) = block
                                            .get("thinking")
                                            .or_else(|| block.get("text"))
                                            .and_then(Value::as_str)
                                            .filter(|thinking| !thinking.trim().is_empty())
                                        {
                                            events.push(import_event(
                                                &value,
                                                ImportableSessionEventKind::AssistantReasoningMessage {
                                                    text: thinking.to_string(),
                                                },
                                            ));
                                        }
                                        skipped_thinking = skipped_thinking.saturating_add(1);
                                    }
                                    Some("toolCall") => {
                                        if !text.trim().is_empty() {
                                            events.push(import_event(
                                                &value,
                                                ImportableSessionEventKind::AssistantMessage {
                                                    text: std::mem::take(&mut text),
                                                },
                                            ));
                                        }
                                        let tool_call_id = string_field(block, "id")
                                            .unwrap_or_else(|| "unknown".to_string());
                                        let tool_name = string_field(block, "name")
                                            .unwrap_or_else(|| "unknown".to_string());
                                        let arguments_json = block
                                            .get("arguments")
                                            .map_or_else(|| "{}".to_string(), ToString::to_string);
                                        events.push(import_event(
                                            &value,
                                            ImportableSessionEventKind::ToolCallRequested {
                                                tool_call_id,
                                                tool_name,
                                                arguments_json,
                                            },
                                        ));
                                    }
                                    Some("image") => {
                                        skipped_images = skipped_images.saturating_add(1);
                                    }
                                    Some(other) => warnings.push(ImportWarning::new(
                                        "unknown_assistant_block",
                                        format!("unknown Pi assistant content block: {other}"),
                                    )),
                                    None => {}
                                }
                            }
                            if !text.trim().is_empty() {
                                events.push(import_event(
                                    &value,
                                    ImportableSessionEventKind::AssistantMessage { text },
                                ));
                            }
                        }
                    }
                    Some("toolResult") => {
                        let (result, images) = content_text_and_images(message.get("content"));
                        skipped_images = skipped_images.saturating_add(images);
                        let tool_call_id =
                            string_field(message, "toolCallId").unwrap_or_else(|| {
                                string_field(&value, "id").unwrap_or_else(|| "unknown".to_string())
                            });
                        let is_error = message
                            .get("isError")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        events.push(import_event(
                            &value,
                            ImportableSessionEventKind::ToolCallFinished {
                                tool_call_id,
                                result,
                                is_error,
                            },
                        ));
                    }
                    Some(other) => warnings.push(ImportWarning::new(
                        "unknown_message_role",
                        format!("unknown Pi message role: {other}"),
                    )),
                    None => {}
                }
            }
            _ => {}
        }
    }

    if skipped_thinking > 0 {
        warnings.push(ImportWarning::counted(
            "imported_thinking",
            "imported Pi thinking blocks as reasoning history",
            skipped_thinking,
        ));
    }
    if skipped_images > 0 {
        warnings.push(ImportWarning::counted(
            "skipped_images",
            "skipped Pi image blocks",
            skipped_images,
        ));
    }
    if malformed_lines > 0 {
        warnings.push(ImportWarning::counted(
            "malformed_json",
            "skipped malformed Pi JSONL lines",
            malformed_lines,
        ));
    }

    Ok(ImportableSession {
        summary,
        events,
        warnings,
    })
}

fn read_summary(path: &Path) -> Result<ImportableSessionSummary, std::io::Error> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    let header = serde_json::from_str::<Value>(&first).unwrap_or(Value::Null);
    let external_session_id = string_field(&header, "id").unwrap_or_else(|| {
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("unknown")
            .to_string()
    });
    let created_at_ms =
        string_field(&header, "timestamp").and_then(|timestamp| parse_iso_timestamp_ms(&timestamp));
    let working_directory = string_field(&header, "cwd").map(PathBuf::from);
    let mut title = None;
    let mut first_user = None;

    for (index, line) in reader.lines().enumerate() {
        if index >= DISCOVERY_LINE_BUDGET || (title.is_some() && first_user.is_some()) {
            break;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line?) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("session_info") => {
                title = string_field(&value, "name").filter(|name| !name.trim().is_empty());
            }
            Some("message") if first_user.is_none() => {
                let Some(message) = value.get("message") else {
                    continue;
                };
                if message.get("role").and_then(Value::as_str) == Some("user") {
                    let (text, _) = content_text_and_images(message.get("content"));
                    let text = text.trim();
                    if !text.is_empty() && !text.starts_with('<') {
                        first_user = Some(truncate_title(text));
                    }
                }
            }
            _ => {}
        }
    }

    let updated_at_ms = fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(system_time_ms);
    Ok(ImportableSessionSummary {
        source_id: SOURCE_ID.to_string(),
        source_display_name: SOURCE_DISPLAY_NAME.to_string(),
        external_session_id,
        locator: path.to_string_lossy().into_owned(),
        title: title.or(first_user).or_else(|| {
            working_directory
                .as_ref()
                .and_then(|cwd| cwd.file_name())
                .and_then(|name| name.to_str())
                .map(ToString::to_string)
        }),
        working_directory,
        created_at_ms,
        updated_at_ms,
        message_count: None,
        warnings: Vec::new(),
    })
}

fn content_text_and_images(content: Option<&Value>) -> (String, u64) {
    match content {
        Some(Value::String(text)) => (text.clone(), 0),
        Some(Value::Array(blocks)) => {
            let mut text = String::new();
            let mut images = 0_u64;
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => append_text(
                        &mut text,
                        block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    ),
                    Some("image") => images = images.saturating_add(1),
                    _ => {}
                }
            }
            (text, images)
        }
        _ => (String::new(), 0),
    }
}

fn maybe_usage_event(source: &Value) -> Option<ImportableSessionEvent> {
    let usage = source.get("usage")?;
    let token = |name: &str| {
        usage
            .get(name)
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
    };
    let event = ImportableSessionEventKind::ModelUsage {
        input_tokens: token("inputTokens").or_else(|| token("promptTokens")),
        output_tokens: token("outputTokens").or_else(|| token("completionTokens")),
        total_tokens: token("totalTokens"),
        cached_input_tokens: token("cachedInputTokens"),
        cache_write_input_tokens: token("cacheWriteInputTokens"),
        reasoning_tokens: token("reasoningTokens"),
    };
    Some(import_event(source, event))
}

fn import_event(source: &Value, kind: ImportableSessionEventKind) -> ImportableSessionEvent {
    ImportableSessionEvent {
        external_event_id: string_field(source, "id"),
        timestamp_ms: string_field(source, "timestamp")
            .and_then(|timestamp| parse_iso_timestamp_ms(&timestamp)),
        kind,
    }
}

fn append_text(target: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    if !target.is_empty() {
        target.push('\n');
    }
    target.push_str(text);
}

fn string_field(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn truncate_title(text: &str) -> String {
    let mut title = String::new();
    for character in text.chars().take(TITLE_MAX_CHARS) {
        title.push(character);
    }
    if text.chars().count() > TITLE_MAX_CHARS {
        title.push('…');
    }
    title
}

fn default_sessions_root() -> PathBuf {
    std::env::var_os("HOME").map_or_else(
        || PathBuf::from(".pi/agent/sessions"),
        |home| PathBuf::from(home).join(".pi/agent/sessions"),
    )
}

fn pi_import_enabled() -> bool {
    bcode_config::load_config().map_or(true, |config| {
        config.session_import.enabled && config.session_import.pi.enabled
    })
}

fn configured_session_roots() -> Vec<PathBuf> {
    let Ok(config) = bcode_config::load_config() else {
        return vec![default_sessions_root()];
    };
    let pi = config.session_import.pi;
    match pi.path_mode {
        SessionImportPathMode::DefaultsOnly => vec![default_sessions_root()],
        SessionImportPathMode::CustomOnly => pi.paths,
        SessionImportPathMode::DefaultsAndCustom => {
            let mut paths = vec![default_sessions_root()];
            paths.extend(pi.paths);
            paths
        }
    }
}

fn system_time_ms(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH).ok().map(|duration| {
        duration
            .as_secs()
            .saturating_mul(1_000)
            .saturating_add(u64::from(duration.subsec_millis()))
    })
}

fn parse_iso_timestamp_ms(timestamp: &str) -> Option<u64> {
    // Fast, dependency-free parser for Pi timestamps like 2026-05-22T20:57:13.970Z.
    let year = timestamp.get(0..4)?.parse::<i32>().ok()?;
    let month = timestamp.get(5..7)?.parse::<u32>().ok()?;
    let day = timestamp.get(8..10)?.parse::<u32>().ok()?;
    let hour = timestamp.get(11..13)?.parse::<u32>().ok()?;
    let minute = timestamp.get(14..16)?.parse::<u32>().ok()?;
    let second = timestamp.get(17..19)?.parse::<u32>().ok()?;
    let millis = timestamp
        .get(20..23)
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let days = days_from_civil(year, month, day)?;
    let seconds = days
        .saturating_mul(86_400)
        .saturating_add(i64::from(hour.saturating_mul(3_600)))
        .saturating_add(i64::from(minute.saturating_mul(60)))
        .saturating_add(i64::from(second));
    u64::try_from(seconds).ok().map(|seconds| {
        seconds
            .saturating_mul(1_000)
            .saturating_add(u64::from(millis))
    })
}

fn days_from_civil(year: i32, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let year = year - i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = i32::try_from(month).ok()?;
    let day = i32::try_from(day).ok()?;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(i64::from(era * 146_097 + doe - 719_468))
}

fn json_response<T: serde::Serialize>(value: &T) -> ServiceResponse {
    ServiceResponse::json(value)
        .unwrap_or_else(|error| ServiceResponse::error("serialization_failed", error.to_string()))
}

fn invalid_request(error: &serde_json::Error) -> ServiceResponse {
    ServiceResponse::error("invalid_request", error.to_string())
}

export_plugin!(PiSessionImportPlugin, MANIFEST);

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    fn fixture_path(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join(name)
    }

    #[test]
    fn summary_prefers_session_info_name() {
        let summary =
            read_summary(&fixture_path("pi-session.jsonl")).expect("summary should parse");

        assert_eq!(summary.external_session_id, "pi-session-1");
        assert_eq!(summary.title.as_deref(), Some("Named Pi Session"));
        assert_eq!(
            summary.working_directory.as_deref(),
            Some(Path::new("/tmp/pi-project"))
        );
        assert_eq!(summary.created_at_ms, Some(1_779_483_433_970));
    }

    #[test]
    fn summary_falls_back_to_first_user_message() {
        let summary = read_summary(&fixture_path("pi-session-first-user-title.jsonl"))
            .expect("summary should parse");

        assert_eq!(
            summary.title.as_deref(),
            Some("Fallback title from first user message")
        );
    }

    #[test]
    fn load_maps_core_pi_events_and_aggregates_warnings() {
        let session =
            load_pi_session(&fixture_path("pi-session.jsonl")).expect("session should load");

        assert!(
            session
                .events
                .iter()
                .any(|event| matches!(event.kind, ImportableSessionEventKind::UserMessage { .. }))
        );
        assert!(session.events.iter().any(|event| matches!(
            event.kind,
            ImportableSessionEventKind::ToolCallRequested { .. }
        )));
        assert!(session.events.iter().any(|event| matches!(
            event.kind,
            ImportableSessionEventKind::ToolCallFinished { .. }
        )));
        assert!(
            session
                .events
                .iter()
                .any(|event| matches!(event.kind, ImportableSessionEventKind::AgentChanged { .. }))
        );
        assert!(
            session
                .warnings
                .iter()
                .any(|warning| warning.code == "imported_thinking")
        );
        assert!(
            session
                .warnings
                .iter()
                .any(|warning| warning.code == "skipped_images")
        );
        assert!(
            session
                .warnings
                .iter()
                .any(|warning| warning.code == "malformed_json")
        );
    }
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(
        PiSessionImportPlugin,
        include_str!("../bcode-plugin.toml")
    )
}
