//! TUI session working-directory dialog rendering.

use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::composition::TextBlock;
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::Modifier;
use bmux_tui_components::modal_frame::{
    ModalFrame, ModalFrameComponent, ModalPlacement, ModalSizing,
};
use bmux_tui_components::text_input::TextInputComponent;

use super::render::TuiTheme;
use super::working_directory_dialog::WorkingDirectoryDialog;

/// Render the working-directory dialog.
pub fn render_dialog(
    dialog: &mut WorkingDirectoryDialog,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
) {
    let modal = ModalFrame::new(
        ModalSizing::new(Size::new(56, 8), Size::new(80, 10), Insets::all(4)),
        theme.modal_theme(),
    )
    .title(" Change working directory ")
    .padding(Insets::new(1, 1, 1, 1))
    .placement(ModalPlacement::Centered);
    let area = Rect::new(0, 0, frame.area().width, frame.area().height);
    let content = modal.content_area(area);
    let shell = ModalFrameComponent::new(
        "working_directory_dialog_render",
        modal.clone(),
        TextBlock::new(""),
    );
    let layout = shell.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    shell.paint(&layout, frame);
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
    let retained = std::cell::RefCell::new(dialog.path().clone());
    let policy = super::text_input_flow::single_line_policy();
    let editor =
        TextInputComponent::new("working_directory_dialog_render.editor", &retained, &policy)
            .style(theme.selection)
            .selection_style(theme.selection)
            .focused(true);
    let layout = editor.layout(Constraints::tight(input_area.size()), &mut LayoutCx::new());
    frame.with_child(
        i32::from(input_area.x),
        i64::from(input_area.y),
        LocalRect::new(0, 0, input_area.width, input_area.height),
        |cx| editor.paint(&layout, cx),
    );
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
    _modal: &ModalFrame,
    content: Rect,
    row: &mut u16,
    frame: &mut PaintCx<'_, '_>,
) {
    if *row >= content.bottom() {
        return;
    }
    frame.write_line(
        LocalRect::terminal(Rect::new(content.x, *row, content.width, 1)),
        line,
    );
    *row = row.saturating_add(1);
}
