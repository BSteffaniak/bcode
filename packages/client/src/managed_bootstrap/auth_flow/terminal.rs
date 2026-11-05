//! Explicit native terminal login lifecycle on the existing sealed auth caller connection.
use super::{ManagedSessionAuthFlow, invalid};
use crate::ClientError;
use bcode_ipc::harness::{
    ManagedNativeLoginPlan, ManagedSessionHandoffRequest, ManagedSessionOperation as Op,
    ManagedSessionPrepareHandoffRequest,
};
use bcode_provider_auth_models::managed::{
    ManagedFlowPhase, ManagedFlowSnapshot, SensitiveAuthInteraction,
};
use std::{io::IsTerminal as _, process::Stdio};

impl ManagedSessionAuthFlow {
    /// Execute supported native login on an explicitly yielded client terminal.
    ///
    /// The caller must obtain explicit user consent and suspend its TUI terminal ownership before
    /// calling, then restore ownership on every return. No credentials or launch plan enter UI
    /// serialization. Returns `(child_exit_success, reinspected_flow)`; neither child exit nor
    /// reaping establishes readiness. Only the returned host observation describes readiness.
    /// # Errors
    /// Rejects non-terminals, duplicate preparation, unsupported custody, mismatched namespace,
    /// executable/version failures, transport loss and uncertain lifecycle. Never retries login.
    pub async fn terminal_handoff(
        &mut self,
        interface_id: String,
    ) -> Result<(bool, ManagedFlowSnapshot), ClientError> {
        if !std::io::stdin().is_terminal()
            || !std::io::stdout().is_terminal()
            || self.handle.is_some()
            || self.begin_attempted
            || self.terminal_attempted
            || self.uncertain
            || self.custody != bcode_provider_auth_models::managed::AuthCustodyMode::NativeManaged
        {
            return Err(invalid());
        }
        // Retain the fence even if this future is dropped between framed exchanges or during
        // executable verification. Generic auth input/cancel must never replace terminal custody.
        self.terminal_attempted = true;
        let result = self.run_terminal(interface_id).await;
        if result.is_err() {
            self.uncertain = true;
        }
        result
    }

    async fn run_terminal(
        &mut self,
        interface_id: String,
    ) -> Result<(bool, ManagedFlowSnapshot), ClientError> {
        let (selection, plan) = self.prepare_terminal(interface_id).await?;
        verify_executable(&plan).await?;
        // Cancellation/spawn/wait failure cannot authorize replay or claim the child was reaped.
        self.uncertain = true;
        let status = tokio::process::Command::new(&plan.executable)
            .args(&plan.arguments)
            .env_clear()
            .envs(&plan.environment)
            .current_dir(&plan.working_directory)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .status()
            .await
            .map_err(|_| invalid())?;
        self.uncertain = false;
        self.finish_terminal(&selection, status.success()).await
    }

    async fn finish_terminal(
        &mut self,
        selection: &ManagedSessionHandoffRequest,
        child_exit_success: bool,
    ) -> Result<(bool, ManagedFlowSnapshot), ClientError> {
        let payload = serde_json::to_string(selection).map_err(|_| invalid())?;
        let reaped: serde_json::Value = self.exchange(Op::AuthReaped, payload.clone()).await?;
        require_reaped(&reaped)?;
        let snapshot: ManagedFlowSnapshot =
            self.exchange(Op::AuthContinue, payload.clone()).await?;
        let snapshot = self.accept(snapshot)?;
        if snapshot.phase == ManagedFlowPhase::Settled {
            let acknowledgement: serde_json::Value =
                self.exchange(Op::AuthAcknowledge, payload).await?;
            require_acknowledged(&acknowledgement)?;
        }
        Ok((child_exit_success, snapshot))
    }

    async fn prepare_terminal(
        &mut self,
        interface_id: String,
    ) -> Result<(ManagedSessionHandoffRequest, ManagedNativeLoginPlan), ClientError> {
        if interface_id.is_empty()
            || interface_id.len() > 256
            || interface_id.chars().any(char::is_control)
        {
            return Err(invalid());
        }
        self.begin_attempted = true;
        let request = ManagedSessionPrepareHandoffRequest {
            session_id: self.session_id,
            instance_id: self.instance_id.clone(),
            interface_id: interface_id.clone(),
        };
        let snapshot: ManagedFlowSnapshot = self
            .exchange(
                Op::AuthPrepare,
                serde_json::to_string(&request).map_err(|_| invalid())?,
            )
            .await?;
        let snapshot = self.accept(snapshot)?;
        if snapshot.phase != ManagedFlowPhase::Pending || !snapshot.needs_sensitive_interaction {
            return Err(invalid());
        }
        let mut selection = ManagedSessionHandoffRequest {
            session_id: self.session_id,
            instance_id: self.instance_id.clone(),
            interface_id,
            handle: snapshot.handle,
            interaction_id: String::new(),
        };
        let interaction: Option<(u64, SensitiveAuthInteraction)> = self
            .exchange(
                Op::AuthStatus,
                serde_json::to_string(&selection).map_err(|_| invalid())?,
            )
            .await?;
        let Some((revision, interaction @ SensitiveAuthInteraction::NativeHandoff { .. })) =
            interaction
        else {
            return Err(invalid());
        };
        if !interaction.is_valid() || self.revision.is_some_and(|previous| revision < previous) {
            self.uncertain = true;
            return Err(invalid());
        }
        self.revision = Some(revision);
        selection.interaction_id = interaction.request_id().to_owned();
        let plan: ManagedNativeLoginPlan = self
            .exchange(
                Op::AuthHandoff,
                serde_json::to_string(&selection).map_err(|_| invalid())?,
            )
            .await?;
        plan.validate().map_err(|_| invalid())?;
        if plan.namespace != self.namespace || plan.method.custody != self.custody {
            self.uncertain = true;
            return Err(invalid());
        }
        Ok((selection, plan))
    }
}

fn require_reaped(value: &serde_json::Value) -> Result<(), ClientError> {
    if value.get("reaped").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(invalid());
    }
    Ok(())
}

fn require_acknowledged(value: &serde_json::Value) -> Result<(), ClientError> {
    if value
        .get("acknowledged")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return Err(invalid());
    }
    Ok(())
}

async fn verify_executable(plan: &ManagedNativeLoginPlan) -> Result<(), ClientError> {
    use tokio::io::AsyncReadExt as _;
    let mut child = tokio::process::Command::new(&plan.executable)
        .arg("--version")
        .env_clear()
        .envs(&plan.environment)
        .current_dir(&plan.working_directory)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| invalid())?;
    let mut stdout = child.stdout.take().ok_or_else(invalid)?;
    let verified = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut bytes = Vec::new();
        (&mut stdout)
            .take(4097)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| invalid())?;
        if bytes.len() > 4096 {
            return Err(invalid());
        }
        let status = child.wait().await.map_err(|_| invalid())?;
        Ok(status.success()
            && std::str::from_utf8(&bytes).is_ok_and(|text| {
                text.trim() == plan.expected_version
                    || text
                        .split_whitespace()
                        .any(|part| part == plan.expected_version)
            }))
    })
    .await;
    if !matches!(verified, Ok(Ok(true))) {
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn native_terminal_reinspects_independently_of_child_exit() {
        use bcode_provider_auth_models::managed::AuthReadiness;
        for child_exit_success in [false, true] {
            for readiness in [AuthReadiness::LoginRequired, AuthReadiness::Ready] {
                terminal_completion(child_exit_success, readiness).await;
            }
        }
    }

    async fn terminal_completion(
        child_exit_success: bool,
        readiness: bcode_provider_auth_models::managed::AuthReadiness,
    ) {
        use bcode_provider_auth_models::managed::{
            AuthNamespace, AuthReadiness, ManagedAuthHandle,
        };
        let (_directory, endpoint, listener) = terminal_socket();
        let session_id = bcode_session_models::SessionId::new();
        let namespace = AuthNamespace {
            plugin_id: "example.native".into(),
            credential_namespace: "/native/config".into(),
        };
        let handle = ManagedAuthHandle {
            daemon_epoch: "boot".into(),
            flow_id: 1,
        };
        let pending = ManagedFlowSnapshot {
            handle: handle.clone(),
            revision: 1,
            phase: ManagedFlowPhase::Pending,
            readiness: AuthReadiness::LoginRequired,
            needs_sensitive_interaction: true,
        };
        let settled = ManagedFlowSnapshot {
            revision: 2,
            phase: ManagedFlowPhase::Settled,
            readiness,
            needs_sensitive_interaction: false,
            ..pending.clone()
        };
        let plan = serde_json::json!({
            "schema_version": 1, "namespace": namespace,
            "method": {"method_id": "native-login", "support": "supported", "custody": "native_managed", "execution": "client_terminal", "native_inactivity": "inactive"},
            "executable": "/native/executable", "expected_version": "1.0.0",
            "working_directory": "/native", "arguments": ["login"], "environment": {}
        });
        let observed = settled.clone();
        let server = tokio::spawn(serve_terminal_completion(
            listener, session_id, pending, settled, plan,
        ));
        let stream = bcode_ipc::LocalIpcStream::connect(&endpoint).await.unwrap();
        let mut flow = terminal_flow(stream, session_id, namespace);
        // Exercise the framed lifecycle without executing a native program or taking a terminal.
        let (selection, _) = flow
            .prepare_terminal("native.login/v1".into())
            .await
            .unwrap();
        assert!(flow.begin().await.is_err());
        assert!(flow.cancel().await.is_err());
        assert!(flow.status().await.is_err());
        let (exit_success, snapshot) = flow
            .finish_terminal(&selection, child_exit_success)
            .await
            .unwrap();
        assert_eq!(exit_success, child_exit_success);
        assert_eq!(snapshot, observed);
        assert_eq!(snapshot.readiness, readiness);
        server.await.unwrap();
    }

    async fn serve_terminal_completion(
        listener: bcode_ipc::LocalIpcListener,
        session_id: bcode_session_models::SessionId,
        pending: ManagedFlowSnapshot,
        settled: ManagedFlowSnapshot,
        plan: serde_json::Value,
    ) {
        let mut stream = listener.accept().await.unwrap();
        for (operation, response) in [
            (Op::AuthPrepare, serde_json::to_value(&pending).unwrap()),
            (
                Op::AuthStatus,
                serde_json::json!([1, {"kind": "native_handoff", "request_id": "native-request"}]),
            ),
            (Op::AuthHandoff, plan),
            (Op::AuthReaped, serde_json::json!({"reaped": true})),
            (Op::AuthContinue, serde_json::to_value(&settled).unwrap()),
            (
                Op::AuthAcknowledge,
                serde_json::json!({"acknowledged": true}),
            ),
        ] {
            let envelope = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
            let bcode_ipc::Request::ManagedSession { request } =
                bcode_ipc::decode_request(&envelope.payload).unwrap()
            else {
                panic!("only sealed managed auth requests allowed")
            };
            assert_eq!(request.operation, operation);
            let payload: serde_json::Value = serde_json::from_str(&request.request_json).unwrap();
            assert_eq!(payload["session_id"], serde_json::json!(session_id));
            assert_eq!(payload["instance_id"], "primary");
            assert_eq!(payload["interface_id"], "native.login/v1");
            if operation != Op::AuthPrepare {
                assert_eq!(payload["handle"], serde_json::json!(pending.handle));
            }
            if operation != Op::AuthPrepare && operation != Op::AuthStatus {
                assert_eq!(payload["interaction_id"], "native-request");
            }
            let response = bcode_ipc::response_envelope(
                envelope.request_id,
                &bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::ManagedSession {
                    response: bcode_ipc::harness::ManagedSessionResponse {
                        schema_version: bcode_ipc::harness::MANAGED_SESSION_VERSION,
                        response_json: serde_json::to_string(&response).unwrap(),
                    },
                }),
            )
            .unwrap();
            bcode_ipc::send_envelope(&mut stream, &response)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn invalid_reap_response_never_continues_or_acknowledges() {
        use bcode_provider_auth_models::managed::{AuthNamespace, ManagedAuthHandle};
        for reaped in [
            serde_json::json!({"reaped": false}),
            serde_json::json!({"acknowledged": true}),
            serde_json::json!({"reaped": "true"}),
            serde_json::json!({"success": true, "readiness": "ready"}),
            serde_json::json!(null),
        ] {
            for child_exit_success in [false, true] {
                let (_directory, endpoint, listener) = terminal_socket();
                let session_id = bcode_session_models::SessionId::new();
                let namespace = AuthNamespace {
                    plugin_id: "example.native".into(),
                    credential_namespace: "/native/config".into(),
                };
                let selection = ManagedSessionHandoffRequest {
                    session_id,
                    instance_id: "primary".into(),
                    handle: ManagedAuthHandle {
                        daemon_epoch: "boot".into(),
                        flow_id: 1,
                    },
                    interaction_id: "native-request".into(),
                    interface_id: "native.login/v1".into(),
                };
                let response_json = serde_json::to_string(&reaped).unwrap();
                let server = tokio::spawn(async move {
                    let mut stream = listener.accept().await.unwrap();
                    let envelope = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
                    let bcode_ipc::Request::ManagedSession { request } =
                        bcode_ipc::decode_request(&envelope.payload).unwrap()
                    else {
                        panic!("only sealed managed auth requests allowed")
                    };
                    assert_eq!(request.operation, Op::AuthReaped);
                    let response = bcode_ipc::response_envelope(
                        envelope.request_id,
                        &bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::ManagedSession {
                            response: bcode_ipc::harness::ManagedSessionResponse {
                                schema_version: bcode_ipc::harness::MANAGED_SESSION_VERSION,
                                response_json,
                            },
                        }),
                    )
                    .unwrap();
                    bcode_ipc::send_envelope(&mut stream, &response)
                        .await
                        .unwrap();
                    // The caller closes after rejection; no continuation or acknowledgement
                    // may be sent merely because its native child exited successfully.
                    assert!(bcode_ipc::recv_envelope(&mut stream).await.is_err());
                });
                let stream = bcode_ipc::LocalIpcStream::connect(&endpoint).await.unwrap();
                let mut flow = terminal_flow(stream, session_id, namespace);
                flow.handle = Some(selection.handle.clone());
                flow.revision = Some(1);
                assert!(
                    flow.finish_terminal(&selection, child_exit_success)
                        .await
                        .is_err()
                );
                drop(flow);
                server.await.unwrap();
            }
        }
    }

    fn terminal_socket() -> (
        tempfile::TempDir,
        bcode_ipc::IpcEndpoint,
        bcode_ipc::LocalIpcListener,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let socket_directory = directory.path().join("ipc");
        std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700)
            .create(&socket_directory)
            .unwrap();
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(
            socket_directory.canonicalize().unwrap().join("auth.sock"),
        );
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
        (directory, endpoint, listener)
    }

    fn terminal_flow(
        stream: bcode_ipc::LocalIpcStream,
        session_id: bcode_session_models::SessionId,
        namespace: bcode_provider_auth_models::managed::AuthNamespace,
    ) -> ManagedSessionAuthFlow {
        let connection = crate::ClientConnection {
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
        ManagedSessionAuthFlow {
            connection,
            session_id,
            instance_id: "primary".into(),
            namespace,
            custody: bcode_provider_auth_models::managed::AuthCustodyMode::NativeManaged,
            handle: None,
            revision: None,
            uncertain: false,
            begin_attempted: false,
            terminal_attempted: true,
        }
    }

    #[test]
    fn terminal_reap_and_acknowledgement_are_distinct_evidence() {
        require_reaped(&serde_json::json!({"reaped": true})).unwrap();
        require_acknowledged(&serde_json::json!({"acknowledged": true})).unwrap();
        assert!(require_reaped(&serde_json::json!({"acknowledged": true})).is_err());
        assert!(require_acknowledged(&serde_json::json!({"reaped": true})).is_err());
        for value in [
            serde_json::json!({"success": true}),
            serde_json::json!({"readiness": "ready"}),
            serde_json::json!({"acknowledged": false}),
            serde_json::json!({"reaped": false}),
            serde_json::json!({"reaped": "true"}),
            serde_json::json!(null),
        ] {
            assert!(require_reaped(&value).is_err());
            assert!(require_acknowledged(&value).is_err());
        }
    }
}
