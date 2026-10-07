//! Real session bytes joined to immutable typed workflow dispatch receipts.
use super::*;
use bcode_workflow_store::representation;

struct Fixture {
    _root: tempfile::TempDir,
    state: ServerState,
    request: WorkflowDiagnosticArtifactRequest,
    bytes: Vec<u8>,
    path: std::path::PathBuf,
}

async fn session_fixture() -> (tempfile::TempDir, ServerState, SessionId) {
    let root = tempfile::tempdir().unwrap();
    let sessions = crate::SessionManager::persistent(root.path()).unwrap();
    let session_id = sessions
        .create_session(Some("diagnostic fixture".into()), root.path().to_path_buf())
        .await
        .unwrap()
        .id;
    let store =
        bcode_workflow_store::WorkflowStore::create_coexistence_baseline(root.path()).unwrap();
    let mut state = crate::tests::test_server_state(sessions);
    state.workflow_store = std::sync::Arc::new(std::sync::Mutex::new(store));
    (root, state, session_id)
}

async fn prepare_dispatch(
    state: &ServerState,
    db: &dyn switchy_database::Database,
    session_id: SessionId,
) -> (
    bcode_workflow_store::PreparedAttempt,
    bcode_workflow_store::WorkflowExecutionAuthority,
    String,
) {
    representation::install(db).await.unwrap();
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
    let run = bcode_workflow_store::NewWorkflowRun {
        run_id: "diagnostic-run".into(),
        definition_id: definition_identity.definition_id,
        definition_version: definition_identity.definition_version,
        workspace_snapshot: "fixture".into(),
        parent_session_id: Some(session_id.to_string()),
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
    state
        .workflow_store
        .lock()
        .unwrap()
        .persist_definition(&run.definition_id, run.definition_version, &definition)
        .unwrap();
    representation::admit(db, &run, &definition).await.unwrap();
    let authority = crate::tests::test_workflow_execution_authority();
    representation::attempts::claim_authority(db, &run.run_id, &authority)
        .await
        .unwrap();
    let activation = representation::graph::activation_id(db, &run.run_id, "worker")
        .await
        .unwrap()
        .unwrap();
    let prepared = bcode_workflow_store::PreparedAttempt {
        run_id: run.run_id.clone(),
        node_id: "worker".into(),
        activation_id: activation,
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({"owner":"fixture"}),
        prepared_at_ms: 2,
    };
    let dispatch = representation::attempts::prepare(db, &authority, &prepared)
        .await
        .unwrap();
    (prepared, authority, dispatch)
}

fn publish_artifact(
    state: &ServerState,
    session_id: SessionId,
    dispatch: &str,
    artifact_id: &str,
    bytes: &[u8],
) -> (serde_json::Value, std::path::PathBuf) {
    let cancel = crate::TurnCancelState::default();
    let scope = crate::InvocationScope::new(
        crate::TurnScope::without_events("diagnostic-fixture", crate::TurnGeneration::new(1)),
        dispatch.to_owned(),
    );
    let artifact_root = crate::session_artifact_dir(state, session_id).unwrap();
    let reference = crate::write_session_invocation_artifact(
        &artifact_root,
        dispatch,
        &crate::ToolArtifactWriteRequest {
            invocation_id: dispatch.to_owned(),
            artifact_id: artifact_id.into(),
            content_type: "application/json".into(),
            bytes: bytes.to_vec(),
            metadata: serde_json::json!({"sensitive":true}),
        },
        &cancel,
        scope.artifact_commit_guard(),
    )
    .unwrap();
    let path =
        crate::artifact_reference_path(reference["uri"].as_str().unwrap(), &artifact_root).unwrap();
    (reference, path)
}

async fn fixture() -> Fixture {
    let (root, state, session_id) = session_fixture().await;
    let db = open_sqlite_rusqlite(
        &crate::workflow_store_path(&state),
        &SqliteOpenOptions {
            access: SqliteAccess::ReadWrite,
            allow_uri: false,
            busy_timeout: std::time::Duration::ZERO,
            foreign_keys: true,
            connections: std::num::NonZeroUsize::MIN,
        },
    )
    .unwrap();
    let (prepared, authority, dispatch) = prepare_dispatch(&state, &db, session_id).await;
    assert_eq!(
        list(&state, &prepared.run_id, &dispatch).await,
        Err(Error::Unavailable)
    );
    let artifact_id = "fixture-request";
    let bytes = b"synthetic retained judgement request".to_vec();
    let (reference, path) = publish_artifact(&state, session_id, &dispatch, artifact_id, &bytes);
    let receipt = bcode_workflow_store::DispatchReceipt {
        run_id: prepared.run_id.clone(),
        node_id: prepared.node_id,
        activation_id: prepared.activation_id,
        attempt: 1,
        dispatch_identity: dispatch.clone(),
        admitted_at_ms: 3,
        receipt: serde_json::json!({"diagnostics":{"version":1,"parent_session_id":session_id,
            "artifacts":[{"artifact_id":artifact_id,"byte_len":bytes.len(),"reference":reference}]}}),
    };
    representation::attempts::attach_receipt(&db, &authority, &receipt)
        .await
        .unwrap();
    let request = WorkflowDiagnosticArtifactRequest {
        version: 1,
        run_id: prepared.run_id,
        dispatch_identity: dispatch,
        artifact_id: artifact_id.into(),
        offset: 0,
        length: MAX_WORKFLOW_DIAGNOSTIC_RANGE_BYTES,
    };
    Fixture {
        _root: root,
        state,
        request,
        bytes,
        path,
    }
}

#[tokio::test]
async fn successful_101st_write_is_preserved_and_discovery_reports_overflow() {
    let (root, state, session_id) = session_fixture().await;
    let collected = std::sync::Arc::new(std::sync::Mutex::new(
        crate::WorkflowArtifactReferences::default(),
    ));
    let dispatch = "overflow-dispatch";
    let bytes = b"synthetic overflow artifact";
    let mut last_path = None;
    for index in 0..101 {
        let artifact_id = format!("artifact-{index}");
        let (reference, path) = publish_artifact(&state, session_id, dispatch, &artifact_id, bytes);
        let resolution = crate::ToolArtifactWriteResolution::Written {
            artifact_id,
            byte_len: bytes.len() as u64,
            reference,
        };
        crate::record_workflow_artifact_reference(Some(&collected), &resolution);
        assert!(matches!(
            resolution,
            crate::ToolArtifactWriteResolution::Written { .. }
        ));
        last_path = Some(path);
    }
    let retained = collected.lock().unwrap();
    assert_eq!(retained.references.len(), 100);
    assert_eq!(retained.omitted_artifacts, 1);
    assert_eq!(std::fs::read(last_path.unwrap()).unwrap(), bytes);
    let receipt = bcode_workflow_store::DispatchReceipt {
        run_id: "overflow-run".into(),
        node_id: "worker".into(),
        activation_id: "activation".into(),
        attempt: 1,
        dispatch_identity: dispatch.into(),
        admitted_at_ms: 3,
        receipt: serde_json::json!({"diagnostics": {
            "version":1, "parent_session_id":session_id, "artifacts":retained.references,
            "omitted_artifacts":retained.omitted_artifacts
        }}),
    };
    let discovered = summaries(&receipt, "overflow-run", dispatch).unwrap();
    assert_eq!(discovered.artifacts.len(), 100);
    assert!(discovered.unavailable.is_empty());
    assert_eq!(discovered.omitted_artifacts, Some(1));
    drop(retained);
    drop(state);
    drop(root);
}

#[tokio::test]
async fn persisted_capture_lists_reads_and_fails_closed_on_damaged_bytes() {
    let Fixture {
        _root: root,
        state,
        mut request,
        bytes,
        path,
    } = fixture().await;
    let artifacts = list(&state, &request.run_id, &request.dispatch_identity)
        .await
        .unwrap();
    assert_eq!(artifacts.artifacts.len(), 1);
    assert_eq!(artifacts.artifacts[0].artifact_id, request.artifact_id);
    assert_eq!(
        artifacts.artifacts[0].sha256,
        hex::encode(Sha256::digest(&bytes))
    );
    let exported = read(&state, &request).await.unwrap();
    assert_eq!(exported.bytes, bytes);
    assert_eq!(exported.sha256, artifacts.artifacts[0].sha256);
    assert_eq!(exported.byte_len, bytes.len() as u64);
    assert!(exported.eof);
    request.offset = 2;
    request.length = 5;
    assert_eq!(read(&state, &request).await.unwrap().bytes, bytes[2..7]);
    let mut wrong = request.clone();
    wrong.run_id = "other-run".into();
    assert!(read(&state, &wrong).await.is_err());
    let mut corrupted = bytes.clone();
    corrupted[0] ^= 1;
    std::fs::write(&path, corrupted).unwrap();
    assert_eq!(read(&state, &request).await, Err(Error::VerificationFailed));
    // Discovery reports recorded publication metadata, not current physical availability.
    assert_eq!(
        list(&state, &request.run_id, &request.dispatch_identity)
            .await
            .unwrap(),
        artifacts
    );
    std::fs::remove_file(&path).unwrap();
    assert_eq!(read(&state, &request).await, Err(Error::Unavailable));
    #[cfg(unix)]
    {
        let outside = root.path().join("outside.bin");
        std::fs::write(&outside, &bytes).unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert_eq!(read(&state, &request).await, Err(Error::Unavailable));
    }
}
