#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bcode_shell_command_analysis::analyze;
use bcode_shell_command_analysis_models::ShellAnalysisRequest;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use switchy_database::{Database, query::FilterableQuery as _};
use switchy_database_connection::{SqliteAccess, SqliteOpenOptions, open_sqlite_rusqlite};

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

async fn shell_sources(database: &Path) -> Vec<String> {
    let options = SqliteOpenOptions {
        access: SqliteAccess::ReadOnly,
        allow_uri: false,
        busy_timeout: std::time::Duration::from_secs(5),
        foreign_keys: false,
        connections: std::num::NonZeroUsize::new(1).expect("one connection"),
    };
    let Ok(connection) = open_sqlite_rusqlite(database, &options) else {
        return Vec::new();
    };
    let mut sources = Vec::new();
    let mut after = None;
    loop {
        let mut query = connection
            .select("events")
            .columns(&["event_seq", "payload"])
            .where_eq("event_type", "tool_call_requested")
            .sort("event_seq", switchy_database::query::SortDirection::Asc)
            .limit(128);
        if let Some(sequence) = after {
            query = query.where_gt("event_seq", sequence);
        }
        let Ok(rows) = query.execute(&connection).await else {
            break;
        };
        if rows.is_empty() {
            break;
        }
        let Some(sequence) = rows
            .last()
            .and_then(|row| row.get("event_seq"))
            .and_then(|value| i64::try_from(value).ok())
        else {
            break;
        };
        if after.is_some_and(|previous| sequence <= previous) {
            break;
        }
        after = Some(sequence);
        sources.extend(rows.into_iter().filter_map(|row| {
            let value = row.get("payload")?;
            shell_source(value.as_str()?)
        }));
    }
    connection.close().await.expect("release historical reader");
    sources
}

fn shell_source(payload: &str) -> Option<String> {
    let event = serde_json::from_str::<Value>(payload).ok()?;
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
}

#[tokio::test]
async fn structured_history_reader_filters_without_mutating_storage() {
    use switchy_database::schema::{Column, DataType};

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.db");
    let options = SqliteOpenOptions {
        access: SqliteAccess::ReadWriteCreate,
        allow_uri: false,
        busy_timeout: std::time::Duration::from_secs(5),
        foreign_keys: false,
        connections: std::num::NonZeroUsize::new(1).unwrap(),
    };
    let db = open_sqlite_rusqlite(&path, &options).unwrap();
    let mut table = db.create_table("events");
    for (name, data_type, nullable) in [
        ("event_seq", DataType::BigInt, false),
        ("event_type", DataType::Text, false),
        ("payload", DataType::Text, true),
    ] {
        table = table.column(Column {
            name: name.into(),
            data_type,
            nullable,
            auto_increment: false,
            default: None,
        });
    }
    table.execute(&db).await.unwrap();
    let event = |tool: &str, command: &str| {
        serde_json::json!({"kind": {"tool_call_requested": {
            "tool_name": tool,
            "arguments_json": serde_json::json!({"command": command}).to_string()
        }}})
        .to_string()
    };
    for (sequence, (kind, payload)) in [
        (
            "tool_call_requested",
            event("shell.run", "printf '漢字; 🦀'"),
        ),
        ("tool_call_requested", event("not.shell.run", "ignored")),
        ("other_event", event("shell.run", "ignored")),
        ("tool_call_requested", "malformed".to_owned()),
        ("tool_call_requested", "{}".to_owned()),
    ]
    .into_iter()
    .enumerate()
    {
        db.insert("events")
            .value("event_seq", sequence)
            .value("event_type", kind)
            .value("payload", payload)
            .execute(&db)
            .await
            .unwrap();
    }
    db.insert("events")
        .value("event_seq", 5)
        .value("event_type", "tool_call_requested")
        .value("payload", switchy_database::DatabaseValue::Null)
        .execute(&db)
        .await
        .unwrap();
    for sequence in 6..140 {
        db.insert("events")
            .value("event_seq", sequence)
            .value("event_type", "tool_call_requested")
            .value("payload", event("shell.run", "next page"))
            .execute(&db)
            .await
            .unwrap();
    }
    db.close().await.unwrap();
    let before = std::fs::read(&path).unwrap();
    let sources = shell_sources(&path).await;
    assert_eq!(sources.len(), 135);
    assert_eq!(sources[0], "printf '漢字; 🦀'");
    assert!(sources[1..].iter().all(|source| source == "next page"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let read_only = open_sqlite_rusqlite(
        &path,
        &SqliteOpenOptions {
            access: SqliteAccess::ReadOnly,
            ..options
        },
    )
    .unwrap();
    assert!(
        read_only
            .insert("events")
            .value("event_seq", 200)
            .value("event_type", "forbidden")
            .execute(&read_only)
            .await
            .is_err()
    );
    read_only.close().await.unwrap();
}

#[tokio::test]
async fn historical_reader_preserves_missing_and_invalid_files() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("missing.db");
    assert!(shell_sources(&missing).await.is_empty());
    assert!(!missing.exists());
    let invalid = directory.path().join("invalid.db");
    std::fs::write(&invalid, b"not a database").unwrap();
    assert!(shell_sources(&invalid).await.is_empty());
    assert_eq!(std::fs::read(&invalid).unwrap(), b"not a database");
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

#[tokio::test]
#[ignore = "read-only local historical shadow harness; run explicitly"]
async fn historical_shell_corpus_shadow_report() {
    let paths = session_databases(&state_dir());
    assert!(!paths.is_empty(), "no historical session databases found");
    let mut counts = ShadowCounts::default();
    let mut unique_sources = BTreeSet::new();
    for database in paths {
        counts.databases += 1;
        unique_sources.extend(shell_sources(&database).await);
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
