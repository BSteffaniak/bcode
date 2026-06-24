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
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        if is_test_line(path, line_number, &test_ranges)
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

fn is_temporary_boundary_allowlist(path: &Path, line: &str) -> bool {
    let path = path.to_string_lossy();
    // Transitional allowlist: agent-policy model docs describe policy categories, not
    // production tool routing.
    if path.ends_with("packages/agent-policy/models/src/lib.rs") {
        return true;
    }
    // Transitional allowlist: config validation reports removed shorthand tool-ID replacements.
    if path.ends_with("packages/config/src/lib.rs")
        && (line.contains("=> Some(\"") || line.contains("RemovedShorthandToolId"))
    {
        return true;
    }
    // Transitional allowlist: model-context truncation text teaches agents to use
    // artifact/filesystem tools; this is product guidance, not tool routing.
    path.ends_with("packages/server/src/lib.rs")
        && line.contains("tool output truncated for model context")
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
