//! Isolated IPC regression tests for CLI reads without a live session owner.
use super::*;

const CHILD_ENV: &str = "BCODE_CLI_OWNERLESS_READ_CHILD";

#[test]
fn ownerless_history_reads_use_supported_read_client() {
    if std::env::var_os(CHILD_ENV).is_some() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    #[cfg(unix)]
    command.env("BCODE_SOCKET", root.path().join("history.sock"));
    #[cfg(not(unix))]
    command.env_remove("BCODE_SOCKET");
    let output = command
        .args([
            "--exact",
            "session_read_cli_tests::ownerless_history_read_child",
            "--nocapture",
        ])
        .env(CHILD_ENV, "1")
        .env("BCODE_STATE_DIR", root.path())
        .env("BCODE_SESSION_STORE_DIR", root.path().join("sessions"))
        .env_remove("BCODE_IPC_ENDPOINT")
        .env_remove("BCODE_IPC_ENDPOINT_NAMESPACE")
        .env_remove("BCODE_DAEMON_LOG")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "isolated history reads failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn reply(
    stream: &mut bcode_ipc::LocalIpcStream,
    request_id: u64,
    response: bcode_ipc::Response,
) {
    bcode_ipc::send_envelope(
        stream,
        &bcode_ipc::response_envelope(request_id, &response).unwrap(),
    )
    .await
    .unwrap();
}

async fn hello(listener: &bcode_ipc::LocalIpcListener) -> bcode_ipc::LocalIpcStream {
    let mut stream = listener.accept().await.unwrap();
    let request = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
    let daemon = bcode_ipc::DaemonStatus {
        namespace: bcode_ipc::daemon_namespace(),
        protocol_version: u32::from(bcode_ipc::CURRENT_PROTOCOL_VERSION),
        artifact_id: Some(bcode_ipc::ArtifactId::current()),
        executable_digest: Some(
            bcode_daemon_lifecycle::current_executable_identity()
                .unwrap()
                .1,
        ),
        build_fingerprint: bcode_ipc::BUILD_FINGERPRINT.into(),
        storage_writer_epoch: Some(bcode_ipc::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
        session_event_schema_version: Some(
            bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        ),
        state_location_id: Some(bcode_ipc::state_location_id()),
        ..Default::default()
    };
    reply(
        &mut stream,
        request.request_id,
        bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::Hello {
            protocol_version: bcode_ipc::ProtocolVersion(bcode_ipc::CURRENT_PROTOCOL_VERSION),
            client_id: bcode_session_models::ClientId::new(),
            daemon,
        }),
    )
    .await;
    stream
}

fn history_payload(
    request: &bcode_ipc::Request,
    session_id: SessionId,
    inspection: bool,
) -> bcode_ipc::ResponsePayload {
    match request {
        bcode_ipc::Request::SessionHistoryAround {
            session_id: actual,
            query,
        } => {
            assert!(!inspection);
            assert_eq!(*actual, session_id);
            assert_eq!(query.sequence, 42);
            assert_eq!(query.before, 2);
            assert_eq!(query.after, 3);
            bcode_ipc::ResponsePayload::SessionHistoryAround {
                window: bcode_session_models::SessionHistoryWindow {
                    session_id,
                    requested_sequence: 42,
                    events: Vec::new(),
                    anchor_present: false,
                    first_available_sequence: Some(1),
                    last_available_sequence: Some(100),
                    compatibility_issues: Vec::new(),
                },
            }
        }
        bcode_ipc::Request::SessionCompactionInspection {
            session_id: actual,
            query,
        } => {
            assert!(!inspection);
            assert_eq!(*actual, session_id);
            assert_eq!(query.before, Some(SessionHistoryCursor { sequence: 20 }));
            assert_eq!(query.limit, 5);
            bcode_ipc::ResponsePayload::SessionCompactionInspection {
                page: bcode_session_models::SessionCompactionInspectionPage {
                    version: 1,
                    session_id,
                    entries: Vec::new(),
                    scanned_events: 5,
                    degraded: false,
                    next_cursor: Some(SessionHistoryCursor { sequence: 10 }),
                    has_more: true,
                },
            }
        }
        bcode_ipc::Request::SessionInspection {
            session_id: actual,
            query,
        } => {
            assert!(inspection);
            assert_eq!(*actual, session_id);
            assert_eq!(query.category, SessionInspectionCategory::FailedToolCalls);
            assert_eq!(query.cursor, Some(SessionHistoryCursor { sequence: 20 }));
            assert_eq!(query.limit, 5);
            assert_eq!(query.direction, SessionHistoryDirection::Backward);
            bcode_ipc::ResponsePayload::SessionInspection {
                page: bcode_session_models::SessionInspectionPage {
                    session_id,
                    category: query.category,
                    events: Vec::new(),
                    scanned_events: 5,
                    compatibility_issues: Vec::new(),
                    next_cursor: Some(SessionHistoryCursor { sequence: 15 }),
                    has_more: true,
                },
            }
        }
        other => panic!("unexpected request: {other:?}"),
    }
}

#[tokio::test]
async fn ownerless_history_read_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let session_id = SessionId::new();
    assert!(
        bcode_session::lease::active_session_owners(
            &bcode_config::default_session_store_dir(),
            session_id,
        )
        .unwrap()
        .is_empty()
    );
    let endpoint = bcode_ipc::default_endpoint();
    let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
    let server = tokio::spawn(async move {
        for fail in [false, true] {
            for inspection in [false, true, false] {
                let mut stream = hello(&listener).await;
                let envelope = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
                let request = bcode_ipc::decode_request(&envelope.payload).unwrap();
                let payload = history_payload(&request, session_id, inspection);
                let response = if fail {
                    bcode_ipc::Response::Err(bcode_ipc::ErrorResponse::new(
                        "repair_required",
                        "history requires explicit repair",
                    ))
                } else {
                    bcode_ipc::Response::Ok(payload)
                };
                reply(&mut stream, envelope.request_id, response).await;
            }
        }
    });
    let operations = async {
        for fail in [false, true] {
            let around = session_around(session_id, 42, 2, 3, true).await;
            let inspect = session_inspect(
                session_id,
                SessionInspectionCategoryArg::FailedToolCalls,
                None,
                Some(20),
                5,
                true,
            )
            .await;
            let compaction = session_compaction_status(session_id, Some(20), 5, true).await;
            for result in [around, inspect, compaction] {
                if fail {
                    let error = result.unwrap_err().to_string();
                    assert!(
                        error.contains("history requires explicit repair"),
                        "{error}"
                    );
                } else {
                    result.unwrap();
                }
            }
        }
        server.await.unwrap();
    };
    tokio::time::timeout(std::time::Duration::from_secs(15), operations)
        .await
        .expect("bounded IPC history tests timed out");
    assert!(
        bcode_session::lease::active_session_owners(
            &bcode_config::default_session_store_dir(),
            session_id,
        )
        .unwrap()
        .is_empty()
    );
}
