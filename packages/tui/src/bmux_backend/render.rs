//! BMUX backend rendering.

use std::fmt::Write as _;

use bmux_tui::chrome::{Border, Panel};
use bmux_tui::diff::{DiffFileList, DiffFileListState, DiffView, DiffViewMode, DiffViewState};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::hit::{HitRegion, HitRole};
use bmux_tui::input::TextInput;
use bmux_tui::prelude::{Line, Span, StatefulWidget, Style, Widget};
use bmux_tui::style::{Color, Modifier};
use bmux_tui::text_block::{TextBlock, TextWrap};

use super::activity::ActivityState;
use super::app::BmuxApp;
use super::pending_submission::{PendingSubmission, PendingSubmissionState};
use super::transcript::{TranscriptItem, TranscriptItemKind};

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const MAX_COMPOSER_ROWS: u16 = 6;
/// Render one BMUX backend frame.
pub(super) fn render(app: &mut BmuxApp, frame: &mut Frame<'_>) {
    let area = frame.area();
    if area.is_empty() {
        return;
    }

    let header = Rect::new(area.x, area.y, area.width, 1);
    render_header(app, header, frame);

    let composer_height = composer_height(app, area);
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

fn composer_height(app: &BmuxApp, area: Rect) -> u16 {
    if area.height == 0 {
        return 0;
    }
    let content_width = area.width.saturating_sub(4).max(1);
    let rows = app
        .composer()
        .wrapped_layout(usize::from(content_width))
        .lines
        .len()
        .max(1);
    let content_rows = usize_to_u16_saturating(rows).clamp(1, MAX_COMPOSER_ROWS);
    content_rows
        .saturating_add(2)
        .min(area.height.saturating_sub(2).max(3))
        .min(area.height)
}

fn render_header(app: &BmuxApp, area: Rect, frame: &mut Frame<'_>) {
    let session_id = app.session_id().map_or_else(
        || "new".to_owned(),
        |id| truncate_middle(&id.to_string(), 12),
    );
    let session_title = app.session_title().map_or_else(
        || "Untitled session".to_owned(),
        |title| truncate_end(title, 28),
    );
    let provider = truncate_middle(app.selected_provider_plugin_id().unwrap_or("auto"), 22);
    let model = truncate_middle(app.selected_model_id().unwrap_or("default"), 30);
    let agent = truncate_middle(app.current_agent_id(), 18);
    let line = Line::from_spans(vec![
        Span::styled(
            "bcode",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled("session ", Style::new().fg(Color::BrightBlack)),
        Span::raw(session_title),
        Span::styled(
            format!(" ({session_id})"),
            Style::new().fg(Color::BrightBlack),
        ),
        Span::raw("  "),
        Span::styled("provider: ", Style::new().fg(Color::BrightBlack)),
        Span::styled(provider, Style::new().fg(Color::Cyan)),
        Span::raw("  "),
        Span::styled("model: ", Style::new().fg(Color::BrightBlack)),
        Span::raw(model),
        Span::raw("  "),
        Span::styled("agent: ", Style::new().fg(Color::BrightBlack)),
        Span::styled(agent, Style::new().fg(Color::Cyan)),
        Span::raw("  "),
        Span::styled("thinking: ", Style::new().fg(Color::BrightBlack)),
        Span::styled(
            app.thinking_label().to_owned(),
            Style::new().fg(Color::BrightBlack),
        ),
    ]);
    frame.write_line(area, &line);
}

fn render_body(app: &BmuxApp, area: Rect, frame: &mut Frame<'_>) {
    if area.is_empty() {
        return;
    }
    let transcript_area = transcript_area_for_body(app, area);
    render_transcript(app, transcript_area, frame);
    frame.push_hit(
        HitRegion::new("transcript", transcript_area)
            .role(HitRole::Scroll)
            .layer(0),
    );
    let diff_height = area.height.saturating_sub(transcript_area.height);
    if diff_height > 0 {
        let diff_area = Rect::new(area.x, transcript_area.bottom(), area.width, diff_height);
        render_changed_files(app, diff_area, frame);
    }
}

fn transcript_area_for_body(app: &BmuxApp, area: Rect) -> Rect {
    let diff_height = if app.changed_files().is_empty() || !app.diff_visible() {
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
    frame.push_hit(
        HitRegion::new("diff-files", list_area)
            .role(HitRole::ListItem)
            .layer(1),
    );
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
    frame.push_hit(
        HitRegion::new("diff-detail", detail_area)
            .role(HitRole::Scroll)
            .layer(1),
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
    match item.kind() {
        TranscriptItemKind::UserMessage => {
            push_message_block(rows, "You", item.text(), Color::Blue, width);
        }
        TranscriptItemKind::AssistantMessage => {
            push_assistant_rows(rows, item, width);
        }
        TranscriptItemKind::ReasoningMessage => {
            push_reasoning_rows(rows, item, width);
        }
        TranscriptItemKind::ToolRequest {
            tool_call_id,
            tool_name,
            diff_summary,
        } => {
            push_tool_request_rows(
                rows,
                item,
                tool_call_id,
                tool_name,
                diff_summary.as_deref(),
                width,
            );
        }
        TranscriptItemKind::ToolResult {
            tool_call_id,
            is_error,
        } => {
            push_tool_result_rows(rows, item, tool_call_id, *is_error, width);
        }
        TranscriptItemKind::Usage { turn_id } => {
            push_usage_rows(rows, item, turn_id, width);
        }
        TranscriptItemKind::PermissionRequest {
            permission_id,
            tool_call_id,
            tool_name,
        } => {
            push_permission_request_rows(rows, item, permission_id, tool_call_id, tool_name, width);
        }
        TranscriptItemKind::PermissionResult { approved } => {
            push_detail_block(
                rows,
                "Permission",
                item.text(),
                if *approved { Color::Green } else { Color::Red },
                width,
            );
        }
        TranscriptItemKind::System => {
            push_detail_block(rows, "System", item.text(), Color::BrightBlack, width);
        }
        TranscriptItemKind::Meta => {
            push_meta_block(rows, item.text(), width);
        }
        TranscriptItemKind::Skill => {
            push_detail_block(rows, "Skill", item.text(), Color::Magenta, width);
        }
        TranscriptItemKind::SkillError => {
            push_detail_block(rows, "Skill error", item.text(), Color::Red, width);
        }
        TranscriptItemKind::Generic => {
            push_detail_block(rows, item.role(), item.text(), Color::BrightBlack, width);
        }
    }
}

fn push_assistant_rows(rows: &mut Vec<Line>, item: &TranscriptItem, width: u16) {
    let title = if item.streaming() {
        "Bcode …"
    } else {
        "Bcode"
    };
    let color = if item.streaming() {
        Color::Cyan
    } else {
        Color::Green
    };
    push_message_block(rows, title, item.text(), color, width);
}

fn push_reasoning_rows(rows: &mut Vec<Line>, item: &TranscriptItem, width: u16) {
    let title = if item.streaming() {
        "thinking …"
    } else {
        "thinking"
    };
    push_detail_block(rows, title, item.text(), Color::BrightBlack, width);
}

fn push_tool_request_rows(
    rows: &mut Vec<Line>,
    item: &TranscriptItem,
    tool_call_id: &str,
    tool_name: &str,
    diff_summary: Option<&str>,
    width: u16,
) {
    let mut body = format!("call {}", truncate_middle(tool_call_id, 20));
    if let Some(summary) = diff_summary {
        body.push_str("\ndiff ");
        body.push_str(summary);
    }
    if !item.text().is_empty() {
        body.push_str("\narguments:\n");
        body.push_str(item.text());
    }
    push_detail_block(
        rows,
        &format!("Tool · {tool_name}"),
        &body,
        Color::Yellow,
        width,
    );
}

fn push_tool_result_rows(
    rows: &mut Vec<Line>,
    item: &TranscriptItem,
    tool_call_id: &str,
    is_error: bool,
    width: u16,
) {
    let mut body = tool_result_preview(item.text());
    if is_error {
        body.push_str("\ntool call ");
        body.push_str(&truncate_middle(tool_call_id, 20));
    }
    push_detail_block(
        rows,
        if is_error {
            "Tool result · failed"
        } else {
            "Tool result · ok"
        },
        &body,
        if is_error { Color::Red } else { Color::Yellow },
        width,
    );
}

fn push_usage_rows(rows: &mut Vec<Line>, item: &TranscriptItem, turn_id: &str, width: u16) {
    push_meta_block(
        rows,
        &format!("Usage · {} · {}", truncate_middle(turn_id, 18), item.text()),
        width,
    );
}

fn push_permission_request_rows(
    rows: &mut Vec<Line>,
    item: &TranscriptItem,
    permission_id: &str,
    tool_call_id: &str,
    tool_name: &str,
    width: u16,
) {
    let body = format!(
        "permission {}\ntool call {}\narguments:\n{}",
        truncate_middle(permission_id, 20),
        truncate_middle(tool_call_id, 20),
        item.text()
    );
    push_detail_block(
        rows,
        &format!("Permission required · {tool_name}"),
        &body,
        Color::Red,
        width,
    );
}

fn push_pending_submission_rows(rows: &mut Vec<Line>, pending: &PendingSubmission, width: u16) {
    let title = format!("You · {}", pending_label(pending.state()));
    push_message_block(rows, &title, pending.text(), Color::Blue, width);
}

fn push_message_block(rows: &mut Vec<Line>, title: &str, body: &str, color: Color, width: u16) {
    push_block(rows, title, body, color, true, width);
}

fn push_detail_block(rows: &mut Vec<Line>, title: &str, body: &str, color: Color, width: u16) {
    push_block(rows, title, body, color, false, width);
}

fn push_meta_block(rows: &mut Vec<Line>, text: &str, width: u16) {
    push_wrapped_styled_text(
        rows,
        vec![Span::styled("· ", muted_style())],
        text,
        width,
        muted_style(),
        muted_style(),
    );
}

fn push_block(
    rows: &mut Vec<Line>,
    title: &str,
    body: &str,
    color: Color,
    prominent: bool,
    width: u16,
) {
    let heading_style = if prominent {
        Style::new().fg(color).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(color)
    };
    push_wrapped_styled_text(rows, Vec::new(), title, width, heading_style, heading_style);
    let body_style = if prominent {
        Style::new()
    } else {
        muted_style()
    };
    if body.is_empty() {
        rows.push(Line::from_spans(vec![
            Span::styled("  ", muted_style()),
            Span::styled("·", body_style),
        ]));
    } else {
        for line in body.lines() {
            push_wrapped_styled_text(
                rows,
                vec![Span::styled("  ", muted_style())],
                line,
                width,
                body_style,
                muted_style(),
            );
        }
    }
    rows.push(Line::default());
}

fn push_wrapped_styled_text(
    rows: &mut Vec<Line>,
    prefix: Vec<Span>,
    text: &str,
    width: u16,
    body_style: Style,
    continuation_style: Style,
) {
    let max_width = usize::from(width.max(1));
    let prefix_width = spans_width(&prefix);
    let available_first = max_width.saturating_sub(prefix_width).max(1);
    let available_next = max_width.saturating_sub(2).max(1);
    let continuation_prefix = Span::styled("  ", continuation_style);

    let chunks = wrap_text_with_continuation(text, available_first, available_next);
    for (chunk_index, chunk) in chunks.iter().enumerate() {
        if chunk_index == 0 {
            let mut spans = prefix.clone();
            spans.push(Span::styled(chunk.clone(), body_style));
            rows.push(Line::from_spans(spans));
        } else {
            rows.push(Line::from_spans(vec![
                continuation_prefix.clone(),
                Span::styled(chunk.clone(), body_style),
            ]));
        }
    }

    if chunks.is_empty() {
        rows.push(Line::from_spans(prefix));
    }
}

fn wrap_text_with_continuation(
    text: &str,
    first_width: usize,
    continuation_width: usize,
) -> Vec<String> {
    let mut rows = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    let mut max_width = first_width;
    for ch in text.chars() {
        let width = char_display_width(ch);
        if current_width > 0 && current_width.saturating_add(width) > max_width {
            rows.push(current);
            current = String::new();
            current_width = 0;
            max_width = continuation_width;
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
    let mut spans = vec![
        Span::styled(activity_label(app.activity()), Style::new().fg(Color::Cyan)),
        Span::styled(" · ", Style::new().fg(Color::BrightBlack)),
        Span::styled(app.status().to_owned(), Style::new().fg(Color::BrightBlack)),
    ];
    if app.scroll_offset() > 0 {
        spans.push(Span::styled(
            format!(" · {} rows from bottom", app.scroll_offset()),
            Style::new().fg(Color::BrightBlack),
        ));
    }
    spans.extend([
        Span::styled(" · ", Style::new().fg(Color::BrightBlack)),
        Span::styled(app.token_summary(), Style::new().fg(Color::BrightBlack)),
        Span::styled(" · ", Style::new().fg(Color::BrightBlack)),
        Span::styled(
            app.key_hints().to_owned(),
            Style::new().fg(Color::BrightBlack),
        ),
    ]);
    frame.write_line(area, &Line::from_spans(spans));
}

fn activity_label(activity: &ActivityState) -> String {
    match activity {
        ActivityState::Idle => "ready".to_owned(),
        ActivityState::Thinking => format!("{} thinking", spinner_frame()),
        ActivityState::Compacting { detail } => {
            format!("{} compacting · {detail}", spinner_frame())
        }
        ActivityState::Streaming { chars } => {
            format!("{} streaming · {chars} chars", spinner_frame())
        }
        ActivityState::ProviderStream { detail } => {
            format!("{} provider stream · {detail}", spinner_frame())
        }
        ActivityState::RunningTool { name } => format!("{} tool {name}", spinner_frame()),
        ActivityState::WaitingPermission { name } => format!("permission {name}"),
        ActivityState::Cancelling => format!("{} cancelling", spinner_frame()),
    }
}

fn spinner_frame() -> &'static str {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let index = usize::try_from((elapsed / 100) % SPINNER_FRAMES.len() as u128).unwrap_or(0);
    SPINNER_FRAMES[index]
}

fn render_composer(app: &mut BmuxApp, area: Rect, frame: &mut Frame<'_>) {
    if area.is_empty() {
        return;
    }
    let panel = Panel::new()
        .border(Border::single().style(Style::new().fg(Color::Cyan)))
        .title(" Message ")
        .padding(Insets::new(0, 1, 0, 1));
    panel.render(area, frame);
    let inner = panel.inner_area(area);
    app.set_composer_content_area(inner);
    frame.push_hit(
        HitRegion::new("composer", inner)
            .role(HitRole::TextInput)
            .layer(1),
    );
    TextInput::new(app.composer())
        .placeholder("Ask Bcode…")
        .placeholder_style(Style::new().fg(Color::BrightBlack))
        .vertical_scroll(app.composer_scroll_offset())
        .cursor_visible(app.cursor_visible())
        .render(inner, frame);
}

fn truncate_middle(value: &str, max_chars: usize) -> String {
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() <= max_chars {
        return value.to_owned();
    }
    if max_chars <= 1 {
        return "…".to_owned();
    }
    let left = max_chars / 2;
    let right = max_chars.saturating_sub(left).saturating_sub(1);
    let mut output = chars.iter().take(left).collect::<String>();
    output.push('…');
    output.extend(
        chars
            .iter()
            .skip(chars.len().saturating_sub(right))
            .copied(),
    );
    output
}

fn truncate_end(value: &str, max_chars: usize) -> String {
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() <= max_chars {
        return value.to_owned();
    }
    if max_chars <= 1 {
        return "…".to_owned();
    }
    let mut output = chars
        .iter()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    output.push('…');
    output
}

fn usize_to_u16_saturating(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

const fn muted_style() -> Style {
    Style::new().fg(Color::BrightBlack)
}

fn tool_result_preview(result: &str) -> String {
    let lines = result.lines().collect::<Vec<_>>();
    if lines.len() <= 24 {
        return result.to_owned();
    }

    let omitted = lines.len().saturating_sub(20);
    let mut preview = lines
        .iter()
        .take(12)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let _ = write!(preview, "\n… {omitted} lines omitted …\n");
    preview.push_str(
        &lines
            .iter()
            .skip(lines.len().saturating_sub(8))
            .copied()
            .collect::<Vec<_>>()
            .join("\n"),
    );
    preview
}
