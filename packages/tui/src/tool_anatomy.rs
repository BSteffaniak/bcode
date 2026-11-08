//! Tool unit anatomy and its terminal composer.
//!
//! Tool renderers describe a unit as a [`ToolAnatomy`] (header fields, an
//! optional producer summary, and named sections) instead of finished rows.
//! [`compose_tool`] lays the anatomy out from the resolved presentation, so
//! disclosure, section visibility, previews, and summaries are decided before
//! rows exist rather than trimmed afterwards. Disclosure anchors are recorded
//! on the same rows that are painted, so hit regions derive from the layout.

use std::collections::BTreeMap;

use bcode_plugin_sdk::tui::PluginTuiSelectionRow;
use bcode_plugin_sdk::tui_visual::TuiVisualAnchor;
use bcode_presentation_policy::models::{
    Disclosure, PreviewKeep, ResolvedPresentation, ResolvedSection, SummarySource,
};
use bcode_session_view_models::ToolInvocationViewStatus;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::text::truncate_line_to_display_width;

use super::structure::DisclosureTarget;
use super::theme::chrome::UnitChrome;
use super::time_format::{format_millis, unix_time_millis};
use super::transcript::ToolTiming;

/// Header fields rendered when the presentation does not choose any.
pub const DEFAULT_HEADER_FIELDS: [&str; 5] = ["marker", "label", "tool", "status", "timing"];

/// Section names whose first line is preferred for `first_line` summaries.
const SUMMARY_SECTIONS: [&str; 3] = ["output", "result", "visual"];

/// Header facts of a tool unit; the composer formats the selected fields.
#[derive(Debug, Clone, Default)]
pub struct HeaderData {
    /// The body draws its own header (plugin full-block surfaces), so the
    /// composer paints a header only when the unit is not expanded.
    pub self_drawn: bool,
    /// Surface label, for example `Tool result`.
    pub label: Option<String>,
    /// Tool name.
    pub tool: Option<String>,
    /// Target of the operation (path, command, pattern), from the producer's
    /// presentation or the unit's `target` fact.
    pub target: Option<String>,
    /// Displayed status text, for example `ok` or `running`.
    pub status: Option<String>,
    /// Lifecycle state used for header styling and timing phrasing.
    pub state: Option<ToolInvocationViewStatus>,
    /// Timing facts rendered by the `timing` field.
    pub timing: Option<ToolTiming>,
    /// Whether the unit reported an error.
    pub is_error: bool,
    /// Producer-supplied title, rendered by the `title` field.
    pub title: Option<String>,
    /// Producer title that replaces the `label`, `tool`, and `status`
    /// fields (plugin transcript-block surfaces).
    pub title_override: Option<String>,
}

/// Semantic role of a section body, which selects its style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyRole {
    /// Tool output or result text.
    Output,
    /// Tool argument text.
    Argument,
    /// A failure notice.
    Failure,
}

/// Body of one anatomy section.
#[derive(Debug, Clone)]
pub enum SectionBody {
    /// Source text shown line by line, bounded by the section preview.
    Text {
        /// Source text.
        text: String,
        /// Style role.
        role: BodyRole,
    },
    /// Rows pre-rendered by a plugin visual, with item-relative anchors and
    /// selection rows indexed from the first body row.
    Rows {
        /// Rendered rows.
        rows: Vec<Line>,
        /// Content anchors, rows relative to `rows`.
        anchors: Vec<TuiVisualAnchor>,
        /// Selection provenance, rows relative to `rows`.
        selection: Vec<(usize, PluginTuiSelectionRow)>,
    },
    /// One line wrapped to the available width (confirmations, errors).
    Line {
        /// Text, including any inline label.
        text: String,
        /// Style role.
        role: BodyRole,
    },
    /// Rows pre-rendered by a plugin for a named section (plugin contract
    /// v3), bounded by the section preview like text bodies.
    Lines {
        /// Rendered rows.
        rows: Vec<Line>,
    },
}

impl SectionBody {
    const fn is_empty(&self) -> bool {
        match self {
            Self::Text { text, .. } | Self::Line { text, .. } => text.is_empty(),
            Self::Rows { rows, .. } | Self::Lines { rows } => rows.is_empty(),
        }
    }

    fn row_count(&self) -> usize {
        match self {
            Self::Text { text, .. } => text.lines().count(),
            Self::Rows { rows, .. } | Self::Lines { rows } => rows.len(),
            Self::Line { .. } => 1,
        }
    }

    fn first_line(&self) -> Option<String> {
        let first = |mut lines: Box<dyn Iterator<Item = String> + '_>| {
            lines.find_map(|line| {
                let line = line.trim();
                (!line.is_empty()).then(|| line.to_owned())
            })
        };
        match self {
            Self::Text { text, .. } | Self::Line { text, .. } => {
                first(Box::new(text.lines().map(str::to_owned)))
            }
            Self::Rows { rows, .. } | Self::Lines { rows } => {
                first(Box::new(rows.iter().map(Line::plain_text)))
            }
        }
    }
}

/// One named, ordered section of a tool unit.
///
/// Consecutive sections may share a name (for example one `arguments`
/// section per argument field); they then share one disclosure target.
#[derive(Debug, Clone)]
pub struct Section {
    /// Host or namespaced section name, the key for presentation and targets.
    pub name: String,
    /// Label row text; `None` paints the body without a label row.
    pub label: Option<String>,
    /// Section body.
    pub body: SectionBody,
}

impl Section {
    /// Text section shown under a label row.
    #[must_use]
    pub fn text(name: &str, label: &str, text: &str, role: BodyRole) -> Self {
        Self {
            name: name.to_owned(),
            label: Some(label.to_owned()),
            body: SectionBody::Text {
                text: text.to_owned(),
                role,
            },
        }
    }

    /// Single wrapped line without a label row.
    #[must_use]
    pub fn line(name: &str, text: String, role: BodyRole) -> Self {
        Self {
            name: name.to_owned(),
            label: None,
            body: SectionBody::Line { text, role },
        }
    }
}

/// A tool unit described for composition.
#[derive(Debug, Clone, Default)]
pub struct ToolAnatomy {
    /// Header facts.
    pub header: HeaderData,
    /// Producer-supplied one-line summary.
    pub summary: Option<String>,
    /// Ordered sections.
    pub sections: Vec<Section>,
}

/// Inputs the composer needs besides the anatomy.
pub struct ComposeContext<'a> {
    /// Effective unit presentation.
    pub presentation: &'a ResolvedPresentation,
    /// Whether the unit takes part in structure (markers, disclosure, anchors).
    pub structural: bool,
    /// Transcript item id, for disclosure targets.
    pub item_id: u64,
    /// Available content width in terminal cells.
    pub width: u16,
    /// Effective presentation of a named section of this unit.
    pub section: &'a dyn Fn(&str) -> ResolvedSection,
}

/// Item-relative output buffers the composer appends to.
pub struct ComposeOutput<'a> {
    /// Painted rows.
    pub rows: &'a mut Vec<Line>,
    /// Content and disclosure anchors.
    pub anchors: &'a mut Vec<TuiVisualAnchor>,
    /// Selection provenance rows.
    pub selection: &'a mut Vec<(usize, PluginTuiSelectionRow)>,
}

/// Columns of indentation actually applied at `width`.
///
/// The presentation indent never takes more than half the width, so narrow
/// terminals keep room for content.
#[must_use]
pub fn effective_indent(presentation: &ResolvedPresentation, width: u16) -> u16 {
    u16::from(presentation.indent).min(width / 2)
}

/// Width available to section bodies after indentation; plugin visuals are
/// rendered at this width.
#[must_use]
pub fn inner_width(presentation: &ResolvedPresentation, width: u16) -> u16 {
    width.saturating_sub(effective_indent(presentation, width))
}

/// Lay out `anatomy` from the resolved presentation into `out`.
///
/// * `expanded`: header, then each visible section with its own disclosure.
/// * `summary`: header, then one summary line.
/// * `collapsed`: header only.
/// * `hidden`: a one-row count marker.
///
/// Structural units record a `disclosure:unit:<id>` anchor on the header's
/// first row and a `disclosure:section:<id>:<name>` anchor on each
/// collapsible section label row.
pub fn compose_tool(anatomy: ToolAnatomy, cx: &ComposeContext<'_>, out: ComposeOutput<'_>) {
    let presentation = cx.presentation;
    let disclosure = if cx.structural {
        presentation.disclosure
    } else {
        Disclosure::Expanded
    };
    let indent = effective_indent(presentation, cx.width);
    let mut resolved = BTreeMap::new();
    for section in &anatomy.sections {
        if !resolved.contains_key(section.name.as_str()) {
            resolved.insert(section.name.clone(), (cx.section)(&section.name));
        }
    }
    let mut composer = Composer {
        rows: Vec::new(),
        anchors: Vec::new(),
        selection: Vec::new(),
        width: cx.width.saturating_sub(indent),
        structural: cx.structural,
        item_id: cx.item_id,
        theme: super::render::semantic_state_theme(),
    };
    let spacing = presentation.spacing.clamped();
    composer.blank_rows(spacing.before);
    // The unit target sits on the first painted row: the header, the hidden
    // marker, or the first row of a full-block surface that draws its own header.
    composer.unit_anchor();
    match disclosure {
        Disclosure::Hidden => composer.push_hidden_marker(),
        Disclosure::Expanded if anatomy.header.self_drawn => {
            composer.push_sections(anatomy.sections, &resolved);
        }
        Disclosure::Expanded => {
            composer.push_header(&anatomy.header, presentation, disclosure);
            composer.push_sections(anatomy.sections, &resolved);
        }
        Disclosure::Summary => {
            composer.push_header(&anatomy.header, presentation, disclosure);
            if let Some(summary) = summary_text(&anatomy, presentation, &resolved) {
                composer.push_summary(&summary);
            }
        }
        Disclosure::Collapsed => composer.push_header(&anatomy.header, presentation, disclosure),
    }
    composer.blank_rows(spacing.after);
    let mut out = out;
    composer.flush(&mut out, indent, cx.width);
}

struct Composer {
    rows: Vec<Line>,
    anchors: Vec<TuiVisualAnchor>,
    selection: Vec<(usize, PluginTuiSelectionRow)>,
    width: u16,
    structural: bool,
    item_id: u64,
    theme: super::theme::PresentedTheme,
}

impl Composer {
    fn blank_rows(&mut self, count: u8) {
        for _ in 0..count {
            self.rows.push(Line::default());
        }
    }

    fn anchor(&mut self, target: &DisclosureTarget) {
        if self.structural {
            self.anchors.push(TuiVisualAnchor {
                key: target.to_hit_id(),
                row: self.rows.len(),
                source: None,
            });
        }
    }

    fn unit_anchor(&mut self) {
        self.anchor(&DisclosureTarget::Unit(self.item_id));
    }

    fn wrapped(&mut self, prefix: Vec<Span>, text: &str, body: Style, continuation: Style) {
        bcode_tui_components::transcript_markdown::append_styled_text(
            &mut self.rows,
            prefix,
            text,
            self.width,
            body,
            continuation,
        );
    }

    fn bounded(&mut self, spans: Vec<Span>) {
        self.rows.push(truncate_line_to_display_width(
            &Line::from_spans(spans),
            usize::from(self.width),
        ));
    }

    fn push_hidden_marker(&mut self) {
        let meta = self.theme.transcript.tool_metadata;
        let marker = glyph(self.theme.unit_chrome, Disclosure::Hidden);
        self.bounded(vec![Span::styled(
            format!("{marker} 1 tool call hidden"),
            meta,
        )]);
    }

    fn push_header(
        &mut self,
        header: &HeaderData,
        presentation: &ResolvedPresentation,
        disclosure: Disclosure,
    ) {
        let meta = self.theme.transcript.tool_metadata;
        let title_style =
            tool_title_style(&self.theme, header.state, header.timing, header.is_error);
        let (marker, text) = header_text(
            header,
            presentation,
            self.theme.unit_chrome,
            self.structural,
            disclosure,
        );
        let prefix = marker
            .map(|marker| vec![Span::styled(format!("{marker} "), meta)])
            .unwrap_or_default();
        let before = self.rows.len();
        self.wrapped(prefix, &text, title_style, meta);
        if self.rows.len() == before {
            self.rows.push(Line::default());
        }
    }

    fn push_summary(&mut self, summary: &str) {
        let meta = self.theme.transcript.tool_metadata;
        self.bounded(vec![
            Span::styled("  ", meta),
            Span::styled(summary.to_owned(), meta),
        ]);
    }

    fn push_sections(
        &mut self,
        sections: Vec<Section>,
        resolved: &BTreeMap<String, ResolvedSection>,
    ) {
        let mut sections = sections
            .into_iter()
            .filter(|section| !section.body.is_empty())
            .peekable();
        while let Some(first) = sections.next() {
            let mut run = vec![first];
            while sections.peek().is_some_and(|next| next.name == run[0].name) {
                run.extend(sections.next());
            }
            let section = resolved
                .get(run[0].name.as_str())
                .copied()
                .unwrap_or_else(|| {
                    ResolvedSection::defaults(bcode_presentation_policy::host_defaults().preview)
                });
            if !section.visible {
                continue;
            }
            match section.disclosure {
                Disclosure::Expanded => {
                    for (index, part) in run.into_iter().enumerate() {
                        if index == 0 && part.label.is_some() {
                            self.anchor(&DisclosureTarget::Section(
                                self.item_id,
                                part.name.clone(),
                            ));
                        }
                        self.push_expanded_section(part, section);
                    }
                }
                Disclosure::Collapsed | Disclosure::Summary => {
                    self.push_collapsed_section(&run, section.disclosure);
                }
                Disclosure::Hidden => {}
            }
        }
    }

    fn push_label(&mut self, label: &str) {
        let meta = self.theme.transcript.tool_metadata;
        let label_style = self.theme.transcript.tool_label;
        self.wrapped(vec![Span::styled("  ", meta)], label, label_style, meta);
    }

    fn push_collapsed_section(&mut self, run: &[Section], disclosure: Disclosure) {
        let meta = self.theme.transcript.tool_metadata;
        let name = &run[0].name;
        let label = match run {
            [single] => single.label.as_deref().unwrap_or(name),
            _ => name,
        };
        let count: usize = run.iter().map(|section| section.body.row_count()).sum();
        let unit = if count == 1 { "row" } else { "rows" };
        self.anchor(&DisclosureTarget::Section(self.item_id, name.clone()));
        self.bounded(vec![
            Span::styled("  ", meta),
            Span::styled(
                format!("{} ", glyph(self.theme.unit_chrome, Disclosure::Collapsed)),
                meta,
            ),
            Span::styled(label.to_owned(), self.theme.transcript.tool_label),
            Span::styled(format!(" · {count} {unit}"), meta),
        ]);
        if disclosure == Disclosure::Summary
            && let Some(line) = run.iter().find_map(|section| section.body.first_line())
        {
            self.bounded(vec![
                Span::styled("    ", meta),
                Span::styled(line, self.theme.transcript.tool_output),
            ]);
        }
    }

    fn push_expanded_section(&mut self, section: Section, resolved: ResolvedSection) {
        let meta = self.theme.transcript.tool_metadata;
        if let Some(label) = &section.label {
            self.push_label(label);
        }
        match section.body {
            SectionBody::Text { text, role } => {
                let label = section.label.as_deref().unwrap_or(&section.name);
                self.push_text_preview(label, &text, role, resolved);
            }
            SectionBody::Rows {
                rows,
                anchors,
                selection,
            } => {
                let start = self.rows.len();
                self.anchors.extend(anchors.into_iter().map(|mut anchor| {
                    anchor.row = anchor.row.saturating_add(start);
                    anchor
                }));
                self.selection.extend(
                    selection
                        .into_iter()
                        .map(|(row, source)| (row.saturating_add(start), source)),
                );
                self.rows.extend(rows);
            }
            SectionBody::Line { text, role } => {
                let style = self.role_style(role);
                self.wrapped(vec![Span::styled("  ", meta)], &text, style, meta);
            }
            SectionBody::Lines { rows } => {
                let label = section.label.as_deref().unwrap_or(&section.name);
                self.push_rows_preview(label, &rows, resolved);
            }
        }
    }

    /// Plugin section rows bounded by the section preview. Rows keep the
    /// adapter's own geometry (rendered at the body width) and are only
    /// clipped to it, never reflowed.
    fn push_rows_preview(&mut self, label: &str, rows: &[Line], resolved: ResolvedSection) {
        let max_rows = usize::try_from(resolved.preview.lines).unwrap_or(usize::MAX);
        let total = rows.len();
        let kept = preview_lines(rows, max_rows, resolved.preview.keep);
        let width = usize::from(self.width);
        self.rows.extend(
            kept.into_iter()
                .map(|row| truncate_line_to_display_width(row, width)),
        );
        self.push_hidden_rows_marker(label, total, max_rows);
    }

    fn push_hidden_rows_marker(&mut self, label: &str, total: usize, max_rows: usize) {
        if total > max_rows {
            let meta = self.theme.transcript.tool_metadata;
            let truncation = self.theme.transcript.tool_truncation;
            self.wrapped(
                vec![Span::styled("    ", meta)],
                &format!("… {} {label} rows hidden …", total - max_rows),
                truncation,
                truncation,
            );
        }
    }

    const fn role_style(&self, role: BodyRole) -> Style {
        match role {
            BodyRole::Output => self.theme.transcript.tool_output,
            BodyRole::Argument => self.theme.transcript.tool_argument,
            BodyRole::Failure => self.theme.transcript.tool_failed_title,
        }
    }

    fn push_text_preview(
        &mut self,
        label: &str,
        text: &str,
        role: BodyRole,
        resolved: ResolvedSection,
    ) {
        let meta = self.theme.transcript.tool_metadata;
        let style = self.role_style(role);
        let max_rows = usize::try_from(resolved.preview.lines).unwrap_or(usize::MAX);
        let lines = text.lines().collect::<Vec<_>>();
        for line in preview_lines(&lines, max_rows, resolved.preview.keep) {
            self.bounded(vec![
                Span::styled("    ", meta),
                Span::styled((*line).to_owned(), style),
            ]);
        }
        self.push_hidden_rows_marker(label, lines.len(), max_rows);
    }

    fn flush(self, out: &mut ComposeOutput<'_>, indent: u16, width: u16) {
        let base = out.rows.len();
        let pad = usize::from(indent);
        out.rows.extend(self.rows.into_iter().map(|mut row| {
            if pad == 0 || row.spans.is_empty() {
                row
            } else {
                row.spans.insert(0, Span::raw(" ".repeat(pad)));
                truncate_line_to_display_width(&row, usize::from(width))
            }
        }));
        out.anchors
            .extend(self.anchors.into_iter().map(|mut anchor| {
                anchor.row = anchor.row.saturating_add(base);
                anchor
            }));
        out.selection
            .extend(self.selection.into_iter().map(|(row, mut source)| {
                for cell in &mut source.cells {
                    cell.column = cell.column.saturating_add(indent);
                }
                (row.saturating_add(base), source)
            }));
    }
}

/// Disclosure marker from the theme's marker glyph set.
fn glyph(chrome: UnitChrome, disclosure: Disclosure) -> &'static str {
    chrome.marker(disclosure == Disclosure::Expanded)
}

enum HeaderPiece {
    /// Disclosure marker; painted as a prefix when it leads the header.
    Marker(String),
    /// A glyph (the tool icon), joined to its neighbours with a space.
    Glyph(String),
    /// A text field, joined to other text fields with ` · `.
    Text(String),
}

/// Leading marker (painted as a prefix) and the joined header text.
fn header_text(
    header: &HeaderData,
    presentation: &ResolvedPresentation,
    chrome: UnitChrome,
    structural: bool,
    disclosure: Disclosure,
) -> (Option<String>, String) {
    let overridden = header.title_override.is_some();
    let field_text = |field: &str| -> Option<String> {
        match field {
            "label" => header
                .title_override
                .clone()
                .or_else(|| header.label.clone()),
            "tool" if !overridden => header.tool.clone(),
            "status" if !overridden => header.status.clone(),
            "title" if !overridden => header.title.clone(),
            "target" => header.target.clone(),
            "timing" if presentation.part_visible("timing") => timing_text(header),
            _ => None,
        }
    };
    let default_fields = DEFAULT_HEADER_FIELDS.map(str::to_owned);
    let fields = presentation
        .header
        .as_ref()
        .map_or(&default_fields[..], |header| &header.fields[..]);
    let mut pieces = fields
        .iter()
        .filter_map(|field| {
            if field == "marker" {
                structural.then(|| HeaderPiece::Marker(glyph(chrome, disclosure).to_owned()))
            } else if field == "icon" {
                let tool = header.tool.as_deref()?;
                chrome
                    .tool_icon(tool)
                    .map(|icon| HeaderPiece::Glyph(icon.to_owned()))
            } else {
                field_text(field)
                    .filter(|text| !text.is_empty())
                    .map(HeaderPiece::Text)
            }
        })
        .peekable();
    let marker = match pieces.peek() {
        Some(HeaderPiece::Marker(marker)) => {
            let marker = marker.clone();
            pieces.next();
            Some(marker)
        }
        _ => None,
    };
    let mut text = String::new();
    let mut previous_marker = false;
    for piece in pieces {
        let (piece, is_marker) = match piece {
            HeaderPiece::Marker(glyph) | HeaderPiece::Glyph(glyph) => (glyph, true),
            HeaderPiece::Text(text) => (text, false),
        };
        if !text.is_empty() {
            text.push_str(if is_marker || previous_marker {
                " "
            } else {
                " · "
            });
        }
        text.push_str(&piece);
        previous_marker = is_marker;
    }
    (marker, text)
}

/// Timing phrase, for example `elapsed 3s · timeout 10s` or `duration 42ms`.
fn timing_text(header: &HeaderData) -> Option<String> {
    let timing = header.timing?;
    let streaming = matches!(
        header.state,
        Some(ToolInvocationViewStatus::Running | ToolInvocationViewStatus::Waiting)
    );
    let now_ms = unix_time_millis(std::time::SystemTime::now());
    let mut parts = Vec::new();
    if timing.timed_out == Some(true) {
        parts.push("timed out".to_owned());
    }
    if streaming && let Some(started_at_ms) = timing.started_at_ms {
        parts.push(format!(
            "elapsed {}",
            format_millis(now_ms.saturating_sub(started_at_ms))
        ));
    }
    if let Some(timeout_ms) = timing.timeout_ms
        && (streaming || timing.duration_ms.is_none() && timing.finished_at_ms.is_none())
    {
        parts.push(format!("timeout {}", format_millis(timeout_ms)));
    }
    if !streaming {
        if let Some(duration_ms) = timing.duration_ms {
            parts.push(format!("duration {}", format_millis(duration_ms)));
        } else if let (Some(started_at_ms), Some(finished_at_ms)) =
            (timing.started_at_ms, timing.finished_at_ms)
        {
            parts.push(format!(
                "duration {}",
                format_millis(finished_at_ms.saturating_sub(started_at_ms))
            ));
        }
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn tool_title_style(
    theme: &super::theme::PresentedTheme,
    state: Option<ToolInvocationViewStatus>,
    timing: Option<ToolTiming>,
    is_error: bool,
) -> Style {
    if timing.is_some_and(|timing| timing.timed_out == Some(true)) {
        theme.transcript.tool_timed_out_title
    } else if is_error || matches!(state, Some(ToolInvocationViewStatus::Failed)) {
        theme.transcript.tool_failed_title
    } else {
        match state {
            Some(ToolInvocationViewStatus::Running) => theme.transcript.tool_running_title,
            Some(ToolInvocationViewStatus::Waiting) => theme.transcript.tool_waiting_title,
            Some(ToolInvocationViewStatus::Finished) => theme.transcript.tool_succeeded_title,
            Some(ToolInvocationViewStatus::Cancelled) => theme.transcript.tool_cancelled_title,
            Some(ToolInvocationViewStatus::Requested | ToolInvocationViewStatus::Failed) | None => {
                theme.transcript.tool_requested_title
            }
        }
    }
}

/// First non-blank line of the first visible output/result/visual section,
/// else of any visible section.
fn first_line(
    anatomy: &ToolAnatomy,
    resolved: &BTreeMap<String, ResolvedSection>,
) -> Option<String> {
    let visible = |section: &&Section| {
        resolved
            .get(section.name.as_str())
            .is_none_or(|resolved| resolved.visible)
    };
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

/// Summary line from the presentation's ordered sources.
fn summary_text(
    anatomy: &ToolAnatomy,
    presentation: &ResolvedPresentation,
    resolved: &BTreeMap<String, ResolvedSection>,
) -> Option<String> {
    let one_line = |text: String| {
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        (!text.is_empty()).then_some(text)
    };
    presentation
        .summary
        .source
        .iter()
        .find_map(|source| match source {
            SummarySource::Plugin => anatomy.summary.clone().and_then(one_line),
            SummarySource::FirstLine => first_line(anatomy, resolved),
            SummarySource::Template => presentation
                .summary
                .template
                .as_deref()
                .map(|template| render_template(template, anatomy, resolved))
                .and_then(one_line),
        })
}

/// Render a summary template through the policy's bounded renderer, so the
/// placeholders it substitutes are exactly those validation accepts.
///
/// `count` has no value for a single tool unit and renders empty.
fn render_template(
    template: &str,
    anatomy: &ToolAnatomy,
    resolved: &BTreeMap<String, ResolvedSection>,
) -> String {
    let header = &anatomy.header;
    let value = |name: &str| -> Option<String> {
        match name {
            "tool" => header.tool.clone(),
            "target" => header.target.clone(),
            "status" => header.status.clone(),
            "timing" => timing_text(header),
            "title" => header
                .title_override
                .clone()
                .or_else(|| header.title.clone()),
            "first_line" => first_line(anatomy, resolved),
            _ => None,
        }
    };
    bcode_presentation_policy::render_summary_template(template, &value)
}

fn preview_lines<T>(lines: &[T], max_rows: usize, keep: PreviewKeep) -> Vec<&T> {
    if lines.len() <= max_rows {
        return lines.iter().collect();
    }
    match keep {
        PreviewKeep::Head => lines.iter().take(max_rows).collect(),
        PreviewKeep::Tail => lines.iter().skip(lines.len() - max_rows).collect(),
        PreviewKeep::Both => {
            let head = max_rows.div_ceil(2);
            let tail = max_rows - head;
            lines
                .iter()
                .take(head)
                .chain(lines.iter().skip(lines.len() - tail))
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use bcode_plugin_sdk::tui::PluginTuiSelectionRow;
    use bcode_plugin_sdk::tui_visual::TuiVisualAnchor;
    use bcode_presentation_policy::models::{
        Disclosure, HeaderSpec, Preview, PreviewKeep, ResolvedPresentation, ResolvedSection,
        SummarySource, SummarySpec,
    };
    use bmux_tui::prelude::{Line, Span};
    use bmux_tui::text_width::display_width;

    use super::{
        BodyRole, ComposeContext, ComposeOutput, HeaderData, Section, SectionBody, ToolAnatomy,
        compose_tool,
    };
    use crate::transcript::ToolTiming;

    const ITEM: u64 = 7;

    struct Composed {
        rows: Vec<String>,
        anchors: Vec<TuiVisualAnchor>,
        selection: Vec<(usize, PluginTuiSelectionRow)>,
    }

    impl Composed {
        fn anchor(&self, key: &str) -> Option<usize> {
            self.anchors
                .iter()
                .find(|anchor| anchor.key == key)
                .map(|anchor| anchor.row)
        }
    }

    fn lines(count: usize) -> String {
        (1..=count)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn header() -> HeaderData {
        HeaderData {
            label: Some("Tool result".to_owned()),
            tool: Some("read".to_owned()),
            status: Some("ok".to_owned()),
            state: Some(bcode_session_view_models::ToolInvocationViewStatus::Finished),
            ..HeaderData::default()
        }
    }

    fn anatomy(sections: Vec<Section>) -> ToolAnatomy {
        ToolAnatomy {
            header: header(),
            summary: None,
            sections,
        }
    }

    fn presentation(disclosure: Disclosure) -> ResolvedPresentation {
        ResolvedPresentation {
            disclosure,
            ..bcode_presentation_policy::host_defaults()
        }
    }

    fn compose_with(
        anatomy: ToolAnatomy,
        presentation: &ResolvedPresentation,
        width: u16,
        sections: &BTreeMap<&str, Disclosure>,
        structural: bool,
    ) -> Composed {
        let section = |name: &str| {
            let mut section = presentation.section(name);
            if let Some(disclosure) = sections.get(name) {
                section.disclosure = *disclosure;
            }
            section
        };
        let (mut rows, mut anchors, mut selection) = (Vec::new(), Vec::new(), Vec::new());
        compose_tool(
            anatomy,
            &ComposeContext {
                presentation,
                structural,
                item_id: ITEM,
                width,
                section: &section,
            },
            ComposeOutput {
                rows: &mut rows,
                anchors: &mut anchors,
                selection: &mut selection,
            },
        );
        for row in &rows {
            assert!(
                display_width(&row.plain_text()) <= usize::from(width.max(1)),
                "row {row:?} exceeds width {width}"
            );
        }
        Composed {
            rows: rows.iter().map(Line::plain_text).collect(),
            anchors,
            selection,
        }
    }

    fn compose(anatomy: ToolAnatomy, presentation: &ResolvedPresentation) -> Composed {
        compose_with(anatomy, presentation, 60, &BTreeMap::new(), true)
    }

    fn output(count: usize) -> Section {
        Section::text("output", "output", &lines(count), BodyRole::Output)
    }

    #[test]
    fn disclosure_table_selects_rows_for_each_level() {
        let expanded = compose(
            anatomy(vec![output(2)]),
            &presentation(Disclosure::Expanded),
        );
        assert_eq!(
            expanded.rows,
            [
                "▾ Tool result · read · ok",
                "  output",
                "    line 1",
                "    line 2",
                ""
            ]
        );
        let summary = compose(anatomy(vec![output(2)]), &presentation(Disclosure::Summary));
        assert_eq!(summary.rows, ["▸ Tool result · read · ok", "  line 1", ""]);
        let collapsed = compose(
            anatomy(vec![output(2)]),
            &presentation(Disclosure::Collapsed),
        );
        assert_eq!(collapsed.rows, ["▸ Tool result · read · ok", ""]);
        let hidden = compose(anatomy(vec![output(2)]), &presentation(Disclosure::Hidden));
        assert_eq!(hidden.rows, ["▸ 1 tool call hidden", ""]);
        for composed in [&expanded, &summary, &collapsed, &hidden] {
            assert_eq!(composed.anchor("disclosure:unit:7"), Some(0));
        }
    }

    #[test]
    fn collapsed_sections_render_a_marked_label_with_row_count() {
        let sections = vec![
            Section::text("arguments", "path", "a.rs", BodyRole::Argument),
            Section::text("arguments", "limit", "10", BodyRole::Argument),
            output(14),
        ];
        let expanded = compose(
            anatomy(sections.clone()),
            &presentation(Disclosure::Expanded),
        );
        let arguments = expanded.anchor("disclosure:section:7:arguments").unwrap();
        assert_eq!(expanded.rows[arguments], "  path");
        let output_row = expanded.anchor("disclosure:section:7:output").unwrap();
        assert_eq!(expanded.rows[output_row], "  output");

        let overrides = BTreeMap::from([
            ("output", Disclosure::Collapsed),
            ("arguments", Disclosure::Collapsed),
        ]);
        let collapsed = compose_with(
            anatomy(sections),
            &presentation(Disclosure::Expanded),
            60,
            &overrides,
            true,
        );
        assert_eq!(
            collapsed.rows,
            [
                "▾ Tool result · read · ok",
                "  ▸ arguments · 2 rows",
                "  ▸ output · 14 rows",
                ""
            ]
        );
        assert_eq!(collapsed.anchor("disclosure:section:7:arguments"), Some(1));
        assert_eq!(collapsed.anchor("disclosure:section:7:output"), Some(2));
    }

    #[test]
    fn summarized_and_invisible_sections() {
        let summary = compose_with(
            anatomy(vec![output(3)]),
            &presentation(Disclosure::Expanded),
            60,
            &BTreeMap::from([("output", Disclosure::Summary)]),
            true,
        );
        assert_eq!(summary.rows[1..], ["  ▸ output · 3 rows", "    line 1", ""]);

        let mut hidden = presentation(Disclosure::Expanded);
        hidden.sections.insert(
            "output".to_owned(),
            ResolvedSection {
                visible: false,
                ..ResolvedSection::defaults(hidden.preview)
            },
        );
        let composed = compose(anatomy(vec![output(3)]), &hidden);
        assert_eq!(composed.rows, ["▾ Tool result · read · ok", ""]);
        assert_eq!(composed.anchor("disclosure:section:7:output"), None);
    }

    #[test]
    fn previews_keep_head_tail_or_both_with_a_hidden_rows_marker() {
        for (keep, expected) in [
            (PreviewKeep::Head, ["line 1", "line 2", "line 3"]),
            (PreviewKeep::Tail, ["line 4", "line 5", "line 6"]),
            (PreviewKeep::Both, ["line 1", "line 2", "line 6"]),
        ] {
            let presentation = ResolvedPresentation {
                preview: Preview { lines: 3, keep },
                ..presentation(Disclosure::Expanded)
            };
            let composed = compose(anatomy(vec![output(6)]), &presentation);
            let body = composed.rows[2..5]
                .iter()
                .map(|row| row.trim())
                .collect::<Vec<_>>();
            assert_eq!(body, expected, "{keep:?}");
            assert_eq!(composed.rows[5].trim(), "… 3 output rows hidden …");
        }
    }

    #[test]
    fn summaries_follow_sources_in_order() {
        let sections = || {
            vec![
                Section::text("arguments", "command", "ls -la", BodyRole::Argument),
                Section::text(
                    "output",
                    "output",
                    "\n   \n  first\nsecond",
                    BodyRole::Output,
                ),
            ]
        };
        let summary = presentation(Disclosure::Summary);
        let first = compose(anatomy(sections()), &summary);
        assert_eq!(
            first.rows[1], "  first",
            "output, not its label or arguments"
        );

        let mut with_plugin = anatomy(sections());
        with_plugin.summary = Some("plugin says hi".to_owned());
        assert_eq!(compose(with_plugin, &summary).rows[1], "  plugin says hi");

        let templated = ResolvedPresentation {
            summary: SummarySpec {
                source: vec![SummarySource::Template, SummarySource::FirstLine],
                template: Some("{tool} {status} {bogus}{first_line}".to_owned()),
            },
            ..summary.clone()
        };
        assert_eq!(
            compose(anatomy(sections()), &templated).rows[1],
            "  read ok first"
        );

        let only_arguments = anatomy(vec![Section::text(
            "arguments",
            "command",
            "ls -la",
            BodyRole::Argument,
        )]);
        assert_eq!(compose(only_arguments, &summary).rows[1], "  ls -la");
    }

    #[test]
    fn header_fields_follow_the_presentation_and_reproduce_the_default() {
        let timed = ToolAnatomy {
            header: HeaderData {
                timing: Some(ToolTiming {
                    duration_ms: Some(42),
                    ..ToolTiming::default()
                }),
                ..header()
            },
            ..ToolAnatomy::default()
        };
        assert_eq!(
            compose(timed.clone(), &presentation(Disclosure::Expanded)).rows[0],
            "▾ Tool result · read · ok · duration 42ms"
        );
        let chosen = ResolvedPresentation {
            header: Some(HeaderSpec {
                fields: ["tool", "marker", "timing", "target", "icon"]
                    .map(str::to_owned)
                    .to_vec(),
            }),
            ..presentation(Disclosure::Expanded)
        };
        assert_eq!(compose(timed, &chosen).rows[0], "read ▾ duration 42ms ≡");

        let overridden = ToolAnatomy {
            header: HeaderData {
                title_override: Some("Edited file".to_owned()),
                ..header()
            },
            ..ToolAnatomy::default()
        };
        assert_eq!(
            compose(overridden, &presentation(Disclosure::Expanded)).rows[0],
            "▾ Edited file"
        );
    }

    #[test]
    fn non_structural_units_have_no_marker_or_anchors() {
        let composed = compose_with(
            anatomy(vec![output(1)]),
            &presentation(Disclosure::Collapsed),
            60,
            &BTreeMap::new(),
            false,
        );
        assert_eq!(composed.rows[0], "Tool result · read · ok");
        assert!(composed.rows.iter().any(|row| row.contains("line 1")));
        assert_eq!(composed.anchors, []);
    }

    fn visual_section() -> Section {
        Section {
            name: "visual".to_owned(),
            label: None,
            body: SectionBody::Rows {
                rows: vec![Line::raw("plugin row 0"), Line::raw("plugin row 1")],
                anchors: vec![TuiVisualAnchor {
                    key: "plugin:body".to_owned(),
                    row: 1,
                    source: None,
                }],
                selection: vec![(
                    1,
                    PluginTuiSelectionRow {
                        identity: "plugin:sel".to_owned(),
                        byte_start: 0,
                        text: "plugin row 1".to_owned(),
                        cells: vec![bcode_plugin_sdk::tui::PluginTuiSelectionCell {
                            column: 0,
                            width: 1,
                            bytes: 0..1,
                        }],
                        revision: 1,
                    },
                )],
            },
        }
    }

    #[test]
    fn pre_rendered_rows_keep_anchors_selection_and_indent_consistently() {
        let composed = compose(
            anatomy(vec![visual_section()]),
            &presentation(Disclosure::Expanded),
        );
        assert_eq!(composed.rows[1], "plugin row 0");
        assert_eq!(composed.anchor("plugin:body"), Some(2));
        assert_eq!(composed.selection[0].0, 2);

        let indented = ResolvedPresentation {
            indent: 2,
            ..presentation(Disclosure::Expanded)
        };
        let composed = compose(anatomy(vec![visual_section()]), &indented);
        assert_eq!(composed.rows[0], "  ▾ Tool result · read · ok");
        assert_eq!(composed.rows[2], "  plugin row 1");
        assert_eq!(composed.anchor("plugin:body"), Some(2));
        assert_eq!(composed.selection[0].1.cells[0].column, 2);

        let collapsed = compose_with(
            anatomy(vec![visual_section()]),
            &presentation(Disclosure::Expanded),
            60,
            &BTreeMap::from([("visual", Disclosure::Collapsed)]),
            true,
        );
        assert_eq!(collapsed.rows[1], "  ▸ visual · 2 rows");
        assert_eq!(collapsed.anchor("disclosure:section:7:visual"), Some(1));
        assert_eq!(collapsed.anchor("plugin:body"), None);
        assert_eq!(collapsed.selection, []);
    }

    #[test]
    fn self_drawn_headers_paint_only_when_not_expanded() {
        let self_drawn = || ToolAnatomy {
            header: HeaderData {
                self_drawn: true,
                ..header()
            },
            ..anatomy(vec![visual_section()])
        };
        let expanded = compose(self_drawn(), &presentation(Disclosure::Expanded));
        assert_eq!(expanded.rows[0], "plugin row 0");
        assert_eq!(expanded.anchor("disclosure:unit:7"), Some(0));
        let collapsed = compose(self_drawn(), &presentation(Disclosure::Collapsed));
        assert_eq!(collapsed.rows, ["▸ Tool result · read · ok", ""]);
    }

    #[test]
    fn tiny_widths_and_wide_graphemes_stay_within_bounds() {
        let wide = "界界界 👩‍💻 e\u{301}e\u{301} 漢字テキスト\n第二行 🚀🚀🚀";
        for disclosure in [
            Disclosure::Expanded,
            Disclosure::Summary,
            Disclosure::Collapsed,
            Disclosure::Hidden,
        ] {
            for width in 0..=8 {
                let mut tool = anatomy(vec![
                    Section::text("arguments", "命令", wide, BodyRole::Argument),
                    Section::text("output", "output", wide, BodyRole::Output),
                    Section::line("error", "tool failed 失败".to_owned(), BodyRole::Failure),
                ]);
                tool.header.tool = Some("工具👩‍💻".to_owned());
                let indented = ResolvedPresentation {
                    indent: 8,
                    preview: Preview {
                        lines: 1,
                        keep: PreviewKeep::Tail,
                    },
                    ..presentation(disclosure)
                };
                // `compose_with` asserts every row fits the width.
                let composed = compose_with(tool, &indented, width, &BTreeMap::new(), true);
                assert_ne!(composed.rows, [] as [String; 0]);
                assert_eq!(composed.anchor("disclosure:unit:7"), Some(0));
            }
        }
    }

    #[test]
    fn styled_header_keeps_marker_and_title_in_separate_spans() {
        let presentation = presentation(Disclosure::Expanded);
        let section = |name: &str| presentation.section(name);
        let mut rows = Vec::new();
        compose_tool(
            anatomy(Vec::new()),
            &ComposeContext {
                presentation: &presentation,
                structural: true,
                item_id: ITEM,
                width: 40,
                section: &section,
            },
            ComposeOutput {
                rows: &mut rows,
                anchors: &mut Vec::new(),
                selection: &mut Vec::new(),
            },
        );
        let spans: &[Span] = &rows[0].spans;
        assert_eq!(spans[0].content, "▾ ");
        assert_ne!(spans[0].style, spans[1].style);
    }

    fn plugin_section(name: &str, label: Option<&str>, count: usize) -> Section {
        Section {
            name: name.to_owned(),
            label: label.map(str::to_owned),
            body: SectionBody::Lines {
                rows: (1..=count)
                    .map(|row| Line::from(format!("row {row}")))
                    .collect(),
            },
        }
    }

    #[test]
    fn plugin_sections_are_bounded_by_their_preview_and_collapse_by_name() {
        let presentation = ResolvedPresentation {
            sections: BTreeMap::from([(
                "bcode.git.files".to_owned(),
                ResolvedSection {
                    visible: true,
                    disclosure: Disclosure::Expanded,
                    preview: Preview {
                        lines: 2,
                        keep: PreviewKeep::Tail,
                    },
                },
            )]),
            ..presentation(Disclosure::Expanded)
        };
        let composed = compose(
            anatomy(vec![plugin_section("bcode.git.files", Some("files"), 5)]),
            &presentation,
        );
        assert_eq!(
            composed.rows,
            [
                "▾ Tool result · read · ok",
                "  files",
                "row 4",
                "row 5",
                "    … 3 files rows hidden …",
                ""
            ]
        );
        assert_eq!(
            composed.anchor("disclosure:section:7:bcode.git.files"),
            Some(1)
        );
        let collapsed = compose_with(
            anatomy(vec![plugin_section("bcode.git.files", Some("files"), 5)]),
            &presentation,
            60,
            &BTreeMap::from([("bcode.git.files", Disclosure::Collapsed)]),
            true,
        );
        assert_eq!(collapsed.rows[1], "  ▸ files · 5 rows");
        assert_eq!(
            collapsed.anchor("disclosure:section:7:bcode.git.files"),
            Some(1)
        );
    }

    #[test]
    fn plugin_section_rows_are_clipped_to_tiny_widths() {
        for width in [0_u16, 1, 2, 5] {
            let wide = Section {
                name: "bcode.test.wide".to_owned(),
                label: Some("wide".to_owned()),
                body: SectionBody::Lines {
                    rows: vec![Line::from("漢字👍e\u{301}漢字")],
                },
            };
            let composed = compose_with(
                anatomy(vec![wide]),
                &presentation(Disclosure::Expanded),
                width,
                &BTreeMap::new(),
                true,
            );
            assert_ne!(composed.rows, Vec::<String>::new());
        }
    }

    #[test]
    fn plugin_summary_and_target_feed_summary_sources_and_templates() {
        let mut unit = anatomy(vec![output(3)]);
        unit.summary = Some("src/picker.rs · 14 lines".to_owned());
        unit.header.target = Some("src/picker.rs".to_owned());
        let summary = presentation(Disclosure::Summary);
        assert_eq!(
            compose(unit.clone(), &summary).rows[1],
            "  src/picker.rs · 14 lines"
        );
        let templated = ResolvedPresentation {
            summary: SummarySpec {
                source: vec![SummarySource::Template],
                template: Some("{tool} {target} {count}{ tool }".to_owned()),
            },
            ..summary
        };
        // `count` has no value for one tool; non-placeholder braces stay literal.
        assert_eq!(
            compose(unit, &templated).rows[1],
            "  read src/picker.rs { tool }"
        );
    }

    fn with_unit_chrome<T>(chrome: crate::theme::chrome::UnitChrome, run: impl FnOnce() -> T) -> T {
        let initial = crate::theme::resolve_initial_theme();
        let mut theme = initial.presented(initial.accent);
        let previous = crate::render::semantic_state_theme();
        theme.unit_chrome = chrome;
        crate::render::set_plugin_visual_theme(&theme);
        let result = run();
        crate::render::set_plugin_visual_theme(&previous);
        result
    }

    #[test]
    fn markers_and_icons_follow_the_theme_icon_set() {
        use bcode_tui_theme::chrome::IconSet;
        let fields = ResolvedPresentation {
            header: Some(HeaderSpec {
                fields: ["marker", "icon", "tool", "status"]
                    .map(str::to_owned)
                    .to_vec(),
            }),
            ..presentation(Disclosure::Collapsed)
        };
        let compose_in = |icons| {
            with_unit_chrome(
                crate::theme::chrome::UnitChrome {
                    icons,
                    ..crate::theme::chrome::UnitChrome::default()
                },
                || {
                    let collapsed = compose(
                        anatomy(vec![Section::text(
                            "output",
                            "output",
                            "x",
                            BodyRole::Output,
                        )]),
                        &fields,
                    );
                    let sections = compose_with(
                        anatomy(vec![output(2)]),
                        &presentation(Disclosure::Expanded),
                        60,
                        &BTreeMap::from([("output", Disclosure::Collapsed)]),
                        true,
                    );
                    (collapsed.rows[0].clone(), sections.rows[1].clone())
                },
            )
        };
        assert_eq!(
            compose_in(IconSet::None),
            ("> read · ok".to_owned(), "  > output · 2 rows".to_owned())
        );
        assert_eq!(
            compose_in(IconSet::Unicode),
            ("▸ ≡ read · ok".to_owned(), "  ▸ output · 2 rows".to_owned())
        );
        let (nerd, _) = compose_in(IconSet::Nerd);
        assert!(nerd.ends_with(" read · ok"), "{nerd}");
        assert_eq!(
            display_width(&nerd),
            display_width("▸ ≡ read · ok"),
            "nerd glyphs measure like their unicode fallbacks: {nerd}"
        );
    }
}
