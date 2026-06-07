//! TUI rendering.

use std::time::{Duration, Instant};

use bcode_config::TuiInlineDiffConfig;
use bcode_markdown_render::{MarkdownRenderOptions, render_markdown_lines};
use bmux_terminal_grid::{
    Color as GridColor, GridLimits, PhysicalRow, Style as GridStyle, TerminalGrid,
    TerminalGridStream,
};
use bmux_tui::ansi::ansi_to_lines;
use bmux_tui::chrome::{Border, Panel};
use bmux_tui::diff::{
    DiffChangedRange, DiffFileList, DiffFileListState, DiffInlineSpan, DiffLine, DiffLineKind,
    DiffView, DiffViewMode, DiffViewState, DiffViewStyles,
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
use super::transcript::{FileEditPhase, TranscriptItem, TranscriptItemKind};
use super::transcript_layout::TranscriptLayoutSignature;
use crate::time_format::{format_elapsed_millis, format_millis};
use bmux_tui::text_width::{display_width as text_display_width, truncate_to_display_width};
use unicode_segmentation::UnicodeSegmentation;

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const MAX_COMPOSER_ROWS: u16 = 6;
const MAX_INLINE_DIFF_ROWS: usize = 28;
const INLINE_DIFF_CARD_MIN_WIDTH: usize = 48;
const INLINE_DIFF_CARD_CHROME_WIDTH: usize = 14;
const INLINE_DIFF_BODY_CHROME_WIDTH: usize = 14;
const MAX_INLINE_STDOUT_ROWS: usize = 24;
const MAX_INLINE_STDERR_ROWS: usize = 24;
const MAX_INLINE_TOOL_TEXT_ROWS: usize = 28;
const LATEST_BAR_ACTIVE_WINDOW: Duration = Duration::from_secs(5);
const LATEST_BAR_STALE_FRAME: Duration = Duration::from_millis(900);
/// Prepared geometry for one TUI frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLayout {
    area: Rect,
    header: Rect,
    body: Rect,
    latest_bar: Option<Rect>,
    status: Rect,
    composer: Rect,
    composer_content: Rect,
}

/// Compute the transcript area for a full terminal frame.
#[must_use]
pub fn transcript_area_for_frame(app: &BmuxApp, area: Rect) -> Rect {
    if area.is_empty() {
        return area;
    }
    let composer_height = composer_height(app, area);
    let composer_y = area.bottom().saturating_sub(composer_height);
    let body_height = composer_y.saturating_sub(area.y.saturating_add(2));
    let body = Rect::new(area.x, area.y.saturating_add(1), area.width, body_height);
    transcript_area_for_body(app, body)
}

/// Prepare derived frame projections before rendering.
pub fn prepare_frame(app: &mut BmuxApp, area: Rect) -> Option<FrameLayout> {
    let layout = frame_layout(app, area)?;
    app.set_composer_content_area(layout.composer_content);
    super::transcript_projection::prepare_for_body(app, layout.body_without_latest_bar());
    Some(frame_layout(app, area).unwrap_or(layout))
}

/// Render one TUI frame.
pub fn render(app: &mut BmuxApp, frame: &mut Frame<'_>) {
    if let Some(layout) = prepare_frame(app, frame.area()) {
        render_prepared(app, frame, layout);
    }
}

/// Render one TUI frame after [`prepare_frame`] has synchronized projections.
pub fn render_prepared(app: &mut BmuxApp, frame: &mut Frame<'_>, layout: FrameLayout) {
    if layout.area.is_empty() {
        return;
    }

    render_header(app, layout.header, frame);
    render_composer(app, layout.composer, frame);
    render_body(app, layout.body, frame);
    if let Some(latest_bar) = layout.latest_bar {
        render_latest_bar(app, latest_bar, frame, Instant::now());
    }
    render_status(app, layout.status, frame);
}

impl FrameLayout {
    const fn body_without_latest_bar(self) -> Rect {
        let latest_bar_height = if self.latest_bar.is_some() { 1 } else { 0 };
        Rect::new(
            self.body.x,
            self.body.y,
            self.body.width,
            self.body.height.saturating_add(latest_bar_height),
        )
    }
}

fn frame_layout(app: &BmuxApp, area: Rect) -> Option<FrameLayout> {
    if area.is_empty() {
        return None;
    }

    let header = Rect::new(area.x, area.y, area.width, 1);
    let composer_height = composer_height(app, area);
    let composer = composer_area(area, composer_height);
    let body_height = composer.y.saturating_sub(area.y.saturating_add(2));
    let body = Rect::new(area.x, area.y.saturating_add(1), area.width, body_height);
    let latest_bar_height = u16::from(app.newer_transcript_content_below());
    let body = Rect::new(
        body.x,
        body.y,
        body.width,
        body.height.saturating_sub(latest_bar_height),
    );
    let latest_bar =
        (latest_bar_height > 0).then_some(Rect::new(area.x, body.bottom(), area.width, 1));
    let status = Rect::new(
        area.x,
        composer.y.saturating_sub(1),
        area.width,
        u16::from(composer.y > area.y.saturating_add(1)),
    );
    Some(FrameLayout {
        area,
        header,
        body,
        latest_bar,
        status,
        composer,
        composer_content: composer_panel().inner_area(composer),
    })
}

fn render_latest_bar(app: &BmuxApp, area: Rect, frame: &mut Frame<'_>, now: Instant) {
    if area.is_empty() {
        return;
    }
    frame.push_hit(
        HitRegion::new("latest-bar", area)
            .role(HitRole::ListItem)
            .layer(1),
    );
    let line = latest_bar_line(
        area.width,
        app.jump_to_latest_key_label(),
        app.latest_hidden_activity_at(),
        app.latest_hidden_activity_burst(),
        app.latest_bar_animation_started_at(),
        now,
    );
    frame.fill(area, " ", latest_bar_background_style());
    frame.write_line_with_fallback_style(area, &line, latest_bar_background_style());
}

fn latest_bar_line(
    width: u16,
    key_label: &str,
    latest_hidden_activity_at: Option<Instant>,
    latest_hidden_activity_burst: u8,
    animation_started_at: Instant,
    now: Instant,
) -> Line {
    let active = latest_hidden_activity_at
        .is_some_and(|at| now.saturating_duration_since(at) < LATEST_BAR_ACTIVE_WINDOW);
    if active {
        active_latest_bar_line(
            width,
            key_label,
            latest_hidden_activity_burst,
            animation_started_at,
            now,
        )
    } else {
        stale_latest_bar_line(width, key_label, animation_started_at, now)
    }
}

fn active_latest_bar_line(
    width: u16,
    key_label: &str,
    burst: u8,
    animation_started_at: Instant,
    now: Instant,
) -> Line {
    let width = usize::from(width);
    let text = if width < 32 {
        format!("activity below · {key_label}")
    } else {
        format!("New activity below · {key_label} to jump")
    };
    let text = centered_bar_text(&text, width);
    let text_width = text_display_width(&text);
    let left_width = width.saturating_sub(text_width) / 2;
    let right_width = width.saturating_sub(text_width).saturating_sub(left_width);
    let phase = latest_bar_phase(
        animation_started_at,
        now,
        latest_bar_active_frame_duration(burst),
    );
    let mut spans = Vec::new();
    push_latest_bar_glow_rail(&mut spans, left_width, phase, burst, false);
    spans.push(Span::styled(
        text,
        latest_bar_background_style()
            .fg(latest_bar_active_text_color(burst))
            .add_modifier(Modifier::BOLD),
    ));
    push_latest_bar_glow_rail(
        &mut spans,
        right_width,
        phase.saturating_add(left_width / 3),
        burst,
        true,
    );
    Line::from_spans(spans)
}

fn stale_latest_bar_line(
    width: u16,
    key_label: &str,
    animation_started_at: Instant,
    now: Instant,
) -> Line {
    let width = usize::from(width);
    let text = if width < 30 {
        format!("latest below · {key_label}")
    } else {
        format!("New messages below · {key_label} to jump")
    };
    let text = centered_bar_text(&text, width.saturating_sub(1));
    let text_width = text_display_width(&text);
    let left_width = width.saturating_sub(1).saturating_sub(text_width) / 2;
    let right_width = width
        .saturating_sub(1)
        .saturating_sub(text_width)
        .saturating_sub(left_width);
    let phase = latest_bar_phase(animation_started_at, now, LATEST_BAR_STALE_FRAME) % 2;
    let chevron_color = if phase == 0 {
        Color::Rgb(74, 154, 174)
    } else {
        Color::Rgb(110, 220, 235)
    };
    Line::from_spans(vec![
        Span::styled(" ".repeat(left_width), latest_bar_background_style()),
        Span::styled(
            text,
            latest_bar_background_style().fg(Color::Rgb(150, 180, 192)),
        ),
        Span::styled(" ".repeat(right_width), latest_bar_background_style()),
        Span::styled(
            "▾",
            latest_bar_background_style()
                .fg(chevron_color)
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

fn centered_bar_text(text: &str, width: usize) -> String {
    truncate_to_display_width(text, width)
}

fn latest_bar_phase(started_at: Instant, now: Instant, frame: Duration) -> usize {
    usize::try_from(
        now.saturating_duration_since(started_at).as_millis() / frame.as_millis().max(1),
    )
    .unwrap_or_default()
}

fn latest_bar_active_frame_duration(burst: u8) -> Duration {
    Duration::from_millis(
        180_u64
            .saturating_sub(u64::from(burst).saturating_mul(10))
            .max(90),
    )
}

fn push_latest_bar_glow_rail(
    spans: &mut Vec<Span>,
    width: usize,
    phase: usize,
    burst: u8,
    reverse: bool,
) {
    const GLYPHS: [&str; 3] = ["·", "•", "▾"];
    if width == 0 {
        return;
    }
    let intensity = usize::from(burst.min(8));
    let period = 14_usize.saturating_sub(intensity).max(7);
    let trail = 2_usize.saturating_add(intensity / 2);
    for column in 0..width {
        let wave_column = if reverse {
            width.saturating_sub(column).saturating_sub(1)
        } else {
            column
        };
        let wave = wave_column.saturating_add(phase) % period;
        let distance = wave.min(period.saturating_sub(wave));
        let glyph_index = match distance {
            0 => 2,
            1 | 2 => 1,
            _ if distance <= trail => 0,
            _ => usize::MAX,
        };
        if glyph_index == usize::MAX {
            spans.push(Span::styled(" ", latest_bar_background_style()));
            continue;
        }
        let mut style = latest_bar_background_style().fg(latest_bar_glow_color(distance, burst));
        if distance == 0 || (intensity >= 5 && distance <= 1) {
            style = style.add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(GLYPHS[glyph_index], style));
    }
}

const fn latest_bar_active_text_color(burst: u8) -> Color {
    if burst >= 6 {
        Color::Rgb(230, 255, 250)
    } else if burst >= 3 {
        Color::Rgb(210, 245, 250)
    } else {
        Color::White
    }
}

const fn latest_bar_glow_color(distance: usize, burst: u8) -> Color {
    match (burst >= 6, burst >= 3, distance) {
        (true, _, 0) => Color::Rgb(245, 255, 255),
        (true, _, 1 | 2) => Color::Rgb(120, 245, 255),
        (true, _, _) => Color::Rgb(70, 170, 205),
        (_, true, 0) => Color::Rgb(220, 255, 250),
        (_, true, 1 | 2) => Color::Rgb(95, 230, 255),
        (_, true, _) => Color::Rgb(60, 145, 180),
        (_, _, 0) => Color::Rgb(205, 255, 245),
        (_, _, 1 | 2) => Color::Rgb(90, 230, 255),
        (_, _, _) => Color::Rgb(62, 142, 170),
    }
}

const fn latest_bar_background_style() -> Style {
    Style::new().bg(Color::Rgb(12, 24, 32))
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

const fn composer_area(area: Rect, composer_height: u16) -> Rect {
    Rect::new(
        area.x,
        area.bottom().saturating_sub(composer_height),
        area.width,
        composer_height,
    )
}

fn composer_panel() -> Panel {
    Panel::new()
        .border(Border::single().style(Style::new().fg(Color::Cyan)))
        .title(" Message ")
        .padding(Insets::new(0, 1, 0, 1))
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

pub fn transcript_area_for_body(app: &BmuxApp, area: Rect) -> Rect {
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
        .visible_lines_from_top(app.transcript_top_row(area.height), area.height)
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

pub fn transcript_item_rows(
    transcript: &[TranscriptItem],
    index: usize,
    width: u16,
    inline_diff_config: TuiInlineDiffConfig,
) -> Vec<Line> {
    let mut rows = Vec::new();
    push_transcript_item_rows(&mut rows, transcript, index, width, inline_diff_config);
    rows
}

pub fn pending_submission_rows(pending: &PendingSubmission, width: u16) -> Vec<Line> {
    let mut rows = Vec::new();
    push_pending_submission_rows(&mut rows, pending, width);
    rows
}

pub fn history_banner_rows(has_older_history: bool, loading_older_history: bool) -> Vec<Line> {
    history_banner_text(has_older_history, loading_older_history).map_or_else(Vec::new, |text| {
        vec![Line::from_spans(vec![Span::styled(
            text,
            Style::new().fg(Color::BrightBlack),
        )])]
    })
}

pub const fn history_banner_text(
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

pub fn transcript_item_signature(
    item: &TranscriptItem,
    width: u16,
    inline_diff_config: TuiInlineDiffConfig,
) -> TranscriptLayoutSignature {
    TranscriptLayoutSignature::new(format!(
        "item:{}:{}:{width}:{inline_diff_config:?}:{}:{}:{:?}:{}:{}",
        item.id().get(),
        item.revision(),
        item.role(),
        item.streaming(),
        item.kind(),
        item.text(),
        terminal_elapsed_signature_fragment(item).unwrap_or_default()
    ))
}

fn terminal_elapsed_signature_fragment(item: &TranscriptItem) -> Option<String> {
    let TranscriptItemKind::TerminalOutput {
        started_at_ms: Some(started_at_ms),
        finished_at_ms: None,
        ..
    } = item.kind()
    else {
        return None;
    };

    format_elapsed_millis(Some(*started_at_ms), None).map(|elapsed| format!("elapsed:{elapsed}"))
}

pub fn pending_submission_signature(
    pending: &PendingSubmission,
    width: u16,
) -> TranscriptLayoutSignature {
    TranscriptLayoutSignature::new(format!(
        "pending:{width}:{:?}:{}",
        pending.state(),
        pending.text()
    ))
}

fn has_file_preview_before(
    transcript: &[TranscriptItem],
    index: usize,
    tool_call_id: &str,
) -> bool {
    transcript[..index].iter().rev().any(|item| {
        let TranscriptItemKind::ToolRequest {
            tool_call_id: item_tool_call_id,
            file_edit,
            ..
        } = item.kind()
        else {
            return false;
        };
        item_tool_call_id == tool_call_id && file_edit.is_some()
    })
}

fn push_transcript_item_rows(
    rows: &mut Vec<Line>,
    transcript: &[TranscriptItem],
    index: usize,
    width: u16,
    inline_diff_config: TuiInlineDiffConfig,
) {
    let item = &transcript[index];
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
            file_edit_phase,
        } => {
            let context = ToolRequestRenderContext {
                tool_call_id,
                tool_name,
                arguments_json,
                file_edit: file_edit.as_ref(),
                file_edit_phase: *file_edit_phase,
                inline_diff_config,
            };
            push_tool_request_rows(rows, item, &context, width);
        }
        TranscriptItemKind::ToolResult {
            tool_call_id,
            tool_name,
            arguments_json: _,
            result,
            is_error,
        } => {
            let has_file_preview = has_file_preview_before(transcript, index, tool_call_id);
            push_tool_result_rows(
                rows,
                item,
                &ToolResultRenderContext {
                    tool_call_id,
                    tool_name: tool_name.as_deref(),
                    result,
                    is_error: *is_error,
                    has_file_preview,
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

#[derive(Clone, Copy)]
struct ToolRequestRenderContext<'a> {
    tool_call_id: &'a str,
    tool_name: &'a str,
    arguments_json: &'a str,
    file_edit: Option<&'a FileEditTranscript>,
    file_edit_phase: Option<FileEditPhase>,
    inline_diff_config: TuiInlineDiffConfig,
}

fn push_tool_request_rows(
    rows: &mut Vec<Line>,
    item: &TranscriptItem,
    context: &ToolRequestRenderContext<'_>,
    width: u16,
) {
    let mut title = if context.file_edit.is_some() {
        format!(
            "{} · {}",
            file_tool_action(context.tool_name, item.streaming()),
            context.tool_name
        )
    } else {
        format!("Tool · {}", context.tool_name)
    };
    if context.file_edit_phase == Some(FileEditPhase::Failed) {
        title.push_str(" · failed");
    }
    let title_color = if context.file_edit_phase == Some(FileEditPhase::Failed) {
        Color::Red
    } else if item.streaming() {
        Color::Cyan
    } else {
        Color::Yellow
    };
    push_wrapped_styled_text(
        rows,
        Vec::new(),
        &title,
        width,
        Style::new().fg(title_color),
        Style::new().fg(title_color),
    );
    push_wrapped_styled_text(
        rows,
        vec![Span::styled("  ", muted_style())],
        &format!("call {}", context.tool_call_id),
        width,
        muted_style(),
        muted_style(),
    );
    if let Some(edit) = context.file_edit {
        push_file_edit_preview_rows(
            rows,
            edit,
            width,
            context.inline_diff_config,
            context.file_edit_phase,
            context.tool_name,
        );
    } else if let Some(presentation) =
        tool_request_presentation(context.tool_name, context.arguments_json)
    {
        push_tool_request_presentation_rows(rows, &presentation, width);
    } else if !item.text().is_empty() {
        push_labeled_text_preview(rows, "arguments", item.text(), width, 16);
    }
    rows.push(Line::default());
}

fn file_tool_action(tool_name: &str, streaming: bool) -> &'static str {
    let normalized = normalized_tool_name_for_render(tool_name);
    match (normalized.as_str(), streaming) {
        ("filesystem_write" | "write", true) => "Writing …",
        ("filesystem_write" | "write", false) => "Write preview",
        ("filesystem_edit" | "edit", true) => "Editing …",
        ("filesystem_edit" | "edit", false) => "Edit preview",
        (_, true) => "File change …",
        (_, false) => "File change preview",
    }
}

fn push_terminal_transcript_item_rows(rows: &mut Vec<Line>, item: &TranscriptItem, width: u16) {
    let TranscriptItemKind::TerminalOutput {
        tool_call_id,
        tool_name,
        output,
        columns,
        rows: terminal_rows,
        started_at_ms,
        finished_at_ms,
        exit_code,
        timed_out,
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
            started_at_ms: *started_at_ms,
            finished_at_ms: *finished_at_ms,
            exit_code: *exit_code,
            timed_out: *timed_out,
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
    started_at_ms: Option<u64>,
    finished_at_ms: Option<u64>,
    exit_code: Option<i32>,
    timed_out: Option<bool>,
    is_error: bool,
    streaming: bool,
}

fn push_terminal_tool_result_rows(
    rows: &mut Vec<Line>,
    context: TerminalToolRenderContext<'_>,
    width: u16,
) {
    let title = terminal_title(
        context.tool_name,
        context.exit_code,
        context.timed_out,
        context.is_error,
        context.streaming,
        context.started_at_ms,
        context.finished_at_ms,
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
            exit_code: context.exit_code,
            timed_out: context.timed_out,
            elapsed: format_elapsed_millis(context.started_at_ms, context.finished_at_ms),
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

fn terminal_title(
    tool_name: Option<&str>,
    exit_code: Option<i32>,
    timed_out: Option<bool>,
    is_error: bool,
    streaming: bool,
    started_at_ms: Option<u64>,
    finished_at_ms: Option<u64>,
) -> String {
    let status = if streaming || timed_out.is_none() {
        "running".to_owned()
    } else if let Some(code) = exit_code {
        let outcome = if is_error { "failed" } else { "ok" };
        format!("{outcome} · exit {code}")
    } else {
        let outcome = if is_error { "failed" } else { "ok" };
        format!("{outcome} · signal")
    };
    let elapsed = format_elapsed_millis(started_at_ms, finished_at_ms)
        .map(|elapsed| format!(" · {elapsed}"))
        .unwrap_or_default();
    let timeout = timed_out
        .filter(|timed_out| *timed_out)
        .map(|_| " · timed out".to_owned())
        .unwrap_or_default();
    tool_name.map_or_else(
        || format!("Terminal · {status}{elapsed}{timeout}"),
        |name| format!("Terminal · {name} · {status}{elapsed}{timeout}"),
    )
}

struct ToolResultRenderContext<'a> {
    tool_call_id: &'a str,
    tool_name: Option<&'a str>,
    result: &'a str,
    is_error: bool,
    has_file_preview: bool,
}

fn push_tool_result_rows(
    rows: &mut Vec<Line>,
    item: &TranscriptItem,
    context: &ToolResultRenderContext<'_>,
    width: u16,
) {
    let presentation = tool_result_presentation(context.tool_name, context.result);
    let status = if context.is_error { "failed" } else { "ok" };
    let title = match &presentation {
        Some(ToolResultPresentation::Shell(ShellResultPresentation::Terminal {
            exit_code,
            timed_out,
            ..
        })) => terminal_title(
            context.tool_name,
            *exit_code,
            Some(*timed_out),
            context.is_error,
            false,
            None,
            None,
        ),
        _ => context.tool_name.map_or_else(
            || format!("Tool result · {status}"),
            |name| format!("Tool result · {name} · {status}"),
        ),
    };
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
    if let Some(presentation) = presentation {
        if context.has_file_preview
            && matches!(
                presentation,
                ToolResultPresentation::Write { .. } | ToolResultPresentation::Edit { .. }
            )
        {
            push_muted_confirmation_rows(rows, &presentation, width);
        } else {
            push_tool_result_presentation_rows(rows, &presentation, width);
        }
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

fn push_muted_confirmation_rows(
    rows: &mut Vec<Line>,
    presentation: &ToolResultPresentation,
    width: u16,
) {
    let (ToolResultPresentation::Write { summary } | ToolResultPresentation::Edit { summary }) =
        presentation
    else {
        return;
    };
    if summary.is_empty() {
        return;
    }
    push_wrapped_styled_text(
        rows,
        vec![Span::styled("  ", muted_style())],
        &format!("confirmation: {summary}"),
        width,
        muted_style(),
        muted_style(),
    );
}

#[allow(clippy::too_many_lines)]
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
                push_kv_row(rows, "timeout", &format_millis(*timeout_ms), width);
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
        ToolRequestPresentation::WebSearch {
            query,
            provider,
            max_results,
        } => {
            push_kv_row(rows, "query", query, width);
            if let Some(provider) = provider {
                push_kv_row(rows, "provider", provider, width);
            }
            if let Some(max_results) = max_results {
                push_kv_row(rows, "limit", &format!("{max_results} results"), width);
            }
        }
        ToolRequestPresentation::WebFetch {
            url,
            max_bytes,
            render,
        } => {
            push_kv_row(rows, "url", url, width);
            if let Some(max_bytes) = max_bytes {
                push_kv_row(rows, "limit", &format!("{max_bytes} bytes"), width);
            }
            push_kv_row(rows, "rendered", if *render { "yes" } else { "no" }, width);
        }
        ToolRequestPresentation::GitClone {
            url,
            git_ref,
            destination,
        } => {
            push_kv_row(rows, "url", url, width);
            if let Some(git_ref) = git_ref {
                push_kv_row(rows, "ref", git_ref, width);
            }
            push_kv_row(
                rows,
                "destination",
                destination.as_deref().unwrap_or("Bcode artifacts"),
                width,
            );
        }
        ToolRequestPresentation::DocumentExtract {
            url,
            path,
            max_bytes,
        } => {
            if let Some(url) = url {
                push_kv_row(rows, "url", url, width);
            }
            if let Some(path) = path {
                push_kv_row(rows, "path", path, width);
            }
            if let Some(max_bytes) = max_bytes {
                push_kv_row(rows, "limit", &format!("{max_bytes} bytes"), width);
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

fn push_file_edit_preview_rows(
    rows: &mut Vec<Line>,
    edit: &FileEditTranscript,
    width: u16,
    inline_diff_config: TuiInlineDiffConfig,
    phase: Option<FileEditPhase>,
    tool_name: &str,
) {
    let summary = edit.summary();
    let phase = phase.unwrap_or(FileEditPhase::Applied);
    let phase_style = file_edit_phase_style(phase);
    push_wrapped_styled_text(
        rows,
        vec![Span::styled("  ", muted_style())],
        &format!(
            "{} · {}",
            phase.label(),
            file_write_mode_label(tool_name, edit.old_text_is_empty())
        ),
        width,
        phase_style,
        muted_style(),
    );
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
    let progress = if total_rows > shown_rows {
        format!(
            "live preview · showing {shown_rows} of {total_rows} diff rows · /diff for full view"
        )
    } else {
        "live preview · /diff for full view".to_owned()
    };
    push_wrapped_styled_text(
        rows,
        vec![Span::styled("  ", muted_style())],
        &progress,
        width,
        muted_style(),
        muted_style(),
    );

    let preview = inline_diff_preview(&diff_lines, MAX_INLINE_DIFF_ROWS);
    let card_width = inline_diff_card_width(&preview, width.saturating_sub(2), inline_diff_config);
    rows.push(inline_diff_card_border('┌', '─', '┐', card_width));
    for row in preview {
        match row {
            InlineDiffPreviewRow::Line(line) => {
                rows.extend(render_inline_diff_line(line, card_width));
            }
            InlineDiffPreviewRow::Hidden(count) => {
                rows.push(render_inline_diff_hidden_row(count, card_width));
            }
        }
    }
    rows.push(inline_diff_card_border('└', '─', '┘', card_width));
}

const fn file_edit_phase_style(phase: FileEditPhase) -> Style {
    match phase {
        FileEditPhase::Pending | FileEditPhase::Applying => Style::new().fg(Color::Cyan),
        FileEditPhase::WaitingPermission => Style::new().fg(Color::Yellow),
        FileEditPhase::Applied => Style::new().fg(Color::Green),
        FileEditPhase::Failed => Style::new().fg(Color::Red),
    }
}

fn file_write_mode_label(tool_name: &str, old_text_is_empty: bool) -> &'static str {
    let normalized = tool_name.replace(['-', '.'], "_").to_ascii_lowercase();
    if matches!(normalized.as_str(), "filesystem_write" | "write") {
        if old_text_is_empty {
            "Writing file"
        } else {
            "Replacing file"
        }
    } else {
        "Editing file"
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

fn inline_diff_card_width(
    preview: &[InlineDiffPreviewRow<'_>],
    available_width: u16,
    config: TuiInlineDiffConfig,
) -> u16 {
    let available = usize::from(available_width.max(1));
    let content_width = preview
        .iter()
        .map(|row| match row {
            InlineDiffPreviewRow::Line(line) => inline_diff_line_display_width(line),
            InlineDiffPreviewRow::Hidden(count) => inline_diff_hidden_text(*count).len(),
        })
        .max()
        .unwrap_or(0);
    let max_width = config
        .max_width
        .filter(|width| *width > 0)
        .map_or(available, |max_width| max_width.min(available));
    let width = content_width
        .saturating_add(INLINE_DIFF_CARD_CHROME_WIDTH)
        .clamp(INLINE_DIFF_CARD_MIN_WIDTH.min(max_width), max_width);
    u16::try_from(width).unwrap_or(u16::MAX)
}

fn inline_diff_line_display_width(line: &DiffLine) -> usize {
    text_display_width(&line.content)
}

fn inline_diff_card_border(left: char, fill: char, right: char, width: u16) -> Line {
    let inner_width = usize::from(width.saturating_sub(2));
    Line::from_spans(vec![
        Span::styled("  ", muted_style()),
        Span::styled(
            format!("{left}{}{right}", fill.to_string().repeat(inner_width)),
            muted_style(),
        ),
    ])
}

fn render_inline_diff_hidden_row(count: usize, width: u16) -> Line {
    let text = inline_diff_hidden_text(count);
    let inner_width = usize::from(width.saturating_sub(4));
    let clipped = truncate_to_display_width(&text, inner_width);
    let clipped_width = text_display_width(&clipped);
    let mut spans = vec![
        Span::styled("  ", muted_style()),
        Span::styled("│ ", muted_style()),
        Span::styled(clipped, muted_style()),
    ];
    let padding = inner_width.saturating_sub(clipped_width);
    if padding > 0 {
        spans.push(Span::styled(" ".repeat(padding), muted_style()));
    }
    spans.push(Span::styled(" │", muted_style()));
    Line::from_spans(spans)
}

fn inline_diff_hidden_text(count: usize) -> String {
    format!("… {count} diff rows hidden …")
}

fn render_inline_diff_line(line: &DiffLine, width: u16) -> Vec<Line> {
    let (sign, sign_style, body_style) = inline_diff_line_styles(line.kind);
    let row_style = inline_diff_row_style(line.kind);
    let emphasis_style = inline_diff_emphasis_style(line.kind);
    let line_number = inline_diff_line_number(line);
    let gutter_style = row_style.patch(muted_style());
    let body_width = inline_diff_body_width(width);
    let content_rows = wrap_inline_diff_content_spans(
        inline_diff_content_spans(line, row_style.patch(body_style), emphasis_style),
        body_width,
    );
    let content_rows = if content_rows.is_empty() {
        vec![Vec::new()]
    } else {
        content_rows
    };

    content_rows
        .into_iter()
        .enumerate()
        .map(|(index, content_spans)| {
            let mut spans = vec![Span::styled("  ", muted_style())];
            let mut card_spans = if index == 0 {
                vec![
                    Span::styled("│ ", muted_style()),
                    Span::styled("  ", gutter_style),
                    Span::styled(
                        sign,
                        row_style.patch(sign_style.add_modifier(Modifier::BOLD)),
                    ),
                    Span::styled(format!("{line_number:>4}"), gutter_style),
                    Span::styled(" │ ", gutter_style),
                ]
            } else {
                inline_diff_continuation_prefix(gutter_style)
            };
            card_spans.extend(content_spans);
            pad_inline_diff_spans(
                &mut card_spans,
                usize::from(width).saturating_sub(2),
                row_style,
            );
            card_spans.push(Span::styled(" │", muted_style()));
            spans.extend(card_spans);
            Line::from_spans(spans)
        })
        .collect()
}

fn inline_diff_continuation_prefix(gutter_style: Style) -> Vec<Span> {
    vec![
        Span::styled("│ ", muted_style()),
        Span::styled("  ", gutter_style),
        Span::styled(" ", gutter_style),
        Span::styled("    ", gutter_style),
        Span::styled(" │ ", gutter_style),
    ]
}

const fn inline_diff_body_width(width: u16) -> usize {
    (width as usize).saturating_sub(INLINE_DIFF_BODY_CHROME_WIDTH)
}

fn inline_diff_content_spans(
    line: &DiffLine,
    body_style: Style,
    emphasis_style: Style,
) -> Vec<Span> {
    let source_spans = if line.inline_spans.is_empty() {
        vec![DiffInlineSpan::new(line.content.clone(), Style::new())]
    } else {
        line.inline_spans.clone()
    };
    let mut spans = Vec::new();
    let mut offset = 0usize;
    for span in source_spans {
        let span_start = offset;
        let span_end = span_start.saturating_add(span.content.len());
        for segment in inline_diff_span_segments(&span, span_start, span_end, &line.changed_ranges)
        {
            spans.push(Span::styled(
                segment.content.to_owned(),
                inline_diff_content_style(
                    body_style,
                    segment.style,
                    if segment.emphasized {
                        emphasis_style
                    } else {
                        Style::new()
                    },
                ),
            ));
        }
        offset = span_end;
    }
    spans
}

fn wrap_inline_diff_content_spans(spans: Vec<Span>, width: usize) -> Vec<Vec<Span>> {
    let width = width.max(1);
    let mut rows: Vec<Vec<Span>> = Vec::new();
    let mut current = Vec::new();
    let mut current_width = 0usize;
    for span in spans {
        for grapheme in span.content.graphemes(true) {
            let grapheme_width = text_display_width(grapheme);
            if current_width > 0 && current_width.saturating_add(grapheme_width) > width {
                rows.push(current);
                current = Vec::new();
                current_width = 0;
            }
            current.push(Span::styled(grapheme.to_owned(), span.style));
            current_width = current_width.saturating_add(grapheme_width);
        }
    }
    rows.push(current);
    rows
}

#[derive(Debug, Clone, Copy)]
struct InlineDiffSpanSegment<'content> {
    content: &'content str,
    style: Style,
    emphasized: bool,
}

fn inline_diff_span_segments<'span>(
    span: &'span DiffInlineSpan,
    span_start: usize,
    span_end: usize,
    changed_ranges: &[DiffChangedRange],
) -> Vec<InlineDiffSpanSegment<'span>> {
    let mut segments = Vec::new();
    let mut local_start = 0usize;
    for range in changed_ranges
        .iter()
        .copied()
        .filter(|range| range.start < span_end && range.end > span_start)
    {
        let overlap_start = range.start.max(span_start).saturating_sub(span_start);
        let overlap_end = range.end.min(span_end).saturating_sub(span_start);
        if overlap_start > span.content.len()
            || overlap_end > span.content.len()
            || !span.content.is_char_boundary(overlap_start)
            || !span.content.is_char_boundary(overlap_end)
        {
            continue;
        }
        if local_start < overlap_start {
            segments.push(InlineDiffSpanSegment {
                content: &span.content[local_start..overlap_start],
                style: span.style,
                emphasized: false,
            });
        }
        segments.push(InlineDiffSpanSegment {
            content: &span.content[overlap_start..overlap_end],
            style: span.style,
            emphasized: true,
        });
        local_start = overlap_end;
    }
    if local_start < span.content.len() {
        segments.push(InlineDiffSpanSegment {
            content: &span.content[local_start..],
            style: span.style,
            emphasized: false,
        });
    }
    segments
}

const fn inline_diff_content_style(
    body_style: Style,
    span_style: Style,
    emphasis_style: Style,
) -> Style {
    body_style.patch(span_style).patch(emphasis_style)
}

fn pad_inline_diff_spans(spans: &mut Vec<Span>, width: usize, style: Style) {
    let current_width = spans
        .iter()
        .map(|span| text_display_width(&span.content))
        .sum::<usize>();
    if current_width < width {
        spans.push(Span::styled(
            " ".repeat(width.saturating_sub(current_width)),
            style,
        ));
    }
}

const fn inline_diff_row_style(kind: DiffLineKind) -> Style {
    match kind {
        DiffLineKind::Added => Style::new().bg(Color::Rgb(0, 24, 16)),
        DiffLineKind::Removed => Style::new().bg(Color::Rgb(32, 10, 10)),
        DiffLineKind::Context | DiffLineKind::FileHeader | DiffLineKind::HunkHeader => Style::new(),
    }
}

const fn inline_diff_emphasis_style(kind: DiffLineKind) -> Style {
    match kind {
        DiffLineKind::Added => Style::new().bg(Color::Rgb(0, 42, 26)),
        DiffLineKind::Removed => Style::new().bg(Color::Rgb(50, 14, 14)),
        DiffLineKind::Context | DiffLineKind::FileHeader | DiffLineKind::HunkHeader => Style::new(),
    }
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
        added_row: Style::new().bg(Color::Indexed(22)),
        removed_row: Style::new().bg(Color::Indexed(52)),
        added_emphasis: Style::new().bg(Color::Indexed(28)),
        removed_emphasis: Style::new().bg(Color::Indexed(88)),
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
                timed_out: Some(*timed_out),
                elapsed: None,
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
    timed_out: Option<bool>,
    elapsed: Option<String>,
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
    let rows = grid.main_content_tail_rows(MAX_INLINE_TOOL_TEXT_ROWS);
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
    let elapsed = output
        .elapsed
        .as_ref()
        .map(|elapsed| format!(" · {elapsed}"))
        .unwrap_or_default();
    let Some(timed_out) = output.timed_out else {
        return format!("running{elapsed} · terminal");
    };
    let exit_code = output
        .exit_code
        .map_or_else(|| "signal".to_owned(), |code| code.to_string());
    let timeout = if timed_out { " · timed out" } else { "" };
    format!("exit code {exit_code}{elapsed} · terminal{timeout}")
}

fn terminal_status_style(output: &TerminalOutputTranscript) -> Style {
    if output
        .timed_out
        .is_some_and(|timed_out| timed_out || output.exit_code.is_some_and(|code| code != 0))
    {
        Style::new().fg(Color::Red)
    } else if output.timed_out.is_none() {
        Style::new().fg(Color::Cyan)
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
    bmux_tui::text_width::wrap_text_with_continuation(text, first_width, continuation_width)
}

fn spans_width(spans: &[Span]) -> usize {
    spans
        .iter()
        .map(|span| text_display_width(&span.content))
        .sum()
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
    let mut spans = vec![Span::styled(
        activity_label(app.activity()),
        Style::new().fg(Color::Cyan),
    )];
    let status_text = statusline_status_text(app);
    if !status_text.is_empty() {
        spans.extend([
            Span::styled(" · ", Style::new().fg(Color::BrightBlack)),
            Span::styled(status_text, Style::new().fg(Color::BrightBlack)),
        ]);
    }
    if app.scroll_offset() > 0 {
        spans.push(Span::styled(
            format!(" · {} rows from bottom", app.scroll_offset()),
            Style::new().fg(Color::BrightBlack),
        ));
    } else if app.bottom_overscroll() > 0 {
        spans.push(Span::styled(
            format!(" · {} rows below latest", app.bottom_overscroll()),
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

fn statusline_status_text(app: &BmuxApp) -> String {
    let max_width = 48;
    let status = app.status();
    status
        .split(" · ")
        .map(|part| truncate_status_part(part, max_width))
        .collect::<Vec<_>>()
        .join(" · ")
}

fn truncate_status_part(part: &str, max_width: usize) -> String {
    if text_display_width(part) <= max_width {
        return part.to_owned();
    }
    let mut suffix = String::new();
    let mut width: usize = 1;
    for grapheme in part.graphemes(true).rev() {
        let grapheme_width = text_display_width(grapheme);
        if width.saturating_add(grapheme_width) > max_width {
            break;
        }
        suffix.insert_str(0, grapheme);
        width = width.saturating_add(grapheme_width);
    }
    format!("…{suffix}")
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
        ActivityState::WritingFile => format!("{} writing", spinner_frame()),
        ActivityState::EditingFile => format!("{} editing", spinner_frame()),
        ActivityState::RunningTool { name } => {
            format!("{} {}", spinner_frame(), tool_activity_label(name))
        }
        ActivityState::WaitingPermission { name } => {
            format!("permission {}", tool_activity_label(name))
        }
        ActivityState::Cancelling => format!("{} cancelling", spinner_frame()),
    }
}

fn tool_activity_label(tool_name: &str) -> String {
    match normalized_tool_name_for_render(tool_name).as_str() {
        "shell_run" | "shell" | "filesystem_shell_run" | "bash" => "shell".to_owned(),
        "filesystem_read" | "read" => "reading".to_owned(),
        "filesystem_write" | "write" => "writing".to_owned(),
        "filesystem_edit" | "edit" => "editing".to_owned(),
        "filesystem_exists" | "exists" => "checking path".to_owned(),
        "filesystem_list" | "list" => "listing".to_owned(),
        "filesystem_find" | "find" => "finding".to_owned(),
        "filesystem_grep" | "grep" => "searching".to_owned(),
        "filesystem_stat" | "stat" => "stat".to_owned(),
        other => format!("tool {other}"),
    }
}

fn normalized_tool_name_for_render(tool_name: &str) -> String {
    tool_name.replace(['-', '.'], "_").to_ascii_lowercase()
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
    let panel = composer_panel();
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
