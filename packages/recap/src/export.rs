//! Self-contained, script-optional HTML export of renderer-neutral recap documents.
//!
//! Asset roots must be private application-owned directories, not concurrently
//! writable by hostile processes. Portable filesystem checks reject symlinks and
//! escapes, but cannot make path lookup atomic against a hostile concurrent rename.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path};

use base64::Engine as _;
use sha2::{Digest as _, Sha256};

use crate::contracts::{Asset, Block, BlockContent, RecapDocument, TimeRange};

const CSS: &str = include_str!("export_assets/recap.css");
const JS: &str = include_str!("export_assets/recap.js");

/// Explicit limits for reads and the resulting embedded media (including base64 overhead).
#[derive(Clone, Copy, Debug)]
pub struct ExportBudget {
    /// Maximum raw bytes read from any one asset.
    pub max_asset_bytes: u64,
    /// Maximum combined data-URI bytes emitted, counting repeated references.
    pub max_total_media_bytes: u64,
    /// Long clips are omitted rather than silently truncated.
    pub max_clip_duration_ms: u64,
}

impl Default for ExportBudget {
    fn default() -> Self {
        Self {
            max_asset_bytes: 12 * 1024 * 1024,
            max_total_media_bytes: 48 * 1024 * 1024,
            max_clip_duration_ms: 30_000,
        }
    }
}

/// An explicit, path-free explanation of an export fallback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DegradationReason {
    /// The asset is missing or could not be read.
    UnreadableAsset,
    /// A symlink, special file, or escaping path was rejected.
    UnsafeAssetPath,
    /// File bytes do not match a passive, supported media format.
    UnsupportedMedia,
    /// A video is not a structurally recognized silent H.264 MP4.
    IncompatibleClip,
    /// The file exceeds the per-file read limit.
    AssetBudgetExceeded,
    /// Embedding would exceed the aggregate data-URI budget.
    TotalBudgetExceeded,
    /// The source interval exceeds the permitted clip duration.
    ClipTooLong,
    /// The source URL is not an absolute credential-free HTTP(S) URL.
    UnsafeSourceUrl,
}

impl DegradationReason {
    const fn description(self) -> &'static str {
        match self {
            Self::UnreadableAsset => "Media unavailable: the local asset could not be read.",
            Self::UnsafeAssetPath => "Media omitted: the local path was not safe to access.",
            Self::UnsupportedMedia => "Media omitted: unsupported or mismatched media format.",
            Self::IncompatibleClip => {
                "Clip omitted: a silent H.264 MP4 is required for portable playback."
            }
            Self::AssetBudgetExceeded => "Media omitted: the per-file size limit was reached.",
            Self::TotalBudgetExceeded => "Media omitted: the embedded-media budget was reached.",
            Self::ClipTooLong => "Clip omitted: the short-clip duration limit was exceeded.",
            Self::UnsafeSourceUrl => "Source link omitted: the URL was not safe to open.",
        }
    }
}

/// One fallback; captions and semantic content remain present in the HTML.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Degradation {
    /// The affected asset, or `None` for source-link degradation.
    pub asset_id: Option<String>,
    /// Machine-readable reason.
    pub reason: DegradationReason,
}

/// Export telemetry, distinct from the document's canonical state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportReport {
    /// Data-URI bytes emitted, including repeated media and subtitle tracks.
    pub embedded_media_bytes: u64,
    /// All omitted media and unsafe source-link fallbacks, in rendering order.
    pub degradations: Vec<Degradation>,
}

/// One portable HTML file and its explicit degradation report.
#[derive(Clone, Debug)]
pub struct HtmlExport {
    /// UTF-8 HTML, ready to write without companion files.
    pub html: String,
    /// Bounded-media accounting and fallbacks.
    pub report: ExportReport,
}

/// Failures that prevent export rather than degrade individual media.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// Invalid or unsupported renderer-neutral input.
    #[error("cannot export invalid recap document: {0}")]
    InvalidDocument(#[from] crate::contracts::Error),
    /// The caller did not supply an existing, nonsymlink directory.
    #[error("recap asset root must be an existing private directory, not a symlink")]
    InvalidAssetRoot,
}

/// Render an offline editorial recap with no model-generated markup or remote dependencies.
///
/// All content is escaped. Only explicit source links use HTTP(S), and only upon
/// activation. Media failures preserve captions and provenance and appear in the
/// report. A zero media budget produces a useful text-only recap.
///
/// # Errors
/// Returns an error for invalid/unsupported documents or an unsafe asset root.
/// The root is checked even for text-only exports; no files or directories are created.
pub fn export_html(
    document: &RecapDocument,
    asset_root: &Path,
    budget: ExportBudget,
) -> Result<HtmlExport, ExportError> {
    document.validate()?;
    let metadata = fs::symlink_metadata(asset_root).map_err(|_| ExportError::InvalidAssetRoot)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ExportError::InvalidAssetRoot);
    }
    let root = asset_root
        .canonicalize()
        .map_err(|_| ExportError::InvalidAssetRoot)?;
    let source_url = document.source.url.as_deref().and_then(safe_source_url);
    let mut report = ExportReport::default();
    if document.source.url.is_some() && source_url.is_none() {
        report.degradations.push(Degradation {
            asset_id: None,
            reason: DegradationReason::UnsafeSourceUrl,
        });
    }
    let assets = document
        .assets
        .iter()
        .map(|asset| (asset.id.as_str(), asset))
        .collect();
    let renderer = Renderer {
        document,
        root: &root,
        budget,
        report,
        source_url,
        assets,
    };
    Ok(render_document(renderer))
}

fn render_document(mut renderer: Renderer<'_>) -> HtmlExport {
    let document = renderer.document;
    let budget = renderer.budget;
    let css_hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(CSS.as_bytes()));
    let js_hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(JS.as_bytes()));
    let mut html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><meta name=\"referrer\" content=\"no-referrer\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src data:; media-src data:; style-src 'sha256-{css_hash}'; script-src 'sha256-{js_hash}'; connect-src 'none'; base-uri 'none'; form-action 'none'; object-src 'none'\"><title>",
    );
    escape(&mut html, &document.title);
    html.push_str(" — Recap</title><style>");
    html.push_str(CSS);
    html.push_str("</style></head><body><a class=\"skip\" href=\"#main\">Skip to recap</a><div class=\"shell\"><header class=\"masthead\"><span class=\"brand\">Recap / Visual field notes</span><span>One video. The essential ideas.</span></header><div class=\"hero\"><div class=\"eyebrow\">A visual video recap</div><h1>");
    escape(&mut html, &document.title);
    html.push_str("</h1><p class=\"lede\">");
    escape(&mut html, &document.source.title);
    html.push_str("</p><div class=\"source-meta\"><span>");
    escape(
        &mut html,
        &format!(
            "{} source · {} {}",
            timestamp(document.source.duration_ms),
            document.sections.len(),
            if document.sections.len() == 1 {
                "chapter"
            } else {
                "chapters"
            }
        ),
    );
    html.push_str("</span><span>Saved for offline reading</span>");
    if let Some(url) = &renderer.source_url {
        external_link(&mut html, url, "Open original video ↗", "");
    }
    html.push_str("</div></div><div class=\"layout\"><nav class=\"contents\" aria-label=\"Recap chapters\"><h2>In this recap</h2><ol>");
    for (index, section) in document.sections.iter().enumerate() {
        let _ = write!(html, "<li><a href=\"#chapter-{index}\">");
        escape(&mut html, &section.title);
        html.push_str("</a></li>");
    }
    html.push_str("</ol></nav><main id=\"main\">");
    for (index, section) in document.sections.iter().enumerate() {
        let _ = write!(
            html,
            "<section class=\"chapter\" id=\"chapter-{index}\" aria-labelledby=\"heading-{index}\"><div class=\"chapter-number\">CHAPTER {:02}</div><h2 id=\"heading-{index}\">",
            index + 1
        );
        escape(&mut html, &section.title);
        html.push_str("</h2>");
        for block in &section.blocks {
            renderer.block(&mut html, block);
        }
        html.push_str("</section>");
    }
    html.push_str("<aside class=\"notice\" aria-label=\"Export limits\"><strong>This offline edition</strong><p>This is a selected recap, not the complete source video. Media is embedded only when it passes format, path, and size checks. Browser playback support can vary.</p><p>");
    escape(
        &mut html,
        &format!(
            "Embedded media: {} / {} bytes (data URIs, including repeated references and caption tracks). Per-asset read limit: {} bytes. Maximum clip duration: {} ms. Unreferenced assets are not exported.",
            renderer.report.embedded_media_bytes,
            budget.max_total_media_bytes,
            budget.max_asset_bytes,
            budget.max_clip_duration_ms,
        ),
    );
    html.push_str("</p></aside>");
    if !renderer.report.degradations.is_empty() {
        html.push_str("<aside class=\"notice\" aria-label=\"Export limitations\"><strong>About this offline edition</strong><ul>");
        for item in &renderer.report.degradations {
            html.push_str("<li>");
            if let Some(id) = &item.asset_id {
                escape(&mut html, id);
                html.push_str(": ");
            }
            escape(&mut html, item.reason.description());
            html.push_str("</li>");
        }
        html.push_str("</ul></aside>");
    }
    html.push_str("</main></div><footer>Made with Recap · Successfully embedded images and short silent clips travel with this file; any export omissions are listed above. Source links require a network connection only when opened. Captions remain readable without playback.<noscript><p class=\"no-script\">The exported text and captions are available without JavaScript, including notices about omitted media. JavaScript only adds image enlargement and chapter highlighting.</p></noscript></footer></div><script>");
    html.push_str(JS);
    html.push_str("</script></body></html>");
    HtmlExport {
        html,
        report: renderer.report,
    }
}

struct Renderer<'a> {
    document: &'a RecapDocument,
    root: &'a Path,
    budget: ExportBudget,
    report: ExportReport,
    source_url: Option<url::Url>,
    assets: BTreeMap<&'a str, &'a Asset>,
}

impl Renderer<'_> {
    fn block(&mut self, html: &mut String, block: &Block) {
        match &block.content {
            BlockContent::Text { text } => {
                html.push_str("<div class=\"prose\"><p>");
                escape(html, text);
                html.push_str("</p></div>");
            }
            BlockContent::Image { asset_id, caption } => self.media(html, asset_id, caption, false),
            BlockContent::Clip { asset_id, caption } => self.media(html, asset_id, caption, true),
            BlockContent::Comparison { items } => {
                html.push_str("<div role=\"group\" aria-label=\"Visual comparison\">");
                for item in items {
                    self.media(html, &item.asset_id, &item.label, false);
                }
                html.push_str("</div>");
            }
        }
        for evidence in &self.document.evidence {
            if block.evidence_ids.contains(&evidence.id) {
                self.source_time(html, evidence.range);
            }
        }
    }

    fn media(&mut self, html: &mut String, asset_id: &str, caption: &str, clip: bool) {
        // Document validation has already checked every reference and its media kind.
        let Some(&asset) = self.assets.get(asset_id) else {
            return;
        };
        html.push_str("<figure class=\"visual\">");
        let track = clip.then(|| caption_track(caption, asset.range.end_ms - asset.range.start_ms));
        match self.embed(
            asset,
            clip,
            track.as_ref().map_or(0, |uri| uri.len() as u64),
        ) {
            Ok(uri) => {
                if clip {
                    html.push_str(
                        "<video controls muted playsinline preload=\"metadata\" aria-label=\"",
                    );
                    escape(html, caption);
                    html.push_str("\"><source src=\"");
                    html.push_str(&uri);
                    html.push_str("\" type=\"video/mp4\">");
                    if let Some(track) = &track {
                        html.push_str("<track kind=\"captions\" srclang=\"en\" label=\"Visual description\" default src=\"");
                        html.push_str(track);
                        html.push_str("\">");
                    }
                    html.push_str("Your browser cannot play this silent clip. The description and transcript follow.</video>");
                } else {
                    html.push_str("<img loading=\"lazy\" decoding=\"async\" src=\"");
                    html.push_str(&uri);
                    html.push_str("\" alt=\"");
                    escape(html, caption);
                    html.push_str("\">");
                }
            }
            Err(reason) => {
                self.report.degradations.push(Degradation {
                    asset_id: Some(asset_id.to_owned()),
                    reason,
                });
                html.push_str("<div class=\"unavailable\">");
                escape(html, reason.description());
                html.push_str("</div>");
            }
        }
        html.push_str("<figcaption><span class=\"caption-title\">");
        escape(html, caption);
        html.push_str("</span>");
        if clip {
            html.push_str("<span>Silent excerpt · </span>");
        }
        self.source_time(html, asset.range);
        html.push_str("</figcaption>");
        if clip {
            let segments: Vec<_> = self
                .document
                .transcript
                .iter()
                .filter(|segment| {
                    segment.range.start_ms < asset.range.end_ms
                        && segment.range.end_ms > asset.range.start_ms
                })
                .collect();
            if !segments.is_empty() {
                html.push_str("<details class=\"transcript\" open><summary>Words from this moment</summary><ol>");
                for segment in segments {
                    html.push_str("<li>");
                    self.source_time(html, segment.range);
                    escape(html, &segment.text);
                    html.push_str("</li>");
                }
                html.push_str("</ol></details>");
            }
        }
        html.push_str("</figure>");
    }

    fn source_time(&self, html: &mut String, range: TimeRange) {
        let precise = !range.start_ms.is_multiple_of(1000) || !range.end_ms.is_multiple_of(1000);
        let label = format!(
            "{}–{}",
            range_timestamp(range.start_ms, precise),
            range_timestamp(range.end_ms, precise)
        );
        if let Some(base) = &self.source_url {
            let mut url = base.clone();
            url.set_fragment(Some(&format!("t={}", range.start_ms / 1000)));
            if matches!(
                url.host_str(),
                Some("youtube.com" | "www.youtube.com" | "m.youtube.com" | "youtu.be")
            ) {
                let query: Vec<_> = url
                    .query_pairs()
                    .filter(|(key, _)| key != "t" && key != "start")
                    .map(|(key, value)| (key.into_owned(), value.into_owned()))
                    .collect();
                url.set_query(None);
                url.query_pairs_mut()
                    .extend_pairs(query)
                    .append_pair("t", &format!("{}s", range.start_ms / 1000));
            }
            external_link(html, &url, &label, "time");
        } else {
            html.push_str("<span class=\"time\">");
            escape(html, &label);
            html.push_str("</span>");
        }
    }

    fn embed(
        &mut self,
        asset: &Asset,
        clip: bool,
        track_bytes: u64,
    ) -> Result<String, DegradationReason> {
        if clip
            && asset.range.end_ms.saturating_sub(asset.range.start_ms)
                > self.budget.max_clip_duration_ms
        {
            return Err(DegradationReason::ClipTooLong);
        }
        let remaining = self
            .budget
            .max_total_media_bytes
            .saturating_sub(self.report.embedded_media_bytes)
            .saturating_sub(track_bytes);
        let prefix_len = u64::try_from(asset.mime_type.len())
            .unwrap_or(u64::MAX)
            .saturating_add(13);
        if remaining <= prefix_len {
            return Err(DegradationReason::TotalBudgetExceeded);
        }
        // Bound the read by both limits, allowing one byte to detect oversized files.
        let total_raw_limit = ((remaining - prefix_len) / 4).saturating_mul(3);
        let limit = self.budget.max_asset_bytes.min(total_raw_limit);
        let bytes = read_asset(self.root, asset.path.as_str(), limit).map_err(|reason| {
            if reason == DegradationReason::AssetBudgetExceeded
                && total_raw_limit < self.budget.max_asset_bytes
            {
                DegradationReason::TotalBudgetExceeded
            } else {
                reason
            }
        })?;
        if clip {
            if asset.mime_type != "video/mp4" || !compatible_mp4(&bytes) {
                return Err(DegradationReason::IncompatibleClip);
            }
        } else if !passive_image(&asset.mime_type, &bytes) {
            return Err(DegradationReason::UnsupportedMedia);
        }
        let uri = format!(
            "data:{};base64,{}",
            asset.mime_type,
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        let size = u64::try_from(uri.len()).unwrap_or(u64::MAX);
        if size > remaining {
            return Err(DegradationReason::TotalBudgetExceeded);
        }
        self.report.embedded_media_bytes += size + track_bytes;
        Ok(uri)
    }
}

fn read_asset(root: &Path, relative: &str, limit: u64) -> Result<Vec<u8>, DegradationReason> {
    let mut path = root.to_path_buf();
    // Defense in depth even though AssetPath already rejects traversal and encodings.
    crate::contracts::AssetPath::new(relative).map_err(|_| DegradationReason::UnsafeAssetPath)?;
    for component in Path::new(relative).components() {
        let Component::Normal(name) = component else {
            return Err(DegradationReason::UnsafeAssetPath);
        };
        path.push(name);
        let metadata =
            fs::symlink_metadata(&path).map_err(|_| DegradationReason::UnreadableAsset)?;
        if metadata.file_type().is_symlink() {
            return Err(DegradationReason::UnsafeAssetPath);
        }
    }
    let canonical = path
        .canonicalize()
        .map_err(|_| DegradationReason::UnreadableAsset)?;
    if !canonical.starts_with(root) {
        return Err(DegradationReason::UnsafeAssetPath);
    }
    let metadata =
        fs::symlink_metadata(&canonical).map_err(|_| DegradationReason::UnreadableAsset)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(DegradationReason::UnsafeAssetPath);
    }
    if metadata.len() > limit {
        return Err(DegradationReason::AssetBudgetExceeded);
    }
    let file = File::open(canonical).map_err(|_| DegradationReason::UnreadableAsset)?;
    if !file
        .metadata()
        .map_err(|_| DegradationReason::UnreadableAsset)?
        .is_file()
    {
        return Err(DegradationReason::UnsafeAssetPath);
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| DegradationReason::UnreadableAsset)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err(DegradationReason::AssetBudgetExceeded);
    }
    Ok(bytes)
}

fn passive_image(mime: &str, bytes: &[u8]) -> bool {
    match mime {
        "image/png" => {
            bytes.starts_with(b"\x89PNG\r\n\x1a\n") && bytes.get(12..16) == Some(b"IHDR")
        }
        "image/jpeg" => bytes.starts_with(b"\xff\xd8\xff") && bytes.ends_with(b"\xff\xd9"),
        "image/webp" => {
            bytes.starts_with(b"RIFF")
                && bytes.get(8..12) == Some(b"WEBP")
                && matches!(bytes.get(12..16), Some(b"VP8 " | b"VP8L" | b"VP8X"))
        }
        _ => false,
    }
}

fn safe_source_url(value: &str) -> Option<url::Url> {
    if value.chars().any(char::is_control) || value.trim() != value {
        return None;
    }
    let url = url::Url::parse(value).ok()?;
    (matches!(url.scheme(), "https" | "http")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

fn external_link(html: &mut String, url: &url::Url, label: &str, class: &str) {
    html.push_str("<a rel=\"noopener noreferrer\" target=\"_blank\" href=\"");
    escape(html, url.as_str());
    html.push_str("\" class=\"");
    escape(html, class);
    html.push_str("\" title=\"Open original source (network required)\">");
    escape(html, label);
    html.push_str("</a>");
}

fn escape(output: &mut String, text: &str) {
    for character in text.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#39;"),
            '\0' => output.push('\u{fffd}'),
            other => output.push(other),
        }
    }
}

fn caption_track(caption: &str, duration_ms: u64) -> String {
    // One escaped, single-line WebVTT cue cannot inject timing lines or cue markup.
    let mut text = String::new();
    escape(&mut text, &caption.replace(['\r', '\n'], " "));
    let end = format!(
        "{:02}:{:02}:{:02}.{:03}",
        duration_ms / 3_600_000,
        (duration_ms / 60_000) % 60,
        (duration_ms / 1000) % 60,
        duration_ms % 1000
    );
    let vtt = format!("WEBVTT\n\n00:00:00.000 --> {end}\n{text}\n");
    format!(
        "data:text/vtt;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(vtt)
    )
}

fn range_timestamp(ms: u64, precise: bool) -> String {
    if precise {
        format!("{}.{:03}", timestamp(ms), ms % 1000)
    } else {
        timestamp(ms)
    }
}

fn timestamp(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            (seconds / 60) % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

// Parse box boundaries rather than searching arbitrary payload bytes for codec names.
// Only known structural containers are descended, so depth is format-bounded.
fn mp4_boxes(mut bytes: &[u8], mut visit: impl FnMut(&[u8], &[u8]) -> bool) -> bool {
    while !bytes.is_empty() {
        if bytes.len() < 8 {
            return false;
        }
        let size = u32::from_be_bytes(bytes[..4].try_into().unwrap_or_default());
        let (size, header) = match size {
            0 => (bytes.len(), 8),
            1 => {
                let Some(wide) = bytes.get(8..16) else {
                    return false;
                };
                let Ok(size) =
                    usize::try_from(u64::from_be_bytes(wide.try_into().unwrap_or_default()))
                else {
                    return false;
                };
                (size, 16)
            }
            size => (size as usize, 8),
        };
        if size < header || size > bytes.len() || !visit(&bytes[4..8], &bytes[header..size]) {
            return false;
        }
        bytes = &bytes[size..];
    }
    true
}

fn compatible_mp4(bytes: &[u8]) -> bool {
    let (mut brand, mut movie, mut media, mut video) = (false, false, false, false);
    let valid = mp4_boxes(bytes, |kind, payload| {
        match kind {
            b"ftyp" => {
                brand = payload.len() >= 8
                    && matches!(
                        &payload[..4],
                        b"isom" | b"iso2" | b"mp41" | b"mp42" | b"avc1"
                    );
            }
            b"mdat" => media = !payload.is_empty(),
            b"moov" => {
                movie = true;
                return mp4_boxes(payload, |kind, payload| {
                    kind != b"trak" || compatible_track(payload, &mut video)
                });
            }
            _ => {}
        }
        true
    });
    valid && brand && movie && media && video
}

fn compatible_track(bytes: &[u8], video: &mut bool) -> bool {
    let (mut handler, mut codec) = (false, false);
    let valid = mp4_boxes(bytes, |kind, payload| {
        kind != b"mdia"
            || mp4_boxes(payload, |kind, payload| {
                match kind {
                    b"hdlr" => {
                        if payload.get(8..12) != Some(b"vide") {
                            return false;
                        }
                        handler = true;
                    }
                    b"minf" => {
                        return mp4_boxes(payload, |kind, payload| {
                            kind != b"stbl"
                                || mp4_boxes(payload, |kind, payload| {
                                    if kind != b"stsd" {
                                        return true;
                                    }
                                    let Some(entries) = payload.get(8..) else {
                                        return false;
                                    };
                                    mp4_boxes(entries, |kind, payload| {
                                        if !matches!(kind, b"avc1" | b"avc3") {
                                            return false;
                                        }
                                        let Some(extensions) = payload.get(78..) else {
                                            return false;
                                        };
                                        mp4_boxes(extensions, |kind, payload| {
                                            if kind == b"avcC" {
                                                codec = payload.len() >= 7
                                                    && payload[0] == 1
                                                    && matches!(payload[1], 66 | 77 | 100)
                                                    && payload[3] <= 52;
                                            }
                                            true
                                        })
                                    })
                                })
                        });
                    }
                    _ => {}
                }
                true
            })
    });
    *video |= handler && codec;
    valid && handler && codec
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{
        AssetPath, DOCUMENT_SCHEMA_VERSION, GenerationIntent, Section, Source, TranscriptSegment,
    };

    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aWQAAAABJRU5ErkJggg==";

    fn fixture() -> (tempfile::TempDir, RecapDocument) {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("frame.png"),
            base64::engine::general_purpose::STANDARD
                .decode(PNG)
                .unwrap(),
        )
        .unwrap();
        let document = RecapDocument {
            schema_version: DOCUMENT_SCHEMA_VERSION,
            id: "recap".into(),
            revision: 1,
            title: "The essential idea".into(),
            source: Source {
                id: "source".into(),
                title: "Original video".into(),
                url: Some("https://example.org/watch?v=123&lang=en".into()),
                duration_ms: 60_000,
            },
            intent: GenerationIntent::default(),
            transcript: vec![],
            evidence: vec![],
            assets: vec![Asset {
                id: "frame".into(),
                path: AssetPath::new("frame.png").unwrap(),
                mime_type: "image/png".into(),
                range: TimeRange {
                    start_ms: 1000,
                    end_ms: 2000,
                },
            }],
            sections: vec![Section {
                id: "chapter".into(),
                title: "A useful chapter".into(),
                blocks: vec![
                    Block {
                        id: "text".into(),
                        evidence_ids: vec![],
                        content: BlockContent::Text {
                            text: "A complete explanation, readable offline.".into(),
                        },
                    },
                    Block {
                        id: "visual".into(),
                        evidence_ids: vec![],
                        content: BlockContent::Image {
                            asset_id: "frame".into(),
                            caption: "The key visual".into(),
                        },
                    },
                ],
            }],
        };
        (root, document)
    }

    #[test]
    fn embeds_image_and_retains_all_content_without_script() {
        let (root, document) = fixture();
        let export = export_html(&document, root.path(), ExportBudget::default()).unwrap();
        assert!(export.report.degradations.is_empty());
        let uri = format!("data:image/png;base64,{PNG}");
        assert!(export.html.contains(&uri));
        assert_eq!(export.report.embedded_media_bytes, uri.len() as u64);
        // Inspect the actual product output before its optional enhancement script.
        let semantic = export.html.split("<script>").next().unwrap();
        assert!(semantic.contains("<main id=\"main\">"));
        assert!(semantic.contains("<h1>The essential idea</h1>"));
        assert!(semantic.contains("A complete explanation, readable offline."));
        assert!(semantic.contains("The key visual"));
        assert!(semantic.contains("href=\"#chapter-0\""));
        assert!(semantic.contains("1:00 source"));
        assert!(!semantic.contains("<iframe"));
        assert!(!semantic.contains("src=\"https:"));
        assert!(!semantic.contains("src=\"http:"));
        assert!(semantic.contains("connect-src 'none'"));
    }

    #[test]
    fn malicious_text_and_attribute_values_are_plain_content() {
        let (root, mut document) = fixture();
        let attack = "</script><script>alert('x')</script><img src=x onerror=\"attack()\"> &";
        document.title = attack.into();
        document.source.title = attack.into();
        document.sections[0].title = attack.into();
        document.sections[0].blocks[0].content = BlockContent::Text {
            text: attack.into(),
        };
        document.sections[0].blocks[1].content = BlockContent::Image {
            asset_id: "frame".into(),
            caption: attack.into(),
        };
        let export = export_html(&document, root.path(), ExportBudget::default()).unwrap();
        assert!(!export.html.contains(attack));
        assert!(!export.html.contains("<script>alert"));
        assert!(
            export
                .html
                .contains("&lt;/script&gt;&lt;script&gt;alert(&#39;x&#39;)")
        );
        assert!(export.html.contains("onerror=&quot;attack()&quot;"));
        assert!(export.html.contains("&amp;lang=en"));
    }

    #[test]
    fn dangerous_source_urls_never_become_links() {
        let (root, mut document) = fixture();
        for value in [
            "javascript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "file:///etc/passwd",
            "//evil.test/x",
            "https://user:secret@example.org",
            "https://example.org/\nattack",
            " https://example.org",
        ] {
            document.source.url = Some(value.into());
            let export = export_html(&document, root.path(), ExportBudget::default()).unwrap();
            assert_eq!(
                export.report.degradations[0].reason,
                DegradationReason::UnsafeSourceUrl
            );
            assert!(!export.html.contains("target=\"_blank\""));
            assert!(!export.html.contains("secret@"));
        }
        assert!(safe_source_url("https://example.org/video").is_some());
    }

    #[test]
    fn budgets_are_exact_and_repeated_media_is_counted() {
        let (root, mut document) = fixture();
        let original = export_html(&document, root.path(), ExportBudget::default()).unwrap();
        let size = original.report.embedded_media_bytes;
        let budget = ExportBudget {
            max_total_media_bytes: size,
            ..ExportBudget::default()
        };
        assert!(
            export_html(&document, root.path(), budget)
                .unwrap()
                .report
                .degradations
                .is_empty()
        );
        let budget = ExportBudget {
            max_total_media_bytes: size - 1,
            ..budget
        };
        let limited = export_html(&document, root.path(), budget).unwrap();
        assert_eq!(limited.report.embedded_media_bytes, 0);
        assert_eq!(
            limited.report.degradations[0].reason,
            DegradationReason::TotalBudgetExceeded
        );
        assert!(limited.html.contains("The key visual"));
        let mut repeated = document.sections[0].blocks[1].clone();
        repeated.id = "repeated".into();
        document.sections[0].blocks.push(repeated);
        let budget = ExportBudget {
            max_total_media_bytes: size,
            ..budget
        };
        let repeated = export_html(&document, root.path(), budget).unwrap();
        assert_eq!(repeated.report.embedded_media_bytes, size);
        assert_eq!(repeated.report.degradations.len(), 1);
        let per_file = export_html(
            &document,
            root.path(),
            ExportBudget {
                max_asset_bytes: 1,
                ..ExportBudget::default()
            },
        )
        .unwrap();
        assert!(
            per_file
                .report
                .degradations
                .iter()
                .all(|item| item.reason == DegradationReason::AssetBudgetExceeded)
        );
    }

    #[test]
    fn missing_and_mismatched_media_degrade_without_losing_captions() {
        let (root, document) = fixture();
        fs::write(
            root.path().join("frame.png"),
            b"<svg onload='attack()'></svg>",
        )
        .unwrap();
        let mismatch = export_html(&document, root.path(), ExportBudget::default()).unwrap();
        assert_eq!(
            mismatch.report.degradations[0].reason,
            DegradationReason::UnsupportedMedia
        );
        assert!(!mismatch.html.contains("data:image/png"));
        assert!(mismatch.html.contains("The key visual"));
        let absent = tempfile::tempdir().unwrap();
        let missing = export_html(&document, absent.path(), ExportBudget::default()).unwrap();
        assert_eq!(
            missing.report.degradations[0].reason,
            DegradationReason::UnreadableAsset
        );
    }

    #[test]
    fn unsafe_paths_are_rejected_before_access() {
        let root = tempfile::tempdir().unwrap();
        for path in [
            "../secret",
            "/etc/passwd",
            "a/../../secret",
            "a\\secret",
            "%2e%2e/secret",
            "https://example.org/file",
            "a//file",
            "./secret",
        ] {
            assert_eq!(
                read_asset(root.path(), path, 100).unwrap_err(),
                DegradationReason::UnsafeAssetPath
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_roots_files_and_parent_directories_are_rejected() {
        use std::os::unix::fs::symlink;
        let (root, document) = fixture();
        let outside = tempfile::tempdir().unwrap();
        let root_link = outside.path().join("root-link");
        symlink(root.path(), &root_link).unwrap();
        assert!(matches!(
            export_html(&document, &root_link, ExportBudget::default()),
            Err(ExportError::InvalidAssetRoot)
        ));
        symlink(
            root.path().join("frame.png"),
            outside.path().join("frame.png"),
        )
        .unwrap();
        let export = export_html(&document, outside.path(), ExportBudget::default()).unwrap();
        assert_eq!(
            export.report.degradations[0].reason,
            DegradationReason::UnsafeAssetPath
        );
        symlink(root.path(), outside.path().join("nested")).unwrap();
        assert_eq!(
            read_asset(outside.path(), "nested/frame.png", 1024).unwrap_err(),
            DegradationReason::UnsafeAssetPath
        );
    }

    fn boxed(kind: [u8; 4], bytes: &[u8]) -> Vec<u8> {
        let mut result = Vec::new();
        result.extend_from_slice(&u32::try_from(bytes.len() + 8).unwrap().to_be_bytes());
        result.extend_from_slice(&kind);
        result.extend_from_slice(bytes);
        result
    }

    fn mp4(audio: bool) -> Vec<u8> {
        let avcc = boxed(*b"avcC", &[1, 66, 0, 30, 255, 225, 0]);
        let mut visual = vec![0; 78];
        visual.extend(avcc);
        let mut descriptions = vec![0; 4];
        descriptions.extend_from_slice(&1_u32.to_be_bytes());
        descriptions.extend(boxed(*b"avc1", &visual));
        let stbl = boxed(*b"stbl", &boxed(*b"stsd", &descriptions));
        let mut handler = vec![0; 8];
        handler.extend_from_slice(if audio { b"soun" } else { b"vide" });
        let mut mdia = boxed(*b"hdlr", &handler);
        mdia.extend(boxed(*b"minf", &stbl));
        let track = boxed(*b"trak", &boxed(*b"mdia", &mdia));
        let mut result = boxed(*b"ftyp", b"isom\0\0\0\0isomavc1");
        result.extend(boxed(*b"moov", &track));
        result.extend(boxed(*b"mdat", &[1]));
        result
    }

    #[test]
    fn clip_container_policy_checks_structure_codec_audio_and_truncation() {
        assert!(compatible_mp4(&mp4(false)));
        assert!(!compatible_mp4(&mp4(true)));
        assert!(!compatible_mp4(b"ftypisom avc1 avcC vide"));
        let bytes = mp4(false);
        for length in 0..bytes.len() {
            assert!(!compatible_mp4(&bytes[..length]));
        }
        let mut incompatible = bytes;
        let offset = incompatible
            .windows(4)
            .position(|window| window == b"avc1")
            .unwrap();
        // First avc1 is a brand; change the actual sample entry as well.
        incompatible[offset..offset + 4].copy_from_slice(b"hvc1");
        let offset = incompatible
            .windows(4)
            .position(|window| window == b"avc1")
            .unwrap();
        incompatible[offset..offset + 4].copy_from_slice(b"hvc1");
        assert!(!compatible_mp4(&incompatible));
    }

    #[test]
    fn silent_clip_embeds_captions_transcript_and_budgeted_tracks() {
        let (root, mut document) = fixture();
        document.assets[0].path = AssetPath::new("clip.mp4").unwrap();
        document.assets[0].mime_type = "video/mp4".into();
        document.sections[0].blocks[1].content = BlockContent::Clip {
            asset_id: "frame".into(),
            caption: "A short visual demonstration".into(),
        };
        document.transcript.push(TranscriptSegment {
            id: "words".into(),
            range: document.assets[0].range,
            text: "The spoken explanation".into(),
        });
        fs::write(root.path().join("clip.mp4"), mp4(false)).unwrap();
        let export = export_html(&document, root.path(), ExportBudget::default()).unwrap();
        assert!(export.report.degradations.is_empty());
        assert!(export.html.contains("data:video/mp4;base64,"));
        assert!(export.html.contains("<video controls muted playsinline"));
        assert!(export.html.contains("<track kind=\"captions\""));
        assert!(export.html.contains("data:text/vtt;base64,"));
        assert!(export.html.contains("The spoken explanation"));
        assert!(export.html.contains("<details class=\"transcript\" open>"));
        let budget = ExportBudget {
            max_total_media_bytes: export.report.embedded_media_bytes - 1,
            ..ExportBudget::default()
        };
        let limited = export_html(&document, root.path(), budget).unwrap();
        assert_eq!(limited.report.embedded_media_bytes, 0);
        assert_eq!(
            limited.report.degradations[0].reason,
            DegradationReason::TotalBudgetExceeded
        );
        assert!(limited.html.contains("The spoken explanation"));
        let long = export_html(
            &document,
            root.path(),
            ExportBudget {
                max_clip_duration_ms: 1,
                ..ExportBudget::default()
            },
        )
        .unwrap();
        assert_eq!(
            long.report.degradations[0].reason,
            DegradationReason::ClipTooLong
        );
    }

    #[test]
    fn caption_tracks_cannot_inject_vtt_cues_or_html() {
        let uri = caption_track("</script>\n\n00:00:00.000 --> 99:00:00.000\n<v Evil>", 1000);
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(uri.strip_prefix("data:text/vtt;base64,").unwrap())
            .unwrap();
        let vtt = String::from_utf8(bytes).unwrap();
        assert_eq!(vtt.split("\n\n").count(), 2);
        assert!(!vtt.contains("<v Evil>"));
        assert!(vtt.contains("&lt;/script&gt;"));
    }

    #[test]
    fn chapter_count_and_subsecond_ranges_are_readable() {
        let (root, mut document) = fixture();
        document.assets[0].range = TimeRange {
            start_ms: 500,
            end_ms: 501,
        };
        let export = export_html(&document, root.path(), ExportBudget::default()).unwrap();
        assert!(export.html.contains("1:00 source · 1 chapter</span>"));
        assert!(!export.html.contains("1 chapters"));
        assert!(export.html.contains("0:00.500–0:00.501"));
        let mut second = document.sections[0].clone();
        second.id = "second".into();
        for block in &mut second.blocks {
            block.id.push_str("-second");
        }
        document.sections.push(second);
        let export = export_html(&document, root.path(), ExportBudget::default()).unwrap();
        assert!(export.html.contains("1:00 source · 2 chapters</span>"));
        assert_eq!(range_timestamp(3_600_001, true), "1:00:00.001");
        assert_eq!(range_timestamp(2000, false), "0:02");
    }

    #[test]
    fn edition_discloses_limits_and_hashes_only_bundled_code() {
        let (root, document) = fixture();
        let budget = ExportBudget {
            max_asset_bytes: 123,
            max_total_media_bytes: 456,
            max_clip_duration_ms: 789,
        };
        let export = export_html(&document, root.path(), budget).unwrap();
        assert!(export.html.contains("/ 456 bytes"));
        assert!(export.html.contains("Per-asset read limit: 123 bytes"));
        assert!(export.html.contains("Maximum clip duration: 789 ms"));
        assert!(export.html.contains("not the complete source video"));
        assert!(!export.html.contains("You're reading the complete recap"));
        assert!(!export.html.contains("unsafe-inline"));
        for (tag, payload) in [("style", CSS), ("script", JS)] {
            let hash = base64::engine::general_purpose::STANDARD
                .encode(Sha256::digest(payload.as_bytes()));
            assert!(export.html.contains(&format!("'sha256-{hash}'")));
            assert!(export.html.contains(&format!("<{tag}>{payload}</{tag}>")));
        }
    }

    #[test]
    fn timestamp_links_replace_old_youtube_offsets() {
        let (root, mut document) = fixture();
        document.source.url = Some("https://www.youtube.com/watch?v=abc&t=99s&start=88".into());
        let export = export_html(&document, root.path(), ExportBudget::default()).unwrap();
        assert!(
            export
                .html
                .contains("href=\"https://www.youtube.com/watch?v=abc&amp;t=1s#t=1\"")
        );
    }

    #[test]
    fn unsupported_document_versions_fail_instead_of_guessing() {
        let (root, mut document) = fixture();
        document.schema_version = u32::MAX;
        assert!(matches!(
            export_html(&document, root.path(), ExportBudget::default()),
            Err(ExportError::InvalidDocument(_))
        ));
    }
}
