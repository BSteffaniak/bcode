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

pub fn capture_history(
    run_id: &str,
    directory: &Path,
    invoke: impl Fn(&[String], &str, Duration) -> Result<Value, EvalError>,
) -> Result<(), EvalError> {
    let mut failures = Vec::new();
    for collection in ["events", "attempts"] {
        if let Err(error) = capture_collection(run_id, directory, &invoke, collection) {
            write_json(
                directory.join(format!("{collection}-capture-error.json")),
                &error.to_string(),
            )?;
            failures.push(collection);
        }
    }
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
