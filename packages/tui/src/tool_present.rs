//! Bcode-specific tool-call presentation models for transcript rendering.

use bcode_session_models::ToolRequestPresentationMetadata;
use serde_json::Value;

/// Human-readable presentation for a tool request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRequestPresentation {
    /// Human-readable title.
    pub title: String,
    /// Labeled detail fields.
    pub fields: Vec<(String, String)>,
}

/// Human-readable presentation for a known tool result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolResultPresentation {
    /// Filesystem read result.
    Read {
        /// Read contents.
        contents: String,
        /// Byte length of the read contents.
        bytes: usize,
        /// Line count of the read contents.
        lines: usize,
    },
    /// Filesystem write result.
    Write {
        /// Human-readable plugin output.
        summary: String,
    },
    /// Filesystem edit result.
    Edit {
        /// Human-readable plugin output.
        summary: String,
    },
    /// Filesystem existence check result.
    Exists {
        /// Whether the path exists.
        exists: bool,
    },
    /// Filesystem list result.
    List {
        /// Directory entries.
        entries: Vec<ListEntryPresentation>,
        /// Whether the result timed out.
        timed_out: bool,
        /// Whether the result is partial.
        partial: bool,
        /// Number of visited entries reported by the tool runtime.
        visited_entries: Option<u64>,
        /// Optional tool runtime message.
        message: Option<String>,
    },
    /// Filesystem find result.
    Find {
        /// Matching paths.
        paths: Vec<String>,
        /// Whether the result timed out.
        timed_out: bool,
        /// Whether the result is partial.
        partial: bool,
        /// Number of visited entries reported by the tool runtime.
        visited_entries: Option<u64>,
        /// Optional tool runtime message.
        message: Option<String>,
    },
    /// Filesystem grep result.
    Grep {
        /// Matching lines.
        matches: Vec<GrepMatchPresentation>,
        /// Whether the result timed out.
        timed_out: bool,
        /// Whether the result is partial.
        partial: bool,
        /// Number of visited entries reported by the tool runtime.
        visited_entries: Option<u64>,
        /// Optional tool runtime message.
        message: Option<String>,
    },
    /// Filesystem stat result.
    Stat {
        /// Whether the path exists.
        exists: bool,
        /// File kind reported by the tool runtime.
        kind: Option<String>,
        /// Optional byte length.
        len: Option<u64>,
    },
    /// Shell command result.
    Shell(ShellResultPresentation),
}

/// Human-readable shell command result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellResultPresentation {
    /// Pseudo-terminal execution result.
    Terminal {
        /// Process exit code.
        exit_code: Option<i32>,
        /// Whether execution timed out.
        timed_out: bool,
        /// Raw terminal byte stream decoded as UTF-8.
        output: String,
        /// Whether the terminal stream was truncated before serialization.
        output_truncated: bool,
        /// Original terminal stream byte count before truncation.
        output_bytes: Option<u64>,
        /// Retained terminal stream byte count after truncation.
        retained_output_bytes: Option<u64>,
        /// Terminal columns used for execution.
        columns: u16,
        /// Terminal rows used for execution.
        rows: u16,
    },
    /// Captured stdout/stderr execution result.
    Capture {
        /// Process exit code.
        exit_code: Option<i32>,
        /// Whether execution timed out.
        timed_out: bool,
        /// Raw ANSI-preserving stdout.
        stdout: String,
        /// Raw ANSI-preserving stderr.
        stderr: String,
    },
}

/// Human-readable directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListEntryPresentation {
    /// Entry path.
    pub path: String,
    /// Entry kind.
    pub kind: String,
}

/// Human-readable grep match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepMatchPresentation {
    /// Match path.
    pub path: String,
    /// One-based line number.
    pub line_number: Option<u64>,
    /// Matching line text.
    pub line: String,
}

/// Build a metadata-driven request presentation from raw tool arguments.
#[must_use]
pub fn tool_request_presentation(
    arguments_json: &str,
    metadata: Option<&ToolRequestPresentationMetadata>,
) -> Option<ToolRequestPresentation> {
    metadata.and_then(|metadata| metadata_request_presentation(arguments_json, metadata))
}

/// Build a known-tool result presentation from raw tool output.
#[must_use]
pub fn tool_result_presentation(
    tool_name: Option<&str>,
    result: &str,
) -> Option<ToolResultPresentation> {
    let normalized = tool_name.map(normalized_tool_name);
    if normalized.as_deref().is_some_and(is_shell_tool_name) || looks_like_shell_result(result) {
        return shell_result(result).map(ToolResultPresentation::Shell);
    }
    match normalized?.as_str() {
        "filesystem_read" | "read" => Some(filesystem_read_result(result)),
        "filesystem_write" | "write" => Some(ToolResultPresentation::Write {
            summary: result.trim().to_owned(),
        }),
        "filesystem_edit" | "edit" => Some(ToolResultPresentation::Edit {
            summary: result.trim().to_owned(),
        }),
        "filesystem_exists" | "exists" => Some(ToolResultPresentation::Exists {
            exists: result.trim() == "true",
        }),
        "filesystem_list" | "list" => filesystem_list_result(result),
        "filesystem_find" | "find" => filesystem_find_result(result),
        "filesystem_grep" | "grep" => filesystem_grep_result(result),
        "filesystem_stat" | "stat" => filesystem_stat_result(result),
        _ => None,
    }
}

fn metadata_request_presentation(
    arguments_json: &str,
    metadata: &ToolRequestPresentationMetadata,
) -> Option<ToolRequestPresentation> {
    let value = serde_json::from_str::<Value>(arguments_json).ok()?;
    let fields = metadata
        .fields
        .iter()
        .filter_map(|field| {
            let argument = value.get(&field.argument)?;
            let rendered = render_metadata_value(argument);
            (!rendered.is_empty()).then(|| (field.label.clone(), rendered))
        })
        .collect::<Vec<_>>();
    Some(ToolRequestPresentation {
        title: metadata.title.clone(),
        fields,
    })
}

fn render_metadata_value(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => value.clone(),
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_string_pretty(value).unwrap_or_default()
        }
    }
}

fn looks_like_shell_result(result: &str) -> bool {
    result.starts_with("exit_code: ")
        || result.contains("\nstdout:\n")
        || serde_json::from_str::<Value>(result)
            .ok()
            .and_then(|value| string_field(&value, "mode"))
            .as_deref()
            == Some("terminal")
}

fn shell_result(result: &str) -> Option<ShellResultPresentation> {
    terminal_shell_result(result).or_else(|| capture_shell_result(result))
}

fn terminal_shell_result(result: &str) -> Option<ShellResultPresentation> {
    let value = serde_json::from_str::<Value>(result).ok()?;
    if string_field(&value, "mode").as_deref() != Some("terminal") {
        return None;
    }
    Some(ShellResultPresentation::Terminal {
        exit_code: i32_field(&value, "exit_code"),
        timed_out: bool_field(&value, "timed_out"),
        output: string_field(&value, "output").unwrap_or_default(),
        output_truncated: bool_field(&value, "output_truncated"),
        output_bytes: u64_field(&value, "output_bytes"),
        retained_output_bytes: u64_field(&value, "retained_output_bytes"),
        columns: u16_field(&value, "columns").unwrap_or(120).max(1),
        rows: u16_field(&value, "rows").unwrap_or(30).max(1),
    })
}

fn capture_shell_result(result: &str) -> Option<ShellResultPresentation> {
    let mut exit_code = None;
    let mut timed_out = false;
    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut section = None;
    for line in result.lines() {
        match line {
            line if line.starts_with("exit_code: ") => {
                let raw = line.trim_start_matches("exit_code: ");
                exit_code = raw.parse::<i32>().ok();
            }
            line if line.starts_with("timed_out: ") => {
                timed_out = line.trim_start_matches("timed_out: ") == "true";
            }
            "stdout:" => section = Some("stdout"),
            "stderr:" => section = Some("stderr"),
            _ => match section {
                Some("stdout") => {
                    stdout.push_str(line);
                    stdout.push('\n');
                }
                Some("stderr") => {
                    stderr.push_str(line);
                    stderr.push('\n');
                }
                _ => {}
            },
        }
    }
    if section.is_some() {
        Some(ShellResultPresentation::Capture {
            exit_code,
            timed_out,
            stdout,
            stderr,
        })
    } else {
        None
    }
}

fn filesystem_read_result(result: &str) -> ToolResultPresentation {
    let contents = serde_json::from_str::<Value>(result)
        .ok()
        .and_then(|value| string_field(&value, "contents"))
        .unwrap_or_else(|| result.to_owned());
    ToolResultPresentation::Read {
        bytes: contents.len(),
        lines: contents.lines().count(),
        contents,
    }
}

fn filesystem_list_result(result: &str) -> Option<ToolResultPresentation> {
    let value = serde_json::from_str::<Value>(result).ok()?;
    let entries = value
        .get("entries")?
        .as_array()?
        .iter()
        .filter_map(|entry| {
            Some(ListEntryPresentation {
                path: string_field(entry, "path")?,
                kind: string_field(entry, "kind").unwrap_or_else(|| "unknown".to_owned()),
            })
        })
        .collect();
    Some(ToolResultPresentation::List {
        entries,
        timed_out: bool_field(&value, "timed_out"),
        partial: bool_field(&value, "partial"),
        visited_entries: u64_field(&value, "visited_entries"),
        message: string_field(&value, "message"),
    })
}

fn filesystem_find_result(result: &str) -> Option<ToolResultPresentation> {
    let value = serde_json::from_str::<Value>(result).ok()?;
    let paths = value
        .get("paths")?
        .as_array()?
        .iter()
        .filter_map(|path| path.as_str().map(ToOwned::to_owned))
        .collect();
    Some(ToolResultPresentation::Find {
        paths,
        timed_out: bool_field(&value, "timed_out"),
        partial: bool_field(&value, "partial"),
        visited_entries: u64_field(&value, "visited_entries"),
        message: string_field(&value, "message"),
    })
}

fn filesystem_grep_result(result: &str) -> Option<ToolResultPresentation> {
    let value = serde_json::from_str::<Value>(result).ok()?;
    let matches = value
        .get("matches")?
        .as_array()?
        .iter()
        .filter_map(|entry| {
            Some(GrepMatchPresentation {
                path: string_field(entry, "path")?,
                line_number: u64_field(entry, "line_number"),
                line: string_field(entry, "line").unwrap_or_default(),
            })
        })
        .collect();
    Some(ToolResultPresentation::Grep {
        matches,
        timed_out: bool_field(&value, "timed_out"),
        partial: bool_field(&value, "partial"),
        visited_entries: u64_field(&value, "visited_entries"),
        message: string_field(&value, "message"),
    })
}

fn filesystem_stat_result(result: &str) -> Option<ToolResultPresentation> {
    let value = serde_json::from_str::<Value>(result).ok()?;
    Some(ToolResultPresentation::Stat {
        exists: bool_field(&value, "exists"),
        kind: string_field(&value, "kind"),
        len: u64_field(&value, "len"),
    })
}

fn normalized_tool_name(tool_name: &str) -> String {
    tool_name.replace(['-', '.'], "_").to_ascii_lowercase()
}

fn is_shell_tool_name(tool_name: &str) -> bool {
    matches!(
        normalized_tool_name(tool_name).as_str(),
        "shell" | "shell_run" | "filesystem_shell_run" | "bash"
    )
}

fn string_field(value: &Value, field: &str) -> Option<String> {
    match value.get(field)? {
        Value::String(text) => Some(text.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

fn bool_field(value: &Value, field: &str) -> bool {
    value.get(field).and_then(Value::as_bool).unwrap_or(false)
}

fn i32_field(value: &Value, field: &str) -> Option<i32> {
    value
        .get(field)
        .and_then(Value::as_i64)
        .and_then(|value| i32::try_from(value).ok())
}

fn u16_field(value: &Value, field: &str) -> Option<u16> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
}

fn u64_field(value: &Value, field: &str) -> Option<u64> {
    value.get(field).and_then(Value::as_u64)
}
