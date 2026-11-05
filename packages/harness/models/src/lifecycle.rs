//! Portable harness identities, capabilities and semantic observations.
use crate::{
    HARNESS_SCHEMA_VERSION, HarnessContractError, MAX_HARNESS_ITEMS, MAX_HARNESS_VENDOR_BYTES, id,
    text, wire,
};
use serde::{Deserialize, Serialize};

/// Reuse provider-auth custody rather than defining a second auth subsystem.
pub use bcode_provider_auth_models::managed::AuthCustodyMode as HarnessAuthCustody;

/// Reference to the existing plugin-owned provider-auth registration and profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessAuthBinding {
    pub provider_id: String,
    pub profile: String,
    /// Actual shared native credential namespace; used to coordinate login waiters.
    pub credential_namespace: String,
    pub custody: HarnessAuthCustody,
}
impl HarnessAuthBinding {
    /// Validate non-secret references. Does not read credentials or prove readiness.
    /// # Errors
    /// Returns an error for invalid or excessive identities.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        id(&self.provider_id)?;
        id(&self.profile)?;
        id(&self.credential_namespace)
    }
}

/// Explicit executable/config selection. Host resolves paths under its admission policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessInstance {
    pub instance_id: String,
    pub plugin_id: String,
    pub executable: String,
    pub config_namespace: String,
    pub auth: HarnessAuthBinding,
}
impl HarnessInstance {
    /// Validate bounded instance references, not filesystem authority.
    /// # Errors
    /// Returns an error for invalid or excessive identity/config fields.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        id(&self.instance_id)?;
        id(&self.plugin_id)?;
        text(&self.executable)?;
        if self.executable.trim().is_empty() {
            return Err(HarnessContractError::InvalidShape);
        }
        text(&self.config_namespace)?;
        if self.config_namespace.trim().is_empty() {
            return Err(HarnessContractError::InvalidShape);
        }
        self.auth.validate()
    }
}

/// Native-confirmed Bcode/native association, never an echo of requested selection.
/// Generation must match verified host ownership; transport readiness alone is insufficient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessSessionBinding {
    pub schema_version: u16,
    pub session_id: String,
    pub instance_id: String,
    pub native_session_id: String,
    pub native_version: String,
    pub generation: u64,
}
impl HarnessSessionBinding {
    /// Validate compatibility and bounded identity.
    /// # Errors
    /// Rejects unknown versions, missing identities and generation zero.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        if self.schema_version != HARNESS_SCHEMA_VERSION {
            return Err(HarnessContractError::UnsupportedVersion);
        }
        id(&self.session_id)?;
        id(&self.instance_id)?;
        id(&self.native_session_id)?;
        id(&self.native_version)?;
        if self.generation == 0 {
            return Err(HarnessContractError::InvalidShape);
        }
        Ok(())
    }
    /// Check one inbound mutation against an already verified host binding.
    /// # Errors
    /// Returns wrong-owner or stale-generation errors; never transfers authority.
    pub fn validate_owner(
        &self,
        session_id: &str,
        generation: u64,
    ) -> Result<(), HarnessContractError> {
        self.validate()?;
        if self.session_id != session_id {
            return Err(HarnessContractError::WrongOwner);
        }
        if self.generation != generation {
            return Err(HarnessContractError::StaleGeneration);
        }
        Ok(())
    }
}

/// Prepared live transport correlation, never proof of native creation or resume.
///
/// A first submission is allowed only on the existing host-owned live transport.
/// Process loss before confirmation is uncertain; this DTO never authorizes replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessPreparedSession {
    pub open_id: String,
    pub session_id: String,
    pub instance_id: String,
    pub generation: u64,
    pub native_version: String,
    /// Requested selection only, not an identity observed from the native process.
    pub requested_native_session_id: Option<String>,
}
impl HarnessPreparedSession {
    /// Validate bounded correlation fields without asserting native confirmation.
    /// # Errors
    /// Rejects invalid IDs, versions or generation zero.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        id(&self.open_id)?;
        id(&self.session_id)?;
        id(&self.instance_id)?;
        id(&self.native_version)?;
        if self.generation == 0 {
            return Err(HarnessContractError::InvalidShape);
        }
        if let Some(selection) = &self.requested_native_session_id {
            id(selection)?;
        }
        Ok(())
    }
}

/// Explicit routing state.
///
/// Neither variant grants execution authority; the host
/// verifies live ownership and byte-equivalent retained target before every operation.
/// In particular observe/control requests must not substitute a caller-selected child,
/// generation, open identity or native selection for the host's admitted target.
/// Prepared targets cannot be used to resume or recreate a transport after process loss.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "session", rename_all = "snake_case")]
pub enum HarnessSessionTarget {
    Prepared(HarnessPreparedSession),
    Confirmed(HarnessSessionBinding),
}
impl HarnessSessionTarget {
    /// Validate shape, not authority or process liveness.
    /// # Errors
    /// Rejects malformed target fields and unsupported confirmed binding versions.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        match self {
            Self::Prepared(value) => value.validate(),
            Self::Confirmed(value) => value.validate(),
        }
    }
    /// Canonical Bcode identity, independent of native identity confirmation.
    #[must_use]
    pub fn session_id(&self) -> &str {
        match self {
            Self::Prepared(value) => &value.session_id,
            Self::Confirmed(value) => &value.session_id,
        }
    }
    /// Explicit instance routing identity.
    #[must_use]
    pub fn instance_id(&self) -> &str {
        match self {
            Self::Prepared(value) => &value.instance_id,
            Self::Confirmed(value) => &value.instance_id,
        }
    }
    /// Host ownership generation; never inferred from native process state.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        match self {
            Self::Prepared(value) => value.generation,
            Self::Confirmed(value) => value.generation,
        }
    }
    /// Validate exact session/generation correlation, not ownership authority.
    /// # Errors
    /// Rejects malformed targets, foreign sessions and stale generations.
    pub fn validate_owner(
        &self,
        session_id: &str,
        generation: u64,
    ) -> Result<(), HarnessContractError> {
        self.validate()?;
        if self.session_id() != session_id {
            return Err(HarnessContractError::WrongOwner);
        }
        if self.generation() != generation {
            return Err(HarnessContractError::StaleGeneration);
        }
        Ok(())
    }
    /// Check native-confirmation evidence against this retained routing target.
    /// Host must additionally verify native provenance before canonical promotion.
    /// # Errors
    /// Rejects open, instance, native selection, version or generation contradictions.
    pub fn validate_confirmation(
        &self,
        open_id: &str,
        binding: &HarnessSessionBinding,
    ) -> Result<(), HarnessContractError> {
        id(open_id)?;
        binding.validate()?;
        self.validate_owner(&binding.session_id, binding.generation)?;
        if self.instance_id() != binding.instance_id {
            return Err(HarnessContractError::WrongOwner);
        }
        match self {
            Self::Prepared(value) => {
                if value.open_id != open_id
                    || value.native_version != binding.native_version
                    || value
                        .requested_native_session_id
                        .as_ref()
                        .is_some_and(|selection| selection != &binding.native_session_id)
                {
                    return Err(HarnessContractError::WrongOwner);
                }
            }
            Self::Confirmed(value) if value != binding => {
                return Err(HarnessContractError::WrongOwner);
            }
            Self::Confirmed(_) => {}
        }
        Ok(())
    }
}

/// Capability support is conditional, not a guessed boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessSupportLevel {
    Unsupported,
    Supported,
    Constrained,
    Unknown,
}
/// Semantic capability declaration with bounded, user-visible limitations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessCapability {
    pub support: HarnessSupportLevel,
    pub constraints: Vec<String>,
}
impl HarnessCapability {
    fn validate(&self) -> Result<(), HarnessContractError> {
        if self.constraints.len() > MAX_HARNESS_ITEMS {
            return Err(HarnessContractError::BoundsExceeded);
        }
        if self.support == HarnessSupportLevel::Constrained && self.constraints.is_empty() {
            return Err(HarnessContractError::InvalidShape);
        }
        for value in &self.constraints {
            text(value)?;
        }
        Ok(())
    }
}
/// Plugin-owned capabilities. Historical readability is independent of resume support.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessCapabilities {
    pub resume: HarnessCapability,
    pub steering: HarnessCapability,
    pub approval: HarnessCapability,
    pub questions: HarnessCapability,
    pub assistance: HarnessCapability,
    pub interrupt: HarnessCapability,
    pub authoritative_settlement: HarnessCapability,
    pub background_continuation: HarnessCapability,
    pub authentication: HarnessCapability,
}
impl HarnessCapabilities {
    /// Validate bounded capability descriptions.
    /// # Errors
    /// Returns an error for excessive or inconsistent declarations.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        for value in [
            &self.resume,
            &self.steering,
            &self.approval,
            &self.questions,
            &self.assistance,
            &self.interrupt,
            &self.authoritative_settlement,
            &self.background_continuation,
            &self.authentication,
        ] {
            value.validate()?;
        }
        Ok(())
    }
}

/// Explicit placement: steering must name the native turn being changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HarnessPlacement {
    NextTurn,
    Steer { expected_native_turn_id: String },
}
/// Retained mutation identity. Retrying observation is safe; resending is not implied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessSubmission {
    pub submission_id: String,
    pub attempt_id: String,
    pub session_id: String,
    pub generation: u64,
    pub text: String,
    pub placement: HarnessPlacement,
}
impl HarnessSubmission {
    /// Validate bounded retained submission fields.
    /// # Errors
    /// Rejects malformed identities, generation zero and excessive text.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        id(&self.submission_id)?;
        id(&self.attempt_id)?;
        id(&self.session_id)?;
        text(&self.text)?;
        if self.generation == 0 {
            return Err(HarnessContractError::InvalidShape);
        }
        if let HarnessPlacement::Steer {
            expected_native_turn_id,
        } = &self.placement
        {
            id(expected_native_turn_id)?;
        }
        Ok(())
    }
}
/// Native acknowledgement is not proof of completion or absence of side effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessDeliveryState {
    Accepted,
    Prepared,
    DeliveryAttempted,
    NativeAccepted,
    Executing,
    Settled,
    Uncertain,
    Rejected,
}
/// Delivery evidence, retained by canonical host storage rather than a plugin journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessReceipt {
    pub submission_id: String,
    pub attempt_id: String,
    pub session_id: String,
    pub generation: u64,
    pub state: HarnessDeliveryState,
    pub native_request_id: Option<String>,
}

macro_rules! semantic_enum {
    ($name:ident { $($variant:ident),+ }) => {
        #[doc = "Orthogonal normalized execution evidence; unknown is not terminal."]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }
    };
}
semantic_enum!(HarnessConnectivity {
    Connected,
    Disconnected,
    Unknown
});
semantic_enum!(HarnessActivity {
    Idle,
    Running,
    Interrupted,
    Settled,
    Unknown
});
semantic_enum!(HarnessAttention {
    None,
    Approval,
    Question,
    Assistance,
    Authentication,
    Unknown
});
semantic_enum!(HarnessContinuation {
    Available,
    Unavailable,
    Unknown
});
semantic_enum!(HarnessCompleteness {
    Complete,
    Partial,
    Gap,
    Unknown
});
/// Connectivity, activity, attention and completeness must not be conflated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessExecutionStatus {
    pub connectivity: HarnessConnectivity,
    pub activity: HarnessActivity,
    pub attention: HarnessAttention,
    pub continuation: HarnessContinuation,
    pub completeness: HarnessCompleteness,
}

/// Interaction category, not a Bcode tool-dispatch request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessInteractionKind {
    Approval,
    Question,
    Assistance,
}
/// Bounded choice with a stable machine identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessChoice {
    pub choice_id: String,
    pub label: String,
}
/// A native request observed under the canonical generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessInteraction {
    pub request_id: String,
    pub session_id: String,
    pub generation: u64,
    pub kind: HarnessInteractionKind,
    pub prompt: String,
    /// Human-readable requested scope; grants no authority.
    pub scope: String,
    pub choices: Vec<HarnessChoice>,
}
impl HarnessInteraction {
    /// Validate bounded interaction shape, including unique choices.
    /// # Errors
    /// Rejects invalid identity, generation, duplicate choices and excessive content.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        id(&self.request_id)?;
        id(&self.session_id)?;
        text(&self.prompt)?;
        text(&self.scope)?;
        if self.generation == 0 {
            return Err(HarnessContractError::InvalidShape);
        }
        if self.choices.len() > MAX_HARNESS_ITEMS {
            return Err(HarnessContractError::BoundsExceeded);
        }
        let mut seen = std::collections::BTreeSet::new();
        for choice in &self.choices {
            id(&choice.choice_id)?;
            text(&choice.label)?;
            if !seen.insert(&choice.choice_id) {
                return Err(HarnessContractError::InvalidShape);
            }
        }
        Ok(())
    }
}
/// Explicit user/host decision; generic content cannot masquerade as approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HarnessResponseValue {
    Approval {
        approved: bool,
    },
    Answer {
        choice_id: Option<String>,
        text: String,
    },
    Assistance {
        text: String,
    },
    Cancel,
}
/// Generation-fenced native interaction response with retained mutation identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessResponse {
    pub response_id: String,
    pub request_id: String,
    pub session_id: String,
    pub generation: u64,
    pub value: HarnessResponseValue,
}
impl HarnessResponse {
    /// Validate shape against the exact outstanding interaction.
    /// # Errors
    /// Rejects stale generations, mismatched requests, invalid choices and kind mismatch.
    pub fn validate_for(
        &self,
        interaction: &HarnessInteraction,
    ) -> Result<(), HarnessContractError> {
        interaction.validate()?;
        id(&self.response_id)?;
        if self.session_id != interaction.session_id || self.request_id != interaction.request_id {
            return Err(HarnessContractError::WrongOwner);
        }
        if self.generation != interaction.generation {
            return Err(HarnessContractError::StaleGeneration);
        }
        match (&self.value, interaction.kind) {
            (HarnessResponseValue::Cancel, _)
            | (HarnessResponseValue::Approval { .. }, HarnessInteractionKind::Approval) => Ok(()),
            (
                HarnessResponseValue::Assistance { text: value },
                HarnessInteractionKind::Assistance,
            ) => text(value),
            (
                HarnessResponseValue::Answer {
                    choice_id,
                    text: value,
                },
                HarnessInteractionKind::Question,
            ) => {
                text(value)?;
                if let Some(choice) = choice_id
                    && !interaction
                        .choices
                        .iter()
                        .any(|item| item.choice_id == *choice)
                {
                    return Err(HarnessContractError::InvalidShape);
                }
                Ok(())
            }
            _ => Err(HarnessContractError::InvalidShape),
        }
    }
}

/// Authoritative native turn outcome, distinct from background-work settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessTerminalDisposition {
    Completed,
    Interrupted,
    Failed,
}

/// Normalized observations only. Tool/subagent activity is never dispatched as Bcode work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HarnessObservationKind {
    /// Independent foreground, background, continuation, and tracking-coverage evidence.
    Execution {
        event: crate::HarnessExecutionEvent,
    },
    /// Native acceptance evidence for the exact retained submission and attempt.
    ///
    /// Session and generation are inherited from the enclosing observation. Native
    /// producers must establish this association from protocol evidence, never from
    /// binding confirmation, result text, idle or settlement alone. A native request
    /// ID is not a native turn ID. Host admission must verify the retained pair before
    /// canonical publication; this mapping proves neither settlement nor replay safety.
    SubmissionAccepted {
        submission_id: String,
        attempt_id: String,
        native_turn_id: String,
    },
    /// Main-thread native reply explicitly consumed the retained submission attempt.
    /// This proves neither native turn identity, settlement nor replay safety.
    SubmissionConsumed {
        submission_id: String,
        attempt_id: String,
        native_frame_id: String,
    },
    /// Native result explicitly attributes its response to the retained attempt.
    /// Queued user sends exclude native automatic/background work: zero is not settlement.
    SubmissionResultObserved {
        submission_id: String,
        attempt_id: String,
        native_frame_id: String,
        is_error: bool,
        queued_user_sends: Option<u32>,
    },
    /// Native-proven identity; requested IDs and initialize transport readiness are insufficient.
    BindingConfirmed {
        open_id: String,
        binding: HarnessSessionBinding,
    },
    Text {
        text: String,
    },
    Tool {
        native_tool_id: String,
        name: String,
        status: String,
    },
    Subagent {
        native_agent_id: String,
        status: String,
    },
    Interaction {
        interaction: HarnessInteraction,
    },
    Status {
        status: HarnessExecutionStatus,
    },
    Result {
        text: String,
    },
    Diagnostic {
        code: String,
        message: String,
    },
    /// Native-proven terminal disposition, not proof of background-work settlement.
    NativeTurnTerminal {
        native_turn_id: String,
        disposition: HarnessTerminalDisposition,
    },
    /// Only this explicit authoritative evidence can claim native settlement.
    Settlement {
        native_turn_id: String,
    },
    /// Plugin-approved sanitized detail, never raw secret-bearing protocol input.
    Vendor {
        namespace: String,
        detail: serde_json::Value,
    },
}
/// Stable bounded observation identity and native correlations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessObservation {
    pub observation_id: String,
    pub session_id: String,
    pub generation: u64,
    pub native_correlations: Vec<String>,
    pub completeness: HarnessCompleteness,
    pub kind: HarnessObservationKind,
}
impl HarnessObservation {
    /// Validate normalized semantic payload and bounded vendor detail.
    /// # Errors
    /// Returns an error for malformed, stale-shaped or excessive content.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        id(&self.observation_id)?;
        id(&self.session_id)?;
        if self.generation == 0 {
            return Err(HarnessContractError::InvalidShape);
        }
        if self.native_correlations.len() > MAX_HARNESS_ITEMS {
            return Err(HarnessContractError::BoundsExceeded);
        }
        for correlation in &self.native_correlations {
            id(correlation)?;
        }
        match &self.kind {
            HarnessObservationKind::Execution { event } => event.validate(),
            HarnessObservationKind::SubmissionAccepted {
                submission_id,
                attempt_id,
                native_turn_id,
            } => {
                id(submission_id)?;
                id(attempt_id)?;
                id(native_turn_id)
            }
            HarnessObservationKind::SubmissionConsumed {
                submission_id,
                attempt_id,
                native_frame_id,
            }
            | HarnessObservationKind::SubmissionResultObserved {
                submission_id,
                attempt_id,
                native_frame_id,
                ..
            } => {
                id(submission_id)?;
                id(attempt_id)?;
                id(native_frame_id)
            }
            HarnessObservationKind::BindingConfirmed { open_id, binding } => {
                id(open_id)?;
                binding.validate_owner(&self.session_id, self.generation)
            }
            HarnessObservationKind::Text { text: value }
            | HarnessObservationKind::Result { text: value } => text(value),
            HarnessObservationKind::Tool {
                native_tool_id,
                name,
                status,
            } => {
                id(native_tool_id)?;
                text(name)?;
                text(status)
            }
            HarnessObservationKind::Subagent {
                native_agent_id,
                status,
            } => {
                id(native_agent_id)?;
                text(status)
            }
            HarnessObservationKind::Interaction { interaction } => {
                interaction.validate()?;
                if interaction.session_id != self.session_id
                    || interaction.generation != self.generation
                {
                    return Err(HarnessContractError::WrongOwner);
                }
                Ok(())
            }
            HarnessObservationKind::Status { .. } => Ok(()),
            HarnessObservationKind::Diagnostic { code, message } => {
                id(code)?;
                text(message)
            }
            HarnessObservationKind::Settlement { native_turn_id }
            | HarnessObservationKind::NativeTurnTerminal { native_turn_id, .. } => {
                id(native_turn_id)
            }
            HarnessObservationKind::Vendor { namespace, detail } => {
                id(namespace)?;
                wire(detail, MAX_HARNESS_VENDOR_BYTES)
            }
        }
    }
}
