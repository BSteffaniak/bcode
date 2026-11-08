//! Structural frame for non-tool units (messages, reasoning, permissions,
//! interactions, notes).
//!
//! The frame applies the unit's resolved disclosure, header fields, summary,
//! spacing, and indent around today's body rendering:
//!
//! * `expanded`: the heading and body exactly as before;
//! * `summary`: the heading plus one summary line;
//! * `collapsed`: the heading only;
//! * `hidden`: a one-row count marker (`▸ 1 message hidden`), so a hidden
//!   unit always leaves a visible, clickable trace (visibility floor). The
//!   only exception is reasoning hidden by the legacy reasoning visibility
//!   setting, which renders nothing (see `StructureSnapshot::suppressed`).
//!
//! The default presentation (expanded, no header fields, spacing
//! `{ before = 0, after = 1 }`, no indent) reproduces the unframed rows
//! byte for byte. Structural units record a `disclosure:unit:<id>` anchor on
//! their heading row, so the hit region and focus row derive from the row
//! that is painted.

use bcode_presentation_policy::models::{
    Disclosure, ResolvedPresentation, Spacing, SummarySource, UnitFacts, UnitKind, UnitStatus,
};
use bmux_tui::prelude::{Line, Span};
use bmux_tui::text::truncate_line_to_display_width;

use super::super::structure::DisclosureTarget;
use super::super::transcript::{TranscriptItem, TranscriptItemKind};
use super::{
    STRUCTURE, default_unit_title, muted_style, push_unit_body, push_wrapped_styled_text,
    semantic_state_theme, unit_title_style,
};

/// Spacing the host default presentation resolves to.
const DEFAULT_SPACING: Spacing = Spacing {
    before: 0,
    after: 1,
};

/// Push `item` framed by its structural presentation.
pub(super) fn push_unit(
    rows: &mut Vec<Line>,
    anchors: &mut Vec<bcode_plugin_sdk::tui_visual::TuiVisualAnchor>,
    item: &TranscriptItem,
    width: u16,
    markdown: Option<&bcode_markdown_render::MarkdownRenderResult>,
) {
    let resolved = STRUCTURE.with(|state| {
        let state = state.borrow();
        let presentation = state.presentation(item)?;
        let facts = state.facts(item)?;
        let suppressed = state.suppressed(&facts, &presentation);
        Some((presentation, facts, suppressed))
    });
    let Some((presentation, facts, suppressed)) = resolved else {
        push_unit_body(rows, item, &default_unit_title(item), width, markdown);
        return;
    };
    if suppressed {
        return;
    }
    compose_unit(
        rows,
        anchors,
        item,
        &UnitContext {
            presentation: &presentation,
            facts: &facts,
            width,
        },
        markdown,
    );
}

/// Resolved inputs of one framed unit.
struct UnitContext<'a> {
    presentation: &'a ResolvedPresentation,
    facts: &'a UnitFacts,
    width: u16,
}

fn compose_unit(
    rows: &mut Vec<Line>,
    anchors: &mut Vec<bcode_plugin_sdk::tui_visual::TuiVisualAnchor>,
    item: &TranscriptItem,
    cx: &UnitContext<'_>,
    markdown: Option<&bcode_markdown_render::MarkdownRenderResult>,
) {
    let presentation = cx.presentation;
    let disclosure = presentation.disclosure;
    let spacing = presentation.spacing.clamped();
    // Metadata notes have no trailing blank row today; the default spacing
    // keeps each kind's natural trailing rows.
    let natural_after = u8::from(!matches!(item.kind(), TranscriptItemKind::Meta));
    let after = if spacing == DEFAULT_SPACING {
        natural_after
    } else {
        spacing.after
    };
    let indent = super::super::tool_anatomy::effective_indent(presentation, cx.width);
    let inner = cx.width.saturating_sub(indent);
    let anchor = bcode_plugin_sdk::tui_visual::TuiVisualAnchor {
        key: DisclosureTarget::Unit(item.id().get()).to_hit_id(),
        row: rows.len().saturating_add(usize::from(spacing.before)),
        source: None,
    };
    rows.extend((0..spacing.before).map(|_| Line::default()));
    let mut body = Vec::new();
    if disclosure == Disclosure::Hidden {
        push_hidden_marker(&mut body, cx.facts, inner);
        let pad = usize::from(indent);
        rows.extend(body.into_iter().map(|row| indent_row(row, pad, cx.width)));
        rows.extend((0..spacing.after).map(|_| Line::default()));
        anchors.push(anchor);
        return;
    }
    let title = header_text(item, cx, disclosure);
    match disclosure {
        Disclosure::Expanded | Disclosure::Hidden => {
            let (marker, text) = title;
            let heading = match marker {
                Some(marker) if text.is_empty() => marker,
                Some(marker) => format!("{marker} {text}"),
                None => text,
            };
            push_unit_body(&mut body, item, &heading, inner, markdown);
            // Every body but a metadata note ends with one blank row, which
            // the frame's trailing spacing replaces.
            if natural_after == 1 && body.last().is_some_and(|row| row.spans.is_empty()) {
                body.pop();
            }
        }
        Disclosure::Summary => {
            push_heading(&mut body, item, title, inner);
            if let Some(summary) = summary_text(item, cx) {
                body.push(truncate_line_to_display_width(
                    &Line::from_spans(vec![
                        Span::styled("  ", muted_style()),
                        Span::styled(summary, muted_style()),
                    ]),
                    usize::from(inner),
                ));
            }
        }
        Disclosure::Collapsed => push_heading(&mut body, item, title, inner),
    }
    if body.is_empty() {
        // Keep the anchor on a painted row.
        body.push(Line::default());
    }
    let pad = usize::from(indent);
    rows.extend(body.into_iter().map(|row| indent_row(row, pad, cx.width)));
    rows.extend((0..after).map(|_| Line::default()));
    anchors.push(anchor);
}

/// Indent a painted row by `pad` columns, bounded to `width` cells.
fn indent_row(mut row: Line, pad: usize, width: u16) -> Line {
    if pad == 0 || row.spans.is_empty() {
        row
    } else {
        row.spans.insert(0, Span::raw(" ".repeat(pad)));
        truncate_line_to_display_width(&row, usize::from(width))
    }
}

/// Push the one-row count marker of a hidden unit, bounded to `width`.
fn push_hidden_marker(rows: &mut Vec<Line>, facts: &UnitFacts, width: u16) {
    let meta = semantic_state_theme().transcript.tool_metadata;
    let noun = match facts.kind {
        Some(UnitKind::UserMessage | UnitKind::AssistantMessage) => "message",
        Some(UnitKind::Reasoning) => "reasoning block",
        Some(UnitKind::Permission | UnitKind::Interaction) => "request",
        _ => "note",
    };
    rows.push(truncate_line_to_display_width(
        &Line::from_spans(vec![Span::styled(
            format!("{} 1 {noun} hidden", glyph(Disclosure::Hidden)),
            meta,
        )]),
        usize::from(width),
    ));
}

/// Push the unit's heading row: the marker prefix and the header text.
fn push_heading(
    rows: &mut Vec<Line>,
    item: &TranscriptItem,
    (marker, text): (Option<String>, String),
    width: u16,
) {
    let style = unit_title_style(item);
    let meta = semantic_state_theme().transcript.tool_metadata;
    let prefix = marker
        .map(|marker| vec![Span::styled(format!("{marker} "), meta)])
        .unwrap_or_default();
    let before = rows.len();
    push_wrapped_styled_text(rows, prefix, &text, width, style, style);
    if rows.len() == before {
        rows.push(Line::default());
    }
}

/// Disclosure marker glyph from the theme's chrome icon set, matching tool units.
fn glyph(disclosure: Disclosure) -> String {
    semantic_state_theme()
        .unit_chrome
        .marker(disclosure == Disclosure::Expanded)
        .to_owned()
}

/// Leading marker and joined header text of a non-tool unit.
///
/// Without header fields, expanded units keep today's heading and other
/// disclosures add a leading marker. Registered fields render as: `marker`
/// the disclosure glyph, `label` the speaker or heading, `tool` and
/// `target` the unit's tool facts, `status` a non-success lifecycle; other
/// fields are empty for these kinds and skipped.
fn header_text(
    item: &TranscriptItem,
    cx: &UnitContext<'_>,
    disclosure: Disclosure,
) -> (Option<String>, String) {
    let Some(header) = cx.presentation.header.as_ref() else {
        let marker = (disclosure != Disclosure::Expanded).then(|| glyph(disclosure));
        return (marker, default_unit_title(item));
    };
    let mut marker = None;
    let mut text = String::new();
    let mut previous_marker = false;
    for (index, field) in header.fields.iter().enumerate() {
        let (piece, is_marker) = if field == "marker" {
            if index == 0 {
                marker = Some(glyph(disclosure));
                continue;
            }
            (glyph(disclosure), true)
        } else {
            match field_value(item, cx.facts, field) {
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

/// Value of a header or template field for a non-tool unit.
fn field_value(item: &TranscriptItem, facts: &UnitFacts, field: &str) -> Option<String> {
    match field {
        "label" | "title" => Some(default_unit_title(item)),
        "tool" => facts.tool.clone(),
        "target" => facts.target.clone(),
        "status" => status_text(facts.status?).map(str::to_owned),
        "first_line" => first_line(item.text()).map(str::to_owned),
        _ => None,
    }
}

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

/// Summary line from the presentation's ordered sources; non-tool units
/// have no producer summary.
fn summary_text(item: &TranscriptItem, cx: &UnitContext<'_>) -> Option<String> {
    let summary = &cx.presentation.summary;
    summary.source.iter().find_map(|source| {
        let text = match source {
            SummarySource::Plugin => return None,
            SummarySource::FirstLine => first_line(item.text())?.to_owned(),
            SummarySource::Template => bcode_presentation_policy::render_summary_template(
                summary.template.as_deref()?,
                &|field| field_value(item, cx.facts, field),
            ),
        };
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        (!text.is_empty()).then_some(text)
    })
}
