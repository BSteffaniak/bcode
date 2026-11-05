use super::*;
use bcode_provider_auth_models::managed::{
    AuthCustodyMode, AuthNamespace, AuthReadiness, ManagedFlowPhase,
};

fn protocol_flow(
    connection: ClientConnection,
    session_id: bcode_session_models::SessionId,
) -> ManagedSessionAuthFlow {
    ManagedSessionAuthFlow {
        connection,
        session_id,
        instance_id: "primary".into(),
        namespace: AuthNamespace {
            plugin_id: "example.native".into(),
            credential_namespace: "/native/config".into(),
        },
        custody: AuthCustodyMode::NativeManaged,
        handle: None,
        revision: None,
        uncertain: false,
        begin_attempted: false,
        terminal_attempted: false,
    }
}

#[cfg(unix)]
#[tokio::test]
async fn dedicated_auth_flow_preserves_caller_and_redacts_sensitive_surface() {
    let directory = tempfile::tempdir().unwrap();
    let socket_directory = directory.path().join("ipc");
    std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700)
        .create(&socket_directory)
        .unwrap();
    let socket_directory = socket_directory.canonicalize().unwrap();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&socket_directory).unwrap().permissions()
        ) & 0o777,
        0o700
    );
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_directory.join("auth.sock"));
    let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
    let session_id = bcode_session_models::SessionId::new();
    let handle = ManagedAuthHandle {
        daemon_epoch: "boot".into(),
        flow_id: 1,
    };
    let snapshot = ManagedFlowSnapshot {
        handle: handle.clone(),
        revision: 1,
        phase: ManagedFlowPhase::Pending,
        readiness: AuthReadiness::LoginRequired,
        needs_sensitive_interaction: true,
    };
    let server = tokio::spawn(async move {
        let mut stream = listener.accept().await.unwrap();
        for (operation, json) in [
            (
                ManagedSessionOperation::AuthBegin,
                serde_json::to_string(&snapshot).unwrap(),
            ),
            (
                ManagedSessionOperation::AuthStatus,
                serde_json::to_string(&Some(SensitiveAuthInteraction::Input {
                    request_id: "sensitive-request".into(),
                }))
                .unwrap(),
            ),
        ] {
            let envelope = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
            let bcode_ipc::Request::ManagedSession { request } =
                bcode_ipc::decode_request(&envelope.payload).unwrap()
            else {
                panic!("expected auth only");
            };
            assert_eq!(request.operation, operation);
            let input: ManagedSessionAuthRequest =
                serde_json::from_str(&request.request_json).unwrap();
            assert_eq!(input.session_id, session_id);
            assert_eq!(input.instance_id, "primary");
            assert!(input.input.is_none());
            assert_eq!(
                input.sensitive_interaction,
                operation == ManagedSessionOperation::AuthStatus
            );
            let response = bcode_ipc::response_envelope(
                envelope.request_id,
                &bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::ManagedSession {
                    response: bcode_ipc::ManagedSessionResponse {
                        schema_version: MANAGED_SESSION_VERSION,
                        response_json: json,
                    },
                }),
            )
            .unwrap();
            bcode_ipc::send_envelope(&mut stream, &response)
                .await
                .unwrap();
        }
    });
    let stream = bcode_ipc::LocalIpcStream::connect(&endpoint).await.unwrap();
    let connection = ClientConnection {
        stream,
        next_request_id: 1,
        client_id: Some(bcode_session_models::ClientId::new()),
        pending_events: std::collections::VecDeque::default(),
        request_timeout: std::time::Duration::from_secs(1),
        reconnect_client: None,
        reconnect_name: std::sync::Arc::from(""),
        verified_daemon: None,
        restore_state: crate::ConnectionRestoreState::default(),
    };
    let mut flow = protocol_flow(connection, session_id);
    assert!(flow.status().await.is_err());
    assert_eq!(flow.begin().await.unwrap().handle, handle);
    assert!(flow.begin().await.is_err());
    let mut rendered = false;
    flow.with_sensitive_interaction(|value| {
        assert_eq!(value.unwrap().request_id(), "sensitive-request");
        rendered = true;
    })
    .await
    .unwrap();
    assert!(rendered);
    server.await.unwrap();
    // A lost exchange permanently fences this channel instead of allowing mutation replay.
    assert!(flow.status().await.is_err());
    assert!(flow.uncertain);
    assert!(flow.cancel().await.is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn readiness_preserves_only_typed_host_failure_code_without_sensitive_message_or_replay() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("ipc");
    std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700)
        .create(&socket)
        .unwrap();
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket.join("auth-error.sock"));
    let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
    let server = tokio::spawn(async move {
        let mut stream = listener.accept().await.unwrap();
        let envelope = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
        let bcode_ipc::Request::ManagedSession { request } =
            bcode_ipc::decode_request(&envelope.payload).unwrap()
        else {
            panic!("expected auth request");
        };
        assert_eq!(request.operation, ManagedSessionOperation::AuthStatus);
        let response = bcode_ipc::response_envelope(
            envelope.request_id,
            &bcode_ipc::Response::Err(bcode_ipc::ErrorResponse::new(
                "managed_session_unsupported",
                "private-native-detail-do-not-forward",
            )),
        )
        .unwrap();
        bcode_ipc::send_envelope(&mut stream, &response)
            .await
            .unwrap();
        // The client must not retry after receiving a rejected preflight.
        assert!(bcode_ipc::recv_envelope(&mut stream).await.is_err());
    });
    let stream = bcode_ipc::LocalIpcStream::connect(&endpoint).await.unwrap();
    let connection = ClientConnection {
        stream,
        next_request_id: 1,
        client_id: Some(bcode_session_models::ClientId::new()),
        pending_events: std::collections::VecDeque::default(),
        request_timeout: std::time::Duration::from_secs(1),
        reconnect_client: None,
        reconnect_name: std::sync::Arc::from(""),
        verified_daemon: None,
        restore_state: crate::ConnectionRestoreState::default(),
    };
    let mut flow = protocol_flow(connection, bcode_session_models::SessionId::new());
    let error = flow.readiness().await.unwrap_err();
    assert!(
        matches!(&error, ClientError::Server { code, .. } if code == "managed_session_unsupported")
    );
    assert!(!error.to_string().contains("private-native-detail"));
    assert!(flow.uncertain);
    assert!(flow.readiness().await.is_err());
    drop(flow);
    server.await.unwrap();
}
