//! Markdown bodies for message producers (L7, L8, B10, C24, C38, D2).
//!
//! # Accepted projection
//!
//! A Markdown body is the BMUX Markdown projection (`bmux_markdown::render_markdown`, which
//! wraps through BMUX `TextBlock` projection and keeps source provenance) of one item source
//! at one content width. The layout cache retains exactly one produced component per resident
//! unit, keyed by `(unit, revision, fingerprint, width)`, and the fingerprint covers every
//! Markdown option ([`crate::layout::context::MarkdownOptions`] and the theme). The retained
//! [`MarkdownBody`] is therefore the item's single accepted projection: rows, source anchors,
//! hit regions, content keys and selection fragments all come from it (L7). It is replaced only
//! by a projection for the exact new id, revision, options and width (D2). Nothing here keeps a
//! second, global projection cache.
//!
//! # Streaming
//!
//! Incomplete streaming Markdown is renderer-local (C38): a streaming item renders with
//! `MarkdownRenderOptions::streaming`, which keeps the accepted source visible, redacts unsafe
//! unfinished destinations and projects unfinished constructs as text. When the item
//! completes, its next revision renders without the streaming flag and the body converges to
//! the normal projection. BMUX's `MarkdownStreamingRenderState` produces identical rows (it
//! only reuses completed paragraph rows between calls); producers are pure and keep no state
//! between calls (L4), so the stateless render is used.
//!
//! # Source anchors
//!
//! Each projected row that shows source text maps to the first source byte it shows
//! ([`SourcePosition::MarkdownByte`], C24), read from the BMUX selection provenance of the same
//! projection. Provenance costs one unit per rendered grapheme, so streaming revisions skip it:
//! their rows fall back to item-row anchors until the item completes.
//!
//! # Asynchronous visuals
//!
//! Images and Mermaid diagrams reserve fixed placeholder rows in the projection (L8). The
//! reserved rectangles are exposed as `markdown:` content keys, rendered selection fragments
//! and [`VisualSlot`]s (the request a host-side worker needs, in item-local cells). Mermaid
//! contributions are requested only when [`MarkdownOptions::mermaid`] is set.
//!
//! Host-side workers render slots outside drawing and accept the results into
//! [`MarkdownOptions::visuals`] ([`MarkdownVisualState`]), keyed by contribution id. An
//! accepted visual is a unit revision: its acceptance revision is part of the owning unit's
//! fingerprint, so exactly that unit is re-projected, and the projection applies it only on an
//! exact contribution id and source key match ([`VisualSlotRequest::source_key`]). Acceptance
//! never changes the row count (the reservation is fixed):
//!
//! * [`MarkdownVisual::Image`]: the reserved rows keep only the cells left of the slot and the
//!   body registers one frame-scoped BMUX image placement over the slot from its own retained
//!   node while painting, so placements exist only for painted, correctly positioned content
//!   and are clipped by the painting context (a scrolled-out unit contributes none).
//! * [`MarkdownVisual::Notice`]: the reserved rows show the notice lines (failures, disabled
//!   network loading), clipped at the slot.
//!
//! Either way the rows are rendered rows without source correspondence (ambiguity 10).
//!
//! [`MarkdownOptions::mermaid`]: crate::layout::context::MarkdownOptions::mermaid
//! [`MarkdownOptions::visuals`]: crate::layout::context::MarkdownOptions::visuals

use std::collections::BTreeMap;
use std::sync::Arc;

use bcode_markdown_render::{
    MarkdownContributionKind, MarkdownDestination, MarkdownDocumentContext, MarkdownRenderOptions,
    MarkdownRenderResult, MarkdownSelectionKind, MarkdownTheme, MermaidContributionRendering,
    render_markdown,
};
use bcode_syntax_render::{SyntaxColor, SyntaxPalette};
use bcode_tui_theme::definition::ResolvedThemeDefinition;
use bmux_tui::component::{
    Component, ComponentRevision, Constraints, LayoutCx, LayoutNode, LogicalRect,
};
use bmux_tui::composition::TextBlock;
use bmux_tui::geometry::Rect;
use bmux_tui::image::{
    ImageContribution, ImageFit, ImageKey, ImageLifecycle, ImagePayload, ImagePlacement,
};
use bmux_tui::paint::PaintCx;
use bmux_tui::style::{Modifier, Style};
use bmux_tui::text::{Line, Span, Text, TextWrap};

use super::{ItemContent, ItemRenderContext, ProducedItem, ProducerWork};
use crate::layout::SourcePosition;
use crate::layout::geometry::{
    ContentKeyRegion, ItemAction, ItemGeometry, ItemHitRegion, ItemSelectionFragment,
    SelectableRun, SelectableRunKind, SelectableText, SourceRow, SourceRows,
};

/// Rows reserved for one inline image placeholder (L8).
pub const IMAGE_RESERVED_ROWS: u16 = 4;

/// Rows reserved for one Mermaid diagram placeholder (L8).
pub const MERMAID_RESERVED_ROWS: u16 = 8;

/// Maximum Mermaid SVG size requested from host-side workers, in pixels.
pub const MERMAID_PIXELS: (u32, u32) = (1600, 1200);

/// Content-key namespace of Markdown contributions (E3).
pub const CONTENT_KEY_PREFIX: &str = "markdown:";

// ---- asynchronous visuals -------------------------------------------------------------------

/// What a host-side worker needs to render one reserved Markdown visual.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum VisualSlotRequest {
    /// A Mermaid diagram.
    Mermaid {
        /// Diagram source without the fence.
        source: String,
        /// Versioned renderer cache key (covers the source and the requested size).
        cache_key: String,
    },
    /// An inline image.
    Image {
        /// Parser-semantic alt text.
        alt: String,
        /// Classified source; unsafe schemes are inert.
        source: MarkdownDestination,
        /// Safe classified destination when the image is nested in a link.
        linked_destination: Option<MarkdownDestination>,
    },
}

impl VisualSlotRequest {
    /// Identity of the rendered content: an accepted visual applies to a slot only when its
    /// source key equals the slot's (exact options match). Mermaid uses its renderer cache key;
    /// images use their classified source.
    #[must_use]
    pub fn source_key(&self) -> String {
        match self {
            Self::Mermaid { cache_key, .. } => format!("mermaid:{cache_key}"),
            Self::Image { source, .. } => format!("image:{}", destination_key(source)),
        }
    }
}

/// Stable text of a classified destination.
fn destination_key(destination: &MarkdownDestination) -> String {
    match destination {
        MarkdownDestination::Web(url) => url.as_str().to_owned(),
        MarkdownDestination::LocalPath(path) => format!("file:{}", path.to_string_lossy()),
        MarkdownDestination::Fragment(fragment) => format!("#{fragment}"),
        MarkdownDestination::UnresolvedRelative(source) => format!("relative:{source}"),
        MarkdownDestination::Inert { reason } => format!("inert:{reason:?}"),
    }
}

/// One reserved asynchronous visual of an item (L8), in item-local cells of the retained
/// layout it was derived from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct VisualSlot {
    /// Owner-qualified contribution id (`transcript:<unit>:...`); the acceptance key.
    pub contribution_id: String,
    /// Reserved rectangle: from the slot's first column to the body's right edge, over every
    /// reserved row.
    pub rect: LogicalRect,
    /// What a worker renders for the slot.
    pub request: VisualSlotRequest,
}

/// A host-accepted rendering of one visual slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownVisual {
    /// Decoded image content placed over the slot (contained, aspect ratio preserved).
    Image(Arc<ImagePayload>),
    /// Text rows replacing the slot's rows (one row per line, clipped at the slot).
    Notice(Arc<str>),
}

/// One accepted visual (L8): a worker result accepted for an exact contribution and source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedMarkdownVisual {
    /// [`VisualSlotRequest::source_key`] of the request the result was rendered for.
    pub source_key: String,
    /// Acceptance revision; part of the owning unit's fingerprint.
    pub revision: u64,
    /// The rendering.
    pub visual: MarkdownVisual,
}

/// Accepted Markdown visuals by owner-qualified contribution id (computed outside drawing).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarkdownVisualState {
    accepted: BTreeMap<String, AcceptedMarkdownVisual>,
}

impl MarkdownVisualState {
    /// No accepted visuals.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            accepted: BTreeMap::new(),
        }
    }

    /// The accepted visual of a contribution, if any.
    #[must_use]
    pub fn accepted(&self, contribution_id: &str) -> Option<&AcceptedMarkdownVisual> {
        self.accepted.get(contribution_id)
    }

    /// The accepted visual of `slot`, when it was rendered for the slot's exact source.
    #[must_use]
    pub fn for_slot(&self, slot: &VisualSlot) -> Option<&AcceptedMarkdownVisual> {
        self.accepted_for(&slot.contribution_id, &slot.request)
    }

    fn accepted_for(
        &self,
        contribution_id: &str,
        request: &VisualSlotRequest,
    ) -> Option<&AcceptedMarkdownVisual> {
        self.accepted
            .get(contribution_id)
            .filter(|accepted| accepted.source_key == request.source_key())
    }

    /// Accept a visual for a contribution, replacing any previous one.
    pub fn accept(&mut self, contribution_id: impl Into<String>, visual: AcceptedMarkdownVisual) {
        self.accepted.insert(contribution_id.into(), visual);
    }

    /// Drop the accepted visual of a contribution; returns whether one existed.
    pub fn remove(&mut self, contribution_id: &str) -> bool {
        self.accepted.remove(contribution_id).is_some()
    }

    /// Keep only visuals whose contribution id `keep` accepts (residency reconciliation).
    pub fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.accepted.retain(|id, _| keep(id));
    }

    /// Number of accepted visuals.
    #[must_use]
    pub fn len(&self) -> usize {
        self.accepted.len()
    }

    /// Whether no visual is accepted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.accepted.is_empty()
    }

    /// Whether the accepted visuals of Markdown document `document_id` (contribution ids
    /// under `<document_id>:`) differ between `self` and `other`: a visual added, removed,
    /// replaced or re-accepted.
    #[must_use]
    pub fn document_differs(&self, other: &Self, document_id: &str) -> bool {
        let prefix = format!("{document_id}:");
        let entries = |state: &'_ Self| {
            state
                .accepted
                .range::<str, _>((
                    std::ops::Bound::Included(prefix.as_str()),
                    std::ops::Bound::Unbounded,
                ))
                .take_while(|(id, _)| id.starts_with(&prefix))
                .map(|(id, visual)| (id.clone(), visual.clone()))
                .collect::<Vec<_>>()
        };
        entries(self) != entries(other)
    }

    /// Acceptance revisions of one Markdown document's contributions (ids under
    /// `<document_id>:`), in id order: the unit-scoped fingerprint input (L3).
    pub fn revisions_for<'a>(
        &'a self,
        document_id: &str,
    ) -> impl Iterator<Item = (&'a str, u64)> + 'a {
        let prefix = format!("{document_id}:");
        self.accepted
            .range::<str, _>((
                std::ops::Bound::Included(prefix.as_str()),
                std::ops::Bound::Unbounded,
            ))
            .take_while(move |(id, _)| id.starts_with(&prefix))
            .map(|(id, accepted)| (id.as_str(), accepted.revision))
    }
}

// ---- activation targets ---------------------------------------------------------------------

/// Whether a contribution can be focused and activated.
///
/// Links with a safe web, local or fragment destination, footnote references and definitions,
/// `<details>` and Mermaid diagrams are; images and math are not, and inert or unresolved
/// destinations never are.
#[must_use]
pub const fn contribution_actionable(kind: &MarkdownContributionKind) -> bool {
    match kind {
        MarkdownContributionKind::Link { destination, .. }
        | MarkdownContributionKind::GitHubIssue { destination, .. } => matches!(
            destination,
            MarkdownDestination::Web(_)
                | MarkdownDestination::LocalPath(_)
                | MarkdownDestination::Fragment(_)
        ),
        MarkdownContributionKind::Details { .. }
        | MarkdownContributionKind::FootnoteReference { .. }
        | MarkdownContributionKind::FootnoteDefinition { .. }
        | MarkdownContributionKind::Mermaid { .. } => true,
        MarkdownContributionKind::Image { .. }
        | MarkdownContributionKind::InlineMath { .. }
        | MarkdownContributionKind::DisplayMath { .. } => false,
    }
}

/// One actionable Markdown contribution of an item (FO7, B12).
///
/// Its typed payload and the cells it occupies are read from the same retained projection that
/// paints it and registers its hit regions. Keyboard focus, activation and the focus highlight
/// use these targets only.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MarkdownTarget {
    /// Owner-qualified contribution id (`transcript:<unit>:...`).
    pub contribution_id: String,
    /// Typed payload used for activation.
    pub kind: MarkdownContributionKind,
    /// One rectangle per visual row segment, in item-local cells, clipped to the measured
    /// layout; never empty.
    pub rects: Vec<LogicalRect>,
}

impl MarkdownTarget {
    /// Item-local rows the target spans (first rectangle's row to the last one's end).
    #[must_use]
    pub fn rows(&self) -> std::ops::Range<u64> {
        let start = self.rects.iter().map(|rect| rect.y).min().unwrap_or(0);
        let end = self
            .rects
            .iter()
            .map(|rect| rect.y + rect.height)
            .max()
            .unwrap_or(start);
        start..end.max(start)
    }

    /// Document-order sort key: first row, then first column.
    fn order(&self) -> (u64, u64) {
        self.rects
            .iter()
            .map(|rect| (rect.y, rect.x))
            .min()
            .unwrap_or_default()
    }
}

/// One internal fragment anchor (`#fragment` destination) of an item, at an item-local row.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MarkdownAnchor {
    /// GitHub-compatible fragment identifier without the leading `#`.
    pub fragment: String,
    /// Item-local row the anchored block starts on.
    pub row: u64,
}

/// Markdown activation geometry of one item: actionable contributions in document order and
/// fragment anchors, from the retained projection (L6).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct MarkdownGeometry {
    /// Actionable contributions, in document order (row, then column).
    pub targets: Vec<MarkdownTarget>,
    /// Fragment anchors in row order.
    pub anchors: Vec<MarkdownAnchor>,
}

impl MarkdownGeometry {
    /// The target of `contribution_id`, if the item has it.
    #[must_use]
    pub fn target(&self, contribution_id: &str) -> Option<&MarkdownTarget> {
        self.targets
            .iter()
            .find(|target| target.contribution_id == contribution_id)
    }

    /// The first anchor of `fragment`, if the item has it.
    #[must_use]
    pub fn anchor(&self, fragment: &str) -> Option<&MarkdownAnchor> {
        self.anchors
            .iter()
            .find(|anchor| anchor.fragment == fragment)
    }

    /// The geometry moved by `dx` columns and `dy` rows.
    #[must_use]
    pub fn translated(self, dx: u64, dy: u64) -> Self {
        Self {
            targets: self
                .targets
                .into_iter()
                .map(|target| MarkdownTarget {
                    rects: target
                        .rects
                        .into_iter()
                        .map(|rect| {
                            LogicalRect::new(rect.x + dx, rect.y + dy, rect.width, rect.height)
                        })
                        .collect(),
                    ..target
                })
                .collect(),
            anchors: self
                .anchors
                .into_iter()
                .map(|anchor| MarkdownAnchor {
                    row: anchor.row + dy,
                    ..anchor
                })
                .collect(),
        }
    }

    /// Append the geometry of a part laid out after this one (composite items).
    pub fn extend(&mut self, other: Self) {
        self.targets.extend(other.targets);
        self.anchors.extend(other.anchors);
    }
}

/// BMUX image key of an accepted visual placed in contribution `contribution_id`.
#[must_use]
pub fn visual_image_key(contribution_id: &str) -> ImageKey {
    ImageKey::new(format!("{CONTENT_KEY_PREFIX}{contribution_id}"))
}

/// One accepted image placed over a slot (projection-local cells).
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlacedImage {
    key: ImageKey,
    rect: LogicalRect,
    payload: Arc<ImagePayload>,
}

/// One Markdown projection request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownRequest<'a> {
    /// Markdown source.
    pub source: &'a str,
    /// Content width in cells the projection wraps to.
    pub width: u64,
    /// Stable owning document identity (`transcript:<unit>`). Scopes `<details>` state and
    /// contribution identities.
    pub document_id: Option<String>,
    /// Whether the source is an incomplete streaming document (C38).
    pub streaming: bool,
}

impl<'a> MarkdownRequest<'a> {
    /// A complete document of `source` at `width`, without document identity.
    #[must_use]
    pub const fn new(source: &'a str, width: u64) -> Self {
        Self {
            source,
            width,
            document_id: None,
            streaming: false,
        }
    }

    /// Return the request scoped to `document_id`.
    #[must_use]
    pub fn document(mut self, document_id: impl Into<String>) -> Self {
        self.document_id = Some(document_id.into());
        self
    }

    /// Return the request marked as streaming or complete.
    #[must_use]
    pub const fn streaming(mut self, streaming: bool) -> Self {
        self.streaming = streaming;
        self
    }
}

/// Theme style of `name`, or `fallback`.
fn role(theme: &ResolvedThemeDefinition, name: &str, fallback: Style) -> Style {
    theme.style(name).unwrap_or(fallback)
}

/// Primary text style of a theme (`text.primary`, else the `text` color).
#[must_use]
pub fn text_style(theme: &ResolvedThemeDefinition) -> Style {
    theme.style("text.primary").unwrap_or_else(|| {
        theme
            .color("text")
            .map_or_else(Style::new, |color| Style::new().fg(color))
    })
}

/// Muted text style of a theme (`text.muted`, else the `muted` color, else dim).
#[must_use]
pub fn muted_style(theme: &ResolvedThemeDefinition) -> Style {
    theme.style("text.muted").unwrap_or_else(|| {
        theme.color("muted").map_or_else(
            || Style::new().add_modifier(Modifier::DIM),
            |color| Style::new().fg(color),
        )
    })
}

/// Markdown styles from theme roles (`markdown.*`), with BMUX defaults.
#[must_use]
pub fn markdown_theme(theme: &ResolvedThemeDefinition) -> MarkdownTheme {
    let defaults = MarkdownTheme::default();
    let text = text_style(theme);
    let muted = muted_style(theme);
    MarkdownTheme {
        text: role(theme, "markdown.text", text),
        heading: role(theme, "markdown.heading", defaults.heading),
        link: role(theme, "markdown.link", defaults.link),
        strong: role(theme, "markdown.strong", defaults.strong),
        emphasis: role(theme, "markdown.emphasis", defaults.emphasis),
        strikethrough: role(theme, "markdown.strikethrough", defaults.strikethrough),
        inline_code: role(theme, "markdown.inline_code", defaults.inline_code),
        code_block_text: role(theme, "markdown.code_block_text", defaults.code_block_text),
        code_block_border: role(theme, "markdown.code_block_border", muted),
        blockquote_bar: role(theme, "markdown.blockquote_bar", muted),
        alert_note: role(theme, "markdown.alert_note", defaults.alert_note),
        alert_tip: role(theme, "markdown.alert_tip", defaults.alert_tip),
        alert_important: role(theme, "markdown.alert_important", defaults.alert_important),
        alert_warning: role(theme, "markdown.alert_warning", defaults.alert_warning),
        alert_caution: role(theme, "markdown.alert_caution", defaults.alert_caution),
        list_marker: role(theme, "markdown.list_marker", muted),
        task_checked: role(theme, "markdown.task_checked", muted),
        task_unchecked: role(theme, "markdown.task_unchecked", muted),
        table_border: role(theme, "markdown.table_border", muted),
        horizontal_rule: role(theme, "markdown.horizontal_rule", muted),
    }
}

/// Syntax palette for fenced code from theme roles (`syntax.*`), falling back to the text,
/// muted and state roles.
#[must_use]
pub fn syntax_palette(theme: &ResolvedThemeDefinition) -> SyntaxPalette {
    let fg = |style: Style, fallback: SyntaxColor| style.fg.map_or(fallback, SyntaxColor::from_tui);
    let text = fg(text_style(theme), SyntaxColor::Default);
    let muted = fg(muted_style(theme), text);
    let state = |name: &str| theme.style(name).map_or(text, |style| fg(style, text));
    let info = state("state.info");
    let success = state("state.success");
    let warning = state("state.warning");
    let color = |name: &str, fallback: SyntaxColor| {
        theme
            .style(name)
            .and_then(|style| style.fg)
            .map_or(fallback, SyntaxColor::from_tui)
    };
    SyntaxPalette {
        text: color("syntax.text", text),
        comment: color("syntax.comment", muted),
        keyword: color("syntax.keyword", info),
        function: color("syntax.function", info),
        variable: color("syntax.variable", text),
        string: color("syntax.string", success),
        number: color("syntax.number", warning),
        type_name: color("syntax.type", info),
        operator: color("syntax.operator", text),
        punctuation: color("syntax.punctuation", text),
        heading: color("syntax.heading", info),
        link: color("syntax.link", info),
        raw: color("syntax.raw", success),
    }
}

/// BMUX render options of `request` under `cx`.
///
/// The options carry the theme, syntax palette, streaming flag, document-scoped `<details>`
/// state, readable link fallbacks, placeholder reservations and the base directory relative
/// local links resolve against. The width is clamped to at
/// least one cell; a zero-width body clips every row.
#[must_use]
pub fn render_options(
    request: &MarkdownRequest<'_>,
    cx: &ItemRenderContext<'_>,
) -> MarkdownRenderOptions {
    let width = u16::try_from(request.width.max(1)).unwrap_or(u16::MAX);
    let image_rows = if cx.markdown.images {
        IMAGE_RESERVED_ROWS
    } else {
        1
    };
    let mut options = MarkdownRenderOptions::new(width)
        .with_theme(markdown_theme(cx.theme))
        .with_syntax_palette(syntax_palette(cx.theme))
        .with_streaming(request.streaming)
        .with_link_destination_fallbacks(true)
        .with_rich_reserved_rows(image_rows, MERMAID_RESERVED_ROWS);
    if cx.markdown.mermaid {
        options = options.with_mermaid_contributions(MERMAID_PIXELS.0, MERMAID_PIXELS.1);
    }
    if let Some(base_directory) = &cx.markdown.base_directory {
        options = options.with_document_context(MarkdownDocumentContext {
            base_directory: Some(base_directory.clone()),
            ..MarkdownDocumentContext::default()
        });
    }
    if let Some(document_id) = &request.document_id {
        let details: BTreeMap<String, bool> = cx
            .markdown
            .details_for(document_id)
            .map(|(id, open)| (id.clone(), *open))
            .collect();
        options = options
            .with_document_id(document_id.clone())
            .with_details_open(details);
    }
    options
}

/// A contribution region of the projection, in projection rows and cells.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ContributionRegion {
    id: String,
    rect: LogicalRect,
    interactive: bool,
    rendered: bool,
}

/// One accepted Markdown projection as a retained item component.
///
/// BMUX lays the projected rows out unwrapped and clips them at the assigned width, so the
/// container keeps the projection's row count (B10). Geometry maps the measured rows of the same
/// layout node to the projected lines' source bytes.
#[derive(Debug, Clone)]
pub struct MarkdownBody {
    block: TextBlock,
    /// First source byte shown by each projected line, when known.
    line_sources: Vec<Option<u64>>,
    contributions: Vec<ContributionRegion>,
    /// Reserved asynchronous visuals (projection-local cells).
    slots: Vec<VisualSlot>,
    /// Accepted images placed over their slots while painting.
    images: Vec<PlacedImage>,
    /// Source-backed selectable text (C34), attached with the source the projection was
    /// rendered from ([`Self::attach_selectable_source`]).
    selectable: Option<SelectableText>,
    /// Typed payloads of the actionable contributions, by contribution id.
    actionable: BTreeMap<String, MarkdownContributionKind>,
    /// Fragment anchors (projection rows).
    anchors: Vec<MarkdownAnchor>,
}

impl MarkdownBody {
    /// Wrap a BMUX projection laid out at `width` cells. With `mapped`, per-row source
    /// anchors come from the projection's selection provenance; with `mermaid_placeholders`,
    /// Mermaid contributions were requested and occupy reserved placeholder rows. Visuals in
    /// `visuals` accepted for the exact contribution and source of a reserved slot are applied
    /// (module documentation).
    #[must_use]
    pub fn from_projection(
        result: &MarkdownRenderResult,
        width: u64,
        mapped: bool,
        mermaid_placeholders: bool,
        visuals: &MarkdownVisualState,
    ) -> Self {
        let mut body = Self::project(result, mapped, mermaid_placeholders);
        body.apply_visuals(result, width, mermaid_placeholders, visuals);
        body
    }

    fn project(result: &MarkdownRenderResult, mapped: bool, mermaid_placeholders: bool) -> Self {
        let mut line_sources: Vec<Option<u64>> = vec![None; result.lines.len()];
        if mapped {
            for unit in result.selection_provenance() {
                let Some(start) = unit.source_ranges.iter().map(|range| range.start).min() else {
                    continue;
                };
                let start = start as u64;
                for rect in &unit.rects {
                    if let Some(slot) = line_sources.get_mut(usize::from(rect.y)) {
                        *slot = Some(slot.map_or(start, |current| current.min(start)));
                    }
                }
            }
        }
        let kinds: BTreeMap<&str, &MarkdownContributionKind> = result
            .contributions
            .iter()
            .map(|contribution| (contribution.id.as_str(), &contribution.kind))
            .collect();
        let mut contributions = Vec::new();
        let mut actionable = BTreeMap::new();
        for geometry in &result.geometry {
            let Some(kind) = kinds.get(geometry.contribution_id.as_str()) else {
                continue;
            };
            let interactive = contribution_actionable(kind);
            if interactive {
                actionable.insert(geometry.contribution_id.clone(), (*kind).clone());
            }
            let rendered = match kind {
                // Without placeholder reservations (or after a failure), Mermaid keeps its
                // highlighted, source-backed code block.
                MarkdownContributionKind::Mermaid { rendering, .. }
                    if !mermaid_placeholders
                        || matches!(rendering, MermaidContributionRendering::Failed(_)) =>
                {
                    false
                }
                MarkdownContributionKind::Image { .. }
                | MarkdownContributionKind::Mermaid { .. } => true,
                _ => false,
            };
            contributions.extend(geometry.rects.iter().map(|rect| ContributionRegion {
                id: geometry.contribution_id.clone(),
                rect: LogicalRect::new(
                    u64::from(rect.x),
                    u64::from(rect.y),
                    u64::from(rect.width),
                    u64::from(rect.height),
                ),
                interactive,
                rendered,
            }));
        }
        Self {
            block: TextBlock::new(Text::from_lines(result.lines.clone())).wrap(TextWrap::None),
            line_sources,
            contributions,
            slots: Vec::new(),
            images: Vec::new(),
            selectable: None,
            actionable,
            anchors: result
                .anchors
                .iter()
                .map(|anchor| MarkdownAnchor {
                    fragment: anchor.fragment.clone(),
                    row: u64::from(anchor.row),
                })
                .collect(),
        }
    }

    /// Attach the selectable source text of a mapped projection (C34): runs come from the
    /// projection's selection provenance (already derived for the source map), merged per row
    /// where cells and source bytes are contiguous. A merged run is kept as plain text only
    /// when BMUX's plain-text fragments of its source reproduce the provenance cells exactly;
    /// otherwise each unit stays its own indivisible run. Rows replaced by accepted visuals
    /// are not selectable. Without provenance (streaming documents) nothing is attached.
    pub fn attach_selectable_source(&mut self, result: &MarkdownRenderResult, source: &str) {
        if !result.has_selection_source(source) {
            return;
        }
        let mut units: Vec<(u64, u64, u64, std::ops::Range<usize>)> = result
            .selection_provenance()
            .iter()
            .filter(|unit| {
                matches!(
                    unit.kind,
                    MarkdownSelectionKind::Text | MarkdownSelectionKind::Code
                )
            })
            .filter_map(
                |unit| match (unit.rects.as_slice(), unit.source_ranges.as_slice()) {
                    ([rect], [range]) if rect.height == 1 && rect.width > 0 => Some((
                        u64::from(rect.y),
                        u64::from(rect.x),
                        u64::from(rect.width),
                        range.clone(),
                    )),
                    _ => None,
                },
            )
            .filter(|(row, ..)| {
                usize::try_from(*row)
                    .ok()
                    .and_then(|row| self.line_sources.get(row))
                    .is_some_and(Option::is_some)
            })
            .filter(|(.., range)| source.get(range.clone()).is_some())
            .collect();
        units.sort_by_key(|(row, x, ..)| (*row, *x));
        let mut runs = Vec::new();
        let mut start = 0;
        while start < units.len() {
            let mut end = start + 1;
            while end < units.len() {
                let (row, x, width, range) = &units[end - 1];
                let (next_row, next_x, _, next_range) = &units[end];
                if next_row != row || *next_x != x + width || next_range.start != range.end {
                    break;
                }
                end += 1;
            }
            push_runs(&mut runs, &units[start..end], source);
            start = end;
        }
        if !runs.is_empty() {
            self.selectable = Some(SelectableText::new(Arc::from(source), runs));
        }
    }

    /// Derive the reserved visual slots of the projection and apply accepted visuals to their
    /// rows. Rows keep their count; replaced rows lose their source anchors.
    fn apply_visuals(
        &mut self,
        result: &MarkdownRenderResult,
        width: u64,
        mermaid_placeholders: bool,
        visuals: &MarkdownVisualState,
    ) {
        let mut lines: Option<Vec<Line>> = None;
        for contribution in &result.contributions {
            let request = match &contribution.kind {
                MarkdownContributionKind::Image {
                    alt,
                    source,
                    linked_destination,
                    ..
                } => VisualSlotRequest::Image {
                    alt: alt.clone(),
                    source: source.clone(),
                    linked_destination: linked_destination.clone(),
                },
                MarkdownContributionKind::Mermaid {
                    source,
                    cache_key,
                    rendering,
                } if mermaid_placeholders
                    && !matches!(rendering, MermaidContributionRendering::Failed(_)) =>
                {
                    VisualSlotRequest::Mermaid {
                        source: source.clone(),
                        cache_key: cache_key.clone(),
                    }
                }
                _ => continue,
            };
            let Some(rect) = self.slot_rect(&contribution.id, width) else {
                continue;
            };
            let slot = VisualSlot {
                contribution_id: contribution.id.clone(),
                rect,
                request,
            };
            if let Some(accepted) = visuals.for_slot(&slot) {
                let lines = lines.get_or_insert_with(|| result.lines.clone());
                apply_visual(lines, &mut self.line_sources, &slot, accepted);
                if let MarkdownVisual::Image(payload) = &accepted.visual {
                    self.images.push(PlacedImage {
                        key: visual_image_key(&slot.contribution_id),
                        rect: slot.rect,
                        payload: Arc::clone(payload),
                    });
                }
            }
            self.slots.push(slot);
        }
        if let Some(lines) = lines {
            self.block = TextBlock::new(Text::from_lines(lines)).wrap(TextWrap::None);
        }
    }

    /// The reserved rectangle of contribution `id`: its first column to the right edge at
    /// `width`, over its row span. `None` without geometry or columns.
    fn slot_rect(&self, id: &str, width: u64) -> Option<LogicalRect> {
        let rects = self
            .contributions
            .iter()
            .filter(|region| region.id == id && region.rect.height > 0)
            .map(|region| region.rect);
        let (x, top, bottom) = rects.fold(None, |span: Option<(u64, u64, u64)>, rect| {
            let bottom = rect.y + rect.height;
            Some(span.map_or((rect.x, rect.y, bottom), |(x, top, end)| {
                (x.min(rect.x), top.min(rect.y), end.max(bottom))
            }))
        })?;
        (x < width).then(|| LogicalRect::new(x, top, width - x, bottom - top))
    }

    /// Number of projected rows.
    #[must_use]
    pub const fn rows(&self) -> usize {
        self.line_sources.len()
    }

    /// Reserved asynchronous visuals of the projection (projection-local cells).
    #[must_use]
    pub fn visual_slots(&self) -> &[VisualSlot] {
        &self.slots
    }
}

/// Replace the rows of `slot` by `accepted`: each row keeps its cells left of the slot, then
/// shows the notice line of that row (none for images). Replaced rows lose their anchors.
fn apply_visual(
    lines: &mut [Line],
    line_sources: &mut [Option<u64>],
    slot: &VisualSlot,
    accepted: &AcceptedMarkdownVisual,
) {
    let prefix = usize::try_from(slot.rect.x).unwrap_or(usize::MAX);
    let mut notice = match &accepted.visual {
        MarkdownVisual::Image(_) => None,
        MarkdownVisual::Notice(text) => Some(text.lines()),
    };
    let style = lines
        .get(usize::try_from(slot.rect.y).unwrap_or(usize::MAX))
        .and_then(|line| line.spans.first())
        .map(|span| span.style)
        .unwrap_or_default();
    for row in slot.rect.y..slot.rect.y + slot.rect.height {
        let Ok(row) = usize::try_from(row) else {
            break;
        };
        let Some(line) = lines.get_mut(row) else {
            break;
        };
        let mut replaced = line.truncate(prefix);
        if let Some(text) = notice.as_mut().and_then(Iterator::next) {
            replaced.spans.push(Span::styled(text.to_owned(), style));
        }
        *line = replaced;
        if let Some(source) = line_sources.get_mut(row) {
            *source = None;
        }
    }
}

impl Component for MarkdownBody {
    fn layout(&self, constraints: Constraints, cx: &mut LayoutCx) -> LayoutNode {
        self.block.layout(constraints, cx)
    }

    fn paint(&self, layout: &LayoutNode, cx: &mut PaintCx<'_, '_>) {
        self.block.paint(layout, cx);
        // Placements come from this retained node only while it is painted (B3, F4): the
        // painting context translates and clips them, so a clipped or scrolled-out body
        // registers nothing stale.
        for image in &self.images {
            let Some(rect) = clipped(image.rect, layout.size.width, layout.size.height) else {
                continue;
            };
            cx.push_image(ImageContribution::Present(ImagePlacement {
                key: image.key.clone(),
                payload: ImagePayload::clone(&image.payload),
                destination: rect,
                fit: ImageFit::Contain,
                allocation: None,
                clip: rect,
                lifecycle: ImageLifecycle::Frame,
            }));
        }
    }

    fn revision(&self) -> ComponentRevision {
        self.block.revision()
    }
}

/// `rect` clipped to a `width` by `height` node as a local cell rectangle; `None` when nothing
/// of it lies inside or it is not addressable in cells.
fn clipped(rect: LogicalRect, width: u64, height: u64) -> Option<Rect> {
    if rect.x >= width || rect.y >= height || rect.width == 0 || rect.height == 0 {
        return None;
    }
    let cells = |value: u64| u16::try_from(value).ok();
    Some(Rect::new(
        cells(rect.x)?,
        cells(rect.y)?,
        cells(rect.width.min(width - rect.x))?,
        cells(rect.height.min(height - rect.y))?,
    ))
}

impl ItemContent for MarkdownBody {
    fn geometry(&self, layout: &LayoutNode) -> ItemGeometry {
        // Rows are unwrapped, so measured rows equal projected lines: the source map and
        // contribution rows need no projection or remapping (nothing is materialized again).
        // Regions outside the measured rows or columns are clipped or dropped (B7).
        let height = layout.size.height;
        let rows: Vec<SourceRow> = (0..height)
            .zip(&self.line_sources)
            .filter_map(|(row, byte)| {
                Some(SourceRow {
                    row,
                    position: SourcePosition::MarkdownByte((*byte)?),
                })
            })
            .collect();
        let width = layout.size.width;
        let mut geometry = ItemGeometry {
            selection_fragments: vec![ItemSelectionFragment {
                rows: 0..height,
                source: rows.first().map(|row| row.position.clone()),
                rendered: false,
            }],
            source_map: SourceRows::new(rows),
            ..ItemGeometry::default()
        };
        // Content keys and rendered fragments cover each contribution's row span; hit
        // regions keep one rectangle per visual row segment.
        let mut spans: Vec<(&ContributionRegion, std::ops::Range<u64>)> = Vec::new();
        for region in &self.contributions {
            let rect = region.rect;
            if rect.y >= height || rect.x >= width || rect.width == 0 || rect.height == 0 {
                continue;
            }
            let rect = LogicalRect::new(
                rect.x,
                rect.y,
                rect.width.min(width - rect.x),
                rect.height.min(height - rect.y),
            );
            let rows = rect.y..rect.y + rect.height;
            match spans.last_mut() {
                Some((last, span)) if last.id == region.id => {
                    span.start = span.start.min(rows.start);
                    span.end = span.end.max(rows.end);
                }
                _ => spans.push((region, rows)),
            }
            if region.interactive {
                geometry.hit_regions.push(ItemHitRegion {
                    rect,
                    action: ItemAction::Activate(format!("{CONTENT_KEY_PREFIX}{}", region.id)),
                });
                // The activation target keeps exactly the hit rectangles (B3, B12).
                if let Some(kind) = self.actionable.get(&region.id) {
                    match geometry.markdown.targets.last_mut() {
                        Some(target) if target.contribution_id == region.id => {
                            target.rects.push(rect);
                        }
                        _ => geometry.markdown.targets.push(MarkdownTarget {
                            contribution_id: region.id.clone(),
                            kind: kind.clone(),
                            rects: vec![rect],
                        }),
                    }
                }
            }
        }
        geometry.markdown.targets.sort_by_key(MarkdownTarget::order);
        geometry.markdown.anchors = self
            .anchors
            .iter()
            .filter(|anchor| anchor.row < height)
            .cloned()
            .collect();
        geometry.visual_slots = self
            .slots
            .iter()
            .filter_map(|slot| {
                let rect = slot.rect;
                (rect.y < height && rect.x < width).then(|| VisualSlot {
                    rect: LogicalRect::new(
                        rect.x,
                        rect.y,
                        rect.width.min(width - rect.x),
                        rect.height.min(height - rect.y),
                    ),
                    ..slot.clone()
                })
            })
            .collect();
        geometry.selectable = self.selectable.iter().cloned().collect();
        for (region, rows) in spans {
            geometry.content_keys.push(ContentKeyRegion {
                key: format!("{CONTENT_KEY_PREFIX}{}", region.id),
                rows: rows.clone(),
            });
            if region.rendered {
                geometry.selection_fragments.push(ItemSelectionFragment {
                    rows,
                    source: None,
                    rendered: true,
                });
            }
        }
        geometry
    }
}

/// Project `request` under `cx`: one parse and one projection; fenced code blocks count as
/// highlights.
#[must_use]
pub fn markdown_projection(
    request: &MarkdownRequest<'_>,
    cx: &ItemRenderContext<'_>,
) -> (MarkdownBody, ProducerWork) {
    let options = render_options(request, cx);
    let result = render_markdown(request.source, &options);
    let work = ProducerWork {
        markdown_parses: 1,
        highlights: result.code_block_geometry.len() as u64,
        projections: 1,
    };
    let mut body = MarkdownBody::from_projection(
        &result,
        request.width.max(1),
        !request.streaming,
        cx.markdown.mermaid,
        &cx.markdown.visuals,
    );
    if !request.streaming {
        body.attach_selectable_source(&result, request.source);
    }
    (body, work)
}

/// Runs of one row-contiguous group of provenance units: one plain-text run when BMUX's
/// plain-text fragments of the group's source reproduce the units' cells and bytes exactly,
/// else one indivisible run per unit (C34, B3: the cells come from the projection, never from
/// a separate measurement).
fn push_runs(
    runs: &mut Vec<SelectableRun>,
    group: &[(u64, u64, u64, std::ops::Range<usize>)],
    source: &str,
) {
    let (Some(first), Some(last)) = (group.first(), group.last()) else {
        return;
    };
    let (row, x, ..) = first;
    let range = first.3.start..last.3.end;
    let width = group.iter().map(|unit| unit.2).sum::<u64>();
    let cells = |value: u64| u16::try_from(value).ok();
    let exact = source.get(range.clone()).is_some_and(|text| {
        let (Some(left), Some(cells_wide)) = (cells(*x), cells(width)) else {
            return false;
        };
        // The same painted-cell fragments the frame registers for a Text run (B3).
        let fragments = bmux_tui::selection::painted_text_fragments(
            "",
            "",
            Rect::new(left, 0, cells_wide, 1),
            0,
            text,
            range.start,
            0,
        );
        fragments.len() == group.len()
            && fragments.iter().zip(group).all(|(fragment, unit)| {
                u64::from(fragment.area.x) == unit.1
                    && u64::from(fragment.area.width) == unit.2
                    && fragment.source_range == unit.3
            })
    });
    if exact {
        runs.push(SelectableRun {
            row: *row,
            x: *x,
            width,
            source: range,
            kind: SelectableRunKind::Text,
        });
    } else {
        runs.extend(group.iter().map(|(row, x, width, range)| SelectableRun {
            row: *row,
            x: *x,
            width: *width,
            source: range.clone(),
            kind: SelectableRunKind::Unit,
        }));
    }
}

/// Markdown body of `request` under `cx`.
#[must_use]
pub fn markdown_body_with(
    request: &MarkdownRequest<'_>,
    cx: &ItemRenderContext<'_>,
) -> ProducedItem {
    let (body, work) = markdown_projection(request, cx);
    ProducedItem::new(body).with_work(work)
}

/// Markdown body of a complete `source` at `cx.width`.
#[must_use]
pub fn markdown_body(source: &str, cx: &ItemRenderContext<'_>) -> ProducedItem {
    markdown_body_with(&MarkdownRequest::new(source, cx.width), cx)
}

#[cfg(test)]
mod tests;
