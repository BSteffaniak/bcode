use super::*;
use serde_json::json;

fn page(scope: Scope, after: u64, end: u64, through: u64) -> Value {
    let cursor = json!({"version":1,"run_id":"run","scope":scope.name(),
        "coverage":{"kind":"complete_from_admission"},"through_sequence":through,"after_sequence":after});
    let events = ((after + 1)..=end).map(|sequence| {
        let mut event = json!({"run_id":"run","sequence":sequence,
            "operation_id":format!("{sequence:064x}"),"recorded_at_ms":1,
            "payload":{"kind":"run_admitted","status":"paused"}});
        if matches!(scope, Scope::Attempts) {
            event["attempt"] = json!({"run_id":"run","node_id":"worker","activation_id":"activation","attempt":1});
            event["dispatch_identity"] = json!("a".repeat(64));
            event["payload"] = json!({"kind":"receipt_committed"});
        }
        event
    }).collect::<Vec<_>>();
    let next = if end < through {
        let mut next = cursor.clone();
        next["after_sequence"] = json!(end);
        next
    } else {
        Value::Null
    };
    json!({"cursor":cursor,"events":events,"next_cursor":next})
}

#[test]
fn arbitrary_paging_exhausts_both_scopes_beyond_one_hundred_rows() {
    for scope in [Scope::Attempts, Scope::Lifecycle] {
        let directory = tempfile::tempdir().unwrap();
        let calls = std::cell::Cell::new(0);
        let (boundary, result) = capture(
            scope,
            "run",
            directory.path(),
            Instant::now() + Duration::from_secs(5),
            &|args, kind, allowance| {
                assert_eq!(kind, scope.name());
                assert!(allowance <= Duration::from_secs(5));
                assert_eq!(args[1], scope.command());
                let after = args
                    .iter()
                    .position(|arg| arg == "--cursor-json")
                    .map_or(0, |i| {
                        serde_json::from_str::<Value>(&args[i + 1]).unwrap()["after_sequence"]
                            .as_u64()
                            .unwrap()
                    });
                calls.set(calls.get() + 1);
                Ok(page(scope, after, (after + 37).min(213), 213))
            },
        );
        result.unwrap();
        assert_eq!(calls.get(), 6);
        assert_eq!(boundary["validated_rows"], 213);
        assert_eq!(boundary["status"], "complete");
        assert_eq!(boundary["full_history_complete"], false);
    }
}

#[test]
fn invalid_future_mismatched_gap_and_stalled_pages_are_retained() {
    for scope in [Scope::Attempts, Scope::Lifecycle] {
        for defect in [
            "gap",
            "future",
            "run",
            "highwater",
            "stall",
            "malformed",
            "order",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let calls = std::cell::Cell::new(0);
            let (boundary, result) = capture(
                scope,
                "run",
                directory.path(),
                Instant::now() + Duration::from_secs(5),
                &|_, _, _| {
                    let index = calls.get();
                    calls.set(index + 1);
                    if index == 0 {
                        return Ok(page(scope, 0, 1, 3));
                    }
                    let mut value = page(scope, 1, 3, 3);
                    match defect {
                        "gap" => value["cursor"]["coverage"] = json!({"kind":"historical_gap"}),
                        "future" => value["cursor"]["version"] = json!(99),
                        "run" => value["cursor"]["run_id"] = json!("other"),
                        "highwater" => value["cursor"]["through_sequence"] = json!(4),
                        "stall" => value = page(scope, 1, 1, 3),
                        "malformed" => value["events"][0]["payload"] = json!({"kind":"future"}),
                        "order" => value["events"][0]["sequence"] = json!(1),
                        _ => unreachable!(),
                    }
                    Ok(value)
                },
            );
            assert!(result.is_err(), "{defect}");
            assert_eq!(boundary["status"], "partial_or_unavailable");
            assert_eq!(boundary["pages_retained"], 2);
            assert_eq!(boundary["validated_rows"], 1);
            assert!(
                directory
                    .path()
                    .join(format!("{}-0001.json", scope.name()))
                    .is_file()
            );
        }
    }
}

#[test]
fn initial_historical_gap_and_shared_expiry_do_not_certify_exhaustion() {
    let directory = tempfile::tempdir().unwrap();
    let (boundary, result) = capture(
        Scope::Lifecycle,
        "run",
        directory.path(),
        Instant::now() + Duration::from_secs(5),
        &|_, _, _| {
            let mut value = page(Scope::Lifecycle, 0, 1, 1);
            value["cursor"]["coverage"] = json!({"kind":"historical_gap"});
            Ok(value)
        },
    );
    assert!(result.is_err());
    assert_eq!(boundary["pages_retained"], 1);
    let (boundary, result) = capture(
        Scope::Attempts,
        "run",
        directory.path(),
        Instant::now(),
        &|_, _, _| panic!("expired must not invoke"),
    );
    assert!(result.is_err());
    assert_eq!(boundary["pages_retained"], 0);
}

#[test]
fn late_response_is_retained_without_deadline_extension() {
    let directory = tempfile::tempdir().unwrap();
    let (boundary, result) = capture(
        Scope::Lifecycle,
        "run",
        directory.path(),
        Instant::now() + Duration::from_millis(5),
        &|_, _, _| {
            std::thread::sleep(Duration::from_millis(10));
            Ok(page(Scope::Lifecycle, 0, 1, 1))
        },
    );
    assert!(result.is_err());
    assert_eq!(boundary["pages_retained"], 1);
    assert_eq!(boundary["validated_rows"], 0);
    assert!(
        directory
            .path()
            .join("run_lifecycle_v1-0000.json")
            .is_file()
    );
}

#[test]
fn failure_in_one_scope_preserves_other_scope_and_legacy_boundaries() {
    let directory = tempfile::tempdir().unwrap();
    let result = crate::capture::capture_history_scopes(
        "run",
        directory.path(),
        true,
        Instant::now() + Duration::from_secs(5),
        |args, kind, _| match kind {
            "attempts_v1" => Ok(page(Scope::Attempts, 0, 1, 1)),
            "run_lifecycle_v1" | "observations" => Err(EvalError::Validation("unavailable".into())),
            "attempts" => {
                assert_eq!(args[1], "attempts");
                Ok(json!([]))
            }
            _ => panic!("unexpected {kind}"),
        },
    );
    assert!(result.is_err());
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(directory.path().join("capture-completeness.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        manifest["semantic_scopes"]["attempts_v1"]["status"],
        "complete"
    );
    assert_eq!(
        manifest["semantic_scopes"]["run_lifecycle_v1"]["status"],
        "partial_or_unavailable"
    );
    assert_eq!(manifest["boundaries"]["events"]["status"], "unsupported");
    assert_eq!(manifest["complete"], false);
}
