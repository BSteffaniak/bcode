//! Revision-pinned public graph capture. Work per request stays bounded.
use super::{EvalError, Path, write_json};
use serde_json::Value;
use std::time::Duration;

pub fn capture(
    run_id: &str,
    revision: u64,
    directory: &Path,
    invoke: impl Fn(&[String], &str, Duration) -> Result<Value, EvalError>,
) -> Result<(), EvalError> {
    let mut node: Option<String> = None;
    let mut edge: Option<u64> = None;
    let mut page_index = 0_u64;
    loop {
        let mut args = vec![
            "workflow".into(),
            "inspect-run-graph".into(),
            "--run-id".into(),
            run_id.into(),
            "--expected-revision".into(),
            revision.to_string(),
            "--limit".into(),
            "100".into(),
        ];
        if let Some(id) = &node {
            args.extend(["--after-node-id".into(), id.clone()]);
        }
        if let Some(id) = edge {
            args.extend(["--after-edge-id".into(), id.to_string()]);
        }
        let page = invoke(&args, "graph", Duration::from_secs(15))?;
        write_json(directory.join(format!("graph-{page_index:04}.json")), &page)?;
        let graph = page.get("graph").unwrap_or(&page);
        let nodes_complete = graph["nodes_complete"] == true;
        let edges_complete = graph["edges_complete"] == true;
        if nodes_complete && edges_complete {
            return Ok(());
        }
        let next_node = graph["nodes"]
            .as_array()
            .and_then(|nodes| nodes.last())
            .and_then(|node| node.pointer("/node/id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| node.clone());
        let next_edge = graph["edges"]
            .as_array()
            .and_then(|edges| edges.last())
            .and_then(|edge| edge["edge_id"].as_u64())
            .or(edge);
        if next_node == node && next_edge == edge {
            return Err(EvalError::Validation(
                "graph capture cursor did not advance".into(),
            ));
        }
        node = next_node;
        edge = next_edge;
        page_index += 1;
    }
}

pub fn capture_observations(
    run_id: &str,
    directory: &Path,
    invoke: impl Fn(&[String], &str, Duration) -> Result<Value, EvalError>,
) -> Result<(), EvalError> {
    let mut after: Option<u64> = None;
    let mut through: Option<u64> = None;
    let mut index = 0_u64;
    loop {
        let mut args = vec![
            "workflow".into(),
            "observations".into(),
            "--run-id".into(),
            run_id.into(),
            "--limit".into(),
            "100".into(),
        ];
        if let Some(value) = after {
            args.extend(["--after-sequence".into(), value.to_string()]);
        }
        if let Some(value) = through {
            args.extend(["--through-sequence".into(), value.to_string()]);
        }
        let value = invoke(&args, "observations", Duration::from_secs(15))?;
        let page: bcode_workflow::WorkflowObservationPage =
            serde_json::from_value(value.clone())
                .map_err(|_| EvalError::Validation("invalid observation page".into()))?;
        if page.version != 1
            || page.run_id != run_id
            || page.observations.len() > 100
            || through.is_some_and(|value| value != page.through_sequence)
        {
            return Err(EvalError::Validation(
                "observation page identity mismatch".into(),
            ));
        }
        write_json(
            directory.join(format!("workflow-observations-{index:04}.json")),
            &value,
        )?;
        if page.origin != bcode_workflow::WorkflowObservationOrigin::Admission {
            return Err(EvalError::Validation(
                "observation coverage was not enabled at admission".into(),
            ));
        }
        let mut previous = after.unwrap_or(0);
        for row in &page.observations {
            if row.sequence <= previous || row.sequence > page.through_sequence {
                return Err(EvalError::Validation(
                    "observation sequence did not advance".into(),
                ));
            }
            previous = row.sequence;
        }
        let Some(next) = page.next_after_sequence else {
            return Ok(());
        };
        if next <= after.unwrap_or(0) || next != previous {
            return Err(EvalError::Validation("observation cursor mismatch".into()));
        }
        after = Some(next);
        through = Some(page.through_sequence);
        index += 1;
    }
}

pub fn capture_history(
    run_id: &str,
    directory: &Path,
    invoke: impl Fn(&[String], &str, Duration) -> Result<Value, EvalError>,
) -> Result<(), EvalError> {
    let mut failures = Vec::new();
    let mut boundaries = serde_json::Map::new();
    for collection in ["events", "attempts"] {
        if let Err(error) = capture_collection(run_id, directory, &invoke, collection) {
            write_json(
                directory.join(format!("{collection}-capture-error.json")),
                &error.to_string(),
            )?;
            failures.push(collection);
            boundaries.insert(
                collection.into(),
                serde_json::json!({"status":"partial_or_unavailable"}),
            );
        } else {
            boundaries.insert(collection.into(), serde_json::json!({"status":"complete"}));
        }
    }
    boundaries.insert(
        "judgement".into(),
        serde_json::json!({"status":"not_captured","scope":"request and answer correlation"}),
    );
    boundaries.insert("provider_wire".into(), serde_json::json!({"status":"not_captured","scope":"wire requests, retries and queue attribution"}));
    write_json(
        directory.join("capture-completeness.json"),
        &serde_json::json!({
            "schema":"bcode.eval.capture_completeness.v1",
            "scope":"workflow public history collections; model exports have independent receipts",
            "boundaries":boundaries,
            "complete":false
        }),
    )?;
    if failures.is_empty() {
        Ok(())
    } else {
        Err(EvalError::Validation(format!(
            "incomplete history collections: {}",
            failures.join(", ")
        )))
    }
}

fn capture_collection(
    run_id: &str,
    directory: &Path,
    invoke: &impl Fn(&[String], &str, Duration) -> Result<Value, EvalError>,
    collection: &str,
) -> Result<(), EvalError> {
    let mut cursor: Vec<String> = Vec::new();
    let mut index = 0_u64;
    loop {
        let mut args = vec![
            "workflow".into(),
            collection.into(),
            "--run-id".into(),
            run_id.into(),
            "--limit".into(),
            "100".into(),
        ];
        args.extend(cursor.clone());
        let page = invoke(&args, collection, Duration::from_secs(15))?;
        write_json(
            directory.join(format!("workflow-{collection}-{index:04}.json")),
            &page,
        )?;
        let rows = page
            .as_array()
            .ok_or_else(|| EvalError::Validation("history response is not an array".into()))?;
        if rows.len() < 100 {
            break;
        }
        let last = rows.last().expect("nonempty full page");
        let next = if collection == "events" {
            vec![
                "--after-sequence".into(),
                last["event_seq"]
                    .as_u64()
                    .ok_or_else(|| EvalError::Validation("event cursor missing".into()))?
                    .to_string(),
            ]
        } else {
            vec![
                "--after-prepared-at-ms".into(),
                last["prepared_at_ms"]
                    .as_u64()
                    .ok_or_else(|| EvalError::Validation("attempt timestamp missing".into()))?
                    .to_string(),
                "--after-dispatch-identity".into(),
                last["dispatch_identity"]
                    .as_str()
                    .ok_or_else(|| EvalError::Validation("attempt identity missing".into()))?
                    .into(),
            ]
        };
        if next == cursor {
            return Err(EvalError::Validation(
                "history cursor did not advance".into(),
            ));
        }
        cursor = next;
        index += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::capture;
    use std::cell::Cell;

    #[test]
    fn observation_capture_pins_high_water_and_preserves_disabled_coverage() {
        let directory =
            std::env::temp_dir().join(format!("bcode-observation-capture-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let calls = Cell::new(0);
        super::capture_observations("run", &directory, |args, _, _| {
            let index = calls.get(); calls.set(index + 1);
            let sequence = if index == 0 { 3 } else { 8 };
            if index == 1 {
                assert!(args.windows(2).any(|pair| pair == ["--through-sequence", "8"]));
                assert!(args.windows(2).any(|pair| pair == ["--after-sequence", "3"]));
            }
            Ok(serde_json::json!({"version":1,"run_id":"run","origin":"admission","sources":[],"limitations":["finite"],"through_sequence":8,"next_after_sequence":if index == 0 {Some(3)} else {None},"observations":[{"sequence":sequence,"source":"runs","operation":"update","facts":{},"recorded_at_ms":null,"timestamp_source":null}]}))
        }).unwrap();
        assert_eq!(calls.get(), 2);
        let result = super::capture_observations("run", &directory, |_, _, _| {
            Ok(
                serde_json::json!({"version":1,"run_id":"run","origin":"not_enabled","sources":[],"limitations":[],"through_sequence":0,"next_after_sequence":null,"observations":[]}),
            )
        });
        assert!(result.is_err());
        let retained: serde_json::Value = serde_json::from_slice(
            &std::fs::read(directory.join("workflow-observations-0000.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(retained["origin"], "not_enabled");
        for index in 0..2 {
            std::fs::remove_file(directory.join(format!("workflow-observations-{index:04}.json")))
                .unwrap();
        }
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn unavailable_events_do_not_discard_available_attempts() {
        let directory =
            std::env::temp_dir().join(format!("bcode-partial-capture-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let result = super::capture_history("run", &directory, |_, kind, _| {
            if kind == "events" {
                Err(super::EvalError::Validation("unsupported".into()))
            } else {
                Ok(serde_json::json!([]))
            }
        });
        assert!(result.is_err());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                &std::fs::read(directory.join("workflow-attempts-0000.json")).unwrap()
            )
            .unwrap(),
            serde_json::json!([])
        );
        std::fs::remove_file(directory.join("workflow-attempts-0000.json")).unwrap();
        std::fs::remove_file(directory.join("events-capture-error.json")).unwrap();
        std::fs::remove_file(directory.join("capture-completeness.json")).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn history_pages_use_event_and_attempt_cursors() {
        let directory =
            std::env::temp_dir().join(format!("bcode-history-capture-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        super::capture_history("run",&directory,|args,kind,_| {
            if args.len()==6 {
                let rows:Vec<_>=(0..100).map(|index|serde_json::json!({"event_seq":index,"prepared_at_ms":index,"dispatch_identity":format!("dispatch-{index}")})).collect();
                Ok(serde_json::json!(rows))
            } else {
                let cursor=if kind=="events" {"--after-sequence"} else {"--after-prepared-at-ms"};
                assert!(args.windows(2).any(|pair|pair==[cursor,"99"]));
                Ok(serde_json::json!([]))
            }
        }).unwrap();
        for kind in ["events", "attempts"] {
            for index in 0..2 {
                std::fs::remove_file(directory.join(format!("workflow-{kind}-{index:04}.json")))
                    .unwrap();
            }
        }
        std::fs::remove_file(directory.join("capture-completeness.json")).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn partial_history_failure_retains_pages_and_boundary_status() {
        let directory =
            std::env::temp_dir().join(format!("bcode-history-error-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let result = super::capture_history("run", &directory, |args, kind, _| {
            if kind == "attempts" {
                return Ok(serde_json::json!([]));
            }
            if args.len() > 6 {
                return Err(super::EvalError::Client("unavailable".into()));
            }
            Ok(serde_json::json!(
                (0..100)
                    .map(|index| serde_json::json!({"event_seq":index}))
                    .collect::<Vec<_>>()
            ))
        });
        assert!(result.is_err());
        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(directory.join("capture-completeness.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            manifest["boundaries"]["events"]["status"],
            "partial_or_unavailable"
        );
        assert_eq!(manifest["boundaries"]["attempts"]["status"], "complete");
        assert_eq!(
            manifest["boundaries"]["judgement"]["status"],
            "not_captured"
        );
        assert!(directory.join("workflow-events-0000.json").exists());
        for name in [
            "workflow-events-0000.json",
            "workflow-attempts-0000.json",
            "events-capture-error.json",
            "capture-completeness.json",
        ] {
            std::fs::remove_file(directory.join(name)).unwrap();
        }
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn graph_no_progress_preserves_partial_page() {
        let directory =
            std::env::temp_dir().join(format!("bcode-stalled-graph-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let calls = Cell::new(0);
        let result = capture("run", 7, &directory, |_, _, _| {
            calls.set(calls.get() + 1);
            Ok(
                serde_json::json!({"graph":{"nodes":[],"edges":[],"nodes_complete":false,"edges_complete":false}}),
            )
        });
        assert!(result.is_err());
        assert_eq!(calls.get(), 1);
        assert!(directory.join("graph-0000.json").exists());
        std::fs::remove_file(directory.join("graph-0000.json")).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn captures_all_pages_with_revision_and_progressing_cursors() {
        let directory =
            std::env::temp_dir().join(format!("bcode-graph-capture-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let count = Cell::new(0);
        capture("run",7,&directory,|args,_,_| {
            assert!(args.windows(2).any(|pair|pair==["--expected-revision","7"]));
            let index=count.get(); count.set(index+1);
            if index==0 {Ok(serde_json::json!({"graph":{"nodes":[{"node":{"id":"first"}}],"edges":[{"edge_id":4}],"nodes_complete":false,"edges_complete":false}}))}
            else {
                assert!(args.windows(2).any(|pair|pair==["--after-node-id","first"]));
                assert!(args.windows(2).any(|pair|pair==["--after-edge-id","4"]));
                Ok(serde_json::json!({"graph":{"nodes":[],"edges":[],"nodes_complete":true,"edges_complete":true}}))
            }
        }).unwrap();
        assert_eq!(count.get(), 2);
        for index in 0..2 {
            std::fs::remove_file(directory.join(format!("graph-{index:04}.json"))).unwrap();
        }
        std::fs::remove_dir(directory).unwrap();
    }
}
