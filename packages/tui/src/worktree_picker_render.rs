//! TUI worktree picker rendering.

use bmux_tui::frame::Frame;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};

use super::picker_render::{
    picker_list_area, render_picker_chrome, render_picker_list, render_picker_status,
};
use super::worktree_picker::WorktreePickerApp;

/// Render the worktree picker.
pub fn render_picker(app: &mut WorktreePickerApp, frame: &mut Frame<'_>) {
    let Some((inner, list_y)) = render_picker_chrome(
        " Worktrees ",
        &header_line(),
        app.filter_mut(),
        "Filter worktrees",
        frame,
    ) else {
        return;
    };

    let bottom_y = render_picker_status(
        inner,
        app.status(),
        Style::new().fg(Color::BrightBlack),
        frame,
    );
    let Some(list_area) = picker_list_area(inner, list_y, bottom_y) else {
        return;
    };
    let items = app.list_items();
    render_picker_list(&items, app.list_state_mut(), list_area, frame);
}

fn header_line() -> Line {
    Line::from_spans(vec![
        Span::styled("Bcode worktrees", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw("  Enter attaches current session  Esc cancels"),
    ])
}
