//! Persistent actor-serialized managed tool claims; no native process or providers.
use bcode_harness_models::{
    HarnessAuthBinding, HarnessAuthCustody, HarnessCapabilities, HarnessCapability,
    HarnessInstance, HarnessSupportLevel,
};
use bcode_session::{SessionManager, lease::SessionLeaseOwnerContext};
use bcode_session_models::{
    ExternalHarnessOwner, ManagedToolClaimOutcome, ManagedToolIdentity, ToolInvocationResultRecord,
};

fn manager(root: &std::path::Path) -> SessionManager {
    SessionManager::persistent_with_metrics_and_lease_owner(
        root,
        bcode_metrics::MetricsRegistry::disabled(),
        SessionLeaseOwnerContext {
            build_fingerprint: Some("artifact".into()),
            daemon_instance_id: Some("daemon".into()),
            ..SessionLeaseOwnerContext::default()
        },
    )
    .unwrap()
}
async fn setup(root: &std::path::Path) -> (SessionManager, ManagedToolIdentity) {
    let sessions = manager(root);
    let session = sessions.create_session(None, root.into()).await.unwrap();
    let owner = ExternalHarnessOwner {
        session_id: session.id,
        generation: 1,
        artifact_id: "artifact".into(),
        daemon_id: "daemon".into(),
    };
    let c = HarnessCapability {
        support: HarnessSupportLevel::Supported,
        constraints: Vec::new(),
    };
    sessions
        .configure_external_harness(
            &owner,
            HarnessInstance {
                instance_id: "native".into(),
                plugin_id: "native".into(),
                executable: "/not-run".into(),
                config_namespace: "native".into(),
                auth: HarnessAuthBinding {
                    provider_id: "native".into(),
                    profile: "native".into(),
                    credential_namespace: "native".into(),
                    custody: HarnessAuthCustody::NativeManaged,
                },
            },
            HarnessCapabilities {
                resume: c.clone(),
                steering: c.clone(),
                approval: c.clone(),
                questions: c.clone(),
                assistance: c.clone(),
                interrupt: c.clone(),
                authoritative_settlement: c.clone(),
                background_continuation: c.clone(),
                authentication: c,
            },
        )
        .await
        .unwrap();
    let identity = ManagedToolIdentity {
        owner,
        grant_id: "host-grant".into(),
        invocation_id: "ordinary-call".into(),
        producer_plugin_id: "filesystem".into(),
        tool_name: "filesystem.read".into(),
        arguments_sha256: "a".repeat(64),
        preparation_sha256: "b".repeat(64),
        working_directory: root.to_str().unwrap().into(),
    };
    (sessions, identity)
}

#[tokio::test]
async fn simultaneous_claims_and_conflicts_are_serialized() {
    let root = tempfile::tempdir().unwrap();
    let (sessions, identity) = setup(root.path()).await;
    let (left, right) = tokio::join!(
        sessions.claim_managed_tool(identity.clone()),
        sessions.claim_managed_tool(identity.clone())
    );
    assert!(matches!(
        (&left, &right),
        (
            Ok(ManagedToolClaimOutcome::New(_)),
            Ok(ManagedToolClaimOutcome::Uncertain(_))
        ) | (
            Ok(ManagedToolClaimOutcome::Uncertain(_)),
            Ok(ManagedToolClaimOutcome::New(_))
        )
    ));
    let result = ToolInvocationResultRecord {
        invocation_id: identity.invocation_id.clone(),
        model_output: "not started".into(),
        is_error: false,
        content: Vec::new(),
        presentation: None,
        result: None,
    };
    let claim = sessions
        .inspect_managed_tool(&identity.owner, identity.invocation_id.clone())
        .await
        .unwrap()
        .unwrap();
    assert!(
        sessions
            .complete_managed_tool(identity.clone(), claim.claim_sequence, result)
            .await
            .is_err()
    );
    assert_eq!(
        sessions
            .inspect_managed_tool(&identity.owner, identity.invocation_id.clone())
            .await
            .unwrap(),
        Some(claim.clone())
    );
    let (left, right) = tokio::join!(
        sessions.start_managed_tool(identity.clone(), claim.claim_sequence),
        sessions.start_managed_tool(identity.clone(), claim.claim_sequence)
    );
    assert_ne!(
        left.is_ok(),
        right.is_ok(),
        "exactly one dispatch admission"
    );
    let mut conflict = identity.clone();
    conflict.arguments_sha256 = "c".repeat(64);
    assert!(sessions.claim_managed_tool(conflict).await.is_err());
    let mut forged = identity.clone();
    forged.grant_id = "other-grant".into();
    assert!(sessions.claim_managed_tool(forged).await.is_err());
    let mut stale = identity;
    stale.owner.generation += 1;
    assert!(sessions.claim_managed_tool(stale).await.is_err());
    drop(sessions);
}

#[tokio::test]
async fn started_restart_never_replays_and_exact_terminal_result_is_retained() {
    let root = tempfile::tempdir().unwrap();
    let (sessions, identity) = setup(root.path()).await;
    let ManagedToolClaimOutcome::New(claim) =
        sessions.claim_managed_tool(identity.clone()).await.unwrap()
    else {
        panic!("new claim")
    };
    sessions
        .start_managed_tool(identity.clone(), claim.claim_sequence)
        .await
        .unwrap();
    assert!(
        sessions
            .start_managed_tool(identity.clone(), claim.claim_sequence)
            .await
            .is_err()
    );
    sessions
        .release_session_ownership(identity.owner.session_id)
        .await
        .unwrap();
    drop(sessions);
    let sessions = manager(root.path());
    assert!(matches!(
        sessions.claim_managed_tool(identity.clone()).await.unwrap(),
        ManagedToolClaimOutcome::Uncertain(_)
    ));
    let retained = sessions
        .inspect_managed_tool(&identity.owner, identity.invocation_id.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retained.claim_sequence, claim.claim_sequence);
    assert!(retained.started_sequence.is_some());
    assert!(retained.result_sequence.is_none());
    assert!(
        sessions
            .start_managed_tool(identity.clone(), claim.claim_sequence)
            .await
            .is_err()
    );
    let result = ToolInvocationResultRecord {
        invocation_id: identity.invocation_id.clone(),
        model_output: "canonical tool result".into(),
        is_error: false,
        content: Vec::new(),
        presentation: None,
        result: None,
    };
    assert!(
        sessions
            .complete_managed_tool(identity.clone(), claim.claim_sequence + 1, result.clone())
            .await
            .is_err()
    );
    sessions
        .complete_managed_tool(identity.clone(), claim.claim_sequence, result.clone())
        .await
        .unwrap();
    sessions
        .complete_managed_tool(identity.clone(), claim.claim_sequence, result.clone())
        .await
        .unwrap();
    match sessions.claim_managed_tool(identity.clone()).await.unwrap() {
        ManagedToolClaimOutcome::Recorded {
            result: retained, ..
        } => assert_eq!(*retained, result),
        other => panic!("expected exact terminal evidence: {other:?}"),
    }
    let terminal = sessions
        .inspect_managed_tool(&identity.owner, identity.invocation_id.clone())
        .await
        .unwrap()
        .unwrap();
    let result_sequence = terminal.result_sequence.unwrap();
    let events = sessions
        .session_events_range(
            identity.owner.session_id,
            claim.claim_sequence,
            result_sequence,
            16,
        )
        .await
        .unwrap();
    assert_eq!(events.len(), 3, "claim/start/result committed exactly once");
    let mut changed = result;
    changed.model_output = "altered".into();
    assert!(
        sessions
            .complete_managed_tool(identity, claim.claim_sequence, changed)
            .await
            .is_err()
    );
    drop(sessions);
}

#[tokio::test]
async fn changed_generation_cannot_inspect_claim_or_record_result() {
    let root = tempfile::tempdir().unwrap();
    let (sessions, identity) = setup(root.path()).await;
    let ManagedToolClaimOutcome::New(claim) =
        sessions.claim_managed_tool(identity.clone()).await.unwrap()
    else {
        panic!("new claim")
    };
    sessions
        .start_managed_tool(identity.clone(), claim.claim_sequence)
        .await
        .unwrap();
    let mut next = identity.owner.clone();
    next.generation += 1;
    assert!(
        sessions
            .inspect_managed_tool(&next, identity.invocation_id.clone())
            .await
            .is_err()
    );
    let mut changed = identity.clone();
    changed.owner = next;
    assert!(sessions.claim_managed_tool(changed.clone()).await.is_err());
    assert!(
        sessions
            .complete_managed_tool(
                changed,
                claim.claim_sequence,
                ToolInvocationResultRecord {
                    invocation_id: identity.invocation_id,
                    model_output: "stale result".into(),
                    is_error: false,
                    content: Vec::new(),
                    presentation: None,
                    result: None,
                },
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn missing_claim_rows_fail_closed_at_every_transition() {
    for stage in 0..3 {
        check_missing_claim_row(stage).await;
    }
}

async fn check_missing_claim_row(stage: u8) {
    use switchy::database::query::FilterableQuery as _;

    let root = tempfile::tempdir().unwrap();
    let (sessions, identity) = setup(root.path()).await;
    let ManagedToolClaimOutcome::New(claim) =
        sessions.claim_managed_tool(identity.clone()).await.unwrap()
    else {
        panic!("new claim")
    };
    let result = ToolInvocationResultRecord {
        invocation_id: identity.invocation_id.clone(),
        model_output: "canonical".into(),
        is_error: false,
        content: Vec::new(),
        presentation: None,
        result: None,
    };
    if stage >= 1 {
        sessions
            .start_managed_tool(identity.clone(), claim.claim_sequence)
            .await
            .unwrap();
    }
    if stage == 2 {
        sessions
            .complete_managed_tool(identity.clone(), claim.claim_sequence, result.clone())
            .await
            .unwrap();
    }
    sessions
        .release_session_ownership(identity.owner.session_id)
        .await
        .unwrap();
    drop(sessions);
    let db = bcode_session::db::SessionDb::open_existing_turso_in_root(
        identity.owner.session_id,
        root.path(),
    )
    .await
    .unwrap();
    let before = db.all_events_strict().await.unwrap();
    db.database()
        .delete("managed_tool_claims")
        .where_eq("invocation_id", identity.invocation_id.clone())
        .execute(db.database())
        .await
        .unwrap();
    db.database().close().await.unwrap();
    drop(db);
    let sessions = manager(root.path());
    assert!(
        sessions
            .inspect_managed_tool(&identity.owner, identity.invocation_id.clone())
            .await
            .is_err()
    );
    assert!(sessions.claim_managed_tool(identity.clone()).await.is_err());
    assert!(
        sessions
            .start_managed_tool(identity.clone(), claim.claim_sequence)
            .await
            .is_err()
    );
    assert!(
        sessions
            .complete_managed_tool(identity.clone(), claim.claim_sequence, result)
            .await
            .is_err()
    );
    sessions
        .release_session_ownership(identity.owner.session_id)
        .await
        .unwrap();
    drop(sessions);
    let db = bcode_session::db::SessionDb::open_existing_turso_in_root(
        identity.owner.session_id,
        root.path(),
    )
    .await
    .unwrap();
    assert_eq!(db.all_events_strict().await.unwrap(), before);
    db.database().close().await.unwrap();
}

#[tokio::test]
async fn epoch_22_partial_keys_cannot_be_read_as_current_claim_absence() {
    use switchy::database::query::FilterableQuery as _;

    let root = tempfile::tempdir().unwrap();
    let (sessions, identity) = setup(root.path()).await;
    sessions.claim_managed_tool(identity.clone()).await.unwrap();
    sessions
        .release_session_ownership(identity.owner.session_id)
        .await
        .unwrap();
    drop(sessions);
    let db = bcode_session::db::SessionDb::open_existing_turso_in_root(
        identity.owner.session_id,
        root.path(),
    )
    .await
    .unwrap();
    // Synthetic compatibility input, not an authenticated historical producer fixture.
    db.database()
        .update("events")
        .value("managed_tool_key", switchy::database::DatabaseValue::Null)
        .execute(db.database())
        .await
        .unwrap();
    db.database()
        .update("session_storage_contract")
        .value("writer_epoch", 22_i64)
        .where_eq("contract_id", 1_i64)
        .execute(db.database())
        .await
        .unwrap();
    assert!(db.validate_read_readiness().await.is_err());
    assert!(db.validate_write_readiness().await.is_err());
    db.database().close().await.unwrap();
    drop(db);
    let reopened = bcode_session::db::SessionDb::open_existing_turso_in_root(
        identity.owner.session_id,
        root.path(),
    )
    .await
    .unwrap();
    assert!(reopened.validate_read_readiness().await.is_err());
    assert!(reopened.validate_write_readiness().await.is_err());
    reopened.database().close().await.unwrap();
}

#[tokio::test]
async fn changed_claim_projection_is_rejected_without_replay() {
    use switchy::database::query::FilterableQuery as _;

    let root = tempfile::tempdir().unwrap();
    let (sessions, identity) = setup(root.path()).await;
    let ManagedToolClaimOutcome::New(claim) =
        sessions.claim_managed_tool(identity.clone()).await.unwrap()
    else {
        panic!("new claim")
    };
    sessions
        .release_session_ownership(identity.owner.session_id)
        .await
        .unwrap();
    drop(sessions);
    let db = bcode_session::db::SessionDb::open_existing_turso_in_root(
        identity.owner.session_id,
        root.path(),
    )
    .await
    .unwrap();
    let before = db.all_events_strict().await.unwrap();
    db.database()
        .update("managed_tool_claims")
        .value(
            "claim_sequence",
            i64::try_from(claim.claim_sequence + 1).unwrap(),
        )
        .where_eq("invocation_id", identity.invocation_id.clone())
        .execute(db.database())
        .await
        .unwrap();
    db.database().close().await.unwrap();
    drop(db);
    let sessions = manager(root.path());
    assert!(sessions.claim_managed_tool(identity.clone()).await.is_err());
    assert!(
        sessions
            .start_managed_tool(identity.clone(), claim.claim_sequence)
            .await
            .is_err()
    );
    sessions
        .release_session_ownership(identity.owner.session_id)
        .await
        .unwrap();
    drop(sessions);
    let db = bcode_session::db::SessionDb::open_existing_turso_in_root(
        identity.owner.session_id,
        root.path(),
    )
    .await
    .unwrap();
    assert_eq!(db.all_events_strict().await.unwrap(), before);
    db.database().close().await.unwrap();
}
