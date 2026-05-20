//! BMUX backend rendering.

use bmux_tui::chrome::{Border, Panel};
use bmux_tui::diff::{DiffFileList, DiffFileListState, DiffView, DiffViewMode, DiffViewState};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::input::TextInput;
use bmux_tui::prelude::{Line, Span, StatefulWidget, Style, Widget};
use bmux_tui::style::{Color, Modifier};
use bmux_tui::text_block::{TextBlock, TextWrap};

use super::app::{
    ActivityState, BmuxApp, PendingSubmission, PendingSubmissionState, TranscriptItem,
};

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Render one BMUX backend frame.
pub(super) fn render(app: &mut BmuxApp, frame: &mut Frame<'_>) {
    let area = frame.area();
    if area.is_empty() {
        return;
    }

    let header = Rect::new(area.x, area.y, area.width, 1);
    render_header(app, header, frame);

    let composer_height = area.height.clamp(3, 6);
    let composer = Rect::new(
        area.x,
        area.bottom().saturating_sub(composer_height),
        area.width,
        composer_height,
    );
    render_composer(app, composer, frame);

    let body_height = composer.y.saturating_sub(area.y.saturating_add(2));
    let body = Rect::new(area.x, area.y.saturating_add(1), area.width, body_height);
    let transcript_area = transcript_area_for_body(app, body);
    app.sync_transcript_scroll_max(max_transcript_scroll_offset(app, transcript_area));
    render_body(app, body, frame);

    let status = Rect::new(
        area.x,
        composer.y.saturating_sub(1),
        area.width,
        u16::from(composer.y > area.y.saturating_add(1)),
    );
    render_status(app, status, frame);
}

fn render_header(app: &BmuxApp, area: Rect, frame: &mut Frame<'_>) {
    let session = app
        .session_id()
        .map_or_else(|| String::from("new session"), |id| id.to_string());
    let line = Line::from_spans(vec![
        Span::styled("Bcode BMUX TUI", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(session, Style::new().fg(Color::BrightBlack)),
        Span::raw("  Esc/Ctrl-C exits"),
    ]);
    frame.write_line(area, &line);
}

fn render_body(app: &BmuxApp, area: Rect, frame: &mut Frame<'_>) {
    if area.is_empty() {
        return;
    }
    let transcript_area = transcript_area_for_body(app, area);
    render_transcript(app, transcript_area, frame);
    let diff_height = area.height.saturating_sub(transcript_area.height);
    if diff_height > 0 {
        let diff_area = Rect::new(area.x, transcript_area.bottom(), area.width, diff_height);
        render_changed_files(app, diff_area, frame);
    }
}

fn transcript_area_for_body(app: &BmuxApp, area: Rect) -> Rect {
    let diff_height = if app.changed_files().is_empty() {
        0
    } else {
        area.height.min(9)
    };
    Rect::new(
        area.x,
        area.y,
        area.width,
        area.height.saturating_sub(diff_height),
    )
}

fn max_transcript_scroll_offset(app: &BmuxApp, area: Rect) -> usize {
    if area.is_empty() || app.transcript().is_empty() && app.pending_submissions().is_empty() {
        return 0;
    }
    transcript_render_rows(app, area.width)
        .len()
        .saturating_sub(usize::from(area.height))
}

fn render_transcript(app: &BmuxApp, area: Rect, frame: &mut Frame<'_>) {
    if area.is_empty() {
        return;
    }
    if app.transcript().is_empty() && app.pending_submissions().is_empty() {
        TextBlock::new(
            "BMUX backend is attached. Composer submissions are sent to the active Bcode session; live transcript events will appear here.",
        )
        .wrap(TextWrap::Character)
        .render(area.inset(Insets::all(1)), frame);
        return;
    }

    let transcript_rows = transcript_render_rows(app, area.width);
    let end = transcript_rows
        .len()
        .saturating_sub(app.scroll_offset())
        .min(transcript_rows.len());
    let start = end.saturating_sub(usize::from(area.height));
    let mut y = area.y;
    for row in &transcript_rows[start..end] {
        if y >= area.bottom() {
            break;
        }
        frame.write_line(Rect::new(area.x, y, area.width, 1), row);
        y = y.saturating_add(1);
    }
}

fn render_changed_files(app: &BmuxApp, area: Rect, frame: &mut Frame<'_>) {
    let panel = Panel::new()
        .border(Border::single().style(Style::new().fg(Color::BrightBlack)))
        .title(" Changed files / diff preview ")
        .padding(Insets::new(0, 1, 0, 1));
    panel.render(area, frame);
    let inner = panel.inner_area(area);
    if inner.is_empty() {
        return;
    }
    let split = if app.diff_lines().is_empty() {
        inner.width
    } else {
        inner.width.min(32)
    };
    let list_area = Rect::new(inner.x, inner.y, split, inner.height);
    let mut state = DiffFileListState::new();
    if !app.changed_files().is_empty() {
        state.select(Some(0));
    }
    DiffFileList::new(app.changed_files()).render(list_area, frame, &mut state);
    if app.diff_lines().is_empty() || split >= inner.width.saturating_sub(1) {
        return;
    }
    let detail_area = Rect::new(
        inner.x.saturating_add(split).saturating_add(1),
        inner.y,
        inner.width.saturating_sub(split).saturating_sub(1),
        inner.height,
    );
    let mut diff_state = DiffViewState {
        offset: app.diff_scroll_offset(),
    };
    DiffView::new(app.diff_lines())
        .mode(DiffViewMode::Responsive)
        .fold_context(20, 3)
        .render(detail_area, frame, &mut diff_state);
}

fn transcript_render_rows(app: &BmuxApp, width: u16) -> Vec<Line> {
    let mut rows = Vec::new();
    for item in app.transcript() {
        push_transcript_item_rows(&mut rows, item, width);
    }
    for pending in app.pending_submissions() {
        push_pending_submission_rows(&mut rows, pending, width);
    }
    if app.has_older_history() || app.loading_older_history() {
        rows.insert(
            0,
            Line::from_spans(vec![Span::styled(
                if app.loading_older_history() {
                    "Loading older history…"
                } else {
                    "Scroll up to load older history"
                },
                Style::new().fg(Color::BrightBlack),
            )]),
        );
    }
    rows
}
fn push_transcript_item_rows(rows: &mut Vec<Line>, item: &TranscriptItem, width: u16) {
    let role_style = role_style(item.role());
    let marker = if item.streaming() { " …" } else { "" };
    push_wrapped_prefixed_text(
        rows,
        vec![
            Span::styled(format!("{}{}", item.role(), marker), role_style),
            Span::raw(": "),
        ],
        item.text(),
        width,
    );
}

fn push_pending_submission_rows(rows: &mut Vec<Line>, pending: &PendingSubmission, width: u16) {
    push_wrapped_prefixed_text(
        rows,
        vec![
            Span::styled(
                "You",
                Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ["),
            Span::styled(
                pending_label(pending.state()),
                Style::new().fg(Color::BrightBlack),
            ),
            Span::raw("]: "),
        ],
        pending.text(),
        width,
    );
}

fn push_wrapped_prefixed_text(rows: &mut Vec<Line>, prefix: Vec<Span>, text: &str, width: u16) {
    let max_width = usize::from(width.max(1));
    let prefix_width = spans_width(&prefix);
    let available_first = max_width.saturating_sub(prefix_width).max(1);
    let continuation_prefix = Span::raw("  ");
    let available_next = max_width.saturating_sub(2).max(1);

    for (line_index, raw_line) in text.lines().enumerate() {
        let chunks = wrap_text(
            raw_line,
            if line_index == 0 {
                available_first
            } else {
                available_next
            },
        );
        for (chunk_index, chunk) in chunks.iter().enumerate() {
            if line_index == 0 && chunk_index == 0 {
                let mut spans = prefix.clone();
                spans.push(Span::raw(chunk.clone()));
                rows.push(Line::from_spans(spans));
            } else {
                rows.push(Line::from_spans(vec![
                    continuation_prefix.clone(),
                    Span::raw(chunk.clone()),
                ]));
            }
        }
    }

    if text.is_empty() {
        rows.push(Line::from_spans(prefix));
    }
}

fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for ch in text.chars() {
        let width = char_display_width(ch);
        if current_width > 0 && current_width.saturating_add(width) > max_width {
            rows.push(current);
            current = String::new();
            current_width = 0;
        }
        current.push(ch);
        current_width = current_width.saturating_add(width);
    }
    rows.push(current);
    rows
}

fn spans_width(spans: &[Span]) -> usize {
    spans
        .iter()
        .map(|span| text_display_width(&span.content))
        .sum()
}

fn text_display_width(text: &str) -> usize {
    text.chars().map(char_display_width).sum()
}

fn char_display_width(ch: char) -> usize {
    if ch == '\t' {
        4
    } else if ch.is_control() {
        0
    } else if ch.len_utf8() > 1 {
        2
    } else {
        1
    }
}

fn pending_label(state: PendingSubmissionState) -> String {
    match state {
        PendingSubmissionState::Sending => "sending".to_owned(),
        PendingSubmissionState::Sent => "sent".to_owned(),
        PendingSubmissionState::Queued { queue_position } => queue_position.map_or_else(
            || "queued".to_owned(),
            |position| format!("queued #{position}"),
        ),
    }
}

fn render_status(app: &BmuxApp, area: Rect, frame: &mut Frame<'_>) {
    if area.is_empty() {
        return;
    }
    let line = Line::from_spans(vec![
        Span::styled(activity_label(app.activity()), Style::new().fg(Color::Cyan)),
        Span::raw("  "),
        Span::styled(app.status().to_owned(), Style::new().fg(Color::BrightBlack)),
    ]);
    frame.write_line(area, &line);
}

fn activity_label(activity: &ActivityState) -> String {
    match activity {
        ActivityState::Idle => "idle".to_owned(),
        ActivityState::Thinking => format!("{} thinking", spinner_frame()),
        ActivityState::Streaming => format!("{} streaming", spinner_frame()),
        ActivityState::RunningTool { name } => format!("{} tool {name}", spinner_frame()),
        ActivityState::WaitingPermission { name } => format!("permission {name}"),
    }
}

fn spinner_frame() -> &'static str {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let index = usize::try_from((elapsed / 100) % SPINNER_FRAMES.len() as u128).unwrap_or(0);
    SPINNER_FRAMES[index]
}

fn render_composer(app: &BmuxApp, area: Rect, frame: &mut Frame<'_>) {
    if area.is_empty() {
        return;
    }
    let panel = Panel::new()
        .border(Border::single().style(Style::new().fg(Color::Cyan)))
        .title(" Composer ")
        .padding(Insets::new(0, 1, 0, 1));
    panel.render(area, frame);
    TextInput::new(app.composer())
        .placeholder("Type here; Enter sends")
        .cursor_visible(app.cursor_visible())
        .render(panel.inner_area(area), frame);
}

fn role_style(role: &str) -> Style {
    match role {
        "You" => Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        "Assistant" => Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        "Tool" => Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        "Tool error" | "Skill error" => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        "Permission" => Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        "Reasoning" => Style::new().fg(Color::BrightBlack),
        _ => Style::new()
            .fg(Color::BrightBlack)
            .add_modifier(Modifier::BOLD),
    }
}
