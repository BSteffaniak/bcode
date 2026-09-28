//! Exercise the shipped goal surface through its public host contract.
use super::*;
use bcode_plugin_sdk::tui::*;

struct GoalEntryHost {
    session: SessionId,
    state: Arc<ServerState>,
    workspace: PathBuf,
    tasks: StdMutex<Vec<PluginTask>>,
    starts: StdMutex<Vec<PluginWorkflowStartRequest>>,
    execution_cap: Option<u64>,
}

impl PluginTuiHost for GoalEntryHost {
    fn spawn(&self, task: PluginTask) {
        self.tasks.lock().unwrap().push(task);
    }
    fn spawn_blocking(&self, _: Box<dyn FnOnce() + Send + 'static>) {
        panic!("goal entry must not require blocking fixture work");
    }
    fn request_redraw(&self) {}
    fn prepare_fresh_session(&self, existing: Option<SessionId>) -> PluginCreateSessionFuture {
        assert_eq!(
            existing,
            Some(self.session),
            "goal must retain the host session"
        );
        let session = self.session;
        Box::pin(async move { Ok(session) })
    }
    fn workflow_delegation_preflight(
        &self,
        plugin_id: String,
    ) -> PluginWorkflowDelegationPreflightFuture {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let directory = tempfile::tempdir()
                .map_err(|error| PluginTuiHostError::Internal(error.to_string()))?;
            let endpoint =
                bcode_ipc::IpcEndpoint::unix_socket(directory.path().join("preflight.sock"));
            let listener = LocalIpcListener::bind(&endpoint)
                .map_err(|error| PluginTuiHostError::Internal(error.to_string()))?;
            let server = tokio::spawn(async move {
                let stream = listener.accept().await.expect("preflight client");
                handle_client(stream, state)
                    .await
                    .expect("preflight request");
            });
            let result = bcode_client::BcodeClient::new(endpoint)
                .workflow_delegation_preflight(plugin_id)
                .await
                .map_err(|error| PluginTuiHostError::Internal(error.to_string()));
            server.abort();
            result
        })
    }
    fn generate_observable_structured_output(
        &self,
        request: PluginStructuredGenerationRequest,
        _: PluginStructuredGenerationControl,
    ) -> PluginStructuredGenerationFuture {
        Box::pin(async move {
            Ok(PluginStructuredGenerationResult {
                output: serde_json::json!({"outcome":"ready", "clarification":"", "implementation_prompt":"Implement two contributions and verify the integrated result", "stop_condition":"Both contributions are integrated and verified"}),
                source: Some(bcode_session_models::SessionDerivationSourceSnapshot {
                    version: 1,
                    session_id: request.source_session_id.unwrap(),
                    generation: 0,
                    latest_sequence: 0,
                    title: None,
                    working_directory: "/acceptance".into(),
                }),
            })
        })
    }
    fn associated_workflow(&self, _: PluginWorkflowLookup) -> PluginWorkflowLookupFuture {
        Box::pin(async { Ok(None) })
    }
    fn start_workflow(&self, mut request: PluginWorkflowStartRequest) -> PluginWorkflowStartFuture {
        request.run_id = Some("goal-entry-acceptance".into());
        install_goal_script(&mut request, &self.workspace);
        if let Some(cap) = self.execution_cap {
            request.limits.node_execution_cap = cap;
            request.limits.concurrency_cap = 1;
        }
        request.identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
            request.identity.kind.clone(),
            &request.definition,
        )
        .unwrap();
        self.starts.lock().unwrap().push(request.clone());
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let started = bcode_workflow::WorkflowRunApplication::start_workflow(
                &workflow_operations::WorkflowAuthoringApplication::new(&state, ClientId::new()),
                bcode_workflow::WorkflowStartRequest {
                    identity: request.identity,
                    definition: request.definition,
                    run_id: request.run_id,
                    workspace_snapshot: None,
                    parent_session_id: request.parent_session_id,
                    input: request.input,
                    binding: bcode_workflow_store::WorkflowRunBinding {
                        owner_plugin_id: request.binding.owner_plugin_id,
                        workflow_kind: request.binding.workflow_kind,
                        scope_key: request.binding.scope_key,
                        display_label: request.binding.display_label,
                        single_active: request.binding.single_active,
                    },
                    limits: bcode_workflow_store::WorkflowRunLimits {
                        deadline_at_ms: None,
                        node_execution_cap: request.limits.node_execution_cap,
                        concurrency_cap: request.limits.concurrency_cap,
                        cycle_cap: request.limits.cycle_cap,
                        retry_cap: request.limits.retry_cap,
                        recursion_depth_cap: request.limits.recursion_depth_cap,
                        descendant_cap: request.limits.descendant_cap,
                    },
                },
            )
            .await
            .expect("goal host application launch");
            Ok(PluginWorkflowStartResponse {
                run_id: started.run.run_id,
                runtime_work_id: started.runtime_work_id.to_string(),
            })
        })
    }
}

// Script only the model's choices; identity, authorization, publication and execution
// still pass through the production tools and driver.
const LEFT_MODULE: &str = "subtotal() { echo $(( $1 * $2 )); }\n";
const RIGHT_MODULE: &str = "total() { echo $(( $(subtotal \"$1\" \"$2\") + 3 )); }\n";
const BROKEN_RIGHT_MODULE: &str = "total() { echo $(( $(subtotal \"$1\" \"$2\") + 4 )); }\n";
const COMBINED_CHECK: &str = ". ./integrated.sh; actual=$(total 3 8); test \"$actual\" = 27 || { printf \"expected total 27, got %s\\n\" \"$actual\"; exit 1; }; test \"$(total 0 8)\" = 3";

fn install_goal_script(request: &mut PluginWorkflowStartRequest, workspace: &Path) {
    let reference =
        |pointer: &str| serde_json::json!({"$fake_result":{"index":0,"pointer":pointer}});
    let source = &request.definition.nodes["loop.implementation"];
    let successor = request
        .definition
        .edges
        .iter()
        .position(|edge| edge.from == source.id && edge.to == "loop.evaluation")
        .expect("goal evaluation successor");
    let task = |id: &str| {
        serde_json::json!({
            "task_id":id,"objective":format!("Implement {id}.\ntool-call filesystem.write {}", serde_json::json!({"path":workspace.join(format!("{id}.sh")),"contents":if id == "left" { LEFT_MODULE } else { RIGHT_MODULE }})),
            "agent_profile":"build","read_only":false,
            "tool_allowlist":["filesystem.write"],
            "resources":[{"resource":format!("contribution:{id}"),"access":"write"}],
            "model_selection":{"provider":"bcode.fake-provider","model":"fake-echo"},"output":{"type_name":"boolean","schema":{"type":"boolean"}}
        })
    };
    // Integration consumes the files, not workers' boolean claims. The shell's
    // successful exit gates the verification receipt; assertions below inspect both.
    let integrate = serde_json::json!({
        "command":format!("/bin/sh -c 'cat left.sh right.sh > integrated.sh && ({COMBINED_CHECK}) && printf verified > verification.txt'"),
        "cwd":workspace,"timeout_ms":10000
    });
    let mut group = serde_json::json!({
        "version":2,"generated_ids":true,"mutation_id":"goal-workers","failure_policy":"collect_outcomes",
        "run_id":reference("/run_id"),"expected_revision":reference("/graph/revision"),
        "source_node_id":reference("/node_id"),"bind_source_activation":reference("/activation_id"),
        "input":source.output,"preserve_source_output":true,
        "tasks":[task("left"),task("right")],
        "continuation":{"objective":format!("Integrate the actual contributions and verify the combined result.\ntool-call shell.run {integrate}"), "agent_profile":"build", "read_only":false, "tool_allowlist":["shell.run"], "resources":[{"resource":"integration","access":"write"}], "model_selection":{"provider":"bcode.fake-provider","model":"fake-echo"}},
        "first_edge_id":reference("/graph/next_edge_id"),
        "reconnect":{"edge_id":successor,"node_id":"loop.evaluation"},
        "reconciliation":[]
    });
    install_corrective_script(&mut group, &source.output, successor, workspace, &integrate);
    let publication =
        serde_json::json!({"$fake_result":{"index":0,"pointer":"/publication_arguments"}});
    let source = request
        .definition
        .nodes
        .get_mut("loop.implementation")
        .unwrap();
    let instructions = source.configuration["system_prompt"].as_str().unwrap();
    source.configuration["system_prompt"] = serde_json::json!(format!(
        "{instructions}\ntool-call workflow.execution_context {{\"limit\":1,\"compact\":true}}\ntool-call workflow.stage_task_group {group}\ntool-call workflow.publish_run_graph_edit {publication}"
    ));
    let evaluation = request.definition.nodes.get_mut("loop.evaluation").unwrap();
    let instructions = evaluation.configuration["system_prompt"].as_str().unwrap();
    let read = serde_json::json!({"path":workspace.join("verification.txt"),"offset":1,"limit":1});
    evaluation.configuration["system_prompt"] =
        serde_json::json!(format!("{instructions}\ntool-call filesystem.read {read}"));
}

fn install_corrective_script(
    group: &mut serde_json::Value,
    source: &bcode_workflow::ValueSchema,
    successor: usize,
    workspace: &Path,
    integrate: &serde_json::Value,
) {
    let boolean = bcode_workflow::ValueSchema {
        type_name: "boolean".into(),
        schema: serde_json::json!({"type":"boolean"}),
    };
    let failed_output = bcode_workflow::ValueSchema {
        type_name: "receipt".into(),
        schema: serde_json::json!({"type":"string","pattern":"x"}),
    };
    // A valid but unsupported fake-provider schema fails only this worker after its
    // file effect. Recovery must retain that failure, inspect the artifact and correct it.
    group["tasks"][1]["output"] = serde_json::to_value(&failed_output).unwrap();
    let results = bcode_workflow::named_result_schema(
        "delegation.results".into(),
        &BTreeMap::from([
            (
                "left".into(),
                bcode_workflow::workflow_member_outcome_schema(&boolean).unwrap(),
            ),
            (
                "right".into(),
                bcode_workflow::workflow_member_outcome_schema(&failed_output).unwrap(),
            ),
        ]),
    )
    .unwrap();
    let input = bcode_workflow::named_result_schema(
        "bcode.delegation_input.v2".into(),
        &BTreeMap::from([
            ("results".into(), results),
            ("source".into(), source.clone()),
        ]),
    )
    .unwrap();
    let reference =
        |pointer: &str| serde_json::json!({"$fake_result":{"index":0,"pointer":pointer}});
    let transform = bcode_workflow::WorkflowTransform {
        version: bcode_workflow::WORKFLOW_TRANSFORM_VERSION,
        expression: bcode_workflow::WorkflowTransformExpression::Input {
            source: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT.into(),
            path: "source".into(),
        },
        output: source.clone(),
    };
    let fix = serde_json::json!({"path":workspace.join("right.sh"),"contents":RIGHT_MODULE});
    let correction = serde_json::json!({
        "version":2,"generated_ids":true,"mutation_id":"goal-correction",
        "run_id":reference("/run_id"),"expected_revision":reference("/graph/revision"),
        "source_node_id":reference("/node_id"),"bind_source_activation":reference("/activation_id"),
        "input":input,"preserve_source_output":true,
        "tasks":[{"task_id":"repair-right","objective":format!("Fix contribution.\ntool-call filesystem.write {fix}"),
            "agent_profile":"build","read_only":false,"tool_allowlist":["filesystem.write"],
            "resources":[{"resource":"contribution:right","access":"write"}],
            "model_selection":{"provider":"bcode.fake-provider","model":"fake-echo"},
            "output":{"type_name":"boolean","schema":{"type":"boolean"}}}],
        "continuation":{"objective":format!("verify\ntool-call shell.run {integrate}"),
            "agent_profile":"build","read_only":false,"tool_allowlist":["shell.run"],
            "resources":[{"resource":"integration","access":"write"}],
            "model_selection":{"provider":"bcode.fake-provider","model":"fake-echo"}},
        "first_edge_id":reference("/graph/next_edge_id"),
        "reconnect":{"edge_id":successor,"node_id":"loop.evaluation","transform":transform},
        "reconciliation":[]
    });
    let broken =
        serde_json::json!({"path":workspace.join("right.sh"),"contents":BROKEN_RIGHT_MODULE});
    group["tasks"][1]["objective"] = serde_json::json!(format!(
        "Produce a contribution.\ntool-call filesystem.write {broken}"
    ));
    // Exercise an actual failed tool result before corrective delegation, rather
    // than masking the verifier's nonzero exit with a successful diagnostic probe.
    let probe = serde_json::json!({
        "command":format!("/bin/sh -c 'cat left.sh right.sh > integrated.sh && ({COMBINED_CHECK}) > rejected.txt; status=$?; cat rejected.txt; exit $status'"),
        "cwd":workspace,"timeout_ms":10000
    });
    let publication =
        serde_json::json!({"$fake_result":{"index":0,"pointer":"/publication_arguments"}});
    group["continuation"]["tool_allowlist"] = serde_json::json!([
        "shell.run",
        "workflow.execution_context",
        "workflow.stage_task_group",
        "workflow.publish_run_graph_edit"
    ]);
    group["continuation"]["objective"] = serde_json::json!(format!(
        "Check contributions and correct failure.\ntool-call-expect-error expected total 27, got 28 :: shell.run {probe}\ntool-call workflow.execution_context {{\"limit\":1,\"compact\":true}}\ntool-call workflow.stage_task_group {correction}\ntool-call workflow.publish_run_graph_edit {publication}"
    ));
}

async fn goal_entry_request(
    session: SessionId,
    root: &Path,
    state: Arc<ServerState>,
) -> PluginWorkflowStartRequest {
    goal_entry_request_with_cap(session, root, state, None).await
}

async fn goal_entry_request_with_cap(
    session: SessionId,
    root: &Path,
    state: Arc<ServerState>,
    execution_cap: Option<u64>,
) -> PluginWorkflowStartRequest {
    let registry = bcode_bundled_plugins::tui_registry("bcode.loop").unwrap();
    let mut surface = registry
        .open(
            "goal.start",
            PluginTuiSurfaceOpenRequest {
                instance_id: "acceptance".into(),
                repo_path: Some(root.into()),
                session_id: Some(session),
                target: None,
                options: serde_json::json!({"collaboration":true,"worker_attempts":30}),
            },
        )
        .await
        .unwrap();
    let host = GoalEntryHost {
        session,
        state,
        workspace: root.to_path_buf(),
        tasks: StdMutex::default(),
        starts: StdMutex::default(),
        execution_cap,
    };
    let key = |key, ctrl| {
        bmux_tui::event::Event::Key(bmux_keyboard::KeyStroke {
            key,
            modifiers: bmux_keyboard::Modifiers {
                ctrl,
                ..Default::default()
            },
        })
    };
    for character in "Implement two collaborating contributions".chars() {
        surface.handle_event(&key(bmux_keyboard::KeyCode::Char(character), false), &host);
    }
    // Disable the optional progress document; this test owns only its temporary workspace.
    surface.handle_event(&key(bmux_keyboard::KeyCode::Char('p'), true), &host);
    surface.handle_event(&key(bmux_keyboard::KeyCode::Enter, true), &host);
    for _ in 0..16 {
        let tasks = std::mem::take(&mut *host.tasks.lock().unwrap());
        for task in tasks {
            task.await;
        }
        surface.poll(&host);
    }
    host.starts
        .lock()
        .unwrap()
        .pop()
        .expect("goal surface started a workflow")
}

fn configure_goal_execution(server: &mut ServerState, root: &Path) {
    // The shared server fixture deliberately truncates tool results at 1,000 characters.
    // Exercise the shipped goal with the production budget, not that truncation fixture.
    server.startup_config.model.tool_output = bcode_config::ToolOutputConfig::default();
    server.daemon_status.instance_id = test_workflow_execution_authority().daemon_instance_id;
    server.plugins = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
        &bcode_plugin::PluginSelection {
            mode: bcode_plugin::PluginSelectionMode::Explicit,
            enabled: BTreeSet::from([
                "bcode.workflow".into(),
                "bcode.fake-provider".into(),
                "bcode.default-agents".into(),
                "bcode.filesystem".into(),
                "bcode.shell".into(),
            ]),
            disabled: BTreeSet::new(),
        },
        &[
            bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../../plugins/filesystem-plugin/bcode-plugin.toml"),
                bcode_filesystem_plugin::static_plugin(),
            ),
            bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../../plugins/shell-plugin/bcode-plugin.toml"),
                bcode_shell_plugin::static_plugin(),
            ),
            bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../../plugins/workflow-plugin/bcode-plugin.toml"),
                bcode_workflow_plugin::static_plugin(),
            ),
            bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../../plugins/fake-provider-plugin/bcode-plugin.toml"),
                bcode_fake_provider_plugin::static_plugin(),
            ),
            bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../../plugins/default-agents-plugin/bcode-plugin.toml"),
                bcode_default_agents_plugin::static_plugin(),
            ),
        ],
    )
    .unwrap();
    server.startup_config.workflows.run_edit_plugins = BTreeSet::from(["bcode.workflow".into()]);
    server.startup_config.workflows.run_publication_plugins =
        BTreeSet::from(["bcode.workflow".into()]);
    server.set_workflow_run_graph_edit_policy(WorkflowRunGraphEditPolicy {
        evaluator: Arc::new(|facts| {
            workflow_operations::authorize_configured_run_graph_edit(
                facts,
                &BTreeSet::from(["bcode.workflow".to_owned()]),
            )
        }),
    });
    server.set_workflow_run_graph_publication_policy(WorkflowRunGraphPublicationPolicy {
        evaluator: Arc::new(|facts| {
            workflow_operations::authorize_configured_run_graph_publication(
                facts,
                &BTreeSet::from(["bcode.workflow".to_owned()]),
                false,
            )
        }),
    });
    server.selected_provider_plugin_id = Some("bcode.fake-provider".into());
    server
        .selected_provider_context
        .settings
        .insert("fake_prompt_tool_directives".into(), "true".into());
    server.selected_provider_context.settings.insert(
        "fake_structured_output_json".into(),
        "loop_tool_evidence:verified".into(),
    );
    register_test_execution_lifetime(server, root);
}

fn contributions_settled(attempts: &[bcode_workflow::AttemptSummary]) -> bool {
    ["left", "right"].iter().all(|id| {
        attempts.iter().any(|attempt| {
            attempt.node_id == *id && attempt.status == expected_worker_terminal_status(id)
        })
    })
}

fn assert_correction_without_replay(attempts: &[bcode_workflow::AttemptSummary]) {
    // Correction reuses successful work instead of replaying settled effects.
    for id in ["left", "right", "repair-right"] {
        let matching: Vec<_> = attempts
            .iter()
            .filter(|attempt| attempt.node_id == id)
            .collect();
        assert_eq!(matching.len(), 1, "unexpected replay of {id}");
        assert!(matching[0].terminal_at_ms.is_some());
    }
    let failed = attempts
        .iter()
        .find(|attempt| attempt.node_id == "right")
        .unwrap();
    let repair = attempts
        .iter()
        .find(|attempt| attempt.node_id == "repair-right")
        .unwrap();
    assert!(repair.prepared_at_ms >= failed.terminal_at_ms.unwrap());
}

fn assert_integrated_files(root: &Path) {
    for (path, contents) in [
        ("left.sh", LEFT_MODULE),
        ("right.sh", RIGHT_MODULE),
        ("verification.txt", "verified"),
        ("user.txt", "uncommitted user work"),
    ] {
        assert_eq!(std::fs::read_to_string(root.join(path)).unwrap(), contents);
    }
    assert_eq!(
        std::fs::read_to_string(root.join("integrated.sh")).unwrap(),
        format!("{LEFT_MODULE}{RIGHT_MODULE}"),
        "integration must contain both current contributions"
    );
    // Independently execute the artifact: a provider's success output and a marker
    // file are not proof that the integrated implementation behaves correctly.
    let verification = std::process::Command::new("/bin/sh")
        .args(["-c", COMBINED_CHECK])
        .current_dir(root)
        .output()
        .expect("run independent combined verification");
    assert!(
        verification.status.success(),
        "combined verification failed: {verification:?}"
    );
}

fn expected_worker_terminal_status(id: &str) -> &'static str {
    if id == "right" { "failed" } else { "succeeded" }
}

fn assert_worker_sessions(state: &ServerState, run_id: &str, session: SessionId) {
    let links = state
        .workflow_store
        .lock()
        .unwrap()
        .execution_session_links_for_run(run_id, 100)
        .unwrap();
    assert!(links.iter().any(|link| {
        link.node_id == "loop.implementation" && link.session_id != session.to_string()
    }));
    let worker_sessions: BTreeSet<_> = links
        .iter()
        .filter(|link| ["left", "right"].contains(&link.node_id.as_str()))
        .map(|link| link.session_id.clone())
        .collect();
    assert_eq!(
        worker_sessions.len(),
        2,
        "workers need distinct execution sessions"
    );
    assert!(!worker_sessions.contains(&session.to_string()));
}

fn assert_named_contributions(
    outputs: &[bcode_workflow_store::ValidatedOutput],
    collected: &bcode_workflow_store::ValidatedOutput,
) {
    let source = outputs
        .iter()
        .find(|output| output.node_id == "loop.implementation")
        .expect("canonical coordinator output");
    assert_eq!(collected.value["source"], source.value);
    assert!(
        !outputs.iter().any(|output| output.node_id == "right"),
        "failed worker must not gain a successful output"
    );
    assert_eq!(
        collected.value["results"],
        serde_json::json!({"left":{"status":"completed","value":true},"right":{"status":"failed"}})
    );
}

fn assert_corrected_contributions(
    root: &Path,
    outputs: &[bcode_workflow_store::ValidatedOutput],
    collected: &bcode_workflow_store::ValidatedOutput,
) {
    assert_integrated_files(root);
    assert_eq!(
        std::fs::read_to_string(root.join("rejected.txt")).unwrap(),
        "expected total 27, got 28\n"
    );
    assert!(
        outputs
            .iter()
            .any(|output| output.node_id == "repair-right")
    );
    assert!(outputs.iter().any(|output| {
        output.value["results"].get("repair-right").is_some()
            && output.value["source"] == collected.value
    }));
}

fn assert_goal_evaluation(
    outputs: &[bcode_workflow_store::ValidatedOutput],
    input: &serde_json::Value,
) {
    let evaluated = outputs
        .iter()
        .find(|output| output.node_id == "loop.evaluation")
        .expect("successful run retains evaluator output");
    assert_eq!(evaluated.value["condition_met"], true);
    for field in [
        "implementation_prompt",
        "stop_condition",
        "max_iterations",
        "judgement_evaluation",
    ] {
        assert_eq!(evaluated.value[field], input[field], "{field}");
    }
}

async fn approve_goal_permissions(state: &ServerState, session: SessionId) {
    // Use the same application boundary as clients; do not bypass permission waits.
    for permission in interaction_operations::list_permissions(state).await {
        assert!(permission.is_addressed_to(session));
        assert!(
            interaction_operations::resolve_permission(
                state,
                &permission.permission_id,
                true,
                false,
            )
            .await
        );
    }
}

#[tokio::test]
async fn real_goal_entry_denied_delegation_prevents_worker_effects() {
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
    let request = goal_entry_request(session.id, root.path(), Arc::clone(&state)).await;
    let run_id = request.run_id.as_ref().unwrap();
    let mut denied = false;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            for permission in interaction_operations::list_permissions(&state).await {
                assert!(permission.is_addressed_to(session.id));
                assert!(
                    interaction_operations::resolve_permission(
                        &state,
                        &permission.permission_id,
                        false,
                        false,
                    )
                    .await
                );
                denied = true;
            }
            let (evaluation, waits, terminal) = {
                let store = state.workflow_store.lock().unwrap();
                (
                    store
                        .validated_outputs(run_id, 100)
                        .unwrap()
                        .into_iter()
                        .find(|output| output.node_id == "loop.evaluation"),
                    store.waiting_activations(run_id, 10).unwrap(),
                    store.canonical_terminal_output(run_id).unwrap(),
                )
            };
            assert!(terminal.is_none(), "denial must not become goal completion");
            if let Some(evaluation) = evaluation.filter(|_| !waits.is_empty()) {
                assert!(denied, "must observe an actual permission decision");
                for field in ["implementation_prompt", "stop_condition", "max_iterations"] {
                    assert_eq!(evaluation.value[field], request.input[field], "{field}");
                }
                assert_eq!(evaluation.value["condition_met"], false);
                assert_eq!(evaluation.value["external_blocker"], "approval_required");
                assert!(waits.iter().any(|wait| wait.node_id == "loop.blocked"));
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "denied delegation did not settle: {error}; events: {:?}",
            state
                .workflow_store
                .lock()
                .unwrap()
                .event_history(run_id, None, 100)
                .unwrap()
        )
    });
    drop(state);
    assert_eq!(
        std::fs::read_to_string(root.path().join("user.txt")).unwrap(),
        "uncommitted user work"
    );
    for artifact in ["left.sh", "right.sh", "integrated.sh", "verification.txt"] {
        assert!(!root.path().join(artifact).exists(), "{artifact}");
    }
}

#[tokio::test]
async fn real_goal_entry_approval_resumes_correction_and_verified_completion() {
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
    let request = goal_entry_request(session.id, root.path(), Arc::clone(&state)).await;
    let run_id = request.run_id.as_ref().unwrap();
    let wait = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            for permission in interaction_operations::list_permissions(&state).await {
                assert!(permission.is_addressed_to(session.id));
                assert!(
                    interaction_operations::resolve_permission(
                        &state,
                        &permission.permission_id,
                        false,
                        false,
                    )
                    .await
                );
            }
            let wait = state
                .workflow_store
                .lock()
                .unwrap()
                .waiting_activations(run_id, 10)
                .unwrap()
                .into_iter()
                .find(|wait| wait.node_id == "loop.blocked");
            if let Some(wait) = wait {
                break wait;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("denied goal must expose an actionable approval wait");
    assert!(!root.path().join("left.sh").exists());
    assert!(!root.path().join("right.sh").exists());
    workflow_operations::resolve_approval(&state, run_id, &wait.node_id, &wait.activation_id, true)
        .await
        .expect("authorized approval must resume the existing goal");
    // Resume approval does not approve tools: each resumed call still passes
    // through the normal permission boundary before integrated verification.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            approve_goal_permissions(&state, session.id).await;
            let terminal = state
                .workflow_store
                .lock()
                .unwrap()
                .canonical_terminal_output(run_id)
                .unwrap();
            if let Some(terminal) = terminal {
                assert_eq!(terminal.value["condition_met"], true);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("approved resumed work must integrate, correct and verify");
    drop(state);
    assert_integrated_files(root.path());
}

#[tokio::test]
async fn real_goal_entry_denied_verification_does_not_delegate_correction() {
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
    state.start_workflow_driver().await;
    let request = goal_entry_request(session.id, root.path(), Arc::clone(&state)).await;
    let run_id = request.run_id.as_ref().unwrap();
    let mut denied_verification = false;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            for permission in interaction_operations::list_permissions(&state).await {
                assert!(permission.is_addressed_to(session.id));
                let allow = permission.tool_name != "shell.run";
                denied_verification |= !allow;
                assert!(
                    interaction_operations::resolve_permission(
                        &state,
                        &permission.permission_id,
                        allow,
                        false,
                    )
                    .await
                );
            }
            let (attempts, outputs) = {
                let store = state.workflow_store.lock().unwrap();
                assert!(store.canonical_terminal_output(run_id).unwrap().is_none());
                (
                    store.attempt_history(run_id, None, 100).unwrap(),
                    store.validated_outputs(run_id, 100).unwrap(),
                )
            };
            assert!(
                !attempts
                    .iter()
                    .any(|attempt| attempt.node_id == "repair-right"),
                "permission denial is not a failed combined check"
            );
            if let Some(evaluation) = outputs
                .iter()
                .find(|output| output.node_id == "loop.evaluation")
            {
                assert!(denied_verification);
                assert_eq!(evaluation.value["condition_met"], false);
                assert_eq!(evaluation.value["external_blocker"], "approval_required");
                let waits = state
                    .workflow_store
                    .lock()
                    .unwrap()
                    .waiting_activations(run_id, 10)
                    .unwrap();
                if waits.iter().any(|wait| wait.node_id == "loop.blocked") {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "denied verifier: {error}; attempts: {:?}",
            state
                .workflow_store
                .lock()
                .unwrap()
                .attempt_history(run_id, None, 100)
                .unwrap()
        )
    });
    assert_eq!(
        std::fs::read_to_string(root.path().join("left.sh")).unwrap(),
        LEFT_MODULE
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("right.sh")).unwrap(),
        BROKEN_RIGHT_MODULE
    );
    for path in ["integrated.sh", "rejected.txt", "verification.txt"] {
        assert!(
            !root.path().join(path).exists(),
            "denied verifier wrote {path}"
        );
    }
    drop(state);
}

fn goal_ipc_client(
    root: &Path,
    state: &Arc<ServerState>,
) -> (bcode_client::BcodeClient, tokio::task::JoinHandle<()>) {
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(root.join("allowance.sock"));
    let listener = LocalIpcListener::bind(&endpoint).unwrap();
    let server_state = Arc::clone(state);
    let server = tokio::spawn(async move {
        loop {
            let stream = listener.accept().await.unwrap();
            let state = Arc::clone(&server_state);
            tokio::spawn(async move { handle_client(stream, state).await });
        }
    });
    (bcode_client::BcodeClient::new(endpoint), server)
}

// Isolate the default-endpoint environment from concurrently running server tests.
async fn invoke_goal_allowance_command(root: &Path, session: SessionId) {
    let output = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tests::goal_entry::goal_allowance_command_subprocess",
            "--nocapture",
        ])
        .env("BCODE_SOCKET", root.join("allowance.sock"))
        .env_remove(bcode_ipc::BCODE_IPC_ENDPOINT_ENV)
        .env_remove(bcode_ipc::BCODE_IPC_ENDPOINT_NAMESPACE_ENV)
        .env_remove("BCODE_DAEMON_LOG")
        .env("BCODE_GOAL_COMMAND_SESSION", session.to_string())
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "goal command failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
}

#[tokio::test]
async fn goal_allowance_command_subprocess() {
    let Ok(session) = std::env::var("BCODE_GOAL_COMMAND_SESSION") else {
        return;
    };
    let plugins = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
        &bcode_plugin::PluginSelection {
            mode: bcode_plugin::PluginSelectionMode::Explicit,
            enabled: BTreeSet::from(["bcode.loop".into()]),
            disabled: BTreeSet::new(),
        },
        &[bcode_bundled_plugins::static_loop_plugin()],
    )
    .unwrap();
    let response: bcode_command::InvokeCommandResponse = plugins
        .invoke_service_json(
            "bcode.loop",
            bcode_command::COMMAND_INTERFACE_ID,
            bcode_command::OP_INVOKE_COMMAND,
            &bcode_command::InvokeCommandRequest {
                command_id: "goal.continue".into(),
                args: BTreeMap::from([("arguments".into(), "--worker-attempts 99".into())]),
                context: Some(bcode_command::CommandInvocationContext {
                    session_id: Some(session.parse().unwrap()),
                    working_directory: std::env::current_dir().unwrap(),
                }),
            },
        )
        .await
        .unwrap();
    assert!(response.success, "{:?}", response.message);
    assert!(
        response
            .message
            .unwrap()
            .contains("Granted 99 execution attempts")
    );
}

#[tokio::test]
async fn exhausted_goal_resumes_after_idempotent_ipc_allowance_grant() {
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
    let request =
        goal_entry_request_with_cap(session.id, root.path(), Arc::clone(&state), Some(1)).await;
    let run_id = request.run_id.unwrap();
    let (client, server) = goal_ipc_client(root.path(), &state);
    let observation = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            approve_goal_permissions(&state, session.id).await;
            let inspection = client
                .inspect_workflow_run(run_id.clone(), 10)
                .await
                .unwrap();
            let allowance = inspection.execution_allowance.unwrap();
            if allowance.exhausted() == Some(true) {
                break allowance;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(observation.run_cap, 1);
    assert!(!root.path().join("verification.txt").exists());
    let action = bcode_workflow::WorkflowRunControlAction::IncreaseExecutionAllowance {
        expected_cap: observation.run_cap,
        target_cap: 100,
    };
    invoke_goal_allowance_command(root.path(), session.id).await;
    client
        .control_workflow_run(run_id.clone(), action)
        .await
        .unwrap();
    assert!(
        client
            .control_workflow_run(
                run_id.clone(),
                bcode_workflow::WorkflowRunControlAction::IncreaseExecutionAllowance {
                    expected_cap: observation.run_cap,
                    target_cap: 101,
                }
            )
            .await
            .is_err()
    );
    let inspection = client
        .inspect_workflow_run(run_id.clone(), 10)
        .await
        .unwrap();
    assert_eq!(inspection.execution_allowance.unwrap().run_cap, 100);
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            approve_goal_permissions(&state, session.id).await;
            let terminal = state
                .workflow_store
                .lock()
                .unwrap()
                .canonical_terminal_output(&run_id)
                .unwrap();
            if let Some(terminal) = terminal {
                assert_eq!(terminal.value["condition_met"], true);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let attempts = state
        .workflow_store
        .lock()
        .unwrap()
        .attempt_history(&run_id, None, 100)
        .unwrap();
    assert_correction_without_replay(&attempts);
    drop(state);
    assert_integrated_files(root.path());
    server.abort();
}

#[tokio::test]
async fn real_goal_entry_publishes_and_executes_two_workers() {
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
    let request = goal_entry_request(session.id, root.path(), Arc::clone(&state)).await;
    assert!(
        request.input["implementation_prompt"]
            .as_str()
            .unwrap()
            .contains("Implement two collaborating contributions")
    );
    let run_id = request.run_id.unwrap();
    // Admission is not execution: observe the real production driver settlement.
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            approve_goal_permissions(&state, session.id).await;
            let attempts = state
                .workflow_store
                .lock()
                .unwrap()
                .attempt_history(&run_id, None, 100)
                .unwrap();
            if let Some(coordinator) = attempts
                .iter()
                .find(|attempt| {
                    attempt.node_id == "loop.implementation" && attempt.terminal_at_ms.is_some()
                })
                .filter(|_| contributions_settled(&attempts))
            {
                assert_eq!(coordinator.status, "succeeded", "{attempts:?}");
                let outputs = state
                    .workflow_store
                    .lock()
                    .unwrap()
                    .validated_outputs(&run_id, 100)
                    .unwrap();
                let Some(collected) = outputs.iter().find(|output| {
                    output.value["results"].get("left").is_some()
                        && output.value.get("source").is_some()
                }) else {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                };
                assert_named_contributions(&outputs, collected);
                assert!(attempts.iter().any(|attempt| {
                    attempt.node_id == collected.node_id && attempt.status == "succeeded"
                }));
                assert_worker_sessions(&state, &run_id, session.id);
                let terminal = state
                    .workflow_store
                    .lock()
                    .unwrap()
                    .canonical_terminal_output(&run_id)
                    .unwrap();
                let Some(terminal) = terminal else {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                };
                assert_corrected_contributions(root.path(), &outputs, collected);
                assert_correction_without_replay(&attempts);
                assert_goal_evaluation(&outputs, &request.input);
                assert_eq!(terminal.value["condition_met"], true);
                break;
            }
            assert!(
                attempts
                    .iter()
                    .all(|attempt| attempt.status != "failed" || attempt.node_id == "right"),
                "goal execution failed before coordinator settlement: {attempts:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|error| {
        panic!(
            "goal execution timed out: {error}; attempts: {:?}",
            state
                .workflow_store
                .lock()
                .unwrap()
                .event_history(&run_id, None, 100)
                .unwrap()
        )
    });
    drop(state);
}
