use super::*;

struct Receipt {
    run: &'static str,
    session: &'static str,
    observation: &'static str,
    ready: bool,
    gap: bool,
}
impl ExecutionReadinessReceipt for Receipt {
    fn schema_version(&self) -> u16 {
        1
    }
    fn watch_id(&self) -> &'static str {
        "watch"
    }
    fn run_id(&self) -> &str {
        self.run
    }
    fn session_id(&self) -> &str {
        self.session
    }
    fn observation_id(&self) -> &str {
        self.observation
    }
    fn ready_before_dispatch(&self) -> bool {
        self.ready
    }
    fn prior_execution_gap(&self) -> bool {
        self.gap
    }
}
fn root() -> Receipt {
    Receipt {
        run: "run",
        session: "session",
        observation: "observation",
        ready: true,
        gap: false,
    }
}
fn ready_aggregate() -> EvalXrayAggregation {
    let mut reducer = EvalXrayAggregation::new(CaptureLimits::default());
    reducer.watch_ready("watch", false);
    reducer
}
#[test]
fn portable_hook_ready_is_reduced_without_local_reattach() {
    use bcode_xray_models::execution_watch::{
        ExecutionWatchEvent, ExecutionWatchPage, ExecutionWatchStatus,
    };
    use bcode_xray_models::{XRAY_SCHEMA_VERSION, XrayCorrelation, XrayEnvelope, XrayEvent};
    let mut reducer = ready_aggregate();
    let page = ExecutionWatchPage {
        schema_version: 1,
        watch_id: "watch".into(),
        next_cursor: 1,
        status: ExecutionWatchStatus::Ready,
        prior_execution_gap: false,
        contention_gaps: 0,
        omitted_links: 0,
        omitted_events: 0,
        events: vec![ExecutionWatchEvent {
            run_id: "run".into(),
            session_id: "session".into(),
            envelope: XrayEnvelope {
                schema_version: XRAY_SCHEMA_VERSION,
                observation_id: "observation".into(),
                sequence: 1,
                timestamp_unix_ms: 0,
                event: XrayEvent {
                    domain: "xray".into(),
                    name: "ready".into(),
                    sensitive: false,
                    capture: None,
                    correlation: XrayCorrelation {
                        session_id: "session".into(),
                        ..Default::default()
                    },
                    data: serde_json::json!({}),
                },
            },
        }],
    };
    reducer.execution_watch_page(&page);
    export_root(&mut reducer);
    reducer.watch_drained("watch");
    assert!(reducer.finish().evidence_complete);
}

#[test]
fn portable_empty_page_and_final_inventory_do_not_claim_drain() {
    use bcode_xray_models::execution_watch::{
        ExecutionWatchCutoff, ExecutionWatchFinalEvidence, ExecutionWatchStatus,
    };
    let mut reducer = ready_aggregate();
    reducer.execution_watch_final_evidence(&ExecutionWatchFinalEvidence {
        schema_version: 1,
        watch_id: "watch".into(),
        status: ExecutionWatchStatus::Finalized,
        prior_execution_gap: false,
        contention_gaps: 0,
        omitted_links: 1,
        omitted_events: 2,
        next_offset: None,
        cutoffs: vec![ExecutionWatchCutoff {
            run_id: "run".into(),
            session_id: "session".into(),
            finalization: None,
        }],
    });
    let evidence = reducer.finish();
    assert!(!evidence.watch_drained);
    assert_eq!(evidence.expected_sessions, 1);
    assert_eq!(evidence.observed_sessions, 0);
    assert!(!evidence.evidence_complete);
    assert!(evidence.issues.contains(&"watch_omitted_events"));
}

fn export_root(reducer: &mut EvalXrayAggregation) {
    let evidence = super::super::tests::capture_fixture(false, false);
    assert!(evidence.evidence_complete);
    reducer.segment_finished("run", "session", "watch", "observation", Some(evidence));
}

#[test]
fn owner_receipt_received_later_preserves_actual_pre_dispatch_readiness() {
    let mut reducer = ready_aggregate();
    reducer.expect_session("run", "session");
    reducer.readiness(&root());
    export_root(&mut reducer);
    reducer.watch_drained("watch");
    let evidence = reducer.finish();
    assert!(evidence.evidence_complete);
    assert_eq!(evidence.expected_sessions, 1);
    assert_eq!(evidence.observed_sessions, 1);
}

#[test]
fn complete_root_cannot_certify_expected_unobserved_descendant() {
    let mut reducer = ready_aggregate();
    reducer.readiness(&root());
    export_root(&mut reducer);
    reducer.expect_session("child-run", "child-session");
    reducer.watch_drained("watch");
    let evidence = reducer.finish();
    assert!(!evidence.evidence_complete);
    assert_eq!(evidence.expected_sessions, 2);
    assert_eq!(evidence.observed_sessions, 1);
}

#[test]
fn fresh_reattach_cannot_substitute_for_pre_dispatch_observation() {
    let mut reducer = ready_aggregate();
    let mut receipt = root();
    receipt.observation = "pre-dispatch-observation";
    reducer.readiness(&receipt);
    export_root(&mut reducer); // export belongs to a different observation
    reducer.watch_drained("watch");
    let evidence = reducer.finish();
    assert!(!evidence.evidence_complete);
    assert!(evidence.issues.contains(&"export_without_exact_readiness"));
}

#[test]
fn shared_session_links_have_independent_run_coverage() {
    let mut reducer = ready_aggregate();
    reducer.readiness(&root());
    export_root(&mut reducer);
    reducer.expect_session("other-run", "session");
    reducer.watch_drained("watch");
    let evidence = reducer.finish();
    assert_eq!(evidence.expected_sessions, 1);
    assert_eq!(evidence.expected_links, 2);
    assert_eq!(evidence.observed_sessions, 1);
    assert!(!evidence.evidence_complete);
}

#[test]
fn readiness_conflicts_fail_closed_identical_receipts_are_idempotent() {
    let mut reducer = ready_aggregate();
    reducer.readiness(&root());
    reducer.readiness(&root());
    assert_eq!(reducer.segments, 1);
    let mut conflict = root();
    conflict.ready = false;
    reducer.readiness(&conflict);
    assert!(
        reducer
            .finish()
            .issues
            .contains(&"conflicting_readiness_receipt")
    );
}

#[test]
fn gap_loss_and_revocation_are_not_repaired_by_later_readiness() {
    let mut reducer = ready_aggregate();
    reducer.gap();
    reducer.watch_loss(1, 2, 3);
    reducer.readiness(&root());
    export_root(&mut reducer);
    reducer.revoked();
    reducer.watch_drained("watch");
    let evidence = reducer.finish();
    assert!(!evidence.evidence_complete);
    assert!(evidence.revoked);
    assert!(evidence.issues.contains(&"watch_omitted_links"));
    assert!(evidence.issues.contains(&"watch_omitted_events"));
    assert!(evidence.issues.contains(&"watch_contention_gaps"));
}

#[test]
fn local_export_failure_is_not_hidden_by_successful_watch_drain() {
    let mut reducer = ready_aggregate();
    reducer.readiness(&root());
    let mut evidence = super::super::tests::capture_fixture(false, false);
    evidence.payloads_complete = false;
    // Check dimensions independently, even if a caller left the combined bit true.
    reducer.segment_finished("run", "session", "watch", "observation", Some(evidence));
    reducer.watch_drained("watch");
    assert!(!reducer.finish().evidence_complete);
}

#[test]
fn incomplete_capture_does_not_change_the_independent_task_judge() {
    let mut reducer = ready_aggregate();
    reducer.readiness(&root());
    export_root(&mut reducer);
    // Deterministic capture-only fault, not scheduler contention or a retried run.
    reducer.gap();
    reducer.watch_drained("watch");
    let evidence = reducer.finish();
    assert!(!evidence.evidence_complete);
    assert!(evidence.watch_drained);
    let judge = crate::EvalJudgeConfig::MetricThreshold {
        metric: "workflow.oracle_passed".into(),
        min: Some(1.0),
        max: None,
        required: true,
        variants: Vec::new(),
    };
    let directory = tempfile::tempdir().unwrap();
    for task_passed in [true, false] {
        let mut measurements = crate::EvalMeasurementSet::new();
        measurements.insert(
            "workflow.oracle_passed".into(),
            f64::from(u8::from(task_passed)),
        );
        let before = crate::run_judge(
            &judge,
            directory.path(),
            directory.path(),
            directory.path(),
            "",
            &measurements,
            "single",
        )
        .unwrap();
        measurements.insert(
            "eval.xray.evidence_complete".into(),
            f64::from(u8::from(evidence.evidence_complete)),
        );
        let after = crate::run_judge(
            &judge,
            directory.path(),
            directory.path(),
            directory.path(),
            "",
            &measurements,
            "single",
        )
        .unwrap();
        assert_eq!(before.passed, task_passed);
        assert_eq!(after.passed, before.passed);
        assert_eq!(after.required, before.required);
        assert_eq!(
            after.score.map(f64::to_bits),
            before.score.map(f64::to_bits)
        );
    }
}

#[test]
fn restart_gap_and_missing_drain_barrier_cannot_pass() {
    let mut reducer = ready_aggregate();
    let mut receipt = root();
    receipt.gap = true;
    reducer.readiness(&receipt);
    export_root(&mut reducer);
    let evidence = reducer.finish();
    assert!(!evidence.watch_drained);
    assert!(!evidence.evidence_complete);
    assert!(evidence.sessions["run"]["session"].segments[0].restart_gap_before);
}

#[test]
fn budget_exhaustion_keeps_aggregate_incomplete_not_a_smaller_successful_scope() {
    let mut reducer = EvalXrayAggregation::new(CaptureLimits {
        sessions: 1,
        segments: 1,
    });
    reducer.watch_ready("watch", false);
    reducer.readiness(&root());
    assert!(!reducer.expect_session("child", "child-session"));
    export_root(&mut reducer);
    reducer.watch_drained("watch");
    let evidence = reducer.finish();
    assert!(!evidence.evidence_complete);
    assert!(evidence.issues.contains(&"expected_link_budget_exhausted"));
}
