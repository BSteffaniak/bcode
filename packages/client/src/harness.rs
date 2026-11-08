//! Client-side bounded external-history pagination. Transport stays on the session application
//! boundary; this cursor never calls native harness operations or retries mutations.

use std::fmt::Write as _;

use bcode_session_models::SessionId;
use bcode_session_models::external_backend::{
    ExternalBackendEnvelope, ExternalBackendPage, ExternalBackendQuery,
};

use crate::{BcodeClient, ClientError, Request, ResponsePayload};
use bcode_harness_models::{
    HARNESS_SCHEMA_VERSION, HarnessRequest, HarnessRequestKind, HarnessResponse,
    HarnessResponseValue, HarnessServiceResponse, HarnessServiceResponseKind, HarnessSupportLevel,
    MAX_HARNESS_WIRE_BYTES,
};
use bcode_ipc::harness::{
    MANAGED_SESSION_VERSION, ManagedSessionCall, ManagedSessionHistoryRequest,
    ManagedSessionOperation, ManagedSessionRequest,
};

fn decode_target_snapshot(
    json: &str,
    session_id: SessionId,
) -> Result<Option<bcode_ipc::ManagedSessionTargetSnapshot>, ClientError> {
    if json.len() > MAX_HARNESS_WIRE_BYTES {
        return Err(ClientError::Protocol("managed target exceeds bound".into()));
    }
    let snapshot: Option<bcode_ipc::ManagedSessionTargetSnapshot> = serde_json::from_str(json)
        .map_err(|_| ClientError::Protocol("invalid managed target response".into()))?;
    if let Some(value) = &snapshot {
        value
            .validate()
            .map_err(|_| ClientError::Protocol("invalid managed target".into()))?;
        if value.session_id != session_id {
            return Err(ClientError::Protocol(
                "managed target owner mismatch".into(),
            ));
        }
    }
    Ok(snapshot)
}

pub(crate) fn submission_call(
    instance: bcode_harness_models::HarnessInstance,
    target: bcode_harness_models::HarnessSessionTarget,
    submission: bcode_harness_models::HarnessSubmission,
) -> Result<ManagedSessionCall, ClientError> {
    target
        .validate_owner(&submission.session_id, submission.generation)
        .map_err(|_| ClientError::Protocol("managed target owner mismatch".into()))?;
    if instance.instance_id != target.instance_id() {
        return Err(ClientError::Protocol(
            "managed target instance mismatch".into(),
        ));
    }
    let session_id = submission
        .session_id
        .parse()
        .map_err(|_| ClientError::Protocol("invalid managed target session".into()))?;
    let call = ManagedSessionCall {
        session_id: Some(session_id),
        harness: HarnessRequest {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: submission.submission_id.clone(),
            instance,
            kind: HarnessRequestKind::Submit { target, submission },
        },
        parent_session_id: None,
        assistance_destination_session_id: None,
    };
    call.harness.validate().map_err(|_| {
        ClientError::Protocol("invalid native submission or prepared steering".into())
    })?;
    Ok(call)
}

struct Budget {
    used: usize,
    maximum: usize,
}
impl std::io::Write for Budget {
    fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
        self.used = self.used.saturating_add(input.len());
        if self.used > self.maximum {
            return Err(std::io::Error::other("external page budget"));
        }
        Ok(input.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn command_response_value(kind: &str, rest: &[&str]) -> Result<HarnessResponseValue, ClientError> {
    Ok(match kind {
        "approve" if rest.is_empty() => HarnessResponseValue::Approval { approved: true },
        "deny" if rest.is_empty() => HarnessResponseValue::Approval { approved: false },
        "answer" => HarnessResponseValue::Answer {
            choice_id: None,
            text: rest.join(" "),
        },
        "choose" if !rest.is_empty() => HarnessResponseValue::Answer {
            choice_id: Some(rest[0].to_owned()),
            text: rest[1..].join(" "),
        },
        "assist" => HarnessResponseValue::Assistance {
            text: rest.join(" "),
        },
        _ => {
            return Err(ClientError::Protocol(
                "invalid native response command".into(),
            ));
        }
    })
}

fn command_submission_placement(
    mode: &str,
    rest: &[&str],
) -> Result<(bcode_harness_models::HarnessPlacement, String), ClientError> {
    let (placement, text) = if mode == "steer" {
        let Some((expected, text)) = rest.split_first() else {
            return Err(ClientError::Protocol(
                "native steering requires an expected native turn".into(),
            ));
        };
        (
            bcode_harness_models::HarnessPlacement::Steer {
                expected_native_turn_id: (*expected).into(),
            },
            text.join(" "),
        )
    } else {
        (
            bcode_harness_models::HarnessPlacement::NextTurn,
            rest.join(" "),
        )
    };
    if text.trim().is_empty() {
        return Err(ClientError::Protocol(
            "native submission text cannot be empty".into(),
        ));
    }
    Ok((placement, text))
}

fn response_command_text(response: &HarnessServiceResponse) -> Result<String, ClientError> {
    let body = match &response.kind {
        HarnessServiceResponseKind::Failure(failure) => {
            return Ok(format!(
                "Native response not accepted: {failure:?}. No automatic replay."
            ));
        }
        HarnessServiceResponseKind::Receipt(receipt) => format!(
            "Native response delivery {:?}. Not settlement; no automatic replay.",
            receipt.state
        ),
        HarnessServiceResponseKind::Acknowledged { .. } => {
            "Native response acknowledged; not settlement or assignment completion.".into()
        }
        _ => return Err(ClientError::UnexpectedResponse),
    };
    Ok(body)
}

impl crate::ClientConnection {
    /// Send one bounded managed authentication operation on this authenticated connection.
    ///
    /// Keep this connection for the entire begin/status/handoff/reaped/continue flow. This method
    /// never reconnects or retries; unrelated events remain in the ordinary pending-event queue.
    /// Request and response envelopes retain their redacted Debug and sensitive Drop behavior.
    ///
    /// # Errors
    /// Rejects non-auth operations, incompatible versions, oversized envelopes, transport failures,
    /// and unexpected responses without logging payloads or returning raw native diagnostics.
    pub async fn managed_session_auth(
        &mut self,
        request: ManagedSessionRequest,
    ) -> Result<bcode_ipc::ManagedSessionResponse, ClientError> {
        if !request.operation.is_auth() {
            return Err(ClientError::Protocol(
                "expected managed authentication operation".into(),
            ));
        }
        request
            .validate_envelope()
            .map_err(|_| ClientError::Protocol("invalid managed authentication envelope".into()))?;
        let payload = self
            .send_request(Request::ManagedSession {
                request: Box::new(request),
            })
            .await?;
        let ResponsePayload::ManagedSession { response } = payload else {
            return Err(ClientError::UnexpectedResponse);
        };
        if response.schema_version != MANAGED_SESSION_VERSION
            || response.response_json.len() > MAX_HARNESS_WIRE_BYTES
        {
            return Err(ClientError::Protocol(
                "incompatible managed authentication response".into(),
            ));
        }
        Ok(response)
    }
}

impl BcodeClient {
    /// Perform one typed non-auth managed-session application operation, never a vendor service call.
    /// Transport failures are returned without retrying mutations.
    /// # Errors
    /// Rejects incompatible or oversized envelopes and daemon admission failures.
    pub async fn managed_session_call(
        &self,
        operation: ManagedSessionOperation,
        call: ManagedSessionCall,
    ) -> Result<HarnessServiceResponse, ClientError> {
        if operation.is_auth() {
            return Err(ClientError::Protocol(
                "authentication requires the dedicated sensitive client flow".into(),
            ));
        }
        let request = ManagedSessionRequest {
            schema_version: MANAGED_SESSION_VERSION,
            operation,
            request_json: serde_json::to_string(&call)
                .map_err(|_| ClientError::Protocol("invalid managed request".into()))?,
        };
        request
            .decode_call()
            .map_err(|e| ClientError::Protocol(e.into()))?;
        let json = self.managed_session_request(request).await?;
        let response: HarnessServiceResponse = serde_json::from_str(&json)
            .map_err(|_| ClientError::Protocol("invalid managed response".into()))?;
        response
            .validate_for(&call.harness)
            .map_err(|_| ClientError::Protocol("managed response correlation mismatch".into()))?;
        Ok(response)
    }

    /// Send one already-bounded managed application envelope exactly once.
    /// # Errors
    /// Returns transport, daemon rejection, or incompatible response errors without retry.
    pub(crate) async fn managed_session_request(
        &self,
        request: ManagedSessionRequest,
    ) -> Result<String, ClientError> {
        request
            .validate_envelope()
            .map_err(|e| ClientError::Protocol(e.into()))?;
        let ResponsePayload::ManagedSession { response } = self
            .send_request(Request::ManagedSession {
                request: Box::new(request),
            })
            .await?
        else {
            return Err(ClientError::UnexpectedResponse);
        };
        if response.schema_version != MANAGED_SESSION_VERSION
            || response.response_json.len() > MAX_HARNESS_WIRE_BYTES
        {
            return Err(ClientError::Protocol(
                "incompatible managed response".into(),
            ));
        }
        Ok(response.response_json.clone())
    }

    /// Recover an existing managed session after its previous daemon owner was lost.
    ///
    /// Wraps `ManagedSessionOperation::Recover`: the daemon selects the next owner/generation,
    /// resumes only the exact retained native binding, and never replays prior input. A dropped
    /// acknowledgement is uncertain and must not be retried with a different recovery identity
    /// without first re-reading the canonical binding.
    /// # Errors
    /// Rejects invalid identities, daemon refusal (including unpublished recovery), malformed
    /// responses, and responses that do not belong to the requested session.
    pub async fn managed_session_recover(
        &self,
        request: bcode_ipc::harness::ManagedSessionRecoveryRequest,
    ) -> Result<HarnessServiceResponse, ClientError> {
        request
            .validate()
            .map_err(|e| ClientError::Protocol(e.into()))?;
        let envelope = ManagedSessionRequest {
            schema_version: MANAGED_SESSION_VERSION,
            operation: ManagedSessionOperation::Recover,
            request_json: serde_json::to_string(&request)
                .map_err(|_| ClientError::Protocol("invalid managed recovery request".into()))?,
        };
        envelope
            .decode_recovery()
            .map_err(|e| ClientError::Protocol(e.into()))?;
        let json = self.managed_session_request(envelope).await?;
        let response: HarnessServiceResponse = serde_json::from_str(&json)
            .map_err(|_| ClientError::Protocol("invalid managed recovery response".into()))?;
        response
            .validate()
            .map_err(|_| ClientError::Protocol("invalid managed recovery response".into()))?;
        if let HarnessServiceResponseKind::Opened(binding) = &response.kind
            && (binding.session_id != request.session_id.to_string()
                || binding.instance_id != request.instance_id
                || binding.generation <= request.expected_generation)
        {
            return Err(ClientError::Protocol(
                "managed recovery owner mismatch".into(),
            ));
        }
        Ok(response)
    }

    /// Read the exact canonical prepared/confirmed target without invoking a native plugin.
    /// # Errors
    /// Rejects incompatible, malformed, or foreign target snapshots; never fabricates a binding.
    pub async fn managed_session_binding(
        &self,
        session_id: SessionId,
    ) -> Result<Option<bcode_ipc::ManagedSessionTargetSnapshot>, ClientError> {
        let json = self
            .managed_session_request(ManagedSessionRequest {
                schema_version: MANAGED_SESSION_VERSION,
                operation: ManagedSessionOperation::Binding,
                request_json: serde_json::json!({"session_id": session_id}).to_string(),
            })
            .await?;
        decode_target_snapshot(&json, session_id)
    }

    /// Read one bounded canonical observation page independently of native resume availability.
    /// # Errors
    /// Returns canonical-history/transport/compatibility failures without repair or replay.
    pub async fn managed_session_history(
        &self,
        request: ManagedSessionHistoryRequest,
    ) -> Result<bcode_harness_models::HarnessObservationPage, ClientError> {
        let envelope = ManagedSessionRequest {
            schema_version: MANAGED_SESSION_VERSION,
            operation: ManagedSessionOperation::History,
            request_json: serde_json::to_string(&request)
                .map_err(|_| ClientError::Protocol("invalid managed history request".into()))?,
        };
        envelope
            .decode_history()
            .map_err(|e| ClientError::Protocol(e.into()))?;
        let page: bcode_harness_models::HarnessObservationPage =
            serde_json::from_str(&self.managed_session_request(envelope).await?)
                .map_err(|_| ClientError::Protocol("invalid managed history response".into()))?;
        if page.observations.len() > usize::from(request.limit)
            || page.observations.iter().any(|observation| {
                observation.session_id != request.session_id.to_string()
                    || observation.validate().is_err()
            })
        {
            return Err(ClientError::Protocol(
                "managed history bounds or owner mismatch".into(),
            ));
        }
        Ok(page)
    }

    /// Execute explicit native interaction input shared by TUI and browser composers. Ordinary
    /// prompts are never interpreted as approval. A stable response ID is mandatory; uncertainty
    /// is surfaced, never automatically replayed.
    /// # Errors
    /// Rejects stale generations, unsupported capabilities, invalid input and daemon admission.
    pub async fn managed_session_respond(
        &self,
        session_id: SessionId,
        generation: u64,
        request_id: String,
        response_id: String,
        value: HarnessResponseValue,
    ) -> Result<HarnessServiceResponse, ClientError> {
        let Some(envelope) = self.managed_session_binding(session_id).await? else {
            return Err(ClientError::Protocol(
                "session has no managed external binding".into(),
            ));
        };
        let bcode_ipc::ManagedSessionTargetSnapshot {
            instance, target, ..
        } = envelope;
        target
            .validate_owner(&session_id.to_string(), generation)
            .map_err(|_| {
                ClientError::Protocol(
                    "stale managed ownership generation; refresh before responding".into(),
                )
            })?;
        let inspect = HarnessRequest {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: format!("{response_id}:inspect"),
            instance: instance.clone(),
            kind: HarnessRequestKind::Inspect,
        };
        let inspection = self
            .managed_session_call(
                ManagedSessionOperation::Inspect,
                ManagedSessionCall {
                    session_id: Some(session_id),
                    harness: inspect,
                    parent_session_id: None,
                    assistance_destination_session_id: None,
                },
            )
            .await?;
        let HarnessServiceResponseKind::Inspection(inspection) = inspection.kind else {
            return Err(ClientError::UnexpectedResponse);
        };
        let capability = match &value {
            HarnessResponseValue::Approval { .. } => &inspection.capabilities.approval,
            HarnessResponseValue::Answer { .. } => &inspection.capabilities.questions,
            HarnessResponseValue::Assistance { .. } => &inspection.capabilities.assistance,
            HarnessResponseValue::Cancel => {
                return Err(ClientError::Protocol(
                    "use explicit interrupt; cancel is not an answer".into(),
                ));
            }
        };
        if !matches!(
            capability.support,
            HarnessSupportLevel::Supported | HarnessSupportLevel::Constrained
        ) {
            return Err(ClientError::Protocol(
                "native interaction capability unavailable".into(),
            ));
        }
        let response = HarnessResponse {
            response_id: response_id.clone(),
            session_id: session_id.to_string(),
            generation,
            request_id,
            value,
        };
        self.managed_session_call(
            ManagedSessionOperation::Respond,
            ManagedSessionCall {
                session_id: Some(session_id),
                harness: HarnessRequest {
                    schema_version: HARNESS_SCHEMA_VERSION,
                    request_id: response_id,
                    instance,
                    kind: HarnessRequestKind::Respond { target, response },
                },
                parent_session_id: None,
                assistance_destination_session_id: None,
            },
        )
        .await
    }

    /// Capture a managed composer envelope without sending input. A missing/unavailable target
    /// never permits fallback after managed dispatch; binding discovery must distinguish absent
    /// managed state from managed state whose transport has not been prepared.
    /// # Errors
    /// Rejects unavailable targets, unsupported steering, missing expected turns and invalid IDs.
    pub async fn prepare_managed_composer_submission(
        &self,
        session_id: SessionId,
        identities: (String, String),
        text: String,
        steering: bool,
        expected_native_turn_id: Option<String>,
    ) -> Result<Option<bcode_session_models::ExternalHarnessRequest>, ClientError> {
        let (submission_id, attempt_id) = identities;
        let expected_native_turn_id =
            expected_native_turn_id.filter(|value| !value.trim().is_empty());
        let Some(snapshot) = self.managed_session_binding(session_id).await? else {
            return Ok(None);
        };
        if steering && expected_native_turn_id.is_none() {
            // A plain Send is explicit next-turn input only when the harness is known managed.
            // Do not guess an active native turn or silently steer a later turn.
            return Err(ClientError::Protocol("Native steering requires /native composer steer <native-turn>; choose Queue as follow-up or /native composer next for ordinary input".into()));
        }
        let placement = if steering {
            let expected_native_turn_id = expected_native_turn_id
                .filter(|value| !value.is_empty())
                .ok_or_else(|| ClientError::Protocol(
                    "Native steering requires an explicitly selected native turn; use Queue as follow-up for ordinary input".into(),
                ))?;
            let inspected = self
                .managed_session_call(
                    ManagedSessionOperation::Inspect,
                    ManagedSessionCall {
                        session_id: Some(session_id),
                        harness: HarnessRequest {
                            schema_version: HARNESS_SCHEMA_VERSION,
                            request_id: submission_id.clone(),
                            instance: snapshot.instance.clone(),
                            kind: HarnessRequestKind::Inspect,
                        },
                        parent_session_id: None,
                        assistance_destination_session_id: None,
                    },
                )
                .await?;
            if !matches!(inspected.kind, HarnessServiceResponseKind::Inspection(ref inspection)
                if inspection.capabilities.steering.support == HarnessSupportLevel::Supported)
            {
                return Err(ClientError::Protocol(
                    "Exact native steering is not supported by this harness".into(),
                ));
            }
            bcode_harness_models::HarnessPlacement::Steer {
                expected_native_turn_id,
            }
        } else {
            if expected_native_turn_id.is_some() {
                return Err(ClientError::Protocol(
                    "Next-turn input cannot carry a steering expectation".into(),
                ));
            }
            bcode_harness_models::HarnessPlacement::NextTurn
        };
        let call = submission_call(
            snapshot.instance,
            snapshot.target.clone(),
            bcode_harness_models::HarnessSubmission {
                submission_id,
                attempt_id,
                session_id: session_id.to_string(),
                generation: snapshot.target.generation(),
                text,
                placement,
            },
        )?;
        bcode_session_models::ExternalHarnessRequest::from_request(&call.harness)
            .map(Some)
            .map_err(|_| ClientError::Protocol("Invalid managed composer preparation".into()))
    }

    /// Dispatch only a retained composer submission. Never resolves a replacement target or
    /// substitutes a model operation after a native failure.
    /// # Errors
    /// Rejects non-submit requests, invalid ownership/correlation and application failures.
    pub async fn submit_managed_composer_request(
        &self,
        retained: bcode_session_models::ExternalHarnessRequest,
    ) -> Result<bcode_session_models::ExternalHarnessResponse, ClientError> {
        let request = retained
            .request()
            .map_err(|_| ClientError::Protocol("Invalid retained managed submission".into()))?;
        request
            .validate()
            .map_err(|_| ClientError::Protocol("Invalid retained managed submission".into()))?;
        let HarnessRequestKind::Submit { target, submission } = &request.kind else {
            return Err(ClientError::Protocol(
                "Expected a managed composer submission".into(),
            ));
        };
        let call = submission_call(request.instance.clone(), target.clone(), submission.clone())?;
        if request.request_id != submission.submission_id {
            return Err(ClientError::Protocol(
                "Managed composer request identity changed".into(),
            ));
        }
        let response = self
            .managed_session_call(ManagedSessionOperation::Followup, call)
            .await?;
        bcode_session_models::ExternalHarnessResponse::from_response(&request, &response)
            .map_err(|_| ClientError::Protocol("Invalid managed composer receipt".into()))
    }

    /// Submit or steer through the managed application, retaining exact caller-selected identities.
    /// # Errors
    /// Rejects stale binding generations, invalid native-turn expectations, or daemon admission.
    pub async fn managed_session_submit(
        &self,
        session_id: SessionId,
        submission: bcode_harness_models::HarnessSubmission,
    ) -> Result<HarnessServiceResponse, ClientError> {
        submission
            .validate()
            .map_err(|_| ClientError::Protocol("invalid managed submission".into()))?;
        if submission.session_id != session_id.to_string() {
            return Err(ClientError::Protocol(
                "managed submission owner mismatch".into(),
            ));
        }
        let snapshot = self
            .managed_session_binding(session_id)
            .await?
            .ok_or_else(|| ClientError::Protocol("managed target unavailable".into()))?;
        snapshot
            .target
            .validate_owner(&submission.session_id, submission.generation)
            .map_err(|_| ClientError::Protocol("stale managed submission generation".into()))?;
        self.managed_session_submit_target(snapshot.instance, snapshot.target, submission)
            .await
    }

    /// Route one retained explicit target, including a prepared live transport. The caller must
    /// obtain this exact target from the canonical application read; requested native IDs are
    /// never promoted here. Host ownership and live-transport admission remain authoritative.
    /// # Errors
    /// Rejects stale/malformed targets or daemon admission. Prepared steering is unsupported.
    pub async fn managed_session_submit_target(
        &self,
        instance: bcode_harness_models::HarnessInstance,
        target: bcode_harness_models::HarnessSessionTarget,
        submission: bcode_harness_models::HarnessSubmission,
    ) -> Result<HarnessServiceResponse, ClientError> {
        let call = submission_call(instance, target, submission)?;
        self.managed_session_call(ManagedSessionOperation::Followup, call)
            .await
    }

    async fn managed_session_status_text(
        &self,
        session_id: SessionId,
    ) -> Result<String, ClientError> {
        let Some(snapshot) = self.managed_session_binding(session_id).await? else {
            return Ok("No managed native target is recorded".into());
        };
        let detail = match snapshot.target {
            bcode_harness_models::HarnessSessionTarget::Prepared(prepared) => format!(
                "Prepared · generation {} · open {} · requested native selection {} (unconfirmed). Not created/resumed/settled. First input: /native send {} <submission-id> <attempt-id> <text>. Existing live transport only; no automatic recreation.",
                prepared.generation,
                prepared.open_id,
                prepared
                    .requested_native_session_id
                    .as_deref()
                    .unwrap_or("new conversation"),
                prepared.generation
            ),
            bcode_harness_models::HarnessSessionTarget::Confirmed(binding) => format!(
                "Native identity confirmed: {} · generation {}. Confirmation is not settlement or assignment completion.",
                binding.native_session_id, binding.generation
            ),
        };
        let mut text = format!(
            "Managed external · plugin {} · instance {} · Bcode {}\n{detail}",
            snapshot.instance.plugin_id, snapshot.instance.instance_id, snapshot.session_id
        );
        text.retain(|ch| !ch.is_control() || ch == '\n' || ch == '\t');
        Ok(text)
    }

    async fn managed_session_history_text(
        &self,
        session_id: SessionId,
        after: Option<String>,
    ) -> Result<String, ClientError> {
        let page = self
            .managed_session_history(ManagedSessionHistoryRequest {
                session_id,
                after,
                limit: 32,
            })
            .await?;
        let mut text = String::from(
            "Canonical native observations (historical; not current ownership or completion)\n",
        );
        for observation in page.observations {
            let _ = writeln!(
                text,
                "{} · generation {} · {:?}",
                observation.observation_id, observation.generation, observation.completeness
            );
            // Do not expose vendor JSON or raw native payloads in the composer response.
            match observation.kind {
                bcode_harness_models::HarnessObservationKind::SubmissionAccepted {
                    submission_id,
                    attempt_id,
                    native_turn_id,
                } => {
                    let _ = write!(
                        text,
                        "Native submission accepted: {submission_id} · attempt {attempt_id} · native turn {native_turn_id} (not settlement, assignment completion, or replay authority)"
                    );
                }
                bcode_harness_models::HarnessObservationKind::Text { text: value, .. }
                | bcode_harness_models::HarnessObservationKind::Result { text: value } => {
                    text.push_str(&value);
                }
                bcode_harness_models::HarnessObservationKind::Interaction { interaction } => {
                    let _ = write!(
                        text,
                        "Native {:?} request {} · scope {}\n{}",
                        interaction.kind,
                        interaction.request_id,
                        interaction.scope,
                        interaction.prompt
                    );
                }
                _ => text.push_str("Native activity observation; see canonical session transcript"),
            }
            text.push('\n');
        }
        if let Some(value) = page.next_after {
            let _ = write!(text, "Next bounded read: /native history {value}");
        }
        if text.len() > 256 * 1024 {
            return Err(ClientError::Protocol(
                "native history presentation exceeds bound; reduce page size".into(),
            ));
        }
        text.retain(|ch| !ch.is_control() || ch == '\n' || ch == '\t');
        Ok(text)
    }

    /// Shared composer command for explicit native responses and bounded offline history.
    /// # Errors
    /// Returns invalid command, capability, generation, canonical-history, and transport failures.
    pub async fn managed_session_command(
        &self,
        session_id: SessionId,
        command: &str,
    ) -> Result<String, ClientError> {
        let invalid = || {
            ClientError::Protocol("usage: /native status | /native history [cursor] | /native respond <generation> <request-id> <response-id> <approve|deny|answer|choose|assist> [text] | /native send <generation> <submission-id> <attempt-id> <text> | /native steer <generation> <submission-id> <attempt-id> <expected-native-turn> <text>".into())
        };
        if command.len() > bcode_harness_models::MAX_HARNESS_TEXT_BYTES {
            return Err(invalid());
        }
        let parts = command.split_whitespace().collect::<Vec<_>>();
        match parts.as_slice() {
            ["/native", "status"] => self.managed_session_status_text(session_id).await,
            [
                "/native",
                mode @ ("send" | "steer"),
                generation,
                submission_id,
                attempt_id,
                rest @ ..,
            ] => {
                let generation = generation.parse::<u64>().map_err(|_| invalid())?;
                let (placement, text) = command_submission_placement(mode, rest)?;
                let response = self
                    .managed_session_submit(
                        session_id,
                        bcode_harness_models::HarnessSubmission {
                            submission_id: (*submission_id).into(),
                            attempt_id: (*attempt_id).into(),
                            session_id: session_id.to_string(),
                            generation,
                            text,
                            placement,
                        },
                    )
                    .await?;
                match response.kind {
                    HarnessServiceResponseKind::Receipt(receipt) => Ok(format!(
                        "Native submission delivery {:?}. Not assignment completion; no automatic replay.",
                        receipt.state
                    )),
                    HarnessServiceResponseKind::Failure(failure) => Ok(format!(
                        "Native submission not accepted: {failure:?}. No automatic replay."
                    )),
                    _ => Err(ClientError::UnexpectedResponse),
                }
            }
            ["/native", "history"] | ["/native", "history", _] => {
                self.managed_session_history_text(session_id, parts.get(2).map(ToString::to_string))
                    .await
            }
            [
                "/native",
                "respond",
                generation,
                request_id,
                response_id,
                kind,
                rest @ ..,
            ] => {
                let generation = generation.parse::<u64>().map_err(|_| invalid())?;
                let value = command_response_value(kind, rest)?;
                let response = self
                    .managed_session_respond(
                        session_id,
                        generation,
                        (*request_id).into(),
                        (*response_id).into(),
                        value,
                    )
                    .await?;
                response_command_text(&response)
            }
            _ => Err(invalid()),
        }
    }
}

/// Strict pull cursor for canonical managed-session evidence, including offline/disabled plugins.
/// A transport error leaves the last accepted cursor unchanged; reconnect does not imply replay.
#[derive(Debug, Clone)]
pub struct ExternalHistoryCursor {
    session_id: SessionId,
    query: ExternalBackendQuery,
    exhausted: bool,
}

impl ExternalHistoryCursor {
    /// Construct a cursor with explicit bounded budgets.
    /// # Errors
    /// Rejects invalid canonical history page limits or correlation keys.
    pub fn new(session_id: SessionId, query: ExternalBackendQuery) -> Result<Self, String> {
        query.validate()?;
        Ok(Self {
            session_id,
            query,
            exhausted: false,
        })
    }

    /// Canonical session to pass to the application history operation.
    #[must_use]
    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Next bounded request; absent only after a verified terminal page.
    #[must_use]
    pub const fn next_query(&self) -> Option<&ExternalBackendQuery> {
        if self.exhausted {
            None
        } else {
            Some(&self.query)
        }
    }

    /// Validate one application page and advance atomically. A lost reply can safely repeat this
    /// read, but this cursor is never submission/response/interrupt replay authority.
    /// # Errors
    /// Rejects wrong-session evidence, invalid envelopes, bounds violations, non-advancing pages,
    /// cursor contradictions, and duplicate or out-of-order sequences without changing the cursor.
    pub fn accept(&mut self, page: &ExternalBackendPage) -> Result<(), String> {
        if self.exhausted || page.events.len() > self.query.limit {
            return Err("unexpected external history page".into());
        }
        let mut previous = self.query.after_sequence;
        for event in &page.events {
            if event.session_id != self.session_id
                || previous.is_some_and(|seq| event.sequence <= seq)
            {
                return Err("external history owner or sequence mismatch".into());
            }
            let envelope = ExternalBackendEnvelope::from_event(event)?
                .ok_or_else(|| "unexpected non-external history evidence".to_owned())?;
            if envelope.record.session_id() != self.session_id.to_string() {
                return Err("external history record owner mismatch".into());
            }
            previous = Some(event.sequence);
        }
        // Bound serialization without allocating a potentially oversized copy of the page.
        serde_json::to_writer(
            Budget {
                used: 0,
                maximum: self.query.max_bytes,
            },
            &page.events,
        )
        .map_err(|_| "external history byte budget exceeded".to_owned())?;
        if page.has_more {
            let next = page
                .next_after_sequence
                .ok_or_else(|| "external page lacks continuation".to_owned())?;
            if self.query.after_sequence.is_some_and(|seq| next <= seq)
                || previous.is_some_and(|seq| next < seq)
            {
                return Err("external page continuation did not advance".into());
            }
        } else if page
            .next_after_sequence
            .is_some_and(|next| previous != Some(next))
        {
            return Err("external terminal page cursor mismatch".into());
        }
        self.query.after_sequence = page.next_after_sequence.or(previous);
        self.exhausted = !page.has_more;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_placement_keeps_steering_target_separate_from_input() {
        use bcode_harness_models::HarnessPlacement;
        let (placement, text) =
            command_submission_placement("steer", &["turn-7", "native", "input"]).unwrap();
        assert_eq!(
            placement,
            HarnessPlacement::Steer {
                expected_native_turn_id: "turn-7".into()
            }
        );
        assert_eq!(text, "native input");
        assert!(command_submission_placement("steer", &[]).is_err());
        assert!(command_submission_placement("steer", &["turn-7"]).is_err());
        assert!(command_submission_placement("send", &[]).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn auth_flow_reuses_connection_and_preserves_unrelated_event_order() {
        let socket_dir = tempfile::Builder::new()
            .prefix("bca-")
            .tempdir_in("/tmp")
            .unwrap();
        // The transport creates and validates its own private immediate socket parent.
        // tempfile's outer directory is only an owned ancestor, not the IPC runtime directory.
        let endpoint =
            bcode_ipc::IpcEndpoint::unix_socket(socket_dir.path().join("runtime/auth.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
        let operations = [
            ManagedSessionOperation::AuthBegin,
            ManagedSessionOperation::AuthStatus,
            ManagedSessionOperation::AuthHandoff,
            ManagedSessionOperation::AuthReaped,
            ManagedSessionOperation::AuthContinue,
        ];
        let server = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            for (index, operation) in operations.into_iter().enumerate() {
                let envelope = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
                let Request::ManagedSession { request } =
                    bcode_ipc::decode_request(&envelope.payload).unwrap()
                else {
                    panic!("expected managed auth request");
                };
                assert_eq!(request.operation, operation);
                assert_eq!(envelope.request_id, u64::try_from(index).unwrap() + 1);
                let event = bcode_ipc::event_envelope(&bcode_ipc::Event::SessionCatalogUpdated {
                    revision: u64::try_from(index).unwrap(),
                })
                .unwrap();
                bcode_ipc::send_envelope(&mut stream, &event).await.unwrap();
                let response = bcode_ipc::response_envelope(
                    envelope.request_id,
                    &bcode_ipc::Response::Ok(ResponsePayload::ManagedSession {
                        response: bcode_ipc::ManagedSessionResponse {
                            schema_version: MANAGED_SESSION_VERSION,
                            response_json: "{}".into(),
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
        let client_id = bcode_session_models::ClientId::new();
        let mut connection = crate::ClientConnection {
            stream,
            next_request_id: 1,
            client_id: Some(client_id),
            verified_daemon: None,
            pending_events: std::collections::VecDeque::new(),
            request_timeout: std::time::Duration::from_secs(1),
            reconnect_client: None,
            reconnect_name: std::sync::Arc::from(""),
            restore_state: crate::ConnectionRestoreState::default(),
        };
        let denied = connection
            .managed_session_auth(ManagedSessionRequest {
                schema_version: MANAGED_SESSION_VERSION,
                operation: ManagedSessionOperation::Followup,
                request_json: "{}".into(),
            })
            .await;
        assert!(matches!(denied, Err(ClientError::Protocol(_))));
        assert_eq!(connection.next_request_id, 1);
        for operation in operations {
            let response = connection
                .managed_session_auth(ManagedSessionRequest {
                    schema_version: MANAGED_SESSION_VERSION,
                    operation,
                    request_json: "{}".into(),
                })
                .await
                .unwrap();
            assert_eq!(response.response_json, "{}");
            assert_eq!(connection.client_id, Some(client_id));
        }
        for revision in 0..5 {
            assert_eq!(
                connection.recv_event().await.unwrap(),
                bcode_ipc::Event::SessionCatalogUpdated { revision }
            );
        }
        drop(connection);
        server.await.unwrap();
    }

    fn cursor() -> ExternalHistoryCursor {
        ExternalHistoryCursor::new(
            SessionId::new(),
            ExternalBackendQuery {
                after_sequence: Some(5),
                limit: 8,
                max_bytes: 4096,
                correlation_key: None,
            },
        )
        .unwrap()
    }

    fn prepared_snapshot() -> bcode_ipc::ManagedSessionTargetSnapshot {
        use bcode_harness_models::{
            HarnessAuthBinding, HarnessAuthCustody, HarnessInstance, HarnessPreparedSession,
            HarnessSessionTarget,
        };
        let session_id = SessionId::new();
        bcode_ipc::ManagedSessionTargetSnapshot {
            session_id,
            instance: HarnessInstance {
                instance_id: "instance".into(),
                plugin_id: "plugin".into(),
                executable: "/native".into(),
                config_namespace: "isolated".into(),
                auth: HarnessAuthBinding {
                    provider_id: "auth".into(),
                    profile: "work".into(),
                    credential_namespace: "work".into(),
                    custody: HarnessAuthCustody::NativeManaged,
                },
            },
            target: HarnessSessionTarget::Prepared(HarnessPreparedSession {
                open_id: "open".into(),
                session_id: session_id.to_string(),
                instance_id: "instance".into(),
                generation: 3,
                native_version: "1.0".into(),
                requested_native_session_id: Some("requested-only".into()),
            }),
        }
    }

    #[test]
    fn offline_snapshot_decode_preserves_preparation_and_rejects_foreign_owner() {
        let snapshot = prepared_snapshot();
        let json = serde_json::to_string(&Some(snapshot.clone())).unwrap();
        let decoded = decode_target_snapshot(&json, snapshot.session_id)
            .unwrap()
            .unwrap();
        assert_eq!(decoded, snapshot);
        assert!(matches!(
            decoded.target,
            bcode_harness_models::HarnessSessionTarget::Prepared(_)
        ));
        assert!(decode_target_snapshot(&json, SessionId::new()).is_err());
        assert!(decode_target_snapshot("{\"record\":{}}", snapshot.session_id).is_err());
    }

    #[test]
    fn first_input_retains_exact_prepared_target_and_cannot_steer_or_change_generation() {
        use bcode_harness_models::{HarnessPlacement, HarnessSubmission};
        let snapshot = prepared_snapshot();
        let submission = HarnessSubmission {
            submission_id: "submission".into(),
            attempt_id: "attempt".into(),
            session_id: snapshot.session_id.to_string(),
            generation: 3,
            text: "first native input".into(),
            placement: HarnessPlacement::NextTurn,
        };
        let call = submission_call(
            snapshot.instance.clone(),
            snapshot.target.clone(),
            submission.clone(),
        )
        .unwrap();
        let HarnessRequestKind::Submit {
            target,
            submission: sent,
        } = call.harness.kind
        else {
            panic!("expected native submission");
        };
        assert_eq!(target, snapshot.target);
        assert_eq!(sent, submission);
        let mut stale = submission.clone();
        stale.generation = 4;
        assert!(
            submission_call(snapshot.instance.clone(), snapshot.target.clone(), stale).is_err()
        );
        let mut steer = submission;
        steer.placement = HarnessPlacement::Steer {
            expected_native_turn_id: "unconfirmed-turn".into(),
        };
        assert!(submission_call(snapshot.instance, snapshot.target, steer).is_err());
    }

    #[tokio::test]
    async fn captured_composer_request_rejects_changed_correlation_before_transport() {
        use bcode_harness_models::{HarnessPlacement, HarnessSubmission};
        let snapshot = prepared_snapshot();
        let call = submission_call(
            snapshot.instance,
            snapshot.target,
            HarnessSubmission {
                submission_id: "submission".into(),
                attempt_id: "attempt".into(),
                session_id: snapshot.session_id.to_string(),
                generation: 3,
                text: "hello".into(),
                placement: HarnessPlacement::NextTurn,
            },
        )
        .unwrap();
        let mut retained =
            bcode_session_models::ExternalHarnessRequest::from_request(&call.harness).unwrap();
        retained.request_id = "different-submission".into();
        let client = BcodeClient::default_endpoint();
        assert!(matches!(
            client.submit_managed_composer_request(retained).await,
            Err(ClientError::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn native_composer_rejects_implicit_approval_and_missing_fences_without_transport() {
        let client = BcodeClient::default_endpoint();
        let session_id = SessionId::new();
        for command in [
            "approve",
            "/native respond",
            "/native respond stale request response approve",
            "/native respond 7 request response shell rm",
            "/native respond 7 request response approve hidden",
        ] {
            assert!(
                client
                    .managed_session_command(session_id, command)
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn generic_managed_client_cannot_admit_auth_material() {
        use bcode_harness_models::{HarnessAuthBinding, HarnessAuthCustody, HarnessInstance};
        let client = BcodeClient::default_endpoint();
        let result = client
            .managed_session_call(
                ManagedSessionOperation::AuthBegin,
                ManagedSessionCall {
                    session_id: Some(SessionId::new()),
                    parent_session_id: None,
                    assistance_destination_session_id: None,
                    harness: HarnessRequest {
                        schema_version: HARNESS_SCHEMA_VERSION,
                        request_id: "inspect".into(),
                        instance: HarnessInstance {
                            instance_id: "instance".into(),
                            plugin_id: "plugin".into(),
                            executable: "/native".into(),
                            config_namespace: "config".into(),
                            auth: HarnessAuthBinding {
                                provider_id: "auth".into(),
                                profile: "work".into(),
                                credential_namespace: "work".into(),
                                custody: HarnessAuthCustody::NativeManaged,
                            },
                        },
                        kind: HarnessRequestKind::Inspect,
                    },
                },
            )
            .await;
        assert!(matches!(result, Err(ClientError::Protocol(_))));
    }

    #[test]
    fn empty_sparse_page_with_continuation_is_not_end_of_history() {
        let mut cursor = cursor();
        cursor
            .accept(&ExternalBackendPage {
                events: vec![],
                has_more: true,
                next_after_sequence: Some(7),
            })
            .unwrap();
        assert_eq!(cursor.next_query().unwrap().after_sequence, Some(7));
    }

    #[test]
    fn ambiguous_cursor_is_rejected_without_advancing() {
        let mut cursor = cursor();
        assert!(
            cursor
                .accept(&ExternalBackendPage {
                    events: vec![],
                    has_more: true,
                    next_after_sequence: Some(5),
                })
                .is_err()
        );
        assert_eq!(cursor.next_query().unwrap().after_sequence, Some(5));
    }

    #[test]
    fn terminal_empty_history_is_readable_without_native_resume() {
        let mut cursor = cursor();
        cursor
            .accept(&ExternalBackendPage {
                events: vec![],
                has_more: false,
                next_after_sequence: None,
            })
            .unwrap();
        assert!(cursor.next_query().is_none());
    }
}
