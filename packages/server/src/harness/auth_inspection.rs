//! Host consumption of optional plugin-native auth admission inspection.
//!
//! This read-only auth-status extension is transient, bounded and never canonical session data.
//! Missing evidence preserves ordinary status readability but cannot authorize a new login.
use super::ManagedHarnessError;
use bcode_harness_models::HarnessInstance;
use bcode_provider_auth_models::managed::{AuthReadiness, ManagedAuthStatus};
use serde::{Deserialize, Serialize};

/// Positive native login activity observation. An empty host registry is never `Inactive`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessNativeLoginState {
    /// Plugin positively verified no unresolved native login in this exact namespace.
    Inactive,
    /// Plugin observed a native login; do not start another one.
    Active,
    /// Native login state cannot be verified, including after process loss.
    Unknown,
}

/// Plugin-declared execution boundary. Terminal preparation is not native inactivity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessAuthExecution {
    /// Begin can perform native login effects and requires positive inactivity evidence.
    NativeProtocol,
    /// Begin prepares only; an explicit client owns any later launch and reap.
    ClientTerminal,
}

/// Typed method evidence from the selected plugin's bounded, namespace-bound handoff plan.
/// Capability evidence alone never reserves a namespace or authorizes native execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessAuthMethodEvidence {
    /// Exact plugin-selected method, not caller input.
    pub method_id: String,
    /// Only positive Supported evidence qualifies for preparation.
    pub support: bcode_harness_models::HarnessSupportLevel,
    /// Exact credential custody.
    pub custody: bcode_provider_auth_models::managed::AuthCustodyMode,
    /// Typed execution boundary, independent of plugin identity.
    pub execution: HarnessAuthExecution,
    /// Independent native activity observation; Unknown is never upgraded to Inactive.
    pub native_inactivity: HarnessNativeLoginState,
}

impl HarnessAuthMethodEvidence {
    /// Validate non-launching preparation capability. Namespace reservation, prior-issued
    /// claims and explicit client launch/reap fencing remain mandatory dispatch checks.
    /// # Errors
    /// Rejects unsupported/constrained capability, daemon execution, active login or wrong custody.
    pub fn validate_client_terminal(
        &self,
        instance: &HarnessInstance,
    ) -> Result<(), ManagedHarnessError> {
        use bcode_provider_auth_models::managed::AuthCustodyMode;
        if self.support != bcode_harness_models::HarnessSupportLevel::Supported
            || self.execution != HarnessAuthExecution::ClientTerminal
            || self.custody != AuthCustodyMode::NativeManaged
            || instance.auth.custody != self.custody
        {
            return Err(ManagedHarnessError::Unsupported);
        }
        if self.native_inactivity == HarnessNativeLoginState::Active {
            return Err(ManagedHarnessError::Uncertain);
        }
        if self.method_id.trim().is_empty()
            || self.method_id.trim() != self.method_id
            || self.method_id.len() > bcode_harness_models::MAX_HARNESS_ID_BYTES
            || self.method_id.chars().any(char::is_control)
        {
            return Err(ManagedHarnessError::Invalid);
        }
        Ok(())
    }
}

/// Optional `admission` field inside the existing auth-status response payload.
///
/// The producer must derive method and login activity from its supported native protocol,
/// never from display names, credential readiness alone, or an empty daemon flow registry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessAuthAdmissionEvidence {
    /// Independent inspection schema; only one is supported.
    pub schema_version: u16,
    /// Exact enabled native executable/config/credential selection inspected.
    pub instance: HarnessInstance,
    /// Native method selected by the plugin for this instance/custody, not host-guessed.
    pub method_id: String,
    /// Positive native login activity evidence, independent of credential readiness.
    pub native_login: HarnessNativeLoginState,
}

/// Read-only inspected status plus optional native admission evidence.
/// A status without evidence remains actionable to clients, but grants no new begin.
pub struct InspectedHarnessAuth {
    /// Sanitized, namespace-bound readiness.
    pub status: ManagedAuthStatus,
    /// Optional plugin-derived method and native-login activity.
    pub admission: Option<HarnessAuthAdmissionEvidence>,
}

impl HarnessAuthAdmissionEvidence {
    /// Validate plugin evidence against the exact canonical selection and status.
    /// # Errors
    /// Rejects unsupported versions, invalid methods and contradictory instance/namespace/custody.
    pub fn validate_for(
        &self,
        instance: &HarnessInstance,
        status: &ManagedAuthStatus,
    ) -> Result<(), ManagedHarnessError> {
        if self.schema_version != 1 {
            return Err(ManagedHarnessError::Unsupported);
        }
        if self.instance != *instance
            || status.namespace.plugin_id != instance.plugin_id
            || status.namespace.credential_namespace != instance.auth.credential_namespace
            || status.custody != instance.auth.custody
        {
            return Err(ManagedHarnessError::StaleOwner);
        }
        if self.method_id.trim().is_empty()
            || self.method_id.trim() != self.method_id
            || self.method_id.len() > bcode_harness_models::MAX_HARNESS_ID_BYTES
            || self.method_id.chars().any(char::is_control)
        {
            return Err(ManagedHarnessError::Invalid);
        }
        Ok(())
    }

    /// Credential readiness is not inactivity proof. Blocked/unknown readiness cannot begin.
    #[must_use]
    pub fn permits_fresh_begin(&self, status: &ManagedAuthStatus) -> bool {
        self.native_login == HarnessNativeLoginState::Inactive
            && matches!(
                status.readiness,
                AuthReadiness::Ready | AuthReadiness::Verified | AuthReadiness::LoginRequired
            )
    }
}

/// Decode an already bounded auth-status envelope without changing the ordinary wire variant.
/// Unknown admission versions and malformed evidence fail closed instead of being discarded.
pub(super) fn decode_inspection(
    bytes: &[u8],
) -> Result<Option<HarnessAuthAdmissionEvidence>, ManagedHarnessError> {
    #[derive(Deserialize)]
    struct Envelope {
        kind: StatusKind,
    }
    #[derive(Deserialize)]
    #[serde(tag = "kind", content = "response", rename_all = "snake_case")]
    enum StatusKind {
        AuthStatus(Extension),
    }
    #[derive(Deserialize)]
    struct Extension {
        #[serde(default)]
        admission: Option<HarnessAuthAdmissionEvidence>,
    }
    let envelope: Envelope =
        serde_json::from_slice(bytes).map_err(|_| ManagedHarnessError::Invalid)?;
    let StatusKind::AuthStatus(extension) = envelope.kind;
    Ok(extension.admission)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_harness_models::{HarnessAuthBinding, HarnessAuthCustody};
    use bcode_provider_auth_models::managed::AuthNamespace;

    fn fixture() -> (HarnessAuthAdmissionEvidence, ManagedAuthStatus) {
        let instance = HarnessInstance {
            instance_id: "native-instance".into(),
            plugin_id: "native".into(),
            executable: "/native".into(),
            config_namespace: "/selected".into(),
            auth: HarnessAuthBinding {
                provider_id: "native-auth".into(),
                profile: "selected".into(),
                credential_namespace: "/selected".into(),
                custody: HarnessAuthCustody::NativeManaged,
            },
        };
        let status = ManagedAuthStatus {
            namespace: AuthNamespace {
                plugin_id: instance.plugin_id.clone(),
                credential_namespace: instance.auth.credential_namespace.clone(),
            },
            custody: instance.auth.custody,
            readiness: AuthReadiness::LoginRequired,
        };
        (
            HarnessAuthAdmissionEvidence {
                schema_version: 1,
                instance,
                method_id: "plugin-selected".into(),
                native_login: HarnessNativeLoginState::Inactive,
            },
            status,
        )
    }

    #[test]
    fn readiness_alone_is_never_native_inactivity_evidence() {
        let (mut evidence, status) = fixture();
        assert!(evidence.permits_fresh_begin(&status));
        evidence.native_login = HarnessNativeLoginState::Unknown;
        assert!(!evidence.permits_fresh_begin(&status));
        evidence.native_login = HarnessNativeLoginState::Active;
        assert!(!evidence.permits_fresh_begin(&status));
    }

    #[test]
    fn inspection_rejects_wrong_namespace_and_future_schema() {
        let (mut evidence, mut status) = fixture();
        assert!(evidence.validate_for(&evidence.instance, &status).is_ok());
        status.namespace.credential_namespace = "/other".into();
        assert_eq!(
            evidence.validate_for(&evidence.instance, &status),
            Err(ManagedHarnessError::StaleOwner)
        );
        evidence.schema_version = 2;
        assert_eq!(
            evidence.validate_for(&evidence.instance, &status),
            Err(ManagedHarnessError::Unsupported)
        );
    }

    #[test]
    fn terminal_preparation_does_not_assert_native_inactivity() {
        use bcode_provider_auth_models::managed::AuthCustodyMode;
        let (evidence, status) = fixture();
        let mut method = HarnessAuthMethodEvidence {
            method_id: evidence.method_id.clone(),
            support: bcode_harness_models::HarnessSupportLevel::Supported,
            custody: AuthCustodyMode::NativeManaged,
            execution: HarnessAuthExecution::ClientTerminal,
            native_inactivity: HarnessNativeLoginState::Unknown,
        };
        assert_eq!(method.validate_client_terminal(&evidence.instance), Ok(()));
        let mut native = evidence;
        native.native_login = HarnessNativeLoginState::Unknown;
        assert!(!native.permits_fresh_begin(&status));
        method.execution = HarnessAuthExecution::NativeProtocol;
        assert_eq!(
            method.validate_client_terminal(&native.instance),
            Err(ManagedHarnessError::Unsupported)
        );
        method.execution = HarnessAuthExecution::ClientTerminal;
        method.native_inactivity = HarnessNativeLoginState::Active;
        assert_eq!(
            method.validate_client_terminal(&native.instance),
            Err(ManagedHarnessError::Uncertain)
        );
        method.native_inactivity = HarnessNativeLoginState::Unknown;
        method.support = bcode_harness_models::HarnessSupportLevel::Constrained;
        assert_eq!(
            method.validate_client_terminal(&native.instance),
            Err(ManagedHarnessError::Unsupported)
        );
    }

    #[test]
    fn legacy_status_remains_readable_but_grants_no_admission() {
        let bytes = br#"{"kind":{"kind":"auth_status","response":{"namespace":{},"custody":"native_managed","readiness":"login_required"}}}"#;
        assert!(decode_inspection(bytes).unwrap().is_none());
        let (evidence, _) = fixture();
        let bytes = serde_json::to_vec(
            &serde_json::json!({"kind":{"kind":"auth_status","response":{"admission":evidence}}}),
        )
        .unwrap();
        assert_eq!(
            decode_inspection(&bytes).unwrap().unwrap().method_id,
            "plugin-selected"
        );
    }
}
