//! Actual plugin-generated loop acceptance against the typed representation.
use super::*;
use bcode_workflow_store::{NewWorkflowRun, WorkflowExecutionAuthority, representation as rep};

#[test]
fn actual_loop_spec_executes_two_rounds_through_typed_store() {
    actual_loop(false);
}

#[test]
fn actual_judgement_loop_spec_executes_two_rounds_through_typed_store() {
    actual_loop(true);
}

fn fixture_authority() -> WorkflowExecutionAuthority {
    WorkflowExecutionAuthority {
        target_artifact_id: "artifact".into(),
        daemon_instance_id: "daemon".into(),
        generation: 1,
        fencing_token: "fence".into(),
    }
}

fn actual_loop(judgement: bool) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let (temp, legacy, db) = database_fixture().await;
            let input = configured_input(judgement);
            let spec = loop_workflow_spec(&input).unwrap();
            assert_eq!(
                rep::definition_dispatch_availability(spec.definition()).unwrap(),
                rep::DispatchAvailability::AgentControlLifecycle
            );
            let run = new_run(&spec, &input, temp.path());
            rep::admit(&*db, &run, spec.definition()).await.unwrap();
            let owner = fixture_authority();
            rep::attempts::claim_authority(&*db, &run.run_id, &owner)
                .await
                .unwrap();
            let mut evaluations = 0;
            for _ in 0..100 {
                rep::graph::frontier::advance(&*db, &owner, &run.run_id, 32)
                    .await
                    .unwrap();
                let ready = rep::graph::ready(&*db, &run.run_id, 1, None, 100)
                    .await
                    .unwrap();
                if ready.is_empty() {
                    if rep::graph::frontier::pending(&*db, &run.run_id)
                        .await
                        .unwrap()
                    {
                        continue;
                    }
                    break;
                }
                for activation in ready {
                    let node = &spec.definition().nodes[&activation.node_id];
                    match node.kind {
                        bcode_workflow::NodeKind::Branch | bcode_workflow::NodeKind::Repeat => {
                            rep::graph::control::settle(
                                &*db,
                                &owner,
                                &run.run_id,
                                &node.id,
                                &activation.activation_id,
                                1,
                            )
                            .await
                            .unwrap();
                        }
                        bcode_workflow::NodeKind::Input => {
                            rep::graph::gates::resolve(
                                &*db,
                                &owner,
                                &run.run_id,
                                &node.id,
                                &activation.activation_id,
                                1,
                                Some(&activation.input),
                            )
                            .await
                            .unwrap();
                        }
                        bcode_workflow::NodeKind::Agent | bcode_workflow::NodeKind::PluginBlock => {
                            let mut value = activation.input.clone();
                            if node.id == "loop.evaluation" {
                                evaluations += 1;
                                value["condition_met"] = serde_json::json!(evaluations == 2);
                            }
                            complete_leaf(&*db, &owner, &run, node, activation, value).await;
                        }
                        kind => panic!("unsupported normal-loop node {kind:?}"),
                    }
                }
            }
            assert_eq!(evaluations, 2);
            assert_eq!(
                rep::runs::terminalize(&*db, &owner, &run.run_id)
                    .await
                    .unwrap(),
                Some(rep::runs::RunState::Completed)
            );
            let output = rep::graph::completion::terminal(&*db, &run.run_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(output.value["condition_met"], true);
            drop(legacy);
        });
}

async fn complete_leaf(
    db: &dyn switchy_database::Database,
    owner: &WorkflowExecutionAuthority,
    run: &NewWorkflowRun,
    node: &bcode_workflow::NodeDefinition,
    activation: rep::graph::Activation,
    value: serde_json::Value,
) {
    let prepared = bcode_workflow_store::PreparedAttempt {
        run_id: run.run_id.clone(),
        node_id: node.id.clone(),
        activation_id: activation.activation_id.clone(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({"test":"normalized owner result"}),
        prepared_at_ms: 1,
    };
    let id = rep::attempts::prepare(db, owner, &prepared).await.unwrap();
    rep::attempts::attach_receipt(
        db,
        owner,
        &bcode_workflow_store::DispatchReceipt {
            run_id: run.run_id.clone(),
            node_id: node.id.clone(),
            activation_id: activation.activation_id.clone(),
            attempt: 1,
            dispatch_identity: id.clone(),
            receipt: serde_json::json!({}),
            admitted_at_ms: 2,
        },
    )
    .await
    .unwrap();
    rep::graph::completion::complete(
        db,
        owner,
        1,
        &id,
        &bcode_workflow::ValidatedOutput {
            output_id: format!("{}:result", activation.activation_id),
            run_id: run.run_id.clone(),
            node_id: node.id.clone(),
            activation_id: activation.activation_id,
            schema_id: node.output.type_name.clone(),
            schema_version: 1,
            value,
            artifact_reference: None,
            created_at_ms: 3,
        },
    )
    .await
    .unwrap();
}

fn new_run(
    spec: &bcode_workflow::WorkflowSpec<LoopWorkflowIteration>,
    input: &LoopWorkflowInput,
    path: &std::path::Path,
) -> NewWorkflowRun {
    NewWorkflowRun {
        run_id: "actual-loop".into(),
        definition_id: spec.identity().definition_id.clone(),
        definition_version: spec.identity().definition_version,
        workspace_snapshot: path.to_string_lossy().into_owned(),
        parent_session_id: Some(SessionId::new().to_string()),
        parent_session_generation: None,
        binding: None,
        authored_provenance: None,
        input: Some(serde_json::to_value(loop_workflow_initial_value(input)).unwrap()),
        execution_authority: None,
        created_at_ms: 1,
        authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
            version: 1,
            provider_id: "test".into(),
            profile_id: "build".into(),
            policy_digest_sha256: "a".repeat(64),
        },
        authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
        limits: bcode_workflow::WorkflowRunLimits {
            cycle_cap: 3,
            ..Default::default()
        },
    }
}

#[test]
fn representation_plugin_capability_rejects_extra_authority_or_preparation() {
    let input = LoopWorkflowInput::new("implement".into(), "verify".into(), 2).unwrap();
    let definition = loop_workflow_spec(&input).unwrap().definition().clone();
    for (field, value) in [
        ("preparation_required", serde_json::json!(true)),
        ("effect", serde_json::json!("mutating")),
    ] {
        let mut changed = definition.clone();
        changed
            .nodes
            .get_mut("loop.judgement.evaluate")
            .unwrap()
            .configuration[field] = value;
        assert!(
            rep::definition_dispatch_availability(&changed).is_err()
                || rep::definition_dispatch_availability(&changed).unwrap()
                    == rep::DispatchAvailability::UnsupportedLifecycle
        );
    }
    let mut changed = definition;
    changed
        .nodes
        .get_mut("loop.judgement.evaluate")
        .unwrap()
        .configuration["authorization"]["explicit_grant_required"] = serde_json::json!(true);
    assert_eq!(
        rep::definition_dispatch_availability(&changed).unwrap(),
        rep::DispatchAvailability::UnsupportedLifecycle
    );
}

fn configured_input(judgement: bool) -> LoopWorkflowInput {
    let mut input = LoopWorkflowInput::new("Implement".into(), "Verify".into(), 3).unwrap();
    if judgement {
        input.judgement_evaluation = Some(judgement_evaluation::EvaluationConfig {
            provider_plugin_id: "bcode.jev".into(),
            model_id: "jev-1.13.0".into(),
            auth_profile: String::new(),
            threshold_percent: 90,
            on_failure: judgement_evaluation::FailurePolicy::Pause,
        });
    }
    input
}

async fn database_fixture() -> (
    tempfile::TempDir,
    bcode_workflow_store::WorkflowStore,
    Box<dyn switchy_database::Database>,
) {
    let temp = tempfile::tempdir().unwrap();
    let legacy =
        bcode_workflow_store::WorkflowStore::create_coexistence_baseline(temp.path()).unwrap();
    let db = switchy_database_connection::init_sqlite_rusqlite(Some(
        &bcode_workflow_store::workflow_database_path(temp.path()),
    ))
    .unwrap();
    rep::install(&*db).await.unwrap();
    (temp, legacy, db)
}
