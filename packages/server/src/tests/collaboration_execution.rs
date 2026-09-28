//! Production tool publication and worker filesystem execution (not goal UI acceptance).
use super::*;

#[tokio::test]
async fn published_workers_write_contributions_without_overwriting_user_work() {
    Box::pin(published_workers_preserve_user_work(false)).await;
}

#[tokio::test]
async fn published_workers_execute_in_explicit_registered_worktrees() {
    Box::pin(published_workers_preserve_user_work(true)).await;
}

async fn published_workers_preserve_user_work(isolated: bool) {
    let (mut state, session, root) = active_edit_execution_fixture_with_graph(
        bcode_workflow_store::DispatchSideEffect::ReadOnly,
        true,
    )
    .await;
    prepare_publication_source(&state, session, &PublicationOwner::CompletedSource).await;
    register_workflow_publication_tool(&mut state);
    configure_coding_plugins(&mut state);
    let trees = tempfile::tempdir().unwrap();
    if isolated {
        create_worker_trees(root.path(), trees.path());
    }
    std::fs::write(root.path().join("user.txt"), "uncommitted user work").unwrap();
    let provenance = state
        .sessions
        .session_summary(session)
        .await
        .unwrap()
        .execution
        .unwrap()
        .provenance;
    let task = |id: &str| {
        let directory = if isolated {
            trees.path().join(id)
        } else {
            root.path().to_path_buf()
        };
        let mut task = serde_json::json!({
            "task_id":id,"agent_profile":"build","read_only":false,
            "objective":format!("Produce one contribution.\ntool-call filesystem.write {}", serde_json::json!({"path":directory.join(format!("{id}.txt")),"contents":id})),
            "tool_allowlist":["filesystem.write"],
            "resources":[{"resource":format!("contribution:{id}"),"access":"write"}],
            "model_selection":{"provider":"bcode.fake-provider","model":"fake-echo"},
            "output":{"type_name":"boolean","schema":{"type":"boolean"}}
        });
        if isolated {
            task["worktree_directory"] = serde_json::json!(directory);
        }
        task
    };
    let call = bcode_model::ToolCall {
        id: "stage-coding".into(),
        name: "workflow.stage_task_group".into(),
        arguments: serde_json::json!({
            "version":2,"generated_ids":true,"mutation_id":"coding","run_id":"edit-run","expected_revision":1,
            "source_node_id":"agent","bind_source_activation":provenance.activation_id.unwrap(),
            "input":{"type_name":"boolean","schema":{"type":"boolean"}},
            "tasks":[task("left"),task("right")],
            "continuation":{"objective":"Return true after both workers settle","agent_profile":"plan","output":{"type_name":"boolean","schema":{"type":"boolean"}},"model_selection":{"provider":"bcode.fake-provider","model":"fake-echo"}},
            "first_edge_id":1,"reconnect":{"edge_id":0,"node_id":"waiting-successor"},"reconciliation":[]
        }),
    };
    let staged = invoke_task_with_permission(&state, session, &call, true)
        .await
        .unwrap();
    assert!(!staged.is_error, "{}", staged.output);
    let staged: serde_json::Value = serde_json::from_str(&staged.output).unwrap();
    let publish = bcode_model::ToolCall {
        id: "publish-coding".into(),
        name: "workflow.publish_run_graph_edit".into(),
        arguments: staged["publication_arguments"].clone(),
    };
    let (sender, _queued) = mpsc::channel(8);
    state.workflow_driver_sender.set(sender).unwrap();
    let published = invoke_task_with_permission(&state, session, &publish, true)
        .await
        .unwrap();
    assert!(!published.is_error, "{}", published.output);
    state.workflow_driver_sender.take();
    let state = Arc::new(state);
    state.start_workflow_driver().await;
    wait_for_contributions(&state).await;
    if isolated {
        verify_worker_directories(&state, root.path(), trees.path()).await;
    }
    drop(state);
    for id in ["left", "right"] {
        let directory = if isolated {
            trees.path().join(id)
        } else {
            root.path().to_path_buf()
        };
        assert_eq!(
            std::fs::read_to_string(directory.join(format!("{id}.txt"))).unwrap(),
            id
        );
    }
    assert_eq!(
        std::fs::read_to_string(root.path().join("user.txt")).unwrap(),
        "uncommitted user work"
    );
}

fn create_worker_trees(root: &Path, trees: &Path) {
    let git = |args: &[&str]| {
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
    };
    git(&["init", "--quiet"]);
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.invalid",
        "commit",
        "--allow-empty",
        "-m",
        "base",
    ]);
    for id in ["left", "right"] {
        git(&[
            "worktree",
            "add",
            "--detach",
            trees.join(id).to_str().unwrap(),
            "HEAD",
        ]);
    }
}

async fn verify_worker_directories(state: &ServerState, root: &Path, trees: &Path) {
    let links = state
        .workflow_store
        .lock()
        .unwrap()
        .execution_session_links_for_run("edit-run", 100)
        .unwrap();
    for id in ["left", "right"] {
        let link = links
            .iter()
            .find(|link| link.node_id == id)
            .expect("worker session");
        let summary = state
            .sessions
            .session_summary(SessionId::from_str(&link.session_id).unwrap())
            .await
            .unwrap();
        assert_eq!(
            summary.working_directory.canonicalize().unwrap(),
            trees.join(id).canonicalize().unwrap()
        );
        assert!(!root.join(format!("{id}.txt")).exists());
    }
}

async fn wait_for_contributions(state: &Arc<ServerState>) {
    let completed = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            // Resolve ordinary permission requests instead of bypassing policy.
            let pending: Vec<_> = state
                .pending_permissions
                .lock()
                .await
                .values()
                .cloned()
                .collect();
            for request in pending {
                state
                    .pending_permissions
                    .lock()
                    .await
                    .remove(&request.summary.permission_id);
                *request.decision.lock().await = Some(true);
                request.notify.notify_waiters();
            }
            let attempts = state
                .workflow_store
                .lock()
                .unwrap()
                .attempt_history("edit-run", None, 100)
                .unwrap();
            if attempts.iter().any(|attempt| {
                attempt.node_id == "waiting-successor" && attempt.terminal_at_ms.is_some()
            }) {
                assert!(
                    attempts.iter().all(|attempt| attempt.status == "succeeded"),
                    "{attempts:?}"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    completed.expect("workers and continuation settled");
}

fn configure_coding_plugins(state: &mut ServerState) {
    state.plugins = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
        &bcode_plugin::PluginSelection {
            mode: bcode_plugin::PluginSelectionMode::Explicit,
            enabled: BTreeSet::from([
                "bcode.workflow".into(),
                "bcode.fake-provider".into(),
                "bcode.filesystem".into(),
            ]),
            disabled: BTreeSet::new(),
        },
        &[
            bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../../plugins/filesystem-plugin/bcode-plugin.toml"),
                bcode_filesystem_plugin::static_plugin(),
            ),
            bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../../plugins/fake-provider-plugin/bcode-plugin.toml"),
                bcode_fake_provider_plugin::static_plugin(),
            ),
            bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../../plugins/workflow-plugin/bcode-plugin.toml"),
                bcode_workflow_plugin::static_plugin(),
            ),
        ],
    )
    .expect("acceptance plugins");
    state
        .selected_provider_context
        .settings
        .insert("fake_prompt_tool_directives".into(), "true".into());
    state
        .selected_provider_context
        .settings
        .insert("fake_structured_output_json".into(), "true".into());
}
