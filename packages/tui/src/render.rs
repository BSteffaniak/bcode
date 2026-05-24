//! TUI rendering.

use bcode_markdown_render::{MarkdownRenderOptions, render_markdown_lines};
use bmux_terminal_grid::{
    Color as GridColor, GridLimits, PhysicalRow, Style as GridStyle, TerminalGrid,
    TerminalGridStream,
};
use bmux_tui::ansi::ansi_to_lines;
use bmux_tui::chrome::{Border, Panel};
use bmux_tui::diff::{
    DiffFileList, DiffFileListState, DiffLine, DiffLineKind, DiffView, DiffViewMode, DiffViewState,
    DiffViewStyles,
};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::hit::{HitRegion, HitRole};
use bmux_tui::input::TextInput;
use bmux_tui::prelude::{Line, Span, StatefulWidget, Style, Widget};
use bmux_tui::style::{Color, Modifier};
use bmux_tui_components::text_input::TextInputControl;

use super::activity::ActivityState;
use super::app::{BmuxApp, composer_policy};
use super::diff_extract::FileEditTranscript;
use super::pending_submission::{PendingSubmission, PendingSubmissionState};
use super::tool_present::{
    GrepMatchPresentation, ListEntryPresentation, ShellResultPresentation, ToolRequestPresentation,
    ToolResultPresentation, tool_request_presentation, tool_result_presentation,
};
use super::transcript::{TranscriptItem, TranscriptItemKind};
use super::transcript_layout::{TranscriptLayoutSignature, TranscriptLayoutSpec};

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const MAX_COMPOSER_ROWS: u16 = 6;
const MAX_INLINE_DIFF_ROWS: usize = 28;
const MAX_INLINE_STDOUT_ROWS: usize = 24;
const MAX_INLINE_STDERR_ROWS: usize = 24;
const MAX_INLINE_TOOL_TEXT_ROWS: usize = 28;
/// Render one TUI frame.
pub fn render(app: &mut BmuxApp, frame: &mut Frame<'_>) {
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
    sync_transcript_layout(app, transcript_area.width);
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
    let rows = TextInputControl::new(&composer_policy())
        .visible_rows_for_width(app.composer_state(), content_width);
    let content_rows = rows.clamp(1, MAX_COMPOSER_ROWS);
    content_rows
        .saturating_add(2)
        .min(area.height.saturating_sub(2).max(3))
        .min(area.height)
}

fn render_header(app: &BmuxApp, area: Rect, frame: &mut Frame<'_>) {
    let session_id = app
        .session_id()
        .map_or_else(|| "new".to_owned(), |id| id.to_string());
    let session_title = app
        .session_title()
        .map_or_else(|| "Untitled session".to_owned(), ToOwned::to_owned);
    let provider = app.selected_provider_plugin_id().unwrap_or("auto");
    let model = app.selected_model_id().unwrap_or("default");
    let agent = app.current_agent_id();
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
    app.transcript_layout()
        .total_rows()
        .saturating_sub(usize::from(area.height))
}

fn render_transcript(app: &BmuxApp, area: Rect, frame: &mut Frame<'_>) {
    if area.is_empty() {
        return;
    }
    if app.transcript().is_empty() && app.pending_submissions().is_empty() {
        return;
    }

    let mut y = area.y;
    for visible in app
        .transcript_layout()
        .visible_lines(app.scroll_offset(), area.height)
    {
        if y >= area.bottom() {
            break;
        }
        if let Some(row) = app.transcript_layout().line(visible) {
            frame.write_line(Rect::new(area.x, y, area.width, 1), row);
            y = y.saturating_add(1);
        }
    }
}

fn sync_transcript_layout(app: &mut BmuxApp, width: u16) {
    let transcript = app.transcript().to_vec();
    let pending_submissions = app.pending_submissions().to_vec();
    let has_older_history = app.has_older_history();
    let loading_older_history = app.loading_older_history();
    app.transcript_layout_mut().sync(TranscriptLayoutSpec {
        width,
        transcript_len: transcript.len(),
        pending_len: pending_submissions.len(),
        transcript_signature: |index| transcript_item_signature(&transcript[index], width),
        transcript_rows: |index| transcript_item_rows(&transcript[index], width),
        pending_signature: |index| pending_submission_signature(&pending_submissions[index], width),
        pending_rows: |index| pending_submission_rows(&pending_submissions[index], width),
        history_banner_signature: || {
            history_banner_text(has_older_history, loading_older_history)
                .map(|text| TranscriptLayoutSignature::new(format!("history:{width}:{text}")))
        },
        history_banner_rows: || history_banner_rows(has_older_history, loading_older_history),
        reset: || false,
    });
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
        .styles(diff_view_styles())
        .fold_context(20, 3)
        .render(detail_area, frame, &mut diff_state);
}

fn transcript_item_rows(item: &TranscriptItem, width: u16) -> Vec<Line> {
    let mut rows = Vec::new();
    push_transcript_item_rows(&mut rows, item, width);
    rows
}

fn pending_submission_rows(pending: &PendingSubmission, width: u16) -> Vec<Line> {
    let mut rows = Vec::new();
    push_pending_submission_rows(&mut rows, pending, width);
    rows
}

fn history_banner_rows(has_older_history: bool, loading_older_history: bool) -> Vec<Line> {
    history_banner_text(has_older_history, loading_older_history).map_or_else(Vec::new, |text| {
        vec![Line::from_spans(vec![Span::styled(
            text,
            Style::new().fg(Color::BrightBlack),
        )])]
    })
}

const fn history_banner_text(
    has_older_history: bool,
    loading_older_history: bool,
) -> Option<&'static str> {
    if loading_older_history {
        Some("Loading older history…")
    } else if has_older_history {
        Some("Scroll up to load older history")
    } else {
        None
    }
}

fn transcript_item_signature(item: &TranscriptItem, width: u16) -> TranscriptLayoutSignature {
    TranscriptLayoutSignature::new(format!(
        "item:{width}:{}:{}:{:?}:{}",
        item.role(),
        item.streaming(),
        item.kind(),
        item.text()
    ))
}

fn pending_submission_signature(
    pending: &PendingSubmission,
    width: u16,
) -> TranscriptLayoutSignature {
    TranscriptLayoutSignature::new(format!(
        "pending:{width}:{:?}:{}",
        pending.state(),
        pending.text()
    ))
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
            arguments_json,
            file_edit,
        } => {
            push_tool_request_rows(
                rows,
                item,
                tool_call_id,
                tool_name,
                arguments_json,
                file_edit.as_ref(),
                width,
            );
        }
        TranscriptItemKind::ToolResult {
            tool_call_id,
            tool_name,
            arguments_json: _,
            result,
            is_error,
        } => {
            push_tool_result_rows(
                rows,
                item,
                &ToolResultRenderContext {
                    tool_call_id,
                    tool_name: tool_name.as_deref(),
                    result,
                    is_error: *is_error,
                },
                width,
            );
        }
        TranscriptItemKind::TerminalOutput { .. } => {
            push_terminal_transcript_item_rows(rows, item, width);
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
    push_markdown_message_block(
        rows,
        title,
        item.text(),
        color,
        width,
        item.streaming(),
        true,
    );
}

fn push_markdown_message_block(
    rows: &mut Vec<Line>,
    title: &str,
    body: &str,
    color: Color,
    width: u16,
    streaming: bool,
    prominent: bool,
) {
    let heading_style = if prominent {
        Style::new().fg(color).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(color)
    };
    push_wrapped_styled_text(rows, Vec::new(), title, width, heading_style, heading_style);

    if body.is_empty() {
        rows.push(Line::from_spans(vec![
            Span::styled("  ", muted_style()),
            Span::styled(
                "·",
                if prominent {
                    Style::new()
                } else {
                    muted_style()
                },
            ),
        ]));
    } else {
        for line in render_markdown_lines(
            body,
            MarkdownRenderOptions::new(width.saturating_sub(2).max(1)).streaming(streaming),
        ) {
            let mut spans = vec![Span::styled("  ", muted_style())];
            spans.extend(line.spans);
            rows.push(Line::from_spans(spans));
        }
    }

    rows.push(Line::default());
}

fn push_reasoning_rows(rows: &mut Vec<Line>, item: &TranscriptItem, width: u16) {
    let title = if item.streaming() {
        "thinking …"
    } else {
        "thinking"
    };
    push_markdown_message_block(
        rows,
        title,
        item.text(),
        Color::BrightBlack,
        width,
        item.streaming(),
        false,
    );
}

fn push_tool_request_rows(
    rows: &mut Vec<Line>,
    item: &TranscriptItem,
    tool_call_id: &str,
    tool_name: &str,
    arguments_json: &str,
    file_edit: Option<&FileEditTranscript>,
    width: u16,
) {
    push_wrapped_styled_text(
        rows,
        Vec::new(),
        &format!("Tool · {tool_name}"),
        width,
        Style::new().fg(Color::Yellow),
        Style::new().fg(Color::Yellow),
    );
    push_wrapped_styled_text(
        rows,
        vec![Span::styled("  ", muted_style())],
        &format!("call {tool_call_id}"),
        width,
        muted_style(),
        muted_style(),
    );
    if let Some(edit) = file_edit {
        push_file_edit_preview_rows(rows, edit, width);
    } else if let Some(presentation) = tool_request_presentation(tool_name, arguments_json) {
        push_tool_request_presentation_rows(rows, &presentation, width);
    } else if !item.text().is_empty() {
        push_labeled_text_preview(rows, "arguments", item.text(), width, 16);
    }
    rows.push(Line::default());
}

fn push_terminal_transcript_item_rows(rows: &mut Vec<Line>, item: &TranscriptItem, width: u16) {
    let TranscriptItemKind::TerminalOutput {
        tool_call_id,
        tool_name,
        output,
        columns,
        rows: terminal_rows,
        is_error,
    } = item.kind()
    else {
        return;
    };
    push_terminal_tool_result_rows(
        rows,
        TerminalToolRenderContext {
            tool_call_id,
            tool_name: tool_name.as_deref(),
            output,
            columns: *columns,
            rows: *terminal_rows,
            is_error: *is_error,
            streaming: item.streaming(),
        },
        width,
    );
}

#[derive(Clone, Copy)]
struct TerminalToolRenderContext<'a> {
    tool_call_id: &'a str,
    tool_name: Option<&'a str>,
    output: &'a str,
    columns: u16,
    rows: u16,
    is_error: bool,
    streaming: bool,
}

fn push_terminal_tool_result_rows(
    rows: &mut Vec<Line>,
    context: TerminalToolRenderContext<'_>,
    width: u16,
) {
    let status = if context.streaming {
        "running"
    } else if context.is_error {
        "failed"
    } else {
        "ok"
    };
    let title = context.tool_name.map_or_else(
        || format!("Terminal · {status}"),
        |name| format!("Terminal · {name} · {status}"),
    );
    push_wrapped_styled_text(
        rows,
        Vec::new(),
        &title,
        width,
        if context.is_error {
            Style::new().fg(Color::Red)
        } else if context.streaming {
            Style::new().fg(Color::Cyan)
        } else {
            Style::new().fg(Color::Yellow)
        },
        muted_style(),
    );
    push_terminal_output_rows(
        rows,
        &TerminalOutputTranscript {
            exit_code: if context.is_error { Some(1) } else { Some(0) },
            timed_out: false,
            output: context.output.to_owned(),
            output_truncated: false,
            output_bytes: None,
            retained_output_bytes: None,
            columns: context.columns,
            rows: context.rows,
        },
        width,
    );
    if context.is_error {
        push_wrapped_styled_text(
            rows,
            vec![Span::styled("  ", muted_style())],
            &format!("tool call {}", context.tool_call_id),
            width,
            muted_style(),
            muted_style(),
        );
    }
    rows.push(Line::default());
}

struct ToolResultRenderContext<'a> {
    tool_call_id: &'a str,
    tool_name: Option<&'a str>,
    result: &'a str,
    is_error: bool,
}

fn push_tool_result_rows(
    rows: &mut Vec<Line>,
    item: &TranscriptItem,
    context: &ToolResultRenderContext<'_>,
    width: u16,
) {
    let status = if context.is_error { "failed" } else { "ok" };
    let title = context.tool_name.map_or_else(
        || format!("Tool result · {status}"),
        |name| format!("Tool result · {name} · {status}"),
    );
    push_wrapped_styled_text(
        rows,
        Vec::new(),
        &title,
        width,
        if context.is_error {
            Style::new().fg(Color::Red)
        } else {
            Style::new().fg(Color::Yellow)
        },
        muted_style(),
    );
    if let Some(presentation) = tool_result_presentation(context.tool_name, context.result) {
        push_tool_result_presentation_rows(rows, &presentation, width);
    } else {
        push_labeled_text_preview(
            rows,
            "output",
            item.text(),
            width,
            MAX_INLINE_TOOL_TEXT_ROWS,
        );
    }
    if context.is_error {
        push_wrapped_styled_text(
            rows,
            vec![Span::styled("  ", muted_style())],
            &format!("tool call {}", context.tool_call_id),
            width,
            muted_style(),
            muted_style(),
        );
    }
    rows.push(Line::default());
}

fn push_tool_request_presentation_rows(
    rows: &mut Vec<Line>,
    presentation: &ToolRequestPresentation,
    width: u16,
) {
    match presentation {
        ToolRequestPresentation::ShellRun {
            command,
            cwd,
            timeout_ms,
        } => {
            push_kv_row(rows, "command", command, width);
            if let Some(cwd) = cwd {
                push_kv_row(rows, "cwd", cwd, width);
            }
            if let Some(timeout_ms) = timeout_ms {
                push_kv_row(rows, "timeout", &format!("{timeout_ms}ms"), width);
            }
            push_kv_row(rows, "terminal", "yes", width);
        }
        ToolRequestPresentation::Read { path }
        | ToolRequestPresentation::Exists { path }
        | ToolRequestPresentation::Stat { path } => {
            push_kv_row(rows, "path", path, width);
        }
        ToolRequestPresentation::Write { path, bytes, lines } => {
            push_kv_row(rows, "path", path, width);
            push_kv_row(
                rows,
                "contents",
                &format!("{bytes} bytes · {lines} lines"),
                width,
            );
        }
        ToolRequestPresentation::List {
            path,
            recursive,
            max_entries,
        } => {
            push_kv_row(rows, "path", path, width);
            push_kv_row(
                rows,
                "mode",
                if *recursive { "recursive" } else { "direct" },
                width,
            );
            if let Some(max_entries) = max_entries {
                push_kv_row(rows, "limit", &format!("{max_entries} entries"), width);
            }
        }
        ToolRequestPresentation::Find {
            path,
            pattern,
            max_results,
        } => {
            push_kv_row(rows, "path", path, width);
            push_kv_row(rows, "pattern", pattern, width);
            if let Some(max_results) = max_results {
                push_kv_row(rows, "limit", &format!("{max_results} results"), width);
            }
        }
        ToolRequestPresentation::Grep {
            path,
            pattern,
            glob,
            ignore_case,
            max_matches,
        } => {
            push_kv_row(rows, "path", path, width);
            push_kv_row(rows, "pattern", pattern, width);
            if let Some(glob) = glob {
                push_kv_row(rows, "glob", glob, width);
            }
            if *ignore_case {
                push_kv_row(rows, "match", "ignore case", width);
            }
            if let Some(max_matches) = max_matches {
                push_kv_row(rows, "limit", &format!("{max_matches} matches"), width);
            }
        }
    }
}

fn push_tool_result_presentation_rows(
    rows: &mut Vec<Line>,
    presentation: &ToolResultPresentation,
    width: u16,
) {
    match presentation {
        ToolResultPresentation::Read {
            contents,
            bytes,
            lines,
        } => {
            push_kv_row(
                rows,
                "read",
                &format!("{bytes} bytes · {lines} lines"),
                width,
            );
            push_labeled_text_preview(rows, "preview", contents, width, MAX_INLINE_TOOL_TEXT_ROWS);
        }
        ToolResultPresentation::Write { summary } | ToolResultPresentation::Edit { summary } => {
            push_kv_row(rows, "result", summary, width);
        }
        ToolResultPresentation::Exists { exists } => {
            push_kv_row(rows, "exists", if *exists { "yes" } else { "no" }, width);
        }
        ToolResultPresentation::List {
            entries,
            timed_out,
            partial,
            visited_entries,
            message,
        } => push_tool_collection_rows(
            rows,
            CollectionStatus {
                count: entries.len(),
                noun: "entries",
                timed_out: *timed_out,
                partial: *partial,
                visited_entries: *visited_entries,
                message: message.as_deref(),
            },
            width,
            |rows| push_list_entries(rows, entries, width),
        ),
        ToolResultPresentation::Find {
            paths,
            timed_out,
            partial,
            visited_entries,
            message,
        } => push_tool_collection_rows(
            rows,
            CollectionStatus {
                count: paths.len(),
                noun: "paths",
                timed_out: *timed_out,
                partial: *partial,
                visited_entries: *visited_entries,
                message: message.as_deref(),
            },
            width,
            |rows| push_path_results(rows, paths, width),
        ),
        ToolResultPresentation::Grep {
            matches,
            timed_out,
            partial,
            visited_entries,
            message,
        } => push_tool_collection_rows(
            rows,
            CollectionStatus {
                count: matches.len(),
                noun: "matches",
                timed_out: *timed_out,
                partial: *partial,
                visited_entries: *visited_entries,
                message: message.as_deref(),
            },
            width,
            |rows| push_grep_matches(rows, matches, width),
        ),
        ToolResultPresentation::Shell(shell) => push_shell_result_rows(rows, shell, width),
        ToolResultPresentation::Stat { exists, kind, len } => {
            push_kv_row(rows, "exists", if *exists { "yes" } else { "no" }, width);
            if let Some(kind) = kind {
                push_kv_row(rows, "kind", kind, width);
            }
            if let Some(len) = len {
                push_kv_row(rows, "size", &format!("{len} bytes"), width);
            }
        }
    }
}

fn push_kv_row(rows: &mut Vec<Line>, label: &str, value: &str, width: u16) {
    push_wrapped_styled_text(
        rows,
        vec![
            Span::styled("  ", muted_style()),
            Span::styled(
                format!("{label}: "),
                muted_style().add_modifier(Modifier::BOLD),
            ),
        ],
        value,
        width,
        Style::new().fg(Color::BrightWhite),
        muted_style(),
    );
}

fn push_tool_collection_rows(
    rows: &mut Vec<Line>,
    status: CollectionStatus<'_>,
    width: u16,
    push_items: impl FnOnce(&mut Vec<Line>),
) {
    push_collection_status(rows, status, width);
    push_items(rows);
}

fn push_path_results(rows: &mut Vec<Line>, paths: &[String], width: u16) {
    for path in preview_values(paths, MAX_INLINE_TOOL_TEXT_ROWS) {
        push_wrapped_styled_text(
            rows,
            vec![Span::styled("    ", muted_style())],
            path,
            width,
            Style::new(),
            muted_style(),
        );
    }
    push_hidden_count(rows, paths.len(), MAX_INLINE_TOOL_TEXT_ROWS, "paths", width);
}

#[derive(Clone, Copy)]
struct CollectionStatus<'a> {
    count: usize,
    noun: &'a str,
    timed_out: bool,
    partial: bool,
    visited_entries: Option<u64>,
    message: Option<&'a str>,
}

fn push_collection_status(rows: &mut Vec<Line>, status: CollectionStatus<'_>, width: u16) {
    let mut text = format!("{} {}", status.count, status.noun);
    if let Some(visited_entries) = status.visited_entries {
        text.push_str(" · visited ");
        text.push_str(&visited_entries.to_string());
    }
    if status.timed_out {
        text.push_str(" · timed out");
    }
    if status.partial {
        text.push_str(" · partial");
    }
    push_kv_row(rows, "result", &text, width);
    if let Some(message) = status.message {
        push_kv_row(rows, "note", message, width);
    }
}

fn push_list_entries(rows: &mut Vec<Line>, entries: &[ListEntryPresentation], width: u16) {
    for entry in preview_values(entries, MAX_INLINE_TOOL_TEXT_ROWS) {
        let icon = match entry.kind.as_str() {
            "directory" => "dir ",
            "file" => "file",
            other => other,
        };
        push_wrapped_styled_text(
            rows,
            vec![
                Span::styled("    ", muted_style()),
                Span::styled(format!("{icon} "), muted_style()),
            ],
            &entry.path,
            width,
            Style::new(),
            muted_style(),
        );
    }
    push_hidden_count(
        rows,
        entries.len(),
        MAX_INLINE_TOOL_TEXT_ROWS,
        "entries",
        width,
    );
}

fn push_grep_matches(rows: &mut Vec<Line>, matches: &[GrepMatchPresentation], width: u16) {
    for grep_match in preview_values(matches, MAX_INLINE_TOOL_TEXT_ROWS) {
        let location = grep_match.line_number.map_or_else(
            || grep_match.path.clone(),
            |line_number| format!("{}:{line_number}", grep_match.path),
        );
        push_wrapped_styled_text(
            rows,
            vec![
                Span::styled("    ", muted_style()),
                Span::styled(format!("{location}: "), muted_style()),
            ],
            &grep_match.line,
            width,
            Style::new(),
            muted_style(),
        );
    }
    push_hidden_count(
        rows,
        matches.len(),
        MAX_INLINE_TOOL_TEXT_ROWS,
        "matches",
        width,
    );
}

fn preview_values<T>(values: &[T], max_rows: usize) -> impl Iterator<Item = &T> {
    values.iter().take(max_rows)
}

fn push_hidden_count(rows: &mut Vec<Line>, total: usize, shown: usize, noun: &str, width: u16) {
    if total > shown {
        push_wrapped_styled_text(
            rows,
            vec![Span::styled("    ", muted_style())],
            &format!("… {} {noun} hidden …", total - shown),
            width,
            muted_style(),
            muted_style(),
        );
    }
}

fn push_file_edit_preview_rows(rows: &mut Vec<Line>, edit: &FileEditTranscript, width: u16) {
    let summary = edit.summary();
    push_wrapped_styled_text(
        rows,
        vec![Span::styled("  ", muted_style())],
        &format!(
            "{}  +{} -{}",
            summary.display_path(),
            summary.added,
            summary.removed
        ),
        width,
        Style::new()
            .fg(Color::BrightWhite)
            .add_modifier(Modifier::BOLD),
        muted_style(),
    );
    push_wrapped_styled_text(
        rows,
        vec![Span::styled("  ", muted_style())],
        &edit_change_summary(summary.added, summary.removed),
        width,
        muted_style(),
        muted_style(),
    );

    let diff_lines = edit
        .diff_lines()
        .into_iter()
        .filter(|line| line.kind != DiffLineKind::FileHeader)
        .collect::<Vec<_>>();
    let total_rows = diff_lines.len();
    let shown_rows = total_rows.min(MAX_INLINE_DIFF_ROWS);
    push_wrapped_styled_text(
        rows,
        vec![Span::styled("  ", muted_style())],
        &format!("showing {shown_rows} of {total_rows} diff rows · /diff for full view"),
        width,
        muted_style(),
        muted_style(),
    );

    let preview = inline_diff_preview(&diff_lines, MAX_INLINE_DIFF_ROWS);
    for row in preview {
        match row {
            InlineDiffPreviewRow::Line(line) => {
                rows.push(render_inline_diff_line(line, width.saturating_sub(2)));
            }
            InlineDiffPreviewRow::Hidden(count) => {
                push_wrapped_styled_text(
                    rows,
                    vec![Span::styled("  ", muted_style())],
                    &format!("… {count} diff rows hidden …"),
                    width,
                    muted_style(),
                    muted_style(),
                );
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum InlineDiffPreviewRow<'line> {
    Line(&'line DiffLine),
    Hidden(usize),
}

fn inline_diff_preview(lines: &[DiffLine], max_rows: usize) -> Vec<InlineDiffPreviewRow<'_>> {
    if lines.len() <= max_rows || max_rows < 4 {
        return lines.iter().map(InlineDiffPreviewRow::Line).collect();
    }
    let head = max_rows / 2;
    let tail = max_rows.saturating_sub(head).saturating_sub(1);
    let hidden = lines.len().saturating_sub(head).saturating_sub(tail);
    lines
        .iter()
        .take(head)
        .map(InlineDiffPreviewRow::Line)
        .chain(std::iter::once(InlineDiffPreviewRow::Hidden(hidden)))
        .chain(
            lines
                .iter()
                .skip(lines.len().saturating_sub(tail))
                .map(InlineDiffPreviewRow::Line),
        )
        .collect()
}

fn render_inline_diff_line(line: &DiffLine, width: u16) -> Line {
    let (sign, sign_style, body_style) = inline_diff_line_styles(line.kind);
    let line_number = inline_diff_line_number(line);
    let gutter_style = muted_style();
    let body_width = usize::from(width)
        .saturating_sub(2)
        .saturating_sub(5)
        .saturating_sub(3);
    Line::from_spans(vec![
        Span::styled("  ", gutter_style),
        Span::styled(sign, sign_style.add_modifier(Modifier::BOLD)),
        Span::styled(format!("{line_number:>4}"), gutter_style),
        Span::styled(" │ ", gutter_style),
        Span::styled(
            truncate_to_display_width(&line.content, body_width),
            body_style,
        ),
    ])
}

const fn inline_diff_line_styles(kind: DiffLineKind) -> (&'static str, Style, Style) {
    match kind {
        DiffLineKind::Added => (
            "+",
            Style::new().fg(Color::BrightGreen),
            Style::new().fg(Color::BrightGreen),
        ),
        DiffLineKind::Removed => (
            "-",
            Style::new().fg(Color::BrightRed),
            Style::new().fg(Color::BrightRed),
        ),
        DiffLineKind::HunkHeader => (
            "·",
            Style::new().fg(Color::BrightCyan),
            Style::new().fg(Color::BrightCyan),
        ),
        DiffLineKind::Context | DiffLineKind::FileHeader => (" ", muted_style(), Style::new()),
    }
}

fn inline_diff_line_number(line: &DiffLine) -> String {
    line.new_line
        .or(line.old_line)
        .map_or_else(|| "·".to_owned(), |line| line.to_string())
}

fn edit_change_summary(added: u32, removed: u32) -> String {
    match (added, removed) {
        (0, 0) => "no textual changes detected".to_owned(),
        (added, 0) => format!("added {}", line_count_label(added)),
        (0, removed) => format!("removed {}", line_count_label(removed)),
        (added, removed) => {
            format!(
                "replaced {} with {}",
                line_count_label(removed),
                line_count_label(added)
            )
        }
    }
}

fn line_count_label(count: u32) -> String {
    if count == 1 {
        "1 line".to_owned()
    } else {
        format!("{count} lines")
    }
}

const fn diff_view_styles() -> DiffViewStyles {
    DiffViewStyles {
        file_header: Style::new().fg(Color::BrightBlack),
        hunk_header: Style::new().fg(Color::BrightCyan),
        context: Style::new(),
        added: Style::new().fg(Color::BrightGreen),
        removed: Style::new().fg(Color::BrightRed),
        gutter: Style::new().fg(Color::BrightBlack),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CaptureShellOutput {
    exit_code: Option<i32>,
    timed_out: bool,
    stdout: String,
    stderr: String,
}

fn push_shell_result_rows(rows: &mut Vec<Line>, shell: &ShellResultPresentation, width: u16) {
    match shell {
        ShellResultPresentation::Terminal {
            exit_code,
            timed_out,
            output,
            output_truncated,
            output_bytes,
            retained_output_bytes,
            columns,
            rows: terminal_rows,
        } => push_terminal_output_rows(
            rows,
            &TerminalOutputTranscript {
                exit_code: *exit_code,
                timed_out: *timed_out,
                output: output.clone(),
                output_truncated: *output_truncated,
                output_bytes: *output_bytes,
                retained_output_bytes: *retained_output_bytes,
                columns: *columns,
                rows: *terminal_rows,
            },
            width,
        ),
        ShellResultPresentation::Capture {
            exit_code,
            timed_out,
            stdout,
            stderr,
        } => push_shell_output_rows(
            rows,
            &CaptureShellOutput {
                exit_code: *exit_code,
                timed_out: *timed_out,
                stdout: stdout.clone(),
                stderr: stderr.clone(),
            },
            width,
        ),
    }
}

fn push_shell_output_rows(rows: &mut Vec<Line>, output: &CaptureShellOutput, width: u16) {
    let status = shell_status(output);
    push_wrapped_styled_text(
        rows,
        vec![Span::styled("  ", muted_style())],
        &status,
        width,
        shell_status_style(output),
        muted_style(),
    );
    push_ansi_output_preview(
        rows,
        "stdout",
        &output.stdout,
        width,
        MAX_INLINE_STDOUT_ROWS,
    );
    push_ansi_output_preview(
        rows,
        "stderr",
        &output.stderr,
        width,
        MAX_INLINE_STDERR_ROWS,
    );
}

struct TerminalOutputTranscript {
    exit_code: Option<i32>,
    timed_out: bool,
    output: String,
    output_truncated: bool,
    output_bytes: Option<u64>,
    retained_output_bytes: Option<u64>,
    columns: u16,
    rows: u16,
}

fn push_terminal_output_rows(rows: &mut Vec<Line>, output: &TerminalOutputTranscript, width: u16) {
    let status = terminal_status(output);
    push_wrapped_styled_text(
        rows,
        vec![Span::styled("  ", muted_style())],
        &status,
        width,
        terminal_status_style(output),
        muted_style(),
    );
    push_wrapped_styled_text(
        rows,
        vec![Span::styled("  ", muted_style())],
        &format!("terminal: {}x{}", output.columns, output.rows),
        width,
        muted_style(),
        muted_style(),
    );
    if output.output_truncated {
        push_wrapped_styled_text(
            rows,
            vec![Span::styled("  ", muted_style())],
            &terminal_truncation_status(output),
            width,
            muted_style(),
            muted_style(),
        );
    }
    for line in terminal_output_lines(output) {
        rows.push(prefix_line(line, "    ", muted_style()));
    }
}

fn terminal_truncation_status(output: &TerminalOutputTranscript) -> String {
    match (output.retained_output_bytes, output.output_bytes) {
        (Some(retained), Some(original)) => {
            format!("output truncated · showing {retained} of {original} bytes")
        }
        _ => "output truncated".to_owned(),
    }
}

fn terminal_output_lines(output: &TerminalOutputTranscript) -> Vec<Line> {
    let Ok(mut stream) = TerminalGridStream::new(
        output.columns.max(1),
        output.rows.max(1),
        GridLimits {
            scrollback_rows: MAX_INLINE_TOOL_TEXT_ROWS.saturating_mul(8),
        },
    ) else {
        return ansi_to_lines(&output.output);
    };
    stream.process(output.output.as_bytes());
    let grid = stream.grid();
    let rows = grid.main_content_rows();
    let lines = rows
        .iter()
        .map(|row| terminal_grid_row_to_line(grid, row))
        .collect::<Vec<_>>();
    preview_lines(&lines, MAX_INLINE_TOOL_TEXT_ROWS)
        .into_iter()
        .cloned()
        .collect()
}

fn terminal_grid_row_to_line(grid: &TerminalGrid, row: &PhysicalRow) -> Line {
    let mut spans = Vec::new();
    let mut current_style = None;
    let mut current_text = String::new();
    for cell in row.cells() {
        if cell.is_wide_continuation() {
            continue;
        }
        let style = terminal_grid_style(grid.palette().get(cell.style()));
        if current_style == Some(style) {
            current_text.push_str(cell.text());
            continue;
        }
        if !current_text.is_empty() {
            spans.push(Span::styled(
                current_text,
                current_style.unwrap_or_default(),
            ));
            current_text = String::new();
        }
        current_style = Some(style);
        current_text.push_str(cell.text());
    }
    if !current_text.is_empty() {
        spans.push(Span::styled(
            current_text,
            current_style.unwrap_or_default(),
        ));
    }
    Line::from_spans(spans)
}

fn terminal_grid_style(style: GridStyle) -> Style {
    let mut output = Style::new();
    if let Some(fg) = style.fg {
        output = output.fg(terminal_grid_color(fg));
    }
    if let Some(bg) = style.bg {
        output = output.bg(terminal_grid_color(bg));
    }
    let mut modifier = Modifier::EMPTY;
    if style.bold {
        modifier |= Modifier::BOLD;
    }
    if style.italic {
        modifier |= Modifier::ITALIC;
    }
    if style.underline {
        modifier |= Modifier::UNDERLINE;
    }
    if style.dim {
        modifier |= Modifier::DIM;
    }
    if style.inverse {
        modifier |= Modifier::REVERSED;
    }
    if style.strike {
        modifier |= Modifier::CROSSED_OUT;
    }
    output.add_modifier(modifier)
}

const fn terminal_grid_color(color: GridColor) -> Color {
    match color {
        GridColor::Indexed(index) => ansi_indexed_color(index),
        GridColor::Rgb { r, g, b } => Color::Rgb(r, g, b),
    }
}

const fn ansi_indexed_color(index: u8) -> Color {
    match index {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::White,
        8 => Color::BrightBlack,
        9 => Color::BrightRed,
        10 => Color::BrightGreen,
        11 => Color::BrightYellow,
        12 => Color::BrightBlue,
        13 => Color::BrightMagenta,
        14 => Color::BrightCyan,
        15 => Color::BrightWhite,
        other => Color::Indexed(other),
    }
}

fn terminal_status(output: &TerminalOutputTranscript) -> String {
    let exit_code = output
        .exit_code
        .map_or_else(|| "signal".to_owned(), |code| code.to_string());
    format!(
        "exit code {exit_code} · terminal · timed out {}",
        output.timed_out
    )
}

fn terminal_status_style(output: &TerminalOutputTranscript) -> Style {
    if output.timed_out || output.exit_code.is_some_and(|code| code != 0) {
        Style::new().fg(Color::Red)
    } else {
        Style::new().fg(Color::Green)
    }
}

fn push_ansi_output_preview(
    rows: &mut Vec<Line>,
    label: &str,
    text: &str,
    width: u16,
    max_rows: usize,
) {
    if text.is_empty() {
        return;
    }
    push_wrapped_styled_text(
        rows,
        vec![Span::styled("  ", muted_style())],
        label,
        width,
        muted_style().add_modifier(Modifier::BOLD),
        muted_style(),
    );
    let parsed = ansi_to_lines(text);
    let total = parsed.len();
    for line in preview_lines(&parsed, max_rows) {
        rows.push(prefix_line(line.clone(), "    ", muted_style()));
    }
    if total > max_rows {
        push_wrapped_styled_text(
            rows,
            vec![Span::styled("    ", muted_style())],
            &format!("… {} {label} rows hidden …", total - max_rows),
            width,
            muted_style(),
            muted_style(),
        );
    }
}

fn push_labeled_text_preview(
    rows: &mut Vec<Line>,
    label: &str,
    text: &str,
    width: u16,
    max_rows: usize,
) {
    if text.is_empty() {
        return;
    }
    push_wrapped_styled_text(
        rows,
        vec![Span::styled("  ", muted_style())],
        label,
        width,
        muted_style().add_modifier(Modifier::BOLD),
        muted_style(),
    );
    let lines = text.lines().map(Line::raw).collect::<Vec<_>>();
    let total = lines.len();
    for line in preview_lines(&lines, max_rows) {
        rows.push(prefix_line(line.clone(), "    ", muted_style()));
    }
    if total > max_rows {
        push_wrapped_styled_text(
            rows,
            vec![Span::styled("    ", muted_style())],
            &format!("… {} {label} rows hidden …", total - max_rows),
            width,
            muted_style(),
            muted_style(),
        );
    }
}

fn preview_lines(lines: &[Line], max_rows: usize) -> Vec<&Line> {
    lines
        .iter()
        .skip(lines.len().saturating_sub(max_rows))
        .collect()
}

fn prefix_line(mut line: Line, prefix: &str, prefix_style: Style) -> Line {
    let mut spans = vec![Span::styled(prefix.to_owned(), prefix_style)];
    spans.append(&mut line.spans);
    Line::from_spans(spans)
}

fn shell_status(output: &CaptureShellOutput) -> String {
    let exit = output.exit_code.map_or_else(
        || "exit unknown".to_owned(),
        |exit_code| format!("exit {exit_code}"),
    );
    if output.timed_out {
        format!("{exit} · timed out")
    } else {
        exit
    }
}

fn shell_status_style(output: &CaptureShellOutput) -> Style {
    if output.timed_out || output.exit_code.is_some_and(|exit_code| exit_code != 0) {
        Style::new().fg(Color::Red)
    } else {
        Style::new().fg(Color::Green)
    }
}

fn push_usage_rows(rows: &mut Vec<Line>, item: &TranscriptItem, turn_id: &str, width: u16) {
    push_meta_block(rows, &format!("Usage · {turn_id} · {}", item.text()), width);
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
        permission_id,
        tool_call_id,
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

fn truncate_to_display_width(text: &str, width: usize) -> String {
    let mut output = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let char_width = char_display_width(ch);
        if used.saturating_add(char_width) > width {
            output.push('…');
            return output;
        }
        output.push(ch);
        used = used.saturating_add(char_width);
    }
    output
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
        .vertical_scroll(app.composer_scroll_offset_for_render())
        .cursor_visible(app.cursor_visible())
        .render(inner, frame);
}

const fn muted_style() -> Style {
    Style::new().fg(Color::BrightBlack)
}
