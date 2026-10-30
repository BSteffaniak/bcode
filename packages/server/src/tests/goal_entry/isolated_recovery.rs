//! Scripted model choices through goal entry, registered worker execution and real Git recovery.
use super::*;
use bcode_session_models::SessionEventKind as Kind;
use futures::FutureExt as _;

// Diagnostics only: neither history nor observations authorize recovery.
// Keep the bounded event read and stage-pair interpretation together for this diagnostic.
#[allow(clippy::too_many_lines)]
async fn log_recovery_progress(
    state: &ServerState,
    run_id: &str,
    parent: SessionId,
    started: Instant,
    previous_attempts: &mut Vec<bcode_workflow::AttemptSummary>,
    sequences: &mut BTreeMap<SessionId, (u64, BTreeMap<String, u64>)>,
) {
    let read_started = Instant::now();
    let (attempts, links) = {
        let store = state.workflow_store.lock().unwrap();
        (
            store.attempt_history(run_id, None, 32).unwrap(),
            store.execution_session_links_for_run(run_id, 8).unwrap(),
        )
    };
    for attempt in &attempts {
        if !previous_attempts.contains(attempt) {
            eprintln!(
                "isolated recovery +{:?} attempt={attempt:?} prepare_to_admit_ms={:?} admit_to_terminal_ms={:?}",
                started.elapsed(),
                attempt
                    .admitted_at_ms
                    .map(|end| end.saturating_sub(attempt.prepared_at_ms)),
                attempt
                    .terminal_at_ms
                    .zip(attempt.admitted_at_ms)
                    .map(|(end, begin)| end.saturating_sub(begin)),
            );
        }
    }
    *previous_attempts = attempts;
    eprintln!(
        "isolated recovery +{:?} attempt/link read took {:?}",
        started.elapsed(),
        read_started.elapsed()
    );
    let mut sessions = BTreeSet::from([parent]);
    sessions.extend(
        links
            .iter()
            .map(|link| link.session_id.parse::<SessionId>().unwrap()),
    );
    for session_id in sessions {
        let read_started = Instant::now();
        eprintln!(
            "isolated recovery +{:?} history begin session={session_id}",
            started.elapsed()
        );
        let page = state
            .sessions
            .session_history_page(
                session_id,
                bcode_session_models::SessionHistoryQuery {
                    cursor: None,
                    limit: 16,
                    direction: bcode_session_models::SessionHistoryDirection::Backward,
                },
            )
            .await;
        eprintln!(
            "isolated recovery +{:?} history end session={session_id} took {:?}",
            started.elapsed(),
            read_started.elapsed()
        );
        match page {
            Ok(page) => {
                let (previous, stages) = sequences.entry(session_id).or_default();
                let mut events = page.events;
                events.sort_by_key(|event| event.sequence);
                if let Some(first) = events.first()
                    && first.sequence > previous.saturating_add(1)
                {
                    eprintln!(
                        "isolated recovery session={session_id} bounded tail gap after={} resumes_at={}; missing stage durations are unknown",
                        *previous, first.sequence
                    );
                    stages.clear();
                }
                for event in events {
                    if event.sequence <= *previous {
                        continue;
                    }
                    // No arguments, tool results, model text, or provider errors in diagnostics.
                    eprintln!(
                        "isolated recovery +{:?} committed session={session_id} sequence={} timestamp_ms={} kind={}",
                        started.elapsed(),
                        event.sequence,
                        event.timestamp_ms,
                        bcode_session_models::persisted_session_event_kind_name(&event.kind),
                    );
                    let (key, begin) = match &event.kind {
                        Kind::ModelTurnStarted { turn_id } => {
                            (Some(format!("model:{turn_id}")), true)
                        }
                        Kind::ModelTurnFinished { turn_id, .. } => {
                            (Some(format!("model:{turn_id}")), false)
                        }
                        Kind::RuntimeWorkStarted { work_id, .. } => {
                            (Some(format!("runtime:{work_id}")), true)
                        }
                        Kind::RuntimeWorkFinished { work_id, .. } => {
                            (Some(format!("runtime:{work_id}")), false)
                        }
                        Kind::PermissionRequested { permission_id, .. } => {
                            (Some(format!("permission:{permission_id}")), true)
                        }
                        Kind::PermissionResolved { permission_id, .. } => {
                            (Some(format!("permission:{permission_id}")), false)
                        }
                        Kind::ToolCallRequested { tool_call_id, .. } => {
                            (Some(format!("tool:{tool_call_id}")), true)
                        }
                        Kind::ToolInvocationResultRecorded { record } => {
                            (Some(format!("tool:{}", record.invocation_id)), false)
                        }
                        _ => (None, false),
                    };
                    if let Some(key) = key {
                        if begin {
                            // Incomplete pairs (including dropped tail events) remain explicit.
                            if stages.len() < 32 {
                                stages.insert(key.clone(), event.timestamp_ms);
                            }
                            eprintln!(
                                "isolated recovery stage begin {key} timestamp_ms={}",
                                event.timestamp_ms
                            );
                        } else {
                            let duration = stages
                                .remove(&key)
                                .map(|begin| event.timestamp_ms.saturating_sub(begin));
                            eprintln!(
                                "isolated recovery stage end {key} timestamp_ms={} committed_duration_ms={duration:?}",
                                event.timestamp_ms
                            );
                        }
                    }
                    *previous = event.sequence;
                }
            }
            Err(_) => eprintln!("isolated recovery history unavailable session={session_id}"),
        }
    }
}

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
    let reference = |index: usize, pointer: &str| serde_json::json!({"$fake_result":{"index":index,"pointer":pointer}});
    let base = git(root, &["rev-parse", "HEAD"]);
    let task = |id: &str, creation_index: usize| {
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
        let revision = shell(&workspace, "printf '%s' \"$(git rev-parse HEAD)\"");
        let result = serde_json::json!({"summary":format!("Committed {id}"),"evidence":["Commit command succeeded"],"blockers":[],"contributions":[{
            "workspace":workspace,"base_revision":base,"source_directory":root,"source_had_local_changes":true,
            "produced_revisions":[{"$fake_result":{"index":0,"pointer":"/output"}}],"artifacts":[workspace.join("integrated.sh")],"validation":[],
            "remaining_work":["Resolve integration conflict and verify combined result"],"retention":"retained"
        }]});
        serde_json::json!({"task_id":id,"objective":format!("Contribute in the assigned checkout.\ntool-call filesystem.write {write}\ntool-call shell.run {commit}\ntool-call shell.run {revision}\nstructured-result {result}"),
            "output":{"type_name":"test.git_contribution.v1","schema":{"type":"object","additionalProperties":false,"required":["summary","evidence","blockers","contributions"],"properties":{"summary":{"type":"string"},"evidence":{"type":"array","items":{"type":"string"}},"blockers":{"type":"array","items":{"type":"string"}},"contributions":{"type":"array","items":{"type":"object"}}}}},
            "agent_profile":"build","read_only":false,"worktree_directory":reference(creation_index, "/path"),
            "acceptance_criteria":["Creation-time base commit (verify before editing):",reference(creation_index, "/provenance/base_commit"),"Canonical source checkout (do not modify):",reference(creation_index, "/provenance/source_directory"),"Creation provenance does not include uncommitted inputs; inspect source dirtiness and report required missing inputs rather than copying or overwriting user work."],
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
        "mutation_id":"isolated-goal-workers",
        "run_id":reference(0, "/run_id"),"expected_revision":reference(0, "/graph/revision"),
        "bind_source_activation":reference(0, "/activation_id"),
        // Results are newest first: execution context, integration, right, left.
        "tasks":[task("left", 3),task("right", 2)],
        "continuation":{"objective":format!("Integrate both retained contributions. Inspect the actual conflict before resolving it; never overwrite the source checkout.\ntool-call-expect-error CONFLICT :: shell.run {probe}\ntool-call shell.run {inspect}\ntool-call filesystem.write {resolution}\ntool-call shell.run {verify}"),
            "agent_profile":"build","read_only":false,"worktree_directory":reference(1, "/path"),"tool_allowlist":["shell.run","filesystem.write"],
            "acceptance_criteria":["Integration creation-time base commit (verify before applying contributions):",reference(1, "/provenance/base_commit"),"Canonical source checkout (retain all user edits):",reference(1, "/provenance/source_directory"),"Retain conflicts and successful contributions; creation provenance is an observation, not authorization to overwrite the source."],
            "resources":[{"resource":"checkout:integration","access":"write"}],"model_selection":{"provider":"bcode.fake-provider","model":"fake-echo"}},
        "reconciliation":[]
    });
    let publication =
        serde_json::json!({"$fake_result":{"index":0,"pointer":"/publication_arguments"}});
    let source = request
        .definition
        .nodes
        .get_mut("loop.implementation")
        .unwrap();
    let instructions = source.configuration["system_prompt"].as_str().unwrap();
    let preparation = ["left", "right", "integration"]
        .map(|id| {
            let create = serde_json::json!({
                "name":id,"cwd":root,"path":trees.join(id),
                "new_branch":format!("swarm-{id}"),"base_ref":"head","no_setup":true
            });
            format!("tool-call worktree.create {create}")
        })
        .join("\n");
    source.configuration["system_prompt"] = serde_json::json!(format!(
        "{instructions}\n{preparation}\ntool-call workflow.execution_context {{\"compact\":true,\"limit\":1}}\ntool-call workflow.stage_delegation {group}\ntool-call workflow.publish_run_graph_edit {publication}"
    ));
    install_evaluation_inspection(request, &integration);
}

fn install_evaluation_inspection(request: &mut PluginWorkflowStartRequest, integration: &Path) {
    let inspect = |index, node| {
        serde_json::json!({"$fake_result":{
            "index":index,"pointer":"/outputs","where":{"node_id":node},
            "select":"/inspection_arguments","latest_by":"created_at_ms"
        }})
    };
    let left = inspect(0, "left");
    let right = inspect(1, "right");
    // Evaluation remains read-only: inspect the revision recorded after the
    // continuation's successful combined check, alongside the actual artifact.
    let revision =
        serde_json::json!({"path":integration.join("revision.txt"),"offset":1,"limit":1});
    let read = serde_json::json!({"path":integration.join("integrated.sh"),"offset":1,"limit":100});
    let evaluation = request.definition.nodes.get_mut("loop.evaluation").unwrap();
    let instructions = evaluation.configuration["system_prompt"].as_str().unwrap();
    evaluation.configuration["system_prompt"] = serde_json::json!(format!(
        "{instructions}\ntool-call workflow.execution_context {{\"outputs_only\":true,\"limit\":3,\"$fake_json_pages\":{{\"items\":\"/outputs\",\"next\":\"/next_page_arguments\"}}}}\ntool-call workflow.execution_context {left}\ntool-call workflow.execution_context {right}\ntool-call filesystem.read {revision}\ntool-call filesystem.read {read}"
    ));
}

fn prepare_source(root: &Path) -> String {
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
    std::fs::write(root.join("staged.txt"), "staged user work\n").unwrap();
    git(root, &["add", "staged.txt"]);
    std::fs::write(root.join("untracked.txt"), "untracked user work\n").unwrap();
    std::fs::write(
        root.join("integrated.sh"),
        "# uncommitted user implementation\n",
    )
    .unwrap();
    base
}

#[tokio::test]
// The real Git scenario and its owned teardown form one sequential acceptance test.
#[allow(clippy::too_many_lines)]
async fn goal_workers_recover_real_git_conflict_in_isolated_integration_checkout() {
    let _execution = GOAL_ENTRY_EXECUTION.lock().await;
    let root = tempfile::tempdir().unwrap();
    let trees = tempfile::tempdir().unwrap();
    let base = prepare_source(root.path());
    let status_args = [
        "status",
        "--porcelain",
        "--",
        "integrated.sh",
        "staged.txt",
        "untracked.txt",
    ];
    let source_status = git(root.path(), &status_args);
    let source_index = git(root.path(), &["diff", "--cached"]);
    assert_eq!(std::fs::read_dir(trees.path()).unwrap().count(), 0);
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

    let started = Instant::now();
    let outcome = std::panic::AssertUnwindSafe(Box::pin(async {
    let request = goal_entry_request_with_workspace(
        session.id,
        root.path(),
        Arc::clone(&state),
        None,
        Some(trees.path().into()),
    )
    .await;
    let run_id = request.run_id.unwrap();
    let mut previous_attempts = Vec::new();
    let mut sequences = BTreeMap::new();
    let mut last_diagnostic = None;
    tokio::time::timeout(Duration::from_mins(1), async {
        loop {
            let diagnostic = last_diagnostic.is_none_or(|last: Instant| last.elapsed() >= Duration::from_secs(1));
            let permission_started = Instant::now();
            if diagnostic {
                eprintln!("isolated recovery +{:?} permission poll begin", started.elapsed());
            }
            approve_goal_permissions(&state, session.id).await;
            if diagnostic {
                eprintln!("isolated recovery +{:?} permission poll end took {:?}; validated_outputs begin", started.elapsed(), permission_started.elapsed());
            }
            let output_started = Instant::now();
            let outputs = state
                .workflow_store
                .lock()
                .unwrap()
                .validated_outputs(&run_id, 100)
                .unwrap();
            if diagnostic {
                eprintln!("isolated recovery +{:?} validated_outputs end took {:?}", started.elapsed(), output_started.elapsed());
                log_recovery_progress(&state, &run_id, session.id, started, &mut previous_attempts, &mut sequences).await;
                last_diagnostic = Some(Instant::now());
            }
            if let Some(evaluation) = outputs
                .iter()
                .find(|output| output.node_id == "loop.evaluation")
            {
                assert_eq!(evaluation.value["condition_met"], true, "{outputs:?}");
                for id in ["left", "right"] {
                    let output = outputs.iter().find(|output| output.node_id == id).unwrap();
                    assert_eq!(output.value["contributions"][0]["base_revision"], base);
                    let worker_revision = git(&trees.path().join(id), &["rev-parse", "HEAD"]);
                    assert_ne!(worker_revision, base);
                    let produced = output.value["contributions"][0]["produced_revisions"]
                        .as_array()
                        .unwrap();
                    assert_eq!(produced.len(), 1);
                    // Shell output is terminal text and can retain its final row break.
                    // Compare the exact revision after removing only that presentation suffix.
                    assert_eq!(
                        produced[0].as_str().unwrap().trim_end_matches(['\r', '\n']),
                        worker_revision
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
    .unwrap_or_else(|error| {
        let (attempts, outputs, terminal) = {
            let store = state.workflow_store.lock().unwrap();
            (store.attempt_history(&run_id, None, 100).unwrap(), store.validated_outputs(&run_id, 100).unwrap(), store.canonical_terminal_output(&run_id).unwrap())
        };
        let output_facts = outputs.iter().map(|output| (&output.node_id, output.value.get("condition_met"))).collect::<Vec<_>>();
        let failures = state.workflow_store.lock().unwrap().failure_history(&run_id, 10).unwrap();
        let failure_facts = failures.iter().map(|event| {
            let error = event.payload.get("error").or_else(|| event.payload.get("message")).map_or_else(|| "unavailable".into(), serde_json::Value::to_string);
            (&event.event_type, event.payload.get("node_id"), error[..error.floor_char_boundary(error.len().min(1024))].to_owned())
        }).collect::<Vec<_>>();
        panic!("isolated goal recovery must reach evaluation: {error}; terminal_present: {}; output_facts: {output_facts:?}; failures: {failure_facts:?}; attempts: {attempts:?}", terminal.is_some());
    });
    }))
    .catch_unwind()
    .await;
    let cleanup_started = Instant::now();
    eprintln!(
        "isolated recovery +{:?} owned shutdown begin",
        started.elapsed()
    );
    let cleanup = Box::pin(shutdown_constructed_server(Arc::clone(&state), Ok(()))).await;
    eprintln!(
        "isolated recovery +{:?} owned shutdown end took {:?}; success={}",
        started.elapsed(),
        cleanup_started.elapsed(),
        cleanup.is_ok()
    );
    drop(state);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    cleanup.expect("isolated recovery owned runtime shutdown");
    assert_eq!(git(root.path(), &["rev-parse", "HEAD"]), base);
    assert_eq!(git(root.path(), &status_args), source_status);
    assert_eq!(git(root.path(), &["diff", "--cached"]), source_index);
    assert_retained_checkouts(root.path(), trees.path(), &base);
}

fn assert_retained_checkouts(root: &Path, trees: &Path, base: &str) {
    for (name, contents) in [
        ("staged.txt", "staged user work\n"),
        ("untracked.txt", "untracked user work\n"),
    ] {
        assert_eq!(std::fs::read_to_string(root.join(name)).unwrap(), contents);
        assert!(!trees.join("integration").join(name).exists());
    }
    assert_eq!(
        std::fs::read_to_string(root.join("integrated.sh")).unwrap(),
        "# uncommitted user implementation\n"
    );
    let integration = trees.join("integration");
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
        assert_ne!(git(&trees.join(id), &["rev-parse", "HEAD"]), base);
        assert!(git(&trees.join(id), &["status", "--porcelain"]).is_empty());
    }
}
