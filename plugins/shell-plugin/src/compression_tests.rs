//! Real recording producer → reference builder → session store → compression → recording reader.
use super::*;
use bcode_session::artifact_compression::ArtifactCompression;
use bcode_session::artifact_storage::{
    ArtifactStorageOutcome, compress_finalized_artifact, read_artifact_range,
};
use bcode_session_models::{
    SessionEventKind, ToolArtifact, ToolInvocationResult, ToolInvocationResultRecord,
};

#[tokio::test]
async fn current_recording_compresses_and_replays() {
    verify_recording(false, false).await;
}

#[tokio::test]
async fn output_only_checksum_recording_compresses_and_replays() {
    verify_recording(true, false).await;
}

#[tokio::test]
async fn same_length_recording_corruption_is_rejected_without_replacement() {
    verify_recording(false, true).await;
}

async fn verify_recording(legacy: bool, corrupt: bool) {
    let root = tempfile::tempdir().expect("root");
    let sessions = bcode_session::SessionManager::persistent(root.path()).expect("sessions");
    let id = sessions
        .create_session(None, root.path().to_path_buf())
        .await
        .expect("session")
        .id;
    let artifacts = root.path().join("session-artifacts").join(id.to_string());
    std::fs::create_dir_all(&artifacts).expect("artifacts");
    let path = artifacts.join("recording.bcsr");
    let output = "hello 世界\r\n".repeat(2000).into_bytes();
    let mut writer = recording::ShellRecordingWriter::create(&path, 80, 24).expect("writer");
    writer.write_output(1, &output).expect("output");
    writer.write_replay_output(2, &output).expect("replay");
    writer.write_resize(3, 100, 30).expect("resize");
    let summary = writer
        .finish(4, Some(0), None, false, false)
        .expect("finalize");
    let mut reference =
        recording_artifact_ref(&path, &summary, 80, 24).expect("production reference");
    let metadata = reference.metadata.as_mut().expect("metadata");
    assert_ne!(
        metadata["checksum_sha256"],
        metadata["content_checksum_sha256"]
    );
    if legacy {
        // Model the previous producer contract by removing only the newly introduced field.
        metadata
            .as_object_mut()
            .expect("object")
            .remove("content_checksum_sha256");
    }
    let original = std::fs::read(&path).expect("original");
    let (_, expected_frames) = recording::read_recording(&path).expect("original replay");
    persist_reference(&sessions, id, reference).await;
    let history = sessions.session_history(id).await.expect("history");
    sessions
        .release_session_ownership(id)
        .await
        .expect("release");
    drop(sessions);
    if corrupt {
        verify_corruption(root.path(), id, &path, &original).await;
    } else {
        let outcome = compress_finalized_artifact(
            root.path(),
            id,
            "recording",
            SHELL_RECORDING_REF_KEY,
            ArtifactCompression::Light,
            4096,
        )
        .await
        .expect("compress real recording");
        assert!(
            matches!(outcome, ArtifactStorageOutcome::Compressed { saved_bytes, .. } if saved_bytes > 4096)
        );
        let (_, decoded) = read_artifact_range(
            &artifacts,
            &path,
            0,
            u32::try_from(original.len()).expect("length"),
        )
        .expect("transparent read");
        assert_eq!(decoded, original);
        let replay = root.path().join("replay.bcsr");
        std::fs::write(&replay, decoded).expect("decoded recording");
        let (_, frames) = recording::read_recording(&replay).expect("reader validates checksums");
        assert_eq!(frames, expected_frames);
        assert_eq!(
            compress_finalized_artifact(
                root.path(),
                id,
                "recording",
                SHELL_RECORDING_REF_KEY,
                ArtifactCompression::Light,
                4096
            )
            .await
            .expect("repeat"),
            ArtifactStorageOutcome::Unchanged
        );
    }
    let reopened = bcode_session::SessionManager::persistent(root.path()).expect("reopen");
    assert_eq!(
        reopened
            .session_history(id)
            .await
            .expect("unchanged history"),
        history
    );
    reopened
        .release_session_ownership(id)
        .await
        .expect("release");
}

async fn persist_reference(
    sessions: &bcode_session::SessionManager,
    id: bcode_session_models::SessionId,
    reference: ToolArtifactRef,
) {
    let reference =
        serde_json::from_value(serde_json::to_value(reference).expect("encode tool reference"))
            .expect("decode persisted reference contract");
    sessions
        .append_event(
            id,
            SessionEventKind::ToolInvocationResultRecorded {
                record: ToolInvocationResultRecord {
                    invocation_id: "recording-fixture".into(),
                    model_output: "done".into(),
                    is_error: false,
                    presentation: None,
                    content: vec![],
                    result: Some(ToolInvocationResult::Artifact {
                        artifact: Box::new(ToolArtifact {
                            artifact_id: "recording".into(),
                            producer_plugin_id: "bcode.shell".into(),
                            schema: "bcode.shell.recording".into(),
                            schema_version: 3,
                            tool_call_id: None,
                            title: None,
                            metadata: serde_json::Value::Null,
                            refs: vec![reference],
                        }),
                    }),
                },
            },
        )
        .await
        .expect("persist production reference");
}

async fn verify_corruption(
    root: &Path,
    id: bcode_session_models::SessionId,
    path: &Path,
    original: &[u8],
) {
    let mut damaged = original.to_vec();
    let last = damaged.last_mut().expect("nonempty");
    *last ^= 1;
    std::fs::write(path, &damaged).expect("same-length damage");
    let error = compress_finalized_artifact(
        root,
        id,
        "recording",
        SHELL_RECORDING_REF_KEY,
        ArtifactCompression::Light,
        4096,
    )
    .await
    .expect_err("checksum rejection");
    assert_eq!(
        bcode_session::artifact_storage::artifact_failure_reason(&error),
        bcode_session_models::ArtifactCompressionFailureReason::ChecksumMismatch
    );
    assert!(path.is_file());
    assert_eq!(
        std::fs::read(path).expect("preserved damaged original"),
        damaged
    );
}
