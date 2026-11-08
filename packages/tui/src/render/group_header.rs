//! Terminal group header rows.
//!
//! A group renders as one header row attached to its first member's layout
//! entry. The header records the group's disclosure anchor on the same row it
//! paints, so hit regions and focus derive from the retained layout.

use bcode_plugin_sdk::tui_visual::TuiVisualAnchor;
use bcode_presentation_policy::models::{
    DEFAULT_GROUP_HEADER, Disclosure, ResolvedPresentation, UnitStatus,
};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::text::truncate_line_to_display_width;

use crate::structure::{DisclosureTarget, GroupView};
use crate::time_format::format_millis;

/// Most distinct tools the `tools` field names before eliding the rest.
pub const MAX_HEADER_TOOLS: usize = 6;

/// Push `view`'s header under its effective `presentation`: leading spacing,
/// then one row bounded to `width` (overflow is truncated with an
/// ellipsis), carrying the `disclosure:group:<id>` anchor.
pub fn push_group_header(
    rows: &mut Vec<Line>,
    anchors: &mut Vec<TuiVisualAnchor>,
    view: &GroupView,
    presentation: &ResolvedPresentation,
    width: u16,
) {
    let theme = super::semantic_state_theme();
    let meta = theme.transcript.tool_metadata;
    for _ in 0..presentation.spacing.clamped().before {
        rows.push(Line::default());
    }
    anchors.push(TuiVisualAnchor {
        key: DisclosureTarget::Group(view.id).to_hit_id(),
        row: rows.len(),
        source: None,
    });
    let indent = crate::tool_anatomy::effective_indent(presentation, width);
    let (marker, text) = header_text(view, presentation);
    let mut spans = Vec::new();
    if indent > 0 {
        spans.push(Span::raw(" ".repeat(usize::from(indent))));
    }
    if let Some(marker) = marker {
        spans.push(Span::styled(format!("{marker} "), meta));
    }
    spans.push(Span::styled(text, status_style(&theme, view)));
    let row = truncate_line_to_display_width(&Line::from_spans(spans), usize::from(width));
    let raised = theme
        .effects
        .raised_background(theme.surfaces.raised, theme.canvas);
    rows.push(match raised {
        Some(raised) => crate::theme::effects::raised_row(&row, usize::from(width), raised),
        None => row,
    });
}

/// Leading marker (painted as a prefix) and the joined header text.
fn header_text(view: &GroupView, presentation: &ResolvedPresentation) -> (Option<String>, String) {
    let default_fields = DEFAULT_GROUP_HEADER.map(str::to_owned);
    let fields = presentation
        .header
        .as_ref()
        .map_or(&default_fields[..], |header| &header.fields[..]);
    let mut marker = None;
    let mut text = String::new();
    let mut previous_marker = false;
    for (index, field) in fields.iter().enumerate() {
        if field == "marker" {
            let glyph = glyph(presentation.disclosure);
            if index == 0 {
                marker = Some(glyph);
                continue;
            }
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(&glyph);
            previous_marker = true;
            continue;
        }
        let Some(piece) = field_text(view, presentation, field).filter(|text| !text.is_empty())
        else {
            continue;
        };
        if !text.is_empty() {
            text.push_str(if previous_marker { " " } else { " · " });
        }
        text.push_str(&piece);
        previous_marker = false;
    }
    (marker, text)
}

fn field_text(
    view: &GroupView,
    presentation: &ResolvedPresentation,
    field: &str,
) -> Option<String> {
    match field {
        "count" => Some(count_text(view.members.len())),
        "tools" => Some(tools_text(&view.tools)),
        "tool" => view.facts.tool.clone(),
        "status" => view
            .facts
            .status
            .map(|status| status_text(status).to_owned()),
        "timing" if presentation.part_visible("timing") => view
            .duration_ms
            .map(|duration| format!("duration {}", format_millis(duration))),
        _ => None,
    }
}

fn count_text(count: usize) -> String {
    if count == 1 {
        "1 tool call".to_owned()
    } else {
        format!("{count} tool calls")
    }
}

/// `read ×3, grep ×2`, eliding tools past [`MAX_HEADER_TOOLS`].
fn tools_text(tools: &[(String, usize)]) -> String {
    let mut parts: Vec<String> = tools
        .iter()
        .take(MAX_HEADER_TOOLS)
        .map(|(tool, count)| {
            if *count == 1 {
                tool.clone()
            } else {
                format!("{tool} ×{count}")
            }
        })
        .collect();
    if tools.len() > MAX_HEADER_TOOLS {
        parts.push("…".to_owned());
    }
    parts.join(", ")
}

const fn status_text(status: UnitStatus) -> &'static str {
    match status {
        UnitStatus::Requested => "requested",
        UnitStatus::Running => "running",
        UnitStatus::Waiting => "waiting",
        UnitStatus::Succeeded => "ok",
        UnitStatus::Failed => "failed",
        UnitStatus::Cancelled => "cancelled",
        UnitStatus::TimedOut => "timed out",
    }
}

/// Header style from the aggregate status, matching tool header styles.
const fn status_style(theme: &crate::theme::PresentedTheme, view: &GroupView) -> Style {
    match view.facts.status {
        Some(UnitStatus::Running) => theme.transcript.tool_running_title,
        Some(UnitStatus::Waiting) => theme.transcript.tool_waiting_title,
        Some(UnitStatus::TimedOut) => theme.transcript.tool_timed_out_title,
        Some(UnitStatus::Failed) => theme.transcript.tool_failed_title,
        Some(UnitStatus::Cancelled) => theme.transcript.tool_cancelled_title,
        Some(UnitStatus::Succeeded) if view.facts.is_error => theme.transcript.tool_failed_title,
        Some(UnitStatus::Succeeded) => theme.transcript.tool_succeeded_title,
        Some(UnitStatus::Requested) | None => theme.transcript.tool_requested_title,
    }
}

/// Disclosure marker glyph from the theme's chrome icon set, matching tool units.
fn glyph(disclosure: Disclosure) -> String {
    super::semantic_state_theme()
        .unit_chrome
        .marker(disclosure == Disclosure::Expanded)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_list_counts_repeats_and_elides_long_lists() {
        let tools = vec![("read".to_owned(), 3), ("grep".to_owned(), 1)];
        assert_eq!(tools_text(&tools), "read ×3, grep");
        let many: Vec<(String, usize)> = (0..=MAX_HEADER_TOOLS)
            .map(|index| (format!("t{index}"), 1))
            .collect();
        assert!(tools_text(&many).ends_with(", …"));
        assert_eq!(count_text(1), "1 tool call");
        assert_eq!(count_text(5), "5 tool calls");
    }
}
