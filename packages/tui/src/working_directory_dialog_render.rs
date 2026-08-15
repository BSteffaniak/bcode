//! TUI session working-directory dialog rendering.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::input::TextInput;
use bmux_tui::prelude::{Line, Span, Style, Widget};
use bmux_tui::style::Modifier;
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing};

use super::render::TuiTheme;
use super::working_directory_dialog::WorkingDirectoryDialog;

/// Render the working-directory dialog.
pub fn render_dialog(dialog: &mut WorkingDirectoryDialog, frame: &mut Frame<'_>, theme: TuiTheme) {
    let modal = ModalFrame::new(
        ModalSizing::new(Size::new(56, 8), Size::new(80, 10), Insets::all(4)),
        theme.modal_theme(),
    )
    .title(" Change working directory ")
    .padding(Insets::new(1, 1, 1, 1))
    .placement(ModalPlacement::Centered);
    modal.render(frame.area(), frame);
    let content = modal.content_area(frame.area());
    let mut row = content.y;

    render_line(
        &Line::from_spans(vec![
            Span::styled("Path: ", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw("Enter an absolute path or one relative to the current directory"),
        ]),
        &modal,
        content,
        &mut row,
        frame,
    );
    let input_area = Rect::new(content.x, row, content.width, 1);
    dialog.set_path_content_area(input_area);
    TextInput::new(dialog.path().buffer())
        .style(theme.selection)
        .selection_style(theme.selection)
        .vertical_scroll(dialog.path().vertical_scroll())
        .cursor_visible(true)
        .render(input_area, frame);
    row = row.saturating_add(1);
    render_line(
        &Line::from_spans(vec![
            Span::styled("Enter", Style::new().add_modifier(Modifier::BOLD)),
            Span::styled(" apply  ", theme.text),
            Span::styled("Esc", Style::new().add_modifier(Modifier::BOLD)),
            Span::styled(" cancel", theme.text),
        ]),
        &modal,
        content,
        &mut row,
        frame,
    );
    render_line(
        &Line::from_spans(vec![Span::styled(dialog.status().to_owned(), theme.muted)]),
        &modal,
        content,
        &mut row,
        frame,
    );
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
