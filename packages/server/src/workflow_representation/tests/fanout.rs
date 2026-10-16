use super::*;

// Each fixture independently proves provider overlap for up to 100 members.
// Do not make two host-saturation fixtures compete for the same process resources.
static FANOUT_FIXTURE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn fanout_definition(
    leaf: bcode_workflow::NodeDefinition,
    count: u32,
) -> bcode_workflow::WorkflowDefinition {
    let mut node = leaf.clone();
    node.id = "fanout".into();
    node.kind = bcode_workflow::NodeKind::FanOut;
    node.input = bcode_workflow::ValueSchema {
        type_name: "fanout.input".into(),
        schema: serde_json::json!({"type":"array","items":leaf.input.schema}),
    };
    node.output = bcode_workflow::ValueSchema {
        type_name: format!("workflow.fan-out-result/v1<{}>", leaf.output.type_name),
        schema: serde_json::json!({"type":"object","additionalProperties":false,"required":["version","members"],"properties":{"version":{"type":"integer","const":1},"members":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["index","value"],"properties":{"index":{"type":"integer","minimum":0},"value":leaf.output.schema}}}}}),
    };
    node.configuration = serde_json::to_value(bcode_workflow::WorkflowFanOutConfiguration {
        version: bcode_workflow::WORKFLOW_FAN_OUT_CONFIGURATION_VERSION,
        member_node: Box::new(leaf),
        max_members: count,
        max_concurrency: count,
        failure_policy: bcode_workflow::ParallelFailurePolicy::WaitAll,
    })
    .expect("config");
    bcode_workflow::WorkflowDefinition {
        schema_version: bcode_workflow::WORKFLOW_DEFINITION_SCHEMA_VERSION,
        name: "fanout".into(),
        input: node.input.clone(),
        output: node.output.clone(),
        nodes: std::collections::BTreeMap::from([("fanout".into(), node)]),
        entries: vec!["fanout".into()],
        exits: vec!["fanout".into()],
        edges: vec![],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fanout_real_provider_members_overlap_at_32_and_100() {
    let _fixture = FANOUT_FIXTURE.lock().await;
    for count in [32, 100] {
        Box::pin(fanout_overlap(count)).await;
    }
}

async fn fanout_overlap(count: u32) {
    let (_root, _sessions, state, _) = typed_provider_fixture_with_delegation(true).await;
    let mut cleanup = FixtureCleanup::new(&state);
    let _release =
        ReleaseGate(state.selected_provider_context.settings["delegation_fixture_gate"].clone());
    let leaf = parallel_provider_definition().nodes["left"].clone();
    let definition = fanout_definition(leaf, count);
    let db = open(&crate::workflow_store_path(&state), true).expect("db");
    let authority = owner::local_authority(&state, "new")
        .await
        .expect("owner")
        .expect("authority");
    let mut run = representation::hierarchy::parent_context(&db, &authority.authority, "new")
        .await
        .expect("context")
        .admission;
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("fanout", &definition)
            .expect("identity");
    run.run_id = "fanout".into();
    run.definition_id = identity.definition_id;
    run.definition_version = identity.definition_version;
    run.input = Some(serde_json::json!(vec![1_u32; count as usize]));
    run.limits.node_execution_cap = u64::from(count) + 1;
    representation::admit(&db, &run, &definition)
        .await
        .expect("fixture typed admission");
    let started = std::time::Instant::now();
    let drive_state = std::sync::Arc::clone(&state);
    let mut initial_drive = OwnedFixtureTask::new(
        &state,
        tokio::spawn(async move {
            driver::drive(&drive_state, "fanout")
                .await
                .expect("fixture drive");
        }),
    );
    let initial = tokio::time::timeout(
        std::time::Duration::from_secs(u64::from(count) * 2),
        initial_drive.handle_mut(),
    )
    .await;
    if initial.is_err() {
        diagnose_fanout_before_failure_cleanup(&state, count, started).await;
        let pending = representation::attempts::pending(&db, "fanout", None, 100).await;
        bcode_fake_provider_plugin::release_fake_event_script_gate(
            &state.selected_provider_context.settings["delegation_fixture_gate"],
        );
        let diagnostic = finish_overdue_fanout_drive(&state, &mut initial_drive).await;
        panic!(
            "initial drive overdue; pending_count={:?}; {diagnostic}",
            pending.map(|rows| rows.len()),
        );
    }
    initial
        .expect("bounded drive")
        .expect("materialize and dispatch");
    initial_drive.disarm_joined();
    drop(initial_drive);
    let pending = representation::attempts::pending(&db, "fanout", None, 100)
        .await
        .expect("overlap");
    assert_eq!(pending.len(), count as usize);
    assert!(
        pending.iter().all(|attempt| attempt.receipt.is_some()),
        "all provider owners admitted before releasing any"
    );
    bcode_fake_provider_plugin::release_fake_event_script_gate(
        &state.selected_provider_context.settings["delegation_fixture_gate"],
    );
    let completion =
        tokio::time::timeout(std::time::Duration::from_secs(u64::from(count)), async {
            loop {
                driver::drive(&state, "fanout").await.expect("drive");
                if status(&state, "fanout")
                    .await
                    .expect("status")
                    .expect("run")
                    .status
                    == bcode_workflow::RunStatus::Completed
                    && state.runtime_work.active_count().await == 0
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
    assert!(
        completion.is_ok(),
        "members complete count={count} remaining={:?}",
        representation::attempts::pending(&db, "fanout", None, 100)
            .await
            .map(|v| v.len())
    );
    cleanup.finish_run("fanout").await;
    drop(cleanup);
}

async fn diagnose_fanout_before_failure_cleanup(
    state: &ServerState,
    count: u32,
    started: std::time::Instant,
) {
    // This observation precedes gate release and cleanup's shutdown request.
    eprintln!(
        "fanout-initial-deadline count={count} elapsed={:?} shutdown_requested={} before_failure_cleanup=true",
        started.elapsed(),
        state
            .shutdown_requested
            .load(std::sync::atomic::Ordering::SeqCst),
    );
    let observed = tokio::time::timeout(std::time::Duration::from_millis(100), async {
        let run = status(state, "fanout").await;
        let canonical = match run {
            Ok(Some(run)) => format!("{:?}", run.status),
            Ok(None) => "missing".into(),
            Err(_) => "unavailable".into(),
        };
        eprintln!("fanout-initial-deadline count={count} elapsed={:?} canonical_status={canonical} active_registrations={} before_failure_cleanup=true", started.elapsed(), state.runtime_work.active_count().await);
    }).await;
    if observed.is_err() {
        eprintln!(
            "fanout-initial-deadline count={count} elapsed={:?} observation=timeout before_failure_cleanup=true",
            started.elapsed()
        );
    }
}

async fn finish_overdue_fanout_drive(
    state: &std::sync::Arc<ServerState>,
    task: &mut OwnedFixtureTask,
) -> String {
    let joined = tokio::time::timeout(std::time::Duration::from_secs(2), task.handle_mut()).await;
    if let Ok(result) = joined {
        result.expect("overdue fixture drive task");
        task.disarm_joined();
    }
    let diagnostic = bounded_run_diagnostic(state, "fanout").await;
    shutdown_fixture_driver(state, true).await;
    diagnostic
}

pub(super) struct ReleaseGate(pub(super) String);
impl Drop for ReleaseGate {
    fn drop(&mut self) {
        bcode_fake_provider_plugin::release_fake_event_script_gate(&self.0);
        bcode_fake_provider_plugin::forget_fake_event_script_gate_ready(&self.0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn normal_start_fanout_respects_cap8_and_uncapped32() {
    let _fixture = FANOUT_FIXTURE.lock().await;
    for cap in [Some(8), None] {
        let (_root, _sessions, state, _) = typed_provider_fixture_with_delegation(true).await;
        let mut cleanup = FixtureCleanup::new(&state);
        let gate = ReleaseGate(
            state.selected_provider_context.settings["delegation_fixture_gate"].clone(),
        );
        let definition =
            fanout_definition(parallel_provider_definition().nodes["left"].clone(), 32);
        let expected_admissions = cap.unwrap_or(32);
        eprintln!(
            "normal fanout arm cap={cap:?}, expected_initial_admissions={expected_admissions}"
        );
        start_public(
            &state,
            "normal-fanout",
            definition,
            serde_json::json!(vec![1; 32]),
            cap,
        )
        .await;
        let db = open(&crate::workflow_store_path(&state), false).expect("db");
        let attempts = representation::attempts::pending(&db, "normal-fanout", None, 100)
            .await
            .expect("attempts");
        assert_eq!(attempts.len(), cap.unwrap_or(32) as usize);
        assert!(attempts.iter().all(|a| a.receipt.is_some()));
        crate::drive_workflow_run(&state, "normal-fanout")
            .await
            .expect("normal continuation while providers are gated");
        let rescanned = representation::attempts::pending(&db, "normal-fanout", None, 100)
            .await
            .expect("rescanned attempts");
        assert_eq!(rescanned.len(), attempts.len());
        assert_eq!(
            rescanned
                .iter()
                .map(|a| a.prepared.dispatch_identity())
                .collect::<Vec<_>>(),
            attempts
                .iter()
                .map(|a| a.prepared.dispatch_identity())
                .collect::<Vec<_>>()
        );
        state.start_workflow_driver().await;
        drop(gate);
        // Up to 24 members still need session setup after the cap8 gate releases.
        // Budget by the configured 32-member workload, not a production retry limit.
        let result = wait_public_with_budget(
            &state,
            "normal-fanout",
            std::time::Duration::from_secs(32 * 2),
        )
        .await;
        let diagnostic = if result.is_err() {
            bounded_run_diagnostic(&state, "normal-fanout").await
        } else {
            String::new()
        };
        assert!(
            result.is_ok(),
            "normal scheduler completion cap={cap:?}: {diagnostic}"
        );
        let outputs = super::super::outputs::run_outputs(&state, "normal-fanout", 100)
            .await
            .expect("outputs");
        let result = outputs
            .iter()
            .find(|output| output.node_id == "fanout")
            .expect("controller output");
        let members = result.value["members"].as_array().expect("members");
        assert_eq!(members.len(), 32);
        for (index, member) in members.iter().enumerate() {
            assert_eq!(member["index"], serde_json::json!(index));
            assert_eq!(member["value"], serde_json::json!(9));
        }
        cleanup.finish_run("normal-fanout").await;
        drop(cleanup);
    }
}

#[tokio::test]
#[ignore = "fixed-artifact causal measurement; no latency threshold"]
async fn probe_real_repo_cap8() {
    Box::pin(probe_workspace("real-cap8", false, Some(8))).await;
}

#[tokio::test]
#[ignore = "fixed-artifact causal measurement; no latency threshold"]
async fn probe_minimal_repo_cap8() {
    Box::pin(probe_workspace("minimal-cap8", true, Some(8))).await;
}

#[tokio::test]
#[ignore = "fixed-artifact causal measurement; no latency threshold"]
async fn probe_real_repo_uncapped32() {
    Box::pin(probe_workspace("real-32", false, None)).await;
}

#[tokio::test]
#[ignore = "fixed-artifact causal measurement; no latency threshold"]
async fn probe_minimal_repo_uncapped32() {
    Box::pin(probe_workspace("minimal-32", true, None)).await;
}

async fn probe_workspace(experiment_id: &str, minimal: bool, cap: Option<u32>) {
    let workspace_root = tempfile::tempdir().expect("minimal workspace");
    let workspace = if minimal {
        workspace_root
            .path()
            .canonicalize()
            .expect("minimal workspace")
    } else {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("checkout workspace")
    };
    let (_root, _sessions, state, _) = typed_provider_fixture_with_delegation(true).await;
    let gate =
        ReleaseGate(state.selected_provider_context.settings["delegation_fixture_gate"].clone());
    bcode_fake_provider_plugin::forget_fake_event_script_gate_ready(&gate.0);
    let experiment =
        super::super::fanout_stages::Experiment::begin(experiment_id, experiment_id, &workspace);
    let definition = fanout_definition(parallel_provider_definition().nodes["left"].clone(), 32);
    let started = std::time::Instant::now();
    let admission = start_public_in_workspace(
        &state,
        experiment_id,
        definition,
        serde_json::json!(vec![1; 32]),
        cap,
        workspace,
        false,
    );
    let admission = bcode_session::child_creation_observation::observe_child_creation(
        super::super::fanout_stages::child_creation_observer(experiment_id),
        admission,
    );
    tokio::pin!(admission);
    // Readiness observes one provider gate while retaining the authoritative admission.
    let ready = async {
        while !bcode_fake_provider_plugin::fake_event_script_gate_ready(&gate.0) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    };
    tokio::pin!(ready);
    tokio::select! {
        () = &mut ready => {
            eprintln!("fanout_probe experiment={experiment_id} first_gate_ready_us={}", started.elapsed().as_micros());
            admission.await;
        }
        () = &mut admission => {
            // Admission can finish before its queued provider begins; readiness still matters.
            ready.await;
        }
    }
    let admission_elapsed = started.elapsed();
    let snapshot = experiment.snapshot();
    eprintln!(
        "fanout_probe experiment={experiment_id} cap={cap:?} admission_us={} dropped={}",
        admission_elapsed.as_micros(),
        snapshot.dropped
    );
    print_probe_observations(experiment_id, &snapshot.observations);
    // Complete admitted work before assertions or fixture teardown.
    bcode_fake_provider_plugin::release_fake_event_script_gate(&gate.0);
    let result = wait_public(&state, experiment_id).await;
    stop_fixture_driver(&state).await;
    let final_snapshot = experiment.snapshot();
    eprintln!(
        "fanout_probe experiment={experiment_id} final_observations={} pre_release_observations={} final_dropped={}",
        final_snapshot.observations.len(),
        snapshot.observations.len(),
        final_snapshot.dropped
    );
    for observation in final_snapshot
        .observations
        .iter()
        .skip(snapshot.observations.len())
    {
        eprintln!("fanout_probe experiment={experiment_id} post_release={observation:?}");
    }
    bcode_fake_provider_plugin::forget_fake_event_script_gate_ready(&gate.0);
    assert!(result.is_ok(), "probe run completion");
    assert_eq!(final_snapshot.dropped, 0, "stage observation overflow");
    assert!(
        !snapshot.observations.is_empty(),
        "stage hooks must be wired before matrix runs"
    );
}

fn print_probe_observations(
    experiment_id: &str,
    observations: &[super::super::fanout_stages::Observation],
) {
    use super::super::fanout_stages::Observation;
    for observation in observations {
        match observation {
            Observation::Stage {
                session_id,
                dispatch_identity,
                stage,
                elapsed,
            } => eprintln!(
                "fanout_probe experiment={experiment_id} session={session_id:?} dispatch={dispatch_identity:?} stage={stage:?} elapsed_us={}",
                elapsed.as_micros()
            ),
            Observation::ChildCreation(observation) => eprintln!(
                "fanout_probe experiment={experiment_id} child_session={} child_stage={:?} elapsed_us={}",
                observation.session_id,
                observation.stage,
                observation.elapsed.as_micros()
            ),
            Observation::RepositoryProbe {
                probe,
                elapsed,
                completed,
            } => eprintln!(
                "fanout_probe experiment={experiment_id} probe={probe:?} completed={completed} elapsed_us={}",
                elapsed.as_micros()
            ),
        }
    }
}

pub(super) async fn start_public(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
    definition: bcode_workflow::WorkflowDefinition,
    input: serde_json::Value,
    cap: Option<u32>,
) {
    start_public_in_workspace(
        state,
        run_id,
        definition,
        input,
        cap,
        std::path::PathBuf::from("."),
        true,
    )
    .await;
}

async fn start_public_in_workspace(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
    definition: bcode_workflow::WorkflowDefinition,
    input: serde_json::Value,
    cap: Option<u32>,
    workspace: std::path::PathBuf,
    observe_timeout: bool,
) {
    let started = std::time::Instant::now();
    let parent = state
        .sessions
        .create_session(None, workspace)
        .await
        .expect("parent");
    let identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(run_id, &definition)
        .expect("identity");
    state
        .workflow_store
        .lock()
        .expect("store")
        .persist_definition(
            &identity.definition_id,
            identity.definition_version,
            &definition,
        )
        .expect("definition");
    eprintln!("bounded provider delivery diagnosis run={run_id} cap={cap:?} starting");
    let admission_state = std::sync::Arc::clone(state);
    let admission_run = run_id.to_owned();
    let mut admission = OwnedFixtureTask::new(
        state,
        tokio::spawn(async move {
            crate::workflow_operations::start_run(
                &admission_state,
                bcode_workflow::WorkflowRunStartRequest {
                    definition_id: identity.definition_id,
                    definition_version: identity.definition_version,
                    run_id: Some(admission_run),
                    workspace_snapshot: "fixture".into(),
                    parent_session_id: parent.id,
                    parent_session_generation: None,
                    binding: None,
                    input: Some(input),
                    limits: bcode_workflow::WorkflowRunLimits {
                        concurrency_cap: cap,
                        ..Default::default()
                    },
                },
                None,
            )
            .await
            .expect("fixture admission application");
        }),
    );
    // The observation deadline must not drop an authoritative admission task.
    let overdue = if observe_timeout {
        if let Ok(result) =
            tokio::time::timeout(std::time::Duration::from_secs(40), admission.handle_mut()).await
        {
            result.expect("normal start");
            false
        } else {
            // Unblock only this fixture's gate before waiting for the retained admission.
            if let Some(gate) = state
                .selected_provider_context
                .settings
                .get("delegation_fixture_gate")
            {
                bcode_fake_provider_plugin::release_fake_event_script_gate(gate);
            }
            fixture_stage_diagnostic(state, "admission-timeout-after-gate-release", started).await;
            eprintln!(
                "normal start observation overdue: {}",
                bounded_run_diagnostic(state, run_id).await
            );
            tokio::time::timeout(std::time::Duration::from_secs(2), admission.handle_mut())
                .await
                .expect("admission unresolved after owned gate release; join handle retained")
                .expect("overdue admission task");
            true
        }
    } else {
        tokio::time::timeout(std::time::Duration::from_secs(40), admission.handle_mut())
            .await
            .expect("probe admission unresolved; join handle retained")
            .expect("probe admission task");
        false
    };
    admission.disarm_joined();
    drop(admission);
    if overdue {
        // Release the fixture provider gate before reporting a failure, after admission settled.
        if let Some(gate) = state
            .selected_provider_context
            .settings
            .get("delegation_fixture_gate")
        {
            bcode_fake_provider_plugin::release_fake_event_script_gate(gate);
        }
        let _ = wait_public(state, run_id).await;
        shutdown_fixture_driver(state, true).await;
        panic!(
            "bounded normal start observation exceeded 40s cap={cap:?}; admission retained to settlement"
        );
    }
    eprintln!("bounded provider delivery diagnosis run={run_id} cap={cap:?} start returned");
}

async fn bounded_run_diagnostic(state: &ServerState, run_id: &str) -> String {
    use std::fmt::Write as _;

    match tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let db = open(&crate::workflow_store_path(state), false)?;
        let snapshot = representation::scheduling::snapshot(&db, run_id, None, None, 100)
            .await
            .map_err(backpressure::normalize)?;
        // Typed snapshot has identities, attempt numbers, receipt-presence and terminal
        // facts only. No input, intent, receipt payload or provider-private metadata.
        let mut diagnostic = format!("{snapshot:?}");
        let pending = representation::attempts::pending(&db, run_id, None, 8)
            .await
            .map_err(backpressure::normalize)?;
        for attempt in pending {
            let Some(receipt) = attempt.receipt else {
                continue;
            };
            let decoded = crate::workflow_receipts::AgentTurnReceipt::decode(&receipt.receipt);
            if let Ok(receipt_owner) = decoded {
                let session = receipt_owner.session_id;
                let turn = receipt_owner.turn_id;
                let live = state.session_current_turn(session).await;
                let runtime = state.session_runtimes.lock().await.get(&session).cloned();
                let runtime_facts = if let Some(runtime) = runtime {
                    let phase = *runtime.phase.lock().await;
                    format!(
                        "phase={phase:?} queued_followups={} queued_steering={}",
                        runtime.queued_followups.load(std::sync::atomic::Ordering::Acquire),
                        runtime.queued_steering.load(std::sync::atomic::Ordering::Acquire),
                    )
                } else {
                    "runtime_absent".into()
                };
                let admission = crate::turn_admission_lock(state, session).await;
                let admission_locked = admission.try_lock().is_err();
                let drive_gate = crate::workflow_drive_gate(state, run_id);
                let drive_locked = drive_gate.try_lock().is_err();
                let provider_admitted = live
                    .as_ref()
                    .and_then(|current| current.model.as_ref())
                    .is_some_and(|model| !model.provider_turn_id.is_empty());
                let request_present = live.as_ref().is_some_and(|current| current.model.is_some());
                let provider_round = live
                    .as_ref()
                    .and_then(|current| current.model.as_ref())
                    .map(|model| model.identity.round);
                let evidence = tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    state.sessions.session_turn_evidence(session, &turn),
                )
                .await;
                let terminal = evidence
                    .as_ref()
                    .ok()
                    .and_then(|result| result.as_ref().ok())
                    .is_some_and(|events| {
                        events.iter().any(|event| matches!(
                            &event.kind,
                            bcode_session_models::SessionEventKind::ModelTurnFinished { turn_id, .. }
                                if turn_id == &turn
                        ))
                    });
                write!(
                    diagnostic,
                    "\nnode={} local_turn={} session_terminal={} evidence_available={} request_present={request_present} provider_admitted={provider_admitted} provider_round={provider_round:?} admission_locked={admission_locked} drive_locked={drive_locked} {runtime_facts}",
                    attempt.prepared.node_id,
                    live.is_some_and(|current| current.turn_id == turn),
                    terminal,
                    matches!(evidence, Ok(Ok(_))),
                )
                .expect("write diagnostic string");
            }
        }
        Ok::<_, ServerError>(diagnostic)
    })
    .await
    {
        Ok(Ok(snapshot)) => snapshot,
        Ok(Err(_)) => "canonical scheduling observation unavailable".into(),
        Err(_) => "canonical scheduling observation timed out".into(),
    }
}

pub(super) async fn wait_public(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
) -> Result<(), tokio::time::error::Elapsed> {
    wait_public_with_budget(state, run_id, std::time::Duration::from_secs(30)).await
}

async fn wait_public_with_budget(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
    budget: std::time::Duration,
) -> Result<(), tokio::time::error::Elapsed> {
    tokio::time::timeout(budget, async {
        loop {
            let application = crate::workflow_operations::WorkflowAuthoringApplication::new(
                state,
                crate::ClientId::new(),
            );
            let summary = bcode_workflow::WorkflowRunApplication::workflow_run_status(
                &application,
                run_id.into(),
            )
            .await
            .expect("public status")
            .expect("run");
            drop(application);
            if summary.status == bcode_workflow::RunStatus::Completed {
                break;
            }
            assert_ne!(summary.status, bcode_workflow::RunStatus::Failed);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
}
