//! TUI session picker rendering.

use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::Modifier;

use super::picker_render::{
    picker_list_area, render_picker_chrome, render_picker_list_with_selection, render_picker_status,
};
use super::render::TuiTheme;
use super::session_picker::{SessionPickerApp, SessionPickerMode};

/// Render the session picker.
pub fn render_picker(app: &mut SessionPickerApp, frame: &mut PaintCx<'_, '_>, theme: TuiTheme) {
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
                LocalRect::terminal(bmux_tui::geometry::Rect::new(
                    inner.x.saturating_add(1),
                    warning_y,
                    inner.width.saturating_sub(2),
                    1,
                )),
                &Line::from_spans(vec![Span::styled(warning_text, theme.muted)]),
                Style::new(),
            );
        }
    }
    let preview_reserved = if mode == SessionPickerMode::TranscriptSearch {
        let preview_y = bottom_y.saturating_sub(1);
        if preview_y > list_y {
            frame.write_line_with_fallback_style(
                LocalRect::terminal(bmux_tui::geometry::Rect::new(
                    inner.x.saturating_add(1),
                    preview_y,
                    inner.width.saturating_sub(2),
                    1,
                )),
                &Line::from_spans(vec![
                    Span::styled("Preview: ", Style::new().add_modifier(Modifier::BOLD)),
                    Span::styled(app.search_preview(), theme.muted),
                ]),
                Style::new(),
            );
            1
        } else {
            0
        }
    } else {
        0
    };
    let bottom_y = bottom_y.saturating_sub(preview_reserved);
    let Some(list_area) = picker_list_area(inner, list_y, bottom_y) else {
        return;
    };
    let items = app.list_items(theme.muted);
    render_picker_list_with_selection(
        &items,
        app.list_render_state(list_area.height),
        list_area,
        frame,
        theme,
        // Keep the row on the raised surface; the BMUX selection marker and
        // accent text identify it without a competing full-width color block.
        theme.raised.patch(theme.focused),
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
            "  ↑↓ browse  ·  Enter open  ·  Ctrl-F search  ·  Ctrl-N new  ·  Ctrl-R rename  ·  Ctrl-D delete  ·  Esc close"
        }
        SessionPickerMode::Rename => "  Rename  ·  Enter save  ·  Esc cancel",
        SessionPickerMode::DeleteConfirm => "  Delete session?  ·  Y confirm  ·  N/Esc cancel",
        SessionPickerMode::TranscriptSearch => {
            "  Enter search/open  ·  ↑↓ browse  ·  Alt-M mode  ·  Alt-D deep  ·  Alt-S sort  ·  Alt-N next  ·  Alt-I inventory  ·  Alt-G migrate  ·  Alt-B backfill  ·  Alt-X cancel  ·  ? details  ·  Esc sessions"
        }
    };
    Line::from_spans(vec![
        Span::styled("SESSIONS", Style::new().add_modifier(Modifier::BOLD)),
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

#[cfg(test)]
mod tests {
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::{Point, Rect};

    use super::{header_line, render_picker};
    use crate::render::TuiTheme;
    use crate::session_picker::{SessionPickerApp, SessionPickerMode};
    use bcode_session_models::{SessionId, SessionSummary, SessionTitleSource};

    fn buffer_text(buffer: &Buffer, area: Rect) -> String {
        (area.y..area.y.saturating_add(area.height))
            .flat_map(|y| {
                (area.x..area.x.saturating_add(area.width)).filter_map(move |x| {
                    buffer
                        .get(bmux_tui::geometry::Point::new(x, y))
                        .map(|cell| cell.symbol.as_str())
                })
            })
            .collect()
    }

    #[test]
    fn search_help_is_textual_and_names_separate_maintenance_actions() {
        let line = header_line(SessionPickerMode::TranscriptSearch);
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_str())
            .collect::<String>();
        for expected in [
            "Alt-I inventory",
            "Alt-G migrate",
            "Alt-B backfill",
            "Alt-X cancel",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
    }

    #[test]
    fn selected_session_uses_accent_on_raised_surface_across_themes() {
        for theme_id in ["terminal-native", "bcode-dark", "bcode-light"] {
            let theme = TuiTheme::for_theme_id(theme_id);
            let session = SessionSummary {
                id: SessionId::new(),
                name: Some("Design review".to_owned()),
                explicit_name: None,
                derived_title: None,
                title_source: SessionTitleSource::Explicit,
                client_count: 0,
                created_at_ms: 1,
                updated_at_ms: 1,
                working_directory: "/tmp/project".into(),
                import: None,
                execution: None,
                location: None,
            };
            let mut app = SessionPickerApp::new(vec![session]);
            let area = Rect::new(0, 0, 90, 15);
            let mut buffer = Buffer::empty(area);
            let mut frame = Frame::new(&mut buffer);
            render_picker(
                &mut app,
                &mut bmux_tui::paint::PaintCx::new(&mut frame),
                theme,
            );
            let text = buffer_text(frame.buffer(), area);
            assert!(text.contains("Design review"), "{theme_id}");
            assert!(text.contains("/tmp/project"), "{theme_id}");
            let marker = frame
                .buffer()
                .get(Point::new(2, 6))
                .expect("selection marker");
            assert_eq!(marker.style.bg, theme.raised.bg, "{theme_id}");
            assert_eq!(marker.style.fg, theme.focused.fg, "{theme_id}");
        }
    }

    #[test]
    fn search_renderer_remains_bounded_on_narrow_terminal() {
        let mut app = SessionPickerApp::new(Vec::new());
        app.start_transcript_search();
        app.set_status("query incomplete; compatibility blockers remain".to_owned());
        let area = Rect::new(0, 0, 24, 8);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        render_picker(
            &mut app,
            &mut bmux_tui::paint::PaintCx::new(&mut frame),
            TuiTheme::for_theme_id("terminal-native"),
        );
        let text = buffer_text(frame.buffer(), area);
        assert!(text.contains("Sessions"));
        assert_eq!(frame.buffer().area(), area);
    }
}
