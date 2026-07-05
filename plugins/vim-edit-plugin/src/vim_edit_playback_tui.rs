//! Native TUI rendering for Vim edit playback artifacts.

use bmux_tui::prelude::Line;

/// Vim edit playback TUI visual adapter.
pub struct VimEditPlaybackTuiVisualAdapter;

impl bcode_plugin_sdk::tui::PluginTuiVisualAdapter for VimEditPlaybackTuiVisualAdapter {
    fn supports(&self, kind: &str) -> bool {
        matches!(kind, "bcode.vim-edit.playback" | "bcode.vim-edit.change")
    }

    fn rows(&self, _kind: &str, payload: &serde_json::Value, _width: u16) -> Vec<Line> {
        let mut rows = Vec::new();
        rows.push(Line::from("Vim edit playback"));

        let path = payload
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<path>");
        rows.push(Line::from(format!("Path: {path}")));

        let success = payload
            .get("success")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let changed = payload
            .get("changed")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        rows.push(Line::from(format!(
            "Status: success={success}, changed={changed}"
        )));

        if let Some(error) = payload.get("error").and_then(serde_json::Value::as_str) {
            rows.push(Line::from(format!("Error: {error}")));
        }

        if let Some(events) = payload.get("events").and_then(serde_json::Value::as_array) {
            rows.push(Line::from("Steps:"));
            for event in events.iter().take(12) {
                rows.push(Line::from(format_step(event)));
            }
            if events.len() > 12 {
                rows.push(Line::from(format!("… {} more steps", events.len() - 12)));
            }
            rows.push(Line::from(
                "Cursor playback: step rows show after-cursor positions.",
            ));
            rows.push(Line::from(format!(
                "Search highlights: {}",
                search_highlight_text(events)
            )));
            rows.push(Line::from(format!(
                "Selected ranges: {}",
                selected_ranges_text(payload)
            )));
            rows.push(Line::from(format!(
                "Command-line text: {}",
                command_line_text(events)
            )));
            rows.push(Line::from(format!(
                "Incremental edits: {} changed step(s)",
                events
                    .iter()
                    .filter(|event| event
                        .get("changed")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false))
                    .count()
            )));
        }

        if let Some(cursor) = payload.get("cursor") {
            rows.push(Line::from(format!("Final cursor: {}", cursor_text(cursor))));
        }
        if let Some(mode) = payload.get("nvim_mode").and_then(serde_json::Value::as_str) {
            rows.push(Line::from(format!("Final mode: {mode}")));
        }

        if let Some(context) = payload.get("final_context") {
            rows.push(Line::from("Final context:"));
            rows.extend(context_rows(context).into_iter().take(8));
        }

        if let Some(diff) = payload.get("diff").and_then(serde_json::Value::as_str) {
            rows.push(Line::from("Final diff:"));
            rows.extend(
                diff.lines()
                    .take(40)
                    .map(|line| Line::from(line.to_string())),
            );
        }

        if payload.get("playback_controls").is_some() {
            rows.push(Line::from(
                "Playback controls: first / previous / next / last.",
            ));
        }

        rows
    }
}

fn format_step(event: &serde_json::Value) -> String {
    let index = event
        .get("step_index")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let mode = event
        .get("nvim_mode")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");
    let changed = event
        .get("changed")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let step = event
        .get("step")
        .map_or_else(|| "<step>".to_string(), step_text);
    let cursor = event
        .get("after_cursor")
        .map_or_else(|| "?:?".to_string(), cursor_text);
    format!("  {index}: {step} -> cursor {cursor}, mode {mode}, changed={changed}")
}

fn step_text(step: &serde_json::Value) -> String {
    step.get("keys")
        .and_then(serde_json::Value::as_str)
        .map_or_else(
            || {
                step.get("insert")
                    .and_then(serde_json::Value::as_str)
                    .map_or_else(
                        || {
                            step.get("ex")
                                .and_then(serde_json::Value::as_str)
                                .map_or_else(|| step.to_string(), |ex| format!("ex {ex:?}"))
                        },
                        |insert| format!("insert {} chars", insert.chars().count()),
                    )
            },
            |keys| format!("keys {keys:?}"),
        )
}

fn search_highlight_text(events: &[serde_json::Value]) -> String {
    let searches = events
        .iter()
        .filter_map(|event| event.get("step"))
        .filter_map(|step| step.get("keys").and_then(serde_json::Value::as_str))
        .filter(|keys| keys.starts_with('/'))
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if searches.is_empty() {
        "none".to_string()
    } else {
        searches.join(", ")
    }
}

fn selected_ranges_text(payload: &serde_json::Value) -> String {
    let Some(ranges) = payload
        .get("selected_ranges")
        .and_then(serde_json::Value::as_array)
    else {
        return "none".to_string();
    };
    if ranges.is_empty() {
        "none".to_string()
    } else {
        ranges
            .iter()
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn command_line_text(events: &[serde_json::Value]) -> String {
    let commands = events
        .iter()
        .filter_map(|event| event.get("step"))
        .filter_map(|step| step.get("ex").and_then(serde_json::Value::as_str))
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if commands.is_empty() {
        "none".to_string()
    } else {
        commands.join(" | ")
    }
}

fn cursor_text(cursor: &serde_json::Value) -> String {
    let line = cursor
        .get("line")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let column = cursor
        .get("column")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    format!("{line}:{column}")
}

fn context_rows(context: &serde_json::Value) -> Vec<Line> {
    let start_line = context
        .get("start_line")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1);
    context
        .get("lines")
        .and_then(serde_json::Value::as_array)
        .map(|lines| {
            lines
                .iter()
                .enumerate()
                .filter_map(|(offset, line)| {
                    let text = line.as_str()?;
                    let number =
                        start_line.saturating_add(u64::try_from(offset).unwrap_or(u64::MAX));
                    Some(Line::from(format!("  {number}: {text}")))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref() as &str)
            .collect::<String>()
    }

    #[test]
    fn adapter_renders_playback_payload() {
        let payload = serde_json::json!({
            "success": true,
            "playback_controls": { "available": ["first", "previous", "next", "last"] },
            "path": "src/lib.rs",
            "changed": true,
            "cursor": { "line": 2, "column": 4 },
            "nvim_mode": "n",
            "final_context": { "start_line": 1, "lines": ["one", "two"] },
            "events": [{
                "step_index": 0,
                "step": { "keys": "w" },
                "after_cursor": { "line": 1, "column": 5 },
                "nvim_mode": "n",
                "changed": false
            }],
            "diff": "+two"
        });
        let rows = bcode_plugin_sdk::tui::PluginTuiVisualAdapter::rows(
            &VimEditPlaybackTuiVisualAdapter,
            "bcode.vim-edit.playback",
            &payload,
            80,
        );
        let rendered = rows.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("src/lib.rs"), "{rendered}");
        assert!(rendered.contains("keys"), "{rendered}");
        assert!(rendered.contains("Final diff"), "{rendered}");
        assert!(rendered.contains("Cursor playback"), "{rendered}");
        assert!(rendered.contains("Search highlights"), "{rendered}");
        assert!(rendered.contains("Command-line text"), "{rendered}");
    }
}
