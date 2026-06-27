#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! OCR tool plugin for Bcode.

use bcode_model_provider_runtime::ProviderRuntime;
use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID, ToolDefinition,
    ToolInvocationRequest, ToolInvocationResponse, ToolInvocationStreamEvent, ToolList,
    ToolResultContent, ToolSideEffect,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::env;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const DEFAULT_ENGINE: &str = "tesseract";
const DEFAULT_LANGUAGE: &str = "eng";
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_BYTES: usize = 100 * 1024 * 1024;
const USER_AGENT: &str = concat!("Bcode/", env!("CARGO_PKG_VERSION"));

/// OCR plugin.
pub struct OcrPlugin {
    runtime: Result<ProviderRuntime, String>,
}

impl Default for OcrPlugin {
    fn default() -> Self {
        Self {
            runtime: ProviderRuntime::new().map_err(|error| error.to_string()),
        }
    }
}

impl RustPlugin for OcrPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match context.request.interface_id.as_str() {
            TOOL_SERVICE_INTERFACE_ID => self.invoke_tool_service(&context),
            _ => ServiceResponse::error(
                "unsupported_interface",
                "unsupported OCR plugin service interface",
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

impl OcrPlugin {
    fn invoke_tool_service(&self, context: &NativeServiceContext) -> ServiceResponse {
        let request = &context.request;
        match request.operation.as_str() {
            OP_LIST_TOOLS => list_tools(request),
            OP_INVOKE_TOOL => self.invoke_tool(request, context.events),
            _ => ServiceResponse::error("unsupported_operation", "unsupported tool operation"),
        }
    }

    fn invoke_tool(
        &self,
        request: &ServiceRequest,
        events: ServiceEventEmitter,
    ) -> ServiceResponse {
        let invocation = match request.payload_json::<ToolInvocationRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        let response = match invocation.name.as_str() {
            "ocr.extract" => self.invoke_extract(&invocation, events),
            "ocr.status" => invoke_status(),
            _ => ToolInvocationResponse {
                output: format!("unknown OCR tool: {}", invocation.name),
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
            Err(error) => return tool_error(format!("OCR runtime unavailable: {error}")),
        };
        let progress = ProgressReporter::new(events, invocation.tool_call_id.clone());
        progress.emit("OCR extraction started");
        match runtime.block_on(extract_async(
            request,
            invocation.artifact_dir.clone(),
            Some(progress),
        )) {
            Ok(Ok(response)) => ocr_tool_response(&response),
            Ok(Err(error)) => tool_error(error.to_string()),
            Err(error) => tool_error(error.to_string()),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ExtractRequest {
    #[serde(default)]
    path: Option<PathBuf>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    engine: Option<String>,
    #[serde(default)]
    options: Option<OcrOptions>,
    #[serde(default)]
    max_bytes: Option<usize>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
struct OcrOptions {
    #[serde(default)]
    psm: Option<u8>,
    #[serde(default)]
    oem: Option<u8>,
    #[serde(default)]
    config: Vec<String>,
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct ExtractResponse {
    text: String,
    #[serde(skip)]
    full_text: String,
    source: SourceResponse,
    engine: String,
    language: String,
    truncated: bool,
    text_bytes: usize,
    full_text_bytes: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct SourceResponse {
    path: String,
    url: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct StatusResponse {
    extract: ExtractStatus,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct ExtractStatus {
    available: bool,
    default_engine: String,
    engines: Vec<EngineStatus>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct EngineStatus {
    name: String,
    available: bool,
    version: Option<String>,
    quality: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OcrSource {
    Path(PathBuf),
    Url(String),
}

impl OcrSource {
    fn as_string(&self) -> String {
        match self {
            Self::Path(path) => path.display().to_string(),
            Self::Url(url) => url.clone(),
        }
    }
}

#[derive(Debug, Error)]
enum OcrError {
    #[error("provide exactly one of path or url")]
    InvalidSource,
    #[error("unsupported OCR engine: {0}")]
    UnsupportedEngine(String),
    #[error("unknown OCR option: {0}")]
    UnknownOption(String),
    #[error("invalid OCR option {name}: {value}")]
    InvalidOption { name: &'static str, value: u8 },
    #[error("tesseract executable was not found; install tesseract or disable bcode.ocr")]
    TesseractUnavailable,
    #[error("OCR command timed out after {0} ms")]
    Timeout(u64),
    #[error("OCR command failed with status {status}: {stderr}")]
    CommandFailed { status: String, stderr: String },
    #[error("download failed: {0}")]
    Download(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[cfg(feature = "bundled-tesseract")]
    #[error("image decoding failed: {0}")]
    Image(#[from] image::ImageError),
    #[cfg(feature = "bundled-tesseract")]
    #[error("bundled tesseract failed: {0}")]
    BundledTesseract(String),
}

async fn extract_async(
    request: ExtractRequest,
    artifact_dir: Option<PathBuf>,
    progress: Option<ProgressReporter>,
) -> Result<ExtractResponse, OcrError> {
    validate_options(request.options.as_ref())?;
    let source = source(&request)?;
    let engine = request.engine.unwrap_or_else(default_engine_name);
    if !is_supported_engine(&engine) {
        return Err(OcrError::UnsupportedEngine(engine));
    }
    let language = request
        .language
        .unwrap_or_else(|| DEFAULT_LANGUAGE.to_string());
    let timeout_ms = request.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    let max_bytes = request
        .max_bytes
        .unwrap_or(DEFAULT_MAX_BYTES)
        .clamp(1, MAX_BYTES);
    let input_path = match &source {
        OcrSource::Path(path) => path.clone(),
        OcrSource::Url(url) => {
            let artifact_root = artifact_dir
                .unwrap_or_else(default_artifact_root)
                .join("ocr");
            std::fs::create_dir_all(&artifact_root)?;
            if let Some(progress) = &progress {
                progress.emit(format!("OCR download started: {url}"));
            }
            download_source(url, &artifact_root, timeout_ms).await?
        }
    };
    if let Some(progress) = &progress {
        progress.emit(format!("OCR source path: {}", input_path.display()));
        progress.emit(format!("{engine} OCR started"));
    }
    let full_text = run_ocr_engine(
        &engine,
        &input_path,
        &language,
        request.options.as_ref(),
        timeout_ms,
    )
    .await?;
    let full_text_bytes = full_text.len();
    let truncated = full_text_bytes > max_bytes;
    let text = truncate_utf8(&full_text, max_bytes).to_string();
    Ok(ExtractResponse {
        text,
        full_text,
        source: SourceResponse {
            path: input_path.display().to_string(),
            url: matches!(source, OcrSource::Url(_)).then(|| source.as_string()),
        },
        engine,
        language,
        truncated,
        text_bytes: full_text_bytes.min(max_bytes),
        full_text_bytes,
    })
}

fn source(request: &ExtractRequest) -> Result<OcrSource, OcrError> {
    match (&request.path, &request.url) {
        (Some(path), None) => Ok(OcrSource::Path(path.clone())),
        (None, Some(url)) => Ok(OcrSource::Url(url.clone())),
        _ => Err(OcrError::InvalidSource),
    }
}

fn validate_options(options: Option<&OcrOptions>) -> Result<(), OcrError> {
    let Some(options) = options else {
        return Ok(());
    };
    if let Some(name) = options.extra.keys().next() {
        return Err(OcrError::UnknownOption(name.clone()));
    }
    if let Some(psm) = options.psm
        && psm > 13
    {
        return Err(OcrError::InvalidOption {
            name: "psm",
            value: psm,
        });
    }
    if let Some(oem) = options.oem
        && oem > 3
    {
        return Err(OcrError::InvalidOption {
            name: "oem",
            value: oem,
        });
    }
    Ok(())
}

fn default_engine_name() -> String {
    #[cfg(feature = "bundled-tesseract")]
    {
        "tesseract".to_string()
    }
    #[cfg(not(feature = "bundled-tesseract"))]
    {
        "tesseract-cli".to_string()
    }
}

fn is_supported_engine(engine: &str) -> bool {
    matches!(engine, "tesseract-cli")
        || cfg!(feature = "bundled-tesseract") && engine == "tesseract"
}

async fn run_ocr_engine(
    engine: &str,
    path: &Path,
    language: &str,
    options: Option<&OcrOptions>,
    timeout_ms: u64,
) -> Result<String, OcrError> {
    match engine {
        "tesseract-cli" => run_tesseract_cli(path, language, options, timeout_ms).await,
        #[cfg(feature = "bundled-tesseract")]
        "tesseract" => run_bundled_tesseract(path, language, options),
        _ => Err(OcrError::UnsupportedEngine(engine.to_string())),
    }
}

async fn run_tesseract_cli(
    path: &Path,
    language: &str,
    options: Option<&OcrOptions>,
    timeout_ms: u64,
) -> Result<String, OcrError> {
    let mut command = Command::new(DEFAULT_ENGINE);
    command
        .arg(path)
        .arg("stdout")
        .arg("-l")
        .arg(language)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(options) = options {
        if let Some(psm) = options.psm {
            command.arg("--psm").arg(psm.to_string());
        }
        if let Some(oem) = options.oem {
            command.arg("--oem").arg(oem.to_string());
        }
        for config in &options.config {
            command.arg("-c").arg(config);
        }
    }
    let child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            OcrError::TesseractUnavailable
        } else {
            OcrError::Io(error)
        }
    })?;
    let output = tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait_with_output())
        .await
        .map_err(|_| OcrError::Timeout(timeout_ms))??;
    if !output.status.success() {
        return Err(OcrError::CommandFailed {
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(feature = "bundled-tesseract")]
fn run_bundled_tesseract(
    path: &Path,
    language: &str,
    options: Option<&OcrOptions>,
) -> Result<String, OcrError> {
    let image = image::open(path)?.to_rgba8();
    let (width, height) = image.dimensions();
    let bytes_per_pixel = 4_i32;
    let bytes_per_line = i32::try_from(width)
        .map_err(|error| OcrError::BundledTesseract(error.to_string()))?
        .saturating_mul(bytes_per_pixel);
    let api = tesseract_rs::TesseractAPI::new();
    let oem = options.and_then(|options| options.oem).map_or(3, i32::from);
    let tessdata_dir = tessdata_dir();
    api.init_2(tessdata_dir.to_str().unwrap_or_default(), language, oem)
        .map_err(|error| OcrError::BundledTesseract(error.to_string()))?;
    if let Some(options) = options {
        if let Some(psm) = options.psm {
            api.set_page_seg_mode(tesseract_rs::TessPageSegMode::from_int(i32::from(psm)))
                .map_err(|error| OcrError::BundledTesseract(error.to_string()))?;
        }
        for config in &options.config {
            let Some((name, value)) = config.split_once('=') else {
                return Err(OcrError::BundledTesseract(format!(
                    "bundled tesseract config must use name=value syntax: {config}"
                )));
            };
            api.set_variable(name, value)
                .map_err(|error| OcrError::BundledTesseract(error.to_string()))?;
        }
    }
    api.set_image(
        image.as_raw(),
        i32::try_from(width).map_err(|error| OcrError::BundledTesseract(error.to_string()))?,
        i32::try_from(height).map_err(|error| OcrError::BundledTesseract(error.to_string()))?,
        bytes_per_pixel,
        bytes_per_line,
    )
    .map_err(|error| OcrError::BundledTesseract(error.to_string()))?;
    api.recognize()
        .map_err(|error| OcrError::BundledTesseract(error.to_string()))?;
    api.get_utf8_text()
        .map_err(|error| OcrError::BundledTesseract(error.to_string()))
}

#[cfg(feature = "bundled-tesseract")]
fn tessdata_dir() -> PathBuf {
    if let Ok(prefix) = env::var("TESSDATA_PREFIX") {
        return PathBuf::from(prefix);
    }
    if cfg!(target_os = "macos")
        && let Ok(home) = env::var("HOME")
    {
        return PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("tesseract-rs")
            .join("tessdata");
    }
    if cfg!(target_os = "windows")
        && let Ok(appdata) = env::var("APPDATA")
    {
        return PathBuf::from(appdata).join("tesseract-rs").join("tessdata");
    }
    env::var("HOME").map_or_else(
        |_| PathBuf::from("tessdata"),
        |home| PathBuf::from(home).join(".tesseract-rs").join("tessdata"),
    )
}

async fn download_source(
    url: &str,
    artifact_root: &Path,
    timeout_ms: u64,
) -> Result<PathBuf, OcrError> {
    let client = Client::builder()
        .timeout(Duration::from_millis(timeout_ms))
        .user_agent(USER_AGENT)
        .build()
        .map_err(|error| OcrError::Download(error.to_string()))?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| OcrError::Download(error.to_string()))?
        .error_for_status()
        .map_err(|error| OcrError::Download(error.to_string()))?;
    let bytes = response
        .bytes()
        .await
        .map_err(|error| OcrError::Download(error.to_string()))?;
    let path = artifact_root.join(stable_name(url));
    let mut file = tokio::fs::File::create(&path).await?;
    file.write_all(&bytes).await?;
    Ok(path)
}

fn stable_name(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn default_artifact_root() -> PathBuf {
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

fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    &text[..end]
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
        name: "ocr.extract".to_string(),
        description:
            "Extract text from images or image-like documents using the configured OCR engine."
                .to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Local path to an image or document to OCR." },
                "url": { "type": "string", "description": "Optional URL to download and OCR." },
                "language": { "type": "string", "description": "OCR language code. Defaults to eng." },
                "engine": { "type": "string", "description": "Optional OCR engine. Defaults to the plugin's configured engine." },
                "options": { "type": "object", "description": "Advanced OCR engine options. Supported keys depend on the selected engine." },
                "max_bytes": { "type": "integer", "minimum": 1, "maximum": MAX_BYTES },
                "timeout_ms": { "type": "integer", "minimum": 1 }
            }
        }),
        side_effect: ToolSideEffect::ExecuteProcess,
        requires_permission: true,
        policy: bcode_tool::ToolPolicyMetadata {
            aliases: vec!["read".to_string()],
            compatibility_aliases: Vec::new(),
            capabilities: vec!["ocr".to_string(), "read".to_string()],
            permission_category: Some("read".to_string()),
            argument_extractors: Vec::new(),
        },
        ui: bcode_tool::ToolUiMetadata::default(),
    }
}

fn status_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "ocr.status".to_string(),
        description: "Report OCR engine availability and default OCR configuration.".to_string(),
        input_schema: json!({ "type": "object", "properties": {} }),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: bcode_tool::ToolPolicyMetadata::default(),
        ui: bcode_tool::ToolUiMetadata::default(),
    }
}

fn invoke_status() -> ToolInvocationResponse {
    json_tool_response(&status_response())
}

fn status_response() -> StatusResponse {
    let engines = ocr_engine_statuses();
    StatusResponse {
        extract: ExtractStatus {
            available: engines.iter().any(|engine| engine.available),
            default_engine: default_engine_name(),
            engines,
        },
    }
}

#[cfg(feature = "bundled-tesseract")]
fn ocr_engine_statuses() -> Vec<EngineStatus> {
    vec![bundled_tesseract_status(), tesseract_cli_status()]
}

#[cfg(not(feature = "bundled-tesseract"))]
fn ocr_engine_statuses() -> Vec<EngineStatus> {
    vec![tesseract_cli_status()]
}

#[cfg(feature = "bundled-tesseract")]
fn bundled_tesseract_status() -> EngineStatus {
    let tessdata = tessdata_dir();
    EngineStatus {
        name: "tesseract".to_string(),
        available: tessdata.is_dir(),
        version: Some(tesseract_rs::TesseractAPI::version()),
        quality: "bundled".to_string(),
    }
}

fn tesseract_cli_status() -> EngineStatus {
    match std::process::Command::new(DEFAULT_ENGINE)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(output) if output.status.success() => EngineStatus {
            name: "tesseract-cli".to_string(),
            available: true,
            version: String::from_utf8_lossy(&output.stdout)
                .lines()
                .next()
                .map(str::to_string),
            quality: "external_optional".to_string(),
        },
        _ => EngineStatus {
            name: "tesseract-cli".to_string(),
            available: false,
            version: None,
            quality: "external_optional".to_string(),
        },
    }
}

fn ocr_tool_response(value: &ExtractResponse) -> ToolInvocationResponse {
    let output = match serde_json::to_string_pretty(value) {
        Ok(output) => output,
        Err(error) => return tool_error(error.to_string()),
    };
    ToolInvocationResponse {
        output,
        is_error: false,
        content: vec![ToolResultContent::Text {
            text: value.text.clone(),
        }],
        full_output: value.truncated.then_some(value.full_text.clone()),
        host_action: None,
        result: None,
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
    bcode_plugin_sdk::static_plugin_vtable!(OcrPlugin, include_str!("../bcode-plugin.toml"))
}

bcode_plugin_sdk::export_plugin!(OcrPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_requires_exactly_one_input() {
        assert!(
            source(&ExtractRequest {
                path: None,
                url: None,
                language: None,
                engine: None,
                options: None,
                max_bytes: None,
                timeout_ms: None,
            })
            .is_err()
        );
        assert!(
            source(&ExtractRequest {
                path: Some(PathBuf::from("a.png")),
                url: Some("https://example.com/a.png".to_string()),
                language: None,
                engine: None,
                options: None,
                max_bytes: None,
                timeout_ms: None,
            })
            .is_err()
        );
    }

    #[test]
    fn source_accepts_path() {
        let result = source(&ExtractRequest {
            path: Some(PathBuf::from("a.png")),
            url: None,
            language: None,
            engine: None,
            options: None,
            max_bytes: None,
            timeout_ms: None,
        })
        .expect("path source");
        assert_eq!(result.as_string(), "a.png");
    }

    #[test]
    fn unknown_options_are_rejected() {
        let options: OcrOptions =
            serde_json::from_value(json!({ "deskew": true })).expect("options deserialize");
        let error = validate_options(Some(&options)).expect_err("unknown option");
        assert!(matches!(error, OcrError::UnknownOption(_)));
    }

    #[test]
    fn psm_is_bounded() {
        let options: OcrOptions =
            serde_json::from_value(json!({ "psm": 14 })).expect("options deserialize");
        let error = validate_options(Some(&options)).expect_err("invalid psm");
        assert!(matches!(error, OcrError::InvalidOption { name: "psm", .. }));
    }

    #[test]
    fn stable_names_are_path_safe() {
        assert_eq!(
            stable_name("https://example.com/a file.png"),
            "https___example.com_a_file.png"
        );
    }

    #[test]
    fn status_mentions_tesseract_engine() {
        let status = status_response();
        assert_eq!(status.extract.default_engine, default_engine_name());
        assert!(
            status
                .extract
                .engines
                .iter()
                .any(|engine| engine.name == status.extract.default_engine)
        );
    }
}
