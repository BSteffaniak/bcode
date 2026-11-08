//! Portable serialized contracts for dynamic TUI visual adapters.
//!
//! These models cross the native plugin service ABI and therefore must not depend on terminal
//! renderer implementation types.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Presentation value types carried by [`TuiVisualPresentation`].
pub use bcode_presentation_policy_models::{Disclosure, Preview, PreviewKeep, ResolvedSection};
use bcode_presentation_policy_models::{MAX_PATCH_ENTRIES, ResolvedPresentation};
use serde::{Deserialize, Serialize};

/// Versioned service interface for serialized TUI visual extensions.
pub const TUI_VISUAL_ADAPTER_INTERFACE_ID: &str = "bcode.tui-visual-adapter/v1";
/// Render one bounded visual through a serialized TUI extension.
pub const OP_RENDER_TUI_VISUAL: &str = "render";
/// Deliver one bounded artifact chunk through a serialized TUI extension.
pub const OP_DELIVER_TUI_VISUAL_ARTIFACT: &str = "artifact_chunk";
/// Earliest serialized TUI visual extension contract version accepted by this SDK.
pub const MIN_TUI_VISUAL_ADAPTER_CONTRACT_VERSION: u32 = 1;
/// Current serialized TUI visual extension contract version.
pub const TUI_VISUAL_ADAPTER_CONTRACT_VERSION: u32 = 5;
/// Earliest response version that may carry a tool presentation (summary, target, sections).
pub const TUI_VISUAL_TOOL_PRESENTATION_VERSION: u32 = 5;
/// Current renderer context version supplied to visual adapters.
///
/// * Version 1 carries width, diff layout, and working directory.
/// * Version 2 adds the theme fingerprint.
/// * Version 3 adds the resolved structural presentation ([`TuiVisualPresentation`]).
///
/// Contexts that omit the version decode as version 2.
pub const PLUGIN_TUI_VISUAL_CONTEXT_VERSION: u32 = 3;
/// Context version assumed when a serialized context omits `context_version`.
pub const LEGACY_TUI_VISUAL_CONTEXT_VERSION: u32 = 2;
/// Maximum UTF-8 bytes of a tool presentation summary.
pub const MAX_TUI_VISUAL_SUMMARY_BYTES: usize = 256;
/// Maximum UTF-8 bytes of a tool presentation target.
pub const MAX_TUI_VISUAL_TARGET_BYTES: usize = 256;
/// Maximum named sections in one tool presentation.
pub const MAX_TUI_VISUAL_SECTIONS: usize = 16;
/// Maximum UTF-8 bytes of a section name.
pub const MAX_TUI_VISUAL_SECTION_NAME_BYTES: usize = 64;
/// Maximum UTF-8 bytes of a section label.
pub const MAX_TUI_VISUAL_SECTION_LABEL_BYTES: usize = 64;
/// Maximum rows across all sections of one tool presentation.
pub const MAX_TUI_VISUAL_SECTION_ROWS: usize = MAX_SERIALIZED_TUI_VISUAL_ROWS;
/// Maximum resolved sections and header fields delivered in one render context.
pub const MAX_TUI_VISUAL_PRESENTATION_ENTRIES: usize = MAX_PATCH_ENTRIES;
/// Maximum rows accepted from one serialized visual response.
pub const MAX_SERIALIZED_TUI_VISUAL_ROWS: usize = 256;
/// Maximum spans accepted across one serialized visual response.
pub const MAX_SERIALIZED_TUI_VISUAL_SPANS: usize = 2_048;
/// Maximum UTF-8 bytes accepted across one serialized visual response.
pub const MAX_SERIALIZED_TUI_VISUAL_TEXT_BYTES: usize = 256 * 1024;

/// Stable terminal color token exposed by the serialized extension contract.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SerializedTuiColor {
    #[default]
    Reset,
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
    DarkGray,
    LightRed,
    LightGreen,
    LightYellow,
    LightBlue,
    LightMagenta,
    LightCyan,
    White,
}

/// Stable text modifier token exposed by the serialized extension contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SerializedTuiModifier {
    Bold,
    Dim,
    Italic,
    Underlined,
}

/// Stable renderer-neutral semantic style role exposed by contract version 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SerializedTuiStyleRole {
    Text,
    Muted,
    Accent,
    Info,
    Success,
    Warning,
    Error,
    DiffAdded,
    DiffRemoved,
    DiffHunk,
}

/// One bounded styled text span returned by a serialized extension.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedTuiSpan {
    pub text: String,
    /// Optional semantic role. When present, renderer-owned role presentation
    /// takes precedence over the compatibility foreground color.
    #[serde(default)]
    pub role: Option<SerializedTuiStyleRole>,
    #[serde(default)]
    pub foreground: SerializedTuiColor,
    #[serde(default)]
    pub modifiers: Vec<SerializedTuiModifier>,
}

/// One bounded terminal row returned by a serialized extension.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedTuiRow {
    pub spans: Vec<SerializedTuiSpan>,
}

/// Renderer-owned context supplied to a serialized extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedTuiVisualContext {
    pub width: u16,
    pub diff_layout: String,
    pub working_directory: Option<PathBuf>,
    /// Stable renderer-owned presentation identity. Plugins may use this only
    /// to invalidate derived presentation, never for execution semantics.
    #[serde(default)]
    pub theme_fingerprint: u64,
    /// Context version; see [`PLUGIN_TUI_VISUAL_CONTEXT_VERSION`].
    #[serde(default = "legacy_context_version")]
    pub context_version: u32,
    /// Resolved structural presentation, from context version 3. Read it through
    /// [`Self::presentation`].
    ///
    /// Decoding is tolerant: a presentation this build cannot decode (for example one
    /// carrying properties or values added by a newer host) decodes as `None`, so the
    /// adapter keeps its existing rendering instead of failing the whole render request.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "tolerant_presentation"
    )]
    pub presentation: Option<TuiVisualPresentation>,
}

const fn legacy_context_version() -> u32 {
    LEGACY_TUI_VISUAL_CONTEXT_VERSION
}

/// Decode an optional presentation, mapping any value this build cannot decode to `None`.
fn tolerant_presentation<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<TuiVisualPresentation>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

impl SerializedTuiVisualContext {
    /// Return the resolved presentation when the context version carries it.
    ///
    /// Contexts older than version 3 return `None`; adapters then keep their
    /// existing rendering semantics.
    #[must_use]
    pub const fn presentation(&self) -> Option<&TuiVisualPresentation> {
        if self.context_version >= PLUGIN_TUI_VISUAL_CONTEXT_VERSION {
            self.presentation.as_ref()
        } else {
            None
        }
    }
}

/// Host-resolved structural presentation for one unit, delivered to visual adapters.
///
/// Plugins render what the host resolved; they never evaluate presentation rules.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TuiVisualPresentation {
    /// Unit disclosure.
    pub disclosure: Disclosure,
    /// Unit preview budget; sections without their own preview use it.
    pub preview: Preview,
    /// Explicitly resolved sections; absent sections use
    /// [`ResolvedSection::defaults`] with [`Self::preview`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sections: BTreeMap<String, ResolvedSection>,
    /// Ordered header fields; empty selects the renderer's default header.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub header_fields: Vec<String>,
}

impl TuiVisualPresentation {
    /// Bounded adapter view of a resolved presentation.
    ///
    /// At most [`MAX_TUI_VISUAL_PRESENTATION_ENTRIES`] sections (in name order) and
    /// header fields are kept.
    #[must_use]
    pub fn from_resolved(resolved: &ResolvedPresentation) -> Self {
        Self {
            disclosure: resolved.disclosure,
            preview: resolved.preview,
            sections: resolved
                .sections
                .iter()
                .take(MAX_TUI_VISUAL_PRESENTATION_ENTRIES)
                .map(|(name, section)| (name.clone(), *section))
                .collect(),
            header_fields: resolved
                .header
                .as_ref()
                .map(|header| {
                    header
                        .fields
                        .iter()
                        .take(MAX_TUI_VISUAL_PRESENTATION_ENTRIES)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// Presentation of section `name`, defaulting to visible, expanded, and the unit preview.
    #[must_use]
    pub fn section(&self, name: &str) -> ResolvedSection {
        self.sections
            .get(name)
            .copied()
            .unwrap_or_else(|| ResolvedSection::defaults(self.preview))
    }
}

impl From<&ResolvedPresentation> for TuiVisualPresentation {
    fn from(resolved: &ResolvedPresentation) -> Self {
        Self::from_resolved(resolved)
    }
}

/// Bound producer text to one display line of at most `max_bytes` UTF-8 bytes.
///
/// Uses the first non-blank line, trims it, drops control characters, and truncates on a
/// character boundary. Returns an empty string when no visible text remains.
#[must_use]
pub fn bounded_line(text: &str, max_bytes: usize) -> String {
    let Some(line) = text.lines().map(str::trim).find(|line| !line.is_empty()) else {
        return String::new();
    };
    let mut bounded = String::with_capacity(line.len().min(max_bytes));
    for character in line.chars().filter(|character| !character.is_control()) {
        if bounded.len() + character.len_utf8() > max_bytes {
            break;
        }
        bounded.push(character);
    }
    bounded.truncate(bounded.trim_end().len());
    bounded
}

/// Normalize optional producer text with [`bounded_line`]; blank results become `None`.
#[must_use]
pub fn bounded_optional_line(text: Option<&str>, max_bytes: usize) -> Option<String> {
    text.map(|text| bounded_line(text, max_bytes))
        .filter(|text| !text.is_empty())
}

/// Whether `name` is a valid tool presentation section name.
///
/// Names are 1..=[`MAX_TUI_VISUAL_SECTION_NAME_BYTES`] bytes of lowercase ASCII letters,
/// digits, `_`, `-`, and `.`; dots separate non-empty namespace segments (`bcode.git.files`).
#[must_use]
pub fn valid_section_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_TUI_VISUAL_SECTION_NAME_BYTES
        && name.split('.').all(|segment| {
            !segment.is_empty()
                && segment.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'_' | b'-')
                })
        })
}

/// Validate the shape of tool presentation sections given as `(name, row count)` pairs.
///
/// # Errors
///
/// Returns an error when:
/// * more than [`MAX_TUI_VISUAL_SECTIONS`] sections are present;
/// * a name is invalid (see [`valid_section_name`]) or repeated;
/// * the sections carry more than [`MAX_TUI_VISUAL_SECTION_ROWS`] rows in total.
pub fn validate_tool_sections<'a>(
    sections: impl IntoIterator<Item = (&'a str, usize)>,
) -> Result<(), String> {
    let mut names = BTreeSet::new();
    let mut rows = 0_usize;
    for (name, row_count) in sections {
        if names.len() == MAX_TUI_VISUAL_SECTIONS {
            return Err("too many tool presentation sections".to_owned());
        }
        if !valid_section_name(name) || !names.insert(name) {
            return Err("invalid tool presentation section name".to_owned());
        }
        rows = rows.saturating_add(row_count);
        if rows > MAX_TUI_VISUAL_SECTION_ROWS {
            return Err("tool presentation section row limit exceeded".to_owned());
        }
    }
    Ok(())
}

/// One named, producer-rendered body of a tool unit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedTuiSection {
    /// Host section name (`output`, `diff`, ...) or a namespaced plugin name.
    pub name: String,
    /// Display label; `None` lets the renderer derive one from the name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Section body rows.
    #[serde(default)]
    pub rows: Vec<SerializedTuiRow>,
}

/// Producer-supplied structural presentation of one tool unit (contract version 5).
///
/// Every field is optional; adapters that supply none render one `visual` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SerializedTuiToolPresentation {
    /// One-line summary, at most [`MAX_TUI_VISUAL_SUMMARY_BYTES`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Operation target (path, command, pattern), at most [`MAX_TUI_VISUAL_TARGET_BYTES`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Ordered named sections.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<SerializedTuiSection>,
}

impl SerializedTuiToolPresentation {
    /// Whether no presentation field is supplied.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.summary.is_none() && self.target.is_none() && self.sections.is_empty()
    }

    /// Bound text fields in place: summary, target, and labels become single lines truncated
    /// on UTF-8 boundaries; blank values become `None`.
    pub fn normalize(&mut self) {
        self.summary = bounded_optional_line(self.summary.as_deref(), MAX_TUI_VISUAL_SUMMARY_BYTES);
        self.target = bounded_optional_line(self.target.as_deref(), MAX_TUI_VISUAL_TARGET_BYTES);
        for section in &mut self.sections {
            section.label =
                bounded_optional_line(section.label.as_deref(), MAX_TUI_VISUAL_SECTION_LABEL_BYTES);
        }
    }

    /// Validate structural bounds.
    ///
    /// # Errors
    ///
    /// Returns an error when sections violate [`validate_tool_sections`] or when text fields
    /// exceed their byte limits (call [`Self::normalize`] first to truncate them).
    pub fn validate(&self) -> Result<(), String> {
        if self
            .summary
            .as_ref()
            .is_some_and(|summary| summary.len() > MAX_TUI_VISUAL_SUMMARY_BYTES)
            || self
                .target
                .as_ref()
                .is_some_and(|target| target.len() > MAX_TUI_VISUAL_TARGET_BYTES)
            || self.sections.iter().any(|section| {
                section
                    .label
                    .as_ref()
                    .is_some_and(|label| label.len() > MAX_TUI_VISUAL_SECTION_LABEL_BYTES)
            })
        {
            return Err("tool presentation text exceeds its limit".to_owned());
        }
        validate_tool_sections(
            self.sections
                .iter()
                .map(|section| (section.name.as_str(), section.rows.len())),
        )
    }

    fn rows(&self) -> impl Iterator<Item = &SerializedTuiRow> {
        self.sections.iter().flat_map(|section| &section.rows)
    }
}

/// Request to render one exact manifest adapter through a dynamic plugin service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderTuiVisualRequest {
    pub version: u32,
    pub adapter_id: String,
    pub invocation_id: String,
    pub schema: String,
    pub schema_version: u32,
    pub payload: serde_json::Value,
    pub context: SerializedTuiVisualContext,
}

/// Stable, adapter-owned correspondence within one terminal visual.
///
/// Keys are scoped to the invocation and producer by the host. They identify
/// presentation content only and never authorize navigation or execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TuiVisualAnchor {
    pub key: String,
    pub row: usize,
    /// Version 4 source interval; coordinates belong to the adapter, not the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<TuiVisualSourceRange>,
}

/// Half-open source interval represented by a display row.
///
/// Empty source lines use equal endpoints. Identity is scoped by the host to producer and invocation
/// and must change when content correspondence is no longer trustworthy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TuiVisualSourceRange {
    pub identity: String,
    pub start: usize,
    pub end: usize,
}

/// Validate bounded correspondence before accepting a plugin layout.
///
/// # Errors
/// Returns an error for excessive, duplicate, empty, or out-of-range anchors.
pub fn validate_visual_anchors(anchors: &[TuiVisualAnchor], rows: usize) -> Result<(), String> {
    if anchors.len()
        > rows
            .saturating_mul(2)
            .min(MAX_SERIALIZED_TUI_VISUAL_ROWS.saturating_mul(2))
    {
        return Err("too many visual anchors".to_owned());
    }
    let mut keys = std::collections::BTreeSet::new();
    let mut ranges: std::collections::BTreeMap<&str, Vec<(usize, usize)>> =
        std::collections::BTreeMap::new();
    for anchor in anchors {
        if let Some(source) = &anchor.source {
            let previous = ranges.entry(&source.identity).or_default();
            if previous.iter().any(|&(start, end)| {
                source.start < end && start < source.end || start == source.start
            }) {
                return Err("overlapping visual source ranges".to_owned());
            }
            previous.push((source.start, source.end));
        }
        if anchor.key.is_empty()
            || anchor.key.len() > 256
            || anchor.row >= rows
            || anchor.source.as_ref().is_some_and(|source| {
                source.identity.is_empty()
                    || source.identity.len() > 256
                    || source.start > source.end
            })
            || !keys.insert(&anchor.key)
        {
            return Err("invalid visual anchor".to_owned());
        }
    }
    Ok(())
}

/// Successful serialized extension response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderTuiVisualResponse {
    pub version: u32,
    #[serde(default)]
    pub render_mode: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    pub rows: Vec<SerializedTuiRow>,
    /// Optional correspondence, supported from contract version 3.
    #[serde(default)]
    pub anchors: Vec<TuiVisualAnchor>,
}

impl RenderTuiVisualResponse {
    /// Validate response bounds before renderer conversion.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, invalid render modes, or excessive rows, spans,
    /// or text bytes.
    pub fn validate(&self) -> Result<(), String> {
        if !(MIN_TUI_VISUAL_ADAPTER_CONTRACT_VERSION..=TUI_VISUAL_ADAPTER_CONTRACT_VERSION)
            .contains(&self.version)
        {
            return Err(format!(
                "unsupported TUI visual response version {}",
                self.version
            ));
        }
        if self.version < 3 && !self.anchors.is_empty() {
            return Err("visual anchors require response version 3".to_owned());
        }
        if self.version < 4 && self.anchors.iter().any(|anchor| anchor.source.is_some()) {
            return Err("visual source ranges require response version 4".to_owned());
        }
        validate_visual_anchors(&self.anchors, self.rows.len())?;
        if self.version < 2
            && self
                .rows
                .iter()
                .flat_map(|row| &row.spans)
                .any(|span| span.role.is_some())
        {
            return Err("semantic TUI visual roles require response version 2".to_owned());
        }
        if !matches!(
            self.render_mode.as_str(),
            "" | "inline" | "transcript_block" | "full_block"
        ) {
            return Err("unsupported TUI visual render mode".to_owned());
        }
        if self.rows.len() > MAX_SERIALIZED_TUI_VISUAL_ROWS {
            return Err("serialized TUI visual row limit exceeded".to_owned());
        }
        validate_content_budget(&self.rows)
    }
}

/// Reject rows whose combined spans or text exceed the serialized content limits.
fn validate_content_budget<'a>(
    rows: impl IntoIterator<Item = &'a SerializedTuiRow>,
) -> Result<(), String> {
    let (span_count, text_bytes) = rows
        .into_iter()
        .flat_map(|row| &row.spans)
        .fold((0_usize, 0_usize), |(spans, bytes), span| {
            (spans + 1, bytes.saturating_add(span.text.len()))
        });
    if span_count > MAX_SERIALIZED_TUI_VISUAL_SPANS
        || text_bytes > MAX_SERIALIZED_TUI_VISUAL_TEXT_BYTES
    {
        return Err("serialized TUI visual content limit exceeded".to_owned());
    }
    Ok(())
}

/// Serialized response carrying an optional tool presentation (contract version 5).
///
/// The wire format is a [`RenderTuiVisualResponse`] with additional top-level `summary`,
/// `target`, and `sections` fields, so older responses decode with an empty presentation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderTuiVisualPresentedResponse {
    /// Rows, anchors, and header hints.
    #[serde(flatten)]
    pub response: RenderTuiVisualResponse,
    /// Producer-supplied structural presentation.
    #[serde(flatten)]
    pub presentation: SerializedTuiToolPresentation,
}

impl From<RenderTuiVisualResponse> for RenderTuiVisualPresentedResponse {
    fn from(response: RenderTuiVisualResponse) -> Self {
        Self {
            response,
            presentation: SerializedTuiToolPresentation::default(),
        }
    }
}

impl RenderTuiVisualPresentedResponse {
    /// Normalize presentation text, then validate the response and presentation bounds.
    ///
    /// # Errors
    ///
    /// Returns an error when:
    /// * the base response is invalid (see [`RenderTuiVisualResponse::validate`]);
    /// * a presentation is supplied by a response older than
    ///   [`TUI_VISUAL_TOOL_PRESENTATION_VERSION`];
    /// * the presentation violates [`SerializedTuiToolPresentation::validate`];
    /// * rows and section rows together exceed the span or text limits.
    pub fn normalize_and_validate(&mut self) -> Result<(), String> {
        self.response.validate()?;
        if self.presentation.is_empty() {
            return Ok(());
        }
        if self.response.version < TUI_VISUAL_TOOL_PRESENTATION_VERSION {
            return Err(format!(
                "tool presentation requires response version {TUI_VISUAL_TOOL_PRESENTATION_VERSION}"
            ));
        }
        self.presentation.normalize();
        self.presentation.validate()?;
        validate_content_budget(self.response.rows.iter().chain(self.presentation.rows()))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn source_ranges_require_v4_and_reject_ambiguous_overlap() {
        let anchor = TuiVisualAnchor {
            key: "row".to_owned(),
            row: 0,
            source: Some(TuiVisualSourceRange {
                identity: "capture:line".to_owned(),
                start: 0,
                end: 2,
            }),
        };
        let mut response = RenderTuiVisualResponse {
            version: 3,
            render_mode: String::new(),
            title: None,
            timeout_ms: None,
            rows: vec![SerializedTuiRow::default()],
            anchors: vec![anchor.clone()],
        };
        assert!(response.validate().is_err());
        response.version = 4;
        assert!(response.validate().is_ok());
        let mut duplicate = anchor;
        duplicate.key = "other".to_owned();
        response.anchors.push(duplicate);
        assert!(response.validate().is_err());
    }

    #[test]
    fn same_row_old_and_new_source_keys_are_valid() {
        let anchors = vec![
            TuiVisualAnchor {
                key: "old:0".to_owned(),
                source: None,
                row: 0,
            },
            TuiVisualAnchor {
                key: "new:0".to_owned(),
                source: None,
                row: 0,
            },
        ];
        assert!(validate_visual_anchors(&anchors, 1).is_ok());
        assert!(validate_visual_anchors(&anchors, 0).is_err());
    }

    #[test]
    fn visual_correspondence_is_bounded_and_versioned() {
        use super::*;
        let anchor = TuiVisualAnchor {
            key: "body".to_owned(),
            source: None,
            row: 0,
        };
        assert!(validate_visual_anchors(std::slice::from_ref(&anchor), 1).is_ok());
        assert!(validate_visual_anchors(std::slice::from_ref(&anchor), 0).is_err());
        assert!(validate_visual_anchors(&[anchor.clone(), anchor.clone()], 2).is_err());
        let mut response = RenderTuiVisualResponse {
            version: 3,
            render_mode: "full_block".to_owned(),
            title: None,
            timeout_ms: None,
            rows: vec![SerializedTuiRow::default()],
            anchors: vec![anchor],
        };
        assert!(response.validate().is_ok());
        response.version = 2;
        assert!(response.validate().is_err());
        response.anchors.clear();
        assert!(response.validate().is_ok());
        response.version = TUI_VISUAL_ADAPTER_CONTRACT_VERSION + 1;
        assert!(response.validate().is_err());
    }

    use super::*;

    #[test]
    fn version_one_concrete_color_response_remains_compatible() {
        let response = RenderTuiVisualResponse {
            anchors: Vec::new(),
            version: 1,
            render_mode: "inline".to_owned(),
            title: None,
            timeout_ms: None,
            rows: vec![SerializedTuiRow {
                spans: vec![SerializedTuiSpan {
                    text: "legacy".to_owned(),
                    role: None,
                    foreground: SerializedTuiColor::Green,
                    modifiers: vec![SerializedTuiModifier::Bold],
                }],
            }],
        };

        assert!(response.validate().is_ok());
        assert_eq!(
            response.rows[0].spans[0].foreground,
            SerializedTuiColor::Green
        );
    }

    #[test]
    fn semantic_roles_require_version_two_and_future_versions_fail_closed() {
        let semantic = RenderTuiVisualResponse {
            anchors: Vec::new(),
            version: 1,
            render_mode: "inline".to_owned(),
            title: None,
            timeout_ms: None,
            rows: vec![SerializedTuiRow {
                spans: vec![SerializedTuiSpan {
                    text: "semantic".to_owned(),
                    role: Some(SerializedTuiStyleRole::Warning),
                    foreground: SerializedTuiColor::Red,
                    modifiers: Vec::new(),
                }],
            }],
        };
        assert!(semantic.validate().is_err());

        let mut future = semantic;
        future.version = TUI_VISUAL_ADAPTER_CONTRACT_VERSION + 1;
        assert!(future.validate().is_err());
    }
}

/// Artifact update delivered to one exact serialized adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedTuiArtifactChunkRequest {
    pub version: u32,
    pub adapter_id: String,
    pub chunk: SerializedTuiArtifactChunk,
}

/// Portable bounded artifact bytes for a serialized adapter service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedTuiArtifactChunk {
    pub tool_call_id: String,
    pub artifact_id: String,
    pub reference_key: String,
    pub producer_plugin_id: String,
    pub schema: String,
    pub schema_version: u32,
    pub content_type: Option<String>,
    pub offset: u64,
    pub total_bytes: u64,
    pub revision: u64,
    pub finalized: bool,
    pub bytes: Vec<u8>,
}

#[cfg(test)]
mod presentation_tests {
    use super::*;
    use bcode_presentation_policy_models::HeaderSpec;

    fn preview() -> Preview {
        Preview {
            lines: 8,
            keep: PreviewKeep::Tail,
        }
    }

    fn row(text: &str) -> SerializedTuiRow {
        SerializedTuiRow {
            spans: vec![SerializedTuiSpan {
                text: text.to_owned(),
                ..SerializedTuiSpan::default()
            }],
        }
    }

    fn response(version: u32) -> RenderTuiVisualResponse {
        RenderTuiVisualResponse {
            version,
            render_mode: String::new(),
            title: None,
            timeout_ms: None,
            rows: vec![row("body")],
            anchors: Vec::new(),
        }
    }

    fn section(name: &str, rows: usize) -> SerializedTuiSection {
        SerializedTuiSection {
            name: name.to_owned(),
            label: None,
            rows: (0..rows).map(|index| row(&index.to_string())).collect(),
        }
    }

    #[test]
    fn bounded_line_keeps_one_line_on_utf8_boundaries() {
        assert_eq!(bounded_line("\n  \n  first \nsecond", 256), "first");
        assert_eq!(bounded_line("a\u{7}b\tc", 256), "abc");
        assert_eq!(bounded_line("界界", 5), "界");
        assert_eq!(bounded_line("👍🏽x", 4), "👍");
        assert_eq!(bounded_line("e\u{301}e\u{301}", 3), "e\u{301}");
        assert_eq!(bounded_line("ab cd", 3), "ab");
        assert_eq!(bounded_line("anything", 0), "");
        assert_eq!(bounded_line(" \n\t\n", 16), "");
        assert_eq!(bounded_optional_line(Some(" \n"), 16), None);
        assert_eq!(bounded_optional_line(Some("ok"), 16), Some("ok".to_owned()));
    }

    #[test]
    fn section_names_and_counts_are_bounded() {
        for name in ["output", "bcode.git.files", "a-b_c", "x1"] {
            assert!(valid_section_name(name), "{name}");
        }
        let long = "a".repeat(MAX_TUI_VISUAL_SECTION_NAME_BYTES + 1);
        for name in ["", "Output", ".x", "x.", "a..b", "a b", "界", long.as_str()] {
            assert!(!valid_section_name(name), "{name}");
        }
        let names = (0..=MAX_TUI_VISUAL_SECTIONS)
            .map(|index| format!("s{index}"))
            .collect::<Vec<_>>();
        let pairs = |count: usize| names.iter().take(count).map(|name| (name.as_str(), 0));
        assert!(validate_tool_sections(pairs(MAX_TUI_VISUAL_SECTIONS)).is_ok());
        assert!(validate_tool_sections(pairs(MAX_TUI_VISUAL_SECTIONS + 1)).is_err());
        assert!(validate_tool_sections([("output", 1), ("output", 1)]).is_err());
        assert!(validate_tool_sections([("a", MAX_TUI_VISUAL_SECTION_ROWS), ("b", 0)]).is_ok());
        assert!(validate_tool_sections([("a", MAX_TUI_VISUAL_SECTION_ROWS), ("b", 1)]).is_err());
    }

    #[test]
    fn presented_response_normalizes_text_and_requires_version_five() {
        let mut presented = RenderTuiVisualPresentedResponse {
            response: response(TUI_VISUAL_TOOL_PRESENTATION_VERSION),
            presentation: SerializedTuiToolPresentation {
                summary: Some(format!("{}\nmore", "s".repeat(300))),
                target: Some("  ".to_owned()),
                sections: vec![SerializedTuiSection {
                    label: Some("界".repeat(40)),
                    ..section("output", 2)
                }],
            },
        };
        assert!(presented.normalize_and_validate().is_ok());
        assert_eq!(
            presented.presentation.summary.as_deref().map(str::len),
            Some(MAX_TUI_VISUAL_SUMMARY_BYTES)
        );
        assert_eq!(presented.presentation.target, None);
        assert_eq!(
            presented.presentation.sections[0].label,
            Some("界".repeat(21))
        );

        let mut legacy = presented.clone();
        legacy.response.version = 4;
        assert!(legacy.normalize_and_validate().is_err());
        let mut plain = RenderTuiVisualPresentedResponse::from(response(1));
        assert!(plain.normalize_and_validate().is_ok());
        let mut future = presented;
        future.response.version = TUI_VISUAL_ADAPTER_CONTRACT_VERSION + 1;
        assert!(future.normalize_and_validate().is_err());
    }

    #[test]
    fn presented_response_rejects_invalid_sections_and_shared_budget_overflow() {
        let presented = |sections| RenderTuiVisualPresentedResponse {
            response: response(TUI_VISUAL_TOOL_PRESENTATION_VERSION),
            presentation: SerializedTuiToolPresentation {
                sections,
                ..SerializedTuiToolPresentation::default()
            },
        };
        assert!(
            presented(vec![section("Bad Name", 0)])
                .normalize_and_validate()
                .is_err()
        );
        assert!(
            presented(vec![section("output", 1), section("output", 1)])
                .normalize_and_validate()
                .is_err()
        );
        assert!(
            presented(vec![section("output", MAX_TUI_VISUAL_SECTION_ROWS + 1)])
                .normalize_and_validate()
                .is_err()
        );
        let wide = SerializedTuiRow {
            spans: vec![SerializedTuiSpan {
                text: "x".repeat(MAX_SERIALIZED_TUI_VISUAL_TEXT_BYTES),
                ..SerializedTuiSpan::default()
            }],
        };
        // The base rows already carry text, so a full-budget section overflows the shared limit.
        assert!(
            presented(vec![SerializedTuiSection {
                rows: vec![wide],
                ..section("output", 0)
            }])
            .normalize_and_validate()
            .is_err()
        );
    }

    #[test]
    fn presented_response_wire_format_is_backward_compatible() {
        let legacy = serde_json::to_value(response(4)).expect("encode legacy");
        let decoded: RenderTuiVisualPresentedResponse =
            serde_json::from_value(legacy).expect("legacy decodes as presented");
        assert!(decoded.presentation.is_empty());
        assert_eq!(decoded.response, response(4));

        let presented = RenderTuiVisualPresentedResponse {
            response: response(5),
            presentation: SerializedTuiToolPresentation {
                summary: Some("read 3 files".to_owned()),
                target: Some("src/lib.rs".to_owned()),
                sections: vec![section("output", 1)],
            },
        };
        let encoded = serde_json::to_value(&presented).expect("encode presented");
        assert_eq!(encoded["summary"], "read 3 files");
        assert_eq!(encoded["sections"][0]["name"], "output");
        let base: RenderTuiVisualResponse =
            serde_json::from_value(encoded.clone()).expect("older hosts ignore new fields");
        assert_eq!(base, response(5));
        let round_trip: RenderTuiVisualPresentedResponse =
            serde_json::from_value(encoded).expect("round trip");
        assert_eq!(round_trip, presented);
    }

    #[test]
    fn context_presentation_is_negotiated_by_context_version() {
        let legacy = serde_json::json!({
            "width": 80,
            "diff_layout": "Unified",
            "working_directory": null,
            "theme_fingerprint": 7,
        });
        let context: SerializedTuiVisualContext =
            serde_json::from_value(legacy).expect("legacy context decodes");
        assert_eq!(context.context_version, LEGACY_TUI_VISUAL_CONTEXT_VERSION);
        assert_eq!(context.presentation(), None);

        let presentation = TuiVisualPresentation {
            disclosure: Disclosure::Summary,
            preview: preview(),
            sections: BTreeMap::from([(
                "output".to_owned(),
                ResolvedSection {
                    disclosure: Disclosure::Collapsed,
                    ..ResolvedSection::defaults(preview())
                },
            )]),
            header_fields: vec!["marker".to_owned(), "tool".to_owned()],
        };
        let mut current = SerializedTuiVisualContext {
            presentation: Some(presentation.clone()),
            context_version: PLUGIN_TUI_VISUAL_CONTEXT_VERSION,
            ..context
        };
        let encoded = serde_json::to_string(&current).expect("encode context");
        let decoded: SerializedTuiVisualContext =
            serde_json::from_str(&encoded).expect("decode context");
        assert_eq!(decoded.presentation(), Some(&presentation));
        assert_eq!(
            decoded
                .presentation()
                .map(|value| value.section("output").disclosure),
            Some(Disclosure::Collapsed)
        );
        assert_eq!(
            decoded.presentation().map(|value| value.section("diff")),
            Some(ResolvedSection::defaults(preview()))
        );

        current.context_version = LEGACY_TUI_VISUAL_CONTEXT_VERSION;
        assert_eq!(current.presentation(), None);
    }

    #[test]
    fn newer_presentation_vocabulary_degrades_to_no_presentation() {
        let mut encoded = serde_json::to_value(SerializedTuiVisualContext {
            presentation: Some(TuiVisualPresentation {
                disclosure: Disclosure::Collapsed,
                preview: preview(),
                sections: BTreeMap::new(),
                header_fields: Vec::new(),
            }),
            context_version: PLUGIN_TUI_VISUAL_CONTEXT_VERSION,
            width: 80,
            diff_layout: "Unified".to_owned(),
            working_directory: None,
            theme_fingerprint: 3,
        })
        .expect("encode context");
        for future in [
            serde_json::json!({ "disclosure": "folded", "preview": encoded["presentation"]["preview"] }),
            serde_json::json!({ "disclosure": "collapsed", "preview": { "lines": 3, "keep": "head", "fade": true } }),
            serde_json::json!("not a table"),
        ] {
            encoded["presentation"] = future;
            encoded["context_version"] = serde_json::json!(PLUGIN_TUI_VISUAL_CONTEXT_VERSION + 1);
            let context: SerializedTuiVisualContext = serde_json::from_value(encoded.clone())
                .expect("the rest of the context still decodes");
            assert_eq!(context.width, 80);
            assert_eq!(context.theme_fingerprint, 3);
            assert_eq!(context.presentation(), None);
        }
    }

    #[test]
    fn presentation_from_resolved_is_bounded() {
        let mut resolved = ResolvedPresentation {
            disclosure: Disclosure::Collapsed,
            detail: bcode_presentation_policy_models::Detail::Normal,
            preview: preview(),
            group: bcode_presentation_policy_models::GroupSpec::none(),
            reveal: Vec::new(),
            placement: bcode_presentation_policy_models::Placement::default(),
            container: None,
            sections: (0..=MAX_TUI_VISUAL_PRESENTATION_ENTRIES)
                .map(|index| (format!("s{index:03}"), ResolvedSection::defaults(preview())))
                .collect(),
            header: None,
            summary: bcode_presentation_policy_models::SummarySpec::default(),
            spacing: bcode_presentation_policy_models::Spacing::default(),
            indent: 0,
            plugin: BTreeMap::new(),
        };
        let presentation = TuiVisualPresentation::from(&resolved);
        assert_eq!(presentation.disclosure, Disclosure::Collapsed);
        assert_eq!(
            presentation.sections.len(),
            MAX_TUI_VISUAL_PRESENTATION_ENTRIES
        );
        assert_eq!(presentation.header_fields, Vec::<String>::new());
        resolved.header = Some(HeaderSpec {
            fields: vec!["marker".to_owned(); MAX_TUI_VISUAL_PRESENTATION_ENTRIES + 1],
        });
        assert_eq!(
            TuiVisualPresentation::from(&resolved).header_fields.len(),
            MAX_TUI_VISUAL_PRESENTATION_ENTRIES
        );
    }
}
