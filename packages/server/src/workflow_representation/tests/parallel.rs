use super::*;

#[tokio::test]
async fn concurrent_page_preserves_resource_exclusion_and_active_cancellation() {
    let _workflow_runtime_guard = crate::tests::WORKFLOW_RUNTIME_TEST_LOCK.lock().await;
    for shared_reads in [false, true] {
        let (_root, _sessions, state, _) = typed_provider_fixture_with_delegation(true).await;
        let _gate = super::fanout::ReleaseGate(
            state.selected_provider_context.settings["delegation_fixture_gate"].clone(),
        );
        let mut definition = parallel_provider_definition();
        for id in ["left", "right"] {
            definition.nodes.get_mut(id).expect("worker").resources =
                vec![bcode_workflow::ResourceClaim {
                    resource: "page-fixture".into(),
                    access: if shared_reads {
                        bcode_workflow::ResourceAccess::Read
                    } else {
                        bcode_workflow::ResourceAccess::Write
                    },
                }];
        }
        super::fanout::start_public(
            &state,
            "resource-page",
            definition,
            serde_json::json!(1),
            None,
        )
        .await;
        let db = open(&crate::workflow_store_path(&state), false).expect("db");
        let pending = representation::attempts::pending(&db, "resource-page", None, 10)
            .await
            .expect("pending");
        assert_eq!(pending.len(), if shared_reads { 2 } else { 1 });
        assert!(pending.iter().all(|attempt| attempt.receipt.is_some()));
        tokio::time::timeout(
            crate::workflow_test_support::WATCHDOG,
            bcode_fake_provider_plugin::wait_for_fake_event_script_gate_ready(
                &state.selected_provider_context.settings["delegation_fixture_gate"],
            ),
        )
        .await
        .expect("provider reached cancellation gate");
        let mut progress = crate::workflow_test_support::Progress::subscribe(&state);
        crate::workflow_operations::cancel_run(&state, "resource-page")
            .await
            .expect("cancel active admissions");
        let result = tokio::time::timeout(crate::workflow_test_support::WATCHDOG, async {
            loop {
                crate::drive_workflow_run(&state, "resource-page")
                    .await
                    .expect("drive");
                progress.drove();
                if status(&state, "resource-page")
                    .await
                    .expect("status")
                    .expect("run")
                    .status
                    == bcode_workflow::RunStatus::Cancelled
                {
                    break;
                }
                progress.changed().await;
            }
        })
        .await;
        stop_fixture_driver(&state).await;
        result.expect("all active owners settle cancellation");
        assert!(
            representation::attempts::pending(&db, "resource-page", None, 10)
                .await
                .expect("settled")
                .is_empty()
        );
    }
}

fn collected_definition() -> bcode_workflow::WorkflowDefinition {
    let config = |schema| {
        serde_json::from_value::<bcode_workflow::WorkflowPromptConfiguration>(
            crate::tests::test_workflow_prompt_configuration(
                schema,
                bcode_workflow::PromptContextTarget::FreshIsolated,
            ),
        )
        .expect("config")
    };
    let mut definition = bcode_workflow::WorkflowBuilder::new(
        "collect",
        bcode_workflow::parallel_named(
            "join",
            bcode_workflow::Step::<u32, u32>::agent(
                "left",
                &config(bcode_workflow::ValueSchema::of::<u32>()),
            )
            .expect("left"),
            bcode_workflow::Step::<u32, u32>::agent(
                "right",
                &config(bcode_workflow::ValueSchema::of::<u32>()),
            )
            .expect("right"),
        ),
    )
    .build()
    .expect("workflow")
    .definition()
    .clone();
    let failed = definition.nodes.get_mut("left").expect("left");
    failed.output = bcode_workflow::ValueSchema {
        type_name: "impossible-worker-output".into(),
        schema: serde_json::json!({"type":"integer","minimum":0,"const":-1}),
    };
    failed.configuration =
        serde_json::to_value(config(failed.output.clone())).expect("failure output config");
    let members = ["left", "right"]
        .into_iter()
        .map(|id| {
            (
                id.into(),
                bcode_workflow::workflow_member_outcome_schema(&definition.nodes[id].output)
                    .expect("outcome"),
            )
        })
        .collect();
    let schema = bcode_workflow::named_result_schema("outcomes".into(), &members).expect("schema");
    let join = definition.nodes.get_mut("join").expect("join");
    join.configuration["failure_policy"] = serde_json::json!("collect_outcomes");
    join.input = schema.clone();
    join.output = schema.clone();
    let mut integration = definition.nodes["right"].clone();
    integration.id = "integrate".into();
    integration.input = schema;
    definition.output = integration.output.clone();
    definition.nodes.insert(integration.id.clone(), integration);
    definition.edges.push(bcode_workflow::EdgeDefinition {
        from: "join".into(),
        to: "integrate".into(),
        kind: bcode_workflow::EdgeKind::Direct,
        transform: None,
    });
    definition.exits = vec!["integrate".into()];
    definition
}

#[tokio::test]
async fn normal_collect_outcomes_preserves_failed_worker_and_runs_integrator() {
    let _workflow_runtime_guard = crate::tests::WORKFLOW_RUNTIME_TEST_LOCK.lock().await;
    let (_root, _sessions, state, _) = typed_provider_fixture().await;
    let mut cleanup = FixtureCleanup::new(&state);
    state.start_workflow_driver().await;
    super::fanout::start_public(
        &state,
        "collect",
        collected_definition(),
        serde_json::json!(1),
        None,
    )
    .await;
    let completed = super::fanout::wait_public(&state, "collect").await;
    completed.expect("integrator completes despite failed worker");
    let outputs = super::super::outputs::run_outputs(&state, "collect", 100)
        .await
        .expect("outputs");
    let join = outputs
        .iter()
        .find(|output| output.node_id == "join")
        .expect("join");
    assert_eq!(join.value["left"]["status"], serde_json::json!("failed"));
    assert_eq!(
        join.value["right"]["status"],
        serde_json::json!("completed")
    );
    assert!(outputs.iter().any(|output| output.node_id == "integrate"));
    cleanup.finish().await;
    drop(cleanup);
}

#[tokio::test]
async fn normal_failfast_cancels_only_exact_sibling_not_unrelated_run() {
    let _workflow_runtime_guard = crate::tests::WORKFLOW_RUNTIME_TEST_LOCK.lock().await;
    let (_root, _sessions, mut state, _) = typed_provider_fixture_with_delegation(true).await;
    let gate = super::fanout::ReleaseGate(
        state.selected_provider_context.settings["delegation_fixture_gate"].clone(),
    );
    let marker = uuid::Uuid::new_v4().to_string();
    let script = delayed_failure_script();
    std::sync::Arc::get_mut(&mut state)
        .expect("unique fixture")
        .selected_provider_context
        .settings
        .insert(
            "fake_event_scripts_by_prompt_marker".into(),
            serde_json::to_string(&std::collections::BTreeMap::from([(
                marker.clone(),
                script,
            )]))
            .expect("selectors"),
        );
    let mut cleanup = FixtureCleanup::new(&state);
    let mut definition = parallel_provider_definition();
    definition
        .nodes
        .get_mut("left")
        .expect("left")
        .configuration["system_prompt"] = serde_json::json!(marker);
    definition
        .nodes
        .get_mut("join")
        .expect("join")
        .configuration["failure_policy"] = serde_json::json!("fail_fast");
    crate::drive_workflow_run(&state, "new")
        .await
        .expect("unrelated gated run");
    state.start_workflow_driver().await;
    super::fanout::start_public(&state, "failfast", definition, serde_json::json!(1), None).await;
    let db = open(&crate::workflow_store_path(&state), false).expect("db");
    let result = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let run = status(&state, "failfast")
                .await
                .expect("status")
                .expect("run");
            if matches!(
                run.status,
                bcode_workflow::RunStatus::Failed | bcode_workflow::RunStatus::Cancelled
            ) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    if result.is_err() {
        let snapshot = representation::scheduling::snapshot(&db, "failfast", None, None, 100).await;
        eprintln!("failfast timeout snapshot: {snapshot:?}");
    }
    result.expect("failfast settles owner cancellation");
    assert!(
        representation::safety::cancellation_requested_at(&db, "failfast")
            .await
            .expect("intent")
            .is_none()
    );
    let activation = representation::graph::activation_id(&db, "failfast", "right")
        .await
        .expect("id")
        .expect("activation");
    let dispatch = bcode_workflow_store::dispatch_identity("failfast", "right", &activation, 1);
    assert_eq!(
        representation::attempts::owner_boundary::terminal_outcome(&db, "failfast", &dispatch)
            .await
            .expect("outcome"),
        Some(representation::attempts::TerminalOutcome::Cancelled)
    );
    assert_eq!(
        representation::attempts::pending(&db, "new", None, 10)
            .await
            .expect("unrelated pending")
            .len(),
        1
    );
    drop(gate);
    // The unrelated run deliberately remains active above. Settle it explicitly before
    // the fixture's drain-before-shutdown checkpoint.
    crate::workflow_operations::cancel_run(&state, "new")
        .await
        .expect("cancel unrelated fixture run after isolation assertion");
    cleanup.finish().await;
    drop(cleanup);
}

fn delayed_failure_script() -> bcode_fake_provider_plugin::FakeProviderEventScript {
    let error: bcode_model::ProviderError=serde_json::from_value(serde_json::json!({"code":"fixture_failure","category":"invalid_request","message":"scripted terminal worker failure","retryable":false,"provider_message":null})).expect("error");
    bcode_fake_provider_plugin::FakeProviderEventScript {
        steps: vec![
            bcode_fake_provider_plugin::FakeProviderEventScriptStep {
                gate: None,
                delay_ms: Some(500),
                event: bcode_model::ProviderTurnEvent::Error { error },
            },
            bcode_fake_provider_plugin::FakeProviderEventScriptStep {
                gate: None,
                delay_ms: None,
                event: bcode_model::ProviderTurnEvent::TurnFinished {
                    stop_reason: bcode_model::StopReason::Error,
                },
            },
        ],
    }
}

#[tokio::test]
async fn public_retry_dispatches_exact_next_attempt_without_replaying_first() {
    let _workflow_runtime_guard = crate::tests::WORKFLOW_RUNTIME_TEST_LOCK.lock().await;
    let (_root, _sessions, state, _) = typed_provider_fixture().await;
    let mut cleanup = FixtureCleanup::new(&state);
    let failed = collected_definition().nodes["left"].clone();
    let definition = bcode_workflow::WorkflowDefinition {
        schema_version: bcode_workflow::WORKFLOW_DEFINITION_SCHEMA_VERSION,
        name: "retry".into(),
        input: failed.input.clone(),
        output: failed.output.clone(),
        nodes: std::collections::BTreeMap::from([("left".into(), failed)]),
        entries: vec!["left".into()],
        exits: vec!["left".into()],
        edges: vec![],
    };
    state.start_workflow_driver().await;
    super::fanout::start_public(&state, "retry", definition, serde_json::json!(1), None).await;
    let db = open(&crate::workflow_store_path(&state), false).expect("db");
    let activation = representation::graph::activation_id(&db, "retry", "left")
        .await
        .expect("id")
        .expect("activation");
    let first = bcode_workflow_store::dispatch_identity("retry", "left", &activation, 1);
    wait_failed_attempt(&db, &first).await;
    let application = crate::workflow_operations::WorkflowAuthoringApplication::new(
        &state,
        crate::ClientId::new(),
    );
    let grant = bcode_workflow::WorkflowRunApplication::retry_workflow_node(
        &application,
        "retry".into(),
        "left".into(),
        activation.clone(),
        1,
    )
    .await
    .expect("authorized retry");
    let duplicate = bcode_workflow::WorkflowRunApplication::retry_workflow_node(
        &application,
        "retry".into(),
        "left".into(),
        activation.clone(),
        1,
    )
    .await
    .expect("duplicate retry");
    drop(application);
    assert_eq!(grant, duplicate);
    assert_eq!(grant.next_attempt, 2);
    let second = bcode_workflow_store::dispatch_identity("retry", "left", &activation, 2);
    wait_failed_attempt(&db, &second).await;
    let first_receipt = representation::attempts::owner_boundary::receipt(&db, &first)
        .await
        .expect("receipt")
        .expect("first");
    let second_receipt = representation::attempts::owner_boundary::receipt(&db, &second)
        .await
        .expect("receipt")
        .expect("second");
    assert_ne!(
        first_receipt.receipt["turn_id"],
        second_receipt.receipt["turn_id"]
    );
    cleanup.finish().await;
    drop(cleanup);
}

async fn wait_failed_attempt(db: &dyn switchy_database::Database, dispatch: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if representation::attempts::owner_boundary::prepared(db, "retry", dispatch)
                .await
                .expect("prepared lookup")
                .is_none()
            {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                continue;
            }
            if representation::attempts::owner_boundary::terminal_outcome(db, "retry", dispatch)
                .await
                .expect("outcome")
                == Some(representation::attempts::TerminalOutcome::Failed)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("failed model attempt");
}
