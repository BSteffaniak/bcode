use super::super::super::{attempts, tests::run};
use super::*;
#[tokio::test]
async fn reduction_pages_waiting_members_survives_reopen_and_resets_after_retry() {
    let temp = tempfile::tempdir().unwrap();
    let _legacy = crate::WorkflowStore::create_coexistence_baseline(temp.path()).unwrap();
    let path = crate::workflow_database_path(temp.path());
    let db = switchy_database_connection::init_sqlite_rusqlite(Some(&path)).unwrap();
    super::super::super::install(&*db).await.unwrap();
    let mut definition = bcode_workflow::WorkflowBuilder::new(
        "many",
        bcode_workflow::Step::task("seed", |v: u32, _| async move { Ok(v) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let seed = definition.nodes["seed"].clone();
    definition.nodes.clear();
    definition.entries.clear();
    for i in 0..130 {
        let mut node = seed.clone();
        node.id = format!("member{i:03}");
        definition.entries.push(node.id.clone());
        definition.nodes.insert(node.id.clone(), node);
    }
    definition.exits = vec!["member129".into()];
    let mut request = run("many", false);
    let id =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("many", &definition).unwrap();
    request.definition_id = id.definition_id;
    request.definition_version = id.definition_version;
    super::super::super::admit(&*db, &request, &definition)
        .await
        .unwrap();
    assert_scan_version(&*db).await;
    let names: Vec<_> = definition.nodes.keys().map(String::as_str).collect();
    assert!(
        reduce(&*db, "many", "join", 1, &names, OutputPolicy::default())
            .await
            .unwrap()
            .is_none()
    );
    let cursor = db.select(JOBS).execute(&*db).await.unwrap();
    assert_eq!(cursor[0].get("next_member"), Some(DatabaseValue::Int64(32)));
    drop(db);
    let db = switchy_database_connection::init_sqlite_rusqlite(Some(&path)).unwrap();
    for _ in 0..3 {
        assert!(
            reduce(&*db, "many", "join", 1, &names, OutputPolicy::default())
                .await
                .unwrap()
                .is_none()
        );
    }
    let state = reduce(&*db, "many", "join", 1, &names, OutputPolicy::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.pending.len(), 130);
    assert_completed_budget(&*db, &names, &state).await;
    // A terminal failure can be superseded by an authorized retry; partial reduction must not
    // carry the old failure past the epoch change.
    let (owner, mut attempt) = failed_member(&*db).await;
    assert!(
        reduce(&*db, "many", "join", 1, &names, OutputPolicy::default())
            .await
            .unwrap()
            .is_none()
    );
    attempt.attempt = 2;
    attempt.prepared_at_ms = 4;
    attempts::owner_boundary::schedule_retry(
        &*db,
        &owner,
        "many",
        &attempt.node_id,
        &attempt.activation_id,
        1,
    )
    .await
    .unwrap();
    attempts::prepare(&*db, &owner, &attempt).await.unwrap();
    assert!(
        reduce(&*db, "many", "join", 1, &names, OutputPolicy::default())
            .await
            .unwrap()
            .is_none()
    );
    for _ in 0..3 {
        assert!(
            reduce(&*db, "many", "join", 1, &names, OutputPolicy::default())
                .await
                .unwrap()
                .is_none()
        );
    }
    let state = reduce(&*db, "many", "join", 1, &names, OutputPolicy::default())
        .await
        .unwrap()
        .unwrap();
    assert!(!state.failed);
    assert_eq!(state.pending.len(), 130);
}

async fn assert_scan_version(db: &dyn Database) {
    let before = scan_version(db, "many").await.unwrap();
    assert_eq!(before.revision, 1);
    assert_eq!(scan_version(db, "many").await.unwrap(), before);
    assert!(scan_version(db, "missing").await.is_err());
    let tx = db.begin_transaction().await.unwrap();
    assert_eq!(scan_version_in(&*tx, "many").await.unwrap(), before);
    tx.commit().await.unwrap();
    invalidate(db, "many").await.unwrap();
    let after = scan_version(db, "many").await.unwrap();
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.epoch, before.epoch + 1);
}

async fn failed_member(
    db: &dyn Database,
) -> (crate::WorkflowExecutionAuthority, crate::PreparedAttempt) {
    let owner = crate::WorkflowExecutionAuthority {
        target_artifact_id: "a".into(),
        daemon_instance_id: "d".into(),
        generation: 1,
        fencing_token: "f".into(),
    };
    attempts::claim_authority(db, "many", &owner).await.unwrap();
    let activation = super::super::activation_id(db, "many", "member000")
        .await
        .unwrap()
        .unwrap();
    let attempt = crate::PreparedAttempt {
        run_id: "many".into(),
        node_id: "member000".into(),
        activation_id: activation.clone(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 1,
    };
    let dispatch = attempts::prepare(db, &owner, &attempt).await.unwrap();
    attempts::attach_receipt(
        db,
        &owner,
        &crate::DispatchReceipt {
            run_id: "many".into(),
            node_id: "member000".into(),
            activation_id: activation,
            attempt: 1,
            dispatch_identity: dispatch.clone(),
            receipt: serde_json::json!({}),
            admitted_at_ms: 2,
        },
    )
    .await
    .unwrap();
    attempts::settle(
        db,
        &owner,
        "many",
        &dispatch,
        attempts::TerminalOutcome::Failed,
        3,
    )
    .await
    .unwrap();
    (owner, attempt)
}

async fn assert_completed_budget(
    db: &dyn Database,
    names: &[&str],
    state: &super::super::parallel::MemberState,
) {
    assert!(!pending(db, "many", "join", 1).await.unwrap());
    let repeated = reduce(db, "many", "join", 1, names, OutputPolicy::default())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(repeated.pending, state.pending);
    assert!(
        reduce(
            db,
            "many",
            "join",
            1,
            names,
            OutputPolicy {
                max_inline_bytes: 1
            }
        )
        .await
        .is_err()
    );
}
