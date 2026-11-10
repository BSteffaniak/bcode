//! Shared producer helpers.
//!
//! Container framing from theme recipes, insets and spacing, header rows, marker glyphs, plain
//! fallback rows, source-mapped text and the one-row status component. Every helper composes BMUX primitives and derives geometry from the retained
//! layout node; none measures text (B1, B2, B3).
//!
//! Narrow layouts degrade explicitly (B4): framing and indentation are dropped when they would
//! leave no content column, and single rows clip.

use std::sync::Arc;

use bcode_presentation_policy::models::{Disclosure, Spacing, UnitStatus};
use bcode_session_view_models::{TranscriptViewItem, TranscriptViewItemKind};
use bcode_tui_components::chrome::ChromeLine;
use bcode_tui_components::transcript::{
    TranscriptContainerBorder, TranscriptContainerLayout, TranscriptContainerRecipe,
    TranscriptContainerStyle, TranscriptContainerWidth,
};
use bcode_tui_theme::chrome::{BorderStyle, IconSet};
use bcode_tui_theme::definition::{
    ContainerBorder, ContainerLayout, ContainerWidth, ResolvedThemeDefinition,
};
use bmux_tui::chrome::{Border, BorderSet, BorderSides};
use bmux_tui::component::{
    Component, ComponentRevision, Constraints, EventCx, LayoutCx, LayoutId, LayoutNode,
    LogicalInsets, LogicalRect, LogicalSize, SyncElement,
};
use bmux_tui::composition::{Surface, TextBlock};
use bmux_tui::event::{Event, EventOutcome};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::style::Style;
use bmux_tui::text::{Line, Span, Text, TextWrap};

use super::{ItemContent, ProducedItem, RetainedObject, UnitProducer, UnitView};
use crate::layout::SourcePosition;
use crate::layout::context::ItemRenderContext;
use crate::layout::geometry::{
    ItemGeometry, ItemSelectionFragment, SelectableRun, SelectableRunKind, SelectableText,
    SourceRow, SourceRows,
};
use crate::units::{BoundaryEdge, UnitKind};
use crate::window::BoundaryStatus;

// ---- one-row status component ------------------------------------------------------------

/// A one-row component whose geometry never depends on its text.
///
/// It always measures exactly one row at the assigned width and clips its line when painting. Used for rows whose text is
/// paint-only state (boundary status, U3, L5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusRow {
    line: Line,
}

impl StatusRow {
    /// A row showing `line`.
    #[must_use]
    pub const fn new(line: Line) -> Self {
        Self { line }
    }
}

impl Component for StatusRow {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        cx.record_measurement();
        LayoutNode::leaf(
            LayoutId::new("status-row"),
            constraints.constrain(LogicalSize::new(constraints.max_width(), 1)),
        )
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        let width = u16::try_from(layout.size.width).unwrap_or(u16::MAX);
        cx.write_line(LocalRect::new(0, 0, width, 1), &self.line);
    }
}

impl ItemContent for StatusRow {}

/// Boundary row text (U3, W13).
#[must_use]
pub const fn boundary_text(edge: BoundaryEdge, status: BoundaryStatus) -> &'static str {
    match (edge, status) {
        (_, BoundaryStatus::Loading) => "loading…",
        (BoundaryEdge::Older, BoundaryStatus::MoreAvailable) => "earlier messages available",
        (BoundaryEdge::Older, BoundaryStatus::Exhausted) => "start of session",
        (BoundaryEdge::Newer, _) => "newer messages available",
    }
}

/// The boundary row for `edge` in `status`: exactly one row at every width and in every state
/// (U3). Status is paint-only; the item source paints the current status over the retained
/// one-row layout.
#[must_use]
pub fn boundary_row(edge: BoundaryEdge, status: BoundaryStatus) -> StatusRow {
    StatusRow::new(Line::raw(boundary_text(edge, status)))
}

/// Producer of boundary rows (layout only; the status is painted by the item source).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BoundaryProducer;

impl UnitProducer for BoundaryProducer {
    fn produce(&self, unit: UnitView<'_>, _cx: &ItemRenderContext<'_>) -> ProducedItem {
        let edge = match unit.unit.kind {
            UnitKind::Boundary(edge) => edge,
            _ => BoundaryEdge::Older,
        };
        ProducedItem::new(boundary_row(edge, BoundaryStatus::MoreAvailable))
    }
}

// ---- plain fallback rows -----------------------------------------------------------------

/// Short semantic label of an item kind.
#[must_use]
pub const fn item_label(item: &TranscriptViewItem) -> &'static str {
    match &item.kind {
        TranscriptViewItemKind::UserMessage { .. } => "user",
        TranscriptViewItemKind::AssistantMessage { .. } => "assistant",
        TranscriptViewItemKind::ReasoningMessage { .. }
        | TranscriptViewItemKind::ReasoningActivity { .. } => "reasoning",
        TranscriptViewItemKind::ToolInvocation { .. }
        | TranscriptViewItemKind::ToolRequestDraft { .. }
        | TranscriptViewItemKind::ToolRequest { .. }
        | TranscriptViewItemKind::ToolContribution { .. } => "tool",
        TranscriptViewItemKind::Permission { .. } => "permission",
        TranscriptViewItemKind::Compaction { .. } => "compaction",
        TranscriptViewItemKind::Interaction { .. } => "interaction",
        TranscriptViewItemKind::Skill { .. } => "skill",
        TranscriptViewItemKind::SystemMessage { .. } => "system",
    }
}

/// Plain rows of an item: message text split into source lines (a Markdown hard-break
/// backslash at a line end is dropped), or a one-row kind label for other items.
#[must_use]
pub fn plain_lines(item: &TranscriptViewItem) -> Vec<Line> {
    let text = match &item.kind {
        TranscriptViewItemKind::UserMessage { message }
        | TranscriptViewItemKind::AssistantMessage { message }
        | TranscriptViewItemKind::ReasoningMessage { message }
        | TranscriptViewItemKind::SystemMessage { message } => Some(message.text.as_str()),
        _ => None,
    };
    text.map_or_else(
        || vec![Line::raw(format!("[{}]", item_label(item)))],
        |text| {
            text.split('\n')
                .map(|line| Line::raw(line.strip_suffix('\\').unwrap_or(line).to_owned()))
                .collect()
        },
    )
}

/// One unwrapped row; overflow clips.
#[must_use]
pub fn single_row(text: impl Into<String>) -> TextBlock {
    TextBlock::new(Text::from_lines(vec![Line::raw(text.into())])).wrap(TextWrap::None)
}

/// Plain renderer-neutral component for any unit (A8, E5): word-wrapped message rows, a kind
/// label for other items, and one-row forms for boundaries, group headers and hidden markers.
#[must_use]
pub fn plain_unit(unit: UnitView<'_>) -> TextBlock {
    match unit.unit.kind {
        UnitKind::Boundary(edge) => single_row(boundary_text(edge, BoundaryStatus::MoreAvailable)),
        UnitKind::GroupHeader { members, expanded } => {
            let marker = if expanded { "▾" } else { "▸" };
            let label = unit.item.map_or("items", item_label);
            single_row(format!("{marker} {members} {label}"))
        }
        UnitKind::HiddenMarker { count } => single_row(hidden_marker_text(count, IconSet::Unicode)),
        UnitKind::Item => {
            let lines = unit.item.map_or_else(Vec::new, plain_lines);
            TextBlock::new(Text::from_lines(lines)).wrap(TextWrap::Word)
        }
    }
}

// ---- marker glyphs -----------------------------------------------------------------------

/// Disclosure marker for a toggleable header: expanded `▾`, otherwise `▸` (ASCII `v`/`>`
/// without icons).
#[must_use]
pub const fn disclosure_marker(disclosure: Disclosure, icons: IconSet) -> &'static str {
    match (icons, disclosure) {
        (IconSet::None, Disclosure::Expanded) => "v",
        (IconSet::None, _) => ">",
        (_, Disclosure::Expanded) => "▾",
        (_, _) => "▸",
    }
}

/// Status glyph of a unit (ASCII without icons).
#[must_use]
pub const fn status_glyph(status: Option<UnitStatus>, icons: IconSet) -> &'static str {
    let unicode = !matches!(icons, IconSet::None);
    match status {
        None | Some(UnitStatus::Requested) => {
            if unicode {
                "○"
            } else {
                "o"
            }
        }
        Some(UnitStatus::Running | UnitStatus::Waiting) => {
            if unicode {
                "◐"
            } else {
                "*"
            }
        }
        Some(UnitStatus::Succeeded) => {
            if unicode {
                "✓"
            } else {
                "+"
            }
        }
        Some(UnitStatus::Failed | UnitStatus::TimedOut) => {
            if unicode {
                "✗"
            } else {
                "x"
            }
        }
        Some(UnitStatus::Cancelled) => {
            if unicode {
                "⊘"
            } else {
                "-"
            }
        }
    }
}

/// Hidden-items marker text, `▸ N hidden` (A16).
#[must_use]
pub fn hidden_marker_text(count: u64, icons: IconSet) -> String {
    format!(
        "{} {count} hidden",
        disclosure_marker(Disclosure::Collapsed, icons)
    )
}

/// Spinner glyph for a frame counter (paint slots, L5).
#[must_use]
pub fn spinner_glyph(frame: u64) -> &'static str {
    const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
    FRAMES[usize::try_from(frame % 8).unwrap_or(0)]
}

/// Compact elapsed label (`950ms`, `12s`, `3m04s`) for paint slots.
#[must_use]
pub fn elapsed_label(millis: u64) -> String {
    match millis {
        0..1_000 => format!("{millis}ms"),
        1_000..60_000 => format!("{}s", millis / 1_000),
        _ => format!("{}m{:02}s", millis / 60_000, (millis / 1_000) % 60),
    }
}

// ---- header rows -------------------------------------------------------------------------

/// One header segment.
#[derive(Debug, Clone)]
pub struct HeaderSegment {
    /// Text.
    pub text: String,
    /// Style.
    pub style: Style,
    /// Drop priority: `None` is required; lower numbers drop first.
    pub priority: Option<u8>,
    /// Whether the segment may be truncated with an ellipsis.
    pub truncatable: bool,
}

impl HeaderSegment {
    /// A required segment.
    #[must_use]
    pub fn required(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
            priority: None,
            truncatable: false,
        }
    }

    /// An optional segment dropped before higher priorities.
    #[must_use]
    pub fn optional(text: impl Into<String>, style: Style, priority: u8) -> Self {
        Self {
            text: text.into(),
            style,
            priority: Some(priority),
            truncatable: false,
        }
    }

    /// Return the segment marked truncatable.
    #[must_use]
    pub const fn truncatable(mut self) -> Self {
        self.truncatable = true;
        self
    }
}

/// Compose a header row that fits `width` cells: optional segments drop lowest priority
/// first, then truncatable segments shorten with an ellipsis (`ChromeLine`, cell widths).
#[must_use]
pub fn header_line(segments: Vec<HeaderSegment>, separator: &str, width: u64) -> Line {
    let line =
        segments
            .into_iter()
            .fold(
                ChromeLine::new(separator, Style::new()),
                |line, segment| match segment.priority {
                    None => line.required(segment.text, segment.style, segment.truncatable),
                    Some(priority) => {
                        line.optional(segment.text, segment.style, priority, segment.truncatable)
                    }
                },
            );
    Line::from_spans(line.spans(usize::try_from(width).unwrap_or(usize::MAX)))
}

/// One-row header component (clipped, never wrapped).
#[must_use]
pub fn header_row(line: Line) -> TextBlock {
    TextBlock::new(Text::from_lines(vec![line])).wrap(TextWrap::None)
}

/// A styled span (re-exported convenience for producers).
#[must_use]
pub fn span(text: impl Into<String>, style: Style) -> Span {
    Span::styled(text.into(), style)
}

// ---- theme chrome ------------------------------------------------------------------------

/// BMUX border glyphs for a theme border style; `None` for no border.
#[must_use]
pub const fn border_set(style: BorderStyle) -> Option<BorderSet> {
    match style {
        BorderStyle::None => None,
        BorderStyle::Ascii => Some(BorderSet::ASCII),
        BorderStyle::Single | BorderStyle::Left => Some(BorderSet::SINGLE),
        BorderStyle::Rounded => Some(BorderSet::ROUNDED),
        BorderStyle::Thick => Some(BorderSet::THICK),
        BorderStyle::Double => Some(BorderSet::DOUBLE),
    }
}

/// Container role of a tool status (`tool.<status>`).
#[must_use]
pub const fn tool_container_role(status: Option<UnitStatus>) -> &'static str {
    match status {
        None | Some(UnitStatus::Requested) => "tool.requested",
        Some(UnitStatus::Running) => "tool.running",
        Some(UnitStatus::Waiting) => "tool.waiting",
        Some(UnitStatus::Succeeded) => "tool.succeeded",
        Some(UnitStatus::Failed) => "tool.failed",
        Some(UnitStatus::Cancelled) => "tool.cancelled",
        Some(UnitStatus::TimedOut) => "tool.timed_out",
    }
}

/// Resolved container style of a theme container `role`.
///
/// Roles are for example `transcript.user` or `tool.failed`; the style is with the theme style `style_role`. Tool roles use the chrome's tool
/// border glyphs. A missing recipe resolves to the plain recipe.
#[must_use]
pub fn container_style(
    theme: &ResolvedThemeDefinition,
    role: &str,
    style_role: &str,
) -> TranscriptContainerStyle {
    let recipe = theme.containers.get(role).copied();
    let border_set = if role.starts_with("tool.") {
        theme.chrome.borders.tool.and_then(border_set)
    } else {
        theme.chrome.borders.panel.and_then(border_set)
    }
    .unwrap_or(BorderSet::SINGLE);
    TranscriptContainerStyle {
        recipe: recipe.map_or(
            TranscriptContainerRecipe {
                layout: TranscriptContainerLayout::Plain,
                width: TranscriptContainerWidth::Content,
                border: TranscriptContainerBorder::None,
                padding_x: 0,
                padding_y: 0,
            },
            |recipe| TranscriptContainerRecipe {
                layout: match recipe.layout {
                    ContainerLayout::Plain => TranscriptContainerLayout::Plain,
                    ContainerLayout::LeftBar => TranscriptContainerLayout::LeftBar,
                    ContainerLayout::Panel => TranscriptContainerLayout::Panel,
                },
                width: match recipe.width {
                    ContainerWidth::Content => TranscriptContainerWidth::Content,
                    ContainerWidth::Full => TranscriptContainerWidth::Full,
                },
                border: match recipe.border {
                    ContainerBorder::None => TranscriptContainerBorder::None,
                    ContainerBorder::Left => TranscriptContainerBorder::Left,
                    ContainerBorder::All => TranscriptContainerBorder::All,
                },
                padding_x: recipe.padding_x,
                padding_y: recipe.padding_y,
            },
        ),
        style: theme.style(style_role).unwrap_or_default(),
        border_set,
    }
}

// ---- geometry-forwarding wrappers ----------------------------------------------------------

/// One item content shared between a retained BMUX composition (which lays it out, paints it
/// and routes events to it) and the geometry pass (which reads it from the child node the
/// composition produced), so wrappers keep geometry derivation on the inner content's own node.
#[derive(Clone)]
pub(super) struct SharedContent(Arc<dyn ItemContent>);

impl SharedContent {
    pub(super) fn new(content: Box<dyn ItemContent>) -> Self {
        Self(Arc::from(content))
    }

    /// The content's geometry read from `layout`, the node it produced.
    pub(super) fn geometry(&self, layout: &LayoutNode) -> ItemGeometry {
        self.0.geometry(layout)
    }
}

impl Component for SharedContent {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.0.layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.0.paint(layout, cx);
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.0.event(event, layout, cx)
    }

    fn revision(&self) -> ComponentRevision {
        self.0.revision()
    }
}

/// A BMUX `Surface` (insets, border, background) around one content, forwarding the inner
/// content's geometry translated by the child placement.
pub struct Wrapped {
    surface: Surface<'static, RetainedObject>,
    inner: SharedContent,
}

impl std::fmt::Debug for Wrapped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wrapped").finish_non_exhaustive()
    }
}

impl Wrapped {
    fn new(
        inner: Box<dyn ItemContent>,
        build: impl FnOnce(Surface<'static, RetainedObject>) -> Surface<'static, RetainedObject>,
    ) -> Self {
        let inner = SharedContent::new(inner);
        let surface = build(Surface::from_element(SyncElement::erase(inner.clone())));
        Self { surface, inner }
    }
}

impl Component for Wrapped {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.surface.layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.surface.paint(layout, cx);
    }

    fn event(&self, event: &Event, layout: &LayoutNode, cx: &mut EventCx<'_>) -> EventOutcome {
        self.surface.event(event, layout, cx)
    }

    fn revision(&self) -> ComponentRevision {
        self.surface.revision()
    }
}

impl ItemContent for Wrapped {
    fn geometry(&self, layout: &LayoutNode) -> ItemGeometry {
        layout
            .children
            .first()
            .map_or_else(ItemGeometry::default, |child| {
                self.inner
                    .geometry(&child.node)
                    .translated(child.x, child.y)
            })
    }
}

/// Inset `content` by `insets` (indentation, spacing rows). Horizontal insets are dropped when
/// they would leave no content column at `width` (explicit narrow degradation, B4); vertical
/// insets are always kept.
#[must_use]
pub fn inset(
    content: Box<dyn ItemContent>,
    insets: LogicalInsets,
    width: u64,
) -> Box<dyn ItemContent> {
    let insets = if insets.horizontal() >= width {
        LogicalInsets::new(insets.top, 0, insets.bottom, 0)
    } else {
        insets
    };
    if insets == LogicalInsets::all(0) {
        return content;
    }
    Box::new(Wrapped::new(content, |surface| {
        surface.id("inset").padding(insets)
    }))
}

/// Apply policy spacing (blank rows, clamped) and indent (columns) around `content`.
#[must_use]
pub fn spaced(
    content: Box<dyn ItemContent>,
    spacing: Spacing,
    indent: u8,
    width: u64,
) -> Box<dyn ItemContent> {
    let spacing = spacing.clamped();
    inset(
        content,
        LogicalInsets::new(
            u64::from(spacing.before),
            0,
            u64::from(spacing.after),
            u64::from(indent),
        ),
        width,
    )
}

/// Insets a container style reserves (border plus padding).
#[must_use]
pub const fn frame_insets(style: &TranscriptContainerStyle) -> LogicalInsets {
    if matches!(style.recipe.layout, TranscriptContainerLayout::Plain) {
        return LogicalInsets::all(0);
    }
    let (top, right, bottom, left) = match style.recipe.border {
        TranscriptContainerBorder::None => (0, 0, 0, 0),
        TranscriptContainerBorder::Left => (0, 0, 0, 1),
        TranscriptContainerBorder::All => (1, 1, 1, 1),
    };
    let px = style.recipe.padding_x as u64;
    let py = style.recipe.padding_y as u64;
    LogicalInsets::new(top + py, right + px, bottom + py, left + px)
}

/// Frame `content` with a theme container style (border glyphs, fill, padding).
///
/// The frame is one BMUX surface. Plain recipes return the content unchanged; a frame that would leave no content
/// column at `width` is dropped (explicit narrow degradation, B4). The frame paints the full
/// assigned width.
#[must_use]
pub fn framed(
    content: Box<dyn ItemContent>,
    style: TranscriptContainerStyle,
    width: u64,
) -> Box<dyn ItemContent> {
    let insets = frame_insets(&style);
    if matches!(style.recipe.layout, TranscriptContainerLayout::Plain)
        || insets.horizontal() >= width
    {
        return content;
    }
    let border = match style.recipe.border {
        TranscriptContainerBorder::None => None,
        TranscriptContainerBorder::Left => {
            Some(Border::new(style.border_set, style.style).sides(BorderSides::LEFT))
        }
        TranscriptContainerBorder::All => {
            Some(Border::new(style.border_set, style.style).sides(BorderSides::ALL))
        }
    };
    let padding = LogicalInsets::new(
        u64::from(style.recipe.padding_y),
        u64::from(style.recipe.padding_x),
        u64::from(style.recipe.padding_y),
        u64::from(style.recipe.padding_x),
    );
    Box::new(Wrapped::new(content, |surface| {
        let surface = surface.id("frame").background(style.style).padding(padding);
        match border {
            Some(border) => surface.border(border),
            None => surface,
        }
    }))
}

// ---- source-mapped text ------------------------------------------------------------------

/// How a text block's byte ranges map to source positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextSource {
    /// Markdown source bytes starting at `base`.
    Markdown {
        /// Byte offset of the block's first byte in the item source.
        base: u64,
    },
    /// Shell output bytes starting at `base`.
    Shell {
        /// Byte offset of the block's first byte in the item output.
        base: u64,
    },
    /// Diff lines starting at `base` (one source line per block line).
    Diff {
        /// Diff line of the block's first line.
        base: u64,
    },
}

/// A text block whose measured rows map to source positions.
///
/// The source map is read from the retained row geometry of the measured layout (BMUX 9,
/// `TextBlock::source_range_for_row`) without materializing projected rows, and the block is
/// one source-backed selection fragment (C34, B6).
#[derive(Debug, Clone)]
pub struct SourceText {
    block: TextBlock,
    source: TextSource,
    /// Byte offset of each source line in the block's newline-separated source text, recorded
    /// when the lines are known ([`Self::from_lines`]); diff rows map to lines through it.
    line_starts: Option<Arc<[usize]>>,
    /// Logical source of the block's newline-separated text when its painted lines differ from
    /// it (control characters painted as spaces, byte for byte); a selection copies it (C34).
    copy_source: Option<Arc<str>>,
}

impl SourceText {
    /// Map `block`'s rows through `source`.
    ///
    /// Diff sources map rows to lines through the block's projection; prefer
    /// [`Self::from_lines`], which maps them from retained row geometry alone.
    #[must_use]
    pub const fn new(block: TextBlock, source: TextSource) -> Self {
        Self {
            block,
            source,
            line_starts: None,
            copy_source: None,
        }
    }

    /// A block of `lines` wrapped by `wrap`, mapped through `source`.
    #[must_use]
    pub fn from_lines(lines: Vec<Line>, wrap: TextWrap, source: TextSource) -> Self {
        // Byte offsets into the newline-separated source text (never cell widths), matching
        // the offsets BMUX reports per measured row.
        let mut offset = 0usize;
        let line_starts = lines
            .iter()
            .map(|line| {
                let start = offset;
                offset = line
                    .spans
                    .iter()
                    .fold(offset, |base, span| base.saturating_add(span.content.len()))
                    .saturating_add(1);
                start
            })
            .collect::<Arc<[usize]>>();
        Self {
            block: TextBlock::new(Text::from_lines(lines)).wrap(wrap),
            source,
            line_starts: Some(line_starts),
            copy_source: None,
        }
    }

    /// Copy `source` instead of the painted text: the logical text the block's lines were
    /// derived from byte for byte (control characters painted as spaces). A source whose byte
    /// length differs from the painted text is ignored, so run ranges always index both.
    #[must_use]
    pub fn with_copy_source(mut self, source: impl Into<Arc<str>>) -> Self {
        self.copy_source = Some(source.into());
        self
    }

    fn position(
        &self,
        start: usize,
        line: impl FnOnce() -> Option<usize>,
    ) -> Option<SourcePosition> {
        let offset = |base: u64| base.saturating_add(start as u64);
        Some(match self.source {
            TextSource::Markdown { base } => SourcePosition::MarkdownByte(offset(base)),
            TextSource::Shell { base } => SourcePosition::ShellByte(offset(base)),
            TextSource::Diff { base } => {
                SourcePosition::DiffLine(base.saturating_add(line()? as u64))
            }
        })
    }
}

impl Component for SourceText {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.block.layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.block.paint(layout, cx);
    }

    fn revision(&self) -> ComponentRevision {
        self.block.revision()
    }
}

impl ItemContent for SourceText {
    fn geometry(&self, layout: &LayoutNode) -> ItemGeometry {
        let height = layout.size.height;
        let rows = match (&self.source, &self.line_starts) {
            (TextSource::Diff { .. }, None) => self
                .block
                .projection_window(layout, 0..height)
                .indexed_rows()
                .filter_map(|(row, projected)| {
                    Some(SourceRow {
                        row,
                        position: self.position(0, || Some(projected.source_line))?,
                    })
                })
                .collect::<Vec<_>>(),
            (_, line_starts) => (0..height)
                .filter_map(|row| {
                    let start = self.block.source_range_for_row(layout, row)?.start;
                    let line = || {
                        let starts = line_starts.as_deref()?;
                        starts
                            .partition_point(|line_start| *line_start <= start)
                            .checked_sub(1)
                    };
                    Some(SourceRow {
                        row,
                        position: self.position(start, line)?,
                    })
                })
                .collect::<Vec<_>>(),
        };
        let first = rows.first().map(|row| row.position.clone());
        ItemGeometry {
            source_map: SourceRows::new(rows),
            selection_fragments: vec![ItemSelectionFragment {
                rows: 0..height,
                source: first,
                rendered: false,
            }],
            selectable: self.selectable(layout).into_iter().collect(),
            ..ItemGeometry::default()
        }
    }
}

impl SourceText {
    /// The block's newline-separated logical text with one run per measured row whose
    /// rendered row is exactly its painted bytes (C34). Rows are read from the retained row
    /// geometry once, when the item is measured (B6); rows whose rendering differs from their
    /// text (none for plain blocks) are not selectable rather than mapped untruthfully. The
    /// copied text is the copy source when one was given ([`Self::with_copy_source`]).
    fn selectable(&self, layout: &LayoutNode) -> Option<SelectableText> {
        let painted: Arc<str> = Arc::from(
            self.block
                .text()
                .lines
                .iter()
                .map(Line::plain_text)
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let width = layout.size.width;
        let runs = self
            .block
            .projection_window(layout, 0..layout.size.height)
            .indexed_rows()
            .filter(|(_, row)| {
                painted.get(row.source_range.clone()) == Some(&*row.line.plain_text())
            })
            .map(|(row, projected)| SelectableRun {
                row,
                x: 0,
                width,
                source: projected.source_range.clone(),
                kind: SelectableRunKind::Text,
            })
            .collect::<Vec<_>>();
        if runs.is_empty() {
            return None;
        }
        Some(match &self.copy_source {
            Some(source) if source.len() == painted.len() && **source != *painted => {
                SelectableText {
                    text: Arc::clone(source),
                    display: Some(painted),
                    runs,
                }
            }
            _ => SelectableText::new(painted, runs),
        })
    }
}

/// Item-local rectangle of a node found by layout id, if present.
#[must_use]
pub fn find_rect(layout: &LayoutNode, id: &LayoutId) -> Option<LogicalRect> {
    layout.find_logical_rect(id)
}

#[cfg(test)]
mod tests;
