//! TUI worktree create dialog rendering.

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
use super::wt_create_dialog::{WorktreeCreateDialog, WorktreeCreateFocus};

/// Render the worktree create dialog.
pub fn render_dialog(
    dialog: &mut WorktreeCreateDialog,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
) {
    let modal = modal_frame(theme);
    let area = Rect::new(0, 0, frame.area().width, frame.area().height);
    let content = modal.content_area(area);
    let shell =
        ModalFrameComponent::new("wt_create_dialog_render", modal.clone(), TextBlock::new(""));
    let layout = shell.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    shell.paint(&layout, frame);
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
    _modal: &ModalFrame,
    content: Rect,
    row: &mut u16,
    frame: &mut PaintCx<'_, '_>,
    theme: TuiTheme,
) {
    if *row >= content.bottom() {
        return;
    }
    let label = "Name: ";
    let label_width = u16::try_from(label.len()).unwrap_or(u16::MAX);
    let line_area = Rect::new(content.x, *row, content.width, 1);
    frame.write_line(
        LocalRect::terminal(line_area),
        &Line::from_spans(vec![Span::styled(
            label,
            Style::new().add_modifier(Modifier::BOLD),
        )]),
    );
    let input_area = Rect::new(
        content.x.saturating_add(label_width),
        *row,
        content.width.saturating_sub(label_width),
        1,
    );
    dialog.set_name_content_area(input_area);
    let focused = dialog.focus() == WorktreeCreateFocus::Name;
    let retained = std::cell::RefCell::new(dialog.name().clone());
    let policy = super::text_input_flow::single_line_policy();
    let editor = TextInputComponent::new("wt_create_dialog_render.editor", &retained, &policy)
        .style(if focused { theme.selection } else { theme.text })
        .selection_style(theme.selection)
        .focused(focused);
    let layout = editor.layout(Constraints::tight(input_area.size()), &mut LayoutCx::new());
    frame.with_child(
        i32::from(input_area.x),
        i64::from(input_area.y),
        LocalRect::new(0, 0, input_area.width, input_area.height),
        |cx| editor.paint(&layout, cx),
    );
    *row = row.saturating_add(1);
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
