// Deterministic coverage of the producer-registration / presentation-queue boundary.

#[cfg(unix)]
#[tokio::test]
async fn same_session_shell_batch_finalizes_recordings_before_presentation_drain() {
    let workspace = tempfile::tempdir().expect("shell lifecycle workspace");
    let sessions = SessionManager::persistent(workspace.path().join("sessions")).unwrap();
    let session_id = sessions
        .create_session(None, workspace.path().to_path_buf())
        .await
        .unwrap()
        .id;
    let state = Arc::new(test_server_state_with_shell_plugin(sessions));
    let (_followups, mut followup_rx) = mpsc::channel(1);
    let (_steering, mut steering_rx) = mpsc::channel(1);
    let (_cancel, mut cancel_rx) = mpsc::channel(1);
    let queued = ActivityCounter::new(0);
    let mut context = RuntimeCommandContext::new(
        &mut followup_rx,
        &mut steering_rx,
        &mut cancel_rx,
        &queued,
        Arc::new(Mutex::new(None)),
    );
    let calls = (0..4).map(|index| bcode_model::ToolCall {
        id: format!("shell-lifecycle-{index}"), name: "shell.run".into(),
        arguments: serde_json::json!({"command":"printf 'lifecycle recording\\n'", "timeout_ms":10000}),
    }).collect();
    let outcome = execute_model_tool_batch(
        &state,
        session_id,
        calls,
        &BTreeMap::new(),
        Arc::new(TurnCancelState::default()),
        &mut context,
        &bcode_session_models::TurnExecutionOptions {
            permission_mode: bcode_session_models::TurnPermissionMode::Bypass,
            ..bcode_session_models::TurnExecutionOptions::default()
        },
        bcode_tool::ToolExecutionOptions::default(),
        None,
    )
    .await;
    assert_eq!(outcome, tool_host_retry::ToolBatchOutcome::Completed);
    assert_eq!(
        active_artifact_snapshot_events(&state, session_id).unwrap(),
        Vec::<bcode_session_models::SessionLiveEvent>::new()
    );
    let history = state.sessions.session_history(session_id).await.unwrap();
    let mut recordings = 0;
    for event in &history {
        if let SessionEventKind::ToolInvocationResultRecorded { record } = &event.kind {
            assert!(!record.is_error, "{}", record.model_output);
            if let Some(ToolInvocationResult::Artifact { artifact }) = &record.result {
                let range = read_session_artifact_range(
                    &state,
                    session_id,
                    &artifact.artifact_id,
                    "shell_recording",
                    0,
                    65536,
                )
                .await
                .unwrap();
                assert!(range.finalized);
                assert_eq!(range.finalized_event_seq, Some(event.sequence));
                assert_eq!(&range.bytes[..8], b"BCSHREC\0");
                recordings += 1;
            }
        }
    }
    assert_eq!(recordings, 4);
    drop(context);
    drop(state);
}
struct ArtifactLifecycleFixture {
    _directory: tempfile::TempDir,
    state: ServerState,
    session_id: SessionId,
    registration: Option<ActivePluginInvocationRegistration>,
    update: bcode_tool::ToolPresentationUpdate,
}

impl ArtifactLifecycleFixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().expect("artifact lifecycle workspace");
        let sessions = SessionManager::persistent(directory.path()).expect("sessions");
        let session_id = sessions
            .create_session(None, test_working_directory())
            .await
            .expect("session")
            .id;
        let state = test_server_state(sessions);
        let root = session_artifact_dir(&state, session_id).expect("artifact root");
        std::fs::create_dir_all(&root).expect("artifact directory");
        let path = root.join("recording.bin");
        std::fs::write(&path, b"canonical").expect("recording");
        let registration = ActivePluginInvocationRegistration::register(
            Arc::clone(&state.active_plugin_invocations),
            Arc::clone(&state.active_artifacts),
            session_id,
            "call-lifecycle",
            ActivePluginInvocation {
                producer_plugin_id: "fixture.plugin".into(),
                inputs: mpsc::channel(ACTIVE_INVOCATION_INPUT_QUEUE_CAPACITY).0,
            },
            None,
        )
        .expect("producer registration");
        let update = bcode_tool::ToolPresentationUpdate {
            invocation_id: "call-lifecycle".into(),
            producer_id: "fixture.plugin".into(),
            generation: 0,
            revision: 1,
            identity: bcode_tool::ToolPresentationIdentity::Primary,
            retention: bcode_tool::ToolPresentationRetention::RetainLatest,
            schema: "fixture.recording".into(),
            schema_version: 1,
            artifact: Some(bcode_tool::ToolContributionArtifact {
                artifact_id: "artifact-lifecycle".into(),
                reference_key: "recording".into(),
                content_type: None,
                storage_uri: url::Url::from_file_path(path)
                    .expect("file URI")
                    .to_string(),
                committed_bytes: 9,
                revision: 9,
                finalized: true,
                availability: None,
            }),
            payload: serde_json::Value::Null,
        };
        Self {
            _directory: directory,
            state,
            session_id,
            registration: Some(registration),
            update,
        }
    }

    async fn commit(&self) {
        let reference = self.update.artifact.as_ref().expect("artifact");
        append_tool_finished_event_inner(
            &self.state,
            self.session_id,
            ToolFinishedEventInput {
                tool_call_id: self.update.invocation_id.clone(),
                result: "done".into(),
                is_error: false,
                content: vec![],
                semantic_result: Some(ToolInvocationResult::Artifact {
                    artifact: Box::new(bcode_session_models::ToolArtifact {
                        artifact_id: reference.artifact_id.clone(),
                        producer_plugin_id: self.update.producer_id.clone(),
                        schema: self.update.schema.clone(),
                        schema_version: 1,
                        tool_call_id: Some(self.update.invocation_id.clone()),
                        title: None,
                        metadata: serde_json::Value::Null,
                        refs: vec![bcode_session_models::ToolArtifactRef {
                            key: reference.reference_key.clone(),
                            content_type: None,
                            storage_uri: Some(reference.storage_uri.clone()),
                            byte_len: Some(9),
                            metadata: Some(
                                serde_json::json!({"availability":"complete", "complete":true}),
                            ),
                        }],
                    }),
                }),
            },
        )
        .await
        .expect("canonical commit");
    }

    async fn read(&self) -> bcode_session_models::SessionArtifactRange {
        read_session_artifact_range(
            &self.state,
            self.session_id,
            "artifact-lifecycle",
            "recording",
            0,
            9,
        )
        .await
        .expect("read artifact")
    }
}

#[tokio::test]
async fn artifact_finalization_bypasses_full_undrained_presentation_queue() {
    let mut fixture = ArtifactLifecycleFixture::new().await;
    let (sink, receiver) = SessionInvocationSink::new(1);
    let turn = TurnScope::new("artifact-finalization", TurnGeneration::new(1), sink);
    let scope = InvocationScope::new(turn, "call-lifecycle");
    let ordinary = bcode_tool::ToolPresentationUpdate {
        artifact: None,
        ..fixture.update.clone()
    };
    assert!(scope.emit_presentation_update(ordinary));
    // No consumer runs until after registration release; a second hint fills/overflows the sink.
    assert!(
        scope.emit_presentation_update(bcode_tool::ToolPresentationUpdate {
            artifact: None,
            revision: 2,
            ..fixture.update.clone()
        })
    );
    route_scoped_plugin_presentation_update(
        &fixture.state,
        fixture.session_id,
        "call-lifecycle",
        "fixture.plugin",
        &scope,
        bcode_tool::ToolPresentationUpdate {
            revision: 3,
            ..fixture.update.clone()
        },
    )
    .await;
    drop(fixture.registration.take());
    let range = fixture.read().await;
    assert_eq!(range.bytes, b"canonical");
    assert!(range.finalized);
    assert!(range.finalized_event_seq.is_none());
    assert!(active_artifact_snapshot_events(&fixture.state, fixture.session_id).unwrap()
        .iter().all(|event| matches!(&event.kind,
            SessionLiveEventKind::ToolContributionPlaced { envelope }
                if envelope.contribution.artifact.as_ref().is_some_and(|a| a.finalized && a.availability.is_none())
        )));
    // Old queued revisions must not overwrite the admitted final presentation.
    drop(scope);
    drain_session_invocation_sink(&fixture.state, fixture.session_id, receiver).await;
    drop(fixture);
}

#[tokio::test]
async fn canonical_commit_retires_abandoned_prefix_and_late_updates_cannot_reopen_it() {
    let mut fixture = ArtifactLifecycleFixture::new().await;
    let mut prefix = fixture.update.clone();
    let artifact = prefix.artifact.as_mut().unwrap();
    artifact.finalized = false;
    artifact.committed_bytes = 3;
    artifact.revision = 3;
    publish_plugin_tool_presentation_update(
        &fixture.state,
        fixture.session_id,
        "call-lifecycle",
        "fixture.plugin",
        prefix,
    )
    .await
    .unwrap();
    drop(fixture.registration.take());
    assert_eq!(
        active_artifact_snapshot_events(&fixture.state, fixture.session_id)
            .unwrap()
            .len(),
        1
    );
    fixture.commit().await;
    assert_eq!(
        active_artifact_snapshot_events(&fixture.state, fixture.session_id).unwrap(),
        Vec::<bcode_session_models::SessionLiveEvent>::new()
    );
    let range = fixture.read().await;
    assert_eq!(range.bytes, b"canonical");
    assert!(range.finalized_event_seq.is_some());
    assert!(
        publish_plugin_tool_presentation_update(
            &fixture.state,
            fixture.session_id,
            "call-lifecycle",
            "fixture.plugin",
            bcode_tool::ToolPresentationUpdate {
                revision: 100,
                ..fixture.update.clone()
            }
        )
        .await
        .is_err()
    );
    assert_eq!(
        active_artifact_snapshot_events(&fixture.state, fixture.session_id).unwrap(),
        Vec::<bcode_session_models::SessionLiveEvent>::new()
    );
    drop(fixture);
}

#[tokio::test]
async fn canonical_read_supersedes_leftover_abandoned_live_entry() {
    let mut fixture = ArtifactLifecycleFixture::new().await;
    let mut prefix = fixture.update.clone();
    prefix.artifact.as_mut().unwrap().finalized = false;
    publish_plugin_tool_presentation_update(
        &fixture.state,
        fixture.session_id,
        "call-lifecycle",
        "fixture.plugin",
        prefix,
    )
    .await
    .unwrap();
    drop(fixture.registration.take());
    // Preserve a stale entry to reproduce state left by an older cleanup path.
    let stale = fixture.state.active_artifacts.lock().unwrap().clone();
    fixture.commit().await;
    fixture.state.active_artifacts.lock().unwrap().extend(stale);
    let range = fixture.read().await;
    assert_eq!(range.bytes, b"canonical");
    assert!(range.finalized_event_seq.is_some());
    assert_eq!(
        active_artifact_snapshot_events(&fixture.state, fixture.session_id).unwrap(),
        Vec::<bcode_session_models::SessionLiveEvent>::new()
    );
    drop(fixture);
}

#[tokio::test]
async fn cancelled_scope_does_not_admit_finalization_or_reopen_abandoned_recording() {
    let mut fixture = ArtifactLifecycleFixture::new().await;
    let turn = TurnScope::without_events("cancelled-artifact", TurnGeneration::new(1));
    let scope = InvocationScope::new(turn.clone(), "call-lifecycle");
    let mut prefix = fixture.update.clone();
    prefix.artifact.as_mut().unwrap().finalized = false;
    publish_plugin_tool_presentation_update(
        &fixture.state,
        fixture.session_id,
        "call-lifecycle",
        "fixture.plugin",
        prefix,
    )
    .await
    .unwrap();
    assert!(turn.control().begin_cancellation());
    route_scoped_plugin_presentation_update(
        &fixture.state,
        fixture.session_id,
        "call-lifecycle",
        "fixture.plugin",
        &scope,
        bcode_tool::ToolPresentationUpdate {
            revision: 2,
            ..fixture.update.clone()
        },
    )
    .await;
    drop(fixture.registration.take());
    let error = read_session_artifact_range(
        &fixture.state,
        fixture.session_id,
        "artifact-lifecycle",
        "recording",
        0,
        9,
    )
    .await
    .unwrap_err();
    assert_eq!(
        artifact_operations::classify_internal_error(&error),
        artifact_operations::ReadArtifactError::Unavailable
    );
    drop(scope);
    drop(turn);
    drop(fixture);
}

#[tokio::test]
async fn canonical_read_supersedes_still_active_prefix() {
    let fixture = ArtifactLifecycleFixture::new().await;
    let mut prefix = fixture.update.clone();
    let artifact = prefix.artifact.as_mut().unwrap();
    artifact.finalized = false;
    artifact.committed_bytes = 3;
    publish_plugin_tool_presentation_update(
        &fixture.state,
        fixture.session_id,
        "call-lifecycle",
        "fixture.plugin",
        prefix,
    )
    .await
    .unwrap();
    let stale = fixture.state.active_artifacts.lock().unwrap().clone();
    fixture.commit().await;
    fixture.state.active_artifacts.lock().unwrap().extend(stale);
    let range = fixture.read().await;
    assert_eq!(range.bytes, b"canonical");
    assert_eq!(range.reference_bytes, Some(9));
    assert!(range.finalized_event_seq.is_some());
    drop(fixture);
}

#[tokio::test]
async fn rejected_presentation_identity_never_registers_an_artifact() {
    let fixture = ArtifactLifecycleFixture::new().await;
    assert!(
        publish_plugin_tool_presentation_update(
            &fixture.state,
            fixture.session_id,
            "different-invocation",
            "fixture.plugin",
            fixture.update.clone()
        )
        .await
        .is_err()
    );
    assert_eq!(
        active_artifact_snapshot_events(&fixture.state, fixture.session_id).unwrap(),
        Vec::<bcode_session_models::SessionLiveEvent>::new()
    );
    assert!(
        publish_plugin_tool_presentation_update(
            &fixture.state,
            fixture.session_id,
            "call-lifecycle",
            "different-producer",
            fixture.update.clone()
        )
        .await
        .is_err()
    );
    assert_eq!(
        active_artifact_snapshot_events(&fixture.state, fixture.session_id).unwrap(),
        Vec::<bcode_session_models::SessionLiveEvent>::new()
    );
    drop(fixture);
}
