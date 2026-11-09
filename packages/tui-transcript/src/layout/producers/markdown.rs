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
//! reserved rectangles are exposed as `markdown:` content keys and rendered selection
//! fragments. Host-side workers paint accepted images over those rows, which never changes
//! layout. Mermaid contributions are requested only when [`MarkdownOptions::mermaid`] is set.
//!
//! [`MarkdownOptions::mermaid`]: crate::layout::context::MarkdownOptions::mermaid

use std::collections::BTreeMap;

use bcode_markdown_render::{
    MarkdownContributionKind, MarkdownRenderOptions, MarkdownRenderResult, MarkdownTheme,
    MermaidContributionRendering, render_markdown,
};
use bcode_syntax_render::{SyntaxColor, SyntaxPalette};
use bcode_tui_theme::definition::ResolvedThemeDefinition;
use bmux_tui::component::{
    Component, ComponentRevision, Constraints, LayoutCx, LayoutNode, LogicalRect,
};
use bmux_tui::composition::TextBlock;
use bmux_tui::paint::PaintCx;
use bmux_tui::style::{Modifier, Style};
use bmux_tui::text::{Text, TextWrap};

use super::{ItemContent, ItemRenderContext, ProducedItem, ProducerWork};
use crate::layout::SourcePosition;
use crate::layout::geometry::{
    ContentKeyRegion, ItemAction, ItemGeometry, ItemHitRegion, ItemSelectionFragment, SourceRow,
    SourceRows,
};

/// Rows reserved for one inline image placeholder (L8).
pub const IMAGE_RESERVED_ROWS: u16 = 4;

/// Rows reserved for one Mermaid diagram placeholder (L8).
pub const MERMAID_RESERVED_ROWS: u16 = 8;

/// Maximum Mermaid SVG size requested from host-side workers, in pixels.
pub const MERMAID_PIXELS: (u32, u32) = (1600, 1200);

/// Content-key namespace of Markdown contributions (E3).
pub const CONTENT_KEY_PREFIX: &str = "markdown:";

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
/// state, readable link fallbacks and placeholder reservations. The width is clamped to at
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
}

impl MarkdownBody {
    /// Wrap a BMUX projection. With `mapped`, per-row source anchors come from the
    /// projection's selection provenance; with `mermaid_placeholders`, Mermaid contributions
    /// were requested and occupy reserved placeholder rows.
    #[must_use]
    pub fn from_projection(
        result: &MarkdownRenderResult,
        mapped: bool,
        mermaid_placeholders: bool,
    ) -> Self {
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
        for geometry in &result.geometry {
            let Some(kind) = kinds.get(geometry.contribution_id.as_str()) else {
                continue;
            };
            let (interactive, rendered) = match kind {
                MarkdownContributionKind::Link { .. }
                | MarkdownContributionKind::Details { .. }
                | MarkdownContributionKind::GitHubIssue { .. }
                | MarkdownContributionKind::FootnoteReference { .. } => (true, false),
                // Without placeholder reservations (or after a failure), Mermaid keeps its
                // highlighted, source-backed code block.
                MarkdownContributionKind::Mermaid { rendering, .. }
                    if !mermaid_placeholders
                        || matches!(rendering, MermaidContributionRendering::Failed(_)) =>
                {
                    (false, false)
                }
                MarkdownContributionKind::Image { .. }
                | MarkdownContributionKind::Mermaid { .. } => (false, true),
                _ => (false, false),
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
        }
    }

    /// Number of projected rows.
    #[must_use]
    pub const fn rows(&self) -> usize {
        self.line_sources.len()
    }
}

impl Component for MarkdownBody {
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
            }
        }
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
    (
        MarkdownBody::from_projection(&result, !request.streaming, cx.markdown.mermaid),
        work,
    )
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
