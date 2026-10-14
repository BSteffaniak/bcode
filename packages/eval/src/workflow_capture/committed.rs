//! Pinned committed producer evidence; never promoted to full workflow capture.
use super::super::{EvalError, Path, write_json};
use bcode_workflow::{
    WORKFLOW_COMMITTED_HISTORY_VERSION, WorkflowCommittedHistoryPage, WorkflowCommittedHistoryQuery,
};
use std::time::{Duration, Instant};

/// Drain bounded immutable event pages while retaining their independent scope coverage.
pub fn capture(
    run: &str,
    directory: &Path,
    deadline: Instant,
    invoke: impl Fn(&[String], &str, Duration) -> Result<serde_json::Value, EvalError>,
) -> Result<usize, EvalError> {
    let mut query = WorkflowCommittedHistoryQuery {
        version: WORKFLOW_COMMITTED_HISTORY_VERSION,
        run_id: run.into(),
        after_sequence: 0,
        through_sequence: None,
        limit: 100,
    };
    let mut pages = 0_usize;
    let mut coverage = None;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(invalid());
        }
        let mut args = vec![
            "workflow".into(),
            "public-events".into(),
            run.into(),
            "--after-sequence".into(),
            query.after_sequence.to_string(),
            "--limit".into(),
            query.limit.to_string(),
            "--json".into(),
        ];
        if let Some(pin) = query.through_sequence {
            args.extend(["--through-sequence".into(), pin.to_string()]);
        }
        let label = format!("workflow-committed-events-{pages}");
        let value = invoke(&args, &label, remaining.min(Duration::from_secs(15)))?;
        write_json(directory.join(format!("{label}.json")), &value)?;
        let page: WorkflowCommittedHistoryPage =
            serde_json::from_value(value).map_err(|_| invalid())?;
        query.validate_page(&page).map_err(|_| invalid())?;
        let observed = (
            page.from_admission,
            page.attempt_coverage.clone(),
            page.lifecycle_coverage.clone(),
            page.control_coverage.clone(),
        );
        if coverage
            .as_ref()
            .is_some_and(|expected| expected != &observed)
        {
            return Err(invalid());
        }
        coverage = Some(observed);
        pages += 1;
        query.through_sequence = Some(page.through_sequence);
        let Some(next) = page.next_after_sequence else {
            return Ok(pages);
        };
        query.after_sequence = next;
    }
}

fn invalid() -> EvalError {
    EvalError::Validation("committed workflow evidence unavailable or inconsistent".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_workflow::{
        WorkflowCommittedHistoryScope as Scope, WorkflowSemanticHistoryCoverage as Coverage,
    };
    use std::cell::Cell;

    fn page() -> WorkflowCommittedHistoryPage {
        WorkflowCommittedHistoryPage {
            version: WORKFLOW_COMMITTED_HISTORY_VERSION,
            run_id: "run".into(),
            from_admission: true,
            scopes: vec![
                Scope::AttemptsV1,
                Scope::RunLifecycleV1,
                Scope::ControlSourcesV1,
            ],
            attempt_coverage: Coverage::Unsupported,
            lifecycle_coverage: Coverage::Unsupported,
            control_coverage: Coverage::Unsupported,
            through_sequence: 0,
            next_after_sequence: None,
            events: vec![],
        }
    }

    #[test]
    fn scoped_unavailable_coverage_is_retained_not_promoted() {
        let dir = tempfile::tempdir().unwrap();
        let calls = Cell::new(0);
        let result = capture(
            "run",
            dir.path(),
            Instant::now() + Duration::from_secs(1),
            |args, _, _| {
                calls.set(calls.get() + 1);
                assert_eq!(args[1], "public-events");
                Ok(serde_json::to_value(page()).unwrap())
            },
        )
        .unwrap();
        assert_eq!(result, 1);
        assert_eq!(calls.get(), 1);
        let retained: WorkflowCommittedHistoryPage = serde_json::from_slice(
            &std::fs::read(dir.path().join("workflow-committed-events-0.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(retained.attempt_coverage, Coverage::Unsupported);
    }

    #[test]
    fn pinned_pages_preserve_coverage_and_reject_changes() {
        for changed in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let calls = Cell::new(0);
            let result = capture(
                "run",
                dir.path(),
                Instant::now() + Duration::from_secs(5),
                |args, _, _| {
                    let index = calls.get();
                    calls.set(index + 1);
                    let mut page = page();
                    page.through_sequence = 2;
                    page.events.push(bcode_workflow::WorkflowHistoryEvent {
                        event_seq: index + 1,
                        run_id: "run".into(),
                        event_type: "committed_fact_v1".into(),
                        created_at_ms: 42,
                        payload: serde_json::json!({}),
                    });
                    if index == 0 {
                        page.next_after_sequence = Some(1);
                    } else {
                        assert!(
                            args.windows(2)
                                .any(|pair| pair == ["--through-sequence", "2"])
                        );
                        if changed {
                            page.control_coverage = Coverage::CompleteFromAdmission;
                        }
                    }
                    Ok(serde_json::to_value(page).unwrap())
                },
            );
            assert_eq!(result.is_err(), changed);
            assert_eq!(calls.get(), 2);
            // Even rejected pages remain available for diagnosing the mismatch.
            assert!(dir.path().join("workflow-committed-events-1.json").exists());
        }
    }

    #[test]
    fn wrong_identity_and_elapsed_deadline_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            capture(
                "other",
                dir.path(),
                Instant::now() + Duration::from_secs(1),
                |_, _, _| { Ok(serde_json::to_value(page()).unwrap()) }
            )
            .is_err()
        );
        assert!(
            capture("run", dir.path(), Instant::now(), |_, _, _| panic!(
                "expired capture invoked CLI"
            ))
            .is_err()
        );
    }
}
