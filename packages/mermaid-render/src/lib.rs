//! Bounded Mermaid rendering behind Bcode-owned request and result types.
//!
//! The concrete backend is private. Consumers must not depend on its types,
//! diagnostics, configuration, or output structures.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use std::{
    collections::BTreeMap,
    io::{Read, Write},
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// Maximum successful Mermaid renders retained in memory.
pub const MAX_CACHE_ENTRIES: usize = 64;
/// Maximum encoded Mermaid output bytes retained in memory.
pub const MAX_CACHE_BYTES: usize = 32 * 1024 * 1024;

static RENDER_PERMITS: OnceLock<(Mutex<usize>, Condvar)> = OnceLock::new();

struct RenderPermit;

impl RenderPermit {
    fn acquire(
        limit: usize,
        timeout: Duration,
        cancellation: &MermaidCancellationToken,
    ) -> Result<Self, MermaidRenderError> {
        let (permits, available) = RENDER_PERMITS.get_or_init(|| (Mutex::new(0), Condvar::new()));
        let mut active = permits
            .lock()
            .map_err(|_| MermaidRenderError::BackendPanicked)?;
        let started = std::time::Instant::now();
        while *active >= limit {
            if cancellation.is_cancelled() {
                return Err(MermaidRenderError::Cancelled);
            }
            let remaining = timeout
                .checked_sub(started.elapsed())
                .ok_or(MermaidRenderError::TimedOut)?;
            let wait = remaining.min(Duration::from_millis(10));
            let result = available
                .wait_timeout(active, wait)
                .map_err(|_| MermaidRenderError::BackendPanicked)?;
            active = result.0;
            if result.1.timed_out() && started.elapsed() >= timeout {
                return Err(MermaidRenderError::TimedOut);
            }
        }
        *active = active.saturating_add(1);
        drop(active);
        Ok(Self)
    }
}

impl Drop for RenderPermit {
    fn drop(&mut self) {
        if let Some((permits, available)) = RENDER_PERMITS.get()
            && let Ok(mut active) = permits.lock()
        {
            *active = active.saturating_sub(1);
            available.notify_one();
        }
    }
}

/// Version of the stable Bcode Mermaid render contract and cache-key semantics.
pub const RENDER_CONTRACT_VERSION: u16 = 1;

/// Mermaid source owned by a render request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MermaidSource(String);

impl MermaidSource {
    /// Create Mermaid source.
    #[must_use]
    pub fn new(source: impl Into<String>) -> Self {
        Self(source.into())
    }

    /// Return the source text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Preferred Bcode-owned render output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MermaidOutputPreference {
    /// Scalable SVG suitable for later rasterization or display.
    Svg,
}

/// Bounds applied before and after backend rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MermaidRenderLimits {
    /// Maximum source bytes accepted.
    pub max_source_bytes: usize,
    /// Maximum SVG bytes returned.
    pub max_output_bytes: usize,
    /// Maximum requested pixel width.
    pub max_width: u32,
    /// Maximum requested pixel height.
    pub max_height: u32,
    /// Maximum simultaneous renders enforced by this crate.
    pub max_concurrent_renders: usize,
    /// End-to-end deadline covering permit wait and backend rendering.
    pub timeout: Duration,
}

impl Default for MermaidRenderLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: 64 * 1024,
            max_output_bytes: 4 * 1024 * 1024,
            max_width: 4096,
            max_height: 4096,
            max_concurrent_renders: 2,
            timeout: Duration::from_secs(5),
        }
    }
}

/// Bounded Mermaid render request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MermaidRenderRequest {
    /// Diagram source.
    pub source: MermaidSource,
    /// Preferred output representation.
    pub preference: MermaidOutputPreference,
    /// Maximum desired output width.
    pub width: u32,
    /// Maximum desired output height.
    pub height: u32,
    /// Safety and resource limits.
    pub limits: MermaidRenderLimits,
}

impl MermaidRenderRequest {
    /// Create an SVG request with default limits.
    #[must_use]
    pub fn svg(source: impl Into<String>, width: u32, height: u32) -> Self {
        Self {
            source: MermaidSource::new(source),
            preference: MermaidOutputPreference::Svg,
            width,
            height,
            limits: MermaidRenderLimits::default(),
        }
    }

    /// Return a stable Bcode-owned cache key.
    #[must_use]
    pub fn cache_key(&self) -> String {
        format!(
            "mermaid-v{RENDER_CONTRACT_VERSION}:svg:{}x{}:{}:{}",
            self.width,
            self.height,
            self.limits.max_output_bytes,
            stable_source_hash(self.source.as_str())
        )
    }
}

/// Cooperative cancellation token independent of any async runtime.
#[derive(Debug, Clone, Default)]
pub struct MermaidCancellationToken(Arc<AtomicBool>);

impl MermaidCancellationToken {
    /// Request cancellation.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Return whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Successfully rendered Mermaid content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MermaidRendered {
    /// Bcode-owned encoded output.
    pub output: MermaidRenderedOutput,
    /// Stable request cache key.
    pub cache_key: String,
    /// Non-fatal renderer diagnostics.
    pub diagnostics: Vec<MermaidDiagnostic>,
}

/// Encoded Mermaid output independent of the backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MermaidRenderedOutput {
    /// UTF-8 SVG bytes.
    Svg(Vec<u8>),
}

impl MermaidRenderedOutput {
    const fn payload_bytes(&self) -> usize {
        match self {
            Self::Svg(bytes) => bytes.len(),
        }
    }
}

/// Deterministic bounded least-recently-used cache of successful Mermaid renders.
#[derive(Debug, Default)]
pub struct MermaidRenderCache {
    entries: BTreeMap<String, MermaidCacheEntry>,
    clock: u64,
    payload_bytes: usize,
}

#[derive(Debug)]
struct MermaidCacheEntry {
    rendered: MermaidRendered,
    last_used: u64,
}

impl MermaidRenderCache {
    /// Return a cloned cached render and mark it most recently used.
    pub fn get(&mut self, key: &str) -> Option<MermaidRendered> {
        self.clock = self.clock.saturating_add(1);
        let entry = self.entries.get_mut(key)?;
        entry.last_used = self.clock;
        Some(entry.rendered.clone())
    }

    /// Insert a successful render and deterministically evict least-recently-used entries.
    pub fn insert(&mut self, rendered: MermaidRendered) {
        let payload_bytes = rendered.output.payload_bytes();
        if payload_bytes > MAX_CACHE_BYTES {
            return;
        }
        self.clock = self.clock.saturating_add(1);
        if let Some(previous) = self.entries.remove(&rendered.cache_key) {
            self.payload_bytes = self
                .payload_bytes
                .saturating_sub(previous.rendered.output.payload_bytes());
        }
        self.payload_bytes = self.payload_bytes.saturating_add(payload_bytes);
        self.entries.insert(
            rendered.cache_key.clone(),
            MermaidCacheEntry {
                rendered,
                last_used: self.clock,
            },
        );
        self.evict_to_limits();
    }

    /// Return the current number of cached renders.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return whether no renders are cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Return current encoded output bytes.
    #[must_use]
    pub const fn payload_bytes(&self) -> usize {
        self.payload_bytes
    }

    fn evict_to_limits(&mut self) {
        while self.entries.len() > MAX_CACHE_ENTRIES || self.payload_bytes > MAX_CACHE_BYTES {
            let Some(key) = self
                .entries
                .iter()
                .min_by_key(|(key, entry)| (entry.last_used, *key))
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(entry) = self.entries.remove(&key) {
                self.payload_bytes = self
                    .payload_bytes
                    .saturating_sub(entry.rendered.output.payload_bytes());
            }
        }
    }
}

/// Stable renderer diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MermaidDiagnostic {
    /// Diagnostic severity.
    pub severity: MermaidDiagnosticSeverity,
    /// Human-readable backend-neutral message.
    pub message: String,
}

/// Diagnostic severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MermaidDiagnosticSeverity {
    /// Informational diagnostic.
    Info,
    /// Recoverable warning.
    Warning,
}

/// Bcode-owned Mermaid rendering failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MermaidRenderError {
    /// Source is empty.
    EmptySource,
    /// Source exceeds the configured byte limit.
    SourceTooLarge { actual: usize, maximum: usize },
    /// Requested dimensions are zero or exceed configured bounds.
    InvalidDimensions,
    /// Configured timeout is zero or concurrency is disabled.
    InvalidExecutionLimits,
    /// Output pixel count exceeds the configured dimensions.
    OutputDimensionsExceeded,
    /// Source contains a directive, which Bcode intentionally disallows.
    DirectiveNotAllowed,
    /// Rendering exceeded the configured wall-clock timeout.
    TimedOut,
    /// Backend panicked while rendering untrusted input.
    BackendPanicked,
    /// Rendering was cancelled.
    Cancelled,
    /// Backend rejected or could not render the diagram.
    InvalidDiagram { message: String },
    /// Private worker could not be started or communicated with.
    WorkerUnavailable { message: String },
    /// Private worker returned an invalid response envelope.
    InvalidWorkerResponse { message: String },
    /// Encoded output exceeds the configured byte limit.
    OutputTooLarge { actual: usize, maximum: usize },
}

impl std::fmt::Display for MermaidRenderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptySource => formatter.write_str("Mermaid source is empty"),
            Self::SourceTooLarge { actual, maximum } => {
                write!(
                    formatter,
                    "Mermaid source is {actual} bytes; maximum is {maximum}"
                )
            }
            Self::InvalidDimensions => formatter.write_str("Mermaid dimensions are outside bounds"),
            Self::InvalidExecutionLimits => {
                formatter.write_str("Mermaid timeout and concurrency limits must be non-zero")
            }
            Self::OutputDimensionsExceeded => {
                formatter.write_str("Mermaid output dimensions exceed configured bounds")
            }
            Self::DirectiveNotAllowed => formatter.write_str("Mermaid directives are not allowed"),
            Self::TimedOut => formatter.write_str("Mermaid rendering timed out"),
            Self::BackendPanicked => formatter.write_str("Mermaid backend panicked"),
            Self::Cancelled => formatter.write_str("Mermaid rendering was cancelled"),
            Self::InvalidDiagram { message } => {
                write!(formatter, "invalid Mermaid diagram: {message}")
            }
            Self::WorkerUnavailable { message } => {
                write!(formatter, "Mermaid worker unavailable: {message}")
            }
            Self::InvalidWorkerResponse { message } => {
                write!(formatter, "invalid Mermaid worker response: {message}")
            }
            Self::OutputTooLarge { actual, maximum } => {
                write!(
                    formatter,
                    "Mermaid output is {actual} bytes; maximum is {maximum}"
                )
            }
        }
    }
}

impl std::error::Error for MermaidRenderError {}

const WORKER_REQUEST_MAGIC: &[u8; 4] = b"BCMW";
const WORKER_RESPONSE_MAGIC: &[u8; 4] = b"BCMR";
const WORKER_PROTOCOL_VERSION: u16 = 1;
const WORKER_RESPONSE_HEADER_BYTES: usize = 11;

/// Render a Mermaid request through the private worker executable.
///
/// The caller supplies the worker path explicitly so application packaging owns
/// executable discovery. Timeout and cancellation forcefully terminate the
/// child before this function returns.
///
/// # Errors
///
/// Returns a typed render error when request validation fails, the worker cannot
/// start, cancellation or timeout occurs, or the response is malformed.
pub fn render_mermaid_with_worker(
    worker_path: &std::path::Path,
    request: &MermaidRenderRequest,
    cancellation: &MermaidCancellationToken,
) -> Result<MermaidRendered, MermaidRenderError> {
    validate_request(request, cancellation)?;
    let request_bytes = encode_worker_request(request)?;
    let mut child = std::process::Command::new(worker_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| MermaidRenderError::WorkerUnavailable {
            message: error.to_string(),
        })?;
    let write_result = child
        .stdin
        .take()
        .ok_or_else(|| MermaidRenderError::WorkerUnavailable {
            message: "worker stdin unavailable".to_owned(),
        })?
        .write_all(&request_bytes);
    if let Err(error) = write_result {
        terminate_worker(&mut child);
        return Err(MermaidRenderError::WorkerUnavailable {
            message: error.to_string(),
        });
    }
    let stdout = child.stdout.take().ok_or_else(|| {
        terminate_worker(&mut child);
        MermaidRenderError::InvalidWorkerResponse {
            message: "worker stdout unavailable".to_owned(),
        }
    })?;
    let response_limit = u64::try_from(request.limits.max_output_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(u64::try_from(WORKER_RESPONSE_HEADER_BYTES).unwrap_or(11))
        .saturating_add(1);
    let reader = std::thread::spawn(move || {
        let mut response = Vec::new();
        let result = stdout.take(response_limit).read_to_end(&mut response);
        (result, response)
    });
    let started = std::time::Instant::now();
    let status = loop {
        if cancellation.is_cancelled() {
            terminate_worker(&mut child);
            let _ = reader.join();
            return Err(MermaidRenderError::Cancelled);
        }
        if started.elapsed() >= request.limits.timeout {
            terminate_worker(&mut child);
            let _ = reader.join();
            return Err(MermaidRenderError::TimedOut);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) => {
                terminate_worker(&mut child);
                let _ = reader.join();
                return Err(MermaidRenderError::WorkerUnavailable {
                    message: error.to_string(),
                });
            }
        }
    };
    let (read_result, response) =
        reader
            .join()
            .map_err(|_| MermaidRenderError::InvalidWorkerResponse {
                message: "worker response reader panicked".to_owned(),
            })?;
    read_result.map_err(|error| MermaidRenderError::InvalidWorkerResponse {
        message: error.to_string(),
    })?;
    if response.len()
        > request
            .limits
            .max_output_bytes
            .saturating_add(WORKER_RESPONSE_HEADER_BYTES)
    {
        return Err(MermaidRenderError::InvalidWorkerResponse {
            message: "worker response exceeds output limit".to_owned(),
        });
    }
    decode_worker_response(request, &response).map_err(|error| {
        if status.success() {
            error
        } else {
            match error {
                MermaidRenderError::InvalidDiagram { .. } => error,
                _ => MermaidRenderError::InvalidWorkerResponse {
                    message: format!("worker exited with {status}: {error}"),
                },
            }
        }
    })
}

fn encode_worker_request(request: &MermaidRenderRequest) -> Result<Vec<u8>, MermaidRenderError> {
    let source = request.source.as_str().as_bytes();
    let source_len =
        u32::try_from(source.len()).map_err(|_| MermaidRenderError::SourceTooLarge {
            actual: source.len(),
            maximum: u32::MAX as usize,
        })?;
    let max_output = u32::try_from(request.limits.max_output_bytes)
        .map_err(|_| MermaidRenderError::InvalidExecutionLimits)?;
    let mut encoded = Vec::with_capacity(22 + source.len());
    encoded.extend_from_slice(WORKER_REQUEST_MAGIC);
    encoded.extend_from_slice(&WORKER_PROTOCOL_VERSION.to_be_bytes());
    encoded.extend_from_slice(&request.width.to_be_bytes());
    encoded.extend_from_slice(&request.height.to_be_bytes());
    encoded.extend_from_slice(&max_output.to_be_bytes());
    encoded.extend_from_slice(&source_len.to_be_bytes());
    encoded.extend_from_slice(source);
    Ok(encoded)
}

fn decode_worker_response(
    request: &MermaidRenderRequest,
    response: &[u8],
) -> Result<MermaidRendered, MermaidRenderError> {
    if response.len() < WORKER_RESPONSE_HEADER_BYTES || &response[..4] != WORKER_RESPONSE_MAGIC {
        return Err(MermaidRenderError::InvalidWorkerResponse {
            message: "missing response envelope".to_owned(),
        });
    }
    let version = u16::from_be_bytes([response[4], response[5]]);
    if version != WORKER_PROTOCOL_VERSION {
        return Err(MermaidRenderError::InvalidWorkerResponse {
            message: format!("unsupported protocol version {version}"),
        });
    }
    let length = usize::try_from(u32::from_be_bytes(
        response[7..11]
            .try_into()
            .expect("fixed worker length field"),
    ))
    .unwrap_or(usize::MAX);
    let payload = response
        .get(WORKER_RESPONSE_HEADER_BYTES..)
        .ok_or_else(|| MermaidRenderError::InvalidWorkerResponse {
            message: "missing response payload".to_owned(),
        })?;
    if payload.len() != length || payload.len() > request.limits.max_output_bytes {
        return Err(MermaidRenderError::InvalidWorkerResponse {
            message: "response length is invalid".to_owned(),
        });
    }
    if response[6] == 0 {
        return Err(MermaidRenderError::InvalidDiagram {
            message: String::from_utf8_lossy(payload).into_owned(),
        });
    }
    Ok(MermaidRendered {
        output: MermaidRenderedOutput::Svg(payload.to_vec()),
        cache_key: request.cache_key(),
        diagnostics: Vec::new(),
    })
}

fn terminate_worker(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Render a Mermaid request through the private native backend.
///
/// # Errors
///
/// Returns an error when:
///
/// * source or output exceeds configured byte/dimension bounds;
/// * requested dimensions or execution limits are invalid;
/// * source contains a Mermaid directive;
/// * cancellation is requested or the deadline expires;
/// * the backend rejects the diagram.
pub fn render_mermaid(
    request: &MermaidRenderRequest,
    cancellation: &MermaidCancellationToken,
) -> Result<MermaidRendered, MermaidRenderError> {
    let started = std::time::Instant::now();
    validate_request(request, cancellation)?;
    let _permit = RenderPermit::acquire(
        request.limits.max_concurrent_renders,
        request.limits.timeout,
        cancellation,
    )?;
    let elapsed = started.elapsed();
    let backend_timeout = request
        .limits
        .timeout
        .checked_sub(elapsed)
        .ok_or(MermaidRenderError::TimedOut)?;
    let source = request.source.as_str().to_owned();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let rendered = std::panic::catch_unwind(|| backend::render_svg(&source))
            .unwrap_or(Err(MermaidRenderError::BackendPanicked));
        let _ = sender.send(rendered);
    });
    let deadline = std::time::Instant::now() + backend_timeout;
    let svg = loop {
        if cancellation.is_cancelled() {
            return Err(MermaidRenderError::Cancelled);
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            return Err(MermaidRenderError::TimedOut);
        }
        let wait = deadline
            .saturating_duration_since(now)
            .min(Duration::from_millis(10));
        match receiver.recv_timeout(wait) {
            Ok(result) => break result?,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(MermaidRenderError::BackendPanicked);
            }
        }
    };
    if cancellation.is_cancelled() {
        return Err(MermaidRenderError::Cancelled);
    }
    if let Some((width, height)) = svg_dimensions(&svg)
        && (width > f64::from(request.width) || height > f64::from(request.height))
    {
        return Err(MermaidRenderError::OutputDimensionsExceeded);
    }
    if svg.len() > request.limits.max_output_bytes {
        return Err(MermaidRenderError::OutputTooLarge {
            actual: svg.len(),
            maximum: request.limits.max_output_bytes,
        });
    }
    Ok(MermaidRendered {
        output: MermaidRenderedOutput::Svg(svg.into_bytes()),
        cache_key: request.cache_key(),
        diagnostics: Vec::new(),
    })
}

fn validate_request(
    request: &MermaidRenderRequest,
    cancellation: &MermaidCancellationToken,
) -> Result<(), MermaidRenderError> {
    if cancellation.is_cancelled() {
        return Err(MermaidRenderError::Cancelled);
    }
    let source = request.source.as_str();
    if source.trim().is_empty() {
        return Err(MermaidRenderError::EmptySource);
    }
    if source.len() > request.limits.max_source_bytes {
        return Err(MermaidRenderError::SourceTooLarge {
            actual: source.len(),
            maximum: request.limits.max_source_bytes,
        });
    }
    if request.width == 0
        || request.height == 0
        || request.width > request.limits.max_width
        || request.height > request.limits.max_height
    {
        return Err(MermaidRenderError::InvalidDimensions);
    }
    if request.limits.timeout.is_zero() || request.limits.max_concurrent_renders == 0 {
        return Err(MermaidRenderError::InvalidExecutionLimits);
    }
    if source
        .lines()
        .any(|line| line.trim_start().starts_with("%%{"))
    {
        return Err(MermaidRenderError::DirectiveNotAllowed);
    }
    Ok(())
}

fn svg_dimensions(svg: &str) -> Option<(f64, f64)> {
    let tag_end = svg.find('>')?;
    let opening = &svg[..tag_end];
    let width = svg_attribute_number(opening, "width")?;
    let height = svg_attribute_number(opening, "height")?;
    Some((width, height))
}

fn svg_attribute_number(opening_tag: &str, name: &str) -> Option<f64> {
    let marker = format!("{name}=\"");
    let start = opening_tag.find(&marker)?.saturating_add(marker.len());
    let end = opening_tag[start..].find('"')?.saturating_add(start);
    let raw = opening_tag[start..end]
        .trim_end_matches("px")
        .trim_end_matches("pt");
    raw.parse().ok()
}

fn stable_source_hash(source: &str) -> u64 {
    const OFFSET: u64 = 14_695_981_039_346_656_037;
    const PRIME: u64 = 1_099_511_628_211;
    source.as_bytes().iter().fold(OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    })
}

mod backend {
    use super::MermaidRenderError;

    pub fn render_svg(source: &str) -> Result<String, MermaidRenderError> {
        mermaid_rs_renderer::render_strict(source, mermaid_rs_renderer::RenderOptions::default())
            .map_err(|error| MermaidRenderError::InvalidDiagram {
                message: error.to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_CACHE_BYTES, MAX_CACHE_ENTRIES, MermaidCancellationToken, MermaidRenderCache,
        MermaidRenderError, MermaidRenderRequest, MermaidRendered, MermaidRenderedOutput,
        render_mermaid,
    };

    fn cached_render(key: &str, bytes: usize) -> MermaidRendered {
        MermaidRendered {
            output: MermaidRenderedOutput::Svg(vec![b'x'; bytes]),
            cache_key: key.to_owned(),
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn renders_svg_without_exposing_backend_types() {
        let request = MermaidRenderRequest::svg("flowchart LR\nA --> B", 800, 600);
        let rendered = render_mermaid(&request, &MermaidCancellationToken::default()).unwrap();
        let MermaidRenderedOutput::Svg(svg) = rendered.output;
        let svg = String::from_utf8(svg).unwrap();
        assert!(svg.contains("<svg"));
        assert!(svg.contains('A'));
        assert_eq!(rendered.cache_key, request.cache_key());
    }

    #[test]
    fn rejects_directives_and_bounds() {
        let directive =
            MermaidRenderRequest::svg("%%{init: {}}%%\nflowchart LR\nA --> B", 800, 600);
        assert_eq!(
            render_mermaid(&directive, &MermaidCancellationToken::default()),
            Err(MermaidRenderError::DirectiveNotAllowed)
        );

        let mut oversized = MermaidRenderRequest::svg("flowchart LR\nA --> B", 800, 600);
        oversized.limits.max_source_bytes = 4;
        assert!(matches!(
            render_mermaid(&oversized, &MermaidCancellationToken::default()),
            Err(MermaidRenderError::SourceTooLarge { .. })
        ));

        let mut disabled = MermaidRenderRequest::svg("flowchart LR\nA --> B", 800, 600);
        disabled.limits.max_concurrent_renders = 0;
        assert_eq!(
            render_mermaid(&disabled, &MermaidCancellationToken::default()),
            Err(MermaidRenderError::InvalidExecutionLimits)
        );

        let too_small = MermaidRenderRequest::svg("flowchart LR\nA --> B", 1, 1);
        assert_eq!(
            render_mermaid(&too_small, &MermaidCancellationToken::default()),
            Err(MermaidRenderError::OutputDimensionsExceeded)
        );
    }

    #[test]
    fn bounded_cache_evicts_deterministic_least_recently_used_render() {
        let mut cache = MermaidRenderCache::default();
        for index in 0..MAX_CACHE_ENTRIES {
            cache.insert(cached_render(&format!("key-{index:03}"), 4));
        }
        assert!(cache.get("key-000").is_some());
        cache.insert(cached_render("key-new", 4));

        assert_eq!(cache.len(), MAX_CACHE_ENTRIES);
        assert!(cache.get("key-000").is_some());
        assert!(cache.get("key-001").is_none());
        assert!(cache.get("key-new").is_some());
    }

    #[test]
    fn bounded_cache_enforces_payload_limit_and_replacement_accounting() {
        let mut cache = MermaidRenderCache::default();
        cache.insert(cached_render("large", MAX_CACHE_BYTES));
        assert_eq!(cache.payload_bytes(), MAX_CACHE_BYTES);

        cache.insert(cached_render("small", 4));
        assert!(cache.get("large").is_none());
        assert!(cache.get("small").is_some());
        assert_eq!(cache.payload_bytes(), 4);

        cache.insert(cached_render("small", 7));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.payload_bytes(), 7);

        cache.insert(cached_render("oversized", MAX_CACHE_BYTES + 1));
        assert!(cache.get("oversized").is_none());
        assert_eq!(cache.payload_bytes(), 7);
    }

    #[test]
    fn cache_stores_only_explicit_successes_under_versioned_request_keys() {
        let request = MermaidRenderRequest::svg("flowchart LR\nA --> B", 800, 600);
        let rendered = render_mermaid(&request, &MermaidCancellationToken::default()).unwrap();
        let mut cache = MermaidRenderCache::default();
        cache.insert(rendered.clone());

        assert_eq!(cache.get(&request.cache_key()), Some(rendered));
        assert!(cache.get("mermaid-v0:obsolete").is_none());
    }

    #[test]
    fn cancellation_is_stable_and_backend_neutral() {
        let request = MermaidRenderRequest::svg("flowchart LR\nA --> B", 800, 600);
        let token = MermaidCancellationToken::default();
        token.cancel();
        assert_eq!(
            render_mermaid(&request, &token),
            Err(MermaidRenderError::Cancelled)
        );
    }

    #[test]
    fn tiny_deadline_returns_typed_timeout() {
        let mut request =
            MermaidRenderRequest::svg("flowchart LR\nA --> B\nB --> C\nC --> D\nD --> E", 800, 600);
        request.limits.timeout = std::time::Duration::from_nanos(1);
        assert_eq!(
            render_mermaid(&request, &MermaidCancellationToken::default()),
            Err(MermaidRenderError::TimedOut)
        );
    }
}
