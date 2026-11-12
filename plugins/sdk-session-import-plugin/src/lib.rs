#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Import provider for conversation files written by the Bcode SDK's `LocalSessionStore`.
//!
//! Applications that embed the SDK keep conversations as `PersistedSession` JSON files. This
//! plugin turns one named file into provider-neutral import events so the host can create a
//! canonical Bcode session from it. It has no default roots and never scans directories:
//! discovery returns nothing, and loading reads exactly the file the caller names. The file is
//! only read, never changed. Tool calls become inert history.

use bcode_model::{ContentBlock, MessageRole, ModelMessage};
use bcode_plugin_sdk::prelude::*;
use bcode_session_import::{
    DiscoverImportableSessionsResponse, ImportSourceInfo, ImportWarning, ImportableSession,
    ImportableSessionEvent, ImportableSessionEventKind, ImportableSessionStatus,
    ImportableSessionSummary, ListImportSourcesResponse, LoadImportableSessionRequest,
    OP_DISCOVER_IMPORTABLE_SESSIONS, OP_LIST_IMPORT_SOURCES, OP_LOAD_IMPORTABLE_SESSION,
    SESSION_IMPORT_INTERFACE_ID,
};
use serde::Deserialize;
use std::io::Read as _;
use std::path::Path;

const MANIFEST: &str = include_str!("../bcode-plugin.toml");
/// Import source id of SDK JSON conversation files.
pub const SOURCE_ID: &str = "sdk-json";
const SOURCE_DISPLAY_NAME: &str = "Bcode SDK JSON";
/// Largest file this importer reads.
pub const MAX_SDK_SESSION_FILE_BYTES: u64 = 64 * 1024 * 1024;
const TITLE_MAX_CHARS: usize = 120;
const SUPPORTED_SCHEMA_VERSION: u32 = 1;

/// SDK JSON session import provider plugin.
#[derive(Default)]
pub struct SdkSessionImportPlugin;

impl RustPlugin for SdkSessionImportPlugin {
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
                    source_id: SOURCE_ID.to_owned(),
                    display_name: SOURCE_DISPLAY_NAME.to_owned(),
                    description: Some(
                        "PersistedSession JSON files named explicitly by the caller".to_owned(),
                    ),
                }],
            }),
            // There are no default roots: only files the caller names are imported.
            OP_DISCOVER_IMPORTABLE_SESSIONS => json_response(&DiscoverImportableSessionsResponse {
                sessions: Vec::new(),
            }),
            OP_LOAD_IMPORTABLE_SESSION => load_session(&context.request),
            _ => ServiceResponse::error(
                "unsupported_operation",
                "unsupported session import operation",
            ),
        }
    }
}

fn load_session(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<LoadImportableSessionRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    if request.source_id != SOURCE_ID {
        return ServiceResponse::error("unsupported_source", "unsupported import source");
    }
    match load_sdk_session_file(Path::new(&request.locator)) {
        Ok(session)
            if request.external_session_id.is_empty()
                || request.external_session_id == session.summary.external_session_id =>
        {
            json_response(&session)
        }
        Ok(_) => ServiceResponse::error(
            "external_session_mismatch",
            "the file holds a different session id than requested",
        ),
        Err(error) => ServiceResponse::error(error.code, error.message),
    }
}

/// Failure to read or decode a named file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdkSessionLoadError {
    /// Stable error code.
    pub code: &'static str,
    /// Human-readable detail.
    pub message: String,
}

impl SdkSessionLoadError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Deserialize)]
struct PersistedSessionFile {
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    session_id: String,
    messages: Vec<ModelMessage>,
    #[serde(default)]
    memories: Vec<PersistedMemory>,
}

#[derive(Deserialize)]
struct PersistedMemory {
    message: ModelMessage,
}

const fn default_schema_version() -> u32 {
    SUPPORTED_SCHEMA_VERSION
}

/// Read one SDK JSON file and normalize it into import events.
///
/// # Errors
///
/// * `invalid_locator` when the path is not absolute.
/// * `read_failed` when the file cannot be opened or read.
/// * `file_too_large` when the file exceeds [`MAX_SDK_SESSION_FILE_BYTES`].
/// * `invalid_session_file` when the file is not a `PersistedSession` document.
/// * `unsupported_schema` when its schema version is newer than this importer.
pub fn load_sdk_session_file(path: &Path) -> Result<ImportableSession, SdkSessionLoadError> {
    if !path.is_absolute() {
        return Err(SdkSessionLoadError::new(
            "invalid_locator",
            "the SDK session file path must be absolute",
        ));
    }
    let file = std::fs::File::open(path)
        .map_err(|error| SdkSessionLoadError::new("read_failed", error.to_string()))?;
    let mut bytes = Vec::new();
    file.take(MAX_SDK_SESSION_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| SdkSessionLoadError::new("read_failed", error.to_string()))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_SDK_SESSION_FILE_BYTES {
        return Err(SdkSessionLoadError::new(
            "file_too_large",
            format!("SDK session files are imported up to {MAX_SDK_SESSION_FILE_BYTES} bytes"),
        ));
    }
    let document: PersistedSessionFile = serde_json::from_slice(&bytes)
        .map_err(|error| SdkSessionLoadError::new("invalid_session_file", error.to_string()))?;
    if document.schema_version != SUPPORTED_SCHEMA_VERSION {
        return Err(SdkSessionLoadError::new(
            "unsupported_schema",
            format!(
                "SDK session schema {} is not supported; update Bcode",
                document.schema_version
            ),
        ));
    }
    Ok(normalize(document, path))
}

#[derive(Default)]
struct Lossy {
    images: u64,
    extensions: u64,
}

fn normalize(document: PersistedSessionFile, path: &Path) -> ImportableSession {
    let mut lossy = Lossy::default();
    let mut events = Vec::new();
    for message in &document.messages {
        push_message_events(message, &mut events, &mut lossy);
    }
    for memory in &document.memories {
        push_message_events(&memory.message, &mut events, &mut lossy);
    }
    let mut warnings = Vec::new();
    if !document.memories.is_empty() {
        warnings.push(ImportWarning::counted(
            "memory_metadata_dropped",
            "persisted memories were imported as messages; their memory metadata was not kept",
            u64::try_from(document.memories.len()).unwrap_or(u64::MAX),
        ));
    }
    if lossy.images > 0 {
        warnings.push(ImportWarning::counted(
            "images_omitted",
            "inline images were replaced by a note",
            lossy.images,
        ));
    }
    if lossy.extensions > 0 {
        warnings.push(ImportWarning::counted(
            "provider_extensions_omitted",
            "provider-specific content blocks were not imported",
            lossy.extensions,
        ));
    }
    let title = document.messages.iter().find_map(|message| {
        (message.role == MessageRole::User)
            .then(|| message_text(message))
            .filter(|text| !text.trim().is_empty())
            .map(|text| truncate_title(&text))
    });
    ImportableSession {
        summary: ImportableSessionSummary {
            source_id: SOURCE_ID.to_owned(),
            source_display_name: SOURCE_DISPLAY_NAME.to_owned(),
            external_session_id: document.session_id,
            locator: path.to_string_lossy().into_owned(),
            title,
            working_directory: None,
            created_at_ms: None,
            updated_at_ms: None,
            message_count: Some(u64::try_from(document.messages.len()).unwrap_or(u64::MAX)),
            status: ImportableSessionStatus::Available,
            warnings: warnings.clone(),
        },
        events,
        warnings,
    }
}

fn push_message_events(
    message: &ModelMessage,
    events: &mut Vec<ImportableSessionEvent>,
    lossy: &mut Lossy,
) {
    let mut text = String::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text: part } => append_text(&mut text, part),
            ContentBlock::Image { .. } => {
                lossy.images += 1;
                append_text(&mut text, "[image omitted]");
            }
            ContentBlock::ToolCall { call } => {
                flush_text(message.role, &mut text, events);
                events.push(event(ImportableSessionEventKind::ToolCallRequested {
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    arguments_json: call.arguments.to_string(),
                }));
            }
            ContentBlock::ToolResult { result } => {
                flush_text(message.role, &mut text, events);
                events.push(event(ImportableSessionEventKind::ToolCallFinished {
                    tool_call_id: result.call_id.clone(),
                    result: result.output.clone(),
                    is_error: result.is_error,
                }));
            }
            ContentBlock::ProviderExtension { .. } => lossy.extensions += 1,
            ContentBlock::CachePoint { .. } => {}
        }
    }
    flush_text(message.role, &mut text, events);
}

/// Emit accumulated text as one message event, keeping it ahead of later tool blocks.
fn flush_text(role: MessageRole, text: &mut String, events: &mut Vec<ImportableSessionEvent>) {
    if text.is_empty() {
        return;
    }
    let text = std::mem::take(text);
    events.push(event(match role {
        MessageRole::User => ImportableSessionEventKind::UserMessage { text },
        MessageRole::Assistant => ImportableSessionEventKind::AssistantMessage { text },
        MessageRole::System | MessageRole::Tool => {
            ImportableSessionEventKind::SystemMessage { text }
        }
    }));
}

const fn event(kind: ImportableSessionEventKind) -> ImportableSessionEvent {
    ImportableSessionEvent {
        external_event_id: None,
        timestamp_ms: None,
        kind,
    }
}

fn message_text(message: &ModelMessage) -> String {
    let mut text = String::new();
    for block in &message.content {
        if let ContentBlock::Text { text: part } = block {
            append_text(&mut text, part);
        }
    }
    text
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

fn truncate_title(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() <= TITLE_MAX_CHARS {
        return line.to_owned();
    }
    let mut title = line.chars().take(TITLE_MAX_CHARS - 1).collect::<String>();
    title.push('…');
    title
}

fn json_response<T: serde::Serialize>(value: &T) -> ServiceResponse {
    ServiceResponse::json(value)
        .unwrap_or_else(|error| ServiceResponse::error("serialization_failed", error.to_string()))
}

#[cfg(not(feature = "static-bundled"))]
export_plugin!(SdkSessionImportPlugin, MANIFEST);

/// Static plugin vtable for builds that bundle this plugin.
#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(SdkSessionImportPlugin, MANIFEST)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, value: &serde_json::Value) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
        path
    }

    #[test]
    fn conversation_turns_tools_and_memories_become_inert_import_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "1700000000000.json",
            &serde_json::json!({
                "session_id": "7d0b6c1e-1111-4f5c-9e6b-0123456789ab",
                "messages": [
                    {"role": "user", "content": [
                        {"type": "text", "text": "How many orders shipped today?"},
                        {"type": "image", "image": {"mime_type": "image/png", "data_base64": ""}}
                    ]},
                    {"role": "assistant", "content": [
                        {"type": "text", "text": "Let me check."},
                        {"type": "tool_call", "call": {"id": "c1", "name": "bdata.run_query", "arguments": {"sql": "select 1"}}}
                    ]},
                    {"role": "tool", "content": [
                        {"type": "tool_result", "result": {"call_id": "c1", "output": "1", "is_error": false}}
                    ]},
                    {"role": "assistant", "content": [{"type": "text", "text": "One order."}]}
                ],
                "memories": [{
                    "id": "m1",
                    "message": {"role": "system", "content": [{"type": "text", "text": "Prefers UTC."}]},
                    "relevance_millis": 500,
                    "provenance": {"source": "app"}
                }]
            }),
        );
        let session = load_sdk_session_file(&path).unwrap();
        assert_eq!(
            session.summary.external_session_id,
            "7d0b6c1e-1111-4f5c-9e6b-0123456789ab"
        );
        assert_eq!(
            session.summary.title.as_deref(),
            Some("How many orders shipped today?")
        );
        assert_eq!(session.summary.locator, path.to_string_lossy());
        let kinds = session
            .events
            .iter()
            .map(|event| event.kind.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [
                ImportableSessionEventKind::UserMessage {
                    text: "How many orders shipped today?\n[image omitted]".into()
                },
                ImportableSessionEventKind::AssistantMessage {
                    text: "Let me check.".into()
                },
                ImportableSessionEventKind::ToolCallRequested {
                    tool_call_id: "c1".into(),
                    tool_name: "bdata.run_query".into(),
                    arguments_json: r#"{"sql":"select 1"}"#.into()
                },
                ImportableSessionEventKind::ToolCallFinished {
                    tool_call_id: "c1".into(),
                    result: "1".into(),
                    is_error: false
                },
                ImportableSessionEventKind::AssistantMessage {
                    text: "One order.".into()
                },
                ImportableSessionEventKind::SystemMessage {
                    text: "Prefers UTC.".into()
                },
            ]
        );
        let codes = session
            .warnings
            .iter()
            .map(|warning| warning.code.as_str())
            .collect::<Vec<_>>();
        assert_eq!(codes, ["memory_metadata_dropped", "images_omitted"]);
    }

    #[test]
    fn unreadable_newer_or_relative_files_fail_without_changing_them() {
        let dir = tempfile::tempdir().unwrap();
        let corrupt = dir.path().join("corrupt.json");
        std::fs::write(&corrupt, b"{not json").unwrap();
        assert_eq!(
            load_sdk_session_file(&corrupt).unwrap_err().code,
            "invalid_session_file"
        );
        assert_eq!(std::fs::read(&corrupt).unwrap(), b"{not json");
        let newer = write(
            dir.path(),
            "newer.json",
            &serde_json::json!({"schema_version": 2, "session_id": "x", "messages": []}),
        );
        assert_eq!(
            load_sdk_session_file(&newer).unwrap_err().code,
            "unsupported_schema"
        );
        assert_eq!(
            load_sdk_session_file(Path::new("relative.json"))
                .unwrap_err()
                .code,
            "invalid_locator"
        );
        assert_eq!(
            load_sdk_session_file(&dir.path().join("missing.json"))
                .unwrap_err()
                .code,
            "read_failed"
        );
    }
}
