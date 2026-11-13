//! Tool producers: tool invocations, historical requests, request drafts, contributions
//! without an accepted plugin visual, permissions and interactions (structure v2 unit
//! anatomy).
//!
//! Every tool-like item is described as a [`ToolAnatomy`] (header facts, an optional producer
//! summary and named sections) and laid out by [`compose`] from the unit's resolved
//! presentation, so disclosure, section visibility, previews and summaries are decided before
//! any component exists rather than trimmed afterwards:
//!
//! | Disclosure | Rows |
//! |---|---|
//! | `expanded` | header, then each visible section with its own disclosure |
//! | `summary` | header, then the one-line summary |
//! | `collapsed` | header only |
//! | `hidden` | a one-row marker (normally the unit is a group or turn marker instead) |
//!
//! A section renders a label row carrying its disclosure marker (`▾ output`), then its body
//! bounded by the section preview with a `… N output rows hidden …` row. A collapsed section
//! renders `▸ output · 14 rows`; a summarized one adds its first non-blank line. Consecutive
//! sections with one name (one `arguments` section per argument field) share one target.
//!
//! Geometry comes from the retained layout only (B3, B6, B12): the header row is the unit's
//! focus target (local `0`) and, unless the unit awaits a decision, its toggle hit region;
//! each section's first label row is a section focus target nested under it and a toggle hit
//! region (X1). Output text is source-mapped by byte ([`SourcePosition::ShellByte`]), diff
//! bodies by diff line ([`SourcePosition::DiffLine`]); argument, metadata and plugin rows are
//! rendered selection fragments. Running units reserve spinner and elapsed paint slots on the
//! header row (L5); nothing reads a clock.
//!
//! Status selects the theme container (`tool.<status>`), reshaped by chrome
//! `borders.tool` (`none` drops it, `left` draws a leading bar, framed styles draw a
//! full-width panel); the visibility floor (A16) is already applied to the resolved
//! presentation the producer reads. Tool output is untrusted (A15): control characters are
//! replaced byte-for-byte with spaces, argument fields are bounded, opaque payloads and
//! snapshots are summarized by size, never shown as raw JSON (E5).
//!
//! [`SourcePosition::ShellByte`]: crate::layout::SourcePosition::ShellByte
//! [`SourcePosition::DiffLine`]: crate::layout::SourcePosition::DiffLine

use std::borrow::Cow;
use std::ops::Range;
use std::sync::Arc;

use bcode_presentation_policy::models::{
    DEFAULT_UNIT_HEADER, Disclosure, Preview, PreviewKeep, ResolvedPresentation, SectionLabels,
    SummarySource, UnitFacts, UnitStatus,
};
use bcode_presentation_policy::{host_defaults, render_summary_template};
use bcode_session_models::ToolContributionPlacement;
use bcode_session_view_models::{
    InteractionViewState, InteractionViewSummary, PermissionView, ToolInvocationView,
    ToolInvocationViewStatus, ToolRequestDraftView, ToolResultView, ToolTimingView,
    TranscriptViewItem, TranscriptViewItemId, TranscriptViewItemKind,
};
use bcode_tui_components::transcript::{
    TranscriptContainerBorder, TranscriptContainerLayout, TranscriptContainerStyle,
    TranscriptContainerWidth,
};
use bcode_tui_theme::chrome::BorderStyle;
use bcode_tui_theme::definition::ResolvedThemeDefinition;
use bmux_tui::component::LogicalInsets;
use bmux_tui::composition::TextBlock;
use bmux_tui::style::Style;
use bmux_tui::text::{Line, Span, Text, TextWrap};

use super::common::{self, HeaderSegment, SourceText, TextSource};
use super::group::{
    ActivitySlots, DisclosureRow, ItemStack, marker_header_line, place, placed_width,
    raised_surface, role_style, status_title_role,
};
use super::visual::{
    ContentKeyed, ContentTargetKind, UnitTargetRow, VisualCorrespondence, VisualPlacements,
    VisualRows,
};
use super::{ItemContent, ItemRenderContext, ProducedItem, UnitProducer, UnitView};
use crate::units::structure::{DisclosureTarget, ResolvedUnitPresentation};

/// Most rows an inline shell screen shows (C39); larger previews are clamped.
pub const SHELL_MAX_ROWS: u32 = 128;
/// Argument fields shown before the rest are dropped.
pub const MAX_ARGUMENT_FIELDS: usize = 6;
/// Characters kept per argument value before an ellipsis.
pub const MAX_ARGUMENT_VALUE_CHARS: usize = 160;
/// Largest arguments document parsed for fields; larger ones show none (A15).
const MAX_ARGUMENT_BYTES: usize = 256 * 1024;
/// Section names whose first line is preferred for `first_line` summaries.
const SUMMARY_SECTIONS: [&str; 3] = ["output", "result", "visual"];
/// Lines scanned when deciding whether output is a unified diff.
const DIFF_SNIFF_LINES: usize = 64;
/// Columns section label rows are indented inside the unit.
const LABEL_INDENT: u64 = 2;
/// [`LABEL_INDENT`] as a repeat count.
const LABEL_INDENT_CELLS: usize = 2;
/// Columns section bodies are indented inside the unit.
const BODY_INDENT: u64 = 4;

// ---- anatomy -----------------------------------------------------------------------------

/// Header facts of a tool unit; [`compose`] formats the fields the presentation selects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeaderData {
    /// Surface label, for example `Tool` or `Permission`.
    pub label: Option<String>,
    /// Tool name.
    pub tool: Option<String>,
    /// Operation target (path, command, pattern), from the unit's `target` fact.
    pub target: Option<String>,
    /// Displayed status text.
    pub status_text: Option<String>,
    /// Lifecycle status (container, title style, activity slots).
    pub status: Option<UnitStatus>,
    /// Whether the unit reported an error.
    pub is_error: bool,
    /// Producer title (`title` field).
    pub title: Option<String>,
    /// Static timing phrase (`timeout 10s`, `duration 42ms`).
    pub timing: Option<String>,
    /// Start of a running call, for the elapsed paint slot.
    pub started_at_ms: Option<u64>,
    /// Duration of a finished call (`elapsed` field).
    pub duration_ms: Option<u64>,
    /// Producer title replacing the `label`, `tool`, `status` and `title` fields (plugin
    /// transcript-block visuals).
    pub title_override: Option<String>,
    /// The body draws its own header (plugin full-block visuals): the host header is painted
    /// only while the unit is not expanded, and the body's first row is the unit's focus target.
    pub self_drawn: bool,
}

impl HeaderData {
    /// Whether the unit is active (spinner and elapsed slots).
    #[must_use]
    pub const fn active(&self) -> bool {
        matches!(self.status, Some(UnitStatus::Running | UnitStatus::Waiting))
    }
}

/// Semantic role of a body, which selects its style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyRole {
    /// Tool output or result text.
    Output,
    /// Tool argument text.
    Argument,
    /// Metadata.
    Metadata,
    /// A failure notice.
    Failure,
}

impl BodyRole {
    const fn role(self) -> &'static str {
        match self {
            Self::Output => "tool.output",
            Self::Argument => "tool.argument",
            Self::Metadata => "tool.metadata",
            Self::Failure => "tool.failed.title",
        }
    }
}

/// Body of one anatomy section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SectionBody {
    /// Tool output bytes, one clipped row per source line, source-mapped by byte. Shell
    /// output (`shell` holds its content key, the identity of host-local horizontal scroll
    /// state) is an inline screen bounded by [`SHELL_MAX_ROWS`].
    Output {
        /// Output text.
        text: String,
        /// Content key of a shell screen.
        shell: Option<String>,
    },
    /// Rendered text, one clipped row per line (arguments, metadata).
    Rendered {
        /// Text.
        text: String,
        /// Style role.
        role: BodyRole,
    },
    /// One paragraph, word-wrapped by BMUX (confirmations, errors).
    Line {
        /// Text.
        text: String,
        /// Style role.
        role: BodyRole,
    },
    /// Unified diff text, one clipped row per diff line, source-mapped by diff line.
    Diff {
        /// Diff text.
        text: String,
    },
    /// Rows rendered by a plugin adapter (accepted, bounded), clipped at the body width.
    Rows {
        /// Rows.
        rows: Arc<[Line]>,
        /// Producer-namespaced content key (E3).
        key: String,
        /// Anchors and selection rows of `rows` (E3, C34; empty for named sections).
        correspondence: Arc<VisualCorrespondence>,
        /// Images and action regions of `rows` (E2, L8; empty for named sections).
        placements: Arc<VisualPlacements>,
    },
}

impl SectionBody {
    fn is_empty(&self) -> bool {
        match self {
            Self::Output { text, .. }
            | Self::Rendered { text, .. }
            | Self::Line { text, .. }
            | Self::Diff { text } => text.trim().is_empty(),
            Self::Rows { rows, .. } => rows.is_empty(),
        }
    }

    fn row_count(&self) -> usize {
        match self {
            Self::Output { text, .. } | Self::Rendered { text, .. } | Self::Diff { text } => {
                source_lines(text).len()
            }
            Self::Line { .. } => 1,
            Self::Rows { rows, .. } => rows.len(),
        }
    }

    fn first_line(&self) -> Option<String> {
        let first = |line: &str| {
            let line = line.trim();
            (!line.is_empty()).then(|| sanitize(line))
        };
        match self {
            Self::Output { text, .. }
            | Self::Rendered { text, .. }
            | Self::Line { text, .. }
            | Self::Diff { text } => text.lines().find_map(first),
            Self::Rows { rows, .. } => rows.iter().find_map(|row| first(&row.plain_text())),
        }
    }
}

/// One named, ordered section of a tool unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// Host or namespaced section name: the key for presentation and disclosure targets.
    pub name: String,
    /// Label row text; `None` paints the body without a label row.
    pub label: Option<String>,
    /// Body.
    pub body: SectionBody,
}

impl Section {
    /// A section under a label row.
    #[must_use]
    pub fn labelled(name: &str, label: &str, body: SectionBody) -> Self {
        Self {
            name: name.to_owned(),
            label: Some(label.to_owned()),
            body,
        }
    }

    /// A section without a label row.
    #[must_use]
    pub fn unlabelled(name: &str, body: SectionBody) -> Self {
        Self {
            name: name.to_owned(),
            label: None,
            body,
        }
    }

    fn label_text(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.name)
    }
}

/// A tool unit described for composition.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolAnatomy {
    /// Header facts.
    pub header: HeaderData,
    /// Producer-supplied one-line summary (`plugin` summary source).
    pub summary: Option<String>,
    /// Ordered sections.
    pub sections: Vec<Section>,
}

// ---- untrusted text ----------------------------------------------------------------------

/// Replace control characters with spaces byte-for-byte, so byte offsets into the original
/// text stay valid (A15).
fn sanitize(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            let replaced = c.is_control();
            let count = if replaced { c.len_utf8() } else { 1 };
            std::iter::repeat_n(if replaced { ' ' } else { c }, count)
        })
        .collect()
}

/// The newline-separated source text of consecutive `lines` of `text` (from [`source_lines`]):
/// the logical text a copy of their block reads (C34).
fn source_slice<'a>(text: &'a str, lines: &[(usize, &str)]) -> &'a str {
    match (lines.first(), lines.last()) {
        (Some((start, _)), Some((last, line))) => text.get(*start..last + line.len()).unwrap_or(""),
        _ => "",
    }
}

/// Source lines of `text` with their byte offsets; one trailing newline ends the last line
/// rather than starting an empty one.
fn source_lines(text: &str) -> Vec<(usize, &str)> {
    let body = text.strip_suffix('\n').unwrap_or(text);
    if body.is_empty() {
        return Vec::new();
    }
    let mut offset = 0;
    body.split('\n')
        .map(|line| {
            let start = offset;
            offset += line.len() + 1;
            (start, line)
        })
        .collect()
}

/// Kept line ranges of a `total`-line body under `preview`, and the number of hidden lines.
fn preview_ranges(total: usize, preview: Preview) -> (Vec<Range<usize>>, usize) {
    let max = usize::try_from(preview.lines).unwrap_or(usize::MAX);
    let mut ranges = Vec::with_capacity(2);
    if total <= max {
        ranges.push(0..total);
        return (ranges, 0);
    }
    match preview.keep {
        PreviewKeep::Head => ranges.push(0..max),
        PreviewKeep::Tail => ranges.push(total - max..total),
        PreviewKeep::Both => {
            let head = max.div_ceil(2);
            ranges.push(0..head);
            ranges.push(total - (max - head)..total);
        }
    }
    (
        ranges
            .into_iter()
            .filter(|range| !range.is_empty())
            .collect(),
        total - max,
    )
}

/// Readable argument fields: at most [`MAX_ARGUMENT_FIELDS`], values bounded to
/// [`MAX_ARGUMENT_VALUE_CHARS`]; nested values are summarized by size.
#[must_use]
pub fn argument_fields(arguments: &str) -> Vec<(String, String)> {
    if arguments.len() > MAX_ARGUMENT_BYTES {
        return Vec::new();
    }
    let Ok(serde_json::Value::Object(fields)) = serde_json::from_str(arguments) else {
        return Vec::new();
    };
    fields
        .into_iter()
        .take(MAX_ARGUMENT_FIELDS)
        .filter_map(|(label, value)| {
            let value = match value {
                serde_json::Value::String(value) => value,
                serde_json::Value::Number(value) => value.to_string(),
                serde_json::Value::Bool(value) => value.to_string(),
                serde_json::Value::Null => "none".to_owned(),
                serde_json::Value::Array(values) => format!("{} values", values.len()),
                serde_json::Value::Object(values) => format!("{} fields", values.len()),
            };
            let mut bounded = value
                .chars()
                .take(MAX_ARGUMENT_VALUE_CHARS)
                .collect::<String>();
            if value.chars().nth(MAX_ARGUMENT_VALUE_CHARS).is_some() {
                bounded.push('…');
            }
            (!bounded.is_empty()).then_some((label, bounded))
        })
        .collect()
}

/// Whether `text` reads as a unified diff (a hunk header plus a file header).
#[must_use]
pub fn looks_like_diff(text: &str) -> bool {
    let mut hunk = false;
    let mut file = false;
    for line in text.lines().take(DIFF_SNIFF_LINES) {
        hunk |= line.starts_with("@@ ");
        file |= line.starts_with("--- ") || line.starts_with("+++ ") || line.starts_with("diff ");
    }
    hunk && file
}

// ---- anatomy from items ------------------------------------------------------------------

/// Facts of the unit, when its structure was resolved.
#[must_use]
pub const fn unit_facts<'a>(cx: &ItemRenderContext<'a>) -> Option<&'a UnitFacts> {
    match cx.structure {
        ResolvedUnitPresentation::Item(item) => Some(&item.facts),
        _ => None,
    }
}

/// Status text of a tool unit.
#[must_use]
pub const fn tool_status_text(status: UnitStatus) -> &'static str {
    match status {
        UnitStatus::Succeeded => "finished",
        other => super::group::status_text(other),
    }
}

const fn invocation_status(tool: &ToolInvocationView) -> UnitStatus {
    let failed = matches!(tool.status, ToolInvocationViewStatus::Failed)
        || matches!(tool.is_error, Some(true));
    if matches!(tool.timing.timed_out, Some(true)) {
        return UnitStatus::TimedOut;
    }
    match tool.status {
        ToolInvocationViewStatus::Requested => UnitStatus::Requested,
        ToolInvocationViewStatus::Running => UnitStatus::Running,
        ToolInvocationViewStatus::Waiting => UnitStatus::Waiting,
        ToolInvocationViewStatus::Cancelled => UnitStatus::Cancelled,
        ToolInvocationViewStatus::Failed => UnitStatus::Failed,
        ToolInvocationViewStatus::Finished if failed => UnitStatus::Failed,
        ToolInvocationViewStatus::Finished => UnitStatus::Succeeded,
    }
}

/// Duration of a finished call.
fn finished_duration(timing: &ToolTimingView) -> Option<u64> {
    timing
        .duration_ms
        .or_else(|| Some(timing.finished_at_ms?.saturating_sub(timing.started_at_ms?)))
}

/// Static timing phrase: `timed out`, `timeout 10s` while active (or never finished), and
/// `duration 42ms` once finished. Elapsed time is a paint slot, never laid out (L5).
pub(super) fn timing_text(timing: &ToolTimingView, active: bool) -> Option<String> {
    let mut parts = Vec::new();
    if timing.timed_out == Some(true) {
        parts.push("timed out".to_owned());
    }
    if let Some(timeout_ms) = timing.timeout_ms
        && (active || timing.duration_ms.is_none() && timing.finished_at_ms.is_none())
    {
        parts.push(format!("timeout {}", common::elapsed_label(timeout_ms)));
    }
    if !active && let Some(duration) = finished_duration(timing) {
        parts.push(format!("duration {}", common::elapsed_label(duration)));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn argument_sections(arguments: Option<&str>) -> (Vec<Section>, bool) {
    let fields = arguments.map(argument_fields).unwrap_or_default();
    let shell = fields.iter().any(|(label, _)| label == "command");
    let sections = fields
        .into_iter()
        .map(|(label, value)| {
            Section::labelled(
                "arguments",
                &label,
                SectionBody::Rendered {
                    text: value,
                    role: BodyRole::Argument,
                },
            )
        })
        .collect();
    (sections, shell)
}

fn output_section(text: &str, shell: Option<String>) -> Section {
    if looks_like_diff(text) {
        Section::labelled(
            "diff",
            "diff",
            SectionBody::Diff {
                text: text.to_owned(),
            },
        )
    } else {
        Section::labelled(
            "output",
            "output",
            SectionBody::Output {
                text: text.to_owned(),
                shell,
            },
        )
    }
}

fn metadata_section(lines: &[String]) -> Option<Section> {
    let text = lines
        .iter()
        .filter(|line| !line.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then(|| {
        Section::labelled(
            "metadata",
            "metadata",
            SectionBody::Rendered {
                text,
                role: BodyRole::Metadata,
            },
        )
    })
}

fn result_sections(tool: &ToolInvocationView, shell: Option<&str>) -> Vec<Section> {
    let shell = shell.map(str::to_owned);
    match &tool.result {
        Some(ToolResultView::Text { text }) => vec![output_section(text, shell)],
        Some(ToolResultView::Json { value }) => vec![Section::labelled(
            "result",
            "result",
            SectionBody::Output {
                text: value.clone(),
                shell: None,
            },
        )],
        Some(ToolResultView::Artifact { artifact }) => {
            let artifact = &artifact.artifact;
            let title = artifact.title.as_deref().unwrap_or("artifact result");
            let mut sections = vec![Section::labelled(
                "artifact",
                "artifact",
                SectionBody::Rendered {
                    text: title.to_owned(),
                    role: BodyRole::Output,
                },
            )];
            sections.extend(metadata_section(&[
                format!("producer {}", artifact.producer_plugin_id),
                format!("schema {} v{}", artifact.schema, artifact.schema_version),
                match artifact.refs.len() {
                    0 => String::new(),
                    1 => "1 reference".to_owned(),
                    count => format!("{count} references"),
                },
            ]));
            if let Some(text) = tool.result_text.as_deref() {
                let trimmed = text.trim();
                if !trimmed.is_empty() && trimmed != title && !trimmed.contains(&artifact.schema) {
                    sections.push(output_section(text, shell));
                }
            }
            sections
        }
        None => tool
            .result_text
            .as_deref()
            .map(|text| vec![output_section(text, shell)])
            .unwrap_or_default(),
    }
}

fn error_section(status: UnitStatus) -> Option<Section> {
    let text = match status {
        UnitStatus::Failed => "tool failed",
        UnitStatus::TimedOut => "tool timed out",
        _ => return None,
    };
    Some(Section::unlabelled(
        "error",
        SectionBody::Line {
            text: text.to_owned(),
            role: BodyRole::Failure,
        },
    ))
}

fn invocation_anatomy(
    tool: &ToolInvocationView,
    label: &str,
    facts: Option<&UnitFacts>,
) -> ToolAnatomy {
    let status = facts
        .and_then(|facts| facts.status)
        .unwrap_or_else(|| invocation_status(tool));
    let active = matches!(status, UnitStatus::Running | UnitStatus::Waiting);
    let (mut sections, shell) = argument_sections(tool.arguments_json.as_deref());
    let shell_key = shell.then(|| format!("shell:{}", tool.tool_call_id));
    sections.extend(result_sections(tool, shell_key.as_deref()));
    sections.extend(error_section(status));
    ToolAnatomy {
        header: HeaderData {
            label: Some(label.to_owned()),
            tool: tool.tool_name.clone(),
            target: facts.and_then(|facts| facts.target.clone()),
            status_text: Some(tool_status_text(status).to_owned()),
            status: Some(status),
            is_error: facts.map_or(matches!(status, UnitStatus::Failed), |facts| facts.is_error),
            title: None,
            timing: timing_text(&tool.timing, active),
            started_at_ms: tool.timing.started_at_ms.filter(|_| active),
            duration_ms: finished_duration(&tool.timing).filter(|_| !active),
            ..HeaderData::default()
        },
        summary: None,
        sections,
    }
}

fn draft_anatomy(
    draft: &ToolRequestDraftView,
    streaming: bool,
    facts: Option<&UnitFacts>,
) -> ToolAnatomy {
    let status = facts
        .and_then(|facts| facts.status)
        .unwrap_or(if streaming {
            UnitStatus::Running
        } else {
            UnitStatus::Requested
        });
    let mut sections = vec![Section::labelled(
        "arguments",
        "arguments",
        SectionBody::Rendered {
            text: draft.preview.clone(),
            role: BodyRole::Argument,
        },
    )];
    sections.extend(metadata_section(&[format!(
        "{} argument bytes{}",
        draft.argument_bytes,
        if draft.truncated {
            " · preview truncated"
        } else {
            ""
        }
    )]));
    ToolAnatomy {
        header: HeaderData {
            label: Some("Tool request".to_owned()),
            tool: Some(draft.tool_name.clone()),
            target: facts.and_then(|facts| facts.target.clone()),
            status_text: Some(if streaming { "drafting" } else { "requested" }.to_owned()),
            status: Some(status),
            ..HeaderData::default()
        },
        summary: None,
        sections,
    }
}

fn contribution_anatomy(
    contribution: &bcode_session_models::ToolContributionEvent,
    placement: ToolContributionPlacement,
    invocation: Option<&ToolInvocationView>,
    facts: Option<&UnitFacts>,
) -> ToolAnatomy {
    let label = match placement {
        ToolContributionPlacement::Request => "Tool request",
        ToolContributionPlacement::Progress => "Tool progress",
        ToolContributionPlacement::Result => "Tool result",
        ToolContributionPlacement::Supplemental | ToolContributionPlacement::Hidden => "Tool",
    };
    // Compact bounded fallback for an unknown schema (E5): canonical invocation facts and the
    // contribution's identity, never its opaque payload.
    let payload = match &contribution.payload {
        serde_json::Value::Object(fields) => format!("payload {} fields", fields.len()),
        serde_json::Value::Array(values) => format!("payload {} values", values.len()),
        serde_json::Value::Null => String::new(),
        _ => "payload 1 value".to_owned(),
    };
    let metadata = metadata_section(&[
        format!("producer {}", contribution.producer_id),
        format!(
            "schema {} v{}",
            contribution.schema, contribution.schema_version
        ),
        payload,
    ]);
    let mut anatomy = invocation.map_or_else(
        || ToolAnatomy {
            header: HeaderData {
                label: Some(label.to_owned()),
                title: Some(contribution.schema.clone()),
                status: facts.and_then(|facts| facts.status),
                status_text: facts
                    .and_then(|facts| facts.status)
                    .map(|status| tool_status_text(status).to_owned()),
                ..HeaderData::default()
            },
            ..ToolAnatomy::default()
        },
        |tool| invocation_anatomy(tool, label, facts),
    );
    let error = anatomy
        .sections
        .iter()
        .position(|section| section.name == "error")
        .unwrap_or(anatomy.sections.len());
    anatomy.sections.splice(error..error, metadata);
    anatomy
}

const fn permission_status(permission: &PermissionView) -> UnitStatus {
    match (permission.resolved, permission.approved) {
        (false, _) => UnitStatus::Waiting,
        (true, Some(true)) => UnitStatus::Succeeded,
        (true, _) => UnitStatus::Cancelled,
    }
}

fn permission_anatomy(permission: &PermissionView, facts: Option<&UnitFacts>) -> ToolAnatomy {
    let status = facts
        .and_then(|facts| facts.status)
        .unwrap_or_else(|| permission_status(permission));
    let decision = match (permission.resolved, permission.approved) {
        (false, _) => "awaiting decision",
        (true, Some(true)) => "approved",
        (true, _) => "denied",
    };
    let (mut sections, _) = argument_sections(Some(&permission.arguments_json));
    sections.extend(metadata_section(&[
        permission
            .policy_source
            .as_ref()
            .map(|source| format!("policy {source}"))
            .unwrap_or_default(),
        if permission.agent_id.is_empty() {
            String::new()
        } else {
            format!("agent {}", permission.agent_id)
        },
        permission
            .batch
            .as_ref()
            .map(|batch| format!("call {} of {}", batch.call_index + 1, batch.call_count))
            .unwrap_or_default(),
    ]));
    let confirmation = permission
        .detail
        .as_deref()
        .filter(|detail| !detail.trim().is_empty())
        .map_or_else(
            || decision.to_owned(),
            |detail| format!("{decision}: {detail}"),
        );
    sections.push(Section::unlabelled(
        "confirmation",
        SectionBody::Line {
            text: confirmation,
            role: BodyRole::Output,
        },
    ));
    ToolAnatomy {
        header: HeaderData {
            // An open request asks for a decision; a resolved one records it (as in V1).
            label: Some(
                if permission.resolved {
                    "Permission"
                } else {
                    "Permission required"
                }
                .to_owned(),
            ),
            tool: (!permission.tool_name.is_empty()).then(|| permission.tool_name.clone()),
            target: facts.and_then(|facts| facts.target.clone()),
            status_text: Some(decision.to_owned()),
            status: Some(status),
            is_error: false,
            title: permission.title.clone(),
            timing: None,
            started_at_ms: None,
            duration_ms: None,
            ..HeaderData::default()
        },
        summary: None,
        sections,
    }
}

const fn interaction_state(state: InteractionViewState) -> (UnitStatus, &'static str) {
    match state {
        InteractionViewState::Pending => (UnitStatus::Waiting, "awaiting input"),
        InteractionViewState::Submitting => (UnitStatus::Running, "submitting"),
        InteractionViewState::ValidationError => (UnitStatus::Failed, "invalid input"),
        InteractionViewState::ActionError => (UnitStatus::Failed, "action failed"),
        InteractionViewState::Resolved => (UnitStatus::Succeeded, "resolved"),
        InteractionViewState::Cancelled => (UnitStatus::Cancelled, "cancelled"),
    }
}

fn interaction_anatomy(
    interaction: &InteractionViewSummary,
    facts: Option<&UnitFacts>,
) -> ToolAnatomy {
    let (status, text) = interaction_state(interaction.state);
    let status = facts.and_then(|facts| facts.status).unwrap_or(status);
    if let Some(anatomy) = prose_interaction_anatomy(interaction, facts, status, text) {
        return anatomy;
    }
    let snapshot = match &interaction.snapshot {
        Some(serde_json::Value::Object(fields)) => format!("snapshot {} fields", fields.len()),
        Some(serde_json::Value::Array(values)) => format!("snapshot {} values", values.len()),
        Some(serde_json::Value::Null) | None => String::new(),
        Some(_) => "snapshot 1 value".to_owned(),
    };
    let mut sections = Vec::new();
    sections.extend(metadata_section(&[
        format!("kind {}", interaction.kind),
        if interaction.required {
            "response required".to_owned()
        } else {
            String::new()
        },
        interaction
            .producer_id
            .as_ref()
            .map(|producer| format!("producer {producer}"))
            .unwrap_or_default(),
        interaction
            .exchange_schema
            .as_ref()
            .map(|schema| {
                interaction.exchange_schema_version.map_or_else(
                    || format!("schema {schema}"),
                    |v| format!("schema {schema} v{v}"),
                )
            })
            .unwrap_or_default(),
        snapshot,
    ]));
    let confirmation = interaction
        .status_detail
        .as_deref()
        .filter(|detail| !detail.trim().is_empty())
        .map_or_else(|| text.to_owned(), |detail| format!("{text}: {detail}"));
    sections.push(Section::unlabelled(
        "confirmation",
        SectionBody::Line {
            text: confirmation,
            role: if matches!(status, UnitStatus::Failed) {
                BodyRole::Failure
            } else {
                BodyRole::Output
            },
        },
    ));
    ToolAnatomy {
        header: HeaderData {
            label: Some("Interaction".to_owned()),
            tool: None,
            target: facts.and_then(|facts| facts.target.clone()),
            status_text: Some(text.to_owned()),
            status: Some(status),
            is_error: matches!(status, UnitStatus::Failed),
            title: interaction.title.clone(),
            timing: None,
            started_at_ms: None,
            duration_ms: None,
            ..HeaderData::default()
        },
        summary: None,
        sections,
    }
}

/// Anatomy of an interaction whose title is prose (several lines, such as a question with its
/// options): every line is a wrapped paragraph of the body, and the status detail (an answer,
/// say) stays in the header where a collapsed unit still shows it.
fn prose_interaction_anatomy(
    interaction: &InteractionViewSummary,
    facts: Option<&UnitFacts>,
    status: UnitStatus,
    state_text: &str,
) -> Option<ToolAnatomy> {
    let prose = interaction
        .title
        .as_deref()
        .filter(|title| title.contains('\n'))?;
    let sections = prose
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(index, line)| {
            Section::unlabelled(
                &format!("prose.{index}"),
                SectionBody::Line {
                    text: line.trim_end().to_owned(),
                    role: BodyRole::Output,
                },
            )
        })
        .collect();
    let status_text = interaction
        .status_detail
        .as_deref()
        .filter(|detail| !detail.trim().is_empty())
        .unwrap_or(state_text);
    Some(ToolAnatomy {
        header: HeaderData {
            label: interaction
                .kind
                .contains("question")
                .then(|| "Question".to_owned()),
            tool: None,
            target: facts.and_then(|facts| facts.target.clone()),
            status_text: Some(status_text.to_owned()),
            status: Some(status),
            is_error: matches!(status, UnitStatus::Failed),
            title: None,
            ..HeaderData::default()
        },
        summary: None,
        sections,
    })
}

/// Anatomy of a tool-like item (invocations, requests, drafts, contributions, permissions,
/// interactions). Other items get a header-only anatomy labelled by kind.
#[must_use]
pub fn tool_anatomy(item: &TranscriptViewItem, facts: Option<&UnitFacts>) -> ToolAnatomy {
    match &item.kind {
        // A recorded result relabels the invocation, as the canonical V1 header does.
        TranscriptViewItemKind::ToolInvocation { tool } => invocation_anatomy(
            tool,
            if tool.result.is_some() || tool.result_text.is_some() {
                "Tool result"
            } else {
                "Tool"
            },
            facts,
        ),
        TranscriptViewItemKind::ToolRequest { tool } => {
            invocation_anatomy(tool, "Tool request", facts)
        }
        TranscriptViewItemKind::ToolRequestDraft { draft } => {
            draft_anatomy(draft, item.streaming, facts)
        }
        TranscriptViewItemKind::ToolContribution {
            contribution,
            placement,
            invocation,
        } => contribution_anatomy(contribution, *placement, invocation.as_deref(), facts),
        TranscriptViewItemKind::Permission { permission } => permission_anatomy(permission, facts),
        TranscriptViewItemKind::Interaction { interaction } => {
            interaction_anatomy(interaction, facts)
        }
        _ => ToolAnatomy {
            header: HeaderData {
                label: Some(common::item_label(item).to_owned()),
                ..HeaderData::default()
            },
            ..ToolAnatomy::default()
        },
    }
}

// ---- composition -------------------------------------------------------------------------

/// Container style of a tool status, reshaped by chrome `borders.tool`:
///
/// * unset: the theme recipe unchanged;
/// * `none`: no container;
/// * `left`: a leading bar, keeping the recipe's padding;
/// * framed styles: a full-width panel bordered on all sides with the chrome glyphs.
#[must_use]
pub fn tool_container(
    theme: &ResolvedThemeDefinition,
    status: Option<UnitStatus>,
    is_error: bool,
) -> TranscriptContainerStyle {
    let status = if is_error && matches!(status, None | Some(UnitStatus::Succeeded)) {
        Some(UnitStatus::Failed)
    } else {
        status
    };
    let role = common::tool_container_role(status);
    let mut style = common::container_style(theme, role, &format!("{role}.container"));
    match theme.chrome.borders.tool {
        None => {}
        Some(BorderStyle::None) => {
            style.recipe.layout = TranscriptContainerLayout::Plain;
            style.recipe.border = TranscriptContainerBorder::None;
        }
        Some(BorderStyle::Left) => {
            style.recipe.layout = TranscriptContainerLayout::LeftBar;
            style.recipe.border = TranscriptContainerBorder::Left;
        }
        Some(
            BorderStyle::Ascii
            | BorderStyle::Single
            | BorderStyle::Rounded
            | BorderStyle::Thick
            | BorderStyle::Double,
        ) => {
            style.recipe.layout = TranscriptContainerLayout::Panel;
            style.recipe.width = TranscriptContainerWidth::Full;
            style.recipe.border = TranscriptContainerBorder::All;
        }
        Some(BorderStyle::Rules) => {
            style.recipe.layout = TranscriptContainerLayout::Panel;
            style.recipe.width = TranscriptContainerWidth::Full;
            style.recipe.border = TranscriptContainerBorder::Rules;
        }
    }
    style
}

/// First non-blank line of the first visible output/result/visual section, else of any
/// visible section.
fn first_line(anatomy: &ToolAnatomy, presentation: &ResolvedPresentation) -> Option<String> {
    let visible = |section: &&Section| presentation.section(&section.name).visible;
    anatomy
        .sections
        .iter()
        .filter(|section| SUMMARY_SECTIONS.contains(&section.name.as_str()))
        .filter(visible)
        .find_map(|section| section.body.first_line())
        .or_else(|| {
            anatomy
                .sections
                .iter()
                .filter(visible)
                .find_map(|section| section.body.first_line())
        })
}

/// One-line summary from the presentation's ordered sources (`plugin`, `first_line`,
/// `template`); whitespace runs collapse to single spaces.
#[must_use]
pub fn summary_text(anatomy: &ToolAnatomy, presentation: &ResolvedPresentation) -> Option<String> {
    let one_line = |text: String| {
        let text = sanitize(&text)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        (!text.is_empty()).then_some(text)
    };
    let header = &anatomy.header;
    presentation
        .summary
        .source
        .iter()
        .find_map(|source| match source {
            SummarySource::Plugin => anatomy.summary.clone().and_then(one_line),
            SummarySource::FirstLine => first_line(anatomy, presentation),
            SummarySource::Template => presentation
                .summary
                .template
                .as_deref()
                .map(|template| {
                    render_summary_template(template, &|name| match name {
                        "tool" => header.tool.clone(),
                        "target" => header.target.clone(),
                        "status" => header.status_text.clone(),
                        "timing" => header.timing.clone(),
                        "title" => header.title.clone(),
                        "first_line" => first_line(anatomy, presentation),
                        _ => None,
                    })
                })
                .and_then(one_line),
        })
}

/// Builds the stacked rows of one tool unit at one content width.
struct Composer<'a> {
    stack: ItemStack,
    width: u64,
    theme: &'a ResolvedThemeDefinition,
    item: &'a TranscriptViewItemId,
    /// Columns the unit's shell screen is scrolled by (C39).
    shell_columns: u64,
    /// Section presentation: label rows, or a connector before the first body row.
    connector: Option<Connector>,
}

/// The connector of a unit whose `section_labels` is `connector`.
struct Connector {
    /// Painted before the first body row; taken by the first body pushed.
    pending: Option<Line>,
    /// Columns every body row hangs at.
    indent: u64,
}

impl Connector {
    /// The connector glyph after the label indent with two cells before the body.
    fn new(theme: &ResolvedThemeDefinition) -> Self {
        let glyph = theme
            .chrome
            .markers
            .connector
            .as_ref()
            .map_or(DEFAULT_CONNECTOR, |glyph| glyph.as_str());
        let text = format!("{}{glyph}  ", " ".repeat(LABEL_INDENT_CELLS));
        let line = Line::from_spans(vec![common::span(text, role_style(theme, "tool.metadata"))]);
        let indent = u64::try_from(line.width()).unwrap_or(BODY_INDENT);
        Self {
            pending: Some(line),
            indent,
        }
    }
}

/// Values of a tool header that both field and template headers present.
struct HeaderValues {
    /// The display name, else the tool name (`tool`).
    tool_name: Option<String>,
    /// The theme's bullet glyph (`bullet`).
    bullet: String,
    /// Duration of a finished call (`elapsed`).
    elapsed_text: Option<String>,
}

/// Connector glyph when the theme sets none.
pub const DEFAULT_CONNECTOR: &str = "└";

/// Header bullet glyph when the theme sets none.
pub const DEFAULT_BULLET: &str = "●";

impl Composer<'_> {
    fn style(&self, role: &str) -> Style {
        role_style(self.theme, role)
    }

    fn push(&mut self, content: Box<dyn ItemContent>, left: u64) {
        self.stack.push(common::inset(
            content,
            LogicalInsets::new(0, 0, 0, left),
            self.width,
        ));
    }

    fn clipped_line(&mut self, line: Line, left: u64) {
        self.push(Box::new(common::header_row(line)), left);
    }

    /// Columns body rows start at: `rows_indent` under label rows, or the connector's.
    fn body_indent(&self, rows_indent: u64) -> u64 {
        self.connector
            .as_ref()
            .map_or(rows_indent, |connector| connector.indent)
    }

    /// Push body content at its indent; under a connector, the first body pushed carries it.
    fn push_body(&mut self, content: Box<dyn ItemContent>, rows_indent: u64) {
        let left = self.body_indent(rows_indent);
        match self
            .connector
            .as_mut()
            .and_then(|connector| connector.pending.take())
        {
            Some(prefix) => self.stack.push(Box::new(common::Prefixed::new(
                content, left, self.width, prefix,
            ))),
            None => self.push(content, left),
        }
    }

    fn hidden_marker(&mut self) {
        let text = common::hidden_marker_text(1, self.theme.chrome.icons);
        let line = Line::from_spans(vec![common::span(text, self.style("tool.metadata"))]);
        self.clipped_line(line, 0);
    }

    fn header(
        &mut self,
        header: &HeaderData,
        presentation: &ResolvedPresentation,
        disclosure: Disclosure,
        target: DisclosureTarget,
        collapsible: bool,
    ) {
        if let Some(template) = presentation
            .header
            .as_ref()
            .and_then(|header| header.template.as_deref())
        {
            self.template_header(
                template,
                header,
                presentation,
                disclosure,
                target,
                collapsible,
            );
            return;
        }
        let (marker, segments, elapsed) = self.field_segments(header, presentation, disclosure);
        let activity = ActivitySlots {
            spinner: header.active(),
            elapsed_from: header.started_at_ms.filter(|_| elapsed && header.active()),
        };
        let line = marker_header_line(
            marker,
            segments,
            self.width
                .saturating_sub(activity.reserved_cells(self.width)),
        );
        self.stack.push(Box::new(
            DisclosureRow::new(common::header_row(line), Some(target))
                .interactive(collapsible)
                .with_activity(activity),
        ));
    }

    /// The leading marker, header segments, and whether the elapsed slot is shown, for a
    /// field-list header.
    fn field_segments(
        &self,
        header: &HeaderData,
        presentation: &ResolvedPresentation,
        disclosure: Disclosure,
    ) -> (Option<(&'static str, Style)>, Vec<HeaderSegment>, bool) {
        let icons = self.theme.chrome.icons;
        let meta = self.style("tool.metadata");
        let title = self.style(&status_title_role(header.status, header.is_error));
        let label_style = self.style("tool.label");
        let title_style = title;
        let overridden = header.title_override.is_some();
        let timing_visible = presentation.part_visible("timing");
        let HeaderValues {
            tool_name,
            bullet,
            elapsed_text,
        } = self.header_values(header, presentation);
        let fields: Vec<&str> = presentation.header.as_ref().map_or_else(
            || DEFAULT_UNIT_HEADER.to_vec(),
            |header| header.fields.iter().map(String::as_str).collect(),
        );
        let mut marker = None;
        let mut segments = Vec::new();
        let mut elapsed = false;
        for field in fields {
            match field {
                "marker" if marker.is_none() && segments.is_empty() => {
                    marker = Some((common::disclosure_marker(disclosure, icons), meta));
                }
                "marker" => segments.push(HeaderSegment::required(
                    common::disclosure_marker(disclosure, icons),
                    meta,
                )),
                "icon" => segments.push(HeaderSegment::optional(
                    common::status_glyph(header.status, icons),
                    title,
                    5,
                )),
                "bullet" => segments.push(HeaderSegment::required(bullet.clone(), title)),
                "label" => {
                    if let Some(title) = &header.title_override {
                        segments.push(
                            HeaderSegment::required(sanitize(title), title_style).truncatable(),
                        );
                    } else if let Some(label) = &header.label {
                        segments.push(if header.tool.is_none() {
                            HeaderSegment::required(label.clone(), label_style).truncatable()
                        } else {
                            HeaderSegment::optional(label.clone(), label_style, 2)
                        });
                    }
                }
                "tool" | "status" | "title" if overridden => {}
                "tool" => {
                    if let Some(tool) = &tool_name {
                        segments.push(HeaderSegment::required(tool.clone(), title).truncatable());
                    }
                }
                "target" => {
                    if let Some(target) = &header.target {
                        segments
                            .push(HeaderSegment::optional(sanitize(target), meta, 3).truncatable());
                    }
                }
                "title" => {
                    if let Some(text) = &header.title {
                        segments
                            .push(HeaderSegment::optional(sanitize(text), title, 3).truncatable());
                    }
                }
                "status" => {
                    if let Some(status) = &header.status_text {
                        segments.push(HeaderSegment::optional(status.clone(), meta, 4));
                    }
                }
                "timing" if timing_visible => {
                    elapsed = true;
                    if let Some(timing) = &header.timing {
                        segments.push(HeaderSegment::optional(timing.clone(), meta, 1));
                    }
                }
                "elapsed" if timing_visible => {
                    elapsed = true;
                    if let Some(text) = &elapsed_text {
                        segments.push(HeaderSegment::optional(text.clone(), meta, 1));
                    }
                }
                _ => {}
            }
        }
        (marker, segments, elapsed)
    }

    /// Header values shared by field and template headers.
    fn header_values(
        &self,
        header: &HeaderData,
        presentation: &ResolvedPresentation,
    ) -> HeaderValues {
        HeaderValues {
            tool_name: presentation
                .display_name
                .clone()
                .or_else(|| header.tool.clone()),
            bullet: self
                .theme
                .chrome
                .markers
                .bullet
                .as_ref()
                .map_or(DEFAULT_BULLET, |glyph| glyph.as_str())
                .to_owned(),
            elapsed_text: header.duration_ms.map(common::elapsed_label),
        }
    }

    /// A header row from the presentation's header `template`, each field in its role.
    fn template_header(
        &mut self,
        template: &str,
        header: &HeaderData,
        presentation: &ResolvedPresentation,
        disclosure: Disclosure,
        target: DisclosureTarget,
        collapsible: bool,
    ) {
        let icons = self.theme.chrome.icons;
        let meta = self.style("tool.metadata");
        let title = self.style(&status_title_role(header.status, header.is_error));
        let label_style = self.style("tool.label");
        let timing_visible = presentation.part_visible("timing");
        let HeaderValues {
            tool_name,
            bullet,
            elapsed_text,
        } = self.header_values(header, presentation);
        let field = |name: &str| -> Option<(String, Style)> {
            match name {
                "marker" => Some((
                    common::disclosure_marker(disclosure, icons).to_owned(),
                    meta,
                )),
                "bullet" => Some((bullet.clone(), title)),
                "icon" => Some((common::status_glyph(header.status, icons).to_owned(), title)),
                "label" => header
                    .title_override
                    .clone()
                    .or_else(|| header.label.clone())
                    .map(|text| (sanitize(&text), label_style)),
                "tool" => tool_name.clone().map(|text| (sanitize(&text), label_style)),
                "target" => header
                    .target
                    .as_ref()
                    .map(|text| (sanitize(text), self.style("tool.argument"))),
                "title" => header.title.as_ref().map(|text| (sanitize(text), meta)),
                "status" => header.status_text.clone().map(|text| (text, meta)),
                "timing" if timing_visible => header.timing.clone().map(|text| (text, meta)),
                "elapsed" if timing_visible => elapsed_text.clone().map(|text| (text, meta)),
                _ => None,
            }
        };
        let line = common::template_line(template, meta, field);
        let activity = ActivitySlots {
            spinner: header.active(),
            elapsed_from: None,
        };
        self.stack.push(Box::new(
            DisclosureRow::new(common::header_row(line), Some(target))
                .interactive(collapsible)
                .with_activity(activity),
        ));
    }

    fn summary(&mut self, summary: Option<String>) {
        if let Some(summary) = summary {
            let style = if self.connector.is_some() {
                self.style("tool.output")
            } else {
                self.style("tool.metadata")
            };
            let line = Line::from_spans(vec![common::span(summary, style)]);
            self.push_body(Box::new(common::header_row(line)), LABEL_INDENT);
        }
    }

    fn sections(&mut self, sections: Vec<Section>, presentation: &ResolvedPresentation) {
        let mut sections = sections
            .into_iter()
            .filter(|section| !section.body.is_empty())
            .peekable();
        while let Some(first) = sections.next() {
            let mut run = vec![first];
            while sections.peek().is_some_and(|next| next.name == run[0].name) {
                run.extend(sections.next());
            }
            let resolved = presentation.section(&run[0].name);
            if !resolved.visible {
                continue;
            }
            match resolved.disclosure {
                Disclosure::Expanded => {
                    for (index, part) in run.into_iter().enumerate() {
                        self.expanded_section(part, index == 0, resolved.preview);
                    }
                }
                Disclosure::Collapsed | Disclosure::Summary => {
                    self.collapsed_section(&run, resolved.disclosure);
                }
                Disclosure::Hidden => {}
            }
        }
    }

    fn section_target(&self, name: &str) -> DisclosureTarget {
        DisclosureTarget::Section(self.item.clone(), name.to_owned())
    }

    fn label_row(&mut self, spans: Vec<Span>, target: Option<DisclosureTarget>) {
        let row = DisclosureRow::new(common::header_row(Line::from_spans(spans)), target);
        self.push(Box::new(row), LABEL_INDENT);
    }

    fn collapsed_section(&mut self, run: &[Section], disclosure: Disclosure) {
        let meta = self.style("tool.metadata");
        let name = &run[0].name;
        let label = match run {
            [single] => single.label_text(),
            _ => name.as_str(),
        };
        let count: usize = run.iter().map(|section| section.body.row_count()).sum();
        let unit = if count == 1 { "row" } else { "rows" };
        let marker = common::disclosure_marker(Disclosure::Collapsed, self.theme.chrome.icons);
        let spans = vec![
            common::span(format!("{marker} "), meta),
            common::span(label, self.style("tool.label")),
            common::span(format!(" · {count} {unit}"), meta),
        ];
        if self.connector.is_some() {
            let row = DisclosureRow::new(
                common::header_row(Line::from_spans(spans)),
                Some(self.section_target(name)),
            );
            self.push_body(Box::new(row), LABEL_INDENT);
        } else {
            self.label_row(spans, Some(self.section_target(name)));
        }
        if disclosure == Disclosure::Summary
            && let Some(line) = run.iter().find_map(|section| section.body.first_line())
        {
            let line = Line::from_spans(vec![common::span(line, self.style("tool.output"))]);
            self.push_body(Box::new(common::header_row(line)), BODY_INDENT);
        }
    }

    fn expanded_section(&mut self, section: Section, first: bool, preview: Preview) {
        if let Some(label) = section.label.as_ref().filter(|_| self.connector.is_none()) {
            let marker = common::disclosure_marker(Disclosure::Expanded, self.theme.chrome.icons);
            let mut spans = Vec::new();
            if first {
                spans.push(common::span(
                    format!("{marker} "),
                    self.style("tool.metadata"),
                ));
            }
            spans.push(common::span(label.clone(), self.style("tool.label")));
            let target = first.then(|| self.section_target(&section.name));
            self.label_row(spans, target);
        }
        let label = section.label_text().to_owned();
        match section.body {
            SectionBody::Output { text, shell } => {
                let preview = if shell.is_some() {
                    Preview {
                        lines: preview.lines.min(SHELL_MAX_ROWS),
                        keep: preview.keep,
                    }
                } else {
                    preview
                };
                self.output_body(&label, &text, shell.as_deref(), preview);
            }
            SectionBody::Rendered { text, role } => {
                self.rendered_body(&label, &text, role, preview);
            }
            SectionBody::Line { text, role } => {
                let block = TextBlock::new(Text::from_lines(vec![Line::from_spans(vec![
                    common::span(sanitize(&text), self.style(role.role())),
                ])]))
                .wrap(TextWrap::Word);
                self.push_body(Box::new(VisualRows::new(block)), LABEL_INDENT);
            }
            SectionBody::Diff { text } => self.diff_body(&label, &text, preview),
            SectionBody::Rows {
                rows,
                key,
                correspondence,
                placements,
            } => self.rows_body(&label, &rows, &key, (&correspondence, &placements), preview),
        }
    }

    fn hidden_rows(&mut self, label: &str, hidden: usize) {
        if hidden > 0 {
            let style = self.style("tool.truncation");
            let block = TextBlock::new(Text::from_lines(vec![Line::from_spans(vec![
                common::span(format!("… {hidden} {label} rows hidden …"), style),
            ])]))
            .wrap(TextWrap::Word);
            self.push_body(Box::new(block), BODY_INDENT);
        }
    }

    fn output_body(&mut self, label: &str, text: &str, shell: Option<&str>, preview: Preview) {
        let style = self.style("tool.output");
        let lines = source_lines(text);
        let (ranges, hidden) = preview_ranges(lines.len(), preview);
        // A shell screen scrolls horizontally on its own (C39), by at most the columns its
        // widest shown line overflows the body.
        let indent = self.body_indent(BODY_INDENT);
        let scroll = shell.map(|_| {
            let body = if self.width > indent {
                self.width - indent
            } else {
                self.width
            };
            let widest = ranges
                .iter()
                .flat_map(|range| &lines[range.clone()])
                .map(|(_, line)| u64::try_from(Line::raw(sanitize(line)).width()).unwrap_or(0))
                .max()
                .unwrap_or(0);
            let max_columns = widest.saturating_sub(body);
            (self.shell_columns.min(max_columns), max_columns)
        });
        for range in ranges {
            let content: Box<dyn ItemContent> = match scroll {
                Some((columns, _)) if columns > 0 => {
                    let mut stack = ItemStack::new();
                    for (offset, line) in &lines[range] {
                        stack.push(scrolled_shell_line(*offset, line, columns, style));
                    }
                    Box::new(stack)
                }
                _ => {
                    let base = lines[range.start].0;
                    let source = source_slice(text, &lines[range.clone()]);
                    let rows = lines[range]
                        .iter()
                        .map(|(_, line)| {
                            Line::from_spans(vec![common::span(sanitize(line), style)])
                        })
                        .collect::<Vec<_>>();
                    let block = TextBlock::new(Text::from_lines(rows)).wrap(TextWrap::None);
                    Box::new(
                        SourceText::new(
                            block,
                            TextSource::Shell {
                                base: u64::try_from(base).unwrap_or(u64::MAX),
                            },
                        )
                        .with_copy_source(source),
                    )
                }
            };
            let content = match (shell, scroll) {
                (Some(key), Some((columns, max_columns))) => Box::new(
                    ContentKeyed::new(content, key).with_target(ContentTargetKind::ShellScreen {
                        columns,
                        max_columns,
                    }),
                ),
                _ => content,
            };
            self.push_body(content, BODY_INDENT);
        }
        self.hidden_rows(label, hidden);
    }

    fn rendered_body(&mut self, label: &str, text: &str, role: BodyRole, preview: Preview) {
        let style = self.style(role.role());
        let lines = source_lines(text);
        let (ranges, hidden) = preview_ranges(lines.len(), preview);
        for range in ranges {
            let rows = lines[range]
                .iter()
                .map(|(_, line)| Line::from_spans(vec![common::span(sanitize(line), style)]))
                .collect::<Vec<_>>();
            let block = TextBlock::new(Text::from_lines(rows)).wrap(TextWrap::None);
            self.push_body(Box::new(VisualRows::new(block)), BODY_INDENT);
        }
        self.hidden_rows(label, hidden);
    }

    fn diff_style(&self, line: &str) -> Style {
        let role = if line.starts_with("+++")
            || line.starts_with("---")
            || line.starts_with("diff ")
            || line.starts_with("index ")
        {
            "diff.label"
        } else if line.starts_with("@@") {
            "diff.hunk"
        } else if line.starts_with('+') {
            "diff.added"
        } else if line.starts_with('-') {
            "diff.removed"
        } else {
            "diff.text"
        };
        self.style(role)
    }

    fn diff_body(&mut self, label: &str, text: &str, preview: Preview) {
        let lines = source_lines(text);
        let (ranges, hidden) = preview_ranges(lines.len(), preview);
        for range in ranges {
            let base = range.start;
            let source = source_slice(text, &lines[range.clone()]);
            let rows = lines[range]
                .iter()
                .map(|(_, line)| {
                    Line::from_spans(vec![common::span(sanitize(line), self.diff_style(line))])
                })
                .collect::<Vec<_>>();
            self.push_body(
                Box::new(
                    SourceText::from_lines(
                        rows,
                        TextWrap::None,
                        TextSource::Diff {
                            base: u64::try_from(base).unwrap_or(u64::MAX),
                        },
                    )
                    .with_copy_source(source),
                ),
                BODY_INDENT,
            );
        }
        self.hidden_rows(label, hidden);
    }

    fn rows_body(
        &mut self,
        label: &str,
        rows: &[Line],
        key: &str,
        (correspondence, placements): (&Arc<VisualCorrespondence>, &Arc<VisualPlacements>),
        preview: Preview,
    ) {
        let (ranges, hidden) = preview_ranges(rows.len(), preview);
        for range in ranges {
            let base = range.start;
            let block = TextBlock::new(Text::from_lines(rows[range].to_vec())).wrap(TextWrap::None);
            self.push(
                Box::new(
                    VisualRows::new(block)
                        .keyed(key)
                        .plugin(Arc::clone(correspondence), base)
                        .placements(Arc::clone(placements)),
                ),
                0,
            );
        }
        self.hidden_rows(label, hidden);
    }
}

/// One shell output line (starting at output byte `offset`) scrolled by `columns`: the BMUX
/// cell viewport of the painted line. When the painted viewport is the tail of the line (no
/// zero-width graphemes were dropped), it stays source-mapped by byte and copies the original
/// bytes; otherwise it is a rendered row.
fn scrolled_shell_line(
    offset: usize,
    line: &str,
    columns: u64,
    style: Style,
) -> Box<dyn ItemContent> {
    let painted = sanitize(line);
    let viewport = Line::from_spans(vec![common::span(painted.clone(), style)])
        .viewport(usize::try_from(columns).unwrap_or(usize::MAX), usize::MAX);
    let shown = viewport.plain_text();
    let skipped = painted.len() - shown.len().min(painted.len());
    match line.get(skipped..).filter(|_| painted.ends_with(&shown)) {
        Some(source) => Box::new(
            SourceText::from_lines(
                vec![viewport],
                TextWrap::None,
                TextSource::Shell {
                    base: u64::try_from(offset + skipped).unwrap_or(u64::MAX),
                },
            )
            .with_copy_source(source),
        ),
        None => Box::new(VisualRows::new(
            TextBlock::new(Text::from_lines(vec![viewport])).wrap(TextWrap::None),
        )),
    }
}

/// Horizontal frame of one tool unit at a layout width: one definition for [`compose`] and
/// [`body_width`].
struct UnitFrame {
    /// Status container framing the unit.
    container: TranscriptContainerStyle,
    /// Width left inside the policy indent.
    outer: u64,
    /// Whether the container is plain (unframed).
    plain: bool,
    /// Width of the unit's header and section rows: `outer` less the frame insets when the
    /// container frames the unit.
    body: u64,
}

impl UnitFrame {
    fn of(
        theme: &ResolvedThemeDefinition,
        presentation: &ResolvedPresentation,
        status: Option<UnitStatus>,
        is_error: bool,
        width: u64,
    ) -> Self {
        let container = tool_container(theme, status, is_error);
        let outer = placed_width(presentation.indent, width);
        let insets = common::frame_insets(&container);
        let plain = matches!(container.recipe.layout, TranscriptContainerLayout::Plain);
        let framed = !plain && insets.horizontal() < outer;
        Self {
            container,
            outer,
            plain,
            body: if framed {
                outer - insets.horizontal()
            } else {
                outer
            },
        }
    }
}

/// Width in cells of the section rows of `item`'s tool unit laid out at `width`.
///
/// It is what the policy indent and, when the status container frames the unit, its frame
/// insets leave under `structure` and `theme`. [`compose`] lays rows out in exactly this
/// width, so a plugin visual adapter rendering at it fills the body without being clipped.
#[must_use]
pub fn body_width(
    item: &TranscriptViewItem,
    structure: &ResolvedUnitPresentation,
    theme: &ResolvedThemeDefinition,
    width: u64,
) -> u64 {
    let (presentation, facts) = match structure {
        ResolvedUnitPresentation::Item(structure) => (
            Cow::Borrowed(&structure.presentation),
            Some(&structure.facts),
        ),
        _ => (Cow::Owned(host_defaults()), None),
    };
    let header = tool_anatomy(item, facts).header;
    UnitFrame::of(theme, &presentation, header.status, header.is_error, width).body
}

/// Lay out `anatomy` for the unit of `item` under `cx` (structure v2 unit anatomy).
///
/// The header comes first, then the sections, the summary or nothing by disclosure, framed by
/// the status container and placed with policy spacing and indent.
#[must_use]
pub fn compose(
    anatomy: ToolAnatomy,
    item: &TranscriptViewItemId,
    cx: &ItemRenderContext<'_>,
) -> ProducedItem {
    let defaults;
    let presentation = if let ResolvedUnitPresentation::Item(structure) = cx.structure {
        &structure.presentation
    } else {
        defaults = host_defaults();
        &defaults
    };
    let theme = cx.theme;
    let UnitFrame {
        container,
        outer,
        plain,
        body,
    } = UnitFrame::of(
        theme,
        presentation,
        anatomy.header.status,
        anatomy.header.is_error,
        cx.width,
    );
    let mut composer = Composer {
        stack: ItemStack::new().nested(),
        width: body,
        theme,
        item,
        shell_columns: cx.shell_columns,
        connector: (presentation.section_labels == SectionLabels::Connector)
            .then(|| Connector::new(theme)),
    };
    let disclosure = cx.disclosure.disclosure;
    let target = cx
        .disclosure
        .target
        .clone()
        .unwrap_or_else(|| DisclosureTarget::Unit(item.clone()));
    let collapsible = cx.disclosure.collapsible;
    match disclosure {
        Disclosure::Hidden => composer.hidden_marker(),
        // A full-block visual draws its own header: its first row is the unit's target.
        Disclosure::Expanded if anatomy.header.self_drawn => {
            composer.sections(anatomy.sections, presentation);
            let stack = std::mem::replace(&mut composer.stack, ItemStack::new().nested());
            composer
                .stack
                .push(Box::new(UnitTargetRow::new(Box::new(stack), target)));
        }
        Disclosure::Expanded => {
            composer.header(
                &anatomy.header,
                presentation,
                disclosure,
                target,
                collapsible,
            );
            composer.sections(anatomy.sections, presentation);
        }
        Disclosure::Summary => {
            composer.header(
                &anatomy.header,
                presentation,
                disclosure,
                target,
                collapsible,
            );
            composer.summary(summary_text(&anatomy, presentation));
        }
        Disclosure::Collapsed => composer.header(
            &anatomy.header,
            presentation,
            disclosure,
            target,
            collapsible,
        ),
    }
    let mut content: Box<dyn ItemContent> = Box::new(composer.stack);
    content = common::framed(content, container, outer);
    if plain && let Some(raised) = raised_surface(theme) {
        content = common::framed(content, raised, outer);
    }
    ProducedItem::boxed(place(
        content,
        presentation.spacing,
        presentation.indent,
        cx.width,
    ))
}

/// Tool-like units through the anatomy; units without an item use the renderer-neutral rows.
fn produce_tool(unit: UnitView<'_>, cx: &ItemRenderContext<'_>) -> ProducedItem {
    unit.item.map_or_else(
        || ProducedItem::new(common::plain_unit(unit)),
        |item| compose(tool_anatomy(item, unit_facts(cx)), &item.id, cx),
    )
}

/// Tool invocation, request, draft and contribution units without an accepted plugin visual.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolProducer;

impl UnitProducer for ToolProducer {
    fn produce(&self, unit: UnitView<'_>, cx: &ItemRenderContext<'_>) -> ProducedItem {
        produce_tool(unit, cx)
    }
}

/// Permission request units: arguments, metadata and the decision (`confirmation`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PermissionProducer;

impl UnitProducer for PermissionProducer {
    fn produce(&self, unit: UnitView<'_>, cx: &ItemRenderContext<'_>) -> ProducedItem {
        produce_tool(unit, cx)
    }
}

/// Interactive request units: metadata and the request state (`confirmation`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InteractionProducer;

impl UnitProducer for InteractionProducer {
    fn produce(&self, unit: UnitView<'_>, cx: &ItemRenderContext<'_>) -> ProducedItem {
        produce_tool(unit, cx)
    }
}

#[cfg(test)]
mod tests;
