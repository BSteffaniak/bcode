#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

const CORE_SCAN_ROOTS: &[&str] = &[
    "packages/server",
    "packages/config",
    "packages/agent-policy",
    "packages/tui",
    "packages/cli",
];

const DENIED_TOOL_NAME_NEEDLES: &[&str] = &[
    "shell.run",
    "filesystem.read",
    "filesystem.write",
    "filesystem.edit",
    "filesystem.list",
    "filesystem.find",
    "filesystem.grep",
    "filesystem.stat",
    "filesystem.exists",
    "web.search",
    "web.fetch",
    "web.status",
    "web.inspect",
    "git.clone",
    "github.clone",
    "worktree.list",
    "worktree.create",
    "worktree.remove",
    "document.extract",
];

const DENIED_NORMALIZED_TOOL_NAME_NEEDLES: &[&str] = &[
    "shell_run",
    "filesystem_read",
    "filesystem_write",
    "filesystem_edit",
    "filesystem_list",
    "filesystem_find",
    "filesystem_grep",
    "filesystem_stat",
    "filesystem_exists",
    "web_search",
    "web_fetch",
    "web_status",
    "web_inspect",
    "git_clone",
    "github_clone",
    "worktree_list",
    "worktree_create",
    "worktree_remove",
    "document_extract",
];

const DENIED_PLUGIN_ID_NEEDLES: &[&str] = &[
    "bcode.filesystem",
    "bcode.shell",
    "bcode.web-search",
    "bcode.git",
    "bcode.worktree",
    "bcode.document",
];

const DENIED_PLUGIN_CRATE_NEEDLES: &[&str] = &[
    "bcode_filesystem_plugin",
    "bcode_shell_plugin",
    "bcode_web_search_plugin",
    "bcode_git_plugin",
    "bcode_worktree_plugin",
    "bcode_document_plugin",
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct BoundaryOffender {
    path: PathBuf,
    line_number: usize,
    needle: &'static str,
    category: &'static str,
    line: String,
}

/// Enforces that remaining hardcoded bundled tool/plugin references in core crates are
/// either removed or covered by an explicit temporary allowlist with a migration comment.
#[test]
fn core_crates_report_bundled_tool_plugin_references() {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let offenders = collect_boundary_offenders(&workspace_root);
    if offenders.is_empty() {
        return;
    }

    let report = format_offender_report(&workspace_root, &offenders);
    eprintln!("{report}");

    assert!(
        std::env::var_os("BCODE_ENFORCE_TOOL_PLUGIN_BOUNDARY").is_none(),
        "{report}"
    );
}

fn collect_boundary_offenders(workspace_root: &Path) -> Vec<BoundaryOffender> {
    let mut offenders = Vec::new();
    for root in CORE_SCAN_ROOTS {
        scan_path(&workspace_root.join(root), &mut offenders);
    }
    offenders.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.line_number.cmp(&right.line_number))
            .then(left.needle.cmp(right.needle))
    });
    offenders
}

fn scan_path(path: &Path, offenders: &mut Vec<BoundaryOffender>) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        scan_dir(path, offenders);
    } else if should_scan_file(path) {
        scan_file(path, offenders);
    }
}

fn scan_dir(dir: &Path, offenders: &mut Vec<BoundaryOffender>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() && should_skip_dir(&name) {
            continue;
        }
        scan_path(&path, offenders);
    }
}

fn should_skip_dir(name: &str) -> bool {
    name == "target" || name.starts_with('.')
}

fn should_scan_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if name == "architecture_guardrails.rs" {
        return false;
    }
    name == "Cargo.toml"
        || path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
}

fn scan_file(path: &Path, offenders: &mut Vec<BoundaryOffender>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let test_ranges = test_module_ranges(&text);
    let function_ranges = allowlisted_function_ranges(path, &text);
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        if is_comment_line(line)
            || is_test_line(path, line_number, &test_ranges)
            || is_allowlisted_function_line(line_number, &function_ranges)
            || is_temporary_boundary_allowlist(path, line)
        {
            continue;
        }
        scan_line(
            path,
            line_number,
            line,
            DENIED_TOOL_NAME_NEEDLES,
            "tool name",
            offenders,
        );
        scan_line(
            path,
            line_number,
            line,
            DENIED_NORMALIZED_TOOL_NAME_NEEDLES,
            "normalized tool name",
            offenders,
        );
        scan_line(
            path,
            line_number,
            line,
            DENIED_PLUGIN_ID_NEEDLES,
            "plugin id",
            offenders,
        );
        scan_line(
            path,
            line_number,
            line,
            DENIED_PLUGIN_CRATE_NEEDLES,
            "plugin crate",
            offenders,
        );
    }
}

fn is_test_line(path: &Path, line_number: usize, test_ranges: &[(usize, usize)]) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "tests.rs" || name.ends_with("_test.rs"))
        || test_ranges
            .iter()
            .any(|(start, end)| line_number >= *start && line_number <= *end)
}

fn test_module_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let lines = text.lines().collect::<Vec<_>>();
    let mut index = 0usize;
    while index < lines.len() {
        if lines[index].trim() == "#[cfg(test)]" {
            let mut module_index = index + 1;
            while module_index < lines.len() && lines[module_index].trim().is_empty() {
                module_index = module_index.saturating_add(1);
            }
            if module_index < lines.len() && lines[module_index].contains("mod tests") {
                let end = module_end_line(&lines, module_index).unwrap_or(lines.len());
                ranges.push((index + 1, end));
                index = end;
                continue;
            }
        }
        index = index.saturating_add(1);
    }
    ranges
}

fn module_end_line(lines: &[&str], module_index: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut opened = false;
    for (index, line) in lines.iter().enumerate().skip(module_index) {
        for character in line.chars() {
            match character {
                '{' => {
                    depth = depth.saturating_add(1);
                    opened = true;
                }
                '}' if opened => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some(index + 1);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

fn is_comment_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("//")
}

fn is_allowlisted_function_line(line_number: usize, function_ranges: &[(usize, usize)]) -> bool {
    function_ranges
        .iter()
        .any(|(start, end)| line_number >= *start && line_number <= *end)
}

fn allowlisted_function_ranges(path: &Path, text: &str) -> Vec<(usize, usize)> {
    if !path.ends_with("packages/config/src/lib.rs") {
        return Vec::new();
    }
    function_ranges(text, &["removed_shorthand_tool_replacement"])
}

fn function_ranges(text: &str, names: &[&str]) -> Vec<(usize, usize)> {
    let lines = text.lines().collect::<Vec<_>>();
    let mut ranges = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if names
            .iter()
            .any(|name| trimmed.starts_with(&format!("fn {name}")))
        {
            let end = module_end_line(&lines, index).unwrap_or(index + 1);
            ranges.push((index + 1, end));
        }
    }
    ranges
}

fn is_temporary_boundary_allowlist(path: &Path, line: &str) -> bool {
    let path = path.to_string_lossy();
    // Transitional allowlist: model-context truncation text teaches agents to use
    // artifact/filesystem tools; this is product guidance, not tool routing.
    if path.ends_with("packages/server/src/lib.rs")
        && line.contains("tool output truncated for model context")
    {
        return true;
    }
    // Server keeps product command error codes and a provider-native host action bridge.
    if path.ends_with("packages/server/src/lib.rs") {
        return line.contains("worktree_list_command_failed")
            || line.contains("worktree_create_command_failed")
            || line.contains("worktree_remove_command_failed")
            || line.contains("invoke_host_provider_native_search")
            || line.contains("web_search");
    }
    // CLI worktree commands are product commands, not model-callable tool IDs.
    if path.ends_with("packages/cli/src/lib.rs") {
        return line.contains("worktree_list_command")
            || line.contains("worktree_create_command")
            || line.contains("worktree_remove_command");
    }
    // Worktree TUI modules own a command/dialog flow separate from model-callable tools.
    if path.contains("packages/tui/src/wt_create_dialog")
        || path.ends_with("packages/tui/src/worktree_flow.rs")
    {
        return line.contains("wt_create_dialog");
    }
    // Legacy config schema exposes provider-side web search settings; this is not tool routing.
    if path.ends_with("packages/config/src/lib.rs") && line.contains("web_search") {
        return true;
    }
    false
}

fn scan_line(
    path: &Path,
    line_number: usize,
    line: &str,
    needles: &'static [&'static str],
    category: &'static str,
    offenders: &mut Vec<BoundaryOffender>,
) {
    for needle in needles {
        if line.contains(needle) {
            offenders.push(BoundaryOffender {
                path: path.to_path_buf(),
                line_number,
                needle,
                category,
                line: line.trim().to_owned(),
            });
        }
    }
}

fn format_offender_report(workspace_root: &Path, offenders: &[BoundaryOffender]) -> String {
    let mut report = String::from(
        "tool/plugin boundary offenders found in core crates; this is report-only until \
         BCODE_ENFORCE_TOOL_PLUGIN_BOUNDARY is set:\n",
    );
    for offender in offenders {
        let path = offender
            .path
            .strip_prefix(workspace_root)
            .unwrap_or(&offender.path);
        let _ = writeln!(
            report,
            "{}:{} contains {} {:?}: {}",
            path.display(),
            offender.line_number,
            offender.category,
            offender.needle,
            offender.line,
        );
    }
    report
}
