//! TUI session fork/clone dialog rendering.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::input::TextInput;
use bmux_tui::prelude::{Line, Span, Style, Widget};
use bmux_tui::style::{Color, Modifier};
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing, ModalTheme};

use super::session_fork_dialog::{SessionForkDialog, SessionForkDialogFocus};

const MODAL_BG: Color = Color::Black;

/// Render the session fork/clone dialog.
pub fn render_dialog(dialog: &mut SessionForkDialog, frame: &mut Frame<'_>) {
    let modal = modal_frame();
    modal.render(frame.area(), frame);
    let content = modal.content_area(frame.area());
    let mut row = content.y;

    render_line(
        &field_line(
            "Mode",
            dialog.mode().label(),
            dialog.focus() == SessionForkDialogFocus::Mode,
        ),
        &modal,
        content,
        &mut row,
        frame,
    );
    render_name_field(dialog, &modal, content, &mut row, frame);
    render_line(
        &field_line(
            "Switch after create",
            bool_label(dialog.switch_after_create()),
            dialog.focus() == SessionForkDialogFocus::SwitchAfterCreate,
        ),
        &modal,
        content,
        &mut row,
        frame,
    );
    render_line(
        &field_line(
            "Install returned draft",
            bool_label(dialog.install_draft()),
            dialog.focus() == SessionForkDialogFocus::InstallDraft,
        ),
        &modal,
        content,
        &mut row,
        frame,
    );
    render_line(&help_line(), &modal, content, &mut row, frame);
    render_line(
        &Line::from_spans(vec![Span::styled(
            dialog.status().to_owned(),
            Style::new().fg(Color::BrightBlack).bg(MODAL_BG),
        )]),
        &modal,
        content,
        &mut row,
        frame,
    );
}

fn modal_frame() -> ModalFrame {
    ModalFrame::new(
        ModalSizing::new(Size::new(62, 12), Size::new(84, 14), Insets::all(4)),
        ModalTheme::dark(Color::Cyan),
    )
    .title(" Fork / clone session ")
    .padding(Insets::new(1, 2, 1, 2))
    .placement(ModalPlacement::UpperThird)
}

fn render_name_field(
    dialog: &mut SessionForkDialog,
    modal: &ModalFrame,
    content: Rect,
    row: &mut u16,
    frame: &mut Frame<'_>,
) {
    if *row >= content.bottom() {
        return;
    }
    let label = "Name: ";
    let label_width = u16::try_from(label.len()).unwrap_or(u16::MAX);
    let line_area = Rect::new(content.x, *row, content.width, 1);
    modal.render_line(
        line_area,
        &Line::from_spans(vec![Span::styled(
            label,
            Style::new().add_modifier(Modifier::BOLD).bg(MODAL_BG),
        )]),
        frame,
    );
    let input_area = Rect::new(
        content.x.saturating_add(label_width),
        *row,
        content.width.saturating_sub(label_width),
        1,
    );
    dialog.set_name_content_area(input_area);
    let focused = dialog.focus() == SessionForkDialogFocus::Name;
    TextInput::new(dialog.name().buffer())
        .style(if focused {
            Style::new().fg(Color::Yellow).bg(MODAL_BG)
        } else {
            Style::new().fg(Color::White).bg(MODAL_BG)
        })
        .selection_style(Style::new().fg(Color::Black).bg(Color::Yellow))
        .vertical_scroll(dialog.name().vertical_scroll())
        .cursor_visible(focused)
        .render(input_area, frame);
    *row = row.saturating_add(1);
}

fn render_line(
    line: &Line,
    modal: &ModalFrame,
    content: Rect,
    row: &mut u16,
    frame: &mut Frame<'_>,
) {
    if *row >= content.bottom() {
        return;
    }
    modal.render_line(Rect::new(content.x, *row, content.width, 1), line, frame);
    *row = row.saturating_add(1);
}

fn field_line(label: &str, value: &str, focused: bool) -> Line {
    let style = if focused {
        Style::new().fg(Color::Yellow).bg(MODAL_BG)
    } else {
        Style::new().fg(Color::White).bg(MODAL_BG)
    };
    Line::from_spans(vec![
        Span::styled(format!("{label}: "), style.add_modifier(Modifier::BOLD)),
        Span::styled(value.to_owned(), style),
    ])
}

fn help_line() -> Line {
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
    ])
}

const fn bool_label(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}
