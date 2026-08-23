#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

#[cfg(feature = "static-bundled")]
mod ocr_tui;

use bcode_model_provider_runtime::ProviderRuntime;
use bcode_plugin_sdk::path::display;
use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID, ToolArtifact,
    ToolDefinition, ToolInvocationLifecycleEvent, ToolInvocationLifecycleStage,
    ToolInvocationRequest, ToolInvocationResponse, ToolInvocationResult, ToolList,
    ToolResultContent,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
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
const OCR_PLUGIN_ID: &str = "bcode.ocr";
const OCR_REQUEST_SCHEMA: &str = "bcode.ocr.request";
const OCR_EXTRACT_SCHEMA: &str = "bcode.ocr.extract_result";
const OCR_STATUS_SCHEMA: &str = "bcode.ocr.status";

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
        let event = progress_lifecycle_event(&self.tool_call_id, sequence, message.into());
        if let Ok(payload) = serde_json::to_vec(&event) {
            self.events.emit(&payload);
        }
    }
}

fn progress_lifecycle_event(
    invocation_id: &str,
    sequence: u64,
    message: String,
) -> ToolInvocationLifecycleEvent {
    ToolInvocationLifecycleEvent {
        invocation_id: invocation_id.to_owned(),
        sequence,
        stage: ToolInvocationLifecycleStage::Progress,
        message: Some(message),
        metadata: serde_json::Value::Null,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct OcrPreparationDescriptor {
    #[serde(default)]
    workspace_root: Option<PathBuf>,
    #[serde(default)]
    source_path: Option<PathBuf>,
}

fn ocr_workspace_root(
    request: &bcode_tool::ToolPreparationRequest,
) -> Result<Option<PathBuf>, String> {
    let mut matching = request
        .host_context
        .iter()
        .filter(|entry| entry.schema == bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA);
    let Some(entry) = matching.next() else {
        return Ok(None);
    };
    if matching.next().is_some() {
        return Err("duplicate OCR workspace host context".to_owned());
    }
    if entry.schema_version != bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA_VERSION {
        return Err(format!(
            "unsupported OCR workspace host context version {}; expected {}",
            entry.schema_version,
            bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA_VERSION
        ));
    }
    let root = entry
        .payload
        .get("working_directory")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "OCR workspace host context working_directory is missing".to_owned())?;
    let root = PathBuf::from(root);
    if !root.is_absolute() {
        return Err("OCR workspace working directory must be absolute".to_owned());
    }
    root.canonicalize().map(Some).map_err(|error| {
        format!(
            "failed to canonicalize OCR workspace {}: {error}",
            root.display()
        )
    })
}

fn canonicalize_confined_ocr_source(workspace_root: &Path, path: &Path) -> Result<PathBuf, String> {
    let canonical_root = workspace_root.canonicalize().map_err(|error| {
        format!(
            "failed to canonicalize OCR workspace {}: {error}",
            workspace_root.display()
        )
    })?;
    let canonical_path = workspace_root.join(path).canonicalize().map_err(|error| {
        format!(
            "failed to canonicalize OCR source {}: {error}",
            workspace_root.join(path).display()
        )
    })?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(format!(
            "OCR source {} escapes workspace {}",
            canonical_path.display(),
            canonical_root.display()
        ));
    }
    Ok(canonical_path)
}

fn ocr_policy_preparation(
    request: &bcode_tool::ToolPreparationRequest,
    definition: &ToolDefinition,
) -> Result<bcode_plugin_sdk::ToolPolicyPreparation, String> {
    let workspace_root = ocr_workspace_root(request)?;
    let (operation, source_path) = match definition.name.as_str() {
        "ocr.status" => (bcode_plugin_sdk::ToolPolicyOperation::ReadOnly, None),
        "ocr.extract" => {
            let extract =
                serde_json::from_value::<ExtractRequest>(request.invocation.arguments.clone())
                    .map_err(|error| format!("invalid OCR extraction request: {error}"))?;
            match source(&extract).map_err(|error| error.to_string())? {
                OcrSource::Url(url) => (
                    bcode_plugin_sdk::ToolPolicyOperation::Web { url: Some(url) },
                    None,
                ),
                OcrSource::Path(path) => {
                    let path = if path.is_absolute() {
                        path.canonicalize().map_err(|error| {
                            format!(
                                "failed to canonicalize OCR source {}: {error}",
                                path.display()
                            )
                        })?
                    } else {
                        let workspace_root = workspace_root.as_ref().ok_or_else(|| {
                            "OCR relative path requires workspace host context".to_owned()
                        })?;
                        canonicalize_confined_ocr_source(workspace_root, &path)?
                    };
                    (
                        bcode_plugin_sdk::ToolPolicyOperation::Read {
                            paths: vec![path.display().to_string()],
                        },
                        Some(path),
                    )
                }
            }
        }
        name => return Err(format!("unsupported OCR policy operation: {name}")),
    };
    let preparation = bcode_plugin_sdk::ToolPolicyPreparation::new(false, operation);
    let preparation = if definition.name == "ocr.extract" {
        preparation.with_identity(bcode_plugin_sdk::ToolPolicyIdentity {
            aliases: vec!["read".to_string()],
            compatibility_aliases: Vec::new(),
            capabilities: vec!["ocr".to_string(), "read".to_string()],
            permission_category: Some("read".to_string()),
        })
    } else {
        preparation
    };
    Ok(preparation.with_descriptor(
        serde_json::to_value(OcrPreparationDescriptor {
            workspace_root,
            source_path,
        })
        .map_err(|error| error.to_string())?,
    ))
}

fn apply_ocr_preparation(
    request: &mut ExtractRequest,
    descriptor: &OcrPreparationDescriptor,
) -> Result<(), String> {
    match source(request).map_err(|error| error.to_string())? {
        OcrSource::Path(_) => {
            let source_path = descriptor.source_path.clone().ok_or_else(|| {
                "OCR preparation descriptor is missing the local source path".to_owned()
            })?;
            request.path = Some(source_path);
        }
        OcrSource::Url(_) if descriptor.source_path.is_some() => {
            return Err(
                "OCR preparation descriptor contains a source path for a URL request".to_owned(),
            );
        }
        OcrSource::Url(_) => {}
    }
    Ok(())
}

impl OcrPlugin {
    fn invoke_tool_service(&self, context: &NativeServiceContext) -> ServiceResponse {
        let request = &context.request;
        match request.operation.as_str() {
            OP_LIST_TOOLS => list_tools(request),
            bcode_tool::OP_PREPARE_TOOL => prepare_tool_service_response(
                request,
                [extract_tool_definition(), status_tool_definition()],
                ocr_policy_preparation,
            ),
            OP_INVOKE_TOOL => self.invoke_tool(context),
            _ => ServiceResponse::error("unsupported_operation", "unsupported tool operation"),
        }
    }

    fn invoke_tool(&self, context: &NativeServiceContext) -> ServiceResponse {
        let invocation = match context.request.payload_json::<ToolInvocationRequest>() {
            Ok(request) => request,
            Err(error) => return invalid_request(&error),
        };
        let mut presentation = PrimaryPresentationPublisher::with_limits_and_cancellation(
            context.events,
            &invocation.tool_call_id,
            OCR_PLUGIN_ID,
            OCR_REQUEST_SCHEMA,
            1,
            bcode_tool::ToolPresentationRetention::RetainLatest,
            context.transient_progress_limits,
            context.cancellation.clone(),
        );
        let _ = presentation.replace(&request_visual_payload(
            &invocation.name,
            &invocation.arguments,
        ));
        let response = match invocation.name.as_str() {
            "ocr.extract" => self.invoke_extract(&invocation, context.events),
            "ocr.status" => invoke_status(&invocation.tool_call_id),
            _ => ToolInvocationResponse {
                output: format!("unknown OCR tool: {}", invocation.name),
                is_error: true,
                content: Vec::new(),
                full_output: None,
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
        let mut request =
            match serde_json::from_value::<ExtractRequest>(invocation.arguments.clone()) {
                Ok(request) => request,
                Err(error) => return tool_error(error.to_string()),
            };
        let descriptor = match serde_json::from_value::<OcrPreparationDescriptor>(
            invocation.preparation_descriptor.clone(),
        ) {
            Ok(descriptor) => descriptor,
            Err(error) => {
                return tool_error(format!("invalid OCR preparation descriptor: {error}"));
            }
        };
        if let Err(error) = apply_ocr_preparation(&mut request, &descriptor) {
            return tool_error(error);
        }
        let runtime = match &self.runtime {
            Ok(runtime) => runtime,
            Err(error) => return tool_error(format!("OCR runtime unavailable: {error}")),
        };
        let progress = ProgressReporter::new(events, invocation.tool_call_id.clone());
        progress.emit("OCR extraction started");
        match runtime.block_on(extract_async(
            request,
            descriptor
                .workspace_root
                .unwrap_or_else(|| PathBuf::from(".")),
            Some(progress),
        )) {
            Ok(Ok(response)) => ocr_tool_response(&response, &invocation.tool_call_id),
            Ok(Err(error)) => tool_error(error.to_string()),
            Err(error) => tool_error(error.to_string()),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ExtractRequest {
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

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
struct OcrOptions {
    #[serde(default)]
    psm: Option<u8>,
    #[serde(default)]
    oem: Option<u8>,
    #[serde(default)]
    config: Vec<String>,
    #[serde(default)]
    tesseract_version: Option<String>,
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtractResponse {
    pub text: String,
    #[serde(skip)]
    pub full_text: String,
    pub source: SourceResponse,
    pub engine: String,
    pub language: String,
    pub truncated: bool,
    pub text_bytes: usize,
    pub full_text_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceResponse {
    pub path: String,
    pub url: Option<String>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    available_bundled_versions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_bundled_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    latest_bundled_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OcrSource {
    Path(PathBuf),
    Url(String),
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
    #[cfg(feature = "_bundled-tesseract-runtime")]
    #[error("image decoding failed: {0}")]
    Image(#[from] image::ImageError),
    #[cfg(feature = "_bundled-tesseract-runtime")]
    #[error("bundled tesseract failed: {0}")]
    BundledTesseract(String),
}

/// Extract text from a local image path without model participation.
///
/// # Errors
///
/// Returns an error when the runtime cannot start, the path or working directory is unavailable,
/// the input is invalid or oversized, or the selected OCR engine fails.
pub fn extract_path(path: &Path) -> Result<ExtractResponse, String> {
    let runtime = ProviderRuntime::new().map_err(|error| error.to_string())?;
    runtime
        .block_on(extract_async(
            ExtractRequest {
                path: Some(path.to_path_buf()),
                url: None,
                language: None,
                engine: None,
                options: None,
                max_bytes: None,
                timeout_ms: None,
            },
            std::env::current_dir().map_err(|error| error.to_string())?,
            None,
        ))
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

async fn extract_async(
    request: ExtractRequest,
    working_directory: PathBuf,
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
    let mut scratch = None;
    let input_path = match &source {
        OcrSource::Path(path) => path.clone(),
        OcrSource::Url(url) => {
            let directory = ocr_scratch_directory()?;
            if let Some(progress) = &progress {
                progress.emit(format!("OCR download started: {url}"));
            }
            let path = download_source(url, directory.path(), timeout_ms).await?;
            scratch = Some(directory);
            path
        }
    };
    if let Some(progress) = &progress {
        progress.emit(format!(
            "OCR source path: {}",
            display(&input_path, &working_directory)
        ));
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
    drop(scratch);
    let full_text_bytes = full_text.len();
    let truncated = full_text_bytes > max_bytes;
    let text = truncate_utf8(&full_text, max_bytes).to_string();
    Ok(ExtractResponse {
        text,
        full_text,
        source: source_response(&source),
        engine,
        language,
        truncated,
        text_bytes: full_text_bytes.min(max_bytes),
        full_text_bytes,
    })
}

fn ocr_scratch_directory() -> Result<tempfile::TempDir, OcrError> {
    tempfile::Builder::new()
        .prefix("bcode-ocr-")
        .tempdir()
        .map_err(OcrError::Io)
}

fn source_response(source: &OcrSource) -> SourceResponse {
    match source {
        OcrSource::Path(path) => SourceResponse {
            path: path.display().to_string(),
            url: None,
        },
        OcrSource::Url(url) => SourceResponse {
            path: String::new(),
            url: Some(url.clone()),
        },
    }
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
    #[cfg(feature = "_bundled-tesseract-runtime")]
    {
        "tesseract".to_string()
    }
    #[cfg(not(feature = "_bundled-tesseract-runtime"))]
    {
        "tesseract-cli".to_string()
    }
}

fn is_supported_engine(engine: &str) -> bool {
    matches!(engine, "tesseract-cli")
        || cfg!(feature = "_bundled-tesseract-runtime") && engine == "tesseract"
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
        #[cfg(feature = "_bundled-tesseract-runtime")]
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

#[cfg(feature = "_bundled-tesseract-runtime")]
fn run_bundled_tesseract(
    path: &Path,
    language: &str,
    options: Option<&OcrOptions>,
) -> Result<String, OcrError> {
    let image = image::open(path)?.to_rgba8();
    let (width, height) = image.dimensions();
    let bytes_per_pixel = 4_i32;
    let width =
        i32::try_from(width).map_err(|error| OcrError::BundledTesseract(error.to_string()))?;
    let height =
        i32::try_from(height).map_err(|error| OcrError::BundledTesseract(error.to_string()))?;
    let bytes_per_line = width.saturating_mul(bytes_per_pixel);
    let runtime = options
        .and_then(|options| options.tesseract_version.as_deref())
        .map_or_else(
            bcode_tesseract_ocr::TesseractRuntime::load_default,
            bcode_tesseract_ocr::TesseractRuntime::load_version,
        )
        .map_err(|error| OcrError::BundledTesseract(error.to_string()))?;
    let engine = runtime
        .create_engine()
        .map_err(|error| OcrError::BundledTesseract(error.to_string()))?;
    let engine_mode = options
        .and_then(|options| options.oem)
        .map(|oem| bcode_tesseract_ocr::EngineMode::from_raw(i32::from(oem)));
    engine
        .init(&bcode_tesseract_ocr::InitOptions {
            datapath: None,
            language: language.to_string(),
            engine_mode,
        })
        .map_err(|error| OcrError::BundledTesseract(error.to_string()))?;

    let mut recognition_options = bcode_tesseract_ocr::RecognitionOptions::default();
    if let Some(options) = options {
        recognition_options.page_seg_mode = options
            .psm
            .map(|psm| bcode_tesseract_ocr::PageSegMode::from_raw(i32::from(psm)));
        for config in &options.config {
            let Some((name, value)) = config.split_once('=') else {
                return Err(OcrError::BundledTesseract(format!(
                    "bundled tesseract config must use name=value syntax: {config}"
                )));
            };
            recognition_options
                .variables
                .push((name.to_string(), value.to_string()));
        }
    }

    engine
        .recognize(
            bcode_tesseract_ocr::ImageView {
                bytes: image.as_raw(),
                width,
                height,
                bytes_per_pixel,
                bytes_per_line,
            },
            &recognition_options,
        )
        .map_err(|error| OcrError::BundledTesseract(error.to_string()))
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
            "Extract text from images or image-like documents using the configured OCR engine. Use this for screenshots, photos, scanned images, or when the user asks what text an image says. Prefer this over filesystem.read for text-in-image questions."
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
    }
}

fn status_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "ocr.status".to_string(),
        description: "Report OCR engine availability and default OCR configuration.".to_string(),
        input_schema: json!({ "type": "object", "properties": {} }),
    }
}

fn request_visual_payload(operation: &str, arguments: &serde_json::Value) -> serde_json::Value {
    let mut payload = arguments.as_object().cloned().unwrap_or_default();
    payload.insert(
        "operation".to_owned(),
        serde_json::Value::String(operation.to_owned()),
    );
    serde_json::Value::Object(payload)
}

fn invoke_status(tool_call_id: &str) -> ToolInvocationResponse {
    json_tool_response_with_artifact(
        &status_response(),
        tool_call_id,
        "status",
        OCR_STATUS_SCHEMA,
        "OCR status",
    )
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

#[cfg(feature = "_bundled-tesseract-runtime")]
fn ocr_engine_statuses() -> Vec<EngineStatus> {
    vec![bundled_tesseract_status(), tesseract_cli_status()]
}

#[cfg(not(feature = "_bundled-tesseract-runtime"))]
fn ocr_engine_statuses() -> Vec<EngineStatus> {
    vec![tesseract_cli_status()]
}

#[cfg(feature = "_bundled-tesseract-runtime")]
fn bundled_tesseract_status() -> EngineStatus {
    let tessdata = bcode_tesseract_ocr::resolve_tessdata_dir();
    let runtime = bcode_tesseract_ocr::TesseractRuntime::load_default();
    EngineStatus {
        name: "tesseract".to_string(),
        available: tessdata.is_dir() && runtime.is_ok(),
        version: Some(format!(
            "{} (bundled runtime {})",
            bcode_tesseract_ocr::TesseractEngine::version(),
            runtime
                .as_ref()
                .map_or("unavailable", |runtime| runtime.version())
        )),
        quality: "bundled".to_string(),
        available_bundled_versions: bcode_tesseract_ocr::available_bundled_versions()
            .into_iter()
            .map(str::to_string)
            .collect(),
        default_bundled_version: Some(bcode_tesseract_ocr::bundled_default_version()),
        latest_bundled_version: Some(bcode_tesseract_ocr::bundled_latest_version()),
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
            available_bundled_versions: Vec::new(),
            default_bundled_version: None,
            latest_bundled_version: None,
        },
        _ => EngineStatus {
            name: "tesseract-cli".to_string(),
            available: false,
            version: None,
            quality: "external_optional".to_string(),
            available_bundled_versions: Vec::new(),
            default_bundled_version: None,
            latest_bundled_version: None,
        },
    }
}

fn ocr_tool_response(value: &ExtractResponse, tool_call_id: &str) -> ToolInvocationResponse {
    let (output, payload) = match serde_json::to_string_pretty(value).and_then(|output| {
        let payload = serde_json::to_value(value)?;
        Ok((output, payload))
    }) {
        Ok(result) => result,
        Err(error) => return tool_error(error.to_string()),
    };
    ToolInvocationResponse {
        output,
        is_error: false,
        content: vec![ToolResultContent::Text {
            text: value.text.clone(),
        }],
        full_output: value.truncated.then_some(value.full_text.clone()),
        result: Some(ocr_artifact_result(
            tool_call_id,
            "extract",
            OCR_EXTRACT_SCHEMA,
            "OCR text",
            payload,
        )),
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

fn json_tool_response_with_artifact<T: Serialize>(
    value: &T,
    tool_call_id: &str,
    artifact_suffix: &str,
    schema: &str,
    title: &str,
) -> ToolInvocationResponse {
    match serde_json::to_string_pretty(value).and_then(|output| {
        let payload = serde_json::to_value(value)?;
        Ok((output, payload))
    }) {
        Ok((output, payload)) => ToolInvocationResponse {
            output,
            is_error: false,
            content: Vec::new(),
            full_output: None,
            result: Some(ocr_artifact_result(
                tool_call_id,
                artifact_suffix,
                schema,
                title,
                payload,
            )),
        },
        Err(error) => tool_error(error.to_string()),
    }
}

fn ocr_artifact_result(
    tool_call_id: &str,
    artifact_suffix: &str,
    schema: &str,
    title: &str,
    payload: serde_json::Value,
) -> ToolInvocationResult {
    ToolInvocationResult::Artifact {
        artifact: Box::new(ToolArtifact {
            artifact_id: format!("{tool_call_id}-ocr-{artifact_suffix}"),
            producer_plugin_id: OCR_PLUGIN_ID.to_string(),
            schema: schema.to_string(),
            schema_version: 1,
            tool_call_id: Some(tool_call_id.to_string()),
            title: Some(title.to_string()),
            metadata: payload,
            refs: Vec::new(),
        }),
    }
}

const fn tool_error(output: String) -> ToolInvocationResponse {
    ToolInvocationResponse {
        output,
        is_error: true,
        content: Vec::new(),
        full_output: None,
        result: None,
    }
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(OcrPlugin, include_str!("../bcode-plugin.toml"))
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn ocr_tui_registry() -> bcode_plugin_sdk::tui::PluginTuiRegistry {
    let mut registry = bcode_plugin_sdk::tui::PluginTuiRegistry::default();
    registry.register_visual_adapter(
        [
            "ocr-request-card",
            "ocr-extract-result-card",
            "ocr-status-card",
        ],
        Box::new(ocr_tui::OcrTuiVisualAdapter),
    );
    registry
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(OcrPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ocr_tools_emit_request_contributions() {
        for definition in [extract_tool_definition(), status_tool_definition()] {
            let encoded = serde_json::to_value(definition).expect("tool definition encodes");
            assert!(encoded.get("ui").is_none());
        }
    }

    fn preparation_request(
        definition: &ToolDefinition,
        arguments: serde_json::Value,
        host_context: Vec<bcode_tool::ToolHostContextEntry>,
    ) -> bcode_tool::ToolPreparationRequest {
        bcode_tool::ToolPreparationRequest {
            invocation: bcode_tool::ToolInvocationDescriptor {
                invocation_id: "call".to_owned(),
                tool_name: definition.name.clone(),
                arguments,
            },
            host_context,
        }
    }

    fn workspace_context(path: &Path) -> Vec<bcode_tool::ToolHostContextEntry> {
        vec![bcode_tool::ToolHostContextEntry {
            schema: bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA.to_owned(),
            schema_version: bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA_VERSION,
            payload: serde_json::json!({"working_directory": path}),
        }]
    }

    #[test]
    fn ocr_owner_prepares_exact_local_source_and_preserves_permission_behavior() {
        let workspace = tempfile::tempdir().expect("workspace");
        let image = workspace.path().join("image.png");
        std::fs::write(&image, b"fixture").expect("fixture");
        let canonical_workspace = workspace
            .path()
            .canonicalize()
            .expect("canonical workspace");
        let canonical_image = image.canonicalize().expect("canonical image");
        let definition = extract_tool_definition();
        let request = preparation_request(
            &definition,
            serde_json::json!({"path": "image.png"}),
            workspace_context(workspace.path()),
        );

        let prepared = ocr_policy_preparation(&request, &definition).expect("OCR preparation");

        assert!(!prepared.requires_permission);
        assert_eq!(prepared.identity.aliases, vec!["read"]);
        assert_eq!(prepared.identity.capabilities, vec!["ocr", "read"]);
        assert_eq!(
            prepared.identity.permission_category.as_deref(),
            Some("read")
        );
        assert_eq!(
            prepared.operation,
            bcode_plugin_sdk::ToolPolicyOperation::Read {
                paths: vec![canonical_image.display().to_string()],
            }
        );
        assert_eq!(
            serde_json::from_value::<OcrPreparationDescriptor>(prepared.descriptor)
                .expect("OCR descriptor"),
            OcrPreparationDescriptor {
                workspace_root: Some(canonical_workspace),
                source_path: Some(canonical_image),
            }
        );
    }

    #[test]
    fn ocr_relative_path_canonicalizes_within_workspace() {
        let workspace = tempfile::tempdir().expect("workspace");
        let images = workspace.path().join("images");
        std::fs::create_dir_all(&images).expect("images");
        let image = images.join("fixture.png");
        std::fs::write(&image, b"fixture").expect("fixture");
        let definition = extract_tool_definition();
        let request = preparation_request(
            &definition,
            serde_json::json!({"path": "images/../images/fixture.png"}),
            workspace_context(workspace.path()),
        );

        let prepared = ocr_policy_preparation(&request, &definition).expect("OCR preparation");
        let descriptor = serde_json::from_value::<OcrPreparationDescriptor>(prepared.descriptor)
            .expect("OCR descriptor");

        assert_eq!(
            descriptor.source_path,
            Some(image.canonicalize().expect("canonical fixture"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn ocr_relative_path_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let outside_image = outside.path().join("outside.png");
        std::fs::write(&outside_image, b"outside").expect("outside fixture");
        symlink(&outside_image, workspace.path().join("escape.png")).expect("symlink");
        let definition = extract_tool_definition();
        let request = preparation_request(
            &definition,
            serde_json::json!({"path": "escape.png"}),
            workspace_context(workspace.path()),
        );

        let error = ocr_policy_preparation(&request, &definition).expect_err("symlink escape");

        assert!(error.contains("escapes workspace"), "{error}");
    }

    #[cfg(windows)]
    #[test]
    fn ocr_relative_path_rejects_directory_junction_escape() {
        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        let outside_image = outside.path().join("outside.png");
        std::fs::write(&outside_image, b"outside").expect("outside fixture");
        let junction = workspace.path().join("escape");
        let status = std::process::Command::new("cmd")
            .args([
                "/D",
                "/C",
                "mklink",
                "/J",
                &junction.display().to_string(),
                &outside.path().display().to_string(),
            ])
            .status()
            .expect("create junction fixture");
        assert!(status.success(), "directory junction fixture");
        let definition = extract_tool_definition();
        let request = preparation_request(
            &definition,
            serde_json::json!({"path": "escape/outside.png"}),
            workspace_context(workspace.path()),
        );

        let error = ocr_policy_preparation(&request, &definition).expect_err("junction escape");

        assert!(error.contains("escapes workspace"), "{error}");
    }

    #[test]
    fn ocr_status_preparation_remains_permission_free_and_read_only() {
        let definition = status_tool_definition();
        let request = preparation_request(&definition, serde_json::Value::Null, Vec::new());

        let prepared = ocr_policy_preparation(&request, &definition).expect("OCR status");

        assert!(!prepared.requires_permission);
        assert_eq!(
            prepared.identity,
            bcode_plugin_sdk::ToolPolicyIdentity::default()
        );
        assert_eq!(
            prepared.operation,
            bcode_plugin_sdk::ToolPolicyOperation::ReadOnly
        );
    }

    #[test]
    fn ocr_relative_source_requires_workspace_context() {
        let definition = extract_tool_definition();
        let request = preparation_request(
            &definition,
            serde_json::json!({"path": "image.png"}),
            Vec::new(),
        );

        let error = ocr_policy_preparation(&request, &definition)
            .expect_err("relative source without workspace");

        assert!(error.contains("requires workspace host context"));
    }

    #[test]
    fn ocr_invocation_uses_prepared_local_source_path() {
        let mut request = ExtractRequest {
            path: Some(PathBuf::from("image.png")),
            url: None,
            language: None,
            engine: None,
            options: None,
            max_bytes: None,
            timeout_ms: None,
        };
        let descriptor = OcrPreparationDescriptor {
            workspace_root: Some(PathBuf::from("/tmp/workspace")),
            source_path: Some(PathBuf::from("/tmp/workspace/image.png")),
        };

        apply_ocr_preparation(&mut request, &descriptor).expect("apply preparation");

        assert_eq!(
            request.path,
            Some(PathBuf::from("/tmp/workspace/image.png"))
        );
    }

    #[test]
    fn ocr_invocation_rejects_missing_local_source_descriptor() {
        let mut request = ExtractRequest {
            path: Some(PathBuf::from("image.png")),
            url: None,
            language: None,
            engine: None,
            options: None,
            max_bytes: None,
            timeout_ms: None,
        };

        let error = apply_ocr_preparation(&mut request, &OcrPreparationDescriptor::default())
            .expect_err("missing prepared source");

        assert!(error.contains("missing the local source path"));
    }

    #[test]
    fn progress_uses_neutral_invocation_lifecycle_contract() {
        let event = progress_lifecycle_event("ocr-call", 3, "extracting".to_owned());
        let encoded = serde_json::to_vec(&event).expect("lifecycle should encode");
        let decoded: ToolInvocationLifecycleEvent =
            serde_json::from_slice(&encoded).expect("lifecycle should decode");
        assert_eq!(decoded.invocation_id, "ocr-call");
        assert_eq!(decoded.sequence, 3);
        assert_eq!(decoded.stage, ToolInvocationLifecycleStage::Progress);
        assert_eq!(decoded.message.as_deref(), Some("extracting"));
    }

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
        assert_eq!(result, OcrSource::Path(PathBuf::from("a.png")));
    }

    #[test]
    fn url_source_metadata_does_not_expose_scratch_path() {
        let source = OcrSource::Url("https://example.com/image.png".to_owned());

        assert_eq!(
            source_response(&source),
            SourceResponse {
                path: String::new(),
                url: Some("https://example.com/image.png".to_owned()),
            }
        );
    }

    #[test]
    fn downloaded_source_scratch_directory_is_removed_on_drop() {
        let directory = ocr_scratch_directory().expect("OCR scratch directory");
        let path = directory.path().to_path_buf();
        std::fs::write(path.join("source.png"), b"fixture").expect("scratch fixture");

        drop(directory);

        assert!(!path.exists());
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
