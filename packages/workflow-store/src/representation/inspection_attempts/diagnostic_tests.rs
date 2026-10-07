//! Production typed admission/attempt/receipt lookup coverage; no direct SQL.
use super::*;

async fn prepare_fixture(
    db: &dyn Database,
) -> (
    crate::WorkflowExecutionAuthority,
    crate::PreparedAttempt,
    String,
) {
    let representation = db;
    crate::representation::install(representation)
        .await
        .unwrap();
    let definition = bcode_workflow::WorkflowBuilder::new(
        "diagnostic-fixture",
        bcode_workflow::Step::task("worker", |value: u32, _| async move { Ok(value) }),
    )
    .build()
    .unwrap()
    .definition()
    .clone();
    let definition_identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
        "diagnostic-fixture",
        &definition,
    )
    .unwrap();
    let authority = crate::WorkflowExecutionAuthority {
        target_artifact_id: "fixture-artifact".into(),
        daemon_instance_id: "fixture-daemon".into(),
        generation: 1,
        fencing_token: "fixture-fence".into(),
    };
    for run_id in ["diagnostic-run", "other-run"] {
        let run = crate::NewWorkflowRun {
            run_id: run_id.into(),
            definition_id: definition_identity.definition_id.clone(),
            definition_version: definition_identity.definition_version,
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
        crate::representation::admit(representation, &run, &definition)
            .await
            .unwrap();
        crate::representation::attempts::claim_authority(representation, run_id, &authority)
            .await
            .unwrap();
    }
    let activation =
        crate::representation::graph::activation_id(representation, "diagnostic-run", "worker")
            .await
            .unwrap()
            .unwrap();
    let prepared = crate::PreparedAttempt {
        run_id: "diagnostic-run".into(),
        node_id: "worker".into(),
        activation_id: activation,
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({"owner":"fixture"}),
        prepared_at_ms: 2,
    };
    let identity = crate::representation::attempts::prepare(representation, &authority, &prepared)
        .await
        .unwrap();
    (authority, prepared, identity)
}

#[tokio::test]
async fn exact_lookup_preserves_persisted_receipt_and_rejects_cross_coordinates() {
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
    let representation = &db;
    let (authority, prepared, identity) = prepare_fixture(representation).await;
    assert!(
        diagnostic_receipt(representation, "diagnostic-run", &identity)
            .await
            .unwrap()
            .is_none()
    );
    let receipt = crate::DispatchReceipt {
        run_id: prepared.run_id.clone(),
        node_id: prepared.node_id.clone(),
        activation_id: prepared.activation_id.clone(),
        attempt: 1,
        dispatch_identity: identity.clone(),
        receipt: serde_json::json!({"diagnostics":{"version":1,"artifacts":[]}}),
        admitted_at_ms: 3,
    };
    crate::representation::attempts::attach_receipt(representation, &authority, &receipt)
        .await
        .unwrap();
    assert_eq!(
        diagnostic_receipt(representation, "diagnostic-run", &identity)
            .await
            .unwrap(),
        Some(receipt.clone())
    );
    assert!(
        diagnostic_receipt(representation, "other-run", &identity)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        diagnostic_receipt(representation, "diagnostic-run", "missing-dispatch")
            .await
            .unwrap()
            .is_none()
    );
    let mut conflicting = receipt;
    conflicting.node_id = "other-node".into();
    assert!(
        crate::representation::attempts::attach_receipt(representation, &authority, &conflicting)
            .await
            .is_err()
    );
    assert!(
        diagnostic_receipt(representation, "", &identity)
            .await
            .is_err()
    );
}
