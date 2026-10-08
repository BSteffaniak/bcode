//! Typed production-path inventory behavior without raw SQL fixtures.
use super::*;

fn open_fixture() -> (
    tempfile::TempDir,
    switchy_database::rusqlite::RusqliteDatabase,
) {
    let directory = tempfile::tempdir().unwrap();
    let store = crate::WorkflowStore::create_coexistence_baseline(directory.path()).unwrap();
    let db = switchy_database_connection::open_sqlite_rusqlite(
        store.path(),
        &switchy_database_connection::SqliteOpenOptions {
            access: switchy_database_connection::SqliteAccess::ReadWrite,
            allow_uri: false,
            busy_timeout: std::time::Duration::ZERO,
            foreign_keys: true,
            connections: std::num::NonZeroUsize::MIN,
        },
    )
    .unwrap();
    (directory, db)
}

async fn admit_fixture(db: &dyn Database) -> crate::NewWorkflowRun {
    crate::representation::install(db).await.unwrap();
    let definition = bcode_workflow::WorkflowBuilder::new(
        "activity-fixture",
        bcode_workflow::parallel(
            bcode_workflow::Step::task("east", |value: u32, _| async move { Ok(value) }),
            bcode_workflow::Step::task("west", |value: u32, _| async move { Ok(value) }),
        ),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let identity =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("activity-fixture", &definition)
            .unwrap();
    let run = crate::NewWorkflowRun {
        run_id: "activity-run".into(),
        definition_id: identity.definition_id,
        definition_version: identity.definition_version,
        workspace_snapshot: "fixture".into(),
        parent_session_id: None,
        parent_session_generation: None,
        binding: None,
        authored_provenance: None,
        input: Some(serde_json::json!(1)),
        execution_authority: None,
        created_at_ms: 1,
        authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
            version: 1,
            provider_id: "fixture".into(),
            profile_id: "build".into(),
            policy_digest_sha256: "a".repeat(64),
        },
        authorization_ceiling: bcode_workflow::WorkflowToolCapability::ReadOnly,
        limits: bcode_workflow::WorkflowRunLimits::default(),
    };
    crate::representation::admit(db, &run, &definition)
        .await
        .unwrap();
    run
}

async fn settle_fixture_attempts(db: &dyn Database, run: &crate::NewWorkflowRun) {
    let authority = crate::WorkflowExecutionAuthority {
        target_artifact_id: "fixture-artifact".into(),
        daemon_instance_id: "fixture-daemon".into(),
        generation: 1,
        fencing_token: "fixture-fence".into(),
    };
    crate::representation::attempts::claim_authority(db, &run.run_id, &authority)
        .await
        .unwrap();
    let mut dispatches = Vec::new();
    for node_id in ["east", "west"] {
        let activation = crate::representation::graph::activation_id(db, &run.run_id, node_id)
            .await
            .unwrap()
            .unwrap();
        let prepared = crate::PreparedAttempt {
            run_id: run.run_id.clone(),
            node_id: node_id.into(),
            activation_id: activation.clone(),
            attempt: 1,
            side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
            intent: serde_json::json!({}),
            prepared_at_ms: 10,
        };
        let dispatch = crate::representation::attempts::prepare(db, &authority, &prepared)
            .await
            .unwrap();
        crate::representation::attempts::attach_receipt(
            db,
            &authority,
            &crate::DispatchReceipt {
                run_id: run.run_id.clone(),
                node_id: node_id.into(),
                activation_id: activation,
                attempt: 1,
                dispatch_identity: dispatch.clone(),
                receipt: serde_json::json!({}),
                admitted_at_ms: 11,
            },
        )
        .await
        .unwrap();
        dispatches.push(dispatch);
    }
    for dispatch in dispatches {
        crate::representation::attempts::settle(
            db,
            &authority,
            &run.run_id,
            &dispatch,
            crate::representation::attempts::TerminalOutcome::Failed,
            12,
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn invalid_inventory_coordinates_fail_without_mutating_admission() {
    let (_directory, db) = open_fixture();
    let run = admit_fixture(&db).await;
    let before = page(&db, &run.run_id, None, Section::Attempts, None, 1)
        .await
        .unwrap();
    for limit in [0, 101] {
        assert!(
            page(&db, &run.run_id, None, Section::Attempts, None, limit)
                .await
                .is_err()
        );
    }
    assert!(
        page(&db, &run.run_id, Some(0), Section::Attempts, None, 1)
            .await
            .is_err()
    );
    assert!(
        page(&db, "missing-run", None, Section::Attempts, None, 1)
            .await
            .is_err()
    );
    assert!(
        page(&db, &run.run_id, None, Section::Attempts, Some(""), 1)
            .await
            .is_err()
    );
    let after = page(&db, &run.run_id, None, Section::Attempts, None, 1)
        .await
        .unwrap();
    assert_eq!(after.revision, before.revision);
    assert!(after.attempts.is_empty());
    assert!(after.next_after.is_none());
}

#[tokio::test]
async fn transaction_compatible_reads_leave_caller_snapshot_active() {
    let (_directory, db) = open_fixture();
    let run = admit_fixture(&db).await;
    settle_fixture_attempts(&db, &run).await;
    let tx = db.begin_transaction().await.unwrap();
    assert!(matches!(
        super::super::runs::lookup_in(&*tx, &run.run_id)
            .await
            .unwrap(),
        super::super::runs::RunLookup::Available(_)
    ));
    assert!(matches!(
        super::super::runs::lookup_in(&*tx, "missing-run")
            .await
            .unwrap(),
        super::super::runs::RunLookup::Missing
    ));
    let inventory = page_in(&*tx, &run.run_id, None, Section::Attempts, None, 100)
        .await
        .unwrap();
    for attempt in inventory.attempts {
        assert_eq!(
            super::super::attempts::owner_boundary::terminal_outcome_in(
                &*tx,
                &run.run_id,
                &attempt.dispatch_identity,
            )
            .await
            .unwrap(),
            Some(super::super::attempts::TerminalOutcome::Failed)
        );
    }
    assert!(
        super::super::attempts::owner_boundary::terminal_outcome_in(
            &*tx,
            &run.run_id,
            "missing-dispatch",
        )
        .await
        .is_err()
    );
    assert!(!tx.sqlite_is_autocommit().await.unwrap());
    assert!(
        page_in(&*tx, &run.run_id, None, Section::Children, None, 1)
            .await
            .unwrap()
            .children
            .is_empty()
    );
    tx.rollback().await.unwrap();
    assert!(
        super::super::attempts::owner_boundary::terminal_outcome(
            &db,
            &run.run_id,
            "missing-dispatch",
        )
        .await
        .is_err()
    );
    assert!(matches!(
        super::super::runs::lookup(&db, "missing-run")
            .await
            .unwrap(),
        super::super::runs::RunLookup::Missing
    ));
    let next = db.begin_transaction().await.unwrap();
    next.rollback().await.unwrap();
}

#[tokio::test]
async fn attempts_without_sessions_still_advance_inventory_cursor() {
    let (_directory, db) = open_fixture();
    let run = admit_fixture(&db).await;
    settle_fixture_attempts(&db, &run).await;
    let first = page(&db, &run.run_id, None, Section::Attempts, None, 1)
        .await
        .unwrap();
    assert_eq!(first.attempts.len(), 1);
    assert!(first.attempts[0].session.is_none());
    let next = first.next_after.as_deref().unwrap();
    let second = page(
        &db,
        &run.run_id,
        Some(first.revision),
        Section::Attempts,
        Some(next),
        1,
    )
    .await
    .unwrap();
    assert_eq!(second.attempts.len(), 1);
    assert_ne!(
        first.attempts[0].dispatch_identity,
        second.attempts[0].dispatch_identity
    );
    assert!(second.next_after.is_none());
    assert!(
        page(
            &db,
            &run.run_id,
            Some(first.revision + 1),
            Section::Attempts,
            None,
            1
        )
        .await
        .is_err()
    );
    assert!(
        page(&db, &run.run_id, None, Section::Children, None, 0)
            .await
            .is_err()
    );
    let snapshot = db.begin_transaction().await.unwrap();
    let pinned_first = page_in(&*snapshot, &run.run_id, None, Section::Attempts, None, 1)
        .await
        .unwrap();
    let pinned_second = page_in(
        &*snapshot,
        &run.run_id,
        Some(pinned_first.revision),
        Section::Attempts,
        pinned_first.next_after.as_deref(),
        1,
    )
    .await
    .unwrap();
    assert_eq!(pinned_second.attempts.len(), 1);
    // The helper did not consume or commit the caller's transaction.
    assert!(
        page_in(
            &*snapshot,
            &run.run_id,
            Some(pinned_first.revision),
            Section::Children,
            None,
            1
        )
        .await
        .unwrap()
        .children
        .is_empty()
    );
    snapshot.rollback().await.unwrap();
    let children = page(
        &db,
        &run.run_id,
        Some(first.revision),
        Section::Children,
        None,
        1,
    )
    .await
    .unwrap();
    assert!(children.children.is_empty());
    assert!(children.next_after.is_none());
}
