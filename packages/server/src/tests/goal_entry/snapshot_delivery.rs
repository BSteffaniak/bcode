//! Deterministic production execution, not live-model acceptance.
use super::*;

#[derive(Clone, Copy, Debug)]
pub(super) enum Case {
    Complete,
    Stale,
    Failed,
    Incomplete,
    Repository(super::repository_delivery::Case),
}

impl Case {
    pub(super) const fn repository(self) -> Option<super::repository_delivery::Case> {
        if let Self::Repository(case) = self {
            Some(case)
        } else {
            None
        }
    }
}

pub(super) fn install(request: &mut PluginWorkflowStartRequest, workspace: &Path, case: Case) {
    if let Case::Repository(case) = case {
        super::repository_delivery::install(request, workspace, case);
        return;
    }
    let manifest: serde_json::Value = toml::from_str(include_str!(
        "../../../../../plugins/shell-plugin/bcode-plugin.toml"
    ))
    .unwrap();
    let block = manifest["services"][0]["workflow_blocks"][0].clone();
    let snapshot = serde_json::json!({"version":1,"files":{"integrated.sh":format!("{LEFT_MODULE}{RIGHT_MODULE}")}});
    let command = if matches!(case, Case::Failed) {
        "exit 9"
    } else {
        COMBINED_CHECK
    };
    let argv = serde_json::json!(["/bin/sh", "-c", command]);
    let plan = serde_json::json!({"version":2,"cwd":".","commands":[{"argv":argv,"timeout_ms":10000,"accepted_exit_codes":[0]}],"environment":{"inherit":false,"set":{}},"output":{"preview_bytes":4096,"artifact_spill":false},"delivered_snapshot":snapshot});
    let node: bcode_workflow::NodeDefinition = serde_json::from_value(serde_json::json!({
        "id":"snapshot-check","name":"Verify immutable integrated snapshot","kind":"plugin_block",
        "input":block["input"],"output":block["output"],"configuration":block
    }))
    .unwrap();
    let edge = request
        .definition
        .edges
        .iter_mut()
        .find(|edge| edge.from == "loop.evaluation")
        .unwrap();
    let successor = edge.to.clone();
    edge.to = node.id.clone();
    edge.transform = Some(serde_json::from_value(serde_json::json!({"version":1,"expression":{"operation":"constant","value":plan},"output":node.input})).unwrap());
    let mut evaluation = request.definition.nodes["loop.evaluation"].clone();
    evaluation.id = "snapshot-evaluation".into();
    evaluation.configuration["execution_target"] = serde_json::json!("fresh_isolated");
    request.definition.edges.push(serde_json::from_value(serde_json::json!({"from":node.id,"to":evaluation.id,"kind":{"kind":"direct"},"transform":{"version":1,"expression":{"operation":"constant","value":request.input},"output":evaluation.input}})).unwrap());
    request.definition.edges.push(
        serde_json::from_value(
            serde_json::json!({"from":evaluation.id,"to":successor,"kind":{"kind":"direct"}}),
        )
        .unwrap(),
    );
    request.definition.nodes.insert(node.id.clone(), node);
    let old = evaluation.configuration["system_prompt"].as_str().unwrap();
    let instructions = old
        .split("\ntool-call workflow.execution_context")
        .next()
        .unwrap()
        .to_owned();
    let read = serde_json::json!({"path":workspace.join("integrated.sh"),"offset":1,"limit":100});
    let mut delivered = snapshot;
    if matches!(case, Case::Stale) {
        delivered["files"]["integrated.sh"] = serde_json::json!("stale replacement");
    }
    let inspect = serde_json::json!({"$fake_result":{"index":0,"pointer":"/outputs","where":{"node_id":"snapshot-check"},"select":"/inspection_arguments","latest_by":"created_at_ms"}});
    let left = serde_json::json!({"$fake_result":{"index":1,"pointer":"/outputs","where":{"node_id":"left"},"select":"/inspection_arguments","latest_by":"created_at_ms"}});
    let repaired = serde_json::json!({"$fake_result":{"index":2,"pointer":"/outputs","where":{"node_id":"repair-right"},"select":"/inspection_arguments","latest_by":"created_at_ms"}});
    let output_id = serde_json::json!({"$fake_result":{"index":3,"pointer":"/output/output_id"}});
    let left_id = serde_json::json!({"$fake_result":{"index":2,"pointer":"/output/output_id"}});
    let repaired_id = serde_json::json!({"$fake_result":{"index":1,"pointer":"/output/output_id"}});
    let report = serde_json::json!({
        "version":"2","delivered_snapshot":delivered,"integrated_targets":["integrated.sh"],
        "original_stop_condition":request.input["stop_condition"],"contribution_output_ids":[left_id,repaired_id],
        "criteria":[{"criterion":request.input["stop_condition"],"status":"passed","basis":"observed_check","check_indices":if matches!(case, Case::Incomplete) {vec![]} else {vec![0]},"evidence":"Combined arithmetic behavior verified on exact delivered bytes"}],
        "checks":[{"command":command,"workspace":workspace,"outcome":"passed","evidence":"Canonical shell snapshot execution","execution":{"output_id":output_id,"command_index":0,"argv":argv}}],
        "retained_workspaces":[workspace],"unresolved_work":[]
    });
    evaluation.configuration["system_prompt"] = serde_json::json!(format!(
        "{instructions}\ntool-call workflow.execution_context {{\"outputs_only\":true,\"limit\":3,\"$fake_json_pages\":{{\"items\":\"/outputs\",\"next\":\"/next_page_arguments\"}}}}\ntool-call workflow.execution_context {inspect}\ntool-call workflow.execution_context {left}\ntool-call workflow.execution_context {repaired}\ntool-call filesystem.read {read}\nloop-delivery {report}"
    ));
    request
        .definition
        .nodes
        .insert(evaluation.id.clone(), evaluation);
}

async fn approve_snapshot_execution(state: &Arc<ServerState>, run_id: &str) {
    let application =
        workflow_operations::WorkflowAuthoringApplication::new(state, ClientId::new());
    for approval in bcode_workflow::WorkflowRunApplication::list_workflow_mutation_approvals(
        &application,
        run_id.into(),
        100,
    )
    .await
    .unwrap()
    {
        bcode_workflow::WorkflowRunApplication::resolve_workflow_mutation_approval(
            &application,
            approval.approval_id,
            bcode_workflow::WorkflowMutationApprovalDecision::Approve,
        )
        .await
        .unwrap();
    }
}

pub(super) async fn exercise(case: Case) {
    let _execution = GOAL_ENTRY_EXECUTION.lock().await;
    let root = tempfile::tempdir().unwrap();
    let sessions = publication_fixture_sessions(root.path(), true);
    let session = sessions
        .create_session(None, root.path().into())
        .await
        .unwrap();
    let store = bcode_workflow_store::WorkflowStore::open_in_state_dir(root.path()).unwrap();
    let mut state = Arc::new(test_server_state_with_workflow_authorization(
        sessions, store,
    ));
    configure_goal_execution(Arc::get_mut(&mut state).unwrap(), root.path());
    std::fs::write(root.path().join("user.txt"), "uncommitted user work").unwrap();
    state.start_workflow_driver().await;
    let request = goal_entry_request_with_snapshot(
        session.id,
        root.path(),
        Arc::clone(&state),
        None,
        None,
        Some(case),
    )
    .await;
    let run_id = request.run_id.unwrap();
    let outcome = tokio::time::timeout(Duration::from_mins(1), async {
        loop {
            approve_goal_permissions(&state, session.id).await;
            approve_snapshot_execution(&state, &run_id).await;
            let (outputs, attempts, terminal) = {
                let store = state.workflow_store.lock().unwrap();
                (
                    store.validated_outputs(&run_id, 100).unwrap(),
                    store.attempt_history(&run_id, None, 100).unwrap(),
                    store.canonical_terminal_output(&run_id).unwrap(),
                )
            };
            if let Some(failed) = attempts
                .iter()
                .find(|attempt| attempt.status == "failed" && attempt.node_id != "right")
            {
                let links = state
                    .workflow_store
                    .lock()
                    .unwrap()
                    .execution_session_links_for_run(&run_id, 100)
                    .unwrap();
                for link in links {
                    if link.node_id.ends_with("integrate") {
                        eprintln!(
                            "HISTORY {} {:?}",
                            link.node_id,
                            state
                                .sessions
                                .session_history(link.session_id.parse().unwrap())
                                .await
                        );
                    }
                }
                panic!(
                    "unexpected failure {failed:?}; events {:?}",
                    state
                        .workflow_store
                        .lock()
                        .unwrap()
                        .event_history(&run_id, None, 100)
                        .unwrap()
                );
            }
            if let Some(judgement) = outputs
                .iter()
                .find(|output| output.node_id == "loop.judgement.evaluate")
            {
                let complete = matches!(
                    case,
                    Case::Complete | Case::Repository(super::repository_delivery::Case::Complete)
                );
                assert_eq!(
                    judgement.value["condition_met"], complete,
                    "{case:?}: {:?}",
                    judgement.value
                );
                if complete && terminal.is_none() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                }
                assert_eq!(terminal.is_some(), complete);
                assert_integrated_files(root.path());
                assert_correction_without_replay(&attempts);
                assert_worker_sessions(&state, &run_id, session.id);
                if complete {
                    let terminal = terminal.unwrap();
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    assert_eq!(
                        state
                            .workflow_store
                            .lock()
                            .unwrap()
                            .canonical_terminal_output(&run_id)
                            .unwrap(),
                        Some(terminal)
                    );
                }
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    if outcome.is_err() {
        let links = state
            .workflow_store
            .lock()
            .unwrap()
            .execution_session_links_for_run(&run_id, 100)
            .unwrap();
        for link in links {
            if link.node_id.ends_with("integrate") {
                eprintln!(
                    "HISTORY {} {:?}",
                    link.node_id,
                    state
                        .sessions
                        .session_history(link.session_id.parse().unwrap())
                        .await
                );
            }
        }
    }
    outcome.unwrap_or_else(|error| {
        let store = state.workflow_store.lock().unwrap();
        panic!(
            "{case:?}: {error}; attempts {:?}; events {:?}",
            store.attempt_history(&run_id, None, 100).unwrap(),
            store.event_history(&run_id, None, 100).unwrap()
        );
    });
    drop(state);
}

#[tokio::test]
async fn positive_snapshot_completion_is_stable() {
    exercise(Case::Complete).await;
}
#[tokio::test]
async fn stale_snapshot_rejects() {
    exercise(Case::Stale).await;
}
#[tokio::test]
async fn failed_snapshot_check_rejects() {
    exercise(Case::Failed).await;
}
#[tokio::test]
async fn incomplete_snapshot_evidence_rejects() {
    exercise(Case::Incomplete).await;
}
