use super::*;
use crate::{LoopExternalBlocker, LoopWorkflowInput, goal_workflow_spec, loop_workflow_spec};

#[test]
fn continuation_failures_are_not_successful_commands() {
    for error in [
        "No associated loop",
        "Unsupported loop continuation topology",
        "workflow state is unavailable",
        "permission denied",
    ] {
        assert!(!continuation_response(Err(error.into())).success);
    }
    assert!(continuation_response(Ok("Granted continuation".into())).success);
    assert!(!command(SessionId::new(), "0").success);
}

#[test]
fn active_grants_preserve_exact_caps_and_reject_unknown_or_terminal_state() {
    let mut run = source(true).run;
    run.status = bcode_workflow::RunStatus::Running;
    let mut allowance = bcode_workflow::WorkflowExecutionAllowanceObservation {
        run_cap: 10,
        run_consumed: Some(10),
        root_cap: 20,
        root_consumed: Some(20),
    };
    assert_eq!(
        active_allowance_action(&run, Some(&allowance), 7).unwrap(),
        bcode_workflow::WorkflowRunControlAction::IncreaseExecutionAllowance {
            expected_cap: 10,
            target_cap: 17
        }
    );
    assert!(active_allowance_action(&run, None, 7).is_err());
    assert!(active_allowance_action(&run, Some(&allowance), 0).is_err());
    assert!(active_allowance_action(&run, Some(&allowance), u64::MAX).is_err());
    allowance.run_consumed = None;
    allowance.root_consumed = None;
    assert!(active_allowance_action(&run, Some(&allowance), 7).is_err());
    allowance.run_consumed = Some(10);
    run.status = bcode_workflow::RunStatus::Paused;
    assert!(active_allowance_action(&run, Some(&allowance), 7).is_ok());
    run.cancellation_requested_at_ms = Some(1);
    assert!(active_allowance_action(&run, Some(&allowance), 7).is_err());
    run.cancellation_requested_at_ms = None;
    run.status = bcode_workflow::RunStatus::Completed;
    assert!(active_allowance_action(&run, Some(&allowance), 7).is_err());
    assert!(!command(SessionId::new(), "--worker-attempts 0").success);
}

#[test]
fn short_continuation_concurrency_does_not_exceed_total_attempt_allowance() {
    let mut checkpoint = source(false);
    checkpoint.limits.concurrency_cap = 100;
    checkpoint.limits.retry_cap = 0;
    let continued = request(checkpoint, 1).unwrap();
    assert!(continued.successor.limits.node_execution_cap < 100);
    assert_eq!(
        u64::from(continued.successor.limits.concurrency_cap),
        continued.successor.limits.node_execution_cap
    );
}

#[test]
fn extra_attempts_are_explicit_and_preserve_continuation_state() {
    let baseline = request(source(true), 2).unwrap();
    let extra = request_with_allowance(source(true), 2, 19).unwrap();
    assert_eq!(extra.successor.definition, baseline.successor.definition);
    assert_eq!(extra.successor.input, baseline.successor.input);
    let mut expected = baseline.successor.limits;
    expected.node_execution_cap += 19;
    assert_eq!(extra.successor.limits, expected);
    assert!(request_with_allowance(source(true), 2, i64::MAX as u64).is_err());
    assert!(request_with_allowance(source(true), 2, u64::MAX).is_err());
    assert_eq!(parse_allowance("2").unwrap(), (2, 0));
    assert_eq!(parse_allowance("2 --worker-attempts 19").unwrap(), (2, 19));
    for invalid in [
        "",
        "0",
        "-1",
        "2 --worker-attempts 0",
        "2 --worker-attempts -1",
        "2 extra",
        "2 --worker-attempts 18446744073709551616",
    ] {
        assert!(parse_allowance(invalid).is_err());
    }
}

fn source(progress: bool) -> bcode_workflow::WorkflowContinuationSource {
    let input = LoopWorkflowInput::new(
        "accepted implementation".into(),
        "accepted stop condition".into(),
        2,
    )
    .unwrap();
    let spec = if progress {
        goal_workflow_spec(&input)
    } else {
        loop_workflow_spec(&input)
    }
    .unwrap();
    let session = SessionId::new();
    bcode_workflow::WorkflowContinuationSource {
        run: bcode_workflow::WorkflowRunSummary {
            run_id: "original".into(),
            definition_id: "definition".into(),
            definition_version: 1,
            workspace_snapshot: "/workspace".into(),
            parent_session_id: Some(session.to_string()),
            parent_session_generation: None,
            binding: Some(bcode_workflow::WorkflowRunBinding {
                owner_plugin_id: PLUGIN_ID.into(),
                workflow_kind: WORKFLOW_KIND.into(),
                scope_key: session.to_string(),
                display_label: None,
                single_active: true,
            }),
            authored_provenance: None,
            terminal_output_id: None,
            terminal_output_checksum_sha256: None,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "policy".into(),
                profile_id: "build".into(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            status: bcode_workflow::RunStatus::Failed,
            cancellation_requested_at_ms: None,
            created_at_ms: 1,
            updated_at_ms: 2,
        },
        definition: spec.definition().clone(),
        input: serde_json::to_value(LoopWorkflowIteration {
            implementation_prompt: input.implementation_prompt,
            stop_condition: input.stop_condition,
            max_iterations: 2,
            judgement_evaluation: None,
            iteration: 2,
            planning_ready: true,
            external_blocker: LoopExternalBlocker::None,
            condition_met: false,
            evidence: vec!["remaining work".into()],
            summary: "incomplete".into(),
        })
        .unwrap(),
        repeat_node_id: "loop.repeat".into(),
        graph_revision: 1,
        output_checksum: "a".repeat(64),
        iterations_completed: 2,
        total_iterations_completed: 2,
        document_scope_id: "original".into(),
        limits: bcode_workflow::WorkflowRunLimits {
            cycle_cap: 2,
            ..Default::default()
        },
    }
}

#[test]
fn graph_allowance_counts_initialization_once_and_excludes_controls() {
    let input = LoopWorkflowInput::new("implement".into(), "done".into(), 2).unwrap();
    let plain = loop_workflow_spec(&input).unwrap();
    let goal = goal_workflow_spec(&input).unwrap();
    assert_eq!(executable_node_count(plain.definition()).unwrap(), 2);
    assert_eq!(executable_node_count(goal.definition()).unwrap(), 3);
    let mut revised = goal.definition().clone();
    let mut worker = revised.nodes["loop.implementation"].clone();
    worker.id = "worker".into();
    revised.nodes.insert(worker.id.clone(), worker);
    assert_eq!(executable_node_count(&revised).unwrap(), 4);
}

#[test]
fn continuation_budgets_reachable_delegated_agents() {
    let mut source = source(false);
    let mut worker = source.definition.nodes["loop.implementation"].clone();
    worker.id = "delegated-worker".into();
    source.definition.nodes.insert(worker.id.clone(), worker);
    let edge = source
        .definition
        .edges
        .iter_mut()
        .find(|edge| edge.from == "loop.implementation" && edge.to == "loop.evaluation")
        .expect("evaluation edge");
    edge.to = "delegated-worker".into();
    source
        .definition
        .edges
        .push(bcode_workflow::EdgeDefinition {
            from: "delegated-worker".into(),
            to: "loop.evaluation".into(),
            kind: bcode_workflow::EdgeKind::Direct,
            transform: None,
        });
    let retry_allowance = u64::from(source.limits.retry_cap) + 1;
    let continued = request(source, 3).expect("continue revised loop");
    assert_eq!(
        continued.successor.limits.node_execution_cap,
        3 * 3 * retry_allowance
    );
    assert!(
        continued
            .successor
            .definition
            .nodes
            .contains_key("delegated-worker")
    );
}

#[test]
fn continuation_reserves_judgement_node_and_keeps_selected_config() {
    let mut source = source(false);
    let config = crate::judgement_evaluation::parse_config("bcode.jev/jev-1.13.0/-/90/pause")
        .unwrap()
        .unwrap();
    let mut input = LoopWorkflowInput::new(
        "accepted implementation".into(),
        "accepted stop condition".into(),
        2,
    )
    .unwrap();
    input.judgement_evaluation = Some(config.clone());
    source.definition = loop_workflow_spec(&input).unwrap().definition().clone();
    source.input["judgement_evaluation"] = serde_json::to_value(config).unwrap();
    let continued = request(source, 3).unwrap();
    assert_eq!(continued.successor.limits.node_execution_cap, 36);
    assert!(
        continued
            .successor
            .definition
            .nodes
            .contains_key("loop.judgement.evaluate")
    );
    assert_eq!(
        continued.successor.input["judgement_evaluation"]["model_id"],
        "jev-1.13.0"
    );
}

#[test]
fn continuation_preserves_prompts_and_skips_initialization() {
    for progress in [false, true] {
        let source = source(progress);
        assert!(request(source.clone(), 0).is_err());
        let mut overflow = source.clone();
        overflow.total_iterations_completed = u64::from(u32::MAX);
        assert!(request(overflow, 1).is_err());
        let mut achieved = source.clone();
        achieved.input["condition_met"] = serde_json::json!(true);
        assert!(request(achieved, 1).is_err());
        let implementation = source.definition.nodes["loop.implementation"].clone();
        let evaluation = source.definition.nodes["loop.evaluation"].clone();
        let request = request(source, 3).unwrap();
        request.successor.definition.validate().unwrap();
        assert_eq!(
            request.successor.input["implementation_prompt"],
            "accepted implementation"
        );
        assert_eq!(
            request.successor.input["stop_condition"],
            "accepted stop condition"
        );
        assert_eq!(request.successor.input["iteration"], 3);
        assert_eq!(request.successor.input["max_iterations"], 5);
        assert_eq!(
            request.successor.definition.nodes["loop.implementation"],
            implementation
        );
        assert_eq!(
            request.successor.definition.nodes["loop.evaluation"],
            evaluation
        );
        assert_eq!(
            request.successor.definition.entries,
            ["loop.implementation"]
        );
        assert_eq!(request.successor.limits.cycle_cap, 3);
        assert_eq!(request.successor.limits.node_execution_cap, 24);
        assert!(
            !request
                .successor
                .definition
                .nodes
                .contains_key("goal.initialization")
        );
    }
}
