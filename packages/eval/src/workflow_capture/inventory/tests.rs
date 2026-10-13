use super::*;
use bcode_workflow::{
    WorkflowInventoryCursor, WorkflowInventoryKey, WorkflowInventoryUnavailableReason,
};
use std::cell::Cell;

fn query(args: &[String]) -> WorkflowInventoryQuery {
    let get = |key: &str| {
        args.iter()
            .position(|arg| arg == key)
            .map(|i| args[i + 1].clone())
    };
    WorkflowInventoryQuery {
        version: WORKFLOW_INVENTORY_VERSION,
        run_id: get("--run-id").unwrap(),
        source: match get("--source").unwrap().as_str() {
            "outputs" => WorkflowInventorySource::Outputs,
            "attempt_contributors" => WorkflowInventorySource::AttemptContributors,
            _ => WorkflowInventorySource::ChildRuns,
        },
        cursor: get("--cursor-json").map(|value| serde_json::from_str(&value).unwrap()),
        limit: get("--limit").unwrap().parse().unwrap(),
    }
}
fn pin(query: &WorkflowInventoryQuery) -> WorkflowInventoryCursor {
    query
        .cursor
        .clone()
        .unwrap_or_else(|| WorkflowInventoryCursor {
            version: WORKFLOW_INVENTORY_VERSION,
            run_id: query.run_id.clone(),
            source: query.source,
            installation_id: "installed".into(),
            graph_revision: 1,
            source_generation: 7,
            after: None,
        })
}
fn empty(query: &WorkflowInventoryQuery) -> WorkflowInventoryPage {
    WorkflowInventoryPage {
        cursor: pin(query),
        coverage: WorkflowInventoryCoverage::CurrentInventoryComplete,
        entries: vec![],
        next_cursor: None,
    }
}
fn output(run: &str, index: usize) -> WorkflowInventoryEntry {
    let value = serde_json::json!({"index":index});
    WorkflowInventoryEntry::Output {
        output: bcode_workflow::WorkflowOutputInspection {
            version: 1,
            output_id: format!("output-{index:04}"),
            run_id: run.into(),
            node_id: "node".into(),
            activation_id: "activation".into(),
            schema_id: "value".into(),
            schema_version: 1,
            checksum_sha256: bcode_workflow::workflow_canonical_value_sha256(&value).unwrap(),
            value,
            artifact_reference: None,
            created_at_ms: 1,
        },
    }
}
fn terminal(args: &[String]) -> serde_json::Value {
    let position = args.iter().position(|arg| arg == "--run-id").unwrap();
    serde_json::json!({"run":{"run_id":args[position+1],"status":"completed"}})
}
fn directory() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "bcode-inventory-{}",
        bcode_session_models::SessionId::new()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn drains_over_one_hundred_rows_and_rechecks_pins() {
    let directory = directory();
    let rechecks = Cell::new(0);
    let captured = capture(
        "root",
        &directory,
        Instant::now() + Duration::from_secs(5),
        |args, label, _| {
            if args[1] == "inspect-run" {
                return Ok(terminal(args));
            }
            let query = query(args);
            let mut page = empty(&query);
            if label.starts_with("inventory-recheck") {
                rechecks.set(rechecks.get() + 1);
            }
            if query.source == WorkflowInventorySource::Outputs {
                let start = match &page.cursor.after {
                    Some(WorkflowInventoryKey::Output(id)) => {
                        id.trim_start_matches("output-").parse::<usize>().unwrap() + 1
                    }
                    _ => 0,
                };
                let end = (start + query.limit).min(205);
                page.entries = (start..end).map(|i| output("root", i)).collect();
                if end < 205 {
                    page.next_cursor = Some(WorkflowInventoryCursor {
                        after: page.entries.last().map(WorkflowInventoryEntry::key),
                        ..page.cursor.clone()
                    });
                }
            }
            Ok(serde_json::to_value(page).unwrap())
        },
    )
    .unwrap();
    assert_eq!(captured.outputs.len(), 205);
    assert_eq!(rechecks.get(), 3);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn descendants_are_drained_and_running_descendants_are_not_closed() {
    for running in [false, true] {
        let directory = directory();
        let result = capture(
            "root",
            &directory,
            Instant::now() + Duration::from_secs(5),
            |args, _, _| {
                if args[1] == "inspect-run" {
                    let mut value = terminal(args);
                    if running && value["run"]["run_id"] == "child" {
                        value["run"]["status"] = "running".into();
                    }
                    return Ok(value);
                }
                let query = query(args);
                let mut page = empty(&query);
                if query.run_id == "root" && query.source == WorkflowInventorySource::ChildRuns {
                    page.entries.push(WorkflowInventoryEntry::ChildRun {
                        child_run_id: "child".into(),
                        root_run_id: "root".into(),
                        parent: bcode_workflow::WorkflowHistoryAttemptCorrelation {
                            run_id: "root".into(),
                            node_id: "node".into(),
                            activation_id: "activation".into(),
                            attempt: 1,
                        },
                    });
                }
                Ok(serde_json::to_value(page).unwrap())
            },
        );
        if running {
            assert!(result.is_err());
        } else {
            assert_eq!(result.unwrap().runs.len(), 2);
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn terminal_attempts_require_receipts_and_verified_session_links() {
    for defect in ["none", "receipt", "session", "running"] {
        let directory = directory();
        let result = capture(
            "root",
            &directory,
            Instant::now() + Duration::from_secs(5),
            |args, _, _| {
                if args[1] == "inspect-run" {
                    return Ok(terminal(args));
                }
                let query = query(args);
                let mut page = empty(&query);
                if query.source == WorkflowInventorySource::AttemptContributors {
                    page.entries
                        .push(WorkflowInventoryEntry::AttemptContributor {
                        attempt: bcode_workflow::AttemptSummary {
                            run_id: "root".into(),
                            node_id: "node".into(),
                            activation_id: "activation".into(),
                            attempt: 1,
                            dispatch_identity: "a".repeat(64),
                            side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
                            status: if defect == "running" {
                                "admitted"
                            } else {
                                "succeeded"
                            }
                            .into(),
                            has_receipt: defect != "receipt",
                            prepared_at_ms: 1,
                            admitted_at_ms: Some(2),
                            terminal_at_ms: if defect == "running" { None } else { Some(3) },
                        },
                        session: if defect == "session" {
                            WorkflowInventorySessionLink::Unavailable {
                                reason: WorkflowInventoryUnavailableReason::Unsupported,
                            }
                        } else {
                            WorkflowInventorySessionLink::Verified {
                                session_id: "session".into(),
                                workspace_snapshot: "workspace".into(),
                                created_at_ms: 2,
                            }
                        },
                        diagnostic_applicability:
                            bcode_workflow::WorkflowInventoryDiagnosticApplicability::Unsupported,
                    });
                }
                Ok(serde_json::to_value(page).unwrap())
            },
        );
        if defect == "none" {
            let capture = result.unwrap();
            assert!(capture.sessions.contains("session"));
            assert_eq!(capture.attempts.len(), 1);
        } else {
            assert!(result.is_err(), "{defect}");
        }
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn unavailable_future_stale_and_wrong_checksum_fail_closed() {
    for defect in [
        "unavailable",
        "future",
        "stale",
        "checksum",
        "missing",
        "stalled",
        "cycle",
        "revision",
    ] {
        let directory = directory();
        let result = capture(
            "root",
            &directory,
            Instant::now() + Duration::from_secs(5),
            |args, label, _| {
                if args[1] == "inspect-run" {
                    return Ok(terminal(args));
                }
                let query = query(args);
                let mut page = empty(&query);
                match defect {
                    "unavailable" => {
                        page.coverage = WorkflowInventoryCoverage::Unavailable {
                            reason: WorkflowInventoryUnavailableReason::Unsupported,
                        }
                    }
                    "future" => page.cursor.version += 1,
                    "stale" if label.starts_with("inventory-recheck") => {
                        page.cursor.source_generation += 1;
                    }
                    "checksum" if query.source == WorkflowInventorySource::Outputs => {
                        let mut entry = output("root", 0);
                        if let WorkflowInventoryEntry::Output { output } = &mut entry {
                            output.checksum_sha256 = "0".repeat(64);
                        }
                        page.entries.push(entry);
                    }
                    "stalled" => page.next_cursor = Some(page.cursor.clone()),
                    "revision" if query.source == WorkflowInventorySource::ChildRuns => {
                        page.cursor.graph_revision += 1;
                    }
                    "cycle" if query.source == WorkflowInventorySource::ChildRuns => {
                        page.entries.push(WorkflowInventoryEntry::ChildRun {
                            child_run_id: "root".into(),
                            root_run_id: "root".into(),
                            parent: bcode_workflow::WorkflowHistoryAttemptCorrelation {
                                run_id: "root".into(),
                                node_id: "node".into(),
                                activation_id: "activation".into(),
                                attempt: 1,
                            },
                        });
                    }
                    "missing" if query.source == WorkflowInventorySource::Outputs => page
                        .entries
                        .push(WorkflowInventoryEntry::OutputUnavailable {
                            output_id: "output".into(),
                            reason: WorkflowInventoryUnavailableReason::Unsupported,
                        }),
                    _ => {}
                }
                Ok(serde_json::to_value(page).unwrap())
            },
        );
        assert!(result.is_err(), "{defect}");
        std::fs::remove_dir_all(directory).unwrap();
    }
}
