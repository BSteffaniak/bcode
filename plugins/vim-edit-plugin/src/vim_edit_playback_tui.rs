//! Native TUI rendering for Vim edit visuals and playback interaction.

use bcode_plugin_sdk::tui::TerminalInteractionRenderer;
use bcode_tool::{InteractionInput, InteractionNavigation};
use bmux_keyboard::KeyCode;
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::prelude::{Color, Line, Modifier, Span, Style};
use serde_json::Value;

use super::vim_edit_interaction::VimEditPlaybackSnapshot;
use super::{
    VIM_EDIT_LIVE_SCHEMA, VIM_EDIT_PLAYBACK_SCHEMA, VIM_EDIT_PLAYBACK_SURFACE,
    VIM_EDIT_REQUEST_APPLY_SCHEMA, VIM_EDIT_REQUEST_PREVIEW_SCHEMA,
};

/// Vim edit TUI visual adapter.
pub struct VimEditPlaybackTuiVisualAdapter;

impl bcode_plugin_sdk::tui::PluginTuiVisualAdapter for VimEditPlaybackTuiVisualAdapter {
    fn supports(&self, kind: &str) -> bool {
        matches!(
            kind,
            VIM_EDIT_REQUEST_PREVIEW_SCHEMA
                | VIM_EDIT_REQUEST_APPLY_SCHEMA
                | VIM_EDIT_LIVE_SCHEMA
                | VIM_EDIT_PLAYBACK_SCHEMA
                | "bcode.vim-edit.change"
        )
    }

    fn render_mode(
        &self,
        _kind: &str,
        _payload: &Value,
    ) -> bcode_plugin_sdk::tui::PluginTuiVisualRenderMode {
        bcode_plugin_sdk::tui::PluginTuiVisualRenderMode::FullBlock
    }

    fn rows(&self, kind: &str, payload: &Value, width: u16) -> Vec<Line> {
        match kind {
            VIM_EDIT_REQUEST_PREVIEW_SCHEMA => request_rows("Vim edit preview", payload),
            VIM_EDIT_REQUEST_APPLY_SCHEMA => request_rows("Vim edit apply", payload),
            VIM_EDIT_LIVE_SCHEMA => live_rows(payload, width),
            VIM_EDIT_PLAYBACK_SCHEMA | "bcode.vim-edit.change" => {
                playback_rows(payload, None, true, true, width)
            }
            _ => Vec::new(),
        }
    }
}

/// Terminal renderer for interactive Vim edit playback.
#[derive(Default)]
pub struct VimEditPlaybackTerminalRenderer;

impl TerminalInteractionRenderer<super::vim_edit_interaction::VimEditPlaybackInteractionController>
    for VimEditPlaybackTerminalRenderer
{
    const SURFACE_KIND: &'static str = VIM_EDIT_PLAYBACK_SURFACE;

    fn id(&self) -> &'static str {
        "vim-edit-playback"
    }

    fn title(&self) -> &'static str {
        "Vim edit playback"
    }

    fn preferred_height(&mut self, snapshot: &VimEditPlaybackSnapshot, width: u16) -> u16 {
        u16::try_from(
            playback_rows(
                &snapshot.playback,
                Some(snapshot.selected_frame),
                snapshot.show_timeline,
                snapshot.show_diff,
                width,
            )
            .len()
            .saturating_add(1),
        )
        .unwrap_or(u16::MAX)
    }

    fn render(&mut self, snapshot: &VimEditPlaybackSnapshot, area: Rect, frame: &mut Frame<'_>) {
        for (offset, line) in playback_rows(
            &snapshot.playback,
            Some(snapshot.selected_frame),
            snapshot.show_timeline,
            snapshot.show_diff,
            area.width,
        )
        .iter()
        .enumerate()
        {
            let Ok(offset) = u16::try_from(offset) else {
                break;
            };
            if offset >= area.height {
                break;
            }
            frame.write_line(Rect::new(area.x, area.y + offset, area.width, 1), line);
        }
    }

    fn input(
        &mut self,
        event: &Event,
        _snapshot: &VimEditPlaybackSnapshot,
    ) -> Option<InteractionInput> {
        match event {
            Event::Key(key) => match key.key {
                KeyCode::Left | KeyCode::Char('h') => Some(InteractionInput::Activate {
                    control_id: bcode_tool::InteractionControlId::new("previous"),
                }),
                KeyCode::Right | KeyCode::Char('l') => Some(InteractionInput::Activate {
                    control_id: bcode_tool::InteractionControlId::new("next"),
                }),
                KeyCode::Char('t') => Some(InteractionInput::Activate {
                    control_id: bcode_tool::InteractionControlId::new("timeline"),
                }),
                KeyCode::Char('d') => Some(InteractionInput::Activate {
                    control_id: bcode_tool::InteractionControlId::new("diff"),
                }),
                KeyCode::Tab | KeyCode::Down => Some(InteractionInput::Navigate {
                    direction: InteractionNavigation::Next,
                }),
                KeyCode::Up => Some(InteractionInput::Navigate {
                    direction: InteractionNavigation::Previous,
                }),
                KeyCode::Enter => Some(InteractionInput::Submit),
                KeyCode::Escape | KeyCode::Char('q') => Some(InteractionInput::Cancel),
                _ => None,
            },
            _ => None,
        }
    }
}

fn request_rows(title: &str, payload: &Value) -> Vec<Line> {
    let arguments = payload.get("arguments").unwrap_or(payload);
    let mut rows = vec![header(title)];
    if let Some(path) = text(arguments, "path") {
        push_kv(&mut rows, "file", path);
        push_kv(&mut rows, "steps", count(arguments, "steps").to_string());
    }
    if let Some(files) = arguments.get("files").and_then(Value::as_array) {
        push_kv(&mut rows, "files", files.len().to_string());
        for file in files.iter().take(8) {
            let path = text(file, "path").unwrap_or("<path>");
            let steps = count(file, "steps");
            rows.push(Line::from_spans(vec![
                Span::styled("  ◆ ", accent()),
                Span::styled(path.to_owned(), value_style()),
                Span::styled(format!("  {steps} steps"), muted()),
            ]));
        }
    }
    push_kv(
        &mut rows,
        "sandbox",
        text(arguments, "sandbox").unwrap_or("default"),
    );
    rows
}

fn live_rows(payload: &Value, width: u16) -> Vec<Line> {
    let mut rows = vim_screen_rows("nvim live", payload, selected_context(payload), width);
    rows.push(Line::from_spans(vec![
        Span::styled("  step ", muted()),
        Span::styled(step_summary(payload), accent()),
        Span::styled(" · ", muted()),
        Span::styled(step_text(payload), value_style()),
    ]));
    rows
}

fn playback_rows(
    payload: &Value,
    selected_frame: Option<usize>,
    show_timeline: bool,
    show_diff: bool,
    width: u16,
) -> Vec<Line> {
    let frame = selected_frame.and_then(|index| event(payload, index));
    let source = frame.unwrap_or(payload);
    let mut rows = vim_screen_rows("nvim playback", payload, selected_context(source), width);
    if show_timeline {
        rows.push(Line::raw(""));
        rows.push(Line::from_spans(vec![Span::styled("Timeline", accent())]));
        if let Some(events) = events(payload) {
            for (index, event) in events.iter().enumerate().take(16) {
                let selected = selected_frame == Some(index);
                rows.push(Line::from_spans(vec![
                    Span::styled(if selected { "▶ " } else { "  " }, accent()),
                    Span::styled(format!("{:02} ", index + 1), muted()),
                    Span::styled(
                        step_text(event),
                        if selected { accent() } else { value_style() },
                    ),
                    Span::styled(format!("  {}", cursor_text(event)), muted()),
                ]));
            }
        }
    }
    if show_diff && let Some(diff) = text(payload, "diff").filter(|diff| !diff.is_empty()) {
        rows.push(Line::raw(""));
        rows.push(Line::from_spans(vec![Span::styled("Diff", accent())]));
        rows.extend(diff_rows(diff, width));
    }
    rows
}

fn vim_screen_rows(title: &str, payload: &Value, context: Option<&Value>, width: u16) -> Vec<Line> {
    let path = text(payload, "path").unwrap_or("<file>");
    let mode = text(payload, "nvim_mode")
        .or_else(|| text(payload, "mode"))
        .unwrap_or("normal");
    let cursor = payload
        .get("cursor")
        .or_else(|| payload.get("after_cursor"));
    let cursor = cursor.map_or_else(|| "?:?".to_string(), cursor_position);
    let heading = format!("╭─ {title}: {path} ── {} {cursor} ", mode.to_uppercase());
    let mut rows = vec![Line::from_spans(vec![Span::styled(
        pad_rule(&heading, width, '─', '╮'),
        border(),
    )])];
    if let Some(context) = context {
        let start_line = context
            .get("start_line")
            .and_then(Value::as_u64)
            .unwrap_or(1);
        let cursor_line = payload
            .get("cursor")
            .or_else(|| payload.get("after_cursor"))
            .and_then(|cursor| cursor.get("line"))
            .and_then(Value::as_u64);
        if let Some(lines) = context.get("lines").and_then(Value::as_array) {
            for (offset, line) in lines.iter().enumerate().take(12) {
                let number = start_line.saturating_add(u64::try_from(offset).unwrap_or(u64::MAX));
                let current = cursor_line == Some(number);
                rows.push(Line::from_spans(vec![
                    Span::styled(if current { "│>" } else { "│ " }, border()),
                    Span::styled(format!("{number:>4} "), muted()),
                    Span::styled(
                        truncate(
                            line.as_str().unwrap_or_default(),
                            usize::from(width.saturating_sub(8)),
                        ),
                        if current {
                            cursor_line_style()
                        } else {
                            value_style()
                        },
                    ),
                ]));
            }
        }
    }
    rows.push(Line::from_spans(vec![Span::styled(
        pad_rule("╰", width, '─', '╯'),
        border(),
    )]));
    rows
}

fn selected_context(payload: &Value) -> Option<&Value> {
    payload
        .get("context")
        .or_else(|| payload.get("final_context"))
}

fn events(payload: &Value) -> Option<&Vec<Value>> {
    payload
        .get("events")
        .or_else(|| payload.get("frames"))
        .and_then(Value::as_array)
}

fn event(payload: &Value, index: usize) -> Option<&Value> {
    events(payload)?.get(index)
}

fn diff_rows(diff: &str, width: u16) -> Vec<Line> {
    diff.lines()
        .take(24)
        .map(|line| {
            let style = if line.starts_with('+') {
                Style::new().fg(Color::Green)
            } else if line.starts_with('-') {
                Style::new().fg(Color::Red)
            } else {
                value_style()
            };
            Line::from_spans(vec![Span::styled(
                format!("  {}", truncate(line, usize::from(width.saturating_sub(4)))),
                style,
            )])
        })
        .collect()
}

fn step_text(payload: &Value) -> String {
    let step = payload.get("step").unwrap_or(payload);
    if let Some(value) = text(step, "keys").or_else(|| text(step, "input")) {
        return format!("keys {value}");
    }
    if let Some(value) = text(step, "insert").or_else(|| text(step, "text")) {
        return format!("insert {}", truncate(value, 40));
    }
    if let Some(value) = text(step, "ex").or_else(|| text(step, "command")) {
        return format!(":{value}");
    }
    "step".to_string()
}

fn step_summary(payload: &Value) -> String {
    let current = payload
        .get("step_index")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .saturating_add(1);
    let total = payload
        .get("step_total")
        .and_then(Value::as_u64)
        .unwrap_or(current);
    format!("{current}/{total}")
}

fn cursor_text(payload: &Value) -> String {
    payload
        .get("after_cursor")
        .or_else(|| payload.get("cursor"))
        .map_or_else(|| "?:?".to_string(), cursor_position)
}

fn cursor_position(cursor: &Value) -> String {
    let line = cursor.get("line").and_then(Value::as_u64).unwrap_or(0);
    let column = cursor.get("column").and_then(Value::as_u64).unwrap_or(0);
    format!("{line}:{column}")
}

fn push_kv<T>(rows: &mut Vec<Line>, key: &str, value: T)
where
    T: Into<String>,
{
    let value = value.into();
    if !value.is_empty() {
        rows.push(Line::from_spans(vec![
            Span::styled(format!("  {key}: "), muted()),
            Span::styled(value, value_style()),
        ]));
    }
}

fn text<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str)
}

fn count(payload: &Value, key: &str) -> usize {
    payload
        .get(key)
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}

fn header(title: &str) -> Line {
    Line::from_spans(vec![Span::styled(format!("◆ {title}"), accent_bold())])
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut output = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    output.push('…');
    output
}

fn pad_rule(prefix: &str, width: u16, fill: char, end: char) -> String {
    let width = usize::from(width.max(8));
    let mut value = prefix.to_string();
    let len = value.chars().count();
    if len < width.saturating_sub(1) {
        value.extend(std::iter::repeat_n(fill, width - len - 1));
    }
    value.push(end);
    value
}

const fn accent() -> Style {
    Style::new().fg(Color::Cyan)
}
const fn accent_bold() -> Style {
    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
}
const fn border() -> Style {
    Style::new().fg(Color::Cyan)
}
const fn muted() -> Style {
    Style::new().fg(Color::BrightBlack)
}
const fn value_style() -> Style {
    Style::new().fg(Color::White)
}
const fn cursor_line_style() -> Style {
    Style::new().fg(Color::Yellow)
}
