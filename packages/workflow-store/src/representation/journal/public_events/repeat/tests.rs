use super::*;

fn repeat_events(page: &Page) -> Vec<&bcode_workflow::WorkflowHistoryEvent> {
    page.events
        .events
        .iter()
        .filter(|event| event.event_type == EVENT_TYPE)
        .collect()
}

async fn admitted() -> (tempfile::TempDir, Box<dyn Database>) {
    let (temp, db) = super::super::super::diagnostics::tests::admitted(true).await;
    let definition = bcode_workflow::WorkflowBuilder::new(
        "repeat-events",
        bcode_workflow::Step::task("work", |value: u32, _| async move { Ok(value) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let mut run = crate::representation::tests::run("covered", false);
    let exact =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("test", &definition).unwrap();
    run.definition_id = exact.definition_id;
    run.definition_version = exact.definition_version;
    let tx = db
        .begin_transaction_with_mode(switchy_database::TransactionMode::Immediate)
        .await
        .unwrap();
    install_in(&*tx).await.unwrap();
    super::super::enable(&*tx, "covered").await.unwrap();
    enable_in(&*tx, "covered").await.unwrap();
    crate::representation::admit_in(&*tx, &run, &definition)
        .await
        .unwrap();
    let mut historical = crate::representation::tests::run("historical", false);
    historical.definition_id = run.definition_id.clone();
    historical.definition_version = run.definition_version;
    crate::representation::admit_in(&*tx, &historical, &definition)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    (temp, db)
}
fn summary(output: &str, time: u64) -> WorkflowRepeatOutcomeSummary {
    WorkflowRepeatOutcomeSummary {
        run_id: "covered".into(),
        node_id: "work".into(),
        activation_id: "activation".into(),
        output_id: output.into(),
        outcome: bcode_workflow::WorkflowRepeatOutcomeKind::ConditionCleared,
        iterations_completed: 1,
        max_iterations: 2,
        cycle_cap: 2,
        effective_iteration_bound: 2,
        checksum_sha256: "a".repeat(64),
        created_at_ms: time,
    }
}
async fn insert_source(
    tx: &dyn DatabaseTransaction,
    summary: &WorkflowRepeatOutcomeSummary,
) -> Result<(), Error> {
    tx.insert(SOURCE)
        .value("output_id", summary.output_id.as_str())
        .value("run_id", summary.run_id.as_str())
        .value("version", 1_i64)
        .value("summary_json", encode(summary)?)
        .execute(tx)
        .await?;
    Ok(())
}

#[tokio::test]
async fn independent_origin_no_backfill_and_guarded_source() {
    let (_temp, db) = admitted().await;
    assert_eq!(
        coverage(&*db, "covered").await.unwrap(),
        Coverage::CompleteFromAdmission
    );
    assert_eq!(
        coverage(&*db, "historical").await.unwrap(),
        Coverage::HistoricalGap
    );
    let tx = db.begin_transaction().await.unwrap();
    assert!(enable_in(&*tx, "historical").await.is_err());
    drop(tx);
    let tx = db.begin_transaction().await.unwrap();
    assert!(
        insert_source(&*tx, &summary("missing-fact", 17))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn original_time_idempotence_conflict_and_rollback() {
    let (_temp, db) = admitted().await;
    let original = summary("output", 17);
    let tx = db.begin_transaction().await.unwrap();
    record_in(&*tx, &original).await.unwrap();
    record_in(&*tx, &original).await.unwrap();
    insert_source(&*tx, &original).await.unwrap();
    tx.commit().await.unwrap();
    let first = page(&*db, 1, "covered", 0, None, 100).await.unwrap();
    assert_eq!(first.repeat_coverage, Coverage::CompleteFromAdmission);
    let emissions = repeat_events(&first);
    assert_eq!(emissions.len(), 1);
    assert_eq!(emissions[0].created_at_ms, 17);
    assert_eq!(
        emissions[0].payload["repeat_emission"]["output_id"],
        "output"
    );
    let tx = db.begin_transaction().await.unwrap();
    assert!(record_in(&*tx, &summary("output", 18)).await.is_err());
    drop(tx);
    let tx = db.begin_transaction().await.unwrap();
    record_in(&*tx, &summary("rolled-back", 19)).await.unwrap();
    drop(tx);
    assert_eq!(
        page(&*db, 1, "covered", 0, None, 100)
            .await
            .unwrap()
            .events
            .through_sequence,
        first.events.through_sequence
    );
    assert!(
        db.update(SOURCE)
            .value("summary_json", "changed")
            .where_eq("output_id", "output")
            .execute(&*db)
            .await
            .is_err()
    );
    assert!(
        db.delete(SOURCE)
            .where_eq("output_id", "output")
            .execute(&*db)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn pinned_pages_exclude_later_commits_and_never_expand_v1_scopes() {
    let (_temp, db) = admitted().await;
    let tx = db.begin_transaction().await.unwrap();
    for i in 0..101 {
        let value = summary(&format!("output-{i}"), i + 100);
        record_in(&*tx, &value).await.unwrap();
        insert_source(&*tx, &value).await.unwrap();
    }
    tx.commit().await.unwrap();
    let first = page(&*db, 1, "covered", 0, None, 100).await.unwrap();
    let emissions = repeat_events(&first);
    assert!(!emissions.is_empty());
    assert_eq!(first.events.events.len(), 100);
    assert_eq!(first.events.scopes.len(), 3);
    let pin = first.events.through_sequence;
    let tx = db.begin_transaction().await.unwrap();
    let later = summary("later", 9999);
    record_in(&*tx, &later).await.unwrap();
    insert_source(&*tx, &later).await.unwrap();
    tx.commit().await.unwrap();
    let last = page(
        &*db,
        1,
        "covered",
        first.events.next_after_sequence.unwrap(),
        Some(pin),
        100,
    )
    .await
    .unwrap();
    let tail = repeat_events(&last);
    assert_eq!(emissions.len() + tail.len(), 101);
    assert!(tail.iter().all(|event| event.created_at_ms != 9999));
    assert!(last.events.next_after_sequence.is_none());
    assert_eq!(last.events.through_sequence, pin);
    assert!(page(&*db, 2, "covered", 0, None, 100).await.is_err());
    assert!(
        page(&*db, 1, "covered", 0, Some(u64::MAX), 100)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn registered_opt_in_and_actual_repeat_writer_commit_original_emission() {
    let (_temp, db, owner) = registered_repeat().await;
    let outcome = bcode_workflow::WorkflowRepeatOutcome {
        version: bcode_workflow::WORKFLOW_REPEAT_OUTCOME_VERSION,
        outcome: bcode_workflow::WorkflowRepeatOutcomeKind::ConditionCleared,
        iterations_completed: 1,
        max_iterations: 5,
        cycle_cap: 5,
        effective_iteration_bound: 5,
        value: serde_json::json!(1),
    };
    let output = crate::ValidatedOutput {
        output_id: "emission:control-output".into(),
        run_id: "run".into(),
        node_id: "repeat".into(),
        activation_id: "emission".into(),
        schema_id: "outcome".into(),
        schema_version: 1,
        value: serde_json::to_value(&outcome).unwrap(),
        artifact_reference: None,
        created_at_ms: 17,
    };
    let tx = db
        .begin_transaction_with_mode(switchy_database::TransactionMode::Immediate)
        .await
        .unwrap();
    // Supply the output producer's exact proof; the real repeat writer must create its own proof.
    super::super::control::record(
        &*tx,
        super::super::control::Commitment {
            run: &output.run_id,
            node: &output.node_id,
            activation: &output.activation_id,
            operation: super::super::control::Operation::OutputCommitted,
            outcome: super::super::control::Outcome::Succeeded,
            time: output.created_at_ms,
            source: Some(("workflow_outputs_v2", &output.output_id, 0)),
        },
    )
    .await
    .unwrap();
    tx.insert("workflow_outputs_v2")
        .value("output_id", output.output_id.as_str())
        .value("run_id", output.run_id.as_str())
        .value("activation_id", output.activation_id.as_str())
        .value("output_json", serde_json::to_string(&output).unwrap())
        .execute(&*tx)
        .await
        .unwrap();
    crate::representation::graph::repeat_outcome::record_in(&*tx, &owner, &output, &outcome)
        .await
        .unwrap();
    crate::representation::graph::repeat_outcome::record_in(&*tx, &owner, &output, &outcome)
        .await
        .unwrap();
    let stale = crate::WorkflowExecutionAuthority {
        generation: 2,
        ..owner.clone()
    };
    assert!(
        crate::representation::graph::repeat_outcome::record_in(&*tx, &stale, &output, &outcome)
            .await
            .is_err()
    );
    tx.commit().await.unwrap();
    let captured = page(&*db, 1, "run", 0, None, 100).await.unwrap();
    assert_eq!(captured.repeat_coverage, Coverage::CompleteFromAdmission);
    let emissions = repeat_events(&captured);
    assert_eq!(emissions.len(), 1);
    assert_eq!(emissions[0].created_at_ms, 17);
    let tx = db.begin_transaction().await.unwrap();
    let (records, next) =
        crate::representation::graph::repeat_outcome::page_in(&*tx, "run", None, 100)
            .await
            .unwrap();
    tx.commit().await.unwrap();
    assert!(next.is_none());
    assert_eq!(records.len(), 1);
    assert_eq!(
        emissions[0].payload["repeat_emission"],
        serde_json::to_value(&records[0]).unwrap()
    );
}

async fn registered_repeat() -> (
    tempfile::TempDir,
    Box<dyn Database>,
    crate::WorkflowExecutionAuthority,
) {
    let definition = bcode_workflow::WorkflowBuilder::new(
        "recorded-repeat",
        bcode_workflow::Step::task(
            "work",
            |value: serde_json::Value, _| async move { Ok(value) },
        )
        .repeat_while(
            "repeat",
            bcode_workflow::field::<serde_json::Value>("done").eq(false),
            5,
        ),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let (temp, db) =
        super::super::super::diagnostics::tests::admitted_definition(true, &definition).await;
    assert_eq!(
        coverage(&*db, "run").await.unwrap(),
        Coverage::CompleteFromAdmission
    );
    let owner = crate::WorkflowExecutionAuthority {
        target_artifact_id: "artifact".into(),
        daemon_instance_id: "daemon".into(),
        generation: 1,
        fencing_token: "fence".into(),
    };
    crate::representation::attempts::claim_authority(&*db, "run", &owner)
        .await
        .unwrap();
    (temp, db, owner)
}
