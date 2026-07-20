//! TUI permission dialog rendering.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui::text_width::{display_width, wrap_text_with_continuation};
use bmux_tui_components::action_row::{ActionButton, ActionRow, ActionRowStyles};
use bmux_tui_components::labeled_details::{DetailItem, LabeledDetails, LabeledDetailsStyles};
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing, ModalTheme};

use super::permission_dialog::PermissionDialogState;
use super::permission_present::{PermissionDetail, permission_presentation};

const MIN_DIALOG_WIDTH: u16 = 48;
const MAX_DIALOG_WIDTH: u16 = 100;
const MIN_DIALOG_HEIGHT: u16 = 12;
const MAX_DIALOG_HEIGHT: u16 = 24;

/// Render a permission approval dialog.
pub fn render_permission_dialog(state: &PermissionDialogState, frame: &mut Frame<'_>) {
    let modal = modal_frame();
    let area = modal.panel_area(frame.area());
    let permission = state.permission();
    let presentation = permission_presentation(&permission.tool_name, &permission.arguments_json);
    let rows = permission_rows(&PermissionRowsInput {
        state,
        tool_name: &permission.tool_name,
        agent_id: &permission.agent_id,
        risk: &presentation.risk,
        policy_source: permission.policy_source.as_deref(),
        policy_reason: permission.policy_reason.as_deref(),
        details: &presentation.details,
        raw_details: presentation.raw_details.as_deref(),
        width: area.width.saturating_sub(4),
    });

    modal.render(frame.area(), frame);

    let content = modal.content_area(frame.area());
    let visible_body_rows = content.height.saturating_sub(2);
    for (row_index, line) in rows.iter().take(usize::from(visible_body_rows)).enumerate() {
        let Ok(row_offset) = u16::try_from(row_index) else {
            return;
        };
        modal.render_line(
            Rect::new(
                content.x,
                content.y.saturating_add(row_offset),
                content.width,
                1,
            ),
            line,
            frame,
        );
    }

    render_actions(state, content, frame);
}

/// Return the permission dialog panel area for a terminal area.
#[must_use]
pub fn dialog_area(area: Rect) -> Rect {
    modal_frame().panel_area(area)
}

fn modal_frame() -> ModalFrame {
    ModalFrame::new(
        ModalSizing::new(
            Size::new(MIN_DIALOG_WIDTH, MIN_DIALOG_HEIGHT),
            Size::new(MAX_DIALOG_WIDTH, MAX_DIALOG_HEIGHT),
            Insets::all(4),
        ),
        ModalTheme::dark(Color::Yellow),
    )
    .title(" Permission requested ")
    .padding(Insets::new(1, 2, 1, 2))
    .placement(ModalPlacement::UpperThird)
}

/// Return approve and deny button hit boxes for a dialog panel area.
#[must_use]
pub fn action_areas(dialog: Rect) -> (Rect, Rect) {
    let content = dialog.inset(Insets::new(2, 3, 2, 3));
    let y = content.bottom().saturating_sub(1);
    let actions = action_buttons(false);
    let areas =
        ActionRow::new(&actions)
            .spacing(2)
            .action_areas(Rect::new(content.x, y, content.width, 1));
    (
        areas
            .first()
            .copied()
            .unwrap_or_else(|| Rect::new(content.x, y, 0, 1)),
        areas
            .get(1)
            .copied()
            .unwrap_or_else(|| Rect::new(content.x, y, 0, 1)),
    )
}

#[derive(Debug, Clone, Copy)]
struct PermissionRowsInput<'a> {
    state: &'a PermissionDialogState,
    tool_name: &'a str,
    agent_id: &'a str,
    risk: &'a str,
    policy_source: Option<&'a str>,
    policy_reason: Option<&'a str>,
    details: &'a [PermissionDetail],
    raw_details: Option<&'a str>,
    width: u16,
}

fn permission_rows(input: &PermissionRowsInput<'_>) -> Vec<Line> {
    let mut rows = Vec::new();
    rows.push(Line::from_spans(vec![Span::styled(
        "Review this tool request before it runs.",
        Style::new().fg(Color::BrightWhite),
    )]));
    rows.push(Line::default());
    push_metadata_row(&mut rows, "tool", input.tool_name, input.width);
    push_metadata_row(&mut rows, "agent", input.agent_id, input.width);
    push_metadata_row(&mut rows, "risk", input.risk, input.width);
    if let Some(source) = input
        .policy_source
        .filter(|source| !source.trim().is_empty())
    {
        push_metadata_row(&mut rows, "policy", source, input.width);
    }
    if let Some(reason) = input
        .policy_reason
        .filter(|reason| !reason.trim().is_empty())
    {
        push_metadata_row(&mut rows, "reason", reason, input.width);
    }
    rows.push(Line::default());

    let detail_items = input
        .details
        .iter()
        .map(|detail| DetailItem::new(detail.label.clone(), detail.value.clone()))
        .collect::<Vec<_>>();
    rows.extend(
        LabeledDetails::new(&detail_items)
            .styles(LabeledDetailsStyles {
                label: muted_style().add_modifier(Modifier::BOLD),
                value: Style::new().fg(Color::BrightWhite),
                continuation: muted_style(),
            })
            .lines(input.width),
    );

    if let Some(raw_details) = input.raw_details.filter(|raw| !raw.trim().is_empty()) {
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
                input.width,
                muted_style(),
            );
        }
    }

    rows.push(Line::default());
    rows.push(Line::from_spans(vec![Span::styled(
        format!(
            "tab/←/→ choose · enter {} · esc deny",
            input.state.focused_label()
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
    let dialog = Rect::new(
        content.x.saturating_sub(3),
        content.y.saturating_sub(2),
        content.width.saturating_add(6),
        content.height.saturating_add(4),
    );
    let (approve_area, _) = action_areas(dialog);
    let buttons = action_buttons(state.permission().can_remember_policy);
    let focused = if state.permission().can_remember_policy {
        match (state.focused_approval(), state.focused_remember()) {
            (true, false) => 0,
            (true, true) => 1,
            (false, false) => 2,
            (false, true) => 3,
        }
    } else {
        usize::from(!state.focused_approval())
    };
    ActionRow::new(&buttons)
        .focused(focused)
        .spacing(2)
        .styles(action_styles())
        .render_with_fallback_style(
            Rect::new(approve_area.x, approve_area.y, content.width, 1),
            frame,
            ModalTheme::dark(Color::Yellow).text,
        );
}

fn action_buttons(can_remember_policy: bool) -> Vec<ActionButton> {
    if can_remember_policy {
        vec![
            ActionButton::new("approve_once", "Approve once"),
            ActionButton::new("remember_allow", "Remember allow"),
            ActionButton::new("deny_once", "Deny once"),
            ActionButton::new("remember_deny", "Remember deny"),
        ]
    } else {
        vec![
            ActionButton::new("approve", "Approve"),
            ActionButton::new("deny", "Deny"),
        ]
    }
}

const fn action_styles() -> ActionRowStyles {
    ActionRowStyles {
        normal: Style::new().fg(Color::BrightWhite),
        focused: Style::new()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        hovered: Style::new()
            .fg(Color::BrightWhite)
            .add_modifier(Modifier::UNDERLINE),
        pressed: Style::new()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        disabled: Style::new().fg(Color::BrightBlack),
    }
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
            batch: None,
            agent_id: "build".to_owned(),
            policy_source: None,
            policy_reason: None,
            can_remember_policy: false,
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
