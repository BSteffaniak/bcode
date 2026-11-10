//! Message producers: user, assistant, reasoning, system and note (compaction, skill) units.
//!
//! Every non-tool transcript item kind renders here:
//!
//! | Item kind | Producer | Heading | Body |
//! |---|---|---|---|
//! | `UserMessage` | [`UserMessageProducer`] | `You` (speaker chrome) | Markdown or text |
//! | `AssistantMessage` | [`AssistantMessageProducer`] | author, `…` while streaming (speaker chrome) | streaming Markdown |
//! | `ReasoningMessage`, `ReasoningActivity` | [`ReasoningProducer`] | `Reasoning`, status title | Markdown, availability note |
//! | `SystemMessage` | [`SystemMessageProducer`] | `System`, `Plugin · <label>` | Markdown or muted text |
//! | `Compaction` | [`NoteProducer`] | none | one metadata line `· <text>` |
//! | `Skill` | [`NoteProducer`] | `Skill`, `Skill context`, `Skill error` | detail text |
//!
//! Permissions and interactions belong to the tool producers.
//!
//! # Anatomy
//!
//! A message unit is, from the outside in: policy spacing and indent ([`common::spaced`]), the
//! optional theme container ([`common::framed`]), the speaker treatment ([`SpeakerLayout`]),
//! and a column of heading, summary and body. Disclosure selects the column (V1 parity):
//!
//! * `expanded`: heading and body;
//! * `summary`: heading and one summary row;
//! * `collapsed`: heading only;
//! * `hidden`: a one-row count marker (`▸ 1 message hidden`), so a hidden unit leaves a
//!   visible, clickable trace (visibility floor). Reasoning hidden by the `tui.thinking`
//!   setting is suppressed during structure resolution and never reaches a producer.
//!
//! The heading (or hidden marker) row is the unit's disclosure focus target and toggle hit
//! region.
//!
//! # One layout definition
//!
//! [`MessageLayout`] resolves every column the unit reserves (indent, frame insets, speaker
//! prefix, body indent) once from the context width, with the same narrow-width degradation
//! rules the wrappers apply. The wrappers are built from those values, and the Markdown body
//! wraps to the resolved body width. Painting, hit regions, selection and source maps then come
//! from the single retained BMUX layout of that composition (B3, B4, L6). No text is measured
//! here except the BMUX cell width of the badge heading, which decides whether the badge fits.

use std::borrow::Cow;

use bcode_presentation_policy::models::{
    Disclosure, ResolvedPresentation, Spacing, SummarySource, UnitFacts, UnitStatus,
};
use bcode_session_view_models::{
    ChatMessageView, CompactionView, ReasoningActivityView, ReasoningContentAvailability,
    SkillViewStatus, TextFormat, TranscriptViewItem, TranscriptViewItemKind,
};
use bcode_tui_components::transcript::TranscriptContainerStyle;
use bcode_tui_theme::chrome::{IconSet, SpeakerStyle};
use bcode_tui_theme::definition::ResolvedThemeDefinition;
use bmux_tui::chrome::{Border, BorderSet, BorderSides};
use bmux_tui::component::{
    Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutNode, LogicalInsets,
    SyncElement,
};
use bmux_tui::composition::{Column, Row, SizeBox, Surface, TextBlock};
use bmux_tui::event::{Event, EventOutcome};
use bmux_tui::paint::PaintCx;
use bmux_tui::style::{Modifier, Style};
use bmux_tui::text::{Line, Span, Text, TextWrap};
use bmux_tui::text_width::display_width;

use super::common::{self, SharedContent, SourceText, TextSource};
use super::group::DisclosureRow;
use super::markdown::{self, MarkdownRequest};
use super::{
    ItemContent, ItemRenderContext, ProducedItem, ProducerWork, RetainedObject, UnitProducer,
    UnitView,
};
use crate::layout::geometry::{ItemFocusTarget, ItemGeometry, SourceRows};
use crate::units::structure::{DisclosureTarget, ResolvedUnitPresentation};

/// Cells reserved for the heading in the `gutter` speaker treatment. Wider headings clip; the
/// body starts at the same column on every row.
pub const GUTTER_LABEL_CELLS: u64 = 8;

/// Narrowest content width (gutter included) that keeps a gutter; narrower messages use the
/// `prefix` treatment.
pub const MIN_GUTTER_WIDTH: u64 = GUTTER_LABEL_CELLS + 12;

/// Blank cells between the speaker bar and the body.
pub const BAR_GAP: u64 = 1;

/// Narrowest body width beside a bar; narrower messages use the `prefix` treatment.
pub const MIN_BAR_BODY_CELLS: u64 = 4;

/// Indent of message bodies under their heading.
pub const BODY_INDENT: u64 = 2;

/// Spacing the host default presentation resolves to; it keeps each kind's natural trailing
/// rows (metadata notes have none).
const DEFAULT_SPACING: Spacing = Spacing {
    before: 0,
    after: 1,
};

// ---- layout definition --------------------------------------------------------------------

/// Speaker treatment of one message at one content width (theme `chrome.speaker`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpeakerLayout {
    /// Effective treatment after narrow-width degradation.
    pub style: SpeakerStyle,
    /// Columns left of the body on every row (`bar` and `gutter`).
    pub prefix: u64,
}

impl SpeakerLayout {
    /// The plain `prefix` treatment (no reserved columns).
    pub const PREFIX: Self = Self {
        style: SpeakerStyle::Prefix,
        prefix: 0,
    };

    /// Columns the bar occupies: the BMUX left border plus the gap.
    #[must_use]
    pub const fn bar_prefix() -> u64 {
        BorderSides::LEFT.insets().left as u64 + BAR_GAP
    }

    /// Treatment of a message at content `width`. Only speaker messages (user and assistant)
    /// carry one; others use `prefix`.
    ///
    /// Degradation is explicit: `bar` needs the bar, its gap and [`MIN_BAR_BODY_CELLS`];
    /// `gutter` needs [`MIN_GUTTER_WIDTH`]; otherwise the message uses `prefix`. `badge`
    /// reserves no columns (it falls back per heading when the badge does not fit).
    #[must_use]
    pub const fn resolve(style: SpeakerStyle, speaker: bool, width: u64) -> Self {
        if !speaker {
            return Self::PREFIX;
        }
        match style {
            SpeakerStyle::Prefix => Self::PREFIX,
            SpeakerStyle::Badge => Self {
                style: SpeakerStyle::Badge,
                prefix: 0,
            },
            SpeakerStyle::Bar => {
                let prefix = Self::bar_prefix();
                if width >= prefix + MIN_BAR_BODY_CELLS {
                    Self {
                        style: SpeakerStyle::Bar,
                        prefix,
                    }
                } else {
                    Self::PREFIX
                }
            }
            SpeakerStyle::Gutter => {
                if width >= MIN_GUTTER_WIDTH {
                    Self {
                        style: SpeakerStyle::Gutter,
                        prefix: GUTTER_LABEL_CELLS,
                    }
                } else {
                    Self::PREFIX
                }
            }
        }
    }
}

/// Width left after reserving `reserved` columns, or `None` when the reservation must be
/// dropped because it would leave no column (the rule of [`common::inset`] and
/// [`common::framed`]).
const fn reserve(width: u64, reserved: u64) -> Option<u64> {
    if reserved == 0 {
        Some(width)
    } else if reserved >= width {
        None
    } else {
        Some(width - reserved)
    }
}

/// Every column a message unit reserves at one width, resolved once (B3, B4).
#[derive(Debug, Clone)]
pub struct MessageLayout {
    /// Unit width.
    pub width: u64,
    /// Effective policy indent (dropped when it would leave no column).
    pub indent: u64,
    /// Effective container frame (dropped when plain or when it would leave no column).
    pub frame: Option<TranscriptContainerStyle>,
    /// Width inside indent and frame.
    pub content_width: u64,
    /// Effective speaker treatment at `content_width`.
    pub speaker: SpeakerLayout,
    /// Width of the heading and body column beside the speaker prefix.
    pub column_width: u64,
    /// Effective body indent (dropped when it would leave no column).
    pub body_indent: u64,
    /// Width the body wraps to.
    pub body_width: u64,
}

impl MessageLayout {
    /// Resolve the layout of a unit at `width`.
    #[must_use]
    pub fn resolve(
        width: u64,
        indent: u8,
        frame: Option<TranscriptContainerStyle>,
        speaker_style: SpeakerStyle,
        speaker: bool,
    ) -> Self {
        let (indent, after_indent) =
            reserve(width, u64::from(indent)).map_or((0, width), |rest| (u64::from(indent), rest));
        let (frame, content_width) = frame
            .filter(|style| !is_plain(style))
            .and_then(|style| {
                reserve(after_indent, common::frame_insets(&style).horizontal())
                    .map(|rest| (Some(style), rest))
            })
            .unwrap_or((None, after_indent));
        let speaker = SpeakerLayout::resolve(speaker_style, speaker, content_width);
        let column_width = content_width - speaker.prefix;
        let (body_indent, body_width) = reserve(column_width, BODY_INDENT)
            .map_or((0, column_width), |rest| (BODY_INDENT, rest));
        Self {
            width,
            indent,
            frame,
            content_width,
            speaker,
            column_width,
            body_indent,
            body_width,
        }
    }
}

const fn is_plain(style: &TranscriptContainerStyle) -> bool {
    matches!(
        style.recipe.layout,
        bcode_tui_components::transcript::TranscriptContainerLayout::Plain
    )
}

// ---- composition ---------------------------------------------------------------------------

/// A retained, thread-safe column of parts.
type RetainedColumn = Column<'static, RetainedObject>;

/// A BMUX composition (`Column`, `Row`, `Surface`, `SizeBox`) whose `i`-th layout child is
/// `parts[i]`; geometry is the parts' geometry translated by their child placement.
struct Composite {
    element: Box<RetainedObject>,
    parts: Vec<SharedContent>,
}

impl Composite {
    fn column(parts: Vec<Box<dyn ItemContent>>) -> Box<dyn ItemContent> {
        let parts: Vec<SharedContent> = parts.into_iter().map(SharedContent::new).collect();
        let column = parts.iter().fold(
            RetainedColumn::empty().id("message-column"),
            |column, part| column.child(part.clone()),
        );
        Box::new(Self {
            element: Box::new(column),
            parts,
        })
    }

    fn single(
        part: Box<dyn ItemContent>,
        build: impl FnOnce(SyncElement<'static>) -> Box<RetainedObject>,
    ) -> Box<dyn ItemContent> {
        let part = SharedContent::new(part);
        Box::new(Self {
            element: build(SyncElement::erase(part.clone())),
            parts: vec![part],
        })
    }

    fn row(fixed: Box<dyn ItemContent>, flex: Box<dyn ItemContent>) -> Box<dyn ItemContent> {
        let fixed = SharedContent::new(fixed);
        let flex = SharedContent::new(flex);
        let row = Row::<'static, RetainedObject>::empty()
            .id("message-gutter")
            .child(fixed.clone())
            .flex_child(1, flex.clone());
        Box::new(Self {
            element: Box::new(row),
            parts: vec![fixed, flex],
        })
    }
}

impl Component for Composite {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.element.layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.element.paint(layout, cx);
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.element.event(event, layout, cx)
    }

    fn revision(&self) -> ComponentRevision {
        self.element.revision()
    }
}

impl ItemContent for Composite {
    fn geometry(&self, layout: &LayoutNode) -> ItemGeometry {
        let mut merged = ItemGeometry::default();
        let mut source_rows = Vec::new();
        for (part, child) in self.parts.iter().zip(&layout.children) {
            let geometry = part.geometry(&child.node).translated(child.x, child.y);
            source_rows.extend_from_slice(geometry.source_map.rows());
            let offset = u32::try_from(merged.focus_targets.len()).unwrap_or(u32::MAX);
            merged
                .focus_targets
                .extend(
                    geometry
                        .focus_targets
                        .into_iter()
                        .map(|target| ItemFocusTarget {
                            local: target.local.saturating_add(offset),
                            parent: target.parent.map(|parent| parent.saturating_add(offset)),
                            ..target
                        }),
                );
            merged.hit_regions.extend(geometry.hit_regions);
            merged
                .selection_fragments
                .extend(geometry.selection_fragments);
            merged.content_keys.extend(geometry.content_keys);
            merged.paint_slots.extend(geometry.paint_slots);
            merged.visual_slots.extend(geometry.visual_slots);
            merged.selectable.extend(geometry.selectable);
        }
        merged.source_map = SourceRows::new(source_rows);
        merged
    }
}

// ---- theme roles ---------------------------------------------------------------------------

/// Theme styles a message reads.
#[derive(Debug, Clone, Copy)]
struct Styles {
    text: Style,
    muted: Style,
    metadata: Style,
}

impl Styles {
    fn of(theme: &ResolvedThemeDefinition) -> Self {
        let muted = markdown::muted_style(theme);
        Self {
            text: markdown::text_style(theme),
            muted,
            metadata: theme.style("tool.metadata").unwrap_or(muted),
        }
    }
}

fn state(theme: &ResolvedThemeDefinition, name: &str, fallback: Style) -> Style {
    theme.style(name).unwrap_or(fallback)
}

// ---- message description -------------------------------------------------------------------

/// What a message's body shows.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Body<'a> {
    /// Markdown source (streaming while the item streams).
    Markdown(Cow<'a, str>),
    /// Plain text rows.
    Plain(Cow<'a, str>),
}

impl Body<'_> {
    fn text(&self) -> &str {
        match self {
            Self::Markdown(text) | Self::Plain(text) => text,
        }
    }
}

/// Everything a message producer resolves from its item before composing.
#[derive(Debug, Clone)]
struct Message<'a> {
    /// Heading text (speaker label or title).
    title: String,
    /// Heading style.
    title_style: Style,
    /// Body source.
    body: Body<'a>,
    /// Body text style for plain bodies.
    body_style: Style,
    /// Whether the message carries a speaker (user and assistant).
    speaker: bool,
    /// Default theme container role (`transcript.user`, `transcript.assistant`).
    container: Option<&'static str>,
    /// Metadata note: no heading, one `· text` row, no trailing row.
    meta: bool,
    /// Noun of the hidden marker.
    noun: &'static str,
}

/// Body of a chat message by format: Markdown, plain text, or pretty-printed JSON.
fn chat_body<'a>(message: &'a ChatMessageView, text: Cow<'a, str>) -> Body<'a> {
    match message.format {
        TextFormat::Markdown => Body::Markdown(text),
        TextFormat::PlainText => Body::Plain(text),
        TextFormat::Json => Body::Plain(
            serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|value| serde_json::to_string_pretty(&value).ok())
                .map_or(text, Cow::Owned),
        ),
    }
}

fn labeled(role: &str, label: Option<&str>) -> String {
    label.map_or_else(|| role.to_owned(), |label| format!("{role} · {label}"))
}

const fn reasoning_title(
    status: bcode_session_models::ReasoningActivityStatus,
    streaming: bool,
) -> &'static str {
    match status {
        _ if streaming => "Reasoning …",
        bcode_session_models::ReasoningActivityStatus::Completed => "Reasoning",
        bcode_session_models::ReasoningActivityStatus::Interrupted => "Reasoning interrupted",
        bcode_session_models::ReasoningActivityStatus::Failed => "Reasoning failed",
    }
}

/// Body of a reasoning activity: readable text, or an explanation of why there is none.
fn reasoning_activity_body(activity: &ReasoningActivityView) -> String {
    match activity.content_availability() {
        ReasoningContentAvailability::Readable => activity.text(),
        ReasoningContentAvailability::Filtered => {
            "_Reasoning content hidden by the current `/thinking` display setting._".to_owned()
        }
        ReasoningContentAvailability::Withheld => {
            "_The model reasoned about this step but did not return readable reasoning content._"
                .to_owned()
        }
        ReasoningContentAvailability::Pending => String::new(),
    }
}

fn compaction(theme: &ResolvedThemeDefinition, compaction: &CompactionView) -> Message<'static> {
    let muted = markdown::muted_style(theme);
    let style = state(theme, "transcript.meta", muted);
    Message {
        title: String::new(),
        title_style: style,
        body: Body::Plain(Cow::Owned(compaction.text.clone())),
        body_style: style,
        speaker: false,
        container: None,
        meta: true,
        noun: "note",
    }
}

/// Resolve the message description of `item`; `None` for kinds this module does not render.
#[allow(clippy::too_many_lines)] // One arm per non-tool item kind, in V1 order.
fn describe<'a>(
    item: &'a TranscriptViewItem,
    theme: &ResolvedThemeDefinition,
) -> Option<Message<'a>> {
    let styles = Styles::of(theme);
    let info = state(theme, "state.info", styles.text);
    let focused = state(theme, "border.focused", info);
    let success = state(theme, "state.success", styles.text);
    let error = state(theme, "state.error", styles.text);
    let detail_body = state(theme, "transcript.detail.body", styles.muted);
    let message = match &item.kind {
        TranscriptViewItemKind::UserMessage { message } => Message {
            title: labeled("You", message.display_label.as_deref()),
            title_style: state(theme, "transcript.user.label", focused)
                .add_modifier(Modifier::BOLD),
            body: chat_body(message, Cow::Borrowed(message.text.as_str())),
            body_style: Style::new(),
            speaker: true,
            container: Some("transcript.user"),
            meta: false,
            noun: "message",
        },
        TranscriptViewItemKind::AssistantMessage { message } => {
            let author = message.display_label.as_deref().unwrap_or("Bcode");
            let (title, style) = if item.streaming {
                (
                    format!("{author} …"),
                    state(theme, "tool.running.title", info),
                )
            } else {
                (
                    author.to_owned(),
                    state(theme, "transcript.assistant.label", success),
                )
            };
            Message {
                title,
                title_style: style.add_modifier(Modifier::BOLD),
                body: chat_body(
                    message,
                    bcode_session_view_models::presentation::model_output_text(&message.text),
                ),
                body_style: Style::new(),
                speaker: true,
                container: Some("transcript.assistant"),
                meta: false,
                noun: "message",
            }
        }
        TranscriptViewItemKind::ReasoningMessage { message } => Message {
            title: if item.streaming {
                "Reasoning …"
            } else {
                "Reasoning"
            }
            .to_owned(),
            title_style: state(theme, "transcript.reasoning.label", styles.muted),
            body: chat_body(message, Cow::Borrowed(message.text.as_str())),
            body_style: styles.muted,
            speaker: false,
            container: None,
            meta: false,
            noun: "reasoning block",
        },
        TranscriptViewItemKind::ReasoningActivity { activity } => Message {
            title: reasoning_title(activity.status, item.streaming).to_owned(),
            title_style: state(theme, "transcript.reasoning.label", styles.muted),
            body: Body::Markdown(Cow::Owned(reasoning_activity_body(activity))),
            body_style: styles.muted,
            speaker: false,
            container: None,
            meta: false,
            noun: "reasoning block",
        },
        TranscriptViewItemKind::SystemMessage { message } => Message {
            title: message.display_label.as_deref().map_or_else(
                || "System".to_owned(),
                |label| labeled("Plugin", Some(label)),
            ),
            title_style: state(theme, "transcript.system.label", styles.muted),
            body: chat_body(message, Cow::Borrowed(message.text.as_str())),
            body_style: styles.muted,
            speaker: false,
            container: None,
            meta: false,
            noun: "note",
        },
        TranscriptViewItemKind::Compaction { compaction: note } => compaction(theme, note),
        TranscriptViewItemKind::Skill { skill } => {
            let (title, style) = match skill.status {
                SkillViewStatus::Invoked | SkillViewStatus::Suggested => {
                    ("Skill", state(theme, "transcript.skill.label", focused))
                }
                SkillViewStatus::ContextLoaded => (
                    "Skill context",
                    state(theme, "transcript.detail.label", styles.muted),
                ),
                SkillViewStatus::Failed => (
                    "Skill error",
                    state(theme, "transcript.skill_error.label", error),
                ),
            };
            Message {
                title: title.to_owned(),
                title_style: style,
                body: Body::Plain(Cow::Borrowed(skill.text.as_str())),
                body_style: detail_body,
                speaker: false,
                container: None,
                meta: false,
                noun: "note",
            }
        }
        _ => return None,
    };
    Some(message)
}

// ---- heading, summary and marker rows ------------------------------------------------------

/// Status text shown for non-success lifecycles.
const fn status_text(status: UnitStatus) -> Option<&'static str> {
    match status {
        UnitStatus::Succeeded => None,
        UnitStatus::Requested => Some("requested"),
        UnitStatus::Running => Some("running"),
        UnitStatus::Waiting => Some("waiting"),
        UnitStatus::Failed => Some("failed"),
        UnitStatus::TimedOut => Some("timed out"),
        UnitStatus::Cancelled => Some("cancelled"),
    }
}

/// First non-blank line of `text`, trimmed.
fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

/// Value of a header or template field for a message unit.
fn field_value(message: &Message<'_>, facts: &UnitFacts, field: &str) -> Option<String> {
    match field {
        "label" | "title" => Some(message.title.clone()),
        "tool" => facts.tool.clone(),
        "target" => facts.target.clone(),
        "status" => status_text(facts.status?).map(str::to_owned),
        "first_line" => first_line(message.body.text()).map(str::to_owned),
        _ => None,
    }
}

/// Leading disclosure marker and joined header text of a message unit.
///
/// Without header fields, expanded units show the title and other disclosures add a leading
/// marker. Registered fields render as: `marker` the disclosure glyph, `label`/`title` the
/// title, `tool` and `target` the facts, `status` a non-success lifecycle; other fields are
/// empty for messages and skipped.
fn header_text(
    message: &Message<'_>,
    presentation: &ResolvedPresentation,
    facts: &UnitFacts,
    disclosure: Disclosure,
    icons: IconSet,
) -> (Option<&'static str>, String) {
    let glyph = common::disclosure_marker(disclosure, icons);
    let Some(header) = presentation.header.as_ref() else {
        let marker = (disclosure != Disclosure::Expanded).then_some(glyph);
        return (marker, message.title.clone());
    };
    let mut marker = None;
    let mut text = String::new();
    let mut previous_marker = false;
    for (index, field) in header.fields.iter().enumerate() {
        let (piece, is_marker) = if field == "marker" {
            if index == 0 {
                marker = Some(glyph);
                continue;
            }
            (glyph.to_owned(), true)
        } else {
            match field_value(message, facts, field) {
                Some(value) if !value.is_empty() => (value, false),
                _ => continue,
            }
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

/// One-line summary from the presentation's ordered sources (messages have no producer
/// summary), whitespace collapsed.
fn summary_text(
    message: &Message<'_>,
    presentation: &ResolvedPresentation,
    facts: &UnitFacts,
) -> Option<String> {
    let summary = &presentation.summary;
    summary.source.iter().find_map(|source| {
        let text = match source {
            SummarySource::Plugin => return None,
            SummarySource::FirstLine => first_line(message.body.text())?.to_owned(),
            SummarySource::Template => bcode_presentation_policy::render_summary_template(
                summary.template.as_deref()?,
                &|field| field_value(message, facts, field),
            ),
        };
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        (!text.is_empty()).then_some(text)
    })
}

/// Heading spans: the muted marker, then the styled text.
fn heading_spans(
    (marker, text): &(Option<&'static str>, String),
    title_style: Style,
    metadata: Style,
) -> Vec<Span> {
    let mut spans = Vec::new();
    if let Some(marker) = marker {
        let marker = if text.is_empty() {
            (*marker).to_owned()
        } else {
            format!("{marker} ")
        };
        spans.push(Span::styled(marker, metadata));
    }
    if !text.is_empty() {
        spans.push(Span::styled(text.clone(), title_style));
    }
    spans
}

/// A heading block toggling `target`: wrapped at word boundaries, or one clipped row.
fn heading_block(
    spans: Vec<Span>,
    wrap: TextWrap,
    target: Option<DisclosureTarget>,
) -> Box<dyn ItemContent> {
    let block = TextBlock::new(Text::from_lines(vec![Line::from_spans(spans)])).wrap(wrap);
    Box::new(DisclosureRow::new(block, target))
}

/// The filled badge heading (` You ` after the marker), or `None` when it does not fit
/// `width` cells (BMUX cell width).
fn badge_spans(
    (marker, text): &(Option<&'static str>, String),
    title_style: Style,
    metadata: Style,
    width: u64,
) -> Option<Vec<Span>> {
    let mut spans = Vec::new();
    if let Some(marker) = marker {
        spans.push(Span::styled(format!("{marker} "), metadata));
    }
    if !text.is_empty() {
        spans.push(Span::styled(
            format!(" {text} "),
            title_style.add_modifier(Modifier::REVERSED),
        ));
    }
    let cells: usize = spans.iter().map(|span| display_width(&span.content)).sum();
    (cells as u64 <= width).then_some(spans)
}

/// The one-row count marker of a hidden unit.
fn hidden_marker(
    noun: &str,
    icons: IconSet,
    style: Style,
    target: Option<DisclosureTarget>,
) -> Box<dyn ItemContent> {
    let text = format!(
        "{} 1 {noun} hidden",
        common::disclosure_marker(Disclosure::Hidden, icons)
    );
    heading_block(vec![Span::styled(text, style)], TextWrap::None, target)
}

// ---- bodies --------------------------------------------------------------------------------

/// The body of an expanded message at the resolved body width, with the body indent applied.
fn body_content(
    message: &Message<'_>,
    layout: &MessageLayout,
    document_id: String,
    streaming: bool,
    cx: &ItemRenderContext<'_>,
) -> (Box<dyn ItemContent>, ProducerWork) {
    let text = message.body.text();
    let (content, work): (Box<dyn ItemContent>, ProducerWork) = if text.is_empty() {
        // An empty body keeps a visible trace (V1: `  ·`).
        (Box::new(common::single_row("·")), ProducerWork::default())
    } else {
        match &message.body {
            Body::Markdown(source) => {
                let request = MarkdownRequest::new(source, layout.body_width)
                    .document(document_id)
                    .streaming(streaming);
                let (body, work) = markdown::markdown_projection(&request, cx);
                (Box::new(body), work)
            }
            Body::Plain(source) => (
                Box::new(plain_text(source, message.body_style)),
                ProducerWork::default(),
            ),
        }
    };
    let content = common::inset(
        content,
        LogicalInsets::new(0, 0, 0, layout.body_indent),
        layout.column_width,
    );
    (content, work)
}

/// Plain text rows wrapped at word boundaries by BMUX, mapped to source bytes.
fn plain_text(source: &str, style: Style) -> SourceText {
    let lines = source
        .split('\n')
        .map(|line| Line::from_spans(vec![Span::styled(line.to_owned(), style)]))
        .collect::<Vec<_>>();
    SourceText::new(
        TextBlock::new(Text::from_lines(lines)).wrap(TextWrap::Word),
        TextSource::Markdown { base: 0 },
    )
}

/// The one-row-per-source metadata note `· text` (compaction).
fn meta_note(text: &str, style: Style) -> Box<dyn ItemContent> {
    let lines = text
        .split('\n')
        .enumerate()
        .map(|(index, line)| {
            let lead = if index == 0 { "· " } else { "  " };
            Line::from_spans(vec![
                Span::styled(lead, style),
                Span::styled(line.to_owned(), style),
            ])
        })
        .collect::<Vec<_>>();
    Box::new(TextBlock::new(Text::from_lines(lines)).wrap(TextWrap::Word))
}

// ---- composition ---------------------------------------------------------------------------

/// Presentation, facts and target of a unit, defaulting when the structure is not an item.
fn unit_structure<'a>(
    cx: &ItemRenderContext<'a>,
) -> (Cow<'a, ResolvedPresentation>, Cow<'a, UnitFacts>) {
    match cx.structure {
        ResolvedUnitPresentation::Item(item) => (
            Cow::Borrowed(&item.presentation),
            Cow::Borrowed(&item.facts),
        ),
        _ => (
            Cow::Owned(bcode_presentation_policy::host_defaults()),
            Cow::Owned(UnitFacts::default()),
        ),
    }
}

/// Theme container of a message: the policy's container id, else the kind's default role.
fn container(
    message: &Message<'_>,
    presentation: &ResolvedPresentation,
    theme: &ResolvedThemeDefinition,
) -> Option<TranscriptContainerStyle> {
    let role = presentation.container.as_deref().or(message.container)?;
    theme.containers.get(role)?;
    Some(common::container_style(
        theme,
        role,
        &format!("{role}.container"),
    ))
}

/// The speaker bar: a BMUX left border in the title style plus the gap.
fn bar(content: Box<dyn ItemContent>, style: Style, icons: IconSet) -> Box<dyn ItemContent> {
    let vertical = if matches!(icons, IconSet::None) {
        '|'
    } else {
        '\u{258e}'
    };
    let set = BorderSet {
        vertical,
        ..BorderSet::SINGLE
    };
    Composite::single(content, |part| {
        Box::new(
            Surface::from_element(part)
                .id("speaker-bar")
                .border(Border::new(set, style).sides(BorderSides::LEFT))
                .padding(LogicalInsets::new(0, 0, 0, BAR_GAP)),
        )
    })
}

/// The resolved inputs of one message unit's heading, summary and body column.
struct ColumnInputs<'a, 'm> {
    message: &'a Message<'m>,
    layout: &'a MessageLayout,
    presentation: &'a ResolvedPresentation,
    facts: &'a UnitFacts,
    styles: Styles,
    document_id: String,
    streaming: bool,
}

/// The heading of a message under its speaker treatment: a row of the column, or the gutter
/// label beside it.
enum Heading {
    Row(Box<dyn ItemContent>),
    Gutter(Box<dyn ItemContent>),
}

fn heading(inputs: &ColumnInputs<'_, '_>, cx: &ItemRenderContext<'_>) -> Heading {
    let message = inputs.message;
    let icons = cx.theme.chrome.icons;
    let target = cx.disclosure.target.clone();
    let header = header_text(
        message,
        inputs.presentation,
        inputs.facts,
        cx.disclosure.disclosure,
        icons,
    );
    let plain = || heading_spans(&header, message.title_style, inputs.styles.metadata);
    match inputs.layout.speaker.style {
        SpeakerStyle::Prefix | SpeakerStyle::Bar => {
            Heading::Row(heading_block(plain(), TextWrap::Word, target))
        }
        SpeakerStyle::Badge => Heading::Row(
            badge_spans(
                &header,
                message.title_style,
                inputs.styles.metadata,
                inputs.layout.column_width,
            )
            .map_or_else(
                || heading_block(plain(), TextWrap::Word, target.clone()),
                |spans| heading_block(spans, TextWrap::None, target.clone()),
            ),
        ),
        SpeakerStyle::Gutter => Heading::Gutter(heading_block(plain(), TextWrap::None, target)),
    }
}

/// The heading, summary and body column of a visible message, wrapped in its speaker chrome.
fn message_column(
    inputs: ColumnInputs<'_, '_>,
    cx: &ItemRenderContext<'_>,
) -> (Box<dyn ItemContent>, ProducerWork) {
    let message = inputs.message;
    let layout = inputs.layout;
    let mut rows: Vec<Box<dyn ItemContent>> = Vec::new();
    let heading = heading(&inputs, cx);
    let gutter_label = match heading {
        Heading::Row(row) => {
            rows.push(row);
            None
        }
        Heading::Gutter(label) => Some(label),
    };
    let mut work = ProducerWork::default();
    match cx.disclosure.disclosure {
        Disclosure::Expanded | Disclosure::Hidden => {
            let (body, body_work) =
                body_content(message, layout, inputs.document_id, inputs.streaming, cx);
            work = body_work;
            rows.push(body);
        }
        Disclosure::Summary => {
            if let Some(summary) = summary_text(message, inputs.presentation, inputs.facts) {
                let row = TextBlock::new(Text::from_lines(vec![Line::from_spans(vec![
                    Span::styled(summary, inputs.styles.muted),
                ])]))
                .wrap(TextWrap::None);
                rows.push(common::inset(
                    Box::new(row),
                    LogicalInsets::new(0, 0, 0, layout.body_indent),
                    layout.column_width,
                ));
            }
        }
        Disclosure::Collapsed => {}
    }
    let column = Composite::column(rows);
    let column = match gutter_label {
        Some(label) => {
            let label = Composite::single(label, |part| {
                Box::new(
                    SizeBox::from_element(part)
                        .id("speaker-gutter")
                        .width(layout.speaker.prefix)
                        .min_height(1),
                )
            });
            Composite::row(label, column)
        }
        None if layout.speaker.style == SpeakerStyle::Bar => {
            bar(column, message.title_style, cx.theme.chrome.icons)
        }
        None => column,
    };
    (column, work)
}

/// Compose and produce one message unit.
fn produce_message(
    unit: UnitView<'_>,
    cx: &ItemRenderContext<'_>,
    message: &Message<'_>,
) -> ProducedItem {
    let styles = Styles::of(cx.theme);
    let (presentation, facts) = unit_structure(cx);
    let disclosure = cx.disclosure.disclosure;
    let spacing = presentation.spacing.clamped();
    let spacing = if spacing == DEFAULT_SPACING {
        Spacing {
            before: 0,
            after: u8::from(!message.meta),
        }
    } else {
        spacing
    };
    let hidden = disclosure == Disclosure::Hidden;
    let frame = if hidden || message.meta {
        None
    } else {
        container(message, &presentation, cx.theme)
    };
    let layout = MessageLayout::resolve(
        cx.width,
        presentation
            .indent
            .min(bcode_presentation_policy::models::MAX_INDENT),
        frame,
        cx.theme.chrome.speaker,
        message.speaker && !message.meta && !hidden,
    );
    let (column, work) = if hidden {
        (
            hidden_marker(
                message.noun,
                cx.theme.chrome.icons,
                styles.metadata,
                cx.disclosure.target.clone(),
            ),
            ProducerWork::default(),
        )
    } else if message.meta {
        (
            meta_note(message.body.text(), message.body_style),
            ProducerWork::default(),
        )
    } else {
        message_column(
            ColumnInputs {
                message,
                layout: &layout,
                presentation: &presentation,
                facts: &facts,
                styles,
                document_id: crate::layout::context::markdown_document_id(&unit.unit.key),
                streaming: unit.item.is_some_and(|item| item.streaming),
            },
            cx,
        )
    };
    let framed = match layout.frame {
        Some(style) => common::framed(column, style, cx.width - layout.indent),
        None => column,
    };
    ProducedItem::boxed(common::spaced(
        framed,
        spacing,
        u8::try_from(layout.indent).unwrap_or(u8::MAX),
        cx.width,
    ))
    .with_work(work)
}

/// Produce `unit` as a message, or as the renderer-neutral fallback when its item is not a
/// message kind.
fn produce(unit: UnitView<'_>, cx: &ItemRenderContext<'_>) -> ProducedItem {
    unit.item
        .and_then(|item| describe(item, cx.theme))
        .map_or_else(
            || ProducedItem::new(common::plain_unit(unit)),
            |message| produce_message(unit, cx, &message),
        )
}

/// User message units: `You` heading with speaker chrome, Markdown or text body, the
/// `transcript.user` container.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UserMessageProducer;

impl UnitProducer for UserMessageProducer {
    fn produce(&self, unit: UnitView<'_>, cx: &ItemRenderContext<'_>) -> ProducedItem {
        produce(unit, cx)
    }
}

/// Assistant message units: author heading (`…` while streaming) with speaker chrome,
/// streaming Markdown body, the `transcript.assistant` container.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AssistantMessageProducer;

impl UnitProducer for AssistantMessageProducer {
    fn produce(&self, unit: UnitView<'_>, cx: &ItemRenderContext<'_>) -> ProducedItem {
        produce(unit, cx)
    }
}

/// Reasoning units: legacy messages and structured activities.
///
/// They show a muted heading and a Markdown body; activities without readable text explain
/// why. Reasoning switched off by `tui.thinking` is suppressed by structure resolution and
/// never produced.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReasoningProducer;

impl UnitProducer for ReasoningProducer {
    fn produce(&self, unit: UnitView<'_>, cx: &ItemRenderContext<'_>) -> ProducedItem {
        produce(unit, cx)
    }
}

/// System and plugin status message units.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SystemMessageProducer;

impl UnitProducer for SystemMessageProducer {
    fn produce(&self, unit: UnitView<'_>, cx: &ItemRenderContext<'_>) -> ProducedItem {
        produce(unit, cx)
    }
}

/// Compaction notes (one metadata row) and skill notes (heading and detail text).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoteProducer;

impl UnitProducer for NoteProducer {
    fn produce(&self, unit: UnitView<'_>, cx: &ItemRenderContext<'_>) -> ProducedItem {
        produce(unit, cx)
    }
}

#[cfg(test)]
mod tests;
