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
//!
//! User and assistant messages apply the theme's `speaker` chrome to their
//! header (see [`speaker_layout`]): `prefix` keeps the heading row, `badge`
//! paints the heading as a filled badge, `bar` paints a leading bar on every
//! row of the message, and `gutter` moves the heading into a left gutter
//! beside the body. The bar and gutter widths come from [`speaker_layout`],
//! which [`super::TranscriptItemLayout`] also uses for Markdown geometry, so
//! painting, hit regions, and selection share one definition.

use bcode_presentation_policy::models::{
    Disclosure, ResolvedPresentation, Spacing, SummarySource, UnitFacts, UnitKind, UnitStatus,
};
use bcode_tui_theme::chrome::SpeakerStyle;
use bmux_tui::prelude::{Line, Span};
use bmux_tui::style::{Modifier, Style};
use bmux_tui::text::truncate_line_to_display_width;
use bmux_tui::text_width::display_width;

use super::super::structure::DisclosureTarget;
use super::super::theme::chrome::UnitChrome;
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

/// Cells reserved for the heading in the `gutter` speaker treatment. Wider
/// headings are truncated; the body starts at the same column for every
/// speaker.
const GUTTER_LABEL_CELLS: u16 = 8;

/// Narrowest content width (gutter included) that keeps a gutter; narrower
/// messages fall back to the `prefix` treatment.
const MIN_GUTTER_WIDTH: u16 = GUTTER_LABEL_CELLS + 12;

/// Leading bar candidates, most preferred first; the first measuring one
/// cell is used.
const BAR_GLYPHS: [&str; 3] = ["\u{258e}", "\u{2502}", "|"];

/// Narrowest body width beside a bar; narrower messages fall back to the
/// `prefix` treatment.
const MIN_BAR_BODY_CELLS: u16 = 4;

/// Speaker treatment of one message at one content width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SpeakerLayout {
    /// Effective treatment after narrow-width degradation.
    pub(super) style: SpeakerStyle,
    /// Columns left of the body on every painted row (`bar` and `gutter`).
    pub(super) prefix: u16,
}

impl SpeakerLayout {
    const PREFIX: Self = Self {
        style: SpeakerStyle::Prefix,
        prefix: 0,
    };
}

/// Leading bar glyph and its measured width.
fn bar_glyph() -> (&'static str, u16) {
    BAR_GLYPHS
        .iter()
        .copied()
        .find(|glyph| display_width(glyph) == 1)
        .map_or(("|", 1), |glyph| (glyph, 1))
}

/// Speaker treatment of `item` at content `width` under `chrome`.
///
/// Only user and assistant messages carry a speaker; every other unit uses
/// `prefix`. Degradation is explicit: `bar` needs room for the bar, its gap,
/// and [`MIN_BAR_BODY_CELLS`]; `gutter` needs [`MIN_GUTTER_WIDTH`]; otherwise
/// the message uses `prefix`. `badge` reserves no columns (it degrades per
/// heading when the badge does not fit, see `push_badge_heading`).
pub(super) fn speaker_layout(
    chrome: UnitChrome,
    item: &TranscriptItem,
    width: u16,
) -> SpeakerLayout {
    if !matches!(
        item.kind(),
        TranscriptItemKind::UserMessage | TranscriptItemKind::AssistantMessage
    ) {
        return SpeakerLayout::PREFIX;
    }
    match chrome.speaker {
        SpeakerStyle::Prefix => SpeakerLayout::PREFIX,
        SpeakerStyle::Badge => SpeakerLayout {
            style: SpeakerStyle::Badge,
            prefix: 0,
        },
        SpeakerStyle::Bar => {
            let prefix = bar_glyph().1.saturating_add(1);
            if width >= prefix.saturating_add(MIN_BAR_BODY_CELLS) {
                SpeakerLayout {
                    style: SpeakerStyle::Bar,
                    prefix,
                }
            } else {
                SpeakerLayout::PREFIX
            }
        }
        SpeakerStyle::Gutter => {
            if width >= MIN_GUTTER_WIDTH {
                SpeakerLayout {
                    style: SpeakerStyle::Gutter,
                    prefix: GUTTER_LABEL_CELLS,
                }
            } else {
                SpeakerLayout::PREFIX
            }
        }
    }
}

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
    let speaker = speaker_layout(semantic_state_theme().unit_chrome, item, cx.width);
    // The body is laid out beside the speaker prefix (bar or gutter).
    let body_width = inner.saturating_sub(speaker.prefix);
    // Badge and gutter headings are painted by the frame, not the body.
    let framed_heading = matches!(speaker.style, SpeakerStyle::Badge | SpeakerStyle::Gutter);
    match disclosure {
        Disclosure::Expanded | Disclosure::Hidden => {
            if framed_heading {
                push_unit_body(&mut body, item, "", body_width, markdown);
                // The empty heading row; body rows always carry their indent.
                if body.first().is_some_and(|row| row.width() == 0) {
                    body.remove(0);
                }
            } else {
                push_unit_body(
                    &mut body,
                    item,
                    &joined_heading(&title),
                    body_width,
                    markdown,
                );
            }
            // Every body but a metadata note ends with one blank row, which
            // the frame's trailing spacing replaces.
            if natural_after == 1 && body.last().is_some_and(|row| row.spans.is_empty()) {
                body.pop();
            }
        }
        Disclosure::Summary => {
            if !framed_heading {
                push_heading(&mut body, item, title.clone(), body_width);
            }
            if let Some(summary) = summary_text(item, cx) {
                body.push(truncate_line_to_display_width(
                    &Line::from_spans(vec![
                        Span::styled("  ", muted_style()),
                        Span::styled(summary, muted_style()),
                    ]),
                    usize::from(body_width),
                ));
            }
        }
        Disclosure::Collapsed => {
            if !framed_heading {
                push_heading(&mut body, item, title.clone(), body_width);
            }
        }
    }
    match speaker.style {
        SpeakerStyle::Prefix => {}
        SpeakerStyle::Badge => {
            let mut heading = Vec::new();
            push_badge_heading(&mut heading, item, title, inner);
            body.splice(0..0, heading);
        }
        SpeakerStyle::Bar => apply_bar(&mut body, unit_title_style(item)),
        SpeakerStyle::Gutter => apply_gutter(&mut body, item, title, speaker.prefix),
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

/// Heading text of an expanded unit: the marker and the header text.
fn joined_heading((marker, text): &(Option<String>, String)) -> String {
    match marker {
        Some(marker) if text.is_empty() => marker.clone(),
        Some(marker) => format!("{marker} {text}"),
        None => text.clone(),
    }
}

/// Push the heading as a filled badge (` You `) after the marker, bounded to
/// `width`. When the padded badge does not fit, the heading degrades to the
/// plain `prefix` heading row.
fn push_badge_heading(
    rows: &mut Vec<Line>,
    item: &TranscriptItem,
    (marker, text): (Option<String>, String),
    width: u16,
) {
    let meta = semantic_state_theme().transcript.tool_metadata;
    let mut spans = marker
        .clone()
        .map(|marker| vec![Span::styled(format!("{marker} "), meta)])
        .unwrap_or_default();
    if !text.is_empty() {
        spans.push(Span::styled(
            format!(" {text} "),
            unit_title_style(item).add_modifier(Modifier::REVERSED),
        ));
    }
    let row = Line::from_spans(spans);
    if row.width() > usize::from(width) {
        push_heading(rows, item, (marker, text), width);
    } else {
        rows.push(row);
    }
}

/// Paint a leading bar and its one-cell gap on every row of the message.
fn apply_bar(rows: &mut [Line], style: Style) {
    let (glyph, _) = bar_glyph();
    for row in rows {
        row.spans
            .splice(0..0, [Span::styled(glyph, style), Span::raw(" ")]);
    }
}

/// Paint the heading into a `gutter`-cell left column beside the body.
///
/// The first row carries the heading (truncated by display width, then padded
/// to the gutter), every other row a blank gutter, so the body column is the
/// same on every row. A message without body rows keeps one row for the
/// heading.
fn apply_gutter(
    rows: &mut Vec<Line>,
    item: &TranscriptItem,
    (marker, text): (Option<String>, String),
    gutter: u16,
) {
    let cells = usize::from(gutter);
    let meta = semantic_state_theme().transcript.tool_metadata;
    let mut label = marker
        .map(|marker| {
            vec![Span::styled(
                if text.is_empty() {
                    marker
                } else {
                    format!("{marker} ")
                },
                meta,
            )]
        })
        .unwrap_or_default();
    label.push(Span::styled(text, unit_title_style(item)));
    let mut label = truncate_line_to_display_width(&Line::from_spans(label), cells);
    let pad = cells.saturating_sub(label.width());
    if pad > 0 {
        label.spans.push(Span::raw(" ".repeat(pad)));
    }
    if rows.is_empty() {
        rows.push(Line::default());
    }
    for (index, row) in rows.iter_mut().enumerate() {
        let lead = if index == 0 {
            std::mem::take(&mut label.spans)
        } else {
            vec![Span::raw(" ".repeat(cells))]
        };
        row.spans.splice(0..0, lead);
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

#[cfg(test)]
mod tests {
    use bcode_config::TuiDiffViewerConfig;
    use bcode_markdown_render::MarkdownRenderOptions;
    use bcode_session_view_models::TextFormat;
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};
    use bmux_tui::paint::{LocalRect, PaintCx};

    use super::*;
    use crate::render::{
        TranscriptItemLayout, set_plugin_visual_theme, set_structure,
        transcript_item_layout_from_item,
    };
    use crate::structure::StructureState;

    const SPEAKERS: [SpeakerStyle; 4] = [
        SpeakerStyle::Prefix,
        SpeakerStyle::Badge,
        SpeakerStyle::Bar,
        SpeakerStyle::Gutter,
    ];

    /// Install `speaker` chrome and the host structure for this test thread.
    fn with_speaker(speaker: SpeakerStyle) -> crate::theme::PresentedTheme {
        let initial = crate::theme::resolve_initial_theme();
        let mut theme = initial.presented(initial.accent);
        theme.unit_chrome.speaker = speaker;
        set_plugin_visual_theme(&theme);
        set_structure(StructureState::default().snapshot());
        theme
    }

    fn with_rules(speaker: SpeakerStyle, rules: &str) {
        with_speaker(speaker);
        let mut state = StructureState::default();
        state.set_layers(&[bcode_presentation_policy::models::RuleLayer {
            source: bcode_presentation_policy::models::LayerSource::Theme("test".into()),
            rules: toml::from_str::<bcode_presentation_policy::models::PresentationSection>(rules)
                .unwrap()
                .rules,
        }]);
        set_structure(state.snapshot());
    }

    fn layout(
        item: &TranscriptItem,
        width: u16,
        markdown: Option<&bcode_markdown_render::MarkdownRenderResult>,
    ) -> (
        Vec<Line>,
        Vec<bcode_plugin_sdk::tui_visual::TuiVisualAnchor>,
    ) {
        let (rows, anchors, _) = transcript_item_layout_from_item(
            item,
            width,
            None,
            TuiDiffViewerConfig::default(),
            markdown,
        );
        (rows, anchors)
    }

    fn text(rows: &[Line]) -> Vec<String> {
        rows.iter().map(Line::plain_text).collect()
    }

    fn unit_anchor(
        item: &TranscriptItem,
        anchors: &[bcode_plugin_sdk::tui_visual::TuiVisualAnchor],
    ) -> usize {
        let key = DisclosureTarget::Unit(item.id().get()).to_hit_id();
        anchors
            .iter()
            .find(|anchor| anchor.key == key)
            .map(|anchor| anchor.row)
            .expect("unit anchor")
    }

    /// Paint `rows` into a terminal buffer, as the transcript does.
    fn paint(rows: &[Line], width: u16) -> Buffer {
        let height = u16::try_from(rows.len()).unwrap();
        let area = Rect::new(0, 0, width.max(1), height.max(1));
        let mut buffer = Buffer::empty(area);
        {
            let mut frame = Frame::new(&mut buffer);
            let mut cx = PaintCx::new(&mut frame);
            for (y, row) in rows.iter().enumerate() {
                let y = u16::try_from(y).unwrap();
                cx.write_line(LocalRect::new(0, i64::from(y), area.width, 1), row);
            }
        }
        buffer
    }

    fn cells(buffer: &Buffer, y: u16, columns: std::ops::Range<u16>) -> String {
        columns
            .filter_map(|x| buffer.get(Point::new(x, y)))
            .map(|cell| cell.symbol.as_str())
            .collect()
    }

    fn messages() -> Vec<TranscriptItem> {
        vec![
            TranscriptItem::new("You", "Why?\n\nBecause.".to_owned()),
            TranscriptItem::new("You", "漢字 body 👍".to_owned())
                .with_display_label("用户👩‍💻e\u{301}".to_owned()),
            TranscriptItem::with_format(
                "Assistant",
                "Answer with `code`.".to_owned(),
                TextFormat::Markdown,
            ),
            TranscriptItem::new("Assistant", String::new()),
        ]
    }

    #[test]
    fn prefix_speaker_is_the_default_and_other_units_ignore_speaker() {
        assert_eq!(UnitChrome::default().speaker, SpeakerStyle::Prefix);
        for speaker in SPEAKERS {
            let theme = with_speaker(speaker);
            let note = TranscriptItem::new("System", "Model changed.".to_owned());
            assert_eq!(
                speaker_layout(theme.unit_chrome, &note, 80),
                SpeakerLayout::PREFIX
            );
            let (rows, _) = layout(&note, 40, None);
            assert_eq!(text(&rows)[0], "System", "{speaker:?}");
        }
    }

    #[test]
    fn speakers_paint_their_header_treatment() {
        with_speaker(SpeakerStyle::Badge);
        let question = TranscriptItem::new("You", "Why?".to_owned());
        let (rows, anchors) = layout(&question, 40, None);
        assert_eq!(text(&rows), [" You ", "  Why?", ""]);
        assert!(
            rows[0]
                .spans
                .iter()
                .any(|span| span.style.modifiers.contains(Modifier::REVERSED)),
            "the badge is filled"
        );
        assert_eq!(unit_anchor(&question, &anchors), 0);

        with_speaker(SpeakerStyle::Bar);
        let (rows, anchors) = layout(&question, 40, None);
        let (bar, _) = bar_glyph();
        assert_eq!(
            text(&rows),
            [format!("{bar} You"), format!("{bar}   Why?"), String::new()]
        );
        assert_eq!(unit_anchor(&question, &anchors), 0);

        with_speaker(SpeakerStyle::Gutter);
        let (rows, anchors) = layout(&question, 40, None);
        assert_eq!(text(&rows), ["You       Why?", ""]);
        assert_eq!(unit_anchor(&question, &anchors), 0);
        let reply = TranscriptItem::new("Assistant", "Hi.".to_owned());
        let (rows, _) = layout(&reply, 40, None);
        assert_eq!(
            text(&rows),
            ["Bcode     Hi.", ""],
            "speakers share the body column"
        );
    }

    #[test]
    fn collapsed_and_summary_messages_keep_the_speaker_treatment() {
        let rules = "[[rules]]\nmatch = { kind = \"assistant_message\" }\nset = { disclosure = \"collapsed\" }\n\
                     [[rules]]\nmatch = { kind = \"user_message\" }\nset = { disclosure = \"summary\" }";
        let reply = TranscriptItem::new("Assistant", "First.\nSecond.".to_owned());
        let question = TranscriptItem::new("You", "What changed?\nMore".to_owned());

        with_rules(SpeakerStyle::Gutter, rules);
        let (rows, anchors) = layout(&reply, 40, None);
        assert_eq!(text(&rows), ["▸ Bcode ", ""]);
        assert_eq!(unit_anchor(&reply, &anchors), 0);
        let (rows, _) = layout(&question, 40, None);
        assert_eq!(text(&rows), ["▸ You     What changed?", ""]);

        with_rules(SpeakerStyle::Badge, rules);
        let (rows, _) = layout(&reply, 40, None);
        assert_eq!(text(&rows), ["▸  Bcode ", ""]);

        with_rules(SpeakerStyle::Bar, rules);
        let (rows, _) = layout(&question, 40, None);
        let (bar, _) = bar_glyph();
        assert_eq!(
            text(&rows),
            [
                format!("{bar} ▸ You"),
                format!("{bar}   What changed?"),
                String::new()
            ]
        );
    }

    #[test]
    fn narrow_widths_degrade_to_prefix_and_stay_bounded() {
        let widths = [0_u16, 1, 2, 3, 5, 6, 7, 12, 19, 20, 21, 40];
        for speaker in SPEAKERS {
            for item in messages() {
                for width in widths {
                    with_speaker(SpeakerStyle::Prefix);
                    let (prefix_rows, _) = layout(&item, width, None);
                    let theme = with_speaker(speaker);
                    let resolved = speaker_layout(theme.unit_chrome, &item, width);
                    assert!(
                        resolved.prefix <= width,
                        "the prefix never exceeds the width"
                    );
                    let (rows, anchors) = layout(&item, width, None);
                    let context =
                        format!("{speaker:?} {} at {width}: {:?}", item.role(), text(&rows));
                    assert!(unit_anchor(&item, &anchors) < rows.len(), "{context}");
                    match resolved.style {
                        SpeakerStyle::Prefix => {
                            assert_eq!(resolved.prefix, 0, "{context}");
                            assert_eq!(rows, prefix_rows, "{context}: degrades to prefix");
                        }
                        SpeakerStyle::Badge => assert_eq!(resolved.prefix, 0, "{context}"),
                        SpeakerStyle::Bar => {
                            assert!(width >= 2 + MIN_BAR_BODY_CELLS, "{context}");
                        }
                        SpeakerStyle::Gutter => assert!(width >= MIN_GUTTER_WIDTH, "{context}"),
                    }
                    let bounded = rows.iter().all(|row| row.width() <= usize::from(width));
                    match resolved.style {
                        SpeakerStyle::Prefix => {}
                        // A badge that does not fit degrades to the heading row.
                        SpeakerStyle::Badge => {
                            assert!(bounded || text(&rows) == text(&prefix_rows), "{context}");
                        }
                        // The treatment never paints past the content width.
                        SpeakerStyle::Bar | SpeakerStyle::Gutter => assert!(bounded, "{context}"),
                    }
                }
            }
        }
        let theme = with_speaker(SpeakerStyle::Gutter);
        let question = TranscriptItem::new("You", "Why?".to_owned());
        assert_eq!(
            speaker_layout(theme.unit_chrome, &question, MIN_GUTTER_WIDTH - 1),
            SpeakerLayout::PREFIX
        );
        let (rows, _) = layout(&question, 3, None);
        assert_eq!(
            text(&rows)[0],
            "You",
            "a narrow gutter falls back to the heading row"
        );
        let theme = with_speaker(SpeakerStyle::Bar);
        assert_eq!(
            speaker_layout(theme.unit_chrome, &question, 2 + MIN_BAR_BODY_CELLS - 1),
            SpeakerLayout::PREFIX
        );
        with_speaker(SpeakerStyle::Badge);
        let (rows, _) = layout(&question, 3, None);
        assert_eq!(
            text(&rows)[0],
            "You",
            "a badge that does not fit is a heading row"
        );
    }

    #[test]
    fn wide_speaker_names_are_truncated_inside_the_gutter() {
        with_speaker(SpeakerStyle::Gutter);
        let item = TranscriptItem::new("You", "body".to_owned())
            .with_display_label("用户👩‍💻e\u{301}名字".to_owned());
        let (rows, _) = layout(&item, 30, None);
        let buffer = paint(&rows, 30);
        eprintln!(
            "DEBUG {:?} {:?} {:?}",
            text(&rows),
            rows.iter().map(Line::width).collect::<Vec<_>>(),
            buffer.row_symbols(0)
        );
        let gutter = usize::from(GUTTER_LABEL_CELLS);
        // The body starts right after the gutter on every row, whatever the
        // label's display width.
        assert_eq!(
            cells(&buffer, 0, GUTTER_LABEL_CELLS..30).trim_end(),
            "  body"
        );
        assert!(display_width(&cells(&buffer, 0, 0..GUTTER_LABEL_CELLS)) <= gutter);
        assert!(cells(&buffer, 0, 0..GUTTER_LABEL_CELLS).starts_with("You · "));
        assert!(
            rows[0].plain_text().contains('…'),
            "the wide label is truncated"
        );
    }

    #[test]
    fn markdown_hit_and_selection_geometry_agree_with_painted_speaker_rows() {
        let source = "See [链接 link](https://example.com) 👍 and more words to wrap around.";
        for speaker in SPEAKERS {
            for width in [12_u16, 24, 40] {
                let theme = with_speaker(speaker);
                let item = TranscriptItem::with_format(
                    "Assistant",
                    source.to_owned(),
                    TextFormat::Markdown,
                );
                let geometry = TranscriptItemLayout::resolve(&theme, &item, width);
                let rendered = bcode_markdown_render::render_markdown(
                    source,
                    &MarkdownRenderOptions::new(geometry.markdown_width())
                        .with_document_id(format!("transcript:{}", item.id().get())),
                );
                let (rows, _) = layout(&item, width, Some(&rendered));
                let buffer = paint(&rows, width);
                // Projection rows start after the heading (none in the gutter)
                // and end before the trailing blank row.
                let start = rows.len() - rendered.lines.len() - 1;
                for (index, line) in rendered.lines.iter().enumerate() {
                    let y = u16::try_from(start + index).unwrap();
                    let x = geometry.markdown_x();
                    let painted = cells(&buffer, y, x..width);
                    let expected = line.plain_text();
                    assert!(
                        painted.starts_with(expected.trim_end()),
                        "{speaker:?} at {width}: row {index} painted {painted:?}, projected {expected:?}"
                    );
                }
                let link = rendered
                    .geometry
                    .first()
                    .and_then(|geometry| geometry.rects.first())
                    .expect("link geometry");
                let y = u16::try_from(start).unwrap() + link.y;
                let x = geometry.markdown_x() + link.x;
                let hit = cells(&buffer, y, x..x + link.width);
                assert!(
                    "链接 link".starts_with(hit.trim_end()) && !hit.trim().is_empty(),
                    "{speaker:?} at {width}: hit cells {hit:?}"
                );
            }
        }
    }
}
