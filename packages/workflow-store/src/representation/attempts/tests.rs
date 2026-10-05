use super::super::tests::{database, run as request};
use super::*;

#[tokio::test]
async fn fenced_ledger_preserves_duplicates_receipts_and_allowance() {
    let db = database().await;
    super::super::install(&*db).await.unwrap();
    let definition = admit_fixture(&*db).await;
    let owner = WorkflowExecutionAuthority {
        target_artifact_id: "artifact".into(),
        daemon_instance_id: "daemon".into(),
        generation: 1,
        fencing_token: "fence".into(),
    };
    claim_authority(&*db, "ledger", &owner).await.unwrap();
    claim_authority(&*db, "ledger", &owner).await.unwrap();
    let stale = WorkflowExecutionAuthority {
        generation: 2,
        ..owner.clone()
    };
    assert!(matches!(
        claim_authority(&*db, "ledger", &stale).await,
        Err(Error::AuthorityConflict)
    ));
    let prepared = PreparedAttempt {
        run_id: "ledger".into(),
        node_id: definition.entries[0].clone(),
        activation_id: super::super::graph::activation_id(&*db, "ledger", &definition.entries[0])
            .await
            .unwrap()
            .unwrap(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({"request":1}),
        prepared_at_ms: 1,
    };
    assert!(matches!(
        prepare(&*db, &stale, &prepared).await,
        Err(Error::AuthorityConflict)
    ));
    let before_prepare = parallel_epoch(&*db).await;
    let identity = prepare(&*db, &owner, &prepared).await.unwrap();
    assert_eq!(parallel_epoch(&*db).await, before_prepare + 1);
    assert_eq!(prepare(&*db, &owner, &prepared).await.unwrap(), identity);
    assert_eq!(parallel_epoch(&*db).await, before_prepare + 1);
    assert_owner_links(&*db, &owner, &prepared).await;
    assert_pending_preparation(&*db, &identity).await;
    let receipt = DispatchReceipt {
        run_id: "ledger".into(),
        node_id: prepared.node_id.clone(),
        activation_id: prepared.activation_id.clone(),
        attempt: 1,
        dispatch_identity: identity.clone(),
        receipt: serde_json::json!({"owner":"receipt"}),
        admitted_at_ms: 2,
    };
    attach_receipt(&*db, &owner, &receipt).await.unwrap();
    attach_receipt(&*db, &owner, &receipt).await.unwrap();
    assert!(
        pending(&*db, "ledger", None, 10).await.unwrap()[0]
            .receipt
            .is_some()
    );
    settle(
        &*db,
        &owner,
        "ledger",
        &identity,
        TerminalOutcome::Succeeded,
        3,
    )
    .await
    .unwrap();
    settle(
        &*db,
        &owner,
        "ledger",
        &identity,
        TerminalOutcome::Succeeded,
        3,
    )
    .await
    .unwrap();
    assert!(
        settle(
            &*db,
            &owner,
            "ledger",
            &identity,
            TerminalOutcome::Failed,
            3
        )
        .await
        .is_err()
    );
    assert_eq!(parallel_epoch(&*db).await, before_prepare + 2);
    assert_terminal_retry(&*db, &owner, &prepared, &identity).await;
    assert_eq!(parallel_epoch(&*db).await, before_prepare + 2);
}

async fn assert_pending_preparation(db: &dyn Database, identity: &str) {
    let rows = pending(db, "ledger", None, 1).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].receipt.is_none());
    assert!(
        pending(db, "ledger", Some(identity), 1)
            .await
            .unwrap()
            .is_empty()
    );
}

async fn parallel_epoch(db: &dyn Database) -> i64 {
    let rows = db
        .select("workflow_parallel_epoch_v2")
        .columns(&["epoch"])
        .where_eq("run_id", "ledger")
        .limit(1)
        .execute(db)
        .await
        .unwrap();
    rows.first().map_or(0, |row| integer(row, "epoch").unwrap())
}

async fn assert_terminal_retry(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    prepared: &PreparedAttempt,
    identity: &str,
) {
    assert_eq!(
        owner_boundary::prepared(db, "ledger", identity)
            .await
            .unwrap()
            .as_ref(),
        Some(prepared)
    );
    assert_eq!(
        owner_boundary::latest_prepared(db, "ledger", &prepared.node_id, &prepared.activation_id)
            .await
            .unwrap()
            .as_ref(),
        Some(prepared)
    );
    assert!(pending(db, "ledger", None, 10).await.unwrap().is_empty());
    assert_eq!(
        owner_boundary::dispatch_intent(db, "ledger", identity)
            .await
            .unwrap(),
        prepared.intent
    );
    assert!(
        owner_boundary::dispatch_intent(db, "wrong-run", identity)
            .await
            .is_err()
    );
    assert_eq!(prepare(db, owner, prepared).await.unwrap(), identity);
    let row = run(db, "ledger").await.unwrap();
    assert_eq!(integer(&row, "consumed").unwrap(), 1);
    assert_eq!(integer(&row, "active_attempts").unwrap(), 0);
}

#[tokio::test]
async fn repeated_terminal_observations_preserve_first_time_and_reservations() {
    for outcome in [TerminalOutcome::Failed, TerminalOutcome::Cancelled] {
        let db = database().await;
        super::super::install(&*db).await.unwrap();
        let definition = admit_fixture(&*db).await;
        let owner = WorkflowExecutionAuthority {
            target_artifact_id: "a".into(),
            daemon_instance_id: "d".into(),
            generation: 1,
            fencing_token: "f".into(),
        };
        claim_authority(&*db, "ledger", &owner).await.unwrap();
        let activation = super::super::graph::activation_id(&*db, "ledger", &definition.entries[0])
            .await
            .unwrap()
            .unwrap();
        let prepared = PreparedAttempt {
            run_id: "ledger".into(),
            node_id: definition.entries[0].clone(),
            activation_id: activation.clone(),
            attempt: 1,
            side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
            intent: serde_json::json!({}),
            prepared_at_ms: 1,
        };
        let identity = prepare(&*db, &owner, &prepared).await.unwrap();
        assert_eq!(
            owner_boundary::terminal_outcome(&*db, "ledger", &identity)
                .await
                .unwrap(),
            None
        );
        assert!(
            owner_boundary::terminal_outcome(&*db, "ledger", "missing-dispatch")
                .await
                .is_err()
        );
        reserve_test_resource(&*db, &identity).await;
        attach_receipt(
            &*db,
            &owner,
            &DispatchReceipt {
                run_id: "ledger".into(),
                node_id: prepared.node_id.clone(),
                activation_id: activation,
                attempt: 1,
                dispatch_identity: identity.clone(),
                receipt: serde_json::json!({}),
                admitted_at_ms: 2,
            },
        )
        .await
        .unwrap();
        settle(&*db, &owner, "ledger", &identity, outcome.clone(), 3)
            .await
            .unwrap();
        reserve_test_resource(&*db, "next-holder").await;
        let before = run(&*db, "ledger").await.unwrap();
        settle(&*db, &owner, "ledger", &identity, outcome.clone(), 100)
            .await
            .unwrap();
        assert_eq!(
            owner_boundary::terminal_outcome(&*db, "ledger", &identity)
                .await
                .unwrap(),
            Some(outcome.clone())
        );
        let row = attempt(&*db, &identity).await.unwrap().unwrap();
        assert_eq!(integer(&row, "terminal_at_ms").unwrap(), 3);
        assert_eq!(run(&*db, "ledger").await.unwrap().columns, before.columns);
        let leases = db
            .select("workflow_resource_leases_v2")
            .where_eq("run_id", "ledger")
            .limit(2)
            .execute(&*db)
            .await
            .unwrap();
        assert_eq!(leases.len(), 1);
        assert_eq!(
            text(&leases[0], "dispatch_identity").unwrap(),
            "next-holder"
        );
        assert_conflicting_terminal_observations(&*db, &owner, &identity, outcome).await;
    }
}

async fn assert_conflicting_terminal_observations(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    identity: &str,
    outcome: TerminalOutcome,
) {
    let stale = WorkflowExecutionAuthority {
        generation: 2,
        ..owner.clone()
    };
    assert!(
        settle(db, &stale, "ledger", identity, outcome, 101)
            .await
            .is_err()
    );
    assert!(
        settle(
            db,
            owner,
            "ledger",
            identity,
            TerminalOutcome::Succeeded,
            101
        )
        .await
        .is_err()
    );
}

async fn reserve_test_resource(db: &dyn Database, identity: &str) {
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await
        .unwrap();
    super::super::safety::reserve(
        &*tx,
        "ledger",
        identity,
        &[bcode_workflow::ResourceClaim::write("repo")],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

async fn assert_owner_links(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    prepared: &PreparedAttempt,
) {
    assert_eq!(
        owner_boundary::execution_authority(db, "ledger")
            .await
            .unwrap(),
        Some(owner.clone())
    );
    let link = crate::WorkflowExecutionSessionLink::new(
        "ledger".into(),
        prepared.node_id.clone(),
        prepared.activation_id.clone(),
        1,
        "execution-session".into(),
        request("ledger", false).workspace_snapshot,
        1,
    );
    owner_boundary::persist_execution_session_link(db, owner, &link)
        .await
        .unwrap();
    owner_boundary::persist_execution_session_link(db, owner, &link)
        .await
        .unwrap();
    assert_eq!(
        owner_boundary::execution_session_link(
            db,
            "ledger",
            &prepared.node_id,
            &prepared.activation_id,
            1
        )
        .await
        .unwrap(),
        Some(link.clone())
    );
    let conflicting = crate::WorkflowExecutionSessionLink {
        session_id: "different".into(),
        ..link
    };
    assert!(
        owner_boundary::persist_execution_session_link(db, owner, &conflicting)
            .await
            .is_err()
    );
}

async fn admit_fixture(db: &dyn Database) -> bcode_workflow::WorkflowDefinition {
    admit_fixture_with_budget(db, 1).await
}

async fn admit_fixture_with_budget(
    db: &dyn Database,
    budget: u64,
) -> bcode_workflow::WorkflowDefinition {
    let definition = bcode_workflow::WorkflowBuilder::new(
        "ledger",
        bcode_workflow::Step::task("work", |value: u32, _| async move { Ok(value) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let exact =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("ledger", &definition).unwrap();
    let mut request = request("ledger", false);
    request.definition_id = exact.definition_id;
    request.definition_version = exact.definition_version;
    request.limits.node_execution_cap = budget;
    request.limits.retry_cap = 1;
    request.limits.concurrency_cap = Some(1);
    super::super::admit(db, &request, &definition)
        .await
        .unwrap();
    definition
}

#[tokio::test]
async fn retry_requires_latest_failed_receipt_and_consumes_one_new_reservation() {
    let db = database().await;
    super::super::install(&*db).await.unwrap();
    let definition = admit_fixture_with_budget(&*db, 10).await;
    let owner = WorkflowExecutionAuthority {
        target_artifact_id: "a".into(),
        daemon_instance_id: "d".into(),
        generation: 1,
        fencing_token: "f".into(),
    };
    claim_authority(&*db, "ledger", &owner).await.unwrap();
    let mut prepared = PreparedAttempt {
        run_id: "ledger".into(),
        node_id: definition.entries[0].clone(),
        activation_id: super::super::graph::activation_id(&*db, "ledger", &definition.entries[0])
            .await
            .unwrap()
            .unwrap(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 1,
    };
    let first = prepare(&*db, &owner, &prepared).await.unwrap();
    prepared.attempt = 2;
    assert!(prepare(&*db, &owner, &prepared).await.is_err());
    let receipt = DispatchReceipt {
        run_id: "ledger".into(),
        node_id: prepared.node_id.clone(),
        activation_id: prepared.activation_id.clone(),
        attempt: 1,
        dispatch_identity: first.clone(),
        receipt: serde_json::json!({}),
        admitted_at_ms: 2,
    };
    attach_receipt(&*db, &owner, &receipt).await.unwrap();
    assert!(prepare(&*db, &owner, &prepared).await.is_err());
    settle(&*db, &owner, "ledger", &first, TerminalOutcome::Failed, 3)
        .await
        .unwrap();
    let facts = owner_boundary::retry_facts(
        &*db,
        &owner,
        "ledger",
        &prepared.node_id,
        &prepared.activation_id,
        1,
    )
    .await
    .unwrap();
    assert_eq!(facts.previous_attempt, 1);
    assert_eq!(facts.next_attempt, 2);
    prepared.prepared_at_ms = 4;
    assert_retry_grant(&*db, &owner, &prepared, &facts).await;
    let second = prepare(&*db, &owner, &prepared).await.unwrap();
    assert_eq!(
        owner_boundary::next_retry_attempt(
            &*db,
            &owner,
            "ledger",
            &prepared.node_id,
            &prepared.activation_id
        )
        .await
        .unwrap(),
        None
    );
    assert_eq!(prepare(&*db, &owner, &prepared).await.unwrap(), second);
    let row = run(&*db, "ledger").await.unwrap();
    assert_eq!(integer(&row, "consumed").unwrap(), 2);
    assert_eq!(integer(&row, "active_attempts").unwrap(), 1);
    let receipt = DispatchReceipt {
        attempt: 2,
        dispatch_identity: second.clone(),
        admitted_at_ms: 5,
        ..receipt
    };
    attach_receipt(&*db, &owner, &receipt).await.unwrap();
    settle(&*db, &owner, "ledger", &second, TerminalOutcome::Failed, 6)
        .await
        .unwrap();
    prepared.attempt = 3;
    prepared.prepared_at_ms = 7;
    assert!(matches!(
        prepare(&*db, &owner, &prepared).await,
        Err(Error::CapacityUnavailable)
    ));
    assert_extended_retry(&*db, &owner, &prepared).await;
}

async fn assert_extended_retry(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    prepared: &PreparedAttempt,
) {
    let request = super::super::allowance::AllowanceExtension {
        run_id: "ledger".into(),
        mutation_id: "retry-more".into(),
        expected_graph_revision: 1,
        node_execution_cap: 10,
        retry_cap: 2,
    };
    super::super::allowance::extend(db, owner, &request)
        .await
        .unwrap();
    owner_boundary::schedule_retry(
        db,
        owner,
        "ledger",
        &prepared.node_id,
        &prepared.activation_id,
        2,
    )
    .await
    .unwrap();
    prepare(db, owner, prepared).await.unwrap();
    let root = run(db, "ledger").await.unwrap();
    assert_eq!(integer(&root, "consumed").unwrap(), 3);
    assert_eq!(integer(&root, "active_attempts").unwrap(), 1);
}

#[tokio::test]
async fn unsupported_activation_and_missing_receipt_do_not_change_counters() {
    let db = database().await;
    super::super::install(&*db).await.unwrap();
    let definition = admit_fixture(&*db).await;
    let owner = WorkflowExecutionAuthority {
        target_artifact_id: "a".into(),
        daemon_instance_id: "d".into(),
        generation: 1,
        fencing_token: "f".into(),
    };
    claim_authority(&*db, "ledger", &owner).await.unwrap();
    let mut prepared = PreparedAttempt {
        run_id: "ledger".into(),
        node_id: definition.entries[0].clone(),
        activation_id: super::super::graph::activation_id(&*db, "ledger", &definition.entries[0])
            .await
            .unwrap()
            .unwrap(),
        attempt: 2,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 1,
    };
    assert!(matches!(
        prepare(&*db, &owner, &prepared).await,
        Err(Error::InvalidAdmission)
    ));
    assert_eq!(
        integer(&run(&*db, "ledger").await.unwrap(), "consumed").unwrap(),
        0
    );
    prepared.attempt = 1;
    let id = prepare(&*db, &owner, &prepared).await.unwrap();
    assert!(
        settle(&*db, &owner, "ledger", &id, TerminalOutcome::Cancelled, 2)
            .await
            .is_err()
    );
    assert_eq!(
        integer(&run(&*db, "ledger").await.unwrap(), "active_attempts").unwrap(),
        1
    );
    let mut changed = prepared;
    changed.intent = serde_json::json!({"different":true});
    assert!(prepare(&*db, &owner, &changed).await.is_err());
    assert_eq!(pending(&*db, "ledger", None, 1).await.unwrap().len(), 1);
}

async fn assert_retry_grant(
    db: &dyn Database,
    owner: &WorkflowExecutionAuthority,
    prepared: &PreparedAttempt,
    facts: &bcode_workflow::WorkflowNodeRetryResult,
) {
    assert!(prepare(db, owner, prepared).await.is_err());
    let grant = owner_boundary::schedule_retry(
        db,
        owner,
        "ledger",
        &prepared.node_id,
        &prepared.activation_id,
        1,
    )
    .await
    .unwrap();
    assert_eq!(&grant, facts);
    assert_eq!(
        super::super::runs::terminalize(db, owner, "ledger")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        owner_boundary::schedule_retry(
            db,
            owner,
            "ledger",
            &prepared.node_id,
            &prepared.activation_id,
            1
        )
        .await
        .unwrap(),
        grant
    );
    assert_eq!(
        owner_boundary::next_retry_attempt(
            db,
            owner,
            "ledger",
            &prepared.node_id,
            &prepared.activation_id
        )
        .await
        .unwrap(),
        Some(2)
    );
}
