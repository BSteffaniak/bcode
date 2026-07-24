#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bcode_shell_command_analysis::analyze;
use bcode_shell_command_analysis_models::ShellAnalysisRequest;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
struct ShadowCounts {
    databases: usize,
    shell_calls: usize,
    complete: usize,
    incomplete: usize,
    errors: usize,
    quoted_separator_calls: usize,
    quoted_separator_legacy_splits: usize,
    newline_multi_command: usize,
    background_multi_command: usize,
}

fn state_dir() -> PathBuf {
    std::env::var_os("BCODE_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_STATE_HOME").map(|path| PathBuf::from(path).join("bcode"))
        })
        .or_else(|| {
            std::env::var_os("HOME").map(|path| PathBuf::from(path).join(".local/state/bcode"))
        })
        .expect("Bcode state directory environment")
}

fn session_databases(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root.join("sessions")) else {
        return Vec::new();
    };
    let mut paths = entries
        .flatten()
        .map(|entry| entry.path().join("session.db"))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn shell_sources(database: &Path) -> Vec<String> {
    let Ok(connection) = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return Vec::new();
    };
    let Ok(mut statement) = connection.prepare(
        "select payload from events where event_type = 'tool_call_requested' and payload like '%shell.run%'",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = statement.query_map([], |row| row.get::<_, String>(0)) else {
        return Vec::new();
    };
    rows.flatten()
        .filter_map(|payload| {
            let event = serde_json::from_str::<Value>(&payload).ok()?;
            let request = event.get("kind")?.get("tool_call_requested")?;
            if request.get("tool_name")?.as_str()? != "shell.run" {
                return None;
            }
            let arguments = request.get("arguments_json")?.as_str()?;
            serde_json::from_str::<Value>(arguments)
                .ok()?
                .get("command")?
                .as_str()
                .map(str::to_owned)
        })
        .collect()
}

fn legacy_parts(source: &str) -> usize {
    let bytes = source.as_bytes();
    let mut parts = 1_usize;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b';' || bytes[index] == b'|' {
            parts += 1;
            index += usize::from(bytes.get(index + 1) == Some(&bytes[index]));
        } else if bytes[index] == b'&' && bytes.get(index + 1) == Some(&b'&') {
            parts += 1;
            index += 1;
        }
        index += 1;
    }
    parts
}

fn has_quoted_separator(source: &str) -> bool {
    let mut single = false;
    let mut double = false;
    let mut escaped = false;
    for character in source.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && !single {
            escaped = true;
            continue;
        }
        match character {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            ';' | '|' if single || double => return true,
            _ => {}
        }
    }
    false
}

#[test]
#[ignore = "read-only local historical shadow harness; run explicitly"]
fn historical_shell_corpus_shadow_report() {
    let paths = session_databases(&state_dir());
    assert!(!paths.is_empty(), "no historical session databases found");
    let mut counts = ShadowCounts::default();
    let mut unique_sources = BTreeSet::new();
    for database in paths {
        counts.databases += 1;
        unique_sources.extend(shell_sources(&database));
    }
    for source in unique_sources {
        counts.shell_calls += 1;
        let quoted = has_quoted_separator(&source);
        if quoted {
            counts.quoted_separator_calls += 1;
        }
        match analyze(&ShellAnalysisRequest::posix(&source)) {
            Ok(analysis) if analysis.completeness.is_complete() => {
                counts.complete += 1;
                if quoted && legacy_parts(&source) > analysis.commands.len() {
                    counts.quoted_separator_legacy_splits += 1;
                }
                if source.contains('\n') && analysis.commands.len() > 1 {
                    counts.newline_multi_command += 1;
                }
                if source.contains(" & ") && analysis.commands.len() > 1 {
                    counts.background_multi_command += 1;
                }
            }
            Ok(_) => counts.incomplete += 1,
            Err(_) => counts.errors += 1,
        }
    }
    println!("historical shell shadow report: {counts:#?}");
    assert_eq!(
        counts.shell_calls,
        counts.complete + counts.incomplete + counts.errors
    );
}
