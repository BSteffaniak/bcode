//! TUI permission dialog rendering.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::hit::{HitRegion, HitRole};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::Modifier;
use bmux_tui::text_width::{display_width, wrap_text_with_continuation};
use bmux_tui_components::action_row::{ActionButton, ActionRow, ActionRowStyles};
use bmux_tui_components::labeled_details::{DetailItem, LabeledDetails, LabeledDetailsStyles};
use bmux_tui_components::modal_frame::ModalFrame;

use super::permission_dialog::PermissionDialogState;
use super::permission_present::{PermissionDetail, permission_presentation};
use super::render::TuiTheme;

/// Render a permission approval dialog.
pub fn render_permission_dialog(
    state: &PermissionDialogState,
    frame: &mut Frame<'_>,
    theme: TuiTheme,
) {
    let modal = modal_frame(theme);
    let area = modal.panel_area(frame.area());
    let permission = state.permission();
    let presentation = permission_presentation(&permission.tool_name, &permission.arguments_json);
    let rows = permission_rows(&PermissionRowsInput {
        state,
        tool_name: &permission.tool_name,
        batch: permission.batch.as_ref(),
        agent_id: &permission.agent_id,
        risk: &presentation.risk,
        policy_source: permission.policy_source.as_deref(),
        policy_reason: permission.detail.as_deref(),
        details: &presentation.details,
        raw_details: presentation.raw_details.as_deref(),
        width: area.width.saturating_sub(4),
        theme,
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

    render_actions(state, content, frame, theme);
}

/// Return the permission dialog panel area for a terminal area.
#[must_use]
#[cfg(test)]
pub fn dialog_area(area: Rect, theme: TuiTheme) -> Rect {
    modal_frame(theme).panel_area(area)
}

fn modal_frame(theme: TuiTheme) -> ModalFrame {
    bcode_tui_components::permission::permission_modal(theme.modal_theme())
}

/// Return the permission action button hit boxes for the current dialog state.
#[must_use]
pub fn action_areas(state: &PermissionDialogState, dialog: Rect) -> Vec<Rect> {
    let content = dialog.inset(Insets::new(2, 3, 2, 3));
    let y = content.bottom().saturating_sub(1);
    let actions = action_buttons(
        state.permission().batch.is_some(),
        state.permission().can_remember,
    );
    ActionRow::new(&actions)
        .spacing(2)
        .action_areas(Rect::new(content.x, y, content.width, 1))
}

#[derive(Debug, Clone, Copy)]
struct PermissionRowsInput<'a> {
    state: &'a PermissionDialogState,
    tool_name: &'a str,
    batch: Option<&'a bcode_session_view_models::PermissionBatchView>,
    agent_id: &'a str,
    risk: &'a str,
    policy_source: Option<&'a str>,
    policy_reason: Option<&'a str>,
    details: &'a [PermissionDetail],
    raw_details: Option<&'a str>,
    width: u16,
    theme: TuiTheme,
}

fn permission_rows(input: &PermissionRowsInput<'_>) -> Vec<Line> {
    let mut rows = Vec::new();
    rows.push(Line::from_spans(vec![Span::styled(
        "Review this tool request before it runs.",
        input.theme.text,
    )]));
    rows.push(Line::default());
    push_metadata_row(&mut rows, "tool", input.tool_name, input.width, input.theme);
    if let Some(batch) = input.batch {
        push_metadata_row(
            &mut rows,
            "batch",
            &format!(
                "{} of {}",
                batch.call_index.saturating_add(1),
                batch.call_count
            ),
            input.width,
            input.theme,
        );
    }
    push_metadata_row(&mut rows, "agent", input.agent_id, input.width, input.theme);
    push_metadata_row(&mut rows, "risk", input.risk, input.width, input.theme);
    if let Some(source) = input
        .policy_source
        .filter(|source| !source.trim().is_empty())
    {
        push_metadata_row(&mut rows, "policy", source, input.width, input.theme);
    }
    if let Some(reason) = input
        .policy_reason
        .filter(|reason| !reason.trim().is_empty())
    {
        push_metadata_row(&mut rows, "reason", reason, input.width, input.theme);
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
                label: input.theme.muted.add_modifier(Modifier::BOLD),
                value: input.theme.text,
                continuation: input.theme.muted,
            })
            .lines(input.width),
    );

    if let Some(raw_details) = input.raw_details.filter(|raw| !raw.trim().is_empty()) {
        rows.push(Line::default());
        rows.push(Line::from_spans(vec![Span::styled(
            "raw details",
            input.theme.muted.add_modifier(Modifier::BOLD),
        )]));
        for line in raw_details.lines().take(8) {
            push_wrapped_rows(
                &mut rows,
                &[Span::styled("  ", input.theme.muted)],
                line,
                input.width,
                input.theme.muted,
            );
        }
    }

    rows.push(Line::default());
    rows.push(Line::from_spans(vec![Span::styled(
        format!(
            "tab/←/→ choose · enter {} · esc deny",
            input.state.focused_label()
        ),
        input.theme.muted,
    )]));
    rows
}

fn push_metadata_row(rows: &mut Vec<Line>, label: &str, value: &str, width: u16, theme: TuiTheme) {
    push_wrapped_rows(
        rows,
        &[
            Span::styled(
                format_label(label),
                theme.muted.add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ", theme.muted),
        ],
        value,
        width,
        theme.text,
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
                Span::styled("  ", style),
                Span::styled(chunk, style),
            ]));
        }
    }
}

fn render_actions(
    state: &PermissionDialogState,
    content: Rect,
    frame: &mut Frame<'_>,
    theme: TuiTheme,
) {
    let dialog = Rect::new(
        content.x.saturating_sub(3),
        content.y.saturating_sub(2),
        content.width.saturating_add(6),
        content.height.saturating_add(4),
    );
    let areas = action_areas(state, dialog);
    let buttons = action_buttons(
        state.permission().batch.is_some(),
        state.permission().can_remember,
    );
    let focused = state.focused_action_index();
    let row_area = Rect::new(
        areas.first().map_or(content.x, |area| area.x),
        areas
            .first()
            .map_or_else(|| content.bottom().saturating_sub(1), |area| area.y),
        content.width,
        1,
    );
    ActionRow::new(&buttons)
        .focused(focused)
        .spacing(2)
        .styles(action_styles(theme))
        .render_with_fallback_style(row_area, frame, theme.modal_theme().text);
    for (index, area) in areas.into_iter().enumerate() {
        frame.push_hit(
            HitRegion::new(format!("permission-action:{index}"), area)
                .role(HitRole::Action)
                .layer(20),
        );
    }
}

fn action_buttons(has_batch: bool, can_remember_policy: bool) -> Vec<ActionButton> {
    match (has_batch, can_remember_policy) {
        (true, true) => vec![
            ActionButton::new("approve_once", "Approve once"),
            ActionButton::new("approve_batch", "Approve batch"),
            ActionButton::new("remember_allow", "Remember allow"),
            ActionButton::new("deny_once", "Deny once"),
            ActionButton::new("deny_batch", "Deny batch"),
            ActionButton::new("remember_deny", "Remember deny"),
        ],
        (true, false) => vec![
            ActionButton::new("approve_once", "Approve once"),
            ActionButton::new("approve_batch", "Approve batch"),
            ActionButton::new("deny_once", "Deny once"),
            ActionButton::new("deny_batch", "Deny batch"),
        ],
        (false, true) => vec![
            ActionButton::new("approve_once", "Approve once"),
            ActionButton::new("remember_allow", "Remember allow"),
            ActionButton::new("deny_once", "Deny once"),
            ActionButton::new("remember_deny", "Remember deny"),
        ],
        (false, false) => vec![
            ActionButton::new("approve", "Approve"),
            ActionButton::new("deny", "Deny"),
        ],
    }
}

const fn action_styles(theme: TuiTheme) -> ActionRowStyles {
    ActionRowStyles {
        normal: theme.text,
        focused: theme.selection.add_modifier(Modifier::BOLD),
        hovered: theme.text.add_modifier(Modifier::UNDERLINE),
        pressed: theme.selection.add_modifier(Modifier::BOLD),
        disabled: theme.muted,
    }
}

fn format_label(label: &str) -> String {
    format!("{label:>5}:")
}

#[cfg(test)]
mod tests {
    use bcode_session_models::SessionId;
    use bcode_session_view_models::{PermissionBatchView, PermissionView};
    use bmux_tui::buffer::Buffer;
    use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
    use bmux_tui::geometry::Point;
    use uuid::Uuid;

    use super::{dialog_area, render_permission_dialog};
    use crate::permission_dialog::PermissionDialogState;
    use crate::render::TuiTheme;

    #[test]
    fn dialog_area_scales_beyond_old_tiny_modal() {
        let area = dialog_area(
            bmux_tui::geometry::Rect::new(0, 0, 140, 50),
            TuiTheme::for_agent("test", None, false),
        );

        assert!(area.width > 76);
        assert!(area.height > 14);
    }

    #[test]
    fn rendered_actions_register_exact_hit_regions_for_every_variant() {
        let state = PermissionDialogState::new(PermissionView {
            permission_id: "perm-actions".to_owned(),
            session_id: Some(SessionId(Uuid::nil())),
            tool_call_id: "call-actions".to_owned(),
            tool_name: "shell.run".to_owned(),
            arguments_json: r#"{"command":"cargo check"}"#.to_owned(),
            batch: Some(PermissionBatchView {
                batch_id: "batch-actions".to_owned(),
                call_index: 0,
                call_count: 2,
            }),
            agent_id: "build".to_owned(),
            title: Some("Permission requested: shell.run".to_owned()),
            policy_source: Some("skill".to_owned()),
            detail: None,
            resolved: false,
            approved: None,
            can_remember: true,
        });
        let mut buffer = Buffer::empty(bmux_tui::geometry::Rect::new(0, 0, 120, 35));
        let mut frame = bmux_tui::frame::Frame::new(&mut buffer);

        render_permission_dialog(&state, &mut frame, TuiTheme::for_agent("test", None, false));

        let action_hits = frame
            .hits()
            .regions()
            .iter()
            .filter(|hit| hit.id.as_str().starts_with("permission-action:"))
            .collect::<Vec<_>>();
        assert_eq!(action_hits.len(), 6);
        for (index, hit) in action_hits.into_iter().enumerate() {
            let event = MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(hit.area.x, hit.area.y),
            );
            let expected = format!("permission-action:{index}");
            assert_eq!(
                frame
                    .hits()
                    .hit_mouse(event)
                    .map(|resolved| resolved.id().as_str().to_owned()),
                Some(expected)
            );
        }
    }

    #[test]
    fn shell_permission_renders_semantic_fields_not_raw_json() {
        let state = PermissionDialogState::new(PermissionView {
            permission_id: "perm-1".to_owned(),
            session_id: Some(SessionId(Uuid::nil())),
            tool_call_id: "call-1".to_owned(),
            tool_name: "shell.run".to_owned(),
            arguments_json: r#"{"command":"cargo check --workspace","cwd":"/repo"}"#.to_owned(),
            batch: None,
            agent_id: "build".to_owned(),
            title: Some("Permission requested: shell.run".to_owned()),
            policy_source: None,
            detail: None,
            resolved: false,
            approved: None,
            can_remember: false,
        });
        let mut buffer = Buffer::empty(bmux_tui::geometry::Rect::new(0, 0, 100, 30));
        let mut frame = bmux_tui::frame::Frame::new(&mut buffer);

        render_permission_dialog(&state, &mut frame, TuiTheme::for_agent("test", None, false));
        let rendered = (0..30)
            .filter_map(|row| frame.buffer().row_symbols(row))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("cargo check --workspace"));
        assert!(!rendered.contains("{\"command\""));
    }

    #[test]
    fn batched_permission_renders_position_and_batch_actions() {
        let state = PermissionDialogState::new(PermissionView {
            permission_id: "perm-batch".to_owned(),
            session_id: Some(SessionId(Uuid::nil())),
            tool_call_id: "call-2".to_owned(),
            tool_name: "shell.run".to_owned(),
            arguments_json: r#"{"command":"cargo check"}"#.to_owned(),
            batch: Some(PermissionBatchView {
                batch_id: "batch-1".to_owned(),
                call_index: 1,
                call_count: 3,
            }),
            agent_id: "build".to_owned(),
            title: Some("Permission requested: shell.run".to_owned()),
            policy_source: None,
            detail: None,
            resolved: false,
            approved: None,
            can_remember: false,
        });
        let mut buffer = Buffer::empty(bmux_tui::geometry::Rect::new(0, 0, 100, 30));
        let mut frame = bmux_tui::frame::Frame::new(&mut buffer);

        render_permission_dialog(&state, &mut frame, TuiTheme::for_agent("test", None, false));
        let rendered = (0..30)
            .filter_map(|row| frame.buffer().row_symbols(row))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("2 of 3"));
        assert!(rendered.contains("Approve batch"));
        assert!(rendered.contains("Deny batch"));
    }
}
