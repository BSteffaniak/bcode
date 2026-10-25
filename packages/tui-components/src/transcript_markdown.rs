//! Accepted Markdown presentation at the explicit BMUX TUI boundary.
//!
//! Hosts prepare projections on source/options changes, never during paint. This
//! module owns no canonical session state or platform services. The same native
//! block recipe and source-selection adapter are usable by browser-local BMUX.

use bcode_markdown_render::{
    MarkdownContributionKind, MarkdownRenderOptions, MarkdownRenderResult, render_markdown,
};
use bmux_tui::prelude::{Line, Modifier, Span, Style};

mod body;
pub use body::MarkdownBody;

#[cfg(test)]
mod tests;

/// Complete retained Markdown presentation, with canonical source correspondence.
///
/// A replacement contains lines, contributions, geometry, anchors and provenance
/// together; consumers must not merge partial paint responses into this value.
#[derive(Debug, Clone)]
pub struct TranscriptMarkdown {
    content_id: String,
    revision: u64,
    source: String,
    projection: MarkdownRenderResult,
}

impl TranscriptMarkdown {
    /// Prepare using the same BMUX Markdown implementation as the native TUI.
    /// `options.document_id` must be scoped by the caller's semantic item identity.
    #[must_use]
    pub fn prepare(
        content_id: String,
        revision: u64,
        source: String,
        options: &MarkdownRenderOptions,
    ) -> Self {
        let projection = render_markdown(&source, options);
        Self {
            content_id,
            revision,
            source,
            projection,
        }
    }

    /// Stable caller-owned semantic content identity.
    #[must_use]
    pub fn content_id(&self) -> &str {
        &self.content_id
    }

    /// Accepted semantic revision, not a generated paint counter.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Canonical source retained for copy/selection, including Markdown syntax.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Complete projection including contributions, anchors and selection provenance.
    #[must_use]
    pub const fn projection(&self) -> &MarkdownRenderResult {
        &self.projection
    }
}

/// Native transcript Markdown block policy, separate from BMUX text mechanics.
#[derive(Debug, Clone, Copy)]
pub struct MarkdownBlockStyle {
    /// Heading style before optional emphasis.
    pub heading: Style,
    /// Continuation/indent and empty marker style.
    pub muted: Style,
    /// Emphasize this message heading and empty marker.
    pub prominent: bool,
    /// Existing native semantic body indentation, in cells.
    pub body_indent: u16,
}

/// Append the native Markdown message recipe using an already accepted projection.
///
/// No Markdown parsing or plugin calls occur here. Projection source coordinates
/// remain relative to its body, at `body_start..body_start + projection.lines.len()`.
/// Returns the first body row so hosts retain the exact mapping with these rows.
pub fn append_markdown_block(
    rows: &mut Vec<Line>,
    title: &str,
    projection: &MarkdownRenderResult,
    width: u16,
    style: MarkdownBlockStyle,
) -> usize {
    let heading = if style.prominent {
        style.heading.add_modifier(Modifier::BOLD)
    } else {
        style.heading
    };
    append_styled_text(rows, Vec::new(), title, width, heading, heading);
    let body_start = rows.len();
    if projection.lines.is_empty() {
        rows.push(Line::from_spans(vec![
            Span::styled(" ".repeat(usize::from(style.body_indent)), style.muted),
            Span::styled(
                "·",
                if style.prominent {
                    Style::new()
                } else {
                    style.muted
                },
            ),
        ]));
    } else {
        for line in &projection.lines {
            let mut spans = vec![Span::styled(
                " ".repeat(usize::from(style.body_indent)),
                style.muted,
            )];
            spans.extend(line.spans.iter().cloned());
            rows.push(Line::from_spans(spans));
        }
    }
    rows.push(Line::default());
    body_start
}

/// Native transcript heading/prose recipe. BMUX owns all wrapping and cell widths.
/// Extracted rather than independently reimplemented by the browser host.
pub fn append_styled_text(
    rows: &mut Vec<Line>,
    prefix: Vec<Span>,
    text: &str,
    width: u16,
    body_style: Style,
    continuation_style: Style,
) {
    let max_width = usize::from(width.max(1));
    let prefix_width: usize = prefix
        .iter()
        .map(|span| bmux_tui::text_width::display_width(&span.content))
        .sum();
    if prefix_width >= max_width || max_width <= 2 {
        let mut spans = prefix;
        spans.push(Span::styled(text, body_style));
        rows.extend(bmux_tui::text::wrap_line_bounded(
            &Line::from_spans(spans),
            bmux_tui::text::TextWrapGeometry::uniform(max_width),
            bmux_tui::text::TextWrap::Word,
        ));
        return;
    }
    let chunks = bmux_tui::text::wrap_line_bounded(
        &Line::raw(text),
        bmux_tui::text::TextWrapGeometry::with_continuation(
            max_width.saturating_sub(prefix_width).max(1),
            max_width.saturating_sub(2).max(1),
        ),
        bmux_tui::text::TextWrap::Word,
    );
    for (index, chunk) in chunks.iter().enumerate() {
        if index == 0 {
            let mut spans = prefix.clone();
            spans.push(Span::styled(chunk.plain_text(), body_style));
            rows.push(Line::from_spans(spans));
        } else {
            rows.push(Line::from_spans(vec![
                Span::styled("  ", continuation_style),
                Span::styled(chunk.plain_text(), body_style),
            ]));
        }
    }
    if chunks.is_empty() {
        rows.push(Line::from_spans(prefix));
    }
}

/// Whether the native TUI admits activation for this Markdown contribution.
#[must_use]
pub const fn contribution_actionable(kind: &MarkdownContributionKind) -> bool {
    match kind {
        MarkdownContributionKind::Link { destination, .. }
        | MarkdownContributionKind::GitHubIssue { destination, .. } => matches!(
            destination,
            bcode_markdown_render::MarkdownDestination::Web(_)
                | bcode_markdown_render::MarkdownDestination::LocalPath(_)
                | bcode_markdown_render::MarkdownDestination::Fragment(_)
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
