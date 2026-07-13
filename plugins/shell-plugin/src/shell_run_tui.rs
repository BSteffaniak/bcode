//! Native TUI rendering for shell run artifacts.
//!
//! Terminal replay and emulation are shell-domain behavior. This adapter is the only component
//! that may interpret shell artifact schemas and terminal recording references; generic TUI and
//! transcript code routes opaque plugin visuals without understanding those values.

use bcode_tui_components::terminal_viewer::{
    MAX_INLINE_TERMINAL_ROWS, TerminalViewerInput, TerminalViewerLiveState, TerminalViewerSizing,
    terminal_viewer_rows,
};
use bmux_tui::prelude::{Color, Line, Span, Style};
use std::collections::BTreeMap;
use std::fs;
use std::sync::Mutex;

const DEFAULT_TERMINAL_COLUMNS: u16 = 120;
const DEFAULT_TERMINAL_ROWS: u16 = 30;
const TERMINAL_PTY_STREAM_REF_KEY: &str = "terminal_pty_stream";
const TERMINAL_PTY_STREAM_CONTENT_TYPE: &str = "application/x-bcode-terminal-pty-stream";
const SHELL_RECORDING_REF_KEY: &str = "shell_recording";
const SHELL_RECORDING_CONTENT_TYPE: &str = "application/x-bcode-shell-recording";

/// Native TUI visual adapter for shell run artifacts.
#[derive(Default)]
pub struct ShellRunTuiVisualAdapter {
    live_states: Mutex<BTreeMap<String, TerminalViewerLiveState>>,
}

impl bcode_plugin_sdk::tui::PluginTuiVisualAdapter for ShellRunTuiVisualAdapter {
    fn supports(&self, kind: &str) -> bool {
        matches!(kind, "bcode.shell.run" | "bcode.tool.request.shell.run")
    }

    fn render_mode(
        &self,
        kind: &str,
        _payload: &serde_json::Value,
    ) -> bcode_plugin_sdk::tui::PluginTuiVisualRenderMode {
        if matches!(kind, "bcode.shell.run" | "bcode.tool.request.shell.run") {
            bcode_plugin_sdk::tui::PluginTuiVisualRenderMode::TranscriptBlock
        } else {
            bcode_plugin_sdk::tui::PluginTuiVisualRenderMode::Inline
        }
    }

    fn rows(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    ) -> Vec<Line> {
        let width = context.width();
        if kind == "bcode.tool.request.shell.run" {
            return self.shell_request_rows(payload, width, context);
        }
        let mode = payload
            .get("mode")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let columns = payload_u16(payload, "columns").unwrap_or(DEFAULT_TERMINAL_COLUMNS);
        let rows = payload_u16(payload, "rows").unwrap_or(DEFAULT_TERMINAL_ROWS);
        let (output, replay_error) = match terminal_replay_output(payload) {
            TerminalReplayOutput::Ready(output) => (output, None),
            TerminalReplayOutput::Unavailable(message) => (String::new(), Some(message)),
            TerminalReplayOutput::Absent => (
                match mode {
                    "terminal" => payload
                        .get("output_tail")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    _ => serde_json::to_string_pretty(payload).unwrap_or_default(),
                },
                None,
            ),
        };

        let mut lines = shell_terminal_prompt_rows(payload, width, context);
        lines.extend(shell_status_rows(payload));
        if let Some(error) = replay_error {
            lines.push(Line::from_spans(vec![Span::styled(
                format!("  durable shell recording unavailable: {error}; inline output was not substituted"),
                Style::new().fg(Color::Red),
            )]));
        }
        lines.extend(terminal_viewer_rows(
            TerminalViewerInput {
                output: &output,
                columns,
                rows,
                exit_code: payload_exit_code(payload),
                timed_out: payload
                    .get("timed_out")
                    .and_then(serde_json::Value::as_bool),
                elapsed: None,
                output_truncated: terminal_replay_truncated(payload).unwrap_or_else(|| {
                    payload
                        .get("output_truncated")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false)
                }),
                output_bytes: payload
                    .get("output_bytes")
                    .and_then(serde_json::Value::as_u64),
                retained_output_bytes: payload
                    .get("retained_output_bytes")
                    .and_then(serde_json::Value::as_u64),
                show_status: false,
                sizing: TerminalViewerSizing::Compact,
            },
            width,
        ));
        lines
    }
}

impl ShellRunTuiVisualAdapter {
    fn shell_request_rows(
        &self,
        payload: &serde_json::Value,
        width: u16,
        context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
    ) -> Vec<Line> {
        let Some(runtime) = payload.get("_bcode_runtime") else {
            return shell_terminal_prompt_rows(payload, width, context);
        };
        let output = runtime
            .get("output")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let columns = payload_u16(runtime, "columns").unwrap_or(DEFAULT_TERMINAL_COLUMNS);
        let rows = payload_u16(runtime, "rows").unwrap_or(DEFAULT_TERMINAL_ROWS);
        let streaming = runtime
            .get("streaming")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let mut input = TerminalViewerInput {
            output,
            columns,
            rows,
            exit_code: payload_exit_code(runtime),
            timed_out: runtime
                .get("timed_out")
                .and_then(serde_json::Value::as_bool),
            elapsed: None,
            output_truncated: false,
            output_bytes: None,
            retained_output_bytes: None,
            show_status: false,
            sizing: TerminalViewerSizing::Compact,
        };
        if streaming {
            let key = runtime
                .get("live_state_key")
                .and_then(serde_json::Value::as_str)
                .or_else(|| {
                    payload
                        .get("arguments")
                        .and_then(|arguments| arguments.get("command"))
                        .and_then(serde_json::Value::as_str)
                })
                .unwrap_or("shell-live-terminal");
            let visible_rows = self.live_visible_rows(key, input);
            input.sizing = TerminalViewerSizing::Live {
                visible_rows,
                max_rows: MAX_INLINE_TERMINAL_ROWS,
            };
        }
        let mut lines = shell_terminal_prompt_rows(payload, width, context);
        lines.extend(shell_status_rows(runtime));
        lines.extend(terminal_viewer_rows(input, width));
        lines
    }

    fn live_visible_rows(&self, key: &str, input: TerminalViewerInput<'_>) -> usize {
        let Ok(mut states) = self.live_states.lock() else {
            return 1;
        };
        let state = states.entry(key.to_owned()).or_default();
        state.update(input, MAX_INLINE_TERMINAL_ROWS);
        state.visible_rows()
    }
}

fn shell_status_rows(payload: &serde_json::Value) -> Vec<Line> {
    let mut parts = Vec::new();
    if payload
        .get("cancelled")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        parts.push("cancelled".to_owned());
    }
    if let Some(exit_code) = payload_exit_code(payload) {
        parts.push(format!("exit code {exit_code}"));
    }
    if parts.is_empty() {
        return Vec::new();
    }
    vec![Line::from_spans(vec![
        Span::styled("  ", muted_style()),
        Span::styled(parts.join(" · "), muted_style()),
    ])]
}

fn shell_terminal_prompt_rows(
    payload: &serde_json::Value,
    _width: u16,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
) -> Vec<Line> {
    let arguments = payload.get("arguments").unwrap_or(payload);
    let command = arguments
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let cwd = arguments.get("cwd").and_then(serde_json::Value::as_str);
    let format_commands = arguments
        .get("format_commands")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    if command.is_empty() {
        return Vec::new();
    }

    let display_command = if format_commands {
        format_shell_command_for_display(command)
    } else {
        command.to_owned()
    };
    display_command
        .lines()
        .enumerate()
        .map(|(index, line)| {
            let mut spans = if index == 0 {
                prompt_spans(cwd, context)
            } else {
                vec![Span::styled("    ", muted_style())]
            };
            spans.extend(shell_command_spans(line));
            Line::from_spans(spans)
        })
        .collect()
}

fn prompt_spans(
    cwd: Option<&str>,
    context: &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext,
) -> Vec<Span> {
    let mut spans = vec![Span::styled("  ", muted_style())];
    if let Some(cwd) = cwd {
        spans.push(Span::styled(
            context.display_path(cwd).to_string(),
            path_style(),
        ));
        spans.push(Span::styled(" ❯ ", prompt_style()));
    } else {
        spans.push(Span::styled("❯ ", prompt_style()));
    }
    spans
}

fn format_shell_command_for_display(command: &str) -> String {
    use shuck_formatter::{
        FormattedSource, IndentStyle, ShellDialect, ShellFormatOptions, format_source,
    };

    let options = ShellFormatOptions::default()
        .with_dialect(ShellDialect::Bash)
        .with_indent_style(IndentStyle::Space)
        .with_indent_width(2);
    match format_source(command, None, &options) {
        Ok(FormattedSource::Formatted(formatted)) => trim_formatted_shell_command(&formatted),
        Ok(FormattedSource::Unchanged) | Err(_) => command.to_owned(),
    }
}

fn trim_formatted_shell_command(command: &str) -> String {
    command.trim_end_matches(['\r', '\n']).to_owned()
}

fn shell_command_spans(command: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    for (index, token) in command.split_whitespace().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" "));
        }
        let style = if index == 0 {
            command_style()
        } else if token.starts_with('-') {
            flag_style()
        } else if token.starts_with('\'') || token.starts_with('"') {
            string_style()
        } else if matches!(token, "|" | "&&" | "||" | ";" | ">" | ">>" | "<") {
            operator_style()
        } else {
            argument_style()
        };
        spans.push(Span::styled(token.to_owned(), style));
    }
    spans
}

const fn muted_style() -> Style {
    Style::new().fg(Color::BrightBlack)
}

const fn path_style() -> Style {
    Style::new().fg(Color::Blue)
}

const fn prompt_style() -> Style {
    Style::new().fg(Color::Magenta)
}

const fn command_style() -> Style {
    Style::new().fg(Color::Cyan)
}

const fn flag_style() -> Style {
    Style::new().fg(Color::Yellow)
}

const fn string_style() -> Style {
    Style::new().fg(Color::Green)
}

const fn operator_style() -> Style {
    Style::new().fg(Color::BrightBlack)
}

const fn argument_style() -> Style {
    Style::new()
}

fn payload_exit_code(payload: &serde_json::Value) -> Option<i32> {
    payload
        .get("exit_code")
        .and_then(serde_json::Value::as_i64)
        .and_then(|value| i32::try_from(value).ok())
}

enum TerminalReplayOutput {
    Ready(String),
    Unavailable(String),
    Absent,
}

fn terminal_replay_output(payload: &serde_json::Value) -> TerminalReplayOutput {
    let Some(reference) = terminal_replay_ref(payload) else {
        return TerminalReplayOutput::Absent;
    };
    let authoritative =
        reference.get("key").and_then(serde_json::Value::as_str) == Some(SHELL_RECORDING_REF_KEY);
    let Some(uri) = reference
        .get("storage_uri")
        .and_then(serde_json::Value::as_str)
    else {
        return if authoritative {
            TerminalReplayOutput::Unavailable(
                "recording reference has no storage location".to_owned(),
            )
        } else {
            TerminalReplayOutput::Absent
        };
    };
    let Ok(url) = url::Url::parse(uri) else {
        return TerminalReplayOutput::Unavailable(
            "recording storage location is invalid".to_owned(),
        );
    };
    if url.scheme() != "file" {
        return TerminalReplayOutput::Unavailable(format!(
            "recording storage scheme '{}' is not available locally",
            url.scheme()
        ));
    }
    let Ok(path) = url.to_file_path() else {
        return TerminalReplayOutput::Unavailable("recording file location is invalid".to_owned());
    };
    if authoritative {
        return match crate::recording::read_recording(&path) {
            Ok((_, frames)) => {
                let bytes = frames.into_iter().fold(Vec::new(), |mut bytes, frame| {
                    if let crate::recording::ShellRecordingFrame::Output { bytes: output, .. } =
                        frame
                    {
                        bytes.extend(output);
                    }
                    bytes
                });
                TerminalReplayOutput::Ready(String::from_utf8_lossy(&bytes).into_owned())
            }
            Err(error) => TerminalReplayOutput::Unavailable(format!(
                "recording could not be validated: {error}"
            )),
        };
    }
    fs::read_to_string(path).map_or(TerminalReplayOutput::Absent, TerminalReplayOutput::Ready)
}

fn terminal_replay_truncated(payload: &serde_json::Value) -> Option<bool> {
    let reference = terminal_replay_ref(payload)?;
    if reference.get("key").and_then(serde_json::Value::as_str) == Some(SHELL_RECORDING_REF_KEY) {
        return Some(false);
    }
    reference
        .get("metadata")
        .and_then(|metadata| metadata.get("tail_truncated"))
        .and_then(serde_json::Value::as_bool)
}

fn terminal_replay_ref(payload: &serde_json::Value) -> Option<&serde_json::Value> {
    payload
        .get("_artifact_refs")
        .and_then(serde_json::Value::as_array)?
        .iter()
        .find(|reference| {
            reference.get("key").and_then(serde_json::Value::as_str)
                == Some(SHELL_RECORDING_REF_KEY)
                || reference
                    .get("content_type")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|content_type| {
                        content_type.starts_with(SHELL_RECORDING_CONTENT_TYPE)
                    })
                || reference.get("key").and_then(serde_json::Value::as_str)
                    == Some(TERMINAL_PTY_STREAM_REF_KEY)
                || reference
                    .get("content_type")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|content_type| {
                        content_type.starts_with(TERMINAL_PTY_STREAM_CONTENT_TYPE)
                    })
                || reference
                    .get("metadata")
                    .and_then(|metadata| metadata.get("stream"))
                    .and_then(serde_json::Value::as_str)
                    == Some("pty")
        })
}

fn payload_u16(payload: &serde_json::Value, key: &str) -> Option<u16> {
    payload
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn adapter_formats_shell_commands_by_default() {
        let payload = serde_json::json!({
            "command": "if true;then echo 'hello world';else echo nope;fi",
            "cwd": "/Users/example/project"
        });
        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &ShellRunTuiVisualAdapter::default(),
            "bcode.tool.request.shell.run",
            &payload,
            &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                64,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
                None,
            ),
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(rendered.contains("; then"), "{rendered}");
        assert!(rendered.contains("; else"), "{rendered}");
        assert!(!rendered.contains(";then"), "{rendered}");
    }

    #[test]
    fn adapter_preserves_unformatted_shell_commands_when_disabled() {
        let command = "if true;then echo 'hello world';else echo nope;fi";
        let payload = serde_json::json!({
            "command": command,
            "format_commands": false
        });
        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &ShellRunTuiVisualAdapter::default(),
            "bcode.tool.request.shell.run",
            &payload,
            &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                48,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
                None,
            ),
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert_eq!(rendered.lines().count(), 1, "{rendered}");
        assert!(rendered.contains(command), "{rendered}");
    }

    #[test]
    fn adapter_does_not_split_quoted_shell_tokens() {
        let payload = serde_json::json!({
            "command": "printf 'hello world from a quoted argument' && echo done"
        });
        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &ShellRunTuiVisualAdapter::default(),
            "bcode.tool.request.shell.run",
            &payload,
            &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                32,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
                None,
            ),
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(
            rendered.contains("'hello world from a quoted argument'"),
            "{rendered}"
        );
    }

    #[test]
    fn adapter_renders_terminal_output_tail_from_raw_shell_run_artifact_metadata() {
        let payload = serde_json::json!({
            "mode": "terminal",
            "output_tail": "\u{1b}[31mhello\u{1b}[0m\nworld\n",
            "columns": 80,
            "rows": 24
        });
        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &ShellRunTuiVisualAdapter::default(),
            "bcode.shell.run",
            &payload,
            &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                100,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
                None,
            ),
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(rendered.contains("hello"), "{rendered}");
        assert!(rendered.contains("world"), "{rendered}");
    }

    #[test]
    fn corrupt_authoritative_recording_is_explicit_and_never_falls_back() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let path = temp_dir.path().join("recording.bcsr");
        fs::write(&path, b"corrupt").expect("write corrupt recording");
        let payload = serde_json::json!({
            "mode": "terminal",
            "output_tail": "forbidden fallback sentinel",
            "columns": 80,
            "rows": 24,
            "_artifact_refs": [{
                "key": SHELL_RECORDING_REF_KEY,
                "content_type": SHELL_RECORDING_CONTENT_TYPE,
                "storage_uri": url::Url::from_file_path(&path).ok().map(|url| url.to_string()),
                "metadata": {"complete": true}
            }]
        });
        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &ShellRunTuiVisualAdapter::default(),
            "bcode.shell.run",
            &payload,
            &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                100,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
                None,
            ),
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            rendered.contains("durable shell recording unavailable"),
            "{rendered}"
        );
        assert!(rendered.contains("could not be validated"), "{rendered}");
        assert!(
            !rendered.contains("forbidden fallback sentinel"),
            "{rendered}"
        );
    }

    #[test]
    fn fresh_adapter_renders_complete_shell_recording() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let path = temp_dir.path().join("recording.bcsr");
        let mut writer = crate::recording::ShellRecordingWriter::create(&path, 80, 24)
            .expect("recording writer");
        writer
            .write_output(1, b"first\rsecond\n")
            .expect("record output");
        writer
            .finish(2, Some(0), None, false, false)
            .expect("finish recording");
        let payload = serde_json::json!({
            "mode": "terminal",
            "output_tail": "fallback\n",
            "columns": 80,
            "rows": 24,
            "_artifact_refs": [{
                "key": SHELL_RECORDING_REF_KEY,
                "content_type": SHELL_RECORDING_CONTENT_TYPE,
                "storage_uri": url::Url::from_file_path(&path).ok().map(|url| url.to_string()),
                "metadata": {"complete": true}
            }]
        });
        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &ShellRunTuiVisualAdapter::default(),
            "bcode.shell.run",
            &payload,
            &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                100,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
                None,
            ),
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("second"), "{rendered}");
        assert!(!rendered.contains("first"), "{rendered}");
        assert!(!rendered.contains("fallback"), "{rendered}");
    }

    #[test]
    fn adapter_renders_terminal_replay_ref_through_terminal_viewer() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let path = temp_dir.path().join("pty.txt");
        fs::write(&path, "first\rsecond\n").expect("write pty");
        let payload = serde_json::json!({
            "mode": "terminal",
            "output_tail": "fallback\n",
            "columns": 80,
            "rows": 24,
            "_artifact_refs": [{
                "key": TERMINAL_PTY_STREAM_REF_KEY,
                "content_type": "application/x-bcode-terminal-pty-stream; charset=utf-8",
                "storage_uri": url::Url::from_file_path(&path).ok().map(|url| url.to_string()),
                "byte_len": 13,
                "metadata": {"stream": "pty", "tail_truncated": false}
            }]
        });
        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &ShellRunTuiVisualAdapter::default(),
            "bcode.shell.run",
            &payload,
            &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                100,
                bcode_plugin_sdk::tui::PluginTuiDiffLayout::Auto { breakpoint: 120 },
                None,
            ),
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(rendered.contains("second"), "{rendered}");
        assert!(!rendered.contains("first"), "{rendered}");
        assert!(!rendered.contains("fallback"), "{rendered}");
    }
}
