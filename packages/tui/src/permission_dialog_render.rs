//! TUI permission dialog rendering.

use bmux_tui::chrome::{Border, Panel};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::prelude::{Line, Span, Style, Widget};
use bmux_tui::style::{Color, Modifier};

use super::permission_dialog::PermissionDialogState;
use super::permission_present::{PermissionDetail, permission_presentation};
use super::text_width::{display_width, wrap_text_with_continuation};

const MIN_DIALOG_WIDTH: u16 = 48;
const MAX_DIALOG_WIDTH: u16 = 100;
const MIN_DIALOG_HEIGHT: u16 = 12;
const MAX_DIALOG_HEIGHT: u16 = 24;

/// Render a permission approval dialog.
pub fn render_permission_dialog(state: &PermissionDialogState, frame: &mut Frame<'_>) {
    let area = dialog_area(frame.area());
    let permission = state.permission();
    let presentation = permission_presentation(&permission.tool_name, &permission.arguments_json);
    let rows = permission_rows(
        state,
        &permission.tool_name,
        &permission.agent_id,
        &presentation.risk,
        &presentation.details,
        presentation.raw_details.as_deref(),
        area.width.saturating_sub(4),
    );

    Panel::new()
        .border(Border::rounded().style(Style::new().fg(Color::Yellow)))
        .title(" Permission requested ")
        .padding(Insets::new(1, 2, 1, 2))
        .render(area, frame);

    let content = area.inset(Insets::new(2, 3, 2, 3));
    let visible_body_rows = content.height.saturating_sub(2);
    for (row_index, line) in rows.iter().take(usize::from(visible_body_rows)).enumerate() {
        let Ok(row_offset) = u16::try_from(row_index) else {
            return;
        };
        frame.write_line(
            Rect::new(
                content.x,
                content.y.saturating_add(row_offset),
                content.width,
                1,
            ),
            line,
        );
    }

    render_actions(state, content, frame);
}

/// Return the permission dialog panel area for a terminal area.
#[must_use]
pub fn dialog_area(area: Rect) -> Rect {
    let available_width = area.width.saturating_sub(4);
    let available_height = area.height.saturating_sub(4);
    let width = available_width.clamp(MIN_DIALOG_WIDTH.min(available_width), MAX_DIALOG_WIDTH);
    let height = available_height.clamp(MIN_DIALOG_HEIGHT.min(available_height), MAX_DIALOG_HEIGHT);
    let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
    let y = area
        .y
        .saturating_add(area.height.saturating_sub(height) / 3);
    Rect::new(x, y, width, height)
}

/// Return approve and deny button hit boxes for a dialog panel area.
#[must_use]
pub const fn action_areas(dialog: Rect) -> (Rect, Rect) {
    let content = dialog.inset(Insets::new(2, 3, 2, 3));
    let y = content.bottom().saturating_sub(1);
    let approve = Rect::new(content.x, y, 11, 1);
    let deny = Rect::new(content.x.saturating_add(13), y, 8, 1);
    (approve, deny)
}

fn permission_rows(
    state: &PermissionDialogState,
    tool_name: &str,
    agent_id: &str,
    risk: &str,
    details: &[PermissionDetail],
    raw_details: Option<&str>,
    width: u16,
) -> Vec<Line> {
    let mut rows = Vec::new();
    rows.push(Line::from_spans(vec![Span::styled(
        "Review this tool request before it runs.",
        Style::new().fg(Color::BrightWhite),
    )]));
    rows.push(Line::default());
    push_metadata_row(&mut rows, "tool", tool_name, width);
    push_metadata_row(&mut rows, "agent", agent_id, width);
    push_metadata_row(&mut rows, "risk", risk, width);
    rows.push(Line::default());

    for detail in details {
        push_detail_rows(&mut rows, detail, width);
    }

    if let Some(raw_details) = raw_details.filter(|raw| !raw.trim().is_empty()) {
        rows.push(Line::default());
        rows.push(Line::from_spans(vec![Span::styled(
            "raw details",
            muted_style().add_modifier(Modifier::BOLD),
        )]));
        for line in raw_details.lines().take(8) {
            push_wrapped_rows(
                &mut rows,
                &[Span::styled("  ", muted_style())],
                line,
                width,
                muted_style(),
            );
        }
    }

    rows.push(Line::default());
    rows.push(Line::from_spans(vec![Span::styled(
        format!(
            "tab/←/→ choose · enter {} · esc deny",
            state.focused_label()
        ),
        muted_style(),
    )]));
    rows
}

fn push_metadata_row(rows: &mut Vec<Line>, label: &str, value: &str, width: u16) {
    push_wrapped_rows(
        rows,
        &[
            Span::styled(
                format_label(label),
                muted_style().add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ", muted_style()),
        ],
        value,
        width,
        Style::new().fg(Color::BrightWhite),
    );
}

fn push_detail_rows(rows: &mut Vec<Line>, detail: &PermissionDetail, width: u16) {
    rows.push(Line::from_spans(vec![Span::styled(
        detail.label.clone(),
        muted_style().add_modifier(Modifier::BOLD),
    )]));
    for line in detail.value.lines() {
        push_wrapped_rows(
            rows,
            &[Span::styled("  ", muted_style())],
            line,
            width,
            Style::new().fg(Color::BrightWhite),
        );
    }
    rows.push(Line::default());
}

fn push_wrapped_rows(rows: &mut Vec<Line>, prefix: &[Span], text: &str, width: u16, style: Style) {
    let max_width = usize::from(width.max(1));
    let prefix_width: usize = prefix.iter().map(|span| display_width(&span.content)).sum();
    let first_width = max_width.saturating_sub(prefix_width).max(1);
    let next_width = max_width.saturating_sub(2).max(1);
    for (index, chunk) in wrap_text_with_continuation(text, first_width, next_width)
        .into_iter()
        .enumerate()
    {
        if index == 0 {
            let mut spans = prefix.to_owned();
            spans.push(Span::styled(chunk, style));
            rows.push(Line::from_spans(spans));
        } else {
            rows.push(Line::from_spans(vec![
                Span::styled("  ", muted_style()),
                Span::styled(chunk, style),
            ]));
        }
    }
}

fn render_actions(state: &PermissionDialogState, content: Rect, frame: &mut Frame<'_>) {
    let (approve_area, deny_area) = action_areas(Rect::new(
        content.x.saturating_sub(3),
        content.y.saturating_sub(2),
        content.width.saturating_add(6),
        content.height.saturating_add(4),
    ));
    render_button("Approve", state.focused_approval(), approve_area, frame);
    render_button("Deny", !state.focused_approval(), deny_area, frame);
}

fn render_button(label: &str, focused: bool, area: Rect, frame: &mut Frame<'_>) {
    let style = if focused {
        Style::new()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::BrightWhite)
    };
    frame.write_line(
        area,
        &Line::from_spans(vec![Span::styled(format!("[ {label} ]"), style)]),
    );
}

fn format_label(label: &str) -> String {
    format!("{label:>5}:")
}

const fn muted_style() -> Style {
    Style::new().fg(Color::BrightBlack)
}

#[cfg(test)]
mod tests {
    use bcode_ipc::PermissionSummary;
    use bcode_session_models::SessionId;
    use bmux_tui::buffer::Buffer;
    use uuid::Uuid;

    use super::{dialog_area, render_permission_dialog};
    use crate::permission_dialog::PermissionDialogState;

    #[test]
    fn dialog_area_scales_beyond_old_tiny_modal() {
        let area = dialog_area(bmux_tui::geometry::Rect::new(0, 0, 140, 50));

        assert!(area.width > 76);
        assert!(area.height > 14);
    }

    #[test]
    fn shell_permission_renders_semantic_fields_not_raw_json() {
        let state = PermissionDialogState::new(PermissionSummary {
            permission_id: "perm-1".to_owned(),
            session_id: SessionId(Uuid::nil()),
            tool_call_id: "call-1".to_owned(),
            tool_name: "shell.run".to_owned(),
            arguments_json: r#"{"command":"cargo check --workspace","cwd":"/repo"}"#.to_owned(),
            agent_id: "build".to_owned(),
        });
        let mut buffer = Buffer::empty(bmux_tui::geometry::Rect::new(0, 0, 100, 30));
        let mut frame = bmux_tui::frame::Frame::new(&mut buffer);

        render_permission_dialog(&state, &mut frame);
        let rendered = (0..30)
            .filter_map(|row| frame.buffer().row_symbols(row))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("cargo check --workspace"));
        assert!(!rendered.contains("{\"command\""));
    }
}
