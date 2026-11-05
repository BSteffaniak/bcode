//! Managed authentication contracts. Sensitive payloads are transient transport only;
//! they must never enter session events, model context, telemetry, or request tracing.

use serde::{Deserialize, Serialize};
use zeroize::Zeroize as _;

/// Supported execution boundary for a plugin-authenticated login method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthPreparationMode {
    /// Begin may mutate native state; positive native inactivity admission is required.
    NativeProtocol,
    /// Begin only prepares a client-terminal plan; it must not launch native login.
    ClientTerminal,
}

/// Custody is explicit; native login never implies an empty host vault profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthCustodyMode {
    /// Supported native login owns credential storage and refresh.
    NativeManaged,
    /// Host-owned, plugin-scoped vault credentials; no general enumeration is granted.
    VaultManaged,
    /// An explicit external credential source, not an ambient fallback.
    External,
}

/// Verified actual credential namespace, independent of session/instance identity.
/// The owning plugin supplies native namespace semantics; the daemon verifies selection.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AuthNamespace {
    /// Canonical enabled plugin owner.
    pub plugin_id: String,
    /// Canonical opaque identity of actual credential storage, not a display profile.
    pub credential_namespace: String,
}

impl AuthNamespace {
    /// Validate bounds, without resolving paths or inferring namespace equivalence.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        bounded(&self.plugin_id, 256) && bounded(&self.credential_namespace, 4096)
    }
}

/// Sanitized reason; provider errors and callback material are never diagnostic text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedAuthFailure {
    /// Credentials are absent.
    Missing,
    /// Credentials expired.
    Expired,
    /// Credentials were rejected.
    Rejected,
    /// Remote verification could not reach the service.
    Network,
    /// Service throttled verification/login.
    RateLimited,
    /// Managed policy disallows this operation or billing route.
    Policy,
    /// Native login is not supported by this version/protocol.
    Unsupported,
    /// An attempted mutation has no authoritative outcome; inspect, never replay.
    Uncertain,
}

/// Local readiness and remote verification are distinct facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthReadiness {
    /// No trustworthy inspection has completed (including daemon restart).
    Unknown,
    /// Usable local configuration observed; not proof of remote acceptance.
    Ready,
    /// User interaction required.
    LoginRequired,
    /// An explicit remote verification succeeded.
    Verified,
    /// Normalized inspection or verification failure.
    Blocked(ManagedAuthFailure),
}

/// Secret-free observation, not an enduring authorization to execute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedAuthStatus {
    /// Actual namespace inspected.
    pub namespace: AuthNamespace,
    /// Explicit custody route selected by admission.
    pub custody: AuthCustodyMode,
    /// Observation; callers must not silently change custody when blocked.
    pub readiness: AuthReadiness,
}

/// Live-daemon flow identity. A handle is correlation, not caller authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedAuthHandle {
    /// Unique daemon boot identity; restart always invalidates callback material.
    pub daemon_epoch: String,
    /// Monotonic identifier within this boot.
    pub flow_id: u64,
}

/// Ephemeral coordination phase, independent of session execution settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedFlowPhase {
    /// Waiting for interaction or an explicit poll.
    Pending,
    /// A one-shot command awaits acknowledgment.
    InFlight,
    /// Cleanup requested; acknowledgment is not proof of no effects.
    Cancelling,
    /// Outcome unknown; read-only inspection required, never automatic replay.
    Uncertain,
    /// An authoritative terminal auth response was observed.
    Settled,
}

/// Secret-free live daemon observation, not a durable reconnect checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedFlowSnapshot {
    /// Boot-scoped flow identity.
    pub handle: ManagedAuthHandle,
    /// Current one-shot step revision; clients reject older live observations.
    pub revision: u64,
    /// Coordination state, not session execution state.
    pub phase: ManagedFlowPhase,
    /// Explicit native readiness observation.
    pub readiness: AuthReadiness,
    /// Dedicated sensitive auth surface has an interaction; no material included.
    pub needs_sensitive_interaction: bool,
}

/// Sensitive interaction dedicated to the authentication surface, never generic Prompt.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SensitiveAuthInteraction {
    /// Native-supported browser/device login.
    Browser {
        /// Correlates a single interaction revision.
        request_id: String,
        /// Sensitive authorization URL, not a public diagnostic.
        url: String,
        /// Optional sensitive device code.
        user_code: Option<String>,
    },
    /// Hidden entry for a callback URL, one-time code, or secret.
    Input {
        /// Correlates a single interaction revision.
        request_id: String,
    },
    /// Version-gated native handoff; no fabricated RPC login protocol.
    NativeHandoff {
        /// Correlates a single interaction revision.
        request_id: String,
    },
}

impl SensitiveAuthInteraction {
    /// Interaction identity, not authority.
    #[must_use]
    pub fn request_id(&self) -> &str {
        match self {
            Self::Browser { request_id, .. }
            | Self::Input { request_id }
            | Self::NativeHandoff { request_id } => request_id,
        }
    }

    /// Validate payload bounds before presentation. URLs are not opened automatically.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        bounded(self.request_id(), 256)
            && match self {
                Self::Browser { url, user_code, .. } => {
                    bounded(url, 8192)
                        && (url.starts_with("https://")
                            || url.starts_with("http://localhost:")
                            || url.starts_with("http://127.0.0.1:"))
                        && user_code.as_ref().is_none_or(|code| bounded(code, 1024))
                }
                Self::Input { .. } | Self::NativeHandoff { .. } => true,
            }
    }
}

impl std::fmt::Debug for SensitiveAuthInteraction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SensitiveAuthInteraction { <redacted> }")
    }
}

impl Drop for SensitiveAuthInteraction {
    fn drop(&mut self) {
        if let Self::Browser { url, user_code, .. } = self {
            url.zeroize();
            if let Some(code) = user_code {
                code.zeroize();
            }
        }
    }
}

/// Dedicated hidden input. Serialization is ONLY for ephemeral, trace-exempt transport.
#[derive(Serialize, Deserialize)]
pub struct AuthSensitiveInput {
    request_id: String,
    value: String,
}

impl AuthSensitiveInput {
    /// Create bounded secret input; invalid input is cleared on drop.
    /// # Errors
    /// Returns `InvalidInput` for missing/oversized identifiers or values.
    pub fn new(request_id: String, value: String) -> Result<Self, InvalidSensitiveInput> {
        let input = Self { request_id, value };
        if !bounded(&input.request_id, 256) || !bounded(&input.value, 16384) {
            return Err(InvalidSensitiveInput);
        }
        Ok(input)
    }

    /// Match against the currently pending sensitive request.
    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// Borrow only for direct authenticated transport to the owning plugin.
    #[must_use]
    pub fn expose_for_transport(&self) -> &str {
        &self.value
    }

    /// Deserialize does not bypass semantic input bounds.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        bounded(&self.request_id, 256) && bounded(&self.value, 16384)
    }
}

impl std::fmt::Debug for AuthSensitiveInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthSensitiveInput { <redacted> }")
    }
}

impl Drop for AuthSensitiveInput {
    fn drop(&mut self) {
        self.value.zeroize();
    }
}

/// Sanitized bounded-input rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidSensitiveInput;

impl std::fmt::Display for InvalidSensitiveInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid sensitive authentication input")
    }
}
impl std::error::Error for InvalidSensitiveInput {}

fn bounded(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_payloads_redact_debug_and_bound_deserialized_input() {
        let input = AuthSensitiveInput::new("request".into(), "private-callback".into()).unwrap();
        assert!(!format!("{input:?}").contains("private-callback"));
        let effect = SensitiveAuthInteraction::Browser {
            request_id: "request".into(),
            url: "https://example.test/private".into(),
            user_code: Some("private-code".into()),
        };
        assert!(!format!("{effect:?}").contains("private"));
        assert!(effect.is_valid());
        let invalid: AuthSensitiveInput =
            serde_json::from_str(r#"{"request_id":"request","value":""}"#).unwrap();
        assert!(!invalid.is_valid());
    }

    #[test]
    fn unknown_future_custody_fails_closed() {
        assert!(serde_json::from_str::<AuthCustodyMode>("\"invented\"").is_err());
        assert_ne!(AuthReadiness::Ready, AuthReadiness::Verified);
    }
}
