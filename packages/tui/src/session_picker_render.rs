//! TUI session picker rendering.

use bmux_text_edit::TextEditBuffer;
use bmux_tui::frame::Frame;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};

use super::picker_render::{
    picker_list_area, render_picker_chrome, render_picker_list, render_picker_status,
};
use super::session_picker::{SessionPickerApp, SessionPickerMode};

/// Render the session picker.
pub fn render_picker(app: &mut SessionPickerApp, frame: &mut Frame<'_>) {
    let Some((inner, list_y)) = render_picker_chrome(
        " Sessions ",
        &header_line(app.mode()),
        filter_input(app),
        input_placeholder(app.mode()),
        frame,
    ) else {
        return;
    };

    let bottom_y = render_picker_status(inner, app.status(), status_style(app.mode()), frame);
    if let Some((session, warnings)) = app.last_import()
        && !warnings.is_empty()
    {
        let warning_text = format_import_warnings(session, warnings);
        let warning_y = bottom_y.saturating_sub(1);
        if warning_y > list_y {
            frame.write_line_with_fallback_style(
                bmux_tui::geometry::Rect::new(
                    inner.x.saturating_add(1),
                    warning_y,
                    inner.width.saturating_sub(2),
                    1,
                ),
                &Line::from_spans(vec![Span::styled(
                    warning_text,
                    Style::new().fg(Color::Yellow),
                )]),
                Style::new(),
            );
        }
    }
    let Some(list_area) = picker_list_area(inner, list_y, bottom_y) else {
        return;
    };
    let items = app.list_items();
    render_picker_list(&items, app.list_state_mut(), list_area, frame);
}

const fn filter_input(app: &SessionPickerApp) -> &TextEditBuffer {
    match app.mode() {
        SessionPickerMode::Filter | SessionPickerMode::DeleteConfirm => app.filter(),
        SessionPickerMode::Rename => app.rename(),
    }
}

const fn input_placeholder(mode: SessionPickerMode) -> &'static str {
    match mode {
        SessionPickerMode::Filter | SessionPickerMode::DeleteConfirm => "Filter sessions",
        SessionPickerMode::Rename => "New session name",
    }
}

fn header_line(mode: SessionPickerMode) -> Line {
    let help = match mode {
        SessionPickerMode::Filter => {
            "  Enter selects/imports  Ctrl-N creates  Ctrl-R renames  Ctrl-D deletes  Esc cancels/dismisses"
        }
        SessionPickerMode::Rename => "  Enter saves rename  Esc cancels",
        SessionPickerMode::DeleteConfirm => "  Y confirms delete  N/Esc cancels",
    };
    Line::from_spans(vec![
        Span::styled("Bcode sessions", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(help),
    ])
}

fn format_import_warnings(
    session: &bcode_session_models::SessionSummary,
    warnings: &[bcode_ipc::SessionImportWarning],
) -> String {
    let source = session
        .import
        .as_ref()
        .map_or("external", |import| import.source_id.as_str());
    let details = warnings
        .iter()
        .take(3)
        .map(|warning| {
            warning.count.map_or_else(
                || warning.message.clone(),
                |count| format!("{} ({count})", warning.message),
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let suffix = if warnings.len() > 3 {
        format!("; +{} more", warnings.len() - 3)
    } else {
        String::new()
    };
    format!(
        "Imported [{source}] with {} warnings: {details}{suffix}. Esc dismisses.",
        warnings.len()
    )
}

const fn status_style(mode: SessionPickerMode) -> Style {
    match mode {
        SessionPickerMode::DeleteConfirm => Style::new().fg(Color::Red),
        SessionPickerMode::Filter | SessionPickerMode::Rename => {
            Style::new().fg(Color::BrightBlack)
        }
    }
}
