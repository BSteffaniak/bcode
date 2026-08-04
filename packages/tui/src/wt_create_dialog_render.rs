//! TUI worktree create dialog rendering.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::input::TextInput;
use bmux_tui::prelude::{Line, Span, Style, Widget};
use bmux_tui::style::Modifier;
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing};

use super::render::TuiTheme;
use super::wt_create_dialog::{WorktreeCreateDialog, WorktreeCreateFocus};

/// Render the worktree create dialog.
pub fn render_dialog(dialog: &mut WorktreeCreateDialog, frame: &mut Frame<'_>, theme: TuiTheme) {
    let modal = modal_frame(theme);
    modal.render(frame.area(), frame);
    let content = modal.content_area(frame.area());
    let mut row = content.y;
    render_name_field(dialog, &modal, content, &mut row, frame, theme);
    let target = field_line(
        "Target",
        dialog.target().label(),
        dialog.focus() == WorktreeCreateFocus::Target,
        theme,
    );
    render_line(&target, &modal, content, &mut row, frame);
    let base = field_line(
        "Base",
        dialog.base().label(),
        dialog.focus() == WorktreeCreateFocus::Base,
        theme,
    );
    render_line(&base, &modal, content, &mut row, frame);
    let help = help_line(theme);
    render_line(&help, &modal, content, &mut row, frame);
    let status = Line::from_spans(vec![Span::styled(dialog.status().to_owned(), theme.muted)]);
    render_line(&status, &modal, content, &mut row, frame);
}

fn modal_frame(theme: TuiTheme) -> ModalFrame {
    ModalFrame::new(
        ModalSizing::new(Size::new(56, 10), Size::new(80, 12), Insets::all(4)),
        theme.modal_theme(),
    )
    .title(" Create worktree ")
    .padding(Insets::new(1, 2, 1, 2))
    .placement(ModalPlacement::UpperThird)
}

fn render_name_field(
    dialog: &mut WorktreeCreateDialog,
    modal: &ModalFrame,
    content: Rect,
    row: &mut u16,
    frame: &mut Frame<'_>,
    theme: TuiTheme,
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
            Style::new().add_modifier(Modifier::BOLD),
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
    let focused = dialog.focus() == WorktreeCreateFocus::Name;
    TextInput::new(dialog.name().buffer())
        .style(if focused { theme.selection } else { theme.text })
        .selection_style(theme.selection)
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

fn help_line(theme: TuiTheme) -> Line {
    Line::from_spans(vec![
        Span::styled("Enter", Style::new().add_modifier(Modifier::BOLD)),
        Span::styled(" create  ", theme.text),
        Span::styled("Tab", Style::new().add_modifier(Modifier::BOLD)),
        Span::styled(" field  ", theme.text),
        Span::styled("←/→", Style::new().add_modifier(Modifier::BOLD)),
        Span::styled(" value  ", theme.text),
        Span::styled("Esc", Style::new().add_modifier(Modifier::BOLD)),
        Span::styled(" cancel", theme.text),
    ])
}

fn field_line(label: &str, value: &str, focused: bool, theme: TuiTheme) -> Line {
    let style = if focused { theme.selection } else { theme.text };
    Line::from_spans(vec![
        Span::styled(
            format!("{label}: "),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Span::styled(value.to_owned(), style),
    ])
}
