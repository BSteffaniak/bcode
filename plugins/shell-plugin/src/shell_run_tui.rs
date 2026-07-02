//! Native TUI rendering for shell run artifacts.

use bmux_tui::prelude::Line;

const DEFAULT_TERMINAL_COLUMNS: u16 = 120;
const DEFAULT_TERMINAL_ROWS: u16 = 30;

/// Native TUI visual adapter for shell run artifacts.
pub struct ShellRunTuiVisualAdapter;

impl bcode_plugin_sdk::tui::PluginTuiVisualAdapter for ShellRunTuiVisualAdapter {
    fn supports(&self, kind: &str) -> bool {
        kind == "bcode.shell.run"
    }

    fn rows(&self, _kind: &str, payload: &serde_json::Value, _width: u16) -> Vec<Line> {
        let mode = payload
            .get("mode")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let output = match mode {
            "terminal" => payload
                .get("output_tail")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            _ => serde_json::to_string_pretty(payload).unwrap_or_default(),
        };
        let columns = payload
            .get("columns")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u16::try_from(value).ok())
            .unwrap_or(DEFAULT_TERMINAL_COLUMNS);
        let rows = payload
            .get("rows")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u16::try_from(value).ok())
            .unwrap_or(DEFAULT_TERMINAL_ROWS);

        let mut lines = Vec::new();
        lines.push(Line::from(format!("Shell run · {mode}")));
        lines.extend(
            output.lines().take(usize::from(rows)).map(|line| {
                Line::from(line.chars().take(usize::from(columns)).collect::<String>())
            }),
        );
        lines
    }
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
}
