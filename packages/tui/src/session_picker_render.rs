//! TUI session picker rendering.

use bmux_tui::frame::Frame;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::Modifier;

use super::picker_render::{
    picker_list_area, render_picker_chrome, render_picker_list, render_picker_status,
};
use super::render::TuiTheme;
use super::session_picker::{SessionPickerApp, SessionPickerMode};

/// Render the session picker.
pub fn render_picker(app: &mut SessionPickerApp, frame: &mut Frame<'_>, theme: TuiTheme) {
    let mode = app.mode();
    let Some((inner, list_y)) = render_picker_chrome(
        " Sessions ",
        &header_line(mode),
        app.active_input_mut(),
        input_placeholder(mode),
        frame,
        theme,
    ) else {
        return;
    };

    let bottom_y = render_picker_status(inner, app.status(), theme.muted, frame, theme);
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
                &Line::from_spans(vec![Span::styled(warning_text, theme.muted)]),
                Style::new(),
            );
        }
    }
    let Some(list_area) = picker_list_area(inner, list_y, bottom_y) else {
        return;
    };
    let items = app.list_items(theme.muted);
    render_picker_list(
        &items,
        app.list_render_state(list_area.height),
        list_area,
        frame,
        theme,
    );
}

const fn input_placeholder(mode: SessionPickerMode) -> &'static str {
    match mode {
        SessionPickerMode::Filter | SessionPickerMode::DeleteConfirm => "Filter sessions",
        SessionPickerMode::TranscriptSearch => "Transcript query",
        SessionPickerMode::Rename => "New session name",
    }
}

fn header_line(mode: SessionPickerMode) -> Line {
    let help = match mode {
        SessionPickerMode::Filter => {
            "  Enter selects/imports  Ctrl-F searches transcripts (deep:/content:/provider:)  Ctrl-N creates  Ctrl-R renames  Ctrl-D deletes  Esc cancels"
        }
        SessionPickerMode::Rename => "  Enter saves rename  Esc cancels",
        SessionPickerMode::DeleteConfirm => "  Y confirms delete  N/Esc cancels",
        SessionPickerMode::TranscriptSearch => {
            "  Enter opens canonical result  Up/Down select  Esc returns to sessions"
        }
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
