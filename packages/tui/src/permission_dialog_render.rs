//! TUI permission dialog rendering.

use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::composition::TextBlock;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::hit::{HitRegion, HitRole};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::Modifier;
use bmux_tui::text_width::display_width;
use bmux_tui_components::action_row::{
    ActionButton, ActionRow, ActionRowComponent, ActionRowState, ActionRowStyles,
};
use bmux_tui_components::labeled_details::{
    DetailItem, LabeledDetailsComponent, LabeledDetailsStyles,
};
use bmux_tui_components::modal_frame::{ModalFrame, ModalFrameComponent};

use super::permission_dialog::PermissionDialogState;
use super::permission_present::{PermissionDetail, permission_presentation};
use super::render::TuiTheme;

/// Render a permission approval dialog.
pub fn render_permission_dialog(
    state: &PermissionDialogState,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
) {
    let modal = modal_frame(theme);
    let area = modal.panel_area(Rect::new(0, 0, frame.area().width, frame.area().height));
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

    let root = Rect::new(0, 0, frame.area().width, frame.area().height);
    let content = modal.content_area(root);
    let shell = ModalFrameComponent::new("permission", modal, TextBlock::new(""));
    let layout = shell.layout(Constraints::tight(root.size()), &mut LayoutCx::new());
    shell.paint(&layout, frame);
    let visible_body_rows = content.height.saturating_sub(2);
    for (row_index, line) in rows.iter().take(usize::from(visible_body_rows)).enumerate() {
        let Ok(row_offset) = u16::try_from(row_index) else {
            return;
        };
        frame.write_line(
            LocalRect::terminal(Rect::new(
                content.x,
                content.y.saturating_add(row_offset),
                content.width,
                1,
            )),
            line,
        );
    }

    render_actions(state, content, frame, theme);
}

/// Most subject rows a docked dialog shows.
const DOCKED_SUBJECT_ROWS: usize = 8;

/// A docked permission dialog laid out at one width: its rows and the row of each action.
///
/// Painting, hit regions, and the dock height all come from this one layout.
pub struct DockedDialog {
    rows: Vec<Line>,
    action_rows: Vec<usize>,
}

impl DockedDialog {
    /// Lay out `state` at `width`: an accent rule, the title, the request's subject between
    /// light rules, the question, the actions (one numbered row each, or one row of buttons),
    /// and a hint row.
    #[must_use]
    pub fn new(state: &PermissionDialogState, width: u16, theme: TuiTheme, numbered: bool) -> Self {
        let permission = state.permission();
        let presentation =
            permission_presentation(&permission.tool_name, &permission.arguments_json);
        let cells = usize::from(width);
        let rule = |glyph: &str, style: Style| {
            Line::from_spans(vec![Span::styled(glyph.repeat(cells), style)])
        };
        let indented = |text: String, style: Style| {
            Line::from_spans(vec![Span::raw(" "), Span::styled(text, style)])
        };
        let mut rows = vec![
            rule("─", theme.focused),
            indented(
                permission
                    .title
                    .clone()
                    .unwrap_or_else(|| presentation.title.clone()),
                theme.focused.add_modifier(Modifier::BOLD),
            ),
            rule("╌", theme.border),
        ];
        let subject = if presentation.details.is_empty() {
            presentation
                .raw_details
                .as_deref()
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        } else {
            presentation
                .details
                .iter()
                .flat_map(|detail| detail.value.lines().map(str::to_owned).collect::<Vec<_>>())
                .collect()
        };
        rows.extend(
            subject
                .into_iter()
                .take(DOCKED_SUBJECT_ROWS)
                .map(|line| indented(line, theme.text)),
        );
        rows.push(rule("╌", theme.border));
        rows.push(indented("Do you want to proceed?".to_owned(), theme.text));
        let buttons = action_buttons(permission.batch.is_some(), permission.can_remember);
        let focused = state.focused_action_index();
        let mut action_rows = Vec::new();
        if numbered {
            for (index, button) in buttons.iter().enumerate() {
                action_rows.push(rows.len());
                let selected = index == focused;
                let cursor = if selected { "❯" } else { " " };
                rows.push(Line::from_spans(vec![
                    Span::styled(format!(" {cursor} "), theme.focused),
                    Span::styled(format!("{}. ", index + 1), theme.muted),
                    Span::styled(
                        button.label.clone(),
                        if selected { theme.focused } else { theme.text },
                    ),
                ]));
            }
        } else {
            let spans = buttons
                .iter()
                .enumerate()
                .flat_map(|(index, button)| {
                    let style = if index == focused {
                        theme.selection.add_modifier(Modifier::BOLD)
                    } else {
                        theme.text
                    };
                    [
                        Span::raw("  "),
                        Span::styled(format!("[ {} ]", button.label), style),
                    ]
                })
                .collect::<Vec<_>>();
            action_rows.push(rows.len());
            rows.push(Line::from_spans(spans));
        }
        rows.push(Line::default());
        rows.push(indented(
            if numbered {
                "Esc to deny · ↑/↓ to choose · Enter or a number to confirm".to_owned()
            } else {
                "Esc to deny · Tab to choose · Enter to confirm".to_owned()
            },
            theme.muted,
        ));
        Self { rows, action_rows }
    }

    /// Rows the dialog occupies.
    #[must_use]
    pub fn height(&self) -> u16 {
        u16::try_from(self.rows.len()).unwrap_or(u16::MAX)
    }

    /// Paint the dialog into `dock` and register one hit region per action (numbered) or
    /// per button (a row of buttons), as the modal dialog's actions are registered.
    pub fn paint(&self, dock: Rect, frame: &mut PaintCx<'_, '_>, theme: TuiTheme, numbered: bool) {
        frame.fill(
            LocalRect::terminal(dock),
            " ",
            theme.text.patch(Style::new()),
        );
        for (index, line) in self.rows.iter().enumerate() {
            let Ok(offset) = u16::try_from(index) else {
                return;
            };
            if offset >= dock.height {
                return;
            }
            frame.write_line(
                LocalRect::terminal(Rect::new(dock.x, dock.y + offset, dock.width, 1)),
                line,
            );
        }
        let visible = |row: usize| u16::try_from(row).ok().filter(|row| *row < dock.height);
        if numbered {
            for (index, row) in self.action_rows.iter().enumerate() {
                if let Some(row) = visible(*row) {
                    frame.push_hit(
                        HitRegion::new(
                            format!("permission-action:{index}"),
                            Rect::new(dock.x, dock.y + row, dock.width, 1),
                        )
                        .role(HitRole::Action)
                        .layer(20),
                    );
                }
            }
        } else if let Some(row) = self.action_rows.first().copied().and_then(visible) {
            let mut x = dock.x;
            for (index, span) in self.rows[usize::from(row)].spans.iter().enumerate() {
                let width = u16::try_from(display_width(&span.content)).unwrap_or(u16::MAX);
                if index % 2 == 1 {
                    frame.push_hit(
                        HitRegion::new(
                            format!("permission-action:{}", index / 2),
                            Rect::new(x, dock.y + row, width, 1),
                        )
                        .role(HitRole::Action)
                        .layer(20),
                    );
                }
                x = x.saturating_add(width);
            }
        }
    }
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
        LabeledDetailsComponent::new("permission.details", &detail_items)
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
            "tab/←/→ choose · enter {} · esc deny{}",
            input.state.focused_label(),
            if matches!(
                input.state.origin(),
                super::permission_dialog::PermissionOrigin::Managed { .. }
            ) {
                " · ctrl+c deny and stop"
            } else {
                ""
            }
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
    if prefix_width >= max_width || max_width <= 2 {
        let mut spans = prefix.to_owned();
        spans.push(Span::styled(text, style));
        rows.extend(bmux_tui::text::wrap_line_bounded(
            &Line::from_spans(spans),
            bmux_tui::text::TextWrapGeometry::uniform(max_width),
            bmux_tui::text::TextWrap::Word,
        ));
        return;
    }
    let first_width = max_width.saturating_sub(prefix_width).max(1);
    let next_width = max_width.saturating_sub(2).max(1);
    for (index, chunk) in bmux_tui::text::wrap_line_bounded(
        &Line::raw(text),
        bmux_tui::text::TextWrapGeometry::with_continuation(first_width, next_width),
        bmux_tui::text::TextWrap::Word,
    )
    .into_iter()
    .enumerate()
    {
        let chunk = chunk.plain_text();
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
    frame: &mut PaintCx<'_, '_>,
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
    let mut actions = ActionRowState::new();
    actions.set_focused(Some(focused));
    let actions = std::cell::Cell::new(actions);
    let component = ActionRowComponent::new("permission.actions", &buttons, &actions)
        .spacing(2)
        .styles(action_styles(theme));
    let layout = component.layout(Constraints::tight(row_area.size()), &mut LayoutCx::new());
    frame.with_child(
        i32::from(row_area.x),
        i64::from(row_area.y),
        LocalRect::new(0, 0, row_area.width, row_area.height),
        |cx| component.paint(&layout, cx),
    );
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
            producer_plugin_id: None,
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

        render_permission_dialog(
            &state,
            &mut bmux_tui::paint::PaintCx::new(&mut frame),
            TuiTheme::for_agent("test", None, false),
        );

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
            producer_plugin_id: None,
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

        render_permission_dialog(
            &state,
            &mut bmux_tui::paint::PaintCx::new(&mut frame),
            TuiTheme::for_agent("test", None, false),
        );
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
            producer_plugin_id: None,
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

        render_permission_dialog(
            &state,
            &mut bmux_tui::paint::PaintCx::new(&mut frame),
            TuiTheme::for_agent("test", None, false),
        );
        let rendered = (0..30)
            .filter_map(|row| frame.buffer().row_symbols(row))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("2 of 3"));
        assert!(rendered.contains("Approve batch"));
        assert!(rendered.contains("Deny batch"));
    }

    fn shell_state() -> PermissionDialogState {
        PermissionDialogState::new(PermissionView {
            permission_id: "perm-1".to_owned(),
            session_id: Some(SessionId(Uuid::nil())),
            tool_call_id: "call-1".to_owned(),
            tool_name: "shell.run".to_owned(),
            producer_plugin_id: None,
            arguments_json: r#"{"command":"cargo check --workspace"}"#.to_owned(),
            batch: None,
            agent_id: "build".to_owned(),
            title: Some("Bash command".to_owned()),
            policy_source: None,
            detail: None,
            resolved: false,
            approved: None,
            can_remember: false,
        })
    }

    fn docked(numbered: bool, width: u16) -> (Vec<String>, Vec<bmux_tui::hit::HitRegion>, u16) {
        let state = shell_state();
        let theme = TuiTheme::for_agent("test", None, false);
        let dialog = super::DockedDialog::new(&state, width, theme, numbered);
        let height = dialog.height();
        let dock = bmux_tui::geometry::Rect::new(0, 2, width, height);
        let mut buffer = Buffer::empty(bmux_tui::geometry::Rect::new(0, 0, width, height + 2));
        let mut frame = bmux_tui::frame::Frame::new(&mut buffer);
        dialog.paint(
            dock,
            &mut bmux_tui::paint::PaintCx::new(&mut frame),
            theme,
            numbered,
        );
        let hits = frame.hits().regions().to_vec();
        let rows = (2..height + 2)
            .filter_map(|row| frame.buffer().row_symbols(row))
            .map(|row| row.trim_end().to_owned())
            .collect();
        (rows, hits, height)
    }

    #[test]
    fn docked_numbered_dialog_lists_one_action_per_row_with_matching_hits() {
        let (rows, hits, height) = docked(true, 60);
        assert_eq!(rows.len(), usize::from(height));
        assert_eq!(rows[0], "─".repeat(60));
        assert_eq!(rows[1], " Bash command");
        assert!(
            rows.contains(&" cargo check --workspace".to_owned()),
            "{rows:?}"
        );
        let approve = rows
            .iter()
            .position(|row| row == " ❯ 1. Approve")
            .expect("first action");
        assert_eq!(rows[approve + 1], "   2. Deny");
        assert_eq!(hits.len(), 2);
        for (index, hit) in hits.iter().enumerate() {
            assert_eq!(hit.id.as_str(), format!("permission-action:{index}"));
            assert_eq!(hit.area.y, 2 + u16::try_from(approve + index).expect("row"));
        }
    }

    #[test]
    fn docked_buttons_register_one_hit_per_button_on_their_row() {
        let (rows, hits, _) = docked(false, 60);
        let row = rows
            .iter()
            .position(|row| row.contains("[ Approve ]"))
            .expect("buttons");
        assert_eq!(hits.len(), 2);
        for hit in &hits {
            let y = usize::from(hit.area.y - 2);
            assert_eq!(y, row);
            let text: String = rows[row]
                .chars()
                .skip(usize::from(hit.area.x))
                .take(usize::from(hit.area.width))
                .collect();
            assert!(text.starts_with("[ ") && text.ends_with(" ]"), "{text:?}");
        }
    }

    #[test]
    fn docked_dialogs_stay_inside_narrow_docks() {
        for width in [1_u16, 4, 12] {
            let (rows, hits, _) = docked(true, width);
            for row in &rows {
                assert!(
                    bmux_tui::text_width::display_width(row) <= usize::from(width),
                    "{row:?}"
                );
            }
            for hit in hits {
                assert!(hit.area.right() <= width);
            }
        }
    }
}
