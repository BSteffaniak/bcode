//! Bounded Mermaid rendering behind Bcode-owned request and result types.
//!
//! The concrete backend is private. Consumers must not depend on its types,
//! diagnostics, configuration, or output structures.

#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

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
    /// Maximum simultaneous renders allowed by an orchestrator.
    pub max_concurrent_renders: usize,
    /// Caller deadline contract. Synchronous backends validate cancellation
    /// before and after rendering; asynchronous orchestration owns preemption.
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
    /// Source contains a directive, which Bcode intentionally disallows.
    DirectiveNotAllowed,
    /// Rendering was cancelled.
    Cancelled,
    /// Backend rejected or could not render the diagram.
    InvalidDiagram { message: String },
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
            Self::DirectiveNotAllowed => formatter.write_str("Mermaid directives are not allowed"),
            Self::Cancelled => formatter.write_str("Mermaid rendering was cancelled"),
            Self::InvalidDiagram { message } => {
                write!(formatter, "invalid Mermaid diagram: {message}")
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

/// Render a Mermaid request through the private native backend.
///
/// # Errors
///
/// Returns an error when:
///
/// * source or output exceeds configured bounds;
/// * requested dimensions or execution limits are invalid;
/// * source contains a Mermaid directive;
/// * cancellation is requested;
/// * the backend rejects the diagram.
pub fn render_mermaid(
    request: &MermaidRenderRequest,
    cancellation: &MermaidCancellationToken,
) -> Result<MermaidRendered, MermaidRenderError> {
    validate_request(request, cancellation)?;
    let svg = backend::render_svg(request.source.as_str())?;
    if cancellation.is_cancelled() {
        return Err(MermaidRenderError::Cancelled);
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
        MermaidCancellationToken, MermaidRenderError, MermaidRenderRequest, MermaidRenderedOutput,
        render_mermaid,
    };

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
}
