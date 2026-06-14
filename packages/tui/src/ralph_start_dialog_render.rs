//! TUI Ralph loop start dialog rendering.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::input::TextInput;
use bmux_tui::prelude::{Line, Span, Style, Widget};
use bmux_tui::style::{Color, Modifier};
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing, ModalTheme};

use super::ralph_start_dialog::RalphStartDialog;

const MODAL_BG: Color = Color::Black;

/// Render the Ralph loop start dialog.
pub fn render_dialog(dialog: &mut RalphStartDialog, frame: &mut Frame<'_>) {
    let modal = modal_frame();
    modal.render(frame.area(), frame);
    let content = modal.content_area(frame.area());
    let mut row = content.y;
    render_loop_name_field(dialog, &modal, content, &mut row, frame);
    render_line(&help_line(), &modal, content, &mut row, frame);
    let status = Line::from_spans(vec![Span::styled(
        dialog.status().to_owned(),
        Style::new().fg(Color::BrightBlack).bg(MODAL_BG),
    )]);
    render_line(&status, &modal, content, &mut row, frame);
}

fn modal_frame() -> ModalFrame {
    ModalFrame::new(
        ModalSizing::new(Size::new(60, 8), Size::new(88, 10), Insets::all(4)),
        ModalTheme::dark(Color::Cyan),
    )
    .title(" Start Ralph loop ")
    .padding(Insets::new(1, 2, 1, 2))
    .placement(ModalPlacement::UpperThird)
}

fn render_loop_name_field(
    dialog: &mut RalphStartDialog,
    modal: &ModalFrame,
    content: Rect,
    row: &mut u16,
    frame: &mut Frame<'_>,
) {
    if *row >= content.bottom() {
        return;
    }
    let label = "Ralph loop: ";
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
    dialog.set_loop_name_content_area(input_area);
    TextInput::new(dialog.loop_name().buffer())
        .style(Style::new().fg(Color::Yellow).bg(MODAL_BG))
        .selection_style(Style::new().fg(Color::Black).bg(Color::Yellow))
        .vertical_scroll(dialog.loop_name().vertical_scroll())
        .cursor_visible(true)
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

fn help_line() -> Line {
    Line::from_spans(vec![
        Span::styled(
            "Enter",
            Style::new().add_modifier(Modifier::BOLD).bg(MODAL_BG),
        ),
        Span::styled(" start  ", Style::new().bg(MODAL_BG)),
        Span::styled(
            "Esc",
            Style::new().add_modifier(Modifier::BOLD).bg(MODAL_BG),
        ),
        Span::styled(" cancel", Style::new().bg(MODAL_BG)),
    ])
}
