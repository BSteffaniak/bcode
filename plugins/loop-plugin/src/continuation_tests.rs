use super::*;
use crate::{LoopWorkflowInput, goal_workflow_spec, loop_workflow_spec};

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
            iteration: 2,
            planning_ready: true,
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
