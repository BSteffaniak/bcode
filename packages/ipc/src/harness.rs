//! Versioned managed-session application transport, distinct from native plugin dispatch.
use bcode_harness_models::{HarnessRequest, HarnessRequestKind, MAX_HARNESS_WIRE_BYTES};
use serde::{Deserialize, Serialize};

/// Host application interface. Native plugins use `bcode.harness/v3` instead.
pub const MANAGED_SESSION_INTERFACE_ID: &str = "bcode.managed-session/v1";
/// Application envelope version.
pub const MANAGED_SESSION_VERSION: u16 = 1;

/// Attachment-owned native polling intent. Cursor and retention evidence are host-selected.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedSessionPollRequest {
    /// Canonical Bcode session, never a native process identity.
    pub session_id: bcode_session_models::SessionId,
    /// Explicit retained configured instance.
    pub instance_id: String,
    /// Expected canonical ownership generation.
    pub generation: u64,
    /// Bounded native page size.
    pub limit: u16,
}
impl ManagedSessionPollRequest {
    /// Validate shape only; caller and canonical ownership are admitted by the host.
    /// # Errors
    /// Rejects invalid identity, zero generation or excessive page size.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.instance_id.is_empty()
            || self.instance_id.len() > 256
            || self.instance_id.trim() != self.instance_id
            || self.instance_id.chars().any(char::is_control)
            || self.generation == 0
            || self.limit == 0
            || usize::from(self.limit) > bcode_harness_models::MAX_HARNESS_OBSERVATIONS
        {
            return Err("invalid managed poll request");
        }
        Ok(())
    }
}

/// Explicit existing-session recovery intent. Host selects the new owner and generation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedSessionRecoveryRequest {
    pub session_id: bcode_session_models::SessionId,
    pub instance_id: String,
    pub expected_generation: u64,
    pub recovery_id: String,
}
impl ManagedSessionRecoveryRequest {
    /// Validate bounded shape, never caller authority or prior owner quiescence.
    /// # Errors
    /// Rejects malformed identities and invalid/overflowing generation expectations.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.expected_generation == 0
            || self.expected_generation == u64::MAX
            || [&self.instance_id, &self.recovery_id].iter().any(|value| {
                value.is_empty()
                    || value.len() > 256
                    || value.trim() != value.as_str()
                    || value.chars().any(char::is_control)
            })
        {
            return Err("invalid managed recovery request");
        }
        Ok(())
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;

    #[test]
    fn recovery_transport_requires_explicit_bounded_identity_and_distinct_family() {
        let mut query = ManagedSessionRecoveryRequest {
            session_id: bcode_session_models::SessionId::new(),
            instance_id: "selected".into(),
            expected_generation: 1,
            recovery_id: "fresh-recovery".into(),
        };
        let envelope = |query: &ManagedSessionRecoveryRequest| ManagedSessionRequest {
            schema_version: 1,
            operation: ManagedSessionOperation::Recover,
            request_json: serde_json::to_string(query).unwrap(),
        };
        assert!(envelope(&query).decode_recovery().is_ok());
        assert!(envelope(&query).decode_call().is_err());
        query.expected_generation = u64::MAX;
        assert!(envelope(&query).decode_recovery().is_err());
        query.expected_generation = 1;
        query.recovery_id = "x".repeat(257);
        assert!(envelope(&query).decode_recovery().is_err());
        query.recovery_id = "fresh-recovery".into();
        let mut request = envelope(&query);
        request.operation = ManagedSessionOperation::Start;
        assert!(request.decode_recovery().is_err());
        request.operation = ManagedSessionOperation::Recover;
        request.request_json = serde_json::json!({"session_id":query.session_id,"instance_id":"selected", "expected_generation":1,
            "recovery_id":"fresh-recovery","replay_submission":"forbidden"}).to_string();
        assert!(request.decode_recovery().is_err());
    }
}

/// Semantic application operations; correlation never conveys authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedSessionOperation {
    Start,
    Recover,
    Inspect,
    Followup,
    Respond,
    Interrupt,
    Observe,
    Close,
    Reconcile,
    History,
    AuthStatus,
    AuthBegin,
    AuthContinue,
    AuthCancel,
    AuthPrepare,
    AuthHandoff,
    /// Exact client child has exited and been reaped; never credential readiness.
    AuthReaped,
    AuthAcknowledge,
    Binding,
}
impl ManagedSessionOperation {
    /// Stable interface operation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Recover => "recover",
            Self::Binding => "binding",
            Self::Inspect => "inspect",
            Self::Followup => "followup",
            Self::Respond => "respond",
            Self::Interrupt => "interrupt",
            Self::Observe => "observe",
            Self::Close => "close",
            Self::Reconcile => "reconcile",
            Self::History => "history",
            Self::AuthStatus => "auth_status",
            Self::AuthBegin => "auth_begin",
            Self::AuthContinue => "auth_continue",
            Self::AuthCancel => "auth_cancel",
            Self::AuthPrepare => "auth_prepare",
            Self::AuthHandoff => "auth_handoff",
            Self::AuthReaped => "auth_reaped",
            Self::AuthAcknowledge => "auth_acknowledge",
        }
    }
    /// Auth interaction material must never enter ordinary tracing or model context.
    #[must_use]
    pub const fn is_auth(self) -> bool {
        matches!(
            self,
            Self::AuthStatus
                | Self::AuthBegin
                | Self::AuthContinue
                | Self::AuthCancel
                | Self::AuthPrepare
                | Self::AuthHandoff
                | Self::AuthReaped
                | Self::AuthAcknowledge
        )
    }
}

/// Bounded application request.
///
/// JSON is used only at the nested semantic contract boundary, avoiding positional
/// serialization of the shared protocol's internally tagged enums.
/// Decode and validate before admission; do not log its contents.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedSessionRequest {
    pub schema_version: u16,
    pub operation: ManagedSessionOperation,
    pub request_json: String,
}
impl Drop for ManagedSessionRequest {
    fn drop(&mut self) {
        if self.operation.is_auth() {
            zeroize::Zeroize::zeroize(&mut self.request_json);
        }
    }
}
impl std::fmt::Debug for ManagedSessionRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedSessionRequest")
            .field("schema_version", &self.schema_version)
            .field("operation", &self.operation)
            .finish_non_exhaustive()
    }
}
/// Authoritative protocol plus non-authoritative association/routing hints.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedSessionCall {
    /// Exact Bcode target for instance inspection, which has no native binding yet.
    /// Other operations carry their session identity in the shared harness contract.
    #[serde(default)]
    pub session_id: Option<bcode_session_models::SessionId>,
    pub harness: HarnessRequest,
    pub parent_session_id: Option<String>,
    pub assistance_destination_session_id: Option<String>,
}
/// Canonical offline target, including a prepared session with no native session ID.
/// Reading this evidence grants no live transport, resume, or mutation authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedSessionTargetSnapshot {
    pub session_id: bcode_session_models::SessionId,
    pub instance: bcode_harness_models::HarnessInstance,
    pub target: bcode_harness_models::HarnessSessionTarget,
}
impl ManagedSessionTargetSnapshot {
    /// Validate exact target ownership without promoting Prepared to Confirmed.
    /// # Errors
    /// Rejects malformed instances, targets, and session identity mismatches.
    pub fn validate(&self) -> Result<(), bcode_harness_models::HarnessContractError> {
        self.instance.validate()?;
        self.target
            .validate_owner(&self.session_id.to_string(), self.target.generation())?;
        let instance_id = match &self.target {
            bcode_harness_models::HarnessSessionTarget::Prepared(value) => &value.instance_id,
            bcode_harness_models::HarnessSessionTarget::Confirmed(value) => &value.instance_id,
        };
        if instance_id != &self.instance.instance_id {
            return Err(bcode_harness_models::HarnessContractError::InvalidShape);
        }
        Ok(())
    }
}

/// Indexed offline history, independent of native plugin availability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedSessionHistoryRequest {
    pub session_id: bcode_session_models::SessionId,
    pub after: Option<String>,
    pub limit: u16,
}
impl ManagedSessionRequest {
    /// Decode shared native semantics without deriving any caller or owner authority.
    /// # Errors
    /// Rejects incompatible versions, excessive input, malformed data and operation mismatch.
    pub fn decode_call(&self) -> Result<ManagedSessionCall, &'static str> {
        self.validate_envelope()?;
        let call: ManagedSessionCall = serde_json::from_str(&self.request_json)
            .map_err(|_| "invalid managed session request")?;
        call.harness
            .validate()
            .map_err(|_| "invalid harness contract")?;
        let matches = matches!(
            (&self.operation, &call.harness.kind),
            (ManagedSessionOperation::Start, HarnessRequestKind::Open(_))
                | (
                    ManagedSessionOperation::Inspect,
                    HarnessRequestKind::Inspect
                        | HarnessRequestKind::Observe(_)
                        | HarnessRequestKind::Reconcile(_)
                )
                | (
                    ManagedSessionOperation::Followup,
                    HarnessRequestKind::Submit { .. }
                )
                | (
                    ManagedSessionOperation::Respond,
                    HarnessRequestKind::Respond { .. }
                )
                | (
                    ManagedSessionOperation::Interrupt,
                    HarnessRequestKind::Interrupt(_)
                )
                | (
                    ManagedSessionOperation::Observe,
                    HarnessRequestKind::Observe(_)
                )
                | (ManagedSessionOperation::Close, HarnessRequestKind::Close(_))
                | (
                    ManagedSessionOperation::Reconcile,
                    HarnessRequestKind::Reconcile(_)
                )
        );
        if !matches {
            return Err("managed session operation mismatch");
        }
        let mutation_id = match &call.harness.kind {
            HarnessRequestKind::Open(value) => Some(value.open_id.as_str()),
            HarnessRequestKind::Submit { submission, .. } => {
                Some(submission.submission_id.as_str())
            }
            HarnessRequestKind::Respond { response, .. } => Some(response.response_id.as_str()),
            HarnessRequestKind::Interrupt(value) | HarnessRequestKind::Close(value) => {
                Some(value.mutation_id.as_str())
            }
            _ => None,
        };
        if mutation_id.is_some_and(|id| id != call.harness.request_id) {
            return Err("mutation correlation mismatch");
        }
        if call.parent_session_id.is_some() && self.operation != ManagedSessionOperation::Start {
            return Err("parent association is creation-only");
        }
        for hint in [
            &call.parent_session_id,
            &call.assistance_destination_session_id,
        ]
        .into_iter()
        .flatten()
        {
            hint.parse::<bcode_session_models::SessionId>()
                .map_err(|_| "invalid session association")?;
        }
        Ok(call)
    }
    /// Check the independently versioned bounded transport envelope.
    /// # Errors
    /// Rejects unknown versions or oversized JSON before allocation of decoded contracts.
    pub const fn validate_envelope(&self) -> Result<(), &'static str> {
        if self.schema_version != MANAGED_SESSION_VERSION {
            return Err("unsupported managed session version");
        }
        if self.request_json.len() > MAX_HARNESS_WIRE_BYTES {
            return Err("managed session request exceeds bound");
        }
        Ok(())
    }
    /// Decode an explicit recovery intent without interpreting it as a native call.
    /// # Errors
    /// Rejects wrong family, malformed/oversized envelopes and invalid exact identities.
    pub fn decode_recovery(&self) -> Result<ManagedSessionRecoveryRequest, &'static str> {
        self.validate_envelope()?;
        if self.operation != ManagedSessionOperation::Recover {
            return Err("managed session operation mismatch");
        }
        let request: ManagedSessionRecoveryRequest = serde_json::from_str(&self.request_json)
            .map_err(|_| "invalid managed recovery request")?;
        request.validate()?;
        Ok(request)
    }

    /// Validate an offline history page request.
    /// # Errors
    /// Rejects invalid shape, excessive page sizes and unbounded cursors.
    pub fn decode_history(&self) -> Result<ManagedSessionHistoryRequest, &'static str> {
        self.validate_envelope()?;
        if self.operation != ManagedSessionOperation::History {
            return Err("managed session operation mismatch");
        }
        let value: ManagedSessionHistoryRequest =
            serde_json::from_str(&self.request_json).map_err(|_| "invalid history request")?;
        if value.limit == 0
            || usize::from(value.limit) > bcode_harness_models::MAX_HARNESS_OBSERVATIONS
            || value.after.as_ref().is_some_and(|cursor| {
                cursor.len() > bcode_harness_models::MAX_HARNESS_ID_BYTES
                    || cursor.chars().any(char::is_control)
            })
        {
            return Err("history bounds exceeded");
        }
        Ok(value)
    }
}
/// Dedicated client-only auth route. Handles are correlation, never caller authority.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedSessionAuthRequest {
    pub session_id: bcode_session_models::SessionId,
    pub instance_id: String,
    pub handle: Option<bcode_provider_auth_models::managed::ManagedAuthHandle>,
    pub input: Option<bcode_provider_auth_models::managed::AuthSensitiveInput>,
    #[serde(default)]
    pub sensitive_interaction: bool,
}

/// Reserve a scoped client-terminal flow without launching native login.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedSessionPrepareHandoffRequest {
    pub session_id: bcode_session_models::SessionId,
    pub instance_id: String,
    pub interface_id: String,
}

/// Client-only request for a plugin-owned native login plan. The interface selection is
/// checked against the owning enabled plugin; it never grants generic service authority.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedSessionHandoffRequest {
    pub session_id: bcode_session_models::SessionId,
    pub instance_id: String,
    pub handle: bcode_provider_auth_models::managed::ManagedAuthHandle,
    pub interaction_id: String,
    pub interface_id: String,
}

/// Supported non-launching native login preparation method.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedNativeLoginMethod {
    pub method_id: String,
    pub support: bcode_harness_models::HarnessSupportLevel,
    pub custody: bcode_provider_auth_models::managed::AuthCustodyMode,
    pub execution: ManagedNativeLoginExecution,
    pub native_inactivity: ManagedNativeLoginInactivity,
}

/// Explicit client-owned launch surface.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedNativeLoginExecution {
    ClientTerminal,
}

/// Native activity evidence, independent of preparation permission.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedNativeLoginInactivity {
    Inactive,
    Active,
    Unknown,
}

/// Native terminal execution plan returned by the admitted plugin extension. No shell is used.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedNativeLoginPlan {
    pub schema_version: u16,
    pub namespace: bcode_provider_auth_models::managed::AuthNamespace,
    pub method: ManagedNativeLoginMethod,
    pub executable: std::path::PathBuf,
    pub expected_version: String,
    pub working_directory: std::path::PathBuf,
    pub arguments: Vec<String>,
    pub environment: std::collections::BTreeMap<String, String>,
}
impl ManagedNativeLoginPlan {
    /// Check bounded direct-execution shape; host admission must additionally bind namespace.
    /// # Errors
    /// Rejects unsupported versions, relative paths, invalid environment and excessive data.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != 1
            || self.method.method_id.is_empty()
            || self.method.method_id.len() > bcode_harness_models::MAX_HARNESS_ID_BYTES
            || self.method.method_id.trim() != self.method.method_id
            || self.method.method_id.chars().any(char::is_control)
            || self.method.support != bcode_harness_models::HarnessSupportLevel::Supported
            || self.method.custody
                != bcode_provider_auth_models::managed::AuthCustodyMode::NativeManaged
            || matches!(
                self.method.native_inactivity,
                ManagedNativeLoginInactivity::Active
            )
            || !self.namespace.is_valid()
            || !self.executable.is_absolute()
            || !self.working_directory.is_absolute()
            || self.expected_version.is_empty()
            || self.expected_version.len() > 256
            || self.expected_version.chars().any(char::is_control)
            || self.arguments.is_empty()
            || self.arguments.len() > 32
            || self.environment.len() > 32
            || self
                .arguments
                .iter()
                .any(|arg| arg.len() > 4096 || arg.chars().any(char::is_control))
            || self.environment.iter().any(|(key, value)| {
                key.is_empty()
                    || key.len() > 128
                    || !key
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                    || value.len() > 16384
                    || value.contains('\0')
            })
        {
            return Err("invalid native login plan");
        }
        Ok(())
    }
}

/// Redacted debug output also applies to sensitive authentication responses.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedSessionResponse {
    pub schema_version: u16,
    pub response_json: String,
}
impl Drop for ManagedSessionResponse {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.response_json);
    }
}
impl std::fmt::Debug for ManagedSessionResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedSessionResponse")
            .field("schema_version", &self.schema_version)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn attachment_poll_request_is_bounded_and_cannot_supply_cursor_or_retention() {
        let mut query = super::ManagedSessionPollRequest {
            session_id: bcode_session_models::SessionId::new(),
            instance_id: "primary".into(),
            generation: 1,
            limit: 64,
        };
        query.validate().unwrap();
        for field in ["after", "retention", "target"] {
            let mut json = serde_json::to_value(&query).unwrap();
            json[field] = serde_json::json!("client-inferred");
            assert!(serde_json::from_value::<super::ManagedSessionPollRequest>(json).is_err());
        }
        query.limit = 0;
        assert!(query.validate().is_err());
        query.limit = u16::MAX;
        assert!(query.validate().is_err());
        query.limit = 1;
        query.generation = 0;
        assert!(query.validate().is_err());
    }

    use super::*;
    #[test]
    fn incompatible_and_oversized_requests_fail_closed() {
        let mut request = ManagedSessionRequest {
            schema_version: 2,
            operation: ManagedSessionOperation::Inspect,
            request_json: "{}".into(),
        };
        assert!(request.decode_call().is_err());
        request.schema_version = 1;
        request.request_json = "x".repeat(MAX_HARNESS_WIRE_BYTES + 1);
        assert!(request.validate_envelope().is_err());
    }
    #[test]
    fn debug_never_contains_sensitive_material() {
        let request = ManagedSessionRequest {
            schema_version: 1,
            operation: ManagedSessionOperation::AuthContinue,
            request_json: "private-callback".into(),
        };
        assert!(!format!("{request:?}").contains("private-callback"));
        let response = ManagedSessionResponse {
            schema_version: 1,
            response_json: "private-callback".into(),
        };
        assert!(!format!("{response:?}").contains("private-callback"));
    }
    #[test]
    fn offline_target_snapshot_preserves_prepared_without_native_binding() {
        let snapshot = ManagedSessionTargetSnapshot {
            session_id: "062eb7ca-1970-4e5c-9f2e-43121a388b11".parse().unwrap(),
            instance: serde_json::from_value(serde_json::json!({"instance_id":"instance","plugin_id":"native",
                "executable":"/explicit/native","config_namespace":"/isolated/config",
                "auth":{"provider_id":"auth","profile":"native","credential_namespace":"/isolated/config","custody":"native_managed"}})).unwrap(),
            target: bcode_harness_models::HarnessSessionTarget::Prepared(bcode_harness_models::HarnessPreparedSession {
                open_id: "open".into(), session_id: "062eb7ca-1970-4e5c-9f2e-43121a388b11".into(),
                instance_id: "instance".into(), generation: 1, native_version: "1.2.3".into(), requested_native_session_id: None,
            }),
        };
        snapshot.validate().unwrap();
        let encoded = serde_json::to_string(&Some(snapshot.clone())).unwrap();
        let decoded: Option<ManagedSessionTargetSnapshot> = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, Some(snapshot.clone()));
        let mut foreign = snapshot;
        foreign.session_id = bcode_session_models::SessionId::new();
        assert!(foreign.validate().is_err());
    }

    #[test]
    fn prepared_target_remains_prepared_across_application_and_ipc_roundtrip() {
        use bcode_harness_models::*;
        let prepared = HarnessPreparedSession {
            open_id: "open".into(),
            session_id: "062eb7ca-1970-4e5c-9f2e-43121a388b11".into(),
            instance_id: "instance".into(),
            generation: 1,
            native_version: "1.2.3".into(),
            requested_native_session_id: None,
        };
        let call = ManagedSessionCall {
            session_id: None,
            parent_session_id: None,
            assistance_destination_session_id: None,
            harness: HarnessRequest {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: "submit".into(),
                instance: HarnessInstance {
                    instance_id: "instance".into(),
                    plugin_id: "native".into(),
                    executable: "/explicit/native".into(),
                    config_namespace: "/isolated/config".into(),
                    auth: HarnessAuthBinding {
                        provider_id: "auth".into(),
                        profile: "native".into(),
                        credential_namespace: "/isolated/config".into(),
                        custody: HarnessAuthCustody::NativeManaged,
                    },
                },
                kind: HarnessRequestKind::Submit {
                    target: HarnessSessionTarget::Prepared(prepared.clone()),
                    submission: HarnessSubmission {
                        submission_id: "submit".into(),
                        attempt_id: "attempt".into(),
                        session_id: prepared.session_id.clone(),
                        generation: 1,
                        text: "explicit first input".into(),
                        placement: HarnessPlacement::NextTurn,
                    },
                },
            },
        };
        let request = ManagedSessionRequest {
            schema_version: MANAGED_SESSION_VERSION,
            operation: ManagedSessionOperation::Followup,
            request_json: serde_json::to_string(&call).unwrap(),
        };
        let decoded = request.decode_call().unwrap();
        assert!(
            matches!(decoded.harness.kind, HarnessRequestKind::Submit { target: HarnessSessionTarget::Prepared(value), .. } if value == prepared)
        );
        let response = ManagedSessionResponse {
            schema_version: MANAGED_SESSION_VERSION,
            response_json: serde_json::to_string(&HarnessServiceResponse {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: "open".into(),
                operation: HarnessOperation::Open,
                kind: HarnessServiceResponseKind::Prepared(prepared.clone()),
            })
            .unwrap(),
        };
        let response = crate::Response::Ok(crate::ResponsePayload::ManagedSession { response });
        let decoded = crate::decode_response(&crate::encode_response(&response).unwrap()).unwrap();
        let crate::Response::Ok(crate::ResponsePayload::ManagedSession { response }) = decoded
        else {
            panic!("wrong response");
        };
        let decoded: HarnessServiceResponse =
            serde_json::from_str(&response.response_json).unwrap();
        assert!(
            matches!(decoded.kind, HarnessServiceResponseKind::Prepared(value) if value == prepared)
        );
    }

    #[test]
    fn native_login_plan_is_explicit_bounded_and_never_a_shell_string() {
        let mut plan = ManagedNativeLoginPlan {
            schema_version: 1,
            namespace: bcode_provider_auth_models::managed::AuthNamespace {
                plugin_id: "native".into(),
                credential_namespace: "/isolated/config".into(),
            },
            method: ManagedNativeLoginMethod {
                method_id: "native-login".into(),
                support: bcode_harness_models::HarnessSupportLevel::Supported,
                custody: bcode_provider_auth_models::managed::AuthCustodyMode::NativeManaged,
                execution: ManagedNativeLoginExecution::ClientTerminal,
                native_inactivity: ManagedNativeLoginInactivity::Unknown,
            },
            executable: "/explicit/native".into(),
            expected_version: "1.2.3".into(),
            working_directory: "/isolated/workspace".into(),
            arguments: vec!["auth".into(), "login".into()],
            environment: std::collections::BTreeMap::from([(
                "NATIVE_CONFIG_DIR".into(),
                "/isolated/config".into(),
            )]),
        };
        assert!(plan.validate().is_ok());
        plan.method.native_inactivity = ManagedNativeLoginInactivity::Active;
        assert!(plan.validate().is_err());
        plan.method.native_inactivity = ManagedNativeLoginInactivity::Unknown;
        plan.method.support = bcode_harness_models::HarnessSupportLevel::Unsupported;
        assert!(plan.validate().is_err());
        plan.method.support = bcode_harness_models::HarnessSupportLevel::Supported;
        plan.executable = "native".into();
        assert!(plan.validate().is_err());
        plan.executable = "/explicit/native".into();
        plan.environment
            .insert("INVALID=KEY".into(), "value".into());
        assert!(plan.validate().is_err());
    }

    #[test]
    fn managed_application_envelopes_roundtrip_through_real_ipc_codec() {
        let request = crate::Request::ManagedSession { request: Box::new(ManagedSessionRequest {
            schema_version: MANAGED_SESSION_VERSION, operation: ManagedSessionOperation::AuthContinue,
            request_json: serde_json::json!({"session_id":"062eb7ca-1970-4e5c-9f2e-43121a388b11", "input":{"request_id":"step", "value":"private-callback"}}).to_string(),
        }) };
        let wire = crate::encode_request(&request).unwrap();
        let decoded = crate::decode_request(&wire).unwrap();
        assert_eq!(decoded, request);
        assert!(!format!("{decoded:?}").contains("private-callback"));
        let response = crate::Response::Ok(crate::ResponsePayload::ManagedSession {
            response: ManagedSessionResponse {
                schema_version: MANAGED_SESSION_VERSION,
                response_json: "{\"phase\":\"uncertain\"}".into(),
            },
        });
        assert_eq!(
            crate::decode_response(&crate::encode_response(&response).unwrap()).unwrap(),
            response
        );
    }

    #[test]
    fn history_is_bounded_and_does_not_accept_authority_fields() {
        let request = ManagedSessionRequest { schema_version: 1, operation: ManagedSessionOperation::History,
            request_json: serde_json::json!({"session_id": "062eb7ca-1970-4e5c-9f2e-43121a388b11", "after": null, "limit": 129}).to_string() };
        assert!(request.decode_history().is_err());
    }
}
