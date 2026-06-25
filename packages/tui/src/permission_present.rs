//! Permission dialog presentation models.

use bcode_session_models::ToolRequestPresentationMetadata;
use serde_json::Value;

use super::tool_present::{ToolRequestPresentation, tool_request_presentation};
use super::transcript::pretty_jsonish;
use crate::time_format::format_millis;
use bmux_tui::text_width::truncate_to_display_width;

/// One labeled permission-detail row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionDetail {
    /// Field label.
    pub label: String,
    /// Field value.
    pub value: String,
}

impl PermissionDetail {
    /// Create a permission detail row.
    #[must_use]
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
        }
    }
}

/// Structured presentation for a pending permission request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionPresentation {
    /// Human-readable title.
    pub title: String,
    /// Tool side-effect/risk label.
    pub risk: String,
    /// Primary details to render in the dialog body.
    pub details: Vec<PermissionDetail>,
    /// Optional fallback raw detail block.
    pub raw_details: Option<String>,
}

/// Build a structured permission presentation from a tool name and arguments.
#[must_use]
pub fn permission_presentation(
    tool_name: &str,
    arguments_json: &str,
    request_presentation: Option<&ToolRequestPresentationMetadata>,
) -> PermissionPresentation {
    if let Some(presentation) =
        tool_request_presentation(tool_name, arguments_json, request_presentation)
    {
        return presentation_from_tool(tool_name, &presentation);
    }

    PermissionPresentation {
        title: tool_name.to_owned(),
        risk: "tool request".to_owned(),
        details: generic_json_details(arguments_json),
        raw_details: Some(pretty_jsonish(arguments_json)),
    }
}

fn presentation_from_tool(
    tool_name: &str,
    presentation: &ToolRequestPresentation,
) -> PermissionPresentation {
    match presentation {
        ToolRequestPresentation::ShellRun {
            command,
            cwd,
            timeout_ms,
        } => shell_permission(tool_name, command, cwd.as_deref(), *timeout_ms),
        ToolRequestPresentation::Read { path } => {
            filesystem_path_permission(tool_name, "read file", path)
        }
        ToolRequestPresentation::Exists { path } | ToolRequestPresentation::Stat { path } => {
            filesystem_path_permission(tool_name, "inspect path", path)
        }
        ToolRequestPresentation::Write { path, bytes, lines } => {
            write_permission(tool_name, path, *bytes, *lines)
        }
        ToolRequestPresentation::List {
            path,
            recursive,
            max_entries,
        } => list_permission(tool_name, path, *recursive, *max_entries),
        ToolRequestPresentation::Find {
            path,
            pattern,
            max_results,
        } => find_permission(tool_name, path, pattern, *max_results),
        ToolRequestPresentation::Grep {
            path,
            pattern,
            glob,
            ignore_case,
            max_matches,
        } => grep_permission(
            tool_name,
            path,
            pattern,
            glob.as_deref(),
            *ignore_case,
            *max_matches,
        ),
        ToolRequestPresentation::WebSearch {
            query,
            provider,
            max_results,
        } => web_search_permission(tool_name, query, provider.as_deref(), *max_results),
        ToolRequestPresentation::WebFetch {
            url,
            max_bytes,
            render,
        } => web_fetch_permission(tool_name, url, *max_bytes, *render),
        ToolRequestPresentation::GitClone {
            url,
            git_ref,
            destination,
        } => git_clone_permission(tool_name, url, git_ref.as_deref(), destination.as_deref()),
        ToolRequestPresentation::DocumentExtract {
            url,
            path,
            max_bytes,
        } => document_extract_permission(tool_name, url.as_deref(), path.as_deref(), *max_bytes),
        ToolRequestPresentation::Generic { title, fields } => PermissionPresentation {
            title: title.clone(),
            risk: "tool request".to_owned(),
            details: fields
                .iter()
                .map(|(label, value)| PermissionDetail::new(label.clone(), value.clone()))
                .collect(),
            raw_details: None,
        },
    }
}

fn git_clone_permission(
    tool_name: &str,
    url: &str,
    git_ref: Option<&str>,
    destination: Option<&str>,
) -> PermissionPresentation {
    let mut details = vec![PermissionDetail::new("url", url.to_owned())];
    if let Some(git_ref) = git_ref {
        details.push(PermissionDetail::new("ref", git_ref.to_owned()));
    }
    details.push(PermissionDetail::new(
        "destination",
        destination
            .unwrap_or("Bcode session artifact state")
            .to_owned(),
    ));
    PermissionPresentation {
        title: tool_name.to_owned(),
        risk: "clone git repository into artifacts".to_owned(),
        details,
        raw_details: None,
    }
}

fn document_extract_permission(
    tool_name: &str,
    url: Option<&str>,
    path: Option<&str>,
    max_bytes: Option<u64>,
) -> PermissionPresentation {
    let mut details = Vec::new();
    if let Some(url) = url {
        details.push(PermissionDetail::new("url", url.to_owned()));
    }
    if let Some(path) = path {
        details.push(PermissionDetail::new("path", path.to_owned()));
    }
    if let Some(max_bytes) = max_bytes {
        details.push(PermissionDetail::new("max bytes", max_bytes.to_string()));
    }
    PermissionPresentation {
        title: tool_name.to_owned(),
        risk: "extract document into artifacts".to_owned(),
        details,
        raw_details: None,
    }
}

fn web_search_permission(
    tool_name: &str,
    query: &str,
    provider: Option<&str>,
    max_results: Option<u64>,
) -> PermissionPresentation {
    let mut details = vec![PermissionDetail::new("query", query.to_owned())];
    if let Some(provider) = provider {
        details.push(PermissionDetail::new("provider", provider.to_owned()));
    }
    if let Some(max_results) = max_results {
        details.push(PermissionDetail::new(
            "max results",
            max_results.to_string(),
        ));
    }
    PermissionPresentation {
        title: tool_name.to_owned(),
        risk: "web search".to_owned(),
        details,
        raw_details: None,
    }
}

fn web_fetch_permission(
    tool_name: &str,
    url: &str,
    max_bytes: Option<u64>,
    render: bool,
) -> PermissionPresentation {
    let mut details = vec![PermissionDetail::new("url", url.to_owned())];
    if let Some(max_bytes) = max_bytes {
        details.push(PermissionDetail::new("max bytes", max_bytes.to_string()));
    }
    details.push(PermissionDetail::new(
        "rendered",
        if render { "yes" } else { "no" },
    ));
    PermissionPresentation {
        title: tool_name.to_owned(),
        risk: if render {
            "rendered web fetch".to_owned()
        } else {
            "web fetch".to_owned()
        },
        details,
        raw_details: None,
    }
}

fn shell_permission(
    tool_name: &str,
    command: &str,
    cwd: Option<&str>,
    timeout_ms: Option<u64>,
) -> PermissionPresentation {
    let mut details = vec![PermissionDetail::new("command", command.to_owned())];
    if let Some(cwd) = cwd {
        details.push(PermissionDetail::new("cwd", cwd.to_owned()));
    }
    if let Some(timeout_ms) = timeout_ms {
        details.push(PermissionDetail::new("timeout", format_millis(timeout_ms)));
    }
    PermissionPresentation {
        title: tool_name.to_owned(),
        risk: "execute process".to_owned(),
        details,
        raw_details: None,
    }
}

fn filesystem_path_permission(tool_name: &str, risk: &str, path: &str) -> PermissionPresentation {
    PermissionPresentation {
        title: tool_name.to_owned(),
        risk: risk.to_owned(),
        details: vec![PermissionDetail::new("path", path.to_owned())],
        raw_details: None,
    }
}

fn write_permission(
    tool_name: &str,
    path: &str,
    bytes: usize,
    lines: usize,
) -> PermissionPresentation {
    PermissionPresentation {
        title: tool_name.to_owned(),
        risk: "write file".to_owned(),
        details: vec![
            PermissionDetail::new("path", path.to_owned()),
            PermissionDetail::new("contents", format!("{bytes} bytes · {lines} lines")),
        ],
        raw_details: None,
    }
}

fn list_permission(
    tool_name: &str,
    path: &str,
    recursive: bool,
    max_entries: Option<u64>,
) -> PermissionPresentation {
    let mut details = vec![
        PermissionDetail::new("path", path.to_owned()),
        PermissionDetail::new("mode", if recursive { "recursive" } else { "direct" }),
    ];
    if let Some(max_entries) = max_entries {
        details.push(PermissionDetail::new(
            "limit",
            format!("{max_entries} entries"),
        ));
    }
    PermissionPresentation {
        title: tool_name.to_owned(),
        risk: "list files".to_owned(),
        details,
        raw_details: None,
    }
}

fn find_permission(
    tool_name: &str,
    path: &str,
    pattern: &str,
    max_results: Option<u64>,
) -> PermissionPresentation {
    let mut details = vec![
        PermissionDetail::new("path", path.to_owned()),
        PermissionDetail::new("pattern", pattern.to_owned()),
    ];
    if let Some(max_results) = max_results {
        details.push(PermissionDetail::new(
            "limit",
            format!("{max_results} results"),
        ));
    }
    PermissionPresentation {
        title: tool_name.to_owned(),
        risk: "find files".to_owned(),
        details,
        raw_details: None,
    }
}

fn grep_permission(
    tool_name: &str,
    path: &str,
    pattern: &str,
    glob: Option<&str>,
    ignore_case: bool,
    max_matches: Option<u64>,
) -> PermissionPresentation {
    let mut details = vec![
        PermissionDetail::new("path", path.to_owned()),
        PermissionDetail::new("pattern", pattern.to_owned()),
    ];
    if let Some(glob) = glob {
        details.push(PermissionDetail::new("glob", glob.to_owned()));
    }
    if ignore_case {
        details.push(PermissionDetail::new("match", "ignore case"));
    }
    if let Some(max_matches) = max_matches {
        details.push(PermissionDetail::new(
            "limit",
            format!("{max_matches} matches"),
        ));
    }
    PermissionPresentation {
        title: tool_name.to_owned(),
        risk: "search files".to_owned(),
        details,
        raw_details: None,
    }
}

fn generic_json_details(arguments_json: &str) -> Vec<PermissionDetail> {
    let Ok(Value::Object(object)) = serde_json::from_str::<Value>(arguments_json) else {
        return vec![PermissionDetail::new(
            "arguments",
            truncate_to_display_width(arguments_json, 240),
        )];
    };

    object
        .iter()
        .take(6)
        .map(|(key, value)| PermissionDetail::new(key, display_json_value(value)))
        .collect()
}

fn display_json_value(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Null | Value::Bool(_) | Value::Number(_) => value.to_string(),
        Value::Array(_) | Value::Object(_) => truncate_to_display_width(&value.to_string(), 240),
    }
}

#[cfg(test)]
mod tests {
    use super::permission_presentation;

    #[test]
    fn shell_permission_uses_semantic_details() {
        let presentation = permission_presentation(
            "shell.run",
            r#"{"command":"cargo check --workspace","cwd":"/repo"}"#,
            None,
        );

        assert_eq!(presentation.risk, "execute process");
        assert_eq!(presentation.details[0].label, "command");
        assert_eq!(presentation.details[0].value, "cargo check --workspace");
    }

    #[test]
    fn generic_json_string_values_are_unescaped() {
        let presentation =
            permission_presentation("custom.tool", r#"{"text":"hello\nworld"}"#, None);

        assert_eq!(presentation.details[0].value, "hello\nworld");
    }
}
