//! TUI Ralph loop start dialog rendering.

use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::input::TextInput;
use bmux_tui::prelude::{Line, Span, Style, Widget};
use bmux_tui::style::Modifier;
use bmux_tui_components::modal_frame::{ModalFrame, ModalPlacement, ModalSizing};

use super::ralph_start_dialog::{RalphStartDialog, RalphStartDialogField};
use super::render::TuiTheme;

/// Render the Ralph loop start dialog.
pub fn render_dialog(dialog: &mut RalphStartDialog, frame: &mut Frame<'_>, theme: TuiTheme) {
    let modal = modal_frame(theme);
    modal.render(frame.area(), frame);
    let content = modal.content_area(frame.area());
    let mut row = content.y;
    render_input_field(
        dialog,
        &modal,
        content,
        &mut row,
        frame,
        RalphStartDialogField::LoopName,
        "Ralph loop",
        theme,
    );
    render_input_field(
        dialog,
        &modal,
        content,
        &mut row,
        frame,
        RalphStartDialogField::WorkAreaPath,
        "Work area",
        theme,
    );
    render_input_field(
        dialog,
        &modal,
        content,
        &mut row,
        frame,
        RalphStartDialogField::Branch,
        "Branch",
        theme,
    );
    render_input_field(
        dialog,
        &modal,
        content,
        &mut row,
        frame,
        RalphStartDialogField::ValidationCommands,
        "Validation",
        theme,
    );
    render_line(&help_line(theme), &modal, content, &mut row, frame);
    render_line(
        &setup_explanation_line(theme),
        &modal,
        content,
        &mut row,
        frame,
    );
    let status = Line::from_spans(vec![Span::styled(dialog.status().to_owned(), theme.muted)]);
    render_line(&status, &modal, content, &mut row, frame);
}

fn modal_frame(theme: TuiTheme) -> ModalFrame {
    ModalFrame::new(
        ModalSizing::new(Size::new(76, 12), Size::new(112, 14), Insets::all(4)),
        theme.modal_theme(),
    )
    .title(" Start Ralph loop ")
    .padding(Insets::new(1, 2, 1, 2))
    .placement(ModalPlacement::UpperThird)
}

#[allow(clippy::too_many_arguments)]
fn render_input_field(
    dialog: &mut RalphStartDialog,
    modal: &ModalFrame,
    content: Rect,
    row: &mut u16,
    frame: &mut Frame<'_>,
    field: RalphStartDialogField,
    label: &str,
    theme: TuiTheme,
) {
    if *row >= content.bottom() {
        return;
    }
    let focused = dialog.focused_field() == field;
    let marker = if focused { ">" } else { " " };
    let label = format!("{marker} {label}: ");
    let label_width = u16::try_from(label.len()).unwrap_or(u16::MAX);
    let line_area = Rect::new(content.x, *row, content.width, 1);
    modal.render_line(
        line_area,
        &Line::from_spans(vec![Span::styled(
            label.as_str(),
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
    let input = match field {
        RalphStartDialogField::LoopName => {
            dialog.set_loop_name_content_area(input_area);
            dialog.loop_name()
        }
        RalphStartDialogField::WorkAreaPath => {
            dialog.set_work_area_path_content_area(input_area);
            dialog.work_area_path()
        }
        RalphStartDialogField::Branch => {
            dialog.set_branch_content_area(input_area);
            dialog.branch()
        }
        RalphStartDialogField::ValidationCommands => {
            dialog.set_validation_commands_content_area(input_area);
            dialog.validation_commands()
        }
    };
    TextInput::new(input.buffer())
        .style(theme.selection)
        .selection_style(theme.selection)
        .vertical_scroll(input.vertical_scroll())
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

fn setup_explanation_line(theme: TuiTheme) -> Line {
    Line::from_spans(vec![Span::styled(
        "Creates docs/worktree/session/validation. After setup: review docs → prepare run → approve/start.",
        theme.muted,
    )])
}

fn help_line(theme: TuiTheme) -> Line {
    Line::from_spans(vec![
        Span::styled("Enter", Style::new().add_modifier(Modifier::BOLD)),
        Span::styled(" start  ", theme.text),
        Span::styled("Tab", Style::new().add_modifier(Modifier::BOLD)),
        Span::styled(" field  ", theme.text),
        Span::styled("Esc", Style::new().add_modifier(Modifier::BOLD)),
        Span::styled(" cancel", theme.text),
    ])
}
