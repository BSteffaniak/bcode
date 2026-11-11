//! Mermaid diagram rendering to bounded pixels for the transcript's Markdown visuals.

use bmux_mermaid_render::{MermaidCancellationToken, MermaidRenderedOutput};
use bmux_tui::image::{ImagePayload, ImagePixelFormat};

/// Rasterized Mermaid image suitable for BMUX protocol-neutral transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedMermaidImage {
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    /// RGBA8 pixels.
    pub rgba: Vec<u8>,
}

impl DecodedMermaidImage {
    /// The pixels as a BMUX protocol-neutral image payload.
    #[must_use]
    pub fn into_payload(self) -> ImagePayload {
        ImagePayload::Pixels {
            bytes: self.rgba,
            width: self.width,
            height: self.height,
            format: ImagePixelFormat::Rgba8,
        }
    }
}

/// Render one Mermaid diagram to bounded RGBA pixels on the calling thread.
///
/// One WASM-isolated render at a fixed 1600×1200 request size, then
/// [`rasterize_mermaid_svg`]. Callers run it on a blocking task outside the frame renderer;
/// `cache_key` must be the request's cache key (the contribution's `cache_key`), so a result
/// is never installed for another source.
///
/// # Errors
///
/// Returns a readable diagnostic when:
///
/// * `cache_key` does not match the source and size,
/// * the renderer fails, times out, exhausts its budget or is cancelled through
///   `cancellation`,
/// * the SVG cannot be rasterized within the fixed dimension and pixel limits.
pub fn render_mermaid_image(
    renderer: &bmux_mermaid_render::WasmMermaidRenderer,
    source: String,
    cache_key: &str,
    cancellation: &MermaidCancellationToken,
) -> Result<DecodedMermaidImage, String> {
    let request = bmux_mermaid_render::MermaidRenderRequest::svg(source, 1600, 1200);
    if request.cache_key() != cache_key {
        return Err("scheduled Mermaid cache key does not match its source".to_owned());
    }
    let output = renderer
        .render(&request, cancellation)
        .map_err(|error| error.to_string())?
        .output;
    let MermaidRenderedOutput::Svg(svg) = output;
    rasterize_mermaid_svg(&svg).map_err(|error| error.to_string())
}

/// SVG rasterization failure.
#[derive(Debug, thiserror::Error)]
pub enum MermaidImageError {
    /// SVG could not be parsed safely.
    #[error("invalid Mermaid SVG: {0}")]
    InvalidSvg(String),
    /// SVG dimensions are invalid or exceed fixed limits.
    #[error("Mermaid SVG dimensions are invalid")]
    InvalidDimensions,
    /// Pixel allocation failed.
    #[error("Mermaid image allocation failed")]
    AllocationFailed,
}

/// Convert successful worker SVG into bounded RGBA pixels for BMUX.
///
/// # Errors
///
/// Returns an error for malformed SVG, invalid dimensions, excessive decoded
/// pixels, or failed pixel allocation.
pub fn rasterize_mermaid_svg(svg: &[u8]) -> Result<DecodedMermaidImage, MermaidImageError> {
    const MAX_DIMENSION: u32 = 4096;
    const MAX_PIXELS: u64 = 16_000_000;

    let tree = resvg::usvg::Tree::from_data(svg, &resvg::usvg::Options::default())
        .map_err(|error| MermaidImageError::InvalidSvg(error.to_string()))?;
    let size = tree.size().to_int_size();
    let width = size.width();
    let height = size.height();
    if width == 0
        || height == 0
        || width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || u64::from(width).saturating_mul(u64::from(height)) > MAX_PIXELS
    {
        return Err(MermaidImageError::InvalidDimensions);
    }
    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(width, height).ok_or(MermaidImageError::AllocationFailed)?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );
    Ok(DecodedMermaidImage {
        width,
        height,
        rgba: pixmap.take(),
    })
}

/// Per-contribution Mermaid presentation state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownMermaidPresentationState {
    /// Rendering failed with a visible typed diagnostic.
    Failed(String),
}

impl MarkdownMermaidPresentationState {
    /// Return source-preserving terminal fallback text.
    #[must_use]
    pub fn fallback(&self, source: &str) -> String {
        match self {
            Self::Failed(diagnostic) => {
                format!("[Mermaid rendering failed: {diagnostic}]\n{source}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bmux_mermaid_render::{MermaidRenderRequest, MermaidRenderedOutput};

    #[test]
    fn embedded_renderer_produces_svg_for_tui_rasterization() {
        let request = MermaidRenderRequest::svg("flowchart LR\nA --> B", 800, 600);
        let rendered = bmux_mermaid_render::render_mermaid(
            &request,
            &bmux_mermaid_render::MermaidCancellationToken::default(),
        )
        .expect("embedded WASM render");
        let MermaidRenderedOutput::Svg(bytes) = rendered.output;
        let svg = std::str::from_utf8(&bytes).expect("UTF-8 SVG");
        assert!(svg.contains("<svg"));
        assert!(svg.contains("</svg>"));
    }
}
