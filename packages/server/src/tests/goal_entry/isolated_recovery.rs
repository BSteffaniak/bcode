//! Scripted model choices through goal entry, registered worker execution and real Git recovery.
use super::*;

fn git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn shell(directory: &Path, command: &str) -> serde_json::Value {
    serde_json::json!({"cwd":directory,"command":format!("/bin/sh -c '{}'", command.replace('\'', "'\\''")),"timeout_ms":10000})
}

pub(super) fn install_script(request: &mut PluginWorkflowStartRequest, root: &Path, trees: &Path) {
    let reference =
        |key: &str| serde_json::json!({"$fake_result":{"index":0,"pointer":format!("/{key}")}});
    let base = git(root, &["rev-parse", "HEAD"]);
    let task = |id: &str| {
        let workspace = trees.join(id);
        let contents = if id == "left" {
            LEFT_MODULE
        } else {
            RIGHT_MODULE
        };
        let write = serde_json::json!({"path":workspace.join("integrated.sh"),"contents":contents});
        let revision_path = trees.join(format!("{id}-revision.json"));
        let commit = shell(
            &workspace,
            &format!(
                "git add integrated.sh && git -c user.name=Test -c user.email=test@example.invalid commit -m contribution && printf '\"%s\"' \"$(git rev-parse HEAD)\" > {}",
                revision_path.display()
            ),
        );
        let revision = serde_json::json!({"path":revision_path,"offset":1,"limit":1});
        let result = serde_json::json!({"summary":format!("Committed {id}"),"evidence":["Commit command succeeded"],"blockers":[],"contributions":[{
            "workspace":workspace,"base_revision":base,"source_directory":root,"source_had_local_changes":true,
            "produced_revisions":[{"$fake_result":{"index":0,"pointer":""}}],"artifacts":[workspace.join("integrated.sh")],"validation":[],
            "remaining_work":["Resolve integration conflict and verify combined result"],"retention":"retained"
        }]});
        serde_json::json!({"task_id":id,"objective":format!("Contribute in the assigned checkout.\ntool-call filesystem.write {write}\ntool-call shell.run {commit}\ntool-call filesystem.read {revision}\nstructured-result {result}"),
            "agent_profile":"build","read_only":false,"worktree_directory":workspace,
            "tool_allowlist":["filesystem.write","filesystem.read","shell.run"],"resources":[{"resource":format!("checkout:{id}"),"access":"write"}],
            "model_selection":{"provider":"bcode.fake-provider","model":"fake-echo"}})
    };
    let integration = trees.join("integration");
    let probe = shell(
        &integration,
        "git -c user.name=Test -c user.email=test@example.invalid cherry-pick swarm-left && git -c user.name=Test -c user.email=test@example.invalid cherry-pick swarm-right",
    );
    let inspect = shell(
        &integration,
        "test -n \"$(git ls-files -u)\" && git rev-parse CHERRY_PICK_HEAD && git status --short",
    );
    let resolution = serde_json::json!({"path":integration.join("integrated.sh"),"contents":format!("{LEFT_MODULE}{RIGHT_MODULE}")});
    let verify = shell(
        &integration,
        &format!(
            "git add integrated.sh && git -c user.name=Test -c user.email=test@example.invalid -c core.editor=true cherry-pick --continue && ({COMBINED_CHECK}) && test -z \"$(git status --porcelain)\" && printf '\"%s\"' \"$(git rev-parse HEAD)\" > revision.txt && printf verified > verification.txt"
        ),
    );
    let group = serde_json::json!({
        "version":2,"generated_ids":true,"mutation_id":"isolated-goal-workers",
        "run_id":reference("run_id"),"expected_revision":reference("expected_revision"),
        "source_node_id":reference("source_node_id"),"bind_source_activation":reference("bind_source_activation"),
        "input":reference("input"),"preserve_source_output":true,"tasks":[task("left"),task("right")],
        "continuation":{"objective":format!("Integrate both retained contributions. Inspect the actual conflict before resolving it; never overwrite the source checkout.\ntool-call-expect-error CONFLICT :: shell.run {probe}\ntool-call shell.run {inspect}\ntool-call filesystem.write {resolution}\ntool-call shell.run {verify}"),
            "agent_profile":"build","read_only":false,"worktree_directory":integration,"tool_allowlist":["shell.run","filesystem.write"],
            "resources":[{"resource":"checkout:integration","access":"write"}],"model_selection":{"provider":"bcode.fake-provider","model":"fake-echo"}},
        "first_edge_id":reference("first_edge_id"),"reconnect":reference("reconnect"),
        "retain_source_edge_ids":reference("retain_source_edge_ids"),"reconciliation":reference("reconciliation")
    });
    let publication =
        serde_json::json!({"$fake_result":{"index":0,"pointer":"/publication_arguments"}});
    let source = request
        .definition
        .nodes
        .get_mut("loop.implementation")
        .unwrap();
    let instructions = source.configuration["system_prompt"].as_str().unwrap();
    source.configuration["system_prompt"] = serde_json::json!(format!(
        "{instructions}\ntool-call workflow.execution_context {{\"delegation_part\":\"serialized\",\"$fake_json_pages\":{{\"chunk\":\"/delegation/chunk\",\"next\":\"/delegation/next_arguments\"}}}}\ntool-call workflow.stage_task_group {group}\ntool-call workflow.publish_run_graph_edit {publication}"
    ));
    install_delivery_report(request, &integration, trees);
}

fn install_delivery_report(
    request: &mut PluginWorkflowStartRequest,
    integration: &Path,
    trees: &Path,
) {
    let inspect = |index, node| {
        serde_json::json!({"$fake_result":{
            "index":index,"pointer":"/outputs","where":{"node_id":node},
            "select":"/inspection_arguments","latest_by":"created_at_ms"
        }})
    };
    let result =
        |index, pointer| serde_json::json!({"$fake_result":{"index":index,"pointer":pointer}});
    let left = inspect(0, "left");
    let right = inspect(1, "right");
    // Evaluation remains read-only: inspect the revision recorded after the
    // continuation's successful combined check, alongside the actual artifact.
    let revision =
        serde_json::json!({"path":integration.join("revision.txt"),"offset":1,"limit":1});
    let read = serde_json::json!({"path":integration.join("integrated.sh"),"offset":1,"limit":100});
    let report = serde_json::json!({
        "version":"1","integrated_targets":[integration, result(1,"")],
        "contribution_output_ids":[result(3,"/output/output_id"),result(2,"/output/output_id")],
        "criteria":[{"criterion":request.input["stop_condition"],"status":"passed","evidence":"Inspected both canonical contributions, resolved artifact and current integrated revision"}],
        "checks":[{"command":COMBINED_CHECK,"workspace":integration,"outcome":"passed","evidence":"Continuation verified combined artifact and clean tracked checkout before recording revision; evaluator independently inspected artifact and recorded revision"}],
        "retained_workspaces":[trees.join("left"),trees.join("right"),integration],
        "unresolved_work":[]
    });
    let evaluation = request.definition.nodes.get_mut("loop.evaluation").unwrap();
    let instructions = evaluation.configuration["system_prompt"].as_str().unwrap();
    evaluation.configuration["system_prompt"] = serde_json::json!(format!(
        "{instructions}\ntool-call workflow.execution_context {{\"outputs_only\":true,\"limit\":3,\"$fake_json_pages\":{{\"items\":\"/outputs\",\"next\":\"/next_page_arguments\"}}}}\ntool-call workflow.execution_context {left}\ntool-call workflow.execution_context {right}\ntool-call filesystem.read {revision}\ntool-call filesystem.read {read}\nloop-delivery {report}"
    ));
}

fn prepare_checkouts(root: &Path, trees: &Path) -> String {
    git(root, &["init", "--quiet"]);
    std::fs::write(root.join("integrated.sh"), "# base\n").unwrap();
    git(root, &["add", "integrated.sh"]);
    git(
        root,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "base",
        ],
    );
    let base = git(root, &["rev-parse", "HEAD"]);
    for id in ["left", "right", "integration"] {
        git(
            root,
            &[
                "worktree",
                "add",
                "-b",
                &format!("swarm-{id}"),
                trees.join(id).to_str().unwrap(),
                "HEAD",
            ],
        );
    }
    std::fs::write(
        root.join("integrated.sh"),
        "# uncommitted user implementation\n",
    )
    .unwrap();
    base
}

#[tokio::test]
async fn goal_workers_recover_real_git_conflict_in_isolated_integration_checkout() {
    let _execution = GOAL_ENTRY_EXECUTION.lock().await;
    let root = tempfile::tempdir().unwrap();
    let trees = tempfile::tempdir().unwrap();
    let base = prepare_checkouts(root.path(), trees.path());
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
    state.start_workflow_driver().await;
    let request = goal_entry_request_with_workspace(
        session.id,
        root.path(),
        Arc::clone(&state),
        None,
        Some(trees.path().into()),
    )
    .await;
    let run_id = request.run_id.unwrap();
    tokio::time::timeout(Duration::from_mins(1), async {
        loop {
            approve_goal_permissions(&state, session.id).await;
            let outputs = state
                .workflow_store
                .lock()
                .unwrap()
                .validated_outputs(&run_id, 100)
                .unwrap();
            if let Some(evaluation) = outputs
                .iter()
                .find(|output| output.node_id == "loop.evaluation")
            {
                assert_eq!(evaluation.value["condition_met"], true, "{outputs:?}");
                let delivery = &evaluation.value["delivery"];
                assert_eq!(
                    delivery["integrated_targets"][0],
                    trees.path().join("integration").to_str().unwrap()
                );
                assert_eq!(
                    delivery["integrated_targets"][1],
                    git(&trees.path().join("integration"), &["rev-parse", "HEAD"])
                );
                assert_eq!(delivery["criteria"][0]["status"], "passed");
                assert_eq!(delivery["checks"][0]["outcome"], "passed");
                assert_eq!(delivery["retained_workspaces"].as_array().unwrap().len(), 3);
                for (index, id) in ["left", "right"].into_iter().enumerate() {
                    let output = outputs.iter().find(|output| output.node_id == id).unwrap();
                    assert_eq!(delivery["contribution_output_ids"][index], output.output_id);
                    assert_eq!(output.value["contributions"][0]["base_revision"], base);
                    let worker_revision = git(&trees.path().join(id), &["rev-parse", "HEAD"]);
                    assert_ne!(worker_revision, base);
                    assert_eq!(
                        output.value["contributions"][0]["produced_revisions"],
                        serde_json::json!([worker_revision])
                    );
                    assert_eq!(
                        output.value["contributions"][0]["workspace"],
                        trees.path().join(id).to_str().unwrap()
                    );
                }
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("isolated goal recovery must reach evaluation");
    drop(state);
    assert_eq!(git(root.path(), &["rev-parse", "HEAD"]), base);
    assert_eq!(
        std::fs::read_to_string(root.path().join("integrated.sh")).unwrap(),
        "# uncommitted user implementation\n"
    );
    let integration = trees.path().join("integration");
    assert_eq!(
        std::fs::read_to_string(integration.join("integrated.sh")).unwrap(),
        format!("{LEFT_MODULE}{RIGHT_MODULE}")
    );
    assert_eq!(
        std::fs::read_to_string(integration.join("verification.txt")).unwrap(),
        "verified"
    );
    assert!(git(&integration, &["ls-files", "-u"]).is_empty());
    for id in ["left", "right"] {
        assert_ne!(git(&trees.path().join(id), &["rev-parse", "HEAD"]), base);
        assert!(git(&trees.path().join(id), &["status", "--porcelain"]).is_empty());
    }
}
