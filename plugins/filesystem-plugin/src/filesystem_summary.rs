//! Structural tool presentation (plugin contract v3) for filesystem visuals.
//!
//! Supplies the one-line summary and operation target the host shows for a
//! collapsed or summarized filesystem unit, for example
//! `src/picker.rs · 14 lines`. The host bounds both values; this module only
//! chooses their content.

use serde_json::Value;

/// Summary and target for a filesystem visual payload.
///
/// `display_path` renders a path relative to the working directory, as the
/// cards do. Drafts and payloads without recognizable facts supply nothing,
/// so the host keeps its generic summary sources.
#[must_use]
pub fn filesystem_tool_presentation(
    kind: &str,
    payload: &Value,
    display_path: &dyn Fn(&str) -> String,
) -> bcode_plugin_sdk::tui::PluginTuiToolPresentation {
    let path = text(payload, "path").map(display_path);
    let (target, detail) = match kind {
        "bcode.filesystem.read" | "bcode.filesystem.artifact.read" => {
            (path, read_lines(payload).map(|lines| plural(lines, "line")))
        }
        "bcode.filesystem.list" => (path, count(payload, "entries", "entry", "entries")),
        "bcode.filesystem.find" => (path, count(payload, "paths", "path", "paths")),
        "bcode.filesystem.grep" | "bcode.filesystem.artifact.grep" => {
            (path, count(payload, "matches", "match", "matches"))
        }
        "bcode.filesystem.exists" => (
            path,
            payload
                .get("exists")
                .and_then(Value::as_bool)
                .map(|exists| if exists { "exists" } else { "missing" }.to_owned()),
        ),
        "bcode.filesystem.stat" | "bcode.filesystem.artifact.metadata" => (
            path,
            payload
                .get("byte_len")
                .or_else(|| payload.get("size"))
                .and_then(Value::as_u64)
                .map(|bytes| plural(bytes, "byte")),
        ),
        "bcode.filesystem.image" => (path, text(payload, "mime_type").map(str::to_owned)),
        "bcode.filesystem.request" => {
            let arguments = payload.get("arguments").unwrap_or(payload);
            let target = text(arguments, "path")
                .map(display_path)
                .or_else(|| text(arguments, "pattern").map(str::to_owned));
            let pattern = text(arguments, "path")
                .and_then(|_| text(arguments, "pattern"))
                .map(str::to_owned);
            (target, pattern)
        }
        _ => (None, None),
    };
    let summary = match (&target, detail) {
        (Some(target), Some(detail)) => Some(format!("{target} · {detail}")),
        (Some(target), None) => Some(target.clone()),
        (None, detail) => detail,
    };
    bcode_plugin_sdk::tui::PluginTuiToolPresentation {
        summary,
        target,
        sections: Vec::new(),
    }
}

fn text<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

/// Lines returned by a read: the reported range, else the returned text.
fn read_lines(payload: &Value) -> Option<u64> {
    let number = |key: &str| payload.get(key).and_then(Value::as_u64);
    if let (Some(start), Some(end)) = (number("start_line"), number("end_line")) {
        return Some(end.saturating_sub(start).saturating_add(1));
    }
    payload
        .get("contents")
        .or_else(|| payload.get("preview"))
        .and_then(Value::as_str)
        .map(|contents| u64::try_from(contents.lines().count()).unwrap_or(u64::MAX))
}

fn count(payload: &Value, key: &str, one: &str, many: &str) -> Option<String> {
    let len = payload.get(key).and_then(Value::as_array)?.len();
    let partial = payload.get("partial").and_then(Value::as_bool) == Some(true);
    let noun = if len == 1 { one } else { many };
    Some(format!("{len}{} {noun}", if partial { "+" } else { "" }))
}

fn plural(value: u64, noun: &str) -> String {
    if value == 1 {
        format!("1 {noun}")
    } else {
        format!("{value} {noun}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn present(kind: &str, payload: &Value) -> bcode_plugin_sdk::tui::PluginTuiToolPresentation {
        filesystem_tool_presentation(kind, payload, &|path| {
            path.strip_prefix("/repo/").unwrap_or(path).to_owned()
        })
    }

    #[test]
    fn reads_summarize_path_and_line_count() {
        let ranged = present(
            "bcode.filesystem.read",
            &serde_json::json!({
                "path": "/repo/src/picker.rs", "start_line": 1, "end_line": 14, "total_lines": 90
            }),
        );
        assert_eq!(ranged.summary.as_deref(), Some("src/picker.rs · 14 lines"));
        assert_eq!(ranged.target.as_deref(), Some("src/picker.rs"));
        let text = present(
            "bcode.filesystem.read",
            &serde_json::json!({ "path": "/repo/a.txt", "contents": "one" }),
        );
        assert_eq!(text.summary.as_deref(), Some("a.txt · 1 line"));
    }

    #[test]
    fn listings_and_searches_count_results() {
        let list = present(
            "bcode.filesystem.list",
            &serde_json::json!({ "path": "/repo/src", "entries": [{}, {}, {}], "partial": true }),
        );
        assert_eq!(list.summary.as_deref(), Some("src · 3+ entries"));
        let grep = present(
            "bcode.filesystem.grep",
            &serde_json::json!({ "matches": [{ "path": "a", "line": "x" }] }),
        );
        assert_eq!(grep.summary.as_deref(), Some("1 match"));
        assert_eq!(grep.target, None);
        let exists = present(
            "bcode.filesystem.exists",
            &serde_json::json!({ "path": "/repo/gone", "exists": false }),
        );
        assert_eq!(exists.summary.as_deref(), Some("gone · missing"));
    }

    #[test]
    fn requests_target_path_then_pattern() {
        let find = present(
            "bcode.filesystem.request",
            &serde_json::json!({ "arguments": { "path": "/repo/src", "pattern": "*.rs" } }),
        );
        assert_eq!(find.target.as_deref(), Some("src"));
        assert_eq!(find.summary.as_deref(), Some("src · *.rs"));
        let pattern_only = present(
            "bcode.filesystem.request",
            &serde_json::json!({ "pattern": "TODO" }),
        );
        assert_eq!(pattern_only.target.as_deref(), Some("TODO"));
    }

    #[test]
    fn unknown_kinds_and_blank_payloads_supply_nothing() {
        assert!(
            present(
                "bcode.filesystem.request-draft.write",
                &serde_json::json!({})
            )
            .is_empty()
        );
        assert!(present("bcode.filesystem.read", &serde_json::json!({ "path": " " })).is_empty());
    }
}
