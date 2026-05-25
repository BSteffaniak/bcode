//! Bcode-specific tool-call presentation models for transcript rendering.

use std::path::Path;

use serde_json::Value;

/// Human-readable presentation for a known tool request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolRequestPresentation {
    /// Shell command execution request.
    ShellRun {
        /// Command line that will run.
        command: String,
        /// Optional working directory.
        cwd: Option<String>,
        /// Optional timeout in milliseconds.
        timeout_ms: Option<u64>,
    },
    /// Filesystem read request.
    Read {
        /// Path to read.
        path: String,
    },
    /// Filesystem write request.
    Write {
        /// Path to write.
        path: String,
        /// Byte length of the requested contents.
        bytes: usize,
        /// Line count of the requested contents.
        lines: usize,
    },
    /// Filesystem existence check request.
    Exists {
        /// Path to inspect.
        path: String,
    },
    /// Filesystem list request.
    List {
        /// Directory path to list.
        path: String,
        /// Whether listing is recursive.
        recursive: bool,
        /// Optional maximum entry count.
        max_entries: Option<u64>,
    },
    /// Filesystem find request.
    Find {
        /// Root path to search.
        path: String,
        /// Glob pattern.
        pattern: String,
        /// Optional maximum result count.
        max_results: Option<u64>,
    },
    /// Filesystem grep request.
    Grep {
        /// Root path to search.
        path: String,
        /// Literal search pattern.
        pattern: String,
        /// Optional glob filter.
        glob: Option<String>,
        /// Whether matching ignores case.
        ignore_case: bool,
        /// Optional maximum match count.
        max_matches: Option<u64>,
    },
    /// Filesystem stat request.
    Stat {
        /// Path to inspect.
        path: String,
    },
    /// Web search request.
    WebSearch {
        /// Search query.
        query: String,
        /// Optional provider override.
        provider: Option<String>,
        /// Optional maximum result count.
        max_results: Option<u64>,
    },
    /// Web fetch request.
    WebFetch {
        /// URL to fetch.
        url: String,
        /// Optional maximum byte count.
        max_bytes: Option<u64>,
        /// Whether rendered fetching was requested.
        render: bool,
    },
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

/// Build a known-tool request presentation from raw tool arguments.
#[must_use]
pub fn tool_request_presentation(
    tool_name: &str,
    arguments_json: &str,
) -> Option<ToolRequestPresentation> {
    let value = serde_json::from_str::<Value>(arguments_json).ok()?;
    let normalized = normalized_tool_name(tool_name);
    match normalized.as_str() {
        "shell_run" | "shell" => Some(ToolRequestPresentation::ShellRun {
            command: string_field(&value, "command")?,
            cwd: string_field(&value, "cwd"),
            timeout_ms: u64_field(&value, "timeout_ms"),
        }),
        "filesystem_read" | "read" => Some(ToolRequestPresentation::Read {
            path: path_field(&value, "path")?,
        }),
        "filesystem_write" | "write" => Some(ToolRequestPresentation::Write {
            path: path_field(&value, "path")?,
            bytes: string_field(&value, "contents").map_or(0, |contents| contents.len()),
            lines: string_field(&value, "contents").map_or(0, |contents| contents.lines().count()),
        }),
        "filesystem_exists" | "exists" => Some(ToolRequestPresentation::Exists {
            path: path_field(&value, "path")?,
        }),
        "filesystem_list" | "list" => Some(ToolRequestPresentation::List {
            path: path_field(&value, "path")?,
            recursive: bool_field(&value, "recursive"),
            max_entries: u64_field(&value, "max_entries"),
        }),
        "filesystem_find" | "find" => Some(ToolRequestPresentation::Find {
            path: path_field(&value, "path")?,
            pattern: string_field(&value, "pattern")?,
            max_results: u64_field(&value, "max_results"),
        }),
        "filesystem_grep" | "grep" => Some(ToolRequestPresentation::Grep {
            path: path_field(&value, "path")?,
            pattern: string_field(&value, "pattern")?,
            glob: string_field(&value, "glob"),
            ignore_case: bool_field(&value, "ignore_case"),
            max_matches: u64_field(&value, "max_matches"),
        }),
        "filesystem_stat" | "stat" => Some(ToolRequestPresentation::Stat {
            path: path_field(&value, "path")?,
        }),
        "web_search" => Some(ToolRequestPresentation::WebSearch {
            query: string_field(&value, "query")?,
            provider: string_field(&value, "provider"),
            max_results: u64_field(&value, "max_results"),
        }),
        "web_fetch" => Some(ToolRequestPresentation::WebFetch {
            url: string_field(&value, "url")?,
            max_bytes: u64_field(&value, "max_bytes"),
            render: bool_field(&value, "render"),
        }),
        _ => None,
    }
}

/// Build a known-tool result presentation from raw tool output.
#[must_use]
pub fn tool_result_presentation(
    tool_name: Option<&str>,
    result: &str,
) -> Option<ToolResultPresentation> {
    let normalized = tool_name.map(normalized_tool_name);
    if matches!(normalized.as_deref(), Some("shell_run" | "shell"))
        || looks_like_shell_result(result)
    {
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

fn string_field(value: &Value, field: &str) -> Option<String> {
    match value.get(field)? {
        Value::String(text) => Some(text.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

fn path_field(value: &Value, field: &str) -> Option<String> {
    string_field(value, field).map(|path| path_display(&path))
}

fn path_display(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| path.to_owned(), |_| path.to_owned())
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
