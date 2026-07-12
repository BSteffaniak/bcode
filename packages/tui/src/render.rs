//! TUI rendering.

use bcode_config::{TuiDiffViewerConfig, TuiDiffViewerLayout};
use bcode_plugin_sdk::tui::{PluginTuiDiffLayout, PluginTuiVisualRenderContext};
use std::cell::Cell;

thread_local! {
    static DIFF_VIEWER_CONFIG: Cell<TuiDiffViewerConfig> = const { Cell::new(TuiDiffViewerConfig {
        layout: TuiDiffViewerLayout::Auto,
        side_by_side_breakpoint: 120,
    }) };
}

fn plugin_visual_context(width: u16) -> PluginTuiVisualRenderContext {
    DIFF_VIEWER_CONFIG.with(|config| {
        let config = config.get();
        let diff_layout = match config.layout {
            TuiDiffViewerLayout::Auto => PluginTuiDiffLayout::Auto {
                breakpoint: config.side_by_side_breakpoint,
            },
            TuiDiffViewerLayout::Unified => PluginTuiDiffLayout::Unified,
            TuiDiffViewerLayout::SideBySide => PluginTuiDiffLayout::SideBySide,
        };
        PluginTuiVisualRenderContext { width, diff_layout }
    })
}

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use bcode_markdown_render::{MarkdownRenderOptions, render_markdown_lines};
use bcode_plugin_sdk::tui::PluginTuiVisualRenderMode;
use bmux_tui::chrome::{Border, Panel};
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Insets, Rect};
use bmux_tui::hit::{HitRegion, HitRole};
use bmux_tui::input::TextInput;
use bmux_tui::prelude::{Line, Span, Style, Widget};
use bmux_tui::style::{Color, Modifier};
use bmux_tui_components::text_input::TextInputControl;

use super::activity::ActivityState;
use super::app::{BmuxApp, DaemonConnectionState, LiveToolPreviewState, composer_policy};
use super::pending_submission::{PendingSubmission, PendingSubmissionState};
use super::time_format::{format_millis, unix_time_millis};
use super::tool_render_projection::{CanonicalPluginVisual, CanonicalToolVisual};
use super::transcript::{ToolTiming, TranscriptItem, TranscriptItemKind};
use super::transcript_layout::TranscriptLayoutSignature;
use bmux_tui::text_width::{display_width as text_display_width, truncate_to_display_width};
use unicode_segmentation::UnicodeSegmentation;

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const MAX_COMPOSER_ROWS: u16 = 6;
const MAX_INLINE_TOOL_TEXT_ROWS: usize = 28;
const LATEST_BAR_ACTIVE_WINDOW: Duration = Duration::from_millis(420);
#[derive(Debug, Clone, Copy)]
pub struct TuiTheme {
    pub accent: Color,
}

impl TuiTheme {
    #[must_use]
    pub const fn for_app(app: &BmuxApp) -> Self {
        Self {
            accent: app.presented_theme().accent,
        }
    }

    #[cfg(test)]
    #[must_use]
    pub fn for_agent(
        agent_id: &str,
        configured_accent: Option<&str>,
        agent_metadata_hydrated: bool,
    ) -> Self {
        Self {
            accent: super::theme::target_agent_accent(
                agent_id,
                configured_accent,
                agent_metadata_hydrated,
            ),
        }
    }
}

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

    let theme = TuiTheme::for_app(app);
    render_header(app, layout.header, frame, theme);
    render_composer(app, layout.composer, frame, theme);
    render_body(app, layout.body, frame);
    if let Some(latest_bar) = layout.latest_bar {
        render_latest_bar(app, latest_bar, frame, Instant::now());
    }
    render_status(app, layout.status, frame, theme);
}

impl FrameLayout {
    /// Return the transcript area for this prepared frame.
    #[must_use]
    pub const fn transcript_area(self, app: &BmuxApp) -> Rect {
        transcript_area_for_body(app, self.body)
    }

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
        composer_content: composer_panel(TuiTheme::for_app(app).accent).inner_area(composer),
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
    let active_age = latest_hidden_activity_at.and_then(|at| {
        let age = now.saturating_duration_since(at);
        (age < LATEST_BAR_ACTIVE_WINDOW).then_some(age)
    });
    active_age.map_or_else(
        || stale_latest_bar_line(width, key_label),
        |active_age| {
            active_latest_bar_line(
                width,
                key_label,
                latest_bar_effective_burst(latest_hidden_activity_burst, active_age),
                animation_started_at,
                now,
            )
        },
    )
}

fn latest_bar_effective_burst(burst: u8, active_age: Duration) -> u8 {
    let age_ms = active_age.as_millis();
    let window_ms = LATEST_BAR_ACTIVE_WINDOW.as_millis().max(1);
    let remaining = window_ms.saturating_sub(age_ms);
    let scaled = (u128::from(burst) * remaining).div_ceil(window_ms);
    u8::try_from(scaled.clamp(1, 8)).unwrap_or(1)
}

fn active_latest_bar_line(
    width: u16,
    key_label: &str,
    burst: u8,
    animation_started_at: Instant,
    now: Instant,
) -> Line {
    let width = usize::from(width);
    let text = latest_bar_message(width, key_label);
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

fn stale_latest_bar_line(width: u16, key_label: &str) -> Line {
    let width = usize::from(width);
    let text = latest_bar_message(width, key_label);
    let text = centered_bar_text(&text, width.saturating_sub(1));
    let text_width = text_display_width(&text);
    let left_width = width.saturating_sub(1).saturating_sub(text_width) / 2;
    let right_width = width
        .saturating_sub(1)
        .saturating_sub(text_width)
        .saturating_sub(left_width);
    Line::from_spans(vec![
        Span::styled(" ".repeat(left_width), latest_bar_background_style()),
        Span::styled(
            text,
            latest_bar_background_style().fg(Color::Rgb(130, 154, 166)),
        ),
        Span::styled(" ".repeat(right_width), latest_bar_background_style()),
        Span::styled(
            "▾",
            latest_bar_background_style()
                .fg(Color::Rgb(105, 210, 230))
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

fn latest_bar_message(width: usize, key_label: &str) -> String {
    if width < 30 {
        format!("messages below · {key_label}")
    } else {
        format!("New messages below · {key_label} to jump")
    }
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
        210_u64
            .saturating_sub(u64::from(burst).saturating_mul(21))
            .max(36),
    )
}

fn push_latest_bar_glow_rail(
    spans: &mut Vec<Span>,
    width: usize,
    phase: usize,
    burst: u8,
    reverse: bool,
) {
    const LOW_GLYPHS: [&str; 3] = ["·", "•", "▾"];
    const HIGH_GLYPHS: [&str; 3] = ["·", "◆", "▾"];
    let glyphs = if burst >= 5 { HIGH_GLYPHS } else { LOW_GLYPHS };
    if width == 0 {
        return;
    }
    let intensity = usize::from(burst.min(8));
    let period = 18_usize.saturating_sub(intensity.saturating_mul(2)).max(4);
    let trail = 1_usize.saturating_add(intensity);
    let phase_step = 1_usize.saturating_add(intensity / 3);
    for column in 0..width {
        let wave_column = if reverse {
            width.saturating_sub(column).saturating_sub(1)
        } else {
            column
        };
        let wave = wave_column.saturating_add(phase.saturating_mul(phase_step)) % period;
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
        if distance == 0 || (intensity >= 3 && distance <= 1) || (intensity >= 7 && distance <= 2) {
            style = style.add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(glyphs[glyph_index], style));
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
        (true, _, 0) => Color::Rgb(255, 255, 255),
        (true, _, 1) => Color::Rgb(0, 255, 255),
        (true, _, 2) => Color::Rgb(0, 190, 235),
        (true, _, _) => Color::Rgb(18, 92, 128),
        (_, true, 0) => Color::Rgb(235, 255, 255),
        (_, true, 1 | 2) => Color::Rgb(55, 235, 255),
        (_, true, _) => Color::Rgb(32, 125, 160),
        (_, _, 0) => Color::Rgb(190, 245, 245),
        (_, _, 1 | 2) => Color::Rgb(75, 190, 215),
        (_, _, _) => Color::Rgb(40, 100, 125),
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

fn composer_panel(accent: Color) -> Panel {
    Panel::new()
        .border(Border::single().style(Style::new().fg(accent)))
        .title(" Message ")
        .padding(Insets::new(0, 1, 0, 1))
}

fn render_header(app: &BmuxApp, area: Rect, frame: &mut Frame<'_>, theme: TuiTheme) {
    if area.is_empty() {
        return;
    }

    let line = Line::from_spans(header_spans(app, usize::from(area.width), theme));
    frame.write_line(area, &line);
}

fn header_spans(app: &BmuxApp, width: usize, theme: TuiTheme) -> Vec<Span> {
    let muted = Style::new().fg(Color::BrightBlack);
    let accent = Style::new().fg(theme.accent);
    let session_title = app
        .session_title()
        .map_or_else(|| "Untitled session".to_owned(), ToOwned::to_owned);
    let mut line = ChromeLine::new(" · ", muted)
        .required(
            "bcode".to_owned(),
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
            false,
        )
        .required(app.display_agent_id().to_owned(), accent, false)
        .required(app.model_header_label(), Style::new(), false)
        .required(session_title, Style::new(), true)
        .optional(
            format!(
                "provider {}",
                app.selected_provider_plugin_id().unwrap_or("auto")
            ),
            accent,
            50,
            false,
        );

    if let Some(session_id) = app.session_id() {
        line = line.optional(short_session_id(&session_id.to_string()), muted, 10, false);
    }

    line.spans(width)
}

fn short_session_id(session_id: &str) -> String {
    format!("#{}", session_id.chars().take(8).collect::<String>())
}

fn render_body(app: &BmuxApp, area: Rect, frame: &mut Frame<'_>) {
    if area.is_empty() {
        return;
    }
    render_transcript(app, area, frame);
    frame.push_hit(
        HitRegion::new("transcript", area)
            .role(HitRole::Scroll)
            .layer(0),
    );
}

pub const fn transcript_area_for_body(_app: &BmuxApp, area: Rect) -> Rect {
    area
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

pub fn transcript_item_rows(
    transcript: &[TranscriptItem],
    live_tool_previews: &BTreeMap<String, LiveToolPreviewState>,
    index: usize,
    width: u16,
    plugin_host: Option<&bcode_plugin::PluginHost>,
    diff_viewer_config: TuiDiffViewerConfig,
) -> Vec<Line> {
    DIFF_VIEWER_CONFIG.with(|config| config.set(diff_viewer_config));
    let mut rows = Vec::new();
    push_transcript_item_rows(
        &mut rows,
        transcript,
        live_tool_previews,
        index,
        width,
        plugin_host,
    );
    rows
}

pub fn pending_submission_rows(pending: &PendingSubmission, width: u16) -> Vec<Line> {
    if matches!(pending.state(), PendingSubmissionState::Sent) {
        return Vec::new();
    }
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
    _inline_view_config: (),
) -> TranscriptLayoutSignature {
    TranscriptLayoutSignature::new(format!(
        "item:{}:{}:{width}::{}:{}:{:?}:{}:{}",
        item.id().get(),
        item.revision(),
        item.role(),
        item.streaming(),
        item.kind(),
        item.text(),
        terminal_elapsed_signature_fragment(item).unwrap_or_default()
    ))
}

pub fn terminal_elapsed_signature_fragment(item: &TranscriptItem) -> Option<String> {
    let timing = item.tool_timing()?;
    if !item.streaming() {
        return None;
    }
    let now_ms = unix_time_millis(std::time::SystemTime::now());
    let elapsed = timing
        .started_at_ms
        .map(|started_at_ms| format_millis(now_ms.saturating_sub(started_at_ms)))
        .unwrap_or_default();
    let timeout = timing.timeout_ms.map(format_millis).unwrap_or_default();
    Some(format!("{elapsed}:{timeout}"))
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

#[allow(clippy::too_many_lines)]
fn push_transcript_item_rows(
    rows: &mut Vec<Line>,
    transcript: &[TranscriptItem],
    live_tool_previews: &BTreeMap<String, LiveToolPreviewState>,
    index: usize,
    width: u16,
    plugin_host: Option<&bcode_plugin::PluginHost>,
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
            producer_plugin_id: _,
            tool_name,
            request_visual,
            live_preview,
        } => {
            let context = ToolRequestRenderContext {
                tool_call_id,
                tool_name,
                request_visual: request_visual.as_ref(),
                _live_preview: *live_preview,
                plugin_host,
            };
            push_tool_request_rows(rows, item, &context, width);
        }
        TranscriptItemKind::LiveToolPreviewAnchor {
            tool_call_id,
            tool_name,
        } => {
            push_live_tool_preview_anchor_rows(
                rows,
                tool_name,
                live_tool_previews.get(tool_call_id),
                width,
                plugin_host,
            );
        }
        TranscriptItemKind::ToolResult {
            tool_call_id,
            tool_name,
            arguments_json: _,
            result,
            artifact,
            is_error,
            ..
        } => {
            push_tool_result_rows(
                rows,
                item,
                &ToolResultRenderContext {
                    tool_call_id,
                    tool_name: tool_name.as_deref(),
                    result,
                    artifact: artifact.as_deref(),
                    is_error: *is_error,
                    has_file_preview: false,
                },
                width,
                plugin_host,
            );
        }
        TranscriptItemKind::InteractiveToolRequest { .. } => {
            push_interactive_surface_placeholder_rows(rows, width);
        }
        TranscriptItemKind::InteractiveToolResolution { .. } => {
            push_detail_block(rows, "Interactive tool", item.text(), Color::Green, width);
        }
        TranscriptItemKind::Usage { turn_id } => {
            push_usage_rows(rows, item, turn_id, width);
        }
        TranscriptItemKind::PermissionRequest {
            permission_id,
            tool_call_id,
            tool_name,
            ..
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

fn push_interactive_surface_placeholder_rows(rows: &mut Vec<Line>, width: u16) {
    push_wrapped_styled_text(
        rows,
        Vec::new(),
        "Interactive tool",
        width,
        Style::new().fg(Color::Cyan),
        Style::new().fg(Color::Cyan),
    );
    let surface_width = width.saturating_sub(2);
    for index in 0..6 {
        let marker = if index == 0 { "┌" } else { "│" };
        rows.push(Line::from_spans(vec![
            Span::styled(marker, muted_style()),
            Span::styled(" ", muted_style()),
            Span::styled(" ".repeat(usize::from(surface_width)), Style::new()),
        ]));
    }
    rows.push(Line::default());
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
        "Reasoning …"
    } else {
        "Reasoning"
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
    request_visual: Option<&'a bcode_session_models::PluginVisualDescriptor>,
    _live_preview: bool,
    plugin_host: Option<&'a bcode_plugin::PluginHost>,
}

fn push_tool_request_rows(
    rows: &mut Vec<Line>,
    item: &TranscriptItem,
    context: &ToolRequestRenderContext<'_>,
    width: u16,
) {
    if let Some(request_visual) = context.request_visual {
        let visual = CanonicalToolVisual::from_plugin_descriptor(request_visual, false);
        if canonical_plugin_visual_available(&visual, context.plugin_host) {
            if let CanonicalToolVisual::Plugin(plugin_visual) = &visual
                && canonical_plugin_visual_render_mode(plugin_visual, context.plugin_host)
                    == Some(PluginTuiVisualRenderMode::TranscriptBlock)
            {
                push_plugin_transcript_block_rows(
                    rows,
                    PluginTranscriptBlockContext {
                        title: request_visual.title.as_deref().unwrap_or(context.tool_name),
                        visual: plugin_visual,
                        plugin_host: context.plugin_host,
                        streaming: item.streaming(),
                        is_error: false,
                        timing: item.tool_timing(),
                    },
                    width,
                );
                return;
            }
            push_canonical_tool_visual_rows(rows, &visual, width, context.plugin_host);
            rows.push(Line::default());
            return;
        }
    }
    let title = format!("Tool · {}", context.tool_name);
    let title_color = if item.streaming() {
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
    if !item.text().is_empty() {
        push_labeled_text_preview(rows, "arguments", item.text(), width, 16);
    }
    rows.push(Line::default());
}

#[allow(clippy::too_many_lines)]
fn push_live_tool_preview_anchor_rows(
    rows: &mut Vec<Line>,
    fallback_tool_name: &str,
    state: Option<&LiveToolPreviewState>,
    width: u16,
    plugin_host: Option<&bcode_plugin::PluginHost>,
) {
    let Some(state) = state else {
        push_wrapped_styled_text(
            rows,
            Vec::new(),
            &format!("Tool call · {fallback_tool_name} · streaming preview"),
            width,
            Style::new().fg(Color::Cyan),
            Style::new().fg(Color::Cyan),
        );
        push_wrapped_styled_text(
            rows,
            vec![Span::styled("  ", muted_style())],
            "assembling arguments …",
            width,
            muted_style(),
            muted_style(),
        );
        rows.push(Line::default());
        return;
    };
    let visual = CanonicalToolVisual::from_live_preview(&state.tool_name, &state.preview);
    push_canonical_tool_visual_rows(rows, &visual, width, plugin_host);
    rows.push(Line::default());
}

fn canonical_plugin_visual_render_mode(
    visual: &CanonicalPluginVisual,
    plugin_host: Option<&bcode_plugin::PluginHost>,
) -> Option<PluginTuiVisualRenderMode> {
    let host = plugin_host?;
    let route = host.visual_adapter(
        &visual.schema,
        visual.schema_version,
        "tui",
        visual.producer_plugin_id.as_deref(),
    )?;
    Some(match route.render_mode {
        bcode_plugin::PluginVisualAdapterRenderMode::Inline => PluginTuiVisualRenderMode::Inline,
        bcode_plugin::PluginVisualAdapterRenderMode::TranscriptBlock => {
            PluginTuiVisualRenderMode::TranscriptBlock
        }
        bcode_plugin::PluginVisualAdapterRenderMode::FullBlock => {
            PluginTuiVisualRenderMode::FullBlock
        }
    })
}

fn canonical_plugin_visual_available(
    visual: &CanonicalToolVisual,
    plugin_host: Option<&bcode_plugin::PluginHost>,
) -> bool {
    let CanonicalToolVisual::Plugin(plugin_visual) = visual;
    let Some(host) = plugin_host else {
        return false;
    };
    host.visual_adapter(
        &plugin_visual.schema,
        plugin_visual.schema_version,
        "tui",
        plugin_visual.producer_plugin_id.as_deref(),
    )
    .is_some()
}

fn push_canonical_tool_visual_rows(
    rows: &mut Vec<Line>,
    visual: &CanonicalToolVisual,
    width: u16,
    plugin_host: Option<&bcode_plugin::PluginHost>,
) {
    let CanonicalToolVisual::Plugin(plugin_visual) = visual;
    push_canonical_plugin_visual_rows(rows, plugin_visual, width, plugin_host);
}

fn push_canonical_plugin_visual_rows(
    rows: &mut Vec<Line>,
    visual: &CanonicalPluginVisual,
    width: u16,
    plugin_host: Option<&bcode_plugin::PluginHost>,
) {
    let Some(host) = plugin_host else {
        push_plugin_visual_degraded_rows(rows, visual, "plugin host unavailable", width);
        return;
    };
    let producer = visual.producer_plugin_id.as_deref();
    let Some(route) = host.visual_adapter(&visual.schema, visual.schema_version, "tui", producer)
    else {
        push_plugin_visual_degraded_rows(rows, visual, "no TUI visual adapter registered", width);
        return;
    };
    let Some(registry) = host.tui_registry(&route.plugin_id) else {
        push_plugin_visual_degraded_rows(
            rows,
            visual,
            "TUI visual adapter plugin unavailable",
            width,
        );
        return;
    };
    if let Some(native_rows) = registry.visual_rows_with_context(
        &route.schema,
        &visual.payload,
        plugin_visual_context(width),
    ) {
        rows.extend(native_rows);
        return;
    }
    push_plugin_visual_degraded_rows(
        rows,
        visual,
        "TUI visual adapter could not render payload",
        width,
    );
}

#[derive(Clone, Copy)]
struct PluginTranscriptBlockContext<'a> {
    title: &'a str,
    visual: &'a CanonicalPluginVisual,
    plugin_host: Option<&'a bcode_plugin::PluginHost>,
    streaming: bool,
    is_error: bool,
    timing: Option<ToolTiming>,
}

fn push_plugin_transcript_block_rows(
    rows: &mut Vec<Line>,
    context: PluginTranscriptBlockContext<'_>,
    width: u16,
) {
    let color = if context.is_error {
        Color::Red
    } else if context.streaming {
        Color::Cyan
    } else {
        Color::Yellow
    };
    let title = tool_block_title_with_timing(context.title, context.timing, context.streaming);
    push_wrapped_styled_text(
        rows,
        Vec::new(),
        &title,
        width,
        Style::new().fg(color),
        muted_style(),
    );
    push_canonical_plugin_visual_rows(rows, context.visual, width, context.plugin_host);
    rows.push(Line::default());
}

fn tool_block_title_with_timing(
    title: &str,
    timing: Option<ToolTiming>,
    streaming: bool,
) -> String {
    let Some(timing) = timing else {
        return title.to_owned();
    };
    let now_ms = unix_time_millis(std::time::SystemTime::now());
    let mut parts = Vec::new();
    if timing.timed_out == Some(true) {
        parts.push("timed out".to_owned());
    }
    if streaming {
        if let Some(started_at_ms) = timing.started_at_ms {
            parts.push(format!(
                "elapsed {}",
                format_millis(now_ms.saturating_sub(started_at_ms))
            ));
        }
        if let Some(timeout_ms) = timing.timeout_ms {
            parts.push(format!("timeout {}", format_millis(timeout_ms)));
        }
    } else if let (Some(started_at_ms), Some(finished_at_ms)) =
        (timing.started_at_ms, timing.finished_at_ms)
    {
        parts.push(format!(
            "duration {}",
            format_millis(finished_at_ms.saturating_sub(started_at_ms))
        ));
    }
    if parts.is_empty() {
        title.to_owned()
    } else {
        format!("{title} · {}", parts.join(" · "))
    }
}

fn push_plugin_visual_degraded_rows(
    rows: &mut Vec<Line>,
    visual: &CanonicalPluginVisual,
    message: &str,
    width: u16,
) {
    let title = visual.title.as_deref().unwrap_or(&visual.schema);
    push_degraded_tool_visual_rows(rows, title, message, width);
    if let Some(subtitle) = &visual.subtitle {
        push_wrapped_styled_text(
            rows,
            vec![Span::styled("  ", muted_style())],
            subtitle,
            width,
            muted_style(),
            muted_style(),
        );
    }
    if visual.streaming
        && let Some(payload) = visual.payload.as_object()
    {
        for (key, value) in payload {
            if matches!(key.as_str(), "argument_bytes" | "truncated") {
                continue;
            }
            let rendered = value
                .as_str()
                .map_or_else(|| value.to_string(), ToOwned::to_owned);
            push_wrapped_styled_text(
                rows,
                vec![Span::styled("  ", muted_style())],
                &format!("{key}: {rendered}"),
                width,
                muted_style(),
                muted_style(),
            );
        }
    }
}

fn push_degraded_tool_visual_rows(rows: &mut Vec<Line>, title: &str, message: &str, width: u16) {
    push_wrapped_styled_text(
        rows,
        Vec::new(),
        title,
        width,
        Style::new().fg(Color::Yellow),
        Style::new().fg(Color::Yellow),
    );
    push_wrapped_styled_text(
        rows,
        vec![Span::styled("  ", muted_style())],
        message,
        width,
        muted_style(),
        muted_style(),
    );
}

struct ToolResultRenderContext<'a> {
    tool_call_id: &'a str,
    tool_name: Option<&'a str>,
    result: &'a str,
    artifact: Option<&'a bcode_session_models::ToolArtifact>,
    is_error: bool,
    has_file_preview: bool,
}

fn push_tool_result_rows(
    rows: &mut Vec<Line>,
    item: &TranscriptItem,
    context: &ToolResultRenderContext<'_>,
    width: u16,
    plugin_host: Option<&bcode_plugin::PluginHost>,
) {
    if let Some(artifact) = context.artifact {
        let visual = CanonicalToolVisual::from_artifact(artifact);
        if let CanonicalToolVisual::Plugin(plugin_visual) = &visual
            && canonical_plugin_visual_render_mode(plugin_visual, plugin_host)
                == Some(PluginTuiVisualRenderMode::FullBlock)
        {
            push_canonical_tool_visual_rows(rows, &visual, width, plugin_host);
            rows.push(Line::default());
            return;
        }
        if let CanonicalToolVisual::Plugin(plugin_visual) = &visual
            && canonical_plugin_visual_render_mode(plugin_visual, plugin_host)
                == Some(PluginTuiVisualRenderMode::TranscriptBlock)
        {
            push_plugin_transcript_block_rows(
                rows,
                PluginTranscriptBlockContext {
                    title: artifact.title.as_deref().unwrap_or("Tool result"),
                    visual: plugin_visual,
                    plugin_host,
                    streaming: item.streaming(),
                    is_error: context.is_error,
                    timing: item.tool_timing(),
                },
                width,
            );
            return;
        }
    }
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
    if let Some(artifact) = context.artifact {
        let visual = CanonicalToolVisual::from_artifact(artifact);
        push_canonical_tool_visual_rows(rows, &visual, width, plugin_host);
        rows.push(Line::default());
        return;
    }
    if context.has_file_preview && !context.result.trim().is_empty() {
        push_wrapped_styled_text(
            rows,
            vec![Span::styled("  ", muted_style())],
            &format!("confirmation: {}", context.result.trim()),
            width,
            muted_style(),
            muted_style(),
        );
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
    if matches!(pending.state(), PendingSubmissionState::Sent) {
        return;
    }
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

fn render_status(app: &BmuxApp, area: Rect, frame: &mut Frame<'_>, theme: TuiTheme) {
    if area.is_empty() {
        return;
    }

    let spans = statusline_spans(app, usize::from(area.width), theme);
    frame.write_line(area, &Line::from_spans(spans));
}

#[derive(Clone)]
struct ChromeSegment {
    text: String,
    style: Style,
    priority: u8,
    truncatable: bool,
}

impl ChromeSegment {
    const fn required(text: String, style: Style, truncatable: bool) -> Self {
        Self {
            text,
            style,
            priority: u8::MAX,
            truncatable,
        }
    }

    const fn optional(text: String, style: Style, priority: u8, truncatable: bool) -> Self {
        Self {
            text,
            style,
            priority,
            truncatable,
        }
    }
}

struct ChromeLine {
    separator: String,
    separator_style: Style,
    segments: Vec<ChromeSegment>,
}

impl ChromeLine {
    fn new(separator: impl Into<String>, separator_style: Style) -> Self {
        Self {
            separator: separator.into(),
            separator_style,
            segments: Vec::new(),
        }
    }

    fn required(mut self, text: String, style: Style, truncatable: bool) -> Self {
        self.segments
            .push(ChromeSegment::required(text, style, truncatable));
        self
    }

    fn optional(mut self, text: String, style: Style, priority: u8, truncatable: bool) -> Self {
        if !text.is_empty() {
            self.segments
                .push(ChromeSegment::optional(text, style, priority, truncatable));
        }
        self
    }

    fn spans(mut self, width: usize) -> Vec<Span> {
        self.fit(width);
        self.into_spans()
    }

    fn fit(&mut self, width: usize) {
        while self.width() > width {
            if let Some(index) = self.lowest_priority_optional_index(true) {
                self.segments.remove(index);
            } else {
                break;
            }
        }

        if self.width() <= width {
            return;
        }

        self.truncate_segments(width);

        while self.width() > width {
            if let Some(index) = self.lowest_priority_optional_index(true) {
                self.segments.remove(index);
            } else {
                break;
            }
        }

        if self.width() > width
            && let Some(segment) = self.segments.first_mut()
        {
            segment.text = truncate_chrome_part(&segment.text, width);
        }
    }

    fn truncate_segments(&mut self, width: usize) {
        let separators = self.separator_width();
        let fixed_width = self
            .segments
            .iter()
            .filter(|segment| !segment.truncatable)
            .map(|segment| text_display_width(&segment.text))
            .sum::<usize>()
            .saturating_add(separators);
        let truncatable_count = self
            .segments
            .iter()
            .filter(|segment| segment.truncatable)
            .count();
        if truncatable_count == 0 {
            return;
        }
        let truncatable_width = width.saturating_sub(fixed_width) / truncatable_count;

        for segment in self
            .segments
            .iter_mut()
            .filter(|segment| segment.truncatable)
        {
            segment.text = truncate_chrome_part(&segment.text, truncatable_width);
        }
    }

    fn lowest_priority_optional_index(&self, include_truncatable: bool) -> Option<usize> {
        self.segments
            .iter()
            .enumerate()
            .filter(|(_, segment)| segment.priority < u8::MAX)
            .filter(|(_, segment)| include_truncatable || !segment.truncatable)
            .min_by_key(|(_, segment)| segment.priority)
            .map(|(index, _)| index)
    }

    fn into_spans(self) -> Vec<Span> {
        let mut spans = Vec::new();
        for (index, segment) in self.segments.into_iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(self.separator.clone(), self.separator_style));
            }
            spans.push(Span::styled(segment.text, segment.style));
        }
        spans
    }

    fn width(&self) -> usize {
        let text_width = self
            .segments
            .iter()
            .map(|segment| text_display_width(&segment.text))
            .sum::<usize>();
        text_width.saturating_add(self.separator_width())
    }

    fn separator_width(&self) -> usize {
        self.segments
            .len()
            .saturating_sub(1)
            .saturating_mul(text_display_width(&self.separator))
    }
}

fn statusline_spans(app: &BmuxApp, width: usize, theme: TuiTheme) -> Vec<Span> {
    let muted = Style::new().fg(Color::BrightBlack);
    let mut line = ChromeLine::new(" · ", muted).required(
        activity_label(
            app.activity(),
            app.activity_started_at(),
            app.daemon_connection(),
        ),
        Style::new().fg(theme.accent),
        true,
    );

    let mut token_segments = compact_statusline_token_segments(&app.token_summary()).into_iter();
    if let Some((context_segment, _)) = token_segments.next() {
        line = line.required(context_segment, muted, false);
    }
    for (token_segment, priority) in token_segments {
        line = line.optional(token_segment, muted, priority, false);
    }

    let status_text = statusline_status_text(app);
    if !status_text.is_empty() {
        line = line.optional(status_text, muted, 90, true);
    }
    if app.scroll_offset() > 0 {
        line = line.optional(
            format!("{} rows from bottom", app.scroll_offset()),
            muted,
            100,
            false,
        );
    } else if app.bottom_overscroll() > 0 {
        line = line.optional(
            format!("{} rows below latest", app.bottom_overscroll()),
            muted,
            100,
            false,
        );
    }

    let key_hints = compact_key_hints(app.key_hints());
    if !key_hints.is_empty() {
        line = line.optional(key_hints, muted, 10, false);
    }

    line.spans(width)
}

fn statusline_status_text(app: &BmuxApp) -> String {
    app.status().to_owned()
}

fn compact_statusline_token_segments(summary: &str) -> Vec<(String, u8)> {
    summary
        .split(" · ")
        .filter_map(|part| match part {
            "reuse on" => Some(("reuse".to_owned(), 50)),
            _ if part.ends_with('%') && part.contains('/') => Some((part.to_owned(), 95)),
            _ => part
                .strip_prefix("spent ")
                .and_then(|value| value.strip_suffix(" tok"))
                .map(|value| (format!("spent {value}"), 30))
                .or_else(|| {
                    part.strip_prefix("cache read ")
                        .and_then(|value| value.strip_suffix(" tok"))
                        .map(|value| (format!("read {value}"), 45))
                })
                .or_else(|| {
                    part.strip_prefix("cache write ")
                        .and_then(|value| value.strip_suffix(" tok"))
                        .map(|value| (format!("write {value}"), 45))
                })
                .or_else(|| {
                    part.strip_prefix("sent ")
                        .and_then(|value| value.strip_suffix(" msgs"))
                        .map(|value| (format!("sent {value}"), 40))
                })
                .or_else(|| {
                    part.strip_prefix("cache points ")
                        .map(|value| (format!("pts {value}"), 40))
                })
                .or_else(|| Some((part.to_owned(), 35))),
        })
        .collect()
}

fn compact_key_hints(hints: &str) -> String {
    hints
        .replace("escape", "esc")
        .replace("ctrl+", "^")
        .replace("palette", "pal")
}

fn truncate_chrome_part(part: &str, max_width: usize) -> String {
    if text_display_width(part) <= max_width {
        return part.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_owned();
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

fn activity_label(
    activity: &ActivityState,
    started_at: std::time::Instant,
    daemon_connection: DaemonConnectionState,
) -> String {
    let elapsed = format_activity_elapsed(started_at.elapsed());
    let active = |label: String| format!("{} {label} · {elapsed}", spinner_frame());
    match activity {
        ActivityState::Idle => match daemon_connection {
            DaemonConnectionState::Connecting => format!("{} connecting…", spinner_frame()),
            DaemonConnectionState::Starting => format!("{} starting daemon…", spinner_frame()),
            DaemonConnectionState::Connected | DaemonConnectionState::IdleOffline => {
                "ready".to_owned()
            }
            DaemonConnectionState::Unavailable => "daemon unavailable".to_owned(),
        },
        ActivityState::PreparingModelRequest => active("preparing model request".to_owned()),
        ActivityState::StartingProviderRequest { provider, round } => active(format!(
            "starting {provider} request{}",
            format_round(*round)
        )),
        ActivityState::WaitingForProvider { provider, round } => active(format!(
            "waiting for {provider} response{}",
            format_round(*round)
        )),
        ActivityState::PreparingToolExecution { name } => {
            active(format!("preparing tool execution · {name}"))
        }
        ActivityState::PreparingFollowUpRequest => {
            active("preparing follow-up model request".to_owned())
        }
        ActivityState::FinalizingModelTurn => active("finalizing model turn".to_owned()),
        ActivityState::RuntimeWork { detail } | ActivityState::ProviderStream { detail } => {
            active(detail.clone())
        }
        ActivityState::Compacting { detail } => active(format!("compacting · {detail}")),
        ActivityState::Streaming { chars } => {
            active(format!("receiving model output · {chars} chars"))
        }
        ActivityState::RetryWait {
            message,
            retry_at_unix,
        } => format!(
            "{} {message}; retrying in {} · Esc to cancel",
            spinner_frame(),
            format_retry_remaining(*retry_at_unix)
        ),
        ActivityState::RunningTool { name } => active(tool_activity_label(name)),
        ActivityState::WaitingPermission { name } => active(format!(
            "waiting for permission · {}",
            tool_activity_label(name)
        )),
        ActivityState::WaitingInteraction { name } => {
            active(format!("waiting for input · {}", tool_activity_label(name)))
        }
        ActivityState::Cancelling => active("cancelling".to_owned()),
    }
}

fn format_round(round: Option<u32>) -> String {
    round.map_or_else(String::new, |round| format!(" · round {round}"))
}

fn format_activity_elapsed(elapsed: std::time::Duration) -> String {
    let millis = elapsed.as_millis();
    if millis < 1_000 {
        format!("{millis}ms")
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    }
}

fn format_retry_remaining(retry_at_unix: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let seconds = retry_at_unix.saturating_sub(now);
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600).div_ceil(60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        "less than 1m".to_owned()
    }
}

fn tool_activity_label(tool_name: &str) -> String {
    format!("tool {tool_name}")
}

fn spinner_frame() -> &'static str {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let index = usize::try_from((elapsed / 100) % SPINNER_FRAMES.len() as u128).unwrap_or(0);
    SPINNER_FRAMES[index]
}

fn render_composer(app: &mut BmuxApp, area: Rect, frame: &mut Frame<'_>, theme: TuiTheme) {
    if area.is_empty() {
        return;
    }
    let panel = composer_panel(theme.accent);
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
