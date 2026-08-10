//! Native TUI rendering for Vim edit visuals and playback interaction.

use bcode_tui_components::tool_card::{push_tool_card_detail, tool_card_header};
use bmux_tui::prelude::{Color, Line, Modifier, Span, Style};
use serde_json::Value;

thread_local! {
    static ACTIVE_THEME: std::cell::Cell<Option<bcode_plugin_sdk::tui::PluginTuiTheme>> = const {
        std::cell::Cell::new(None)
    };
}

use super::{
    VIM_EDIT_LIVE_SCHEMA, VIM_EDIT_PLAYBACK_SCHEMA, VIM_EDIT_REQUEST_APPLY_SCHEMA,
    VIM_EDIT_REQUEST_DRAFT_APPLY_SCHEMA, VIM_EDIT_REQUEST_DRAFT_PREVIEW_SCHEMA,
    VIM_EDIT_REQUEST_PREVIEW_SCHEMA,
};

/// Vim edit TUI visual adapter.
pub struct VimEditPlaybackTuiVisualAdapter;

impl bcode_plugin_sdk::tui::PluginTuiVisualAdapter for VimEditPlaybackTuiVisualAdapter {
    fn supports(&self, kind: &str) -> bool {
        matches!(
            kind,
            VIM_EDIT_REQUEST_PREVIEW_SCHEMA
                | VIM_EDIT_REQUEST_APPLY_SCHEMA
                | VIM_EDIT_REQUEST_DRAFT_PREVIEW_SCHEMA
                | VIM_EDIT_REQUEST_DRAFT_APPLY_SCHEMA
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

    fn rows(
        &self,
        kind: &str,
        payload: &Value,
        context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    ) -> Vec<Line> {
        ACTIVE_THEME.with(|theme| theme.set(context.theme()));
        let width = context.width();
        let rows = match kind {
            VIM_EDIT_REQUEST_PREVIEW_SCHEMA => request_rows("Vim edit preview", payload, context),
            VIM_EDIT_REQUEST_APPLY_SCHEMA => request_rows("Vim edit apply", payload, context),
            VIM_EDIT_REQUEST_DRAFT_PREVIEW_SCHEMA => {
                request_draft_rows("Vim edit preview", payload, context)
            }
            VIM_EDIT_REQUEST_DRAFT_APPLY_SCHEMA => {
                request_draft_rows("Vim edit apply", payload, context)
            }
            VIM_EDIT_LIVE_SCHEMA => live_rows(payload, width, context),
            VIM_EDIT_PLAYBACK_SCHEMA | "bcode.vim-edit.change" => {
                playback_rows(payload, None, true, true, width, context)
            }
            _ => Vec::new(),
        };
        ACTIVE_THEME.with(|theme| theme.set(None));
        rows
    }
}

#[cfg(test)]
const fn unknown_visual_context() -> bcode_plugin_sdk::tui::PluginTuiVisualRenderContext {
    bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
        u16::MAX,
        bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
        None,
    )
}

fn request_draft_rows(
    title: &str,
    payload: &Value,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
) -> Vec<Line> {
    let preview = text(payload, "preview").unwrap_or_default();
    let parsed = serde_json::from_str::<Value>(preview).ok();
    let arguments = parsed.as_ref().unwrap_or(payload);
    let mut rows = vec![header(&format!("{title} · assembling…"))];
    if let Some(path) = text(arguments, "path") {
        push_kv(&mut rows, "file", context.display_path(path).to_string());
        push_kv(&mut rows, "steps", count(arguments, "steps").to_string());
    }
    if let Some(files) = arguments.get("files").and_then(Value::as_array) {
        push_kv(&mut rows, "files", files.len().to_string());
        for file in files.iter().take(8) {
            let path = text(file, "path").unwrap_or("<path>");
            let steps = count(file, "steps");
            rows.push(Line::from_spans(vec![
                Span::styled("  ◆ ", accent()),
                Span::styled(context.display_path(path).to_string(), value_style()),
                Span::styled(format!("  {steps} steps"), muted()),
            ]));
        }
    }
    push_kv(
        &mut rows,
        "received",
        payload
            .get("argument_bytes")
            .and_then(Value::as_u64)
            .unwrap_or_default()
            .to_string(),
    );
    if payload.get("truncated").and_then(Value::as_bool) == Some(true) {
        push_kv(&mut rows, "truncated", "yes".to_owned());
    }
    if parsed.is_none() && !preview.is_empty() {
        push_kv(&mut rows, "state", "incomplete arguments".to_owned());
    }
    rows
}

fn request_rows(
    title: &str,
    payload: &Value,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
) -> Vec<Line> {
    let arguments = payload.get("arguments").unwrap_or(payload);
    let mut rows = vec![header(title)];
    if let Some(path) = text(arguments, "path") {
        push_kv(&mut rows, "file", context.display_path(path).to_string());
        push_kv(&mut rows, "steps", count(arguments, "steps").to_string());
    }
    if let Some(files) = arguments.get("files").and_then(Value::as_array) {
        push_kv(&mut rows, "files", files.len().to_string());
        for file in files.iter().take(8) {
            let path = text(file, "path").unwrap_or("<path>");
            let steps = count(file, "steps");
            rows.push(Line::from_spans(vec![
                Span::styled("  ◆ ", accent()),
                Span::styled(context.display_path(path).to_string(), value_style()),
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

fn live_rows(
    payload: &Value,
    width: u16,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
) -> Vec<Line> {
    if selected_context(payload).is_none() && payload.get("cursor").is_none() {
        return live_lifecycle_rows(payload, width, context);
    }
    let mut rows = vim_screen_rows(
        "nvim live",
        payload,
        selected_context(payload),
        width,
        context,
    );
    rows.push(Line::from_spans(vec![
        Span::styled("  step ", muted()),
        Span::styled(step_summary(payload), accent()),
        Span::styled(" · ", muted()),
        Span::styled(step_text(payload), value_style()),
    ]));
    rows
}

fn live_lifecycle_rows(
    payload: &Value,
    width: u16,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
) -> Vec<Line> {
    let phase = text(payload, "phase").unwrap_or("running");
    let path = context
        .display_path(text(payload, "path").unwrap_or("<file>"))
        .to_string();
    let title = format!("╭─ nvim live: {path} ── {phase} ");
    let mut rows = vec![Line::from_spans(vec![Span::styled(
        pad_rule(&title, width, '─', '╮'),
        border(),
    )])];
    push_kv(&mut rows, "phase", phase);
    if let Some(tool_name) = text(payload, "tool_name") {
        push_kv(&mut rows, "tool", tool_name);
    }
    if let Some(error) = text(payload, "error").filter(|error| !error.is_empty()) {
        push_kv(&mut rows, "error", error);
    } else if phase == "started" {
        push_kv(&mut rows, "status", "starting Neovim");
    }
    rows.push(Line::from_spans(vec![Span::styled(
        pad_rule("╰", width, '─', '╯'),
        border(),
    )]));
    rows
}

fn playback_rows(
    payload: &Value,
    selected_frame: Option<usize>,
    show_timeline: bool,
    show_diff: bool,
    width: u16,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
) -> Vec<Line> {
    let frame = selected_frame.and_then(|index| event(payload, index));
    let source = frame.unwrap_or(payload);
    let mut rows = vim_screen_rows(
        "nvim playback",
        payload,
        selected_context(source),
        width,
        context,
    );
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
    rows.push(playback_control_row(payload));
    rows
}

fn playback_control_row(payload: &Value) -> Line {
    let preview = text(payload, "tool_mode") == Some("preview");
    let mut spans = vec![
        Span::styled(" [First] ", muted()),
        Span::styled("[Prev] ", muted()),
        Span::styled("[Play] ", accent()),
        Span::styled("[Next] ", muted()),
        Span::styled("[Last] ", muted()),
        Span::styled("[Prev Δ] ", muted()),
        Span::styled("[Next Δ] ", muted()),
        Span::styled("[Timeline] ", muted()),
        Span::styled("[Diff] ", muted()),
    ];
    if preview {
        spans.push(Span::styled("[Apply] ", accent()));
    }
    spans.push(Span::styled("[Close]", muted()));
    Line::from_spans(spans)
}

fn vim_screen_rows(
    title: &str,
    payload: &Value,
    selected: Option<&Value>,
    width: u16,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
) -> Vec<Line> {
    let path = context
        .display_path(text(payload, "path").unwrap_or("<file>"))
        .to_string();
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
    if let Some(context) = selected {
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
    push_tool_card_detail(rows, key, Some(&value), muted(), value_style());
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
    tool_card_header(
        Span::styled("◆ ", accent_bold()),
        Span::styled(title.to_owned(), accent_bold()),
    )
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

fn theme_style(
    select: impl FnOnce(bcode_plugin_sdk::tui::PluginTuiTheme) -> Style,
    fallback: Style,
) -> Style {
    ACTIVE_THEME.with(|theme| theme.get().map_or(fallback, select))
}

fn accent() -> Style {
    theme_style(|theme| theme.focused, Style::new().fg(Color::Cyan))
}
fn accent_bold() -> Style {
    accent().add_modifier(Modifier::BOLD)
}
fn border() -> Style {
    theme_style(|theme| theme.border, Style::new().fg(Color::Cyan))
}
fn muted() -> Style {
    theme_style(|theme| theme.muted, Style::new().fg(Color::BrightBlack))
}
fn value_style() -> Style {
    theme_style(|theme| theme.text, Style::new().fg(Color::White))
}
fn cursor_line_style() -> Style {
    theme_style(|theme| theme.selection, Style::new().fg(Color::Yellow))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_plugin_sdk::tui::PluginTuiVisualAdapter;
    use serde_json::json;

    fn row_text(rows: &[Line]) -> String {
        rows.iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn test_theme() -> bcode_plugin_sdk::tui::PluginTuiTheme {
        use bcode_plugin_sdk::tui::{
            PluginTuiDiffTheme, PluginTuiSourceTheme, PluginTuiSyntaxColor, PluginTuiSyntaxTheme,
            PluginTuiTheme,
        };
        let style = Style::new();
        let syntax = PluginTuiSyntaxColor::from_tui(Color::Default);
        PluginTuiTheme {
            component_theme_version: bcode_plugin_sdk::tui::PLUGIN_TUI_COMPONENT_THEME_VERSION,
            canvas: style,
            text: style.fg(Color::Blue),
            muted: style.fg(Color::BrightBlack),
            border: style.fg(Color::Yellow),
            focused: style.fg(Color::Magenta),
            selection: style.fg(Color::Green),
            source: PluginTuiSourceTheme {
                source: style,
                border: style,
                gutter: style,
                truncated: style,
            },
            diff: PluginTuiDiffTheme {
                text: style,
                muted: style,
                title: style,
                label: style,
                added: style,
                removed: style,
                hunk: style,
                added_row: style,
                removed_row: style,
                added_emphasis: style,
                removed_emphasis: style,
            },
            syntax: PluginTuiSyntaxTheme {
                text: syntax,
                comment: syntax,
                keyword: syntax,
                function: syntax,
                variable: syntax,
                string: syntax,
                number: syntax,
                type_name: syntax,
                operator: syntax,
                punctuation: syntax,
            },
        }
    }

    #[test]
    fn visual_adapter_uses_host_theme_for_tool_card_roles() {
        let payload = json!({
            "arguments": {
                "path": "/tmp/demo.txt",
                "steps": [{"keys": "w"}],
                "sandbox": "default"
            }
        });
        let context = unknown_visual_context().with_theme(test_theme());
        let rows = VimEditPlaybackTuiVisualAdapter.rows(
            VIM_EDIT_REQUEST_PREVIEW_SCHEMA,
            &payload,
            &context,
        );

        assert_eq!(rows[0].spans[0].style.fg, Some(Color::Magenta));
        assert!(rows.iter().flat_map(|line| &line.spans).any(|span| {
            span.content.contains("sandbox") && span.style.fg == Some(Color::BrightBlack)
        }));
        assert!(rows.iter().flat_map(|line| &line.spans).any(|span| {
            span.content.contains("default") && span.style.fg == Some(Color::Blue)
        }));
    }

    #[test]
    fn incomplete_request_draft_renders_separately_from_execution_frames() {
        let payload = json!({
            "preview": "{\"path\":\"/tmp/demo.txt\",\"steps\":[",
            "argument_bytes": 38,
            "truncated": false,
        });
        let text = row_text(&request_draft_rows(
            "Vim edit preview",
            &payload,
            &unknown_visual_context(),
        ));
        assert!(text.contains("assembling"), "{text}");
        assert!(text.contains("received"), "{text}");
        assert!(text.contains("incomplete arguments"), "{text}");
        assert!(!text.contains("nvim live"), "{text}");
    }

    #[test]
    fn complete_request_draft_renders_known_paths_and_steps() {
        let payload = json!({
            "preview": "{\"path\":\"/tmp/demo.txt\",\"steps\":[{\"keys\":\"w\"}]}",
            "argument_bytes": 54,
            "truncated": true,
        });
        let text = row_text(&request_draft_rows(
            "Vim edit apply",
            &payload,
            &unknown_visual_context(),
        ));
        assert!(text.contains("demo.txt"), "{text}");
        assert!(text.contains("steps"), "{text}");
        assert!(text.contains("truncated"), "{text}");
    }

    #[test]
    fn sparse_started_live_payload_renders_status_not_fake_vim_state() {
        let payload = json!({
            "tool_name": "vim_edit.apply",
            "phase": "started",
            "path": "/tmp/demo.txt",
            "error": null,
        });
        let text = row_text(&live_rows(&payload, 80, &unknown_visual_context()));
        assert!(text.contains("phase"));
        assert!(text.contains("started"));
        assert!(text.contains("starting Neovim"));
        assert!(!text.contains("?:?"), "{text}");
        assert!(!text.contains("step 1/1"), "{text}");
    }

    #[test]
    fn sparse_error_live_payload_renders_error_not_fake_vim_state() {
        let payload = json!({
            "tool_name": "vim_edit.apply",
            "phase": "error",
            "path": "/tmp/demo.txt",
            "error": "nvim not found",
        });
        let text = row_text(&live_rows(&payload, 80, &unknown_visual_context()));
        assert!(text.contains("error"));
        assert!(text.contains("nvim not found"));
        assert!(!text.contains("?:?"), "{text}");
        assert!(!text.contains("step 1/1"), "{text}");
    }

    #[test]
    fn rich_live_payload_still_renders_vim_context() {
        let payload = json!({
            "tool_name": "vim_edit.apply",
            "phase": "running",
            "path": "/tmp/demo.txt",
            "step_index": 0,
            "step_total": 1,
            "step": { "insert": { "text": "hello" } },
            "cursor": { "line": 1, "column": 6 },
            "nvim_mode": "i",
            "context": {
                "start_line": 1,
                "lines": ["hello"]
            }
        });
        let text = row_text(&live_rows(&payload, 80, &unknown_visual_context()));
        assert!(text.contains("hello"));
        assert!(text.contains("1:6"));
        assert!(text.contains("step"));
    }

    #[test]
    #[ignore = "manual deterministic Vim diff parsing and layout baseline"]
    fn diff_layout_work_per_revision_baseline_report() {
        use std::fmt::Write as _;

        let diff = (0..256).fold(String::new(), |mut output, index| {
            let _ = writeln!(output, "-{index}: old value\n+{index}: new value");
            output
        });
        let revisions = 100_usize;
        let started = std::time::Instant::now();
        let mut emitted_rows = 0_usize;
        for revision in 0..revisions {
            let rows = diff_rows(&format!("@@ revision {revision} @@\n{diff}"), 100);
            emitted_rows = emitted_rows.saturating_add(rows.len());
        }
        println!(
            "BCODE_PERF_CASE {}",
            serde_json::json!({
                "domain": "renderer_parse_layout",
                "format": "diff",
                "revisions": revisions,
                "input_bytes_per_revision": diff.len(),
                "emitted_rows": emitted_rows,
                "parse_layout_us": u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            })
        );
    }
}
