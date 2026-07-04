#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! document extraction tool plugin for Bcode.

use bcode_model_provider_runtime::ProviderRuntime;
use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID, ToolDefinition,
    ToolInvocationRequest, ToolInvocationResponse, ToolInvocationStreamEvent, ToolList,
    ToolSideEffect,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use thiserror::Error;

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_MAX_BYTES: usize = 20 * 1024 * 1024;
const MAX_BYTES: usize = 100 * 1024 * 1024;
const USER_AGENT: &str = concat!("Bcode/", env!("CARGO_PKG_VERSION"));

/// document extraction plugin.
pub struct DocumentPlugin {
    runtime: Result<ProviderRuntime, String>,
}

impl Default for DocumentPlugin {
    fn default() -> Self {
        Self {
            runtime: ProviderRuntime::new().map_err(|error| error.to_string()),
        }
    }
}

impl RustPlugin for DocumentPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match context.request.interface_id.as_str() {
            TOOL_SERVICE_INTERFACE_ID => self.invoke_tool_service(&context),
            _ => ServiceResponse::error(
                "unsupported_interface",
                "unsupported document plugin service interface",
            ),
        }
    }
}

#[derive(Clone)]
struct ProgressReporter {
    events: ServiceEventEmitter,
    tool_call_id: String,
    sequence: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl ProgressReporter {
    fn new(events: ServiceEventEmitter, tool_call_id: String) -> Self {
        Self {
            events,
            tool_call_id,
            sequence: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    fn emit(&self, message: impl Into<String>) {
        let sequence = self
            .sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .saturating_add(1);
        let event = ToolInvocationStreamEvent::Status {
            tool_call_id: self.tool_call_id.clone(),
            sequence,
            message: message.into(),
        };
        if let Ok(payload) = serde_json::to_vec(&event) {
            self.events.emit(&payload);
        }
    }
}

impl DocumentPlugin {
    fn invoke_tool_service(&self, context: &NativeServiceContext) -> ServiceResponse {
        let request = &context.request;
        match request.operation.as_str() {
            OP_LIST_TOOLS => list_tools(request),
            OP_INVOKE_TOOL => self.invoke_tool(context),
            _ => ServiceResponse::error(
                "unsupported_operation",
                "unsupported document tool service operation",
            ),
        }
    }

    fn invoke_tool(&self, context: &NativeServiceContext) -> ServiceResponse {
        let request = &context.request;
        let invocation = match request.payload_json::<ToolInvocationRequest>() {
            Ok(invocation) => invocation,
            Err(error) => return invalid_request(&error),
        };
        if context.cancellation.is_cancelled() {
            return json_response(&tool_error("document tool cancelled".to_string()));
        }
        let response = match invocation.name.as_str() {
            "document.extract" => self.invoke_extract(&invocation, context.events),
            "document.status" => invoke_status(),
            _ => ToolInvocationResponse {
                output: format!("unsupported document tool: {}", invocation.name),
                is_error: true,
                content: Vec::new(),
                full_output: None,
                host_action: None,
                result: None,
            },
        };
        json_response(&response)
    }

    fn invoke_extract(
        &self,
        invocation: &ToolInvocationRequest,
        events: ServiceEventEmitter,
    ) -> ToolInvocationResponse {
        let request = match serde_json::from_value::<ExtractRequest>(invocation.arguments.clone()) {
            Ok(request) => request,
            Err(error) => return tool_error(error.to_string()),
        };
        let runtime = match &self.runtime {
            Ok(runtime) => runtime,
            Err(error) => return tool_error(format!("document runtime unavailable: {error}")),
        };
        let artifact_dir = invocation.artifact_dir.clone();
        let progress = ProgressReporter::new(events, invocation.tool_call_id.clone());
        progress.emit("document extraction started");
        match runtime.block_on(extract_async(request, artifact_dir, Some(progress))) {
            Ok(Ok(response)) => json_tool_response(&response),
            Ok(Err(error)) => tool_error(error.to_string()),
            Err(error) => tool_error(error.to_string()),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ExtractRequest {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    path: Option<PathBuf>,
    #[serde(default)]
    max_bytes: Option<usize>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
struct ExtractResponse {
    source: String,
    content_type: String,
    artifact_kind: String,
    artifact_scope: String,
    document_path: PathBuf,
    text_path: PathBuf,
    text: String,
    truncated: bool,
    extractor: String,
    fallback_used: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct DocumentStatusResponse {
    extract: ExtractStatus,
}

#[derive(Debug, Clone, Serialize)]
struct ExtractStatus {
    available: bool,
    extractors: Vec<ExtractorStatus>,
    configured_order: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ExtractorStatus {
    name: String,
    available: bool,
    quality: String,
}

#[derive(Debug, Error)]
enum DocumentError {
    #[error("provide exactly one of url or path")]
    InvalidSource,
    #[error("url must start with http:// or https://")]
    InvalidUrl,
    #[error("document source must be a PDF for this extractor")]
    UnsupportedDocument,
    #[error("network request failed: {0}")]
    Network(#[from] reqwest::Error),
    #[error("I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("native PDF extraction failed: {0}")]
    NativeExtract(String),
    #[error("pdftotext failed with status {status}: {stderr}")]
    PdfToTextFailed { status: String, stderr: String },
}

async fn extract_async(
    request: ExtractRequest,
    artifact_dir: Option<PathBuf>,
    progress: Option<ProgressReporter>,
) -> Result<ExtractResponse, DocumentError> {
    let source = source(&request)?;
    let artifact_root = artifact_dir
        .as_deref()
        .map_or_else(default_global_document_artifact_dir, Path::to_path_buf)
        .join("documents");
    std::fs::create_dir_all(&artifact_root)?;
    let max_bytes = request
        .max_bytes
        .unwrap_or(DEFAULT_MAX_BYTES)
        .clamp(1, MAX_BYTES);
    let document_path = match &source {
        DocumentSource::Url(url) => {
            if let Some(progress) = &progress {
                progress.emit(format!("document download started: {url}"));
            }
            let path = download_document(
                url,
                &artifact_root,
                max_bytes,
                request.timeout_ms,
                progress.clone(),
            )
            .await?;
            if let Some(progress) = &progress {
                progress.emit(format!("document downloaded: {}", path.display()));
            }
            path
        }
        DocumentSource::Path(path) => {
            if let Some(progress) = &progress {
                progress.emit(format!("document source path: {}", path.display()));
            }
            path.clone()
        }
    };
    if !is_pdf_path(&document_path) {
        return Err(DocumentError::UnsupportedDocument);
    }
    if let Some(progress) = &progress {
        progress.emit("document text extraction started");
    }
    let text_path = document_path.with_extension("txt");
    let extraction = extract_pdf_text(&document_path, &text_path, progress.as_ref())?;
    let bytes = extraction.text.as_bytes();
    let truncated = bytes.len() > max_bytes;
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(max_bytes)]).to_string();
    if let Some(progress) = &progress {
        progress.emit(format!(
            "document text extracted: {} bytes{}",
            bytes.len(),
            if truncated { " (truncated)" } else { "" }
        ));
    }
    Ok(ExtractResponse {
        source: source.as_string(),
        content_type: "application/pdf".to_string(),
        artifact_kind: "document_extraction".to_string(),
        artifact_scope: if artifact_dir.is_some() {
            "session"
        } else {
            "global"
        }
        .to_string(),
        document_path,
        text_path,
        text,
        truncated,
        extractor: extraction.extractor,
        fallback_used: extraction.fallback_used,
    })
}

struct PdfExtraction {
    text: String,
    extractor: String,
    fallback_used: Option<String>,
}

fn extract_pdf_text(
    document_path: &Path,
    text_path: &Path,
    progress: Option<&ProgressReporter>,
) -> Result<PdfExtraction, DocumentError> {
    if let Some(progress) = &progress {
        progress.emit("document native extraction started");
    }
    match extract_pdf_text_native(document_path, text_path) {
        Ok(text) if meaningful_text(&text) => {
            if let Some(progress) = &progress {
                progress.emit(format!(
                    "document native extraction succeeded: {} bytes",
                    text.len()
                ));
            }
            Ok(PdfExtraction {
                text,
                extractor: "native".to_string(),
                fallback_used: None,
            })
        }
        Ok(_) | Err(_) if pdftotext_available() => {
            if let Some(progress) = &progress {
                progress.emit("document native extraction low text; trying pdftotext");
            }
            let text = extract_pdf_text_pdftotext(document_path, text_path)?;
            if let Some(progress) = &progress {
                progress.emit(format!(
                    "document pdftotext extraction succeeded: {} bytes",
                    text.len()
                ));
            }
            Ok(PdfExtraction {
                text,
                extractor: "pdftotext".to_string(),
                fallback_used: Some("native_unavailable_or_low_text".to_string()),
            })
        }
        Ok(text) => {
            if let Some(progress) = &progress {
                progress.emit(format!(
                    "document native extraction low text: {} bytes",
                    text.len()
                ));
            }
            Ok(PdfExtraction {
                text,
                extractor: "native".to_string(),
                fallback_used: Some("native_low_text".to_string()),
            })
        }
        Err(error) => Err(error),
    }
}

fn extract_pdf_text_native(
    document_path: &Path,
    text_path: &Path,
) -> Result<String, DocumentError> {
    let text = pdf_extract::extract_text(document_path)
        .map_err(|error| DocumentError::NativeExtract(error.to_string()))?;
    std::fs::write(text_path, &text)?;
    Ok(text)
}

fn meaningful_text(text: &str) -> bool {
    text.chars()
        .filter(|character| !character.is_whitespace())
        .count()
        >= 20
}

#[derive(Debug, Clone)]
enum DocumentSource {
    Url(String),
    Path(PathBuf),
}

impl DocumentSource {
    fn as_string(&self) -> String {
        match self {
            Self::Url(url) => url.clone(),
            Self::Path(path) => path.display().to_string(),
        }
    }
}

fn source(request: &ExtractRequest) -> Result<DocumentSource, DocumentError> {
    match (&request.url, &request.path) {
        (Some(url), None) => {
            if url.starts_with("http://") || url.starts_with("https://") {
                Ok(DocumentSource::Url(url.clone()))
            } else {
                Err(DocumentError::InvalidUrl)
            }
        }
        (None, Some(path)) => Ok(DocumentSource::Path(path.clone())),
        _ => Err(DocumentError::InvalidSource),
    }
}

async fn download_document(
    url: &str,
    artifact_root: &Path,
    max_bytes: usize,
    timeout_ms: Option<u64>,
    progress: Option<ProgressReporter>,
) -> Result<PathBuf, DocumentError> {
    let client = Client::builder()
        .timeout(Duration::from_millis(
            timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).max(1),
        ))
        .user_agent(USER_AGENT)
        .build()?;
    let response = client.get(url).send().await?;
    let final_url = response.url().to_string();
    if let Some(progress) = &progress {
        progress.emit(format!("document response received: {final_url}"));
    }
    let bytes = response.bytes().await?;
    if let Some(progress) = &progress {
        progress.emit(format!("document received {} bytes", bytes.len()));
    }
    let extension = if final_url.to_ascii_lowercase().contains(".pdf") {
        "pdf"
    } else {
        "bin"
    };
    let path = artifact_root.join(format!("{}.{extension}", stable_name(&final_url)));
    std::fs::write(&path, &bytes[..bytes.len().min(max_bytes)])?;
    if let Some(progress) = &progress {
        progress.emit(format!("document artifact written: {}", path.display()));
    }
    Ok(path)
}

fn extract_pdf_text_pdftotext(
    document_path: &Path,
    text_path: &Path,
) -> Result<String, DocumentError> {
    let output = Command::new("pdftotext")
        .arg("-layout")
        .arg(document_path)
        .arg(text_path)
        .output()?;
    if output.status.success() {
        std::fs::read_to_string(text_path).map_err(DocumentError::Io)
    } else {
        Err(DocumentError::PdfToTextFailed {
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }
}

fn pdftotext_available() -> bool {
    Command::new("pdftotext")
        .arg("-v")
        .output()
        .is_ok_and(|output| output.status.success() || !output.stderr.is_empty())
}

fn is_pdf_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
}

fn stable_name(value: &str) -> String {
    let mut output = String::new();
    for character in value.chars().take(120) {
        output.push(match character {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => character,
            _ => '_',
        });
    }
    output.trim_matches('_').to_string()
}

fn default_global_document_artifact_dir() -> PathBuf {
    default_state_dir().join("artifacts")
}

fn default_state_dir() -> PathBuf {
    if let Ok(path) = env::var("BCODE_STATE_DIR") {
        return PathBuf::from(path);
    }
    if let Ok(state_home) = env::var("XDG_STATE_HOME") {
        return PathBuf::from(state_home).join("bcode");
    }
    if let Ok(home) = env::var("HOME") {
        return PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("bcode");
    }
    env::temp_dir().join("bcode")
}

fn list_tools(request: &ServiceRequest) -> ServiceResponse {
    if let Err(error) = request.payload_json::<ListToolsRequest>() {
        return invalid_request(&error);
    }
    json_response(&ToolList {
        tools: vec![extract_tool_definition(), status_tool_definition()],
    })
}

fn extract_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "document.extract".to_string(),
        description: "Extract text from PDF documents using native Rust extraction with optional pdftotext fallback.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "url": { "type": "string" },
                "path": { "type": "string" },
                "max_bytes": { "type": "integer", "minimum": 1, "maximum": MAX_BYTES },
                "timeout_ms": { "type": "integer", "minimum": 1 }
            }
        }),
        side_effect: ToolSideEffect::WriteFiles,
        requires_permission: true,
        policy: bcode_tool::ToolPolicyMetadata {
            aliases: vec!["read".to_string()],
            compatibility_aliases: Vec::new(),
            capabilities: Vec::new(),
            permission_category: Some("read".to_string()),
            argument_extractors: vec![
                bcode_tool::ToolArgumentExtractor {
                    kind: bcode_tool::ToolArgumentKind::ReadPath,
                    argument: "path".to_string(),
                },
                bcode_tool::ToolArgumentExtractor {
                    kind: bcode_tool::ToolArgumentKind::Url,
                    argument: "url".to_string(),
                },
            ],
        },
        ui: bcode_tool::ToolUiMetadata {
            activity_label: Some("extracting".to_string()),
            request_visual: None,

        },
    }
}

fn status_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "document.status".to_string(),
        description: "Report available document extraction backends.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {}
        }),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: bcode_tool::ToolPolicyMetadata::default(),
        ui: bcode_tool::ToolUiMetadata::default(),
    }
}

fn invoke_status() -> ToolInvocationResponse {
    json_tool_response(&status_response())
}

fn status_response() -> DocumentStatusResponse {
    let extractors = vec![
        ExtractorStatus {
            name: "native".to_string(),
            available: true,
            quality: "built_in".to_string(),
        },
        ExtractorStatus {
            name: "pdftotext".to_string(),
            available: pdftotext_available(),
            quality: "external_optional".to_string(),
        },
    ];
    DocumentStatusResponse {
        extract: ExtractStatus {
            available: true,
            configured_order: vec!["native".to_string(), "pdftotext".to_string()],
            extractors,
        },
    }
}

fn json_response<T: Serialize>(value: &T) -> ServiceResponse {
    match ServiceResponse::json(value) {
        Ok(response) => response,
        Err(error) => ServiceResponse::error("encode_failed", error.to_string()),
    }
}

fn invalid_request(error: &serde_json::Error) -> ServiceResponse {
    ServiceResponse::error("invalid_request", error.to_string())
}

fn json_tool_response<T: Serialize>(value: &T) -> ToolInvocationResponse {
    match serde_json::to_string_pretty(value) {
        Ok(output) => ToolInvocationResponse {
            output,
            is_error: false,
            content: Vec::new(),
            full_output: None,
            host_action: None,
            result: None,
        },
        Err(error) => tool_error(error.to_string()),
    }
}

const fn tool_error(output: String) -> ToolInvocationResponse {
    ToolInvocationResponse {
        output,
        is_error: true,
        content: Vec::new(),
        full_output: None,
        host_action: None,
        result: None,
    }
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(DocumentPlugin, include_str!("../bcode-plugin.toml"))
}

bcode_plugin_sdk::export_plugin!(DocumentPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_requires_exactly_one_input() {
        assert!(
            source(&ExtractRequest {
                url: None,
                path: None,
                max_bytes: None,
                timeout_ms: None
            })
            .is_err()
        );
        assert!(
            source(&ExtractRequest {
                url: Some("https://example.com/a.pdf".to_string()),
                path: Some(PathBuf::from("a.pdf")),
                max_bytes: None,
                timeout_ms: None,
            })
            .is_err()
        );
    }

    #[test]
    fn source_accepts_http_pdf_url() {
        let result = source(&ExtractRequest {
            url: Some("https://example.com/a.pdf".to_string()),
            path: None,
            max_bytes: None,
            timeout_ms: None,
        })
        .expect("url source");
        assert_eq!(result.as_string(), "https://example.com/a.pdf");
    }

    #[test]
    fn stable_names_are_path_safe() {
        assert_eq!(
            stable_name("https://example.com/a file.pdf"),
            "https___example.com_a_file.pdf"
        );
    }

    #[test]
    fn native_extractor_is_available_by_default() {
        let status = status_response();
        assert!(status.extract.available);
        assert!(
            status
                .extract
                .extractors
                .iter()
                .any(|extractor| extractor.name == "native" && extractor.available)
        );
    }
}
