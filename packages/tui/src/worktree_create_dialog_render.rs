//! TUI worktree create dialog rendering.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing, ModalTheme};

use super::worktree_create_dialog::{WorktreeCreateDialog, WorktreeCreateFocus};

const MODAL_BG: Color = Color::Black;

/// Render the worktree create dialog.
pub fn render_dialog(dialog: &WorktreeCreateDialog, frame: &mut Frame<'_>) {
    let modal = modal_frame();
    modal.render(frame.area(), frame);
    let content = modal.content_area(frame.area());
    let rows = rows(dialog);
    for (row_index, line) in rows.iter().take(usize::from(content.height)).enumerate() {
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
}

fn modal_frame() -> ModalFrame {
    ModalFrame::new(
        ModalSizing::new(Size::new(56, 10), Size::new(80, 12), Insets::all(4)),
        ModalTheme::dark(Color::Cyan),
    )
    .title(" Create worktree ")
    .padding(Insets::new(1, 2, 1, 2))
    .placement(ModalPlacement::UpperThird)
}

fn rows(dialog: &WorktreeCreateDialog) -> Vec<Line> {
    vec![
        field_line(
            "Name",
            dialog.name().text(),
            dialog.focus() == WorktreeCreateFocus::Name,
        ),
        field_line(
            "Target",
            dialog.target().label(),
            dialog.focus() == WorktreeCreateFocus::Target,
        ),
        field_line(
            "Base",
            dialog.base().label(),
            dialog.focus() == WorktreeCreateFocus::Base,
        ),
        Line::from_spans(vec![
            Span::styled(
                "Enter",
                Style::new().add_modifier(Modifier::BOLD).bg(MODAL_BG),
            ),
            Span::styled(" create  ", Style::new().bg(MODAL_BG)),
            Span::styled(
                "Tab",
                Style::new().add_modifier(Modifier::BOLD).bg(MODAL_BG),
            ),
            Span::styled(" field  ", Style::new().bg(MODAL_BG)),
            Span::styled(
                "←/→",
                Style::new().add_modifier(Modifier::BOLD).bg(MODAL_BG),
            ),
            Span::styled(" value  ", Style::new().bg(MODAL_BG)),
            Span::styled(
                "Esc",
                Style::new().add_modifier(Modifier::BOLD).bg(MODAL_BG),
            ),
            Span::styled(" cancel", Style::new().bg(MODAL_BG)),
        ]),
        Line::from_spans(vec![Span::styled(
            dialog.status().to_owned(),
            Style::new().fg(Color::BrightBlack).bg(MODAL_BG),
        )]),
    ]
}

fn field_line(label: &str, value: &str, focused: bool) -> Line {
    let style = if focused {
        Style::new().fg(Color::Yellow).bg(MODAL_BG)
    } else {
        Style::new().fg(Color::White).bg(MODAL_BG)
    };
    Line::from_spans(vec![
        Span::styled(
            format!("{label}: "),
            Style::new().add_modifier(Modifier::BOLD).bg(MODAL_BG),
        ),
        Span::styled(value.to_owned(), style),
    ])
}
