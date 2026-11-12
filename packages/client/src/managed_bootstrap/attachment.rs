//! Session-bound managed attachment. All reads and mutations delegate to existing application paths.
use crate::{AttachedSessionHistory, ClientConnection, ClientError, ClientSessionObservation};
use bcode_harness_models::{
    HARNESS_SCHEMA_VERSION, HarnessConfigureRequest, HarnessControlRequest, HarnessRequest,
    HarnessRequestKind, HarnessResponseValue, HarnessServiceResponse, HarnessServiceResponseKind,
    HarnessSessionSetting,
};
use bcode_ipc::harness::{
    ManagedSessionCall, ManagedSessionHistoryRequest, ManagedSessionOperation,
};
use bcode_ipc::{Event, ManagedSessionTargetSnapshot};
use bcode_session_models::{
    ExternalHarnessRequest, ExternalHarnessResponse, ProjectionHistoryPage,
    ProjectionWindowRequest, SessionHistoryPage, SessionHistoryQuery, SessionId,
    SessionObservationApplication, SessionObservationError, SessionObservationRequest,
};

use super::ManagedSessionClient;
use std::{ops::Deref, sync::Arc};

/// Clonable, session-bound commands independent of an attachment's in-flight event read.
/// Clones preserve the chosen configuration and normal host admission; no raw/model API exists.
///
/// ```compile_fail
/// fn no_model(handle: bcode_client::ManagedSessionCommandHandle) {
///     handle.send_user_message("model prompt");
/// }
/// ```
#[derive(Clone)]
pub struct ManagedSessionCommandHandle {
    pub(super) client: Arc<ManagedSessionClient>,
    pub(super) session_id: SessionId,
    pub(super) command_connection: Arc<tokio::sync::Mutex<CommandConnection>>,
}

#[derive(Default)]
pub(super) struct CommandConnection {
    connection: Option<ClientConnection>,
    detached: bool,
}

/// Existing canonical managed session plus one authorized event attachment.
///
/// No model prompt, raw transport, generic plugin invocation or client conversion is exposed.
/// Canonical binding evidence does not grant native authority: every mutation is re-admitted
/// by the existing managed application under the preserved caller configuration.
///
/// ```compile_fail
/// fn model_prompt(connection: &mut bcode_client::ManagedSessionConnection) {
///     connection.send_user_message("ordinary model input");
/// }
/// ```
pub struct ManagedSessionConnection {
    commands: ManagedSessionCommandHandle,
    connection: ClientConnection,
}

impl Deref for ManagedSessionConnection {
    type Target = ManagedSessionCommandHandle;

    fn deref(&self) -> &Self::Target {
        &self.commands
    }
}

fn invalid() -> ClientError {
    ClientError::Protocol("managed attachment requires the exact canonical managed session".into())
}

fn select_terminal_auth_interface(
    plugin_id: &str,
    services: &[bcode_plugin_sdk::application::PluginServiceSummary],
) -> Result<Option<String>, ClientError> {
    let mut matching = services.iter().filter(|service| {
        service.plugin_id == plugin_id
            && service.interface_id == bcode_harness_models::TERMINAL_AUTH_INTERFACE_ID
    });
    let selected = matching.next();
    if matching.next().is_some() {
        return Err(invalid());
    }
    Ok(selected.map(|service| service.interface_id.clone()))
}

fn verify_binding(
    session_id: SessionId,
    snapshot: &ManagedSessionTargetSnapshot,
) -> Result<(), ClientError> {
    snapshot.validate().map_err(|_| invalid())?;
    if snapshot.session_id != session_id {
        return Err(invalid());
    }
    Ok(())
}

// Attaching history is observation, not authority to switch a native target or owner.
fn verify_attachment_binding(
    expected: &ManagedSessionTargetSnapshot,
    actual: &ManagedSessionTargetSnapshot,
) -> Result<(), ClientError> {
    verify_binding(expected.session_id, actual)?;
    if actual.instance != expected.instance || actual.target != expected.target {
        return Err(invalid());
    }
    Ok(())
}

fn event_matches(session_id: SessionId, event: &Event) -> bool {
    match event {
        Event::Session(event) | Event::RuntimeWork(event) => event.session_id == session_id,
        Event::SessionLive(event) => event.session_id == session_id,
        Event::SessionViewResyncRequired { session_id: owner } => *owner == session_id,
        Event::Workflow(_)
        | Event::SessionCatalogUpdated { .. }
        | Event::ClientToolInvocation(_)
        | Event::ClientToolCancelled { .. }
        | Event::ClientToolClaimLost { .. } => false,
    }
}

impl ManagedSessionClient {
    /// Attach one bounded canonical projection with input history using the ordinary attach path.
    /// # Errors
    /// Rejects absent, foreign or changed managed bindings, attach admission and transport errors.
    pub async fn attach_session_projection_window(
        self,
        session_id: SessionId,
        request: ProjectionWindowRequest,
    ) -> Result<(ManagedSessionConnection, AttachedSessionHistory), ClientError> {
        let (mut attached, binding) = self.connect_session(session_id).await?;
        let history = attached
            .connection
            .attach_session_projection_window_with_input_history(session_id, request)
            .await?;
        if history.session.id != session_id {
            return Err(invalid());
        }
        // Recheck the exact association after attach; discovery never transfers ownership.
        verify_attachment_binding(&binding, &attached.binding().await?)?;
        Ok((attached, history))
    }

    /// Attach a recent bounded canonical window and input history without model authentication.
    /// # Errors
    /// Returns canonical binding changes, ordinary attach admission or transport failures.
    pub async fn attach_session_recent(
        self,
        session_id: SessionId,
        limit: usize,
    ) -> Result<(ManagedSessionConnection, AttachedSessionHistory), ClientError> {
        let (mut attached, binding) = self.connect_session(session_id).await?;
        let history = attached
            .connection
            .attach_session_recent_with_input_history(session_id, limit)
            .await?;
        if history.session.id != session_id {
            return Err(invalid());
        }
        verify_attachment_binding(&binding, &attached.binding().await?)?;
        Ok((attached, history))
    }

    async fn connect_session(
        self,
        session_id: SessionId,
    ) -> Result<(ManagedSessionConnection, ManagedSessionTargetSnapshot), ClientError> {
        let snapshot = self
            .inner
            .managed_session_binding(session_id)
            .await?
            .ok_or_else(invalid)?;
        verify_binding(session_id, &snapshot)?;
        let connection = self
            .inner
            .connect("bcode-managed-session-attachment")
            .await?;
        Ok((
            ManagedSessionConnection {
                commands: ManagedSessionCommandHandle {
                    client: Arc::new(self),
                    session_id,
                    command_connection: Arc::default(),
                },
                connection,
            },
            snapshot,
        ))
    }
}

impl ManagedSessionCommandHandle {
    /// Permanently close this handle family's authenticated submission channel.
    /// Existing grants lose their live caller; clones cannot silently reconnect.
    pub async fn detach_commands(&self) {
        let mut channel = self.command_connection.lock().await;
        channel.detached = true;
        channel.connection = None;
    }

    /// Immutable canonical Bcode session identity; never a native conversation identity.
    #[must_use]
    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Refresh canonical prepared/confirmed evidence without native work or promotion.
    /// # Errors
    /// Returns authorization, binding-loss, ownership or transport failures.
    pub async fn binding(&self) -> Result<ManagedSessionTargetSnapshot, ClientError> {
        let snapshot = self
            .client
            .inner
            .managed_session_binding(self.session_id)
            .await?
            .ok_or_else(invalid)?;
        verify_binding(self.session_id, &snapshot)?;
        Ok(snapshot)
    }

    /// Host client for allow-listed host operations of a managed conversation (catalog,
    /// search, rename, timeline and similar daemon reads), sharing this handle's configuration
    /// with model credential materialization deferred. It requires an already running daemon.
    ///
    /// Managed submission, interruption, responses and configuration must still go through
    /// this handle, whose methods carry managed admission; the host client grants no native
    /// authority.
    #[must_use]
    pub fn host_client(&self) -> crate::BcodeClient {
        self.client
            .inner
            .clone()
            .with_daemon_availability(crate::DaemonAvailability::RequireRunning)
    }
}

impl Drop for ManagedSessionConnection {
    fn drop(&mut self) {
        // Event-task teardown (including abort/session switch) ends attachment authority
        // even when UI command clones remain. Never adopt the event client's identity.
        let channel = Arc::clone(&self.commands.command_connection);
        if let Ok(mut state) = channel.try_lock() {
            state.detached = true;
            state.connection = None;
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let mut state = channel.lock().await;
                state.detached = true;
                state.connection = None;
            });
        }
    }
}

impl ManagedSessionConnection {
    /// Clone commands without sharing the mutable event receive connection or its lock.
    #[must_use]
    pub fn command_handle(&self) -> ManagedSessionCommandHandle {
        self.commands.clone()
    }

    /// Receive only this session's existing event stream, without implicit reconnection/replay.
    /// Cancellation rules are those of `ClientConnection::recv_event_without_reconnect`: retain
    /// the in-flight receive or discard this connection after cancellation of a partial read.
    /// # Errors
    /// Returns transport/decoding failures; reattach explicitly to recover a bounded snapshot.
    pub async fn recv_event_without_reconnect(&mut self) -> Result<Event, ClientError> {
        loop {
            let event = self.connection.recv_event_without_reconnect().await?;
            if event_matches(self.session_id, &event) {
                return Ok(event);
            }
        }
    }

    /// Refresh the attached bounded projection and canonical input history on this connection.
    /// # Errors
    /// Returns binding loss/changes, ordinary attach admission, projection or transport failures.
    pub async fn refresh_projection(
        &mut self,
        request: ProjectionWindowRequest,
    ) -> Result<AttachedSessionHistory, ClientError> {
        let binding = self.binding().await?;
        let session_id = self.session_id();
        let history = self
            .connection
            .attach_session_projection_window_with_input_history(session_id, request)
            .await?;
        if history.session.id != self.session_id {
            return Err(invalid());
        }
        verify_attachment_binding(&binding, &self.binding().await?)?;
        Ok(history)
    }
}

impl ManagedSessionCommandHandle {
    /// Hydrate canonical submissions referenced by this session's bounded history window.
    /// # Errors
    /// Rejects foreign events before transport; missing canonical records fail closed through
    /// the existing hydration path rather than becoming incomplete successful paging.
    pub async fn submissions_for_history(
        &self,
        events: &[bcode_session_models::SessionEvent],
    ) -> Result<Vec<bcode_session_models::SubmissionRecord>, ClientError> {
        if events
            .iter()
            .any(|event| event.session_id != self.session_id)
        {
            return Err(invalid());
        }
        self.client
            .inner
            .submissions_for_history(self.session_id, events)
            .await
    }

    /// Discover the bound plugin's unique generic terminal-auth service declaration.
    ///
    /// Metadata is not readiness, availability or permission. The terminal lifecycle re-admits
    /// ownership before issuing a launch plan; no vendor alias is guessed.
    /// # Errors
    /// Returns binding/discovery failures or ambiguous duplicate declarations.
    pub async fn terminal_auth_interface(&self) -> Result<Option<String>, ClientError> {
        let binding = self.binding().await?;
        let services = self.client.inner.plugin_services().await?;
        select_terminal_auth_interface(&binding.instance.plugin_id, &services)
    }

    /// Read this session's composer draft, without submitting input.
    /// # Errors
    /// Returns ordinary draft authorization or transport failures.
    pub async fn composer_draft(&self) -> Result<Option<String>, ClientError> {
        self.client
            .inner
            .composer_draft(bcode_session_models::ComposerDraftScope::Session {
                session_id: self.session_id,
            })
            .await
    }

    /// Persist this session's composer draft; this is not a native or model submission.
    /// # Errors
    /// Returns ordinary draft validation, authorization or transport failures.
    pub async fn set_composer_draft(&self, text: String) -> Result<(), ClientError> {
        self.client
            .inner
            .set_composer_draft(
                bcode_session_models::ComposerDraftScope::Session {
                    session_id: self.session_id,
                },
                text,
            )
            .await
    }

    /// Read canonical bounded history through the existing history path.
    /// # Errors
    /// Returns daemon authorization, query or transport failures.
    pub async fn session_history_page(
        &self,
        query: SessionHistoryQuery,
    ) -> Result<SessionHistoryPage, ClientError> {
        self.client
            .inner
            .session_history_page(self.session_id, query)
            .await
    }

    /// Read one canonical projection-selected page without reimplementing projection mechanics.
    /// # Errors
    /// Returns daemon authorization, projection or transport failures.
    pub async fn session_projection_history_page(
        &self,
        request: ProjectionWindowRequest,
    ) -> Result<ProjectionHistoryPage, ClientError> {
        self.client
            .inner
            .session_projection_history_page(self.session_id, request)
            .await
    }

    /// Open the existing semantic observation stream, confined to this session.
    /// # Errors
    /// Rejects foreign session requests before transport and preserves existing observation errors.
    pub async fn observe_session(
        &self,
        request: SessionObservationRequest,
    ) -> Result<ClientSessionObservation, SessionObservationError> {
        if request.session_id != self.session_id {
            return Err(SessionObservationError::InvalidRequest);
        }
        self.client.inner.observe_session(request).await
    }

    /// Drive one canonical native page for this attachment's exact instance/generation.
    /// No cursor is accepted from callers; empty pages do not imply settlement. Never retries.
    /// # Errors
    /// Returns ownership, transport, native availability or canonical commit errors.
    pub async fn poll_native_observations(
        &self,
        instance_id: &str,
        generation: u64,
    ) -> Result<(), ClientError> {
        self.attachment_ingest(ManagedSessionOperation::Observe, instance_id, generation)
            .await
    }

    /// Ask the daemon to ingest native observations for this attachment until this client
    /// disconnects or ownership changes. Committed pages arrive through the ordinary session
    /// event subscription; no client polling is needed. Repeating the request is idempotent.
    /// # Errors
    /// Returns ownership, transport, native availability or canonical commit errors from the
    /// admission and first drain; later failures end the daemon watch silently.
    pub async fn watch_native_observations(
        &self,
        instance_id: &str,
        generation: u64,
    ) -> Result<(), ClientError> {
        self.attachment_ingest(ManagedSessionOperation::Watch, instance_id, generation)
            .await
    }

    async fn attachment_ingest(
        &self,
        operation: ManagedSessionOperation,
        instance_id: &str,
        generation: u64,
    ) -> Result<(), ClientError> {
        let request = bcode_ipc::harness::ManagedSessionPollRequest {
            session_id: self.session_id,
            instance_id: instance_id.to_owned(),
            generation,
            limit: 64,
        };
        request.validate().map_err(|_| invalid())?;
        let envelope = bcode_ipc::harness::ManagedSessionRequest {
            schema_version: bcode_ipc::harness::MANAGED_SESSION_VERSION,
            operation,
            request_json: serde_json::to_string(&request).map_err(|_| invalid())?,
        };
        // The sealed handle alone owns this route; no arbitrary native call is exposed.
        let response = self
            .client
            .inner
            .call_plugin_service(
                bcode_ipc::harness::MANAGED_SESSION_INTERFACE_ID.to_owned(),
                operation.as_str().to_owned(),
                serde_json::to_vec(&envelope).map_err(|_| invalid())?,
            )
            .await?;
        if response.error.is_some()
            || response.payload.len() > bcode_harness_models::MAX_HARNESS_WIRE_BYTES
        {
            return Err(invalid());
        }
        let completed: serde_json::Value =
            serde_json::from_slice(&response.payload).map_err(|_| invalid())?;
        if completed
            .get("committed")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
        {
            return Err(invalid());
        }
        Ok(())
    }

    /// Read bounded canonical native observations; not a native poll or resume.
    /// # Errors
    /// Returns existing managed history admission/validation failures.
    pub async fn managed_session_history(
        &self,
        after: Option<String>,
        limit: u16,
    ) -> Result<bcode_harness_models::HarnessObservationPage, ClientError> {
        self.client
            .inner
            .managed_session_history(ManagedSessionHistoryRequest {
                session_id: self.session_id,
                after,
                limit,
            })
            .await
    }

    /// Prepare exact retained managed composer intent, never an ordinary model prompt.
    /// # Errors
    /// Rejects lost managed association or unsupported/stale steering; no model fallback exists.
    pub async fn prepare_managed_composer_submission(
        &self,
        identities: (String, String),
        text: String,
        steering: bool,
        expected_native_turn_id: Option<String>,
    ) -> Result<ExternalHarnessRequest, ClientError> {
        self.client
            .inner
            .prepare_managed_composer_submission(
                self.session_id,
                identities,
                text,
                steering,
                expected_native_turn_id,
            )
            .await?
            .ok_or_else(invalid)
    }

    /// Deliver only an exact retained submission for this session. Never automatically retries.
    /// # Errors
    /// Rejects foreign input before effects, then uses existing native delivery admission.
    pub async fn submit_managed_composer_request(
        &self,
        retained: ExternalHarnessRequest,
    ) -> Result<ExternalHarnessResponse, ClientError> {
        verify_submission(self.session_id, &retained)?;
        let request = retained.request().map_err(|_| invalid())?;
        let HarnessRequestKind::Submit { target, submission } = &request.kind else {
            return Err(invalid());
        };
        let call = crate::harness::submission_call(
            request.instance.clone(),
            target.clone(),
            submission.clone(),
        )?;
        let envelope = bcode_ipc::harness::ManagedSessionRequest {
            schema_version: bcode_ipc::harness::MANAGED_SESSION_VERSION,
            operation: ManagedSessionOperation::Followup,
            request_json: serde_json::to_string(&call).map_err(|_| invalid())?,
        };
        envelope.decode_call().map_err(|_| invalid())?;
        // This connection is a live authenticated principal, not a per-request transport.
        // Never reconnect/replay an uncertain submission or transfer its grant to the event client.
        let mut channel = self.command_connection.lock().await;
        if channel.detached {
            return Err(invalid());
        }
        if channel.connection.is_none() {
            channel.connection = Some(
                self.client
                    .inner
                    .connect("bcode-managed-session-command")
                    .await?,
            );
        }
        let payload = channel
            .connection
            .as_mut()
            .ok_or_else(invalid)?
            .send_request(crate::Request::ManagedSession {
                request: Box::new(envelope),
            })
            .await?;
        drop(channel);
        let crate::ResponsePayload::ManagedSession { response } = payload else {
            return Err(ClientError::UnexpectedResponse);
        };
        if response.schema_version != bcode_ipc::harness::MANAGED_SESSION_VERSION
            || response.response_json.len() > bcode_harness_models::MAX_HARNESS_WIRE_BYTES
        {
            return Err(invalid());
        }
        let response: HarnessServiceResponse =
            serde_json::from_str(&response.response_json).map_err(|_| invalid())?;
        ExternalHarnessResponse::from_response(&request, &response).map_err(|_| invalid())
    }

    /// Answer one exact native interaction under current generation and capability checks.
    /// # Errors
    /// Returns existing managed interaction admission failures; parentage grants no authority.
    pub async fn managed_session_respond(
        &self,
        generation: u64,
        request_id: String,
        response_id: String,
        value: HarnessResponseValue,
    ) -> Result<HarnessServiceResponse, ClientError> {
        self.client
            .inner
            .managed_session_respond(self.session_id, generation, request_id, response_id, value)
            .await
    }

    /// Request native interruption using an explicit retained mutation and expected generation.
    /// An acknowledgement never proves foreground/background settlement.
    /// # Errors
    /// Rejects stale canonical targets and delegates authorization/effects to managed admission.
    pub async fn interrupt(
        &self,
        generation: u64,
        mutation_id: String,
    ) -> Result<HarnessServiceResponse, ClientError> {
        let snapshot = self.binding().await?;
        snapshot
            .target
            .validate_owner(&self.session_id.to_string(), generation)
            .map_err(|_| invalid())?;
        self.client
            .inner
            .managed_session_call(
                ManagedSessionOperation::Interrupt,
                ManagedSessionCall {
                    session_id: Some(self.session_id),
                    harness: HarnessRequest {
                        schema_version: HARNESS_SCHEMA_VERSION,
                        request_id: mutation_id.clone(),
                        instance: snapshot.instance,
                        kind: HarnessRequestKind::Interrupt(HarnessControlRequest {
                            target: snapshot.target,
                            mutation_id,
                        }),
                    },
                    parent_session_id: None,
                    assistance_destination_session_id: None,
                },
            )
            .await
    }

    /// Change one native session setting (model or permission mode) mid-session.
    ///
    /// Uses an explicit retained mutation identity under the expected generation. An
    /// acknowledgement is not proof the native setting took effect, and an ambiguous outcome
    /// must not be replayed automatically.
    /// # Errors
    /// Rejects stale canonical targets; delegates capability, authorization and effects to
    /// managed admission (which reports `Failure(Unsupported)` when the plugin lacks support).
    pub async fn managed_session_configure(
        &self,
        generation: u64,
        mutation_id: String,
        setting: HarnessSessionSetting,
    ) -> Result<HarnessServiceResponse, ClientError> {
        let snapshot = self.binding().await?;
        snapshot
            .target
            .validate_owner(&self.session_id.to_string(), generation)
            .map_err(|_| invalid())?;
        self.client
            .inner
            .managed_session_call(
                ManagedSessionOperation::Configure,
                ManagedSessionCall {
                    session_id: Some(self.session_id),
                    harness: HarnessRequest {
                        schema_version: HARNESS_SCHEMA_VERSION,
                        request_id: mutation_id.clone(),
                        instance: snapshot.instance,
                        kind: HarnessRequestKind::Configure(HarnessConfigureRequest {
                            target: snapshot.target,
                            mutation_id,
                            setting,
                        }),
                    },
                    parent_session_id: None,
                    assistance_destination_session_id: None,
                },
            )
            .await
    }

    /// Recover this managed session after daemon loss and return the new ownership generation.
    ///
    /// `expected_generation` is the generation the caller last observed; `recovery_id` is a
    /// fresh retained identity. Prior submissions are never replayed.
    /// # Errors
    /// Returns binding/transport failures, daemon refusal (including unpublished recovery or
    /// a native `Failure`), and responses lacking a confirmed newer binding.
    pub async fn recover(
        &self,
        expected_generation: u64,
        recovery_id: String,
    ) -> Result<u64, ClientError> {
        let snapshot = self.binding().await?;
        let response = self
            .client
            .inner
            .managed_session_recover(bcode_ipc::harness::ManagedSessionRecoveryRequest {
                session_id: self.session_id,
                instance_id: snapshot.instance.instance_id,
                expected_generation,
                recovery_id,
            })
            .await?;
        match response.kind {
            HarnessServiceResponseKind::Opened(binding) => Ok(binding.generation),
            HarnessServiceResponseKind::Failure(failure) => Err(ClientError::Protocol(format!(
                "managed recovery failed: {failure:?}"
            ))),
            _ => Err(invalid()),
        }
    }
}

fn verify_submission(
    session_id: SessionId,
    retained: &ExternalHarnessRequest,
) -> Result<(), ClientError> {
    let request = retained.request().map_err(|_| invalid())?;
    request.validate().map_err(|_| invalid())?;
    let HarnessRequestKind::Submit { target, submission } = &request.kind else {
        return Err(invalid());
    };
    target
        .validate_owner(&session_id.to_string(), submission.generation)
        .map_err(|_| invalid())?;
    if submission.session_id != session_id.to_string() {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
