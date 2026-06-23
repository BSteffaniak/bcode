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
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        if is_temporary_boundary_allowlist(path, line) {
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

fn is_temporary_boundary_allowlist(path: &Path, line: &str) -> bool {
    let path = path.to_string_lossy();
    // Transitional allowlist: agent-policy keeps legacy bundled default tool enablement
    // and compatibility fixture names while policy evaluation itself is metadata-driven.
    if path.ends_with("packages/agent-policy/src/lib.rs")
        && (line.contains(".to_string()")
            || line.contains("tool_request(")
            || line.contains("path_request(")
            || line.contains("web.fetch")
            || line.contains("filesystem.write")
            || line.contains("filesystem.edit")
            || line.contains("filesystem.read")
            || line.contains("filesystem.exists")
            || line.contains("filesystem.stat"))
    {
        return true;
    }
    // Intentional allowlist: core config owns the default bundled plugin selection list;
    // concrete plugin implementations still live outside core and remain disableable.
    if path.ends_with("packages/config/src/lib.rs") && line.contains("DEFAULT_") {
        return true;
    }
    // Intentional allowlist: config tests verify default bundled plugin IDs can be disabled.
    if path.ends_with("packages/config/src/lib.rs") && line.contains("bcode.shell") {
        return true;
    }
    // Transitional allowlist: agent-policy model docs describe legacy config keys.
    if path.ends_with("packages/agent-policy/models/src/lib.rs") {
        return true;
    }
    // Transitional allowlist: TUI command IDs still keep backwards-compatible parsing
    // aliases for old worktree command contribution IDs.
    if path.ends_with("packages/tui/src/command_palette.rs") && line.contains("| \"worktree.") {
        return true;
    }
    // Transitional allowlist: server tests construct service/session presentation fixtures.
    if path.ends_with("packages/server/src/lib.rs")
        && (line.contains("filesystem.write") || line.contains("filesystem.read"))
    {
        return true;
    }
    // Transitional allowlist: TUI tests intentionally assert legacy shell visible
    // rendering because shell terminal formatting is tool-specific UX.
    if path.ends_with("packages/tui/src/tests.rs") && line.contains("shell.run") {
        return true;
    }
    // Transitional allowlist: TUI tests for permission/presentation semantics keep
    // legacy visible tool names where those names are the behavior under test.
    if path.ends_with("packages/tui/src/permission_dialog_render.rs")
        || path.ends_with("packages/tui/src/permission_present.rs")
    {
        return true;
    }
    // Transitional allowlist: model-context truncation text teaches agents to use
    // artifact/filesystem tools; this is product guidance, not tool routing.
    if path.ends_with("packages/server/src/lib.rs")
        && line.contains("tool output truncated for model context")
    {
        return true;
    }
    // Transitional allowlist: server tests construct semantic shell/read tool fixtures.
    path.ends_with("packages/server/src/lib.rs")
        && (line.contains("tool_name: \"shell.run\"")
            || line.contains("tool_name: \"filesystem.read\""))
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
