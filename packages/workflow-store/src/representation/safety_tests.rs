use super::*;

fn recovery_owner(id: &str, generation: u64) -> WorkflowExecutionAuthority {
    WorkflowExecutionAuthority {
        target_artifact_id: id.into(),
        daemon_instance_id: id.into(),
        generation,
        fencing_token: id.into(),
    }
}

async fn recovery_fixture() -> Box<dyn Database> {
    let db = super::super::tests::database().await;
    super::super::install(&*db).await.unwrap();
    let definition = bcode_workflow::WorkflowBuilder::new(
        "recovery",
        bcode_workflow::Step::task("work", |v: u32, _| async move { Ok(v) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let id = bcode_workflow::WorkflowDefinitionIdentity::for_definition("recovery", &definition)
        .unwrap();
    let mut request = super::super::tests::run("recovery", false);
    request.definition_id = id.definition_id;
    request.definition_version = id.definition_version;
    super::super::admit(&*db, &request, &definition)
        .await
        .unwrap();
    db
}

#[tokio::test]
async fn recovery_handoff_preserves_unresolved_receipt_until_exact_settlement() {
    let db = recovery_fixture().await;
    let old = recovery_owner("old", 1);
    let new = recovery_owner("new", 2);
    super::super::attempts::claim_authority(&*db, "recovery", &old)
        .await
        .unwrap();
    let activation = super::super::graph::activation_id(&*db, "recovery", "work")
        .await
        .unwrap()
        .unwrap();
    let prepared = crate::PreparedAttempt {
        run_id: "recovery".into(),
        node_id: "work".into(),
        activation_id: activation.clone(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::Mutating,
        intent: serde_json::json!({}),
        prepared_at_ms: 1,
    };
    let dispatch = super::super::attempts::prepare(&*db, &old, &prepared)
        .await
        .unwrap();
    let transition = AuthorityHandoff {
        run_id: "recovery".into(),
        expected: old.clone(),
        replacement: new.clone(),
    };
    assert!(
        handoff_for_recovery(&*db, &transition, &Resolver(false))
            .await
            .is_err()
    );
    assert!(!recovery_required(&*db, "recovery").await.unwrap());
    handoff_for_recovery(&*db, &transition, &Resolver(true))
        .await
        .unwrap();
    handoff_for_recovery(&*db, &transition, &Resolver(true))
        .await
        .unwrap();
    assert!(
        super::super::attempts::prepare(&*db, &new, &prepared)
            .await
            .is_err()
    );
    assert!(finish_recovery(&*db, "recovery", &new).await.is_err());
    let pending =
        super::super::attempts::owner_boundary::recovery_attempts(&*db, "recovery", &new, None, 1)
            .await
            .unwrap();
    assert_eq!(pending.0.len(), 1);
    assert!(pending.0[0].receipt.is_none());
    let receipt = crate::DispatchReceipt {
        run_id: "recovery".into(),
        node_id: "work".into(),
        activation_id: activation,
        attempt: 1,
        dispatch_identity: dispatch.clone(),
        receipt: serde_json::json!({"recovered":"owner receipt"}),
        admitted_at_ms: 2,
    };
    super::super::attempts::attach_receipt(&*db, &new, &receipt)
        .await
        .unwrap();
    assert!(
        super::super::attempts::settle(
            &*db,
            &old,
            "recovery",
            &dispatch,
            super::super::attempts::TerminalOutcome::Failed,
            3
        )
        .await
        .is_err()
    );
    super::super::attempts::settle(
        &*db,
        &new,
        "recovery",
        &dispatch,
        super::super::attempts::TerminalOutcome::Failed,
        3,
    )
    .await
    .unwrap();
    assert!(finish_recovery(&*db, "recovery", &old).await.is_err());
    finish_recovery(&*db, "recovery", &new).await.unwrap();
    assert!(!recovery_required(&*db, "recovery").await.unwrap());
    assert_eq!(
        super::super::attempts::owner_boundary::receipt(&*db, &dispatch)
            .await
            .unwrap(),
        Some(receipt)
    );
}

struct Resolver(bool);
impl AuthorityHandoffResolver for Resolver {
    fn verify<'a>(
        &'a self,
        transition: &'a AuthorityHandoff,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<HandoffEvidence, Error>> + Send + 'a>,
    > {
        Box::pin(async move {
            Ok(if self.0 {
                HandoffEvidence::Verified {
                    transition: Box::new(transition.clone()),
                    receipt_id: "verified-owner-receipt".into(),
                    verified_at_ms: 30,
                }
            } else {
                HandoffEvidence::Unknown
            })
        })
    }
}

#[tokio::test]
async fn cancellation_is_fenced_idempotent_and_retains_run_state() {
    let db = super::super::tests::database().await;
    super::super::install(&*db).await.unwrap();
    let definition = bcode_workflow::WorkflowBuilder::new(
        "safety",
        bcode_workflow::Step::task("work", |value: u32, _| async move { Ok(value) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let exact =
        bcode_workflow::WorkflowDefinitionIdentity::for_definition("safety", &definition).unwrap();
    let mut request = super::super::tests::run("safety", false);
    request.definition_id = exact.definition_id;
    request.definition_version = exact.definition_version;
    super::super::admit(&*db, &request, &definition)
        .await
        .unwrap();
    let owner = WorkflowExecutionAuthority {
        target_artifact_id: "a".into(),
        daemon_instance_id: "d".into(),
        generation: 1,
        fencing_token: "f".into(),
    };
    super::super::attempts::claim_authority(&*db, "safety", &owner)
        .await
        .unwrap();
    let stale = WorkflowExecutionAuthority {
        generation: 2,
        ..owner.clone()
    };
    assert!(matches!(
        request_cancellation(&*db, "safety", &stale, 10).await,
        Err(Error::AuthorityConflict)
    ));
    assert!(db.select(CANCEL).execute(&*db).await.unwrap().is_empty());
    let before = db
        .select(RUNS)
        .where_eq("run_id", "safety")
        .execute(&*db)
        .await
        .unwrap();
    request_cancellation(&*db, "safety", &owner, 10)
        .await
        .unwrap();
    request_cancellation(&*db, "safety", &owner, 20)
        .await
        .unwrap();
    let after = db
        .select(RUNS)
        .where_eq("run_id", "safety")
        .execute(&*db)
        .await
        .unwrap();
    assert_eq!(before[0].columns, after[0].columns);
    let requests = db.select(CANCEL).execute(&*db).await.unwrap();
    assert_eq!(
        cancellation_requested_at(&*db, "safety").await.unwrap(),
        Some(10)
    );
    assert_handoff(&*db, &owner).await;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].get("requested_at_ms"),
        Some(DatabaseValue::Int64(10))
    );
}

async fn assert_handoff(db: &dyn Database, owner: &WorkflowExecutionAuthority) {
    let transition = AuthorityHandoff {
        run_id: "safety".into(),
        expected: owner.clone(),
        replacement: WorkflowExecutionAuthority {
            target_artifact_id: "b".into(),
            daemon_instance_id: "new".into(),
            generation: 2,
            fencing_token: "new-fence".into(),
        },
    };
    assert!(
        handoff_authority(db, &transition, &Resolver(false))
            .await
            .is_err()
    );
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await
        .unwrap();
    reserve(
        &*tx,
        "safety",
        "ambiguous-holder",
        &[ResourceClaim::write("repo")],
    )
    .await
    .unwrap_err();
    // Cancellation already persisted above; insert retained lease through typed fixture API.
    tx.insert(LEASES)
        .value("run_id", "safety")
        .value("dispatch_identity", "ambiguous-holder")
        .value("resource_key", "repo")
        .value("access", "write")
        .execute(&*tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        handoff_authority(db, &transition, &Resolver(true))
            .await
            .is_err()
    );
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await
        .unwrap();
    release(&*tx, "safety", "ambiguous-holder").await.unwrap();
    tx.commit().await.unwrap();
    handoff_authority(db, &transition, &Resolver(true))
        .await
        .unwrap();
    handoff_authority(db, &transition, &Resolver(true))
        .await
        .unwrap();
    assert!(request_cancellation(db, "safety", owner, 40).await.is_err());
    assert_terminal_handoff(db, &transition.replacement).await;
    let conflicting = AuthorityHandoff {
        replacement: WorkflowExecutionAuthority {
            fencing_token: "conflict".into(),
            ..transition.replacement.clone()
        },
        ..transition
    };
    assert!(
        handoff_authority(db, &conflicting, &Resolver(true))
            .await
            .is_err()
    );
}

async fn assert_terminal_handoff(db: &dyn Database, owner: &WorkflowExecutionAuthority) {
    let before = db
        .select(RUNS)
        .where_eq("run_id", "safety")
        .limit(1)
        .execute(db)
        .await
        .unwrap();
    let association = before[0].get("association_session_id");
    super::super::runs::terminalize(db, owner, "safety")
        .await
        .unwrap();
    let transition = AuthorityHandoff {
        run_id: "safety".into(),
        expected: owner.clone(),
        replacement: WorkflowExecutionAuthority {
            target_artifact_id: "c".into(),
            daemon_instance_id: "third".into(),
            generation: 3,
            fencing_token: "third-fence".into(),
        },
    };
    assert!(
        handoff_authority(db, &transition, &Resolver(true))
            .await
            .is_err()
    );
    let after = db
        .select(RUNS)
        .where_eq("run_id", "safety")
        .limit(1)
        .execute(db)
        .await
        .unwrap();
    assert_eq!(
        after[0].get("status"),
        Some(DatabaseValue::String("cancelled".into()))
    );
    assert_eq!(after[0].get("association_session_id"), association);
    assert_eq!(
        after[0].get("authority_json"),
        before[0].get("authority_json")
    );
}

#[tokio::test]
async fn shared_reads_block_writes_and_failed_claim_sets_leave_no_partial_reservation() {
    let db = super::super::tests::database().await;
    super::super::install(&*db).await.unwrap();
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await
        .unwrap();
    reserve(&*tx, "run", "read-a", &[ResourceClaim::read("repo")])
        .await
        .unwrap();
    reserve(&*tx, "run", "read-b", &[ResourceClaim::read("repo")])
        .await
        .unwrap();
    assert!(
        reserve(
            &*tx,
            "run",
            "writer",
            &[ResourceClaim::write("a-free"), ResourceClaim::write("repo")]
        )
        .await
        .is_err()
    );
    assert!(
        tx.select(LEASES)
            .where_eq("dispatch_identity", "writer")
            .execute(&*tx)
            .await
            .unwrap()
            .is_empty()
    );
    release(&*tx, "run", "read-a").await.unwrap();
    assert!(
        reserve(&*tx, "run", "writer", &[ResourceClaim::write("repo")])
            .await
            .is_err()
    );
    release(&*tx, "run", "read-b").await.unwrap();
    reserve(
        &*tx,
        "run",
        "writer",
        &[ResourceClaim::read("repo"), ResourceClaim::write("repo")],
    )
    .await
    .unwrap();
    reserve(&*tx, "run", "writer", &[ResourceClaim::write("repo")])
        .await
        .unwrap();
    assert!(
        reserve(&*tx, "run", "writer", &[ResourceClaim::read("repo")])
            .await
            .is_err()
    );
    assert!(
        reserve(&*tx, "run", "reader", &[ResourceClaim::read("repo")])
            .await
            .is_err()
    );
    reserve(&*tx, "other-run", "reader", &[ResourceClaim::read("repo")])
        .await
        .unwrap();
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn large_claim_sets_page_duplicates_and_preserve_atomic_reservation() {
    let db = super::super::tests::database().await;
    super::super::install(&*db).await.unwrap();
    let claims = (0..513)
        .map(|i| ResourceClaim::write(format!("resource-{i:04}")))
        .collect::<Vec<_>>();
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await
        .unwrap();
    reserve(&*tx, "large", "holder", &claims).await.unwrap();
    tx.commit().await.unwrap();
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await
        .unwrap();
    reserve(&*tx, "large", "holder", &claims).await.unwrap();
    assert!(
        reserve(&*tx, "large", "holder", &claims[..512])
            .await
            .is_err()
    );
    let mut changed = claims.clone();
    changed[512].access = ResourceAccess::Read;
    assert!(reserve(&*tx, "large", "holder", &changed).await.is_err());
    let mut conflicting = vec![ResourceClaim::write("a-free")];
    conflicting.extend(claims.clone());
    assert!(
        reserve(&*tx, "large", "blocked", &conflicting)
            .await
            .is_err()
    );
    assert!(
        tx.select(LEASES)
            .where_eq("dispatch_identity", "blocked")
            .limit(1)
            .execute(&*tx)
            .await
            .unwrap()
            .is_empty()
    );
    release(&*tx, "large", "holder").await.unwrap();
    reserve(&*tx, "large", "replacement", &claims)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await
        .unwrap();
    reserve(&*tx, "large", "holder", &claims).await.unwrap();
    assert!(
        tx.select(LEASES)
            .where_eq("dispatch_identity", "replacement")
            .limit(1)
            .execute(&*tx)
            .await
            .unwrap()
            .is_empty()
    );
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn cancellation_blocks_admission_without_releasing_existing_claims() {
    let db = super::super::tests::database().await;
    super::super::install(&*db).await.unwrap();
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await
        .unwrap();
    reserve(&*tx, "run", "writer", &[ResourceClaim::write("repo")])
        .await
        .unwrap();
    tx.insert(CANCEL)
        .value("run_id", "run")
        .value("requested_at_ms", 10_i64)
        .execute(&*tx)
        .await
        .unwrap();
    assert!(ensure_admission_allowed(&*tx, "run").await.is_err());
    assert!(reserve(&*tx, "run", "next", &[]).await.is_err());
    assert_eq!(
        tx.select(LEASES)
            .where_eq("run_id", "run")
            .execute(&*tx)
            .await
            .unwrap()
            .len(),
        1
    );
    tx.commit().await.unwrap();
    let tx = db
        .begin_transaction_with_mode(TransactionMode::Immediate)
        .await
        .unwrap();
    assert!(ensure_admission_allowed(&*tx, "run").await.is_err());
    assert_eq!(
        tx.select(LEASES)
            .where_eq("run_id", "run")
            .execute(&*tx)
            .await
            .unwrap()
            .len(),
        1
    );
    tx.rollback().await.unwrap();
}
