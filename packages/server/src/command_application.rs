//! Typed command adapter over the existing plugin invocation and cancellation owners.

use crate::{ServerState, services_application::ServicesApplicationHandle};
use bcode_command::application::{
    CommandApplication, CommandApplicationFailure, CommandCancellationAcknowledgment,
    CommandInvocationOutcome, CommandInvocationRequest, CommandInvocationResult,
};
use bcode_command::{
    COMMAND_INTERFACE_ID, CommandList, InvokeCommandResponse, OP_INVOKE_COMMAND, OP_LIST_COMMANDS,
};
use bcode_plugin_sdk::application::{
    PluginApplication as _, PluginApplicationFailure, PluginServiceContext, PluginServiceResponse,
};
use bcode_session_models::ClientId;
use std::sync::Arc;

/// Verified caller-bound access; lifetime and cancellation remain plugin-owner controlled.
pub struct CommandApplicationHandle<'a> {
    state: &'a Arc<ServerState>,
    client_id: ClientId,
}

impl<'a> CommandApplicationHandle<'a> {
    /// Bind only after the host verifies and registers the caller context.
    #[must_use]
    pub const fn new(state: &'a Arc<ServerState>, client_id: ClientId) -> Self {
        Self { state, client_id }
    }

    async fn invoke(
        &self,
        mut request: CommandInvocationRequest,
    ) -> Result<InvokeCommandResponse, CommandApplicationFailure> {
        if request.command.command_id.len() > 256
            || request.command.args.len() > 1024
            || request
                .command
                .args
                .iter()
                .fold(0usize, |total, (key, value)| {
                    total.saturating_add(key.len()).saturating_add(value.len())
                })
                > 128 * 1024
        {
            return Err(invalid(
                "command_request_too_large",
                "command request exceeds the bounded input allowance",
            ));
        }
        let session_id = request
            .command
            .context
            .as_ref()
            .and_then(|context| context.session_id);
        // Never trust the command payload's claim that its directory is canonical.
        // The contextual plugin owner subsequently verifies attachment/ownership, hint
        // agreement, agent, policy, lifetime and invocation identity before dispatch.
        let directory = if let Some(session_id) = session_id {
            self.state
                .sessions
                .session_working_directory(session_id)
                .await
                .map_err(|_| {
                    invalid(
                        "command_context_unavailable",
                        "canonical command context is unavailable",
                    )
                })?
        } else {
            self.state
                .client_runtime_context(self.client_id)
                .await
                .and_then(|context| context.working_directory)
                .ok_or_else(|| {
                    invalid(
                        "command_context_unavailable",
                        "canonical command context is unavailable",
                    )
                })?
        };
        let directory = tokio::fs::canonicalize(directory).await.map_err(|_| {
            invalid(
                "command_context_unavailable",
                "canonical command context is unavailable",
            )
        })?;
        request.command.context = Some(bcode_command::CommandInvocationContext {
            session_id,
            working_directory: directory,
        });
        let payload = serde_json::to_vec(&request.command).map_err(|_| {
            invalid(
                "invalid_command_request",
                "command request could not be encoded",
            )
        })?;
        if payload.len() > 256 * 1024 {
            return Err(invalid(
                "command_request_too_large",
                "command request exceeds the bounded input allowance",
            ));
        }
        let response = ServicesApplicationHandle::new(self.state, self.client_id)
            .invoke_plugin_service_contextual(
                PluginServiceContext {
                    agent_id: request.agent_id,
                    invocation_id: request.invocation_id,
                    workspace: bcode_plugin_sdk::WorkspaceContext {
                        session_id: session_id.map(|id| id.to_string()),
                        launch_root: request.launch_root,
                    },
                    deadline_ms: request.deadline_ms,
                },
                request.plugin_id,
                COMMAND_INTERFACE_ID.to_owned(),
                OP_INVOKE_COMMAND.to_owned(),
                payload,
            )
            .await
            .map_err(plugin_failure)?;
        decode(response)
    }
}

fn invalid(code: &str, message: &str) -> CommandApplicationFailure {
    CommandApplicationFailure {
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

fn plugin_failure(error: PluginApplicationFailure) -> CommandApplicationFailure {
    CommandApplicationFailure {
        code: error.code,
        message: error.message,
    }
}

fn decode<T: serde::de::DeserializeOwned>(
    response: PluginServiceResponse,
) -> Result<T, CommandApplicationFailure> {
    if let Some(error) = response.error {
        return Err(CommandApplicationFailure {
            code: error.code,
            message: error.message,
        });
    }
    if response.payload.len() > 1024 * 1024 {
        return Err(invalid(
            "command_response_too_large",
            "command response exceeds the bounded output allowance",
        ));
    }
    serde_json::from_slice(&response.payload).map_err(|_| {
        invalid(
            "invalid_command_response",
            "command owner returned an invalid typed response",
        )
    })
}

impl CommandApplication for CommandApplicationHandle<'_> {
    async fn list_commands(
        &self,
        plugin_id: String,
    ) -> Result<CommandList, CommandApplicationFailure> {
        let response = ServicesApplicationHandle::new(self.state, self.client_id)
            .invoke_plugin_service(
                plugin_id,
                COMMAND_INTERFACE_ID.to_owned(),
                OP_LIST_COMMANDS.to_owned(),
                Vec::new(),
            )
            .await
            .map_err(plugin_failure)?;
        decode(response)
    }

    async fn invoke_command(&self, request: CommandInvocationRequest) -> CommandInvocationResult {
        let invocation_id = request.invocation_id.clone();
        let outcome = match self.invoke(request).await {
            Ok(response) => CommandInvocationOutcome::Completed(response),
            Err(error)
                if matches!(
                    error.code.as_str(),
                    "invalid_command_response" | "command_response_too_large"
                ) =>
            {
                CommandInvocationOutcome::Unknown(error)
            }
            Err(error) => CommandInvocationOutcome::Failed(error),
        };
        CommandInvocationResult {
            invocation_id,
            outcome,
        }
    }

    async fn cancel_command(
        &self,
        invocation_id: String,
    ) -> Result<CommandCancellationAcknowledgment, CommandApplicationFailure> {
        let accepted = ServicesApplicationHandle::new(self.state, self.client_id)
            .cancel_plugin_service_invocation(invocation_id.clone())
            .await
            .map_err(plugin_failure)?;
        Ok(CommandCancellationAcknowledgment {
            invocation_id,
            accepted,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCKED_COMMAND_MANIFEST: &str = r#"
id = "test.blocked-command"
name = "Blocked command cancellation fixture"
version = "0.0.1"
[[services]]
interface_id = "bcode.command/v1"
invocation_operations = ["invoke"]
[[command_contributions]]
id = "block"
title = "Block until cancelled"
[runtime]
type = "native"
abi_version = 5
library = "libtest_blocked_command.dylib"
"#;

    static ENTERED_COMMANDS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    static CANCELLED_COMMANDS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    #[derive(Default)]
    struct BlockedCommand;

    impl bcode_plugin_sdk::RustPlugin for BlockedCommand {
        fn invoke_service(
            &mut self,
            context: bcode_plugin_sdk::NativeServiceContext,
        ) -> bcode_plugin_sdk::ServiceResponse {
            ENTERED_COMMANDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(12);
            while !context.cancellation.is_cancelled() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            if context.cancellation.is_cancelled() {
                CANCELLED_COMMANDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            bcode_plugin_sdk::ServiceResponse::error("cancelled", "cancelled")
        }
    }

    fn blocked_command_server(
        directory: &std::path::Path,
    ) -> (
        std::sync::Arc<crate::ServerState>,
        bcode_ipc::IpcEndpoint,
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    ) {
        use std::sync::Arc;
        let plugins = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
            &bcode_plugin::PluginSelection {
                mode: bcode_plugin::PluginSelectionMode::Explicit,
                enabled: std::collections::BTreeSet::from([
                    "test.blocked-command".into(),
                    "bcode.default-agents".into(),
                ]),
                disabled: std::collections::BTreeSet::new(),
            },
            &[
                bcode_plugin::StaticBundledPlugin::new(
                    BLOCKED_COMMAND_MANIFEST,
                    bcode_plugin_sdk::static_plugin_vtable!(
                        BlockedCommand,
                        BLOCKED_COMMAND_MANIFEST
                    ),
                ),
                bcode_plugin::StaticBundledPlugin::new(
                    include_str!("../../../plugins/default-agents-plugin/bcode-plugin.toml"),
                    bcode_default_agents_plugin::static_plugin(),
                ),
            ],
        )
        .unwrap();
        let state = Arc::new(crate::tests::test_server_state_with_plugins(
            bcode_session::SessionManager::default(),
            plugins,
        ));
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(directory.join("command.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server_state = Arc::clone(&state);
        let server = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            tokio::pin!(stopped);
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    stream = listener.accept() => {
                        let state = Arc::clone(&server_state);
                        let stream = stream.unwrap();
                        tasks.spawn(async move { let _ = crate::handle_client(stream, state).await; });
                    }
                }
            }
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        });
        (state, endpoint, stop, server)
    }

    async fn foreign_wire_cancel(endpoint: &bcode_ipc::IpcEndpoint) -> bool {
        use bcode_ipc::{
            Request, Response, ResponsePayload, decode_response, recv_envelope, request_envelope,
            send_envelope,
        };
        let mut stream = bcode_ipc::LocalIpcStream::connect(endpoint).await.unwrap();
        let hello = Request::Hello {
            client_name: "foreign-command-controller".into(),
            runtime_context: None,
            daemon_namespace: bcode_ipc::daemon_namespace(),
            artifact_id: Some(bcode_ipc::ArtifactId::current()),
            build_fingerprint: bcode_ipc::BUILD_FINGERPRINT.into(),
            state_location_id: Some(bcode_ipc::state_location_id()),
        };
        send_envelope(&mut stream, &request_envelope(1, &hello).unwrap())
            .await
            .unwrap();
        let envelope = recv_envelope(&mut stream).await.unwrap();
        assert!(matches!(
            decode_response(&envelope.payload).unwrap(),
            Response::Ok(ResponsePayload::Hello { .. })
        ));
        let cancel = Request::CancelApplicationCommand {
            invocation_id: "active-command".into(),
        };
        send_envelope(&mut stream, &request_envelope(2, &cancel).unwrap())
            .await
            .unwrap();
        let envelope = recv_envelope(&mut stream).await.unwrap();
        assert_eq!(envelope.request_id, 2);
        match decode_response(&envelope.payload).unwrap() {
            Response::Ok(ResponsePayload::ApplicationCommandCancelled { result }) => {
                assert_eq!(result.invocation_id, "active-command");
                result.accepted
            }
            other => panic!("unexpected cancellation reply: {other:?}"),
        }
    }

    #[tokio::test]
    async fn active_typed_command_cancellation_is_same_caller_and_reaches_owner() {
        use std::sync::atomic::Ordering;
        use std::time::Duration;
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().canonicalize().unwrap();
        let (_state, endpoint, stop, server) = blocked_command_server(&directory);
        let client = bcode_client::BcodeClient::new(endpoint.clone()).with_runtime_context(Some(
            bcode_ipc::ClientRuntimeContext {
                working_directory: Some(directory.clone()),
                ..Default::default()
            },
        ));
        let foreign = bcode_client::BcodeClient::new(endpoint.clone()).with_runtime_context(Some(
            bcode_ipc::ClientRuntimeContext {
                working_directory: Some(directory.clone()),
                ..Default::default()
            },
        ));
        let request = CommandInvocationRequest {
            plugin_id: "test.blocked-command".into(),
            invocation_id: "active-command".into(),
            agent_id: Some("build".into()),
            launch_root: Some(directory),
            deadline_ms: 10_000,
            command: bcode_command::InvokeCommandRequest {
                command_id: "block".into(),
                args: std::collections::BTreeMap::new(),
                context: None,
            },
        };
        let before = ENTERED_COMMANDS.load(Ordering::SeqCst);
        let cancelled_before = CANCELLED_COMMANDS.load(Ordering::SeqCst);
        let invoking = client.clone();
        let invocation = tokio::spawn(async move { invoking.invoke_command(request).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            while ENTERED_COMMANDS.load(Ordering::SeqCst) == before {
                assert!(
                    !invocation.is_finished(),
                    "command must enter the blocked owner before cancellation"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            !foreign
                .cancel_command("active-command".into())
                .await
                .unwrap()
                .accepted
        );
        // A real foreign socket must also be fenced, independently of client-local controls.
        assert!(
            !tokio::time::timeout(Duration::from_secs(2), foreign_wire_cancel(&endpoint))
                .await
                .unwrap()
        );
        assert!(!invocation.is_finished());
        let cancellation = tokio::time::timeout(
            Duration::from_secs(2),
            client.cancel_command("active-command".into()),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(cancellation.accepted);
        let result = tokio::time::timeout(Duration::from_secs(2), invocation)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(result.outcome, CommandInvocationOutcome::Failed(ref failure) if failure.code == "plugin_cancelled")
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while CANCELLED_COMMANDS.load(Ordering::SeqCst) == cancelled_before {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            !client
                .cancel_command("active-command".into())
                .await
                .unwrap()
                .accepted
        );
        stop.send(()).unwrap();
        server.await.unwrap();
    }

    #[test]
    fn decoded_command_failure_retains_success_and_effects() {
        let response = InvokeCommandResponse {
            success: false,
            message: Some("owner refusal".to_owned()),
            updated_model: None,
            updated_provider: None,
            updated_thinking: None,
            effects: vec![bcode_command::CommandEffect::AppendText {
                text: "literal λ".to_owned(),
                format: bcode_command::CommandTextFormat::Markdown,
            }],
        };
        let decoded: InvokeCommandResponse = decode(PluginServiceResponse {
            payload: serde_json::to_vec(&response).unwrap(),
            error: None,
        })
        .unwrap();
        assert_eq!(
            decoded, response,
            "adapter must not erase failed response effects"
        );
    }

    #[test]
    fn service_error_wins_over_payload_and_invalid_response_is_secret_safe() {
        let error = decode::<CommandList>(PluginServiceResponse {
            payload: b"invalid-private-payload".to_vec(),
            error: Some(bcode_plugin_sdk::application::PluginServiceError {
                code: "owner_failure".to_owned(),
                message: "owner public diagnostic".to_owned(),
            }),
        })
        .unwrap_err();
        assert_eq!(error.code, "owner_failure");
        assert_eq!(error.message, "owner public diagnostic");
        let invalid = decode::<CommandList>(PluginServiceResponse {
            payload: b"invalid-private-payload".to_vec(),
            error: None,
        })
        .unwrap_err();
        assert_eq!(invalid.code, "invalid_command_response");
        assert!(!invalid.message.contains("private-payload"));
    }
}
