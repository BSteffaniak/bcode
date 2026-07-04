//! Native TUI rendering for shell run artifacts.

use bcode_tui_components::terminal_viewer::{TerminalViewerInput, terminal_viewer_rows};
use bmux_tui::prelude::Line;
use std::fs;

const DEFAULT_TERMINAL_COLUMNS: u16 = 120;
const DEFAULT_TERMINAL_ROWS: u16 = 30;
const TERMINAL_PTY_STREAM_REF_KEY: &str = "terminal_pty_stream";
const TERMINAL_PTY_STREAM_CONTENT_TYPE: &str = "application/x-bcode-terminal-pty-stream";

/// Native TUI visual adapter for shell run artifacts.
pub struct ShellRunTuiVisualAdapter;

impl bcode_plugin_sdk::tui::PluginTuiVisualAdapter for ShellRunTuiVisualAdapter {
    fn supports(&self, kind: &str) -> bool {
        matches!(kind, "bcode.shell.run" | "bcode.tool.request.shell.run")
    }

    fn rows(&self, kind: &str, payload: &serde_json::Value, width: u16) -> Vec<Line> {
        if kind == "bcode.tool.request.shell.run" {
            return shell_request_rows(payload, width);
        }
        let mode = payload
            .get("mode")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let columns = payload_u16(payload, "columns").unwrap_or(DEFAULT_TERMINAL_COLUMNS);
        let rows = payload_u16(payload, "rows").unwrap_or(DEFAULT_TERMINAL_ROWS);
        let output = terminal_replay_output(payload).unwrap_or_else(|| match mode {
            "terminal" => payload
                .get("output_tail")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            _ => serde_json::to_string_pretty(payload).unwrap_or_default(),
        });

        let mut lines = Vec::new();
        lines.push(Line::from(format!("Shell run · {mode}")));
        lines.extend(terminal_viewer_rows(
            TerminalViewerInput {
                output: &output,
                columns,
                rows,
                exit_code: payload
                    .get("exit_code")
                    .and_then(serde_json::Value::as_i64)
                    .and_then(|value| i32::try_from(value).ok()),
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
            },
            width,
        ));
        lines
    }
}

fn shell_request_rows(payload: &serde_json::Value, _width: u16) -> Vec<Line> {
    let arguments = payload.get("arguments").unwrap_or(payload);
    let command = arguments
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let cwd = arguments.get("cwd").and_then(serde_json::Value::as_str);
    let mut lines = vec![Line::from("Shell command")];
    if !command.is_empty() {
        lines.push(Line::from(format!("command: {command}")));
    }
    if let Some(cwd) = cwd {
        lines.push(Line::from(format!("cwd: {cwd}")));
    }
    lines
}

fn terminal_replay_output(payload: &serde_json::Value) -> Option<String> {
    let reference = terminal_replay_ref(payload)?;
    let uri = reference
        .get("storage_uri")
        .and_then(serde_json::Value::as_str)?;
    let url = url::Url::parse(uri).ok()?;
    if url.scheme() != "file" {
        return None;
    }
    fs::read_to_string(url.to_file_path().ok()?).ok()
}

fn terminal_replay_truncated(payload: &serde_json::Value) -> Option<bool> {
    terminal_replay_ref(payload)?
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
    fn adapter_renders_terminal_output_tail_from_raw_shell_run_artifact_metadata() {
        let payload = serde_json::json!({
            "mode": "terminal",
            "output_tail": "\u{1b}[31mhello\u{1b}[0m\nworld\n",
            "columns": 80,
            "rows": 24
        });
        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &ShellRunTuiVisualAdapter,
            "bcode.shell.run",
            &payload,
            100,
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(rendered.contains("Shell run"), "{rendered}");
        assert!(rendered.contains("terminal"), "{rendered}");
        assert!(rendered.contains("hello"), "{rendered}");
        assert!(rendered.contains("world"), "{rendered}");
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
            &ShellRunTuiVisualAdapter,
            "bcode.shell.run",
            &payload,
            100,
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(rendered.contains("second"), "{rendered}");
        assert!(!rendered.contains("first"), "{rendered}");
        assert!(!rendered.contains("fallback"), "{rendered}");
    }
}
