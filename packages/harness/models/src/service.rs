//! Versioned bounded service envelopes. Auth payloads are ephemeral and trace-exempt.
use crate::{
    HARNESS_SCHEMA_VERSION, HarnessCapabilities, HarnessCompleteness, HarnessContractError,
    HarnessExecutionStatus, HarnessInstance, HarnessObservation, HarnessObservationKind,
    HarnessPlacement, HarnessPreparedSession, HarnessReceipt, HarnessResponse,
    HarnessResponseValue, HarnessSessionBinding, HarnessSessionTarget, HarnessSubmission,
    MAX_HARNESS_OBSERVATIONS, MAX_HARNESS_WIRE_BYTES, id, text, wire,
};
use bcode_provider_auth_models::managed::{
    AuthSensitiveInput, ManagedAuthStatus, SensitiveAuthInteraction,
};
use serde::{Deserialize, Serialize};

/// Exactly one semantic operation per invocation; unknown operations fail closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessOperation {
    Inspect,
    Open,
    Submit,
    Observe,
    Respond,
    Interrupt,
    Close,
    Reconcile,
    AuthStatus,
    AuthBegin,
    AuthContinue,
    AuthCancel,
}
impl HarnessOperation {
    /// Stable service ABI operation name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inspect => "inspect",
            Self::Open => "open",
            Self::Submit => "submit",
            Self::Observe => "observe",
            Self::Respond => "respond",
            Self::Interrupt => "interrupt",
            Self::Close => "close",
            Self::Reconcile => "reconcile",
            Self::AuthStatus => "auth_status",
            Self::AuthBegin => "auth_begin",
            Self::AuthContinue => "auth_continue",
            Self::AuthCancel => "auth_cancel",
        }
    }
    /// Auth calls must bypass ordinary request/event tracing.
    #[must_use]
    pub const fn is_sensitive(self) -> bool {
        matches!(
            self,
            Self::AuthBegin | Self::AuthContinue | Self::AuthCancel
        )
    }
}

/// Host-supplied correlation for explicitly admitted parent assistance, not authority.
///
/// Clients cannot self-grant assistance. The host must reauthorize this grant before
/// activation and reauthorize each response and revocation against current ownership
/// and policy. Native observations, configuration and parentage alone never enable
/// assistance. Validation below checks shape/correlation only, not admission validity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessAssistanceGrant {
    /// Retained host admission correlation, not a bearer capability.
    pub admission_id: String,
    pub child_session_id: String,
    pub child_generation: u64,
    pub parent_session_id: String,
    pub parent_generation: u64,
    /// Explicit nonempty semantic scope, bounded by `MAX_HARNESS_TEXT_BYTES`.
    pub scope: String,
}
impl HarnessAssistanceGrant {
    /// Check bounded identities, distinct sessions and nonzero generations.
    /// Does not authorize assistance or prove current ownership/policy.
    /// # Errors
    /// Rejects malformed identities, empty/excessive scope and invalid relationships.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        id(&self.admission_id)?;
        id(&self.child_session_id)?;
        id(&self.parent_session_id)?;
        text(&self.scope)?;
        if self.child_generation == 0
            || self.parent_generation == 0
            || self.child_session_id == self.parent_session_id
            || self.scope.trim().is_empty()
        {
            return Err(HarnessContractError::InvalidShape);
        }
        Ok(())
    }

    /// Check correlation with the exact child open request; never grants authority.
    /// # Errors
    /// Rejects malformed grants, mismatched child identity or stale generation.
    pub fn validate_for(&self, open: &HarnessOpenRequest) -> Result<(), HarnessContractError> {
        self.validate()?;
        if self.child_session_id != open.session_id {
            return Err(HarnessContractError::WrongOwner);
        }
        if self.child_generation != open.generation {
            return Err(HarnessContractError::StaleGeneration);
        }
        Ok(())
    }
}

/// Host-retained open identity. A lost native creation acknowledgement requires reconcile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessOpenRequest {
    pub open_id: String,
    pub session_id: String,
    pub generation: u64,
    pub working_directory: String,
    /// Resume only the exact host-verified binding; never search/adopt unrelated sessions.
    pub resume: Option<HarnessSessionBinding>,
    /// Optional host-admitted correlation only. Absence disables parent assistance;
    /// clients cannot self-grant by supplying this field. Host reauthorization is
    /// required for activation, responses and revocation under current ownership.
    #[serde(default)]
    pub parent_assistance: Option<HarnessAssistanceGrant>,
}
/// Bounded observation page request. A cursor is opaque, not evidence of retention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessObserveRequest {
    pub target: HarnessSessionTarget,
    pub after: Option<String>,
    pub limit: u16,
    /// Bounded long poll; readers must drain native pipes independently.
    pub wait_millis: u32,
}
/// Retained control mutation identity. Interrupt acknowledgement is not settlement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessControlRequest {
    pub target: HarnessSessionTarget,
    pub mutation_id: String,
}
/// Read-only reconciliation of retained identities, never implicit mutation replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessReconcileRequest {
    pub session_id: String,
    pub generation: u64,
    pub open_id: Option<String>,
    pub submission_id: Option<String>,
    pub attempt_id: Option<String>,
    pub binding: Option<HarnessSessionBinding>,
}
/// One transient auth-flow step; flow/input must not be persisted or replayed after restart.
#[derive(Debug, Serialize, Deserialize)]
pub struct HarnessAuthRequest {
    pub flow_id: String,
    pub method_id: String,
    pub input: Option<AuthSensitiveInput>,
}

/// Semantic payload, not a generic JSON method bag.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "operation", content = "request", rename_all = "snake_case")]
pub enum HarnessRequestKind {
    Inspect,
    Open(HarnessOpenRequest),
    Submit {
        target: HarnessSessionTarget,
        submission: HarnessSubmission,
    },
    Observe(HarnessObserveRequest),
    Respond {
        target: HarnessSessionTarget,
        response: HarnessResponse,
    },
    Interrupt(HarnessControlRequest),
    Close(HarnessControlRequest),
    Reconcile(HarnessReconcileRequest),
    AuthStatus {
        verify: bool,
    },
    AuthBegin(HarnessAuthRequest),
    AuthContinue(HarnessAuthRequest),
    AuthCancel(HarnessAuthRequest),
}
impl HarnessRequestKind {
    /// Exact operation corresponding to the typed request variant.
    #[must_use]
    pub const fn operation(&self) -> HarnessOperation {
        match self {
            Self::Inspect => HarnessOperation::Inspect,
            Self::Open(_) => HarnessOperation::Open,
            Self::Submit { .. } => HarnessOperation::Submit,
            Self::Observe(_) => HarnessOperation::Observe,
            Self::Respond { .. } => HarnessOperation::Respond,
            Self::Interrupt(_) => HarnessOperation::Interrupt,
            Self::Close(_) => HarnessOperation::Close,
            Self::Reconcile(_) => HarnessOperation::Reconcile,
            Self::AuthStatus { .. } => HarnessOperation::AuthStatus,
            Self::AuthBegin(_) => HarnessOperation::AuthBegin,
            Self::AuthContinue(_) => HarnessOperation::AuthContinue,
            Self::AuthCancel(_) => HarnessOperation::AuthCancel,
        }
    }
}
/// Version and correlation are checked before any native mutation.
#[derive(Debug, Serialize, Deserialize)]
pub struct HarnessRequest {
    pub schema_version: u16,
    pub request_id: String,
    pub instance: HarnessInstance,
    pub kind: HarnessRequestKind,
}
impl HarnessRequest {
    /// Validate size, compatibility, identity and internal ownership consistency.
    /// # Errors
    /// Rejects unknown versions, invalid shape/identity and excessive payloads.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        if self.schema_version != HARNESS_SCHEMA_VERSION {
            return Err(HarnessContractError::UnsupportedVersion);
        }
        id(&self.request_id)?;
        self.instance.validate()?;
        match &self.kind {
            HarnessRequestKind::Open(value) => validate_open_request(value, &self.instance)?,
            HarnessRequestKind::Submit { target, submission } => {
                submission.validate()?;
                target.validate_owner(&submission.session_id, submission.generation)?;
                validate_target_instance(target, &self.instance)?;
                if matches!(target, HarnessSessionTarget::Prepared(_))
                    && !matches!(submission.placement, HarnessPlacement::NextTurn)
                {
                    return Err(HarnessContractError::InvalidShape);
                }
            }
            HarnessRequestKind::Observe(value) => {
                if value.limit == 0
                    || usize::from(value.limit) > MAX_HARNESS_OBSERVATIONS
                    || value.wait_millis > 30_000
                {
                    return Err(HarnessContractError::BoundsExceeded);
                }
                if let Some(cursor) = &value.after {
                    id(cursor)?;
                }
                validate_target_instance(&value.target, &self.instance)?;
            }
            HarnessRequestKind::Respond { target, response } => {
                id(&response.response_id)?;
                id(&response.request_id)?;
                target.validate_owner(&response.session_id, response.generation)?;
                validate_target_instance(target, &self.instance)?;
                match &response.value {
                    HarnessResponseValue::Answer {
                        choice_id,
                        text: value,
                    } => {
                        if let Some(choice) = choice_id {
                            id(choice)?;
                        }
                        text(value)?;
                    }
                    HarnessResponseValue::Assistance { text: value } => text(value)?,
                    HarnessResponseValue::Approval { .. } | HarnessResponseValue::Cancel => {}
                }
                // Plugin must also validate_for the exact outstanding interaction.
            }
            HarnessRequestKind::Interrupt(value) | HarnessRequestKind::Close(value) => {
                id(&value.mutation_id)?;
                validate_target_instance(&value.target, &self.instance)?;
            }
            HarnessRequestKind::Reconcile(value) => {
                id(&value.session_id)?;
                if value.generation == 0
                    || value.submission_id.is_some() != value.attempt_id.is_some()
                {
                    return Err(HarnessContractError::InvalidShape);
                }
                for identity in [&value.open_id, &value.submission_id, &value.attempt_id]
                    .into_iter()
                    .flatten()
                {
                    id(identity)?;
                }
                if let Some(binding) = &value.binding {
                    binding.validate_owner(&value.session_id, value.generation)?;
                    if binding.instance_id != self.instance.instance_id {
                        return Err(HarnessContractError::WrongOwner);
                    }
                }
            }
            HarnessRequestKind::AuthBegin(value)
            | HarnessRequestKind::AuthContinue(value)
            | HarnessRequestKind::AuthCancel(value) => {
                id(&value.flow_id)?;
                id(&value.method_id)?;
                if let Some(input) = &value.input
                    && (!matches!(self.kind, HarnessRequestKind::AuthContinue(_))
                        || !input.is_valid())
                {
                    return Err(HarnessContractError::InvalidShape);
                }
            }
            HarnessRequestKind::Inspect | HarnessRequestKind::AuthStatus { .. } => {}
        }
        wire(self, MAX_HARNESS_WIRE_BYTES)
    }
}

/// Native executable and config provenance returned by a non-mutating inspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessInspection {
    pub native_version: String,
    pub executable_identity: String,
    pub config_namespace: String,
    pub capabilities: HarnessCapabilities,
}
/// Bounded page with explicit retention/completeness. Empty does not mean settled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessObservationPage {
    pub observations: Vec<HarnessObservation>,
    pub next_after: Option<String>,
    pub completeness: HarnessCompleteness,
    pub status: HarnessExecutionStatus,
}
/// No-effect proof is explicit and separate from unknown delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum HarnessReconciliation {
    /// Existing live transport found, without native-confirmed identity. Never implies safe replay.
    KnownPrepared {
        prepared: HarnessPreparedSession,
        receipt: Option<HarnessReceipt>,
        status: HarnessExecutionStatus,
    },
    Known {
        binding: HarnessSessionBinding,
        receipt: Option<HarnessReceipt>,
        status: HarnessExecutionStatus,
    },
    /// Native inspection could not establish outcome. Must not automatically resend.
    Uncertain,
    /// Positive native evidence that this exact retained mutation was not applied.
    NotApplied,
}
/// Sanitized failure category. Raw native errors are not public diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessFailure {
    Unsupported,
    InvalidRequest,
    Unavailable,
    StaleGeneration,
    Conflict,
    Authentication,
    Policy,
    Uncertain,
}
/// Transient login output reuses provider-auth sensitive effects and sanitized status.
#[derive(Debug, Serialize, Deserialize)]
pub struct HarnessAuthStep {
    pub flow_id: String,
    pub status: ManagedAuthStatus,
    pub interaction: Option<SensitiveAuthInteraction>,
    pub complete: bool,
}
/// Responses retain their semantic family; a receipt never stands in for settlement.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "response", rename_all = "snake_case")]
pub enum HarnessServiceResponseKind {
    Inspection(HarnessInspection),
    /// Native identity has not yet been confirmed; never creation-success evidence.
    Prepared(HarnessPreparedSession),
    /// Only native-confirmed identity may be reported here.
    Opened(HarnessSessionBinding),
    Receipt(HarnessReceipt),
    Observations(HarnessObservationPage),
    /// A control message was acknowledged; no assertion about native termination.
    Acknowledged {
        mutation_id: String,
    },
    Reconciled(HarnessReconciliation),
    AuthStatus(ManagedAuthStatus),
    AuthStep(HarnessAuthStep),
    Failure(HarnessFailure),
}
/// Versioned reply. `request_id` and `operation` must match the pending invocation.
#[derive(Debug, Serialize, Deserialize)]
pub struct HarnessServiceResponse {
    pub schema_version: u16,
    pub request_id: String,
    pub operation: HarnessOperation,
    pub kind: HarnessServiceResponseKind,
}
impl HarnessServiceResponse {
    /// Validate bounded data and compatibility independent of a pending request.
    /// # Errors
    /// Rejects unknown versions, invalid identities and excessive pages/payloads.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        if self.schema_version != HARNESS_SCHEMA_VERSION {
            return Err(HarnessContractError::UnsupportedVersion);
        }
        id(&self.request_id)?;
        match &self.kind {
            HarnessServiceResponseKind::Inspection(value) => {
                id(&value.native_version)?;
                text(&value.executable_identity)?;
                text(&value.config_namespace)?;
                value.capabilities.validate()?;
            }
            HarnessServiceResponseKind::Prepared(value) => value.validate()?,
            HarnessServiceResponseKind::Opened(value) => value.validate()?,
            HarnessServiceResponseKind::Receipt(value) => validate_receipt(value)?,
            HarnessServiceResponseKind::Observations(value) => {
                if value.observations.len() > MAX_HARNESS_OBSERVATIONS {
                    return Err(HarnessContractError::BoundsExceeded);
                }
                let mut seen = std::collections::BTreeSet::new();
                for observation in &value.observations {
                    observation.validate()?;
                    if !seen.insert(&observation.observation_id) {
                        return Err(HarnessContractError::InvalidShape);
                    }
                }
                if let Some(cursor) = &value.next_after {
                    id(cursor)?;
                }
            }
            HarnessServiceResponseKind::Acknowledged { mutation_id } => id(mutation_id)?,
            HarnessServiceResponseKind::Reconciled(HarnessReconciliation::Known {
                binding,
                receipt,
                ..
            }) => {
                binding.validate()?;
                if let Some(receipt) = receipt {
                    validate_receipt(receipt)?;
                    binding.validate_owner(&receipt.session_id, receipt.generation)?;
                }
            }
            HarnessServiceResponseKind::Reconciled(HarnessReconciliation::KnownPrepared {
                prepared,
                receipt,
                ..
            }) => {
                prepared.validate()?;
                if let Some(receipt) = receipt {
                    validate_receipt(receipt)?;
                    HarnessSessionTarget::Prepared(prepared.clone())
                        .validate_owner(&receipt.session_id, receipt.generation)?;
                }
            }
            HarnessServiceResponseKind::AuthStatus(value) => validate_auth_status(value)?,
            HarnessServiceResponseKind::AuthStep(value) => {
                id(&value.flow_id)?;
                validate_auth_status(&value.status)?;
                if let Some(interaction) = &value.interaction
                    && (value.complete || !interaction.is_valid())
                {
                    return Err(HarnessContractError::InvalidShape);
                }
            }
            HarnessServiceResponseKind::Reconciled(_) | HarnessServiceResponseKind::Failure(_) => {}
        }
        wire(self, MAX_HARNESS_WIRE_BYTES)
    }
    /// Validate reply against exact request, including page size, generation and mutation IDs.
    /// # Errors
    /// Rejects mismatched operation/correlation, owner, generation or response family.
    pub fn validate_for(&self, request: &HarnessRequest) -> Result<(), HarnessContractError> {
        request.validate()?;
        self.validate()?;
        if self.request_id != request.request_id || self.operation != request.kind.operation() {
            return Err(HarnessContractError::OperationMismatch);
        }
        if matches!(self.kind, HarnessServiceResponseKind::Failure(_)) {
            return Ok(());
        }
        match (&request.kind, &self.kind) {
            (HarnessRequestKind::Inspect, HarnessServiceResponseKind::Inspection(value)) => {
                if value.config_namespace != request.instance.config_namespace {
                    return Err(HarnessContractError::WrongOwner);
                }
            }
            (HarnessRequestKind::Open(open), HarnessServiceResponseKind::Prepared(prepared)) => {
                if open.resume.is_some() {
                    return Err(HarnessContractError::InvalidShape);
                }
                HarnessSessionTarget::Prepared(prepared.clone())
                    .validate_owner(&open.session_id, open.generation)?;
                if prepared.open_id != open.open_id
                    || prepared.instance_id != request.instance.instance_id
                {
                    return Err(HarnessContractError::WrongOwner);
                }
            }
            (HarnessRequestKind::Open(open), HarnessServiceResponseKind::Opened(binding)) => {
                binding.validate_owner(&open.session_id, open.generation)?;
                if binding.instance_id != request.instance.instance_id
                    || open
                        .resume
                        .as_ref()
                        .is_some_and(|expected| expected != binding)
                {
                    return Err(HarnessContractError::WrongOwner);
                }
            }
            (
                HarnessRequestKind::Submit { submission, .. },
                HarnessServiceResponseKind::Receipt(receipt),
            ) => {
                if receipt.session_id != submission.session_id
                    || receipt.submission_id != submission.submission_id
                    || receipt.attempt_id != submission.attempt_id
                {
                    return Err(HarnessContractError::WrongOwner);
                }
                if receipt.generation != submission.generation {
                    return Err(HarnessContractError::StaleGeneration);
                }
            }
            (
                HarnessRequestKind::Observe(observe),
                HarnessServiceResponseKind::Observations(page),
            ) => validate_observation_page_for(page, observe)?,
            (
                HarnessRequestKind::Respond { response, .. },
                HarnessServiceResponseKind::Acknowledged { mutation_id },
            ) => {
                if *mutation_id != response.response_id {
                    return Err(HarnessContractError::WrongOwner);
                }
            }
            (
                HarnessRequestKind::Interrupt(value) | HarnessRequestKind::Close(value),
                HarnessServiceResponseKind::Acknowledged { mutation_id },
            ) => {
                if *mutation_id != value.mutation_id {
                    return Err(HarnessContractError::WrongOwner);
                }
            }
            (
                HarnessRequestKind::Reconcile(value),
                HarnessServiceResponseKind::Reconciled(result),
            ) => validate_reconciliation_for(result, value, &request.instance)?,
            (
                HarnessRequestKind::AuthStatus { .. },
                HarnessServiceResponseKind::AuthStatus(status),
            ) => validate_auth_owner(status, request)?,
            (
                HarnessRequestKind::AuthBegin(value)
                | HarnessRequestKind::AuthContinue(value)
                | HarnessRequestKind::AuthCancel(value),
                HarnessServiceResponseKind::AuthStep(step),
            ) => {
                if step.flow_id != value.flow_id {
                    return Err(HarnessContractError::WrongOwner);
                }
                validate_auth_owner(&step.status, request)?;
            }
            _ => return Err(HarnessContractError::OperationMismatch),
        }
        Ok(())
    }
}
fn validate_open_request(
    value: &HarnessOpenRequest,
    instance: &HarnessInstance,
) -> Result<(), HarnessContractError> {
    id(&value.open_id)?;
    id(&value.session_id)?;
    text(&value.working_directory)?;
    if value.generation == 0 || value.working_directory.trim().is_empty() {
        return Err(HarnessContractError::InvalidShape);
    }
    if let Some(binding) = &value.resume {
        binding.validate_owner(&value.session_id, value.generation)?;
        if binding.instance_id != instance.instance_id {
            return Err(HarnessContractError::WrongOwner);
        }
    }
    if let Some(grant) = &value.parent_assistance {
        grant.validate_for(value)?;
    }
    Ok(())
}

fn validate_observation_page_for(
    page: &HarnessObservationPage,
    observe: &HarnessObserveRequest,
) -> Result<(), HarnessContractError> {
    if page.observations.len() > usize::from(observe.limit) {
        return Err(HarnessContractError::BoundsExceeded);
    }
    if page.next_after.is_some() && page.next_after == observe.after {
        return Err(HarnessContractError::InvalidShape);
    }
    let mut confirmation: Option<&HarnessSessionBinding> = None;
    for value in &page.observations {
        observe
            .target
            .validate_owner(&value.session_id, value.generation)?;
        if let HarnessObservationKind::BindingConfirmed { open_id, binding } = &value.kind {
            observe.target.validate_confirmation(open_id, binding)?;
            if confirmation.is_some_and(|previous| previous != binding) {
                return Err(HarnessContractError::WrongOwner);
            }
            confirmation = Some(binding);
        }
    }
    Ok(())
}

fn validate_reconciliation_for(
    result: &HarnessReconciliation,
    value: &HarnessReconcileRequest,
    instance: &HarnessInstance,
) -> Result<(), HarnessContractError> {
    let receipt = match result {
        HarnessReconciliation::Known {
            binding, receipt, ..
        } => {
            binding.validate_owner(&value.session_id, value.generation)?;
            if binding.instance_id != instance.instance_id
                || value
                    .binding
                    .as_ref()
                    .is_some_and(|expected| expected != binding)
            {
                return Err(HarnessContractError::WrongOwner);
            }
            receipt.as_ref()
        }
        HarnessReconciliation::KnownPrepared {
            prepared, receipt, ..
        } => {
            if value.binding.is_some() {
                return Err(HarnessContractError::InvalidShape);
            }
            let target = HarnessSessionTarget::Prepared(prepared.clone());
            target.validate_owner(&value.session_id, value.generation)?;
            validate_target_instance(&target, instance)?;
            if value.open_id.as_ref() != Some(&prepared.open_id) {
                return Err(HarnessContractError::WrongOwner);
            }
            receipt.as_ref()
        }
        HarnessReconciliation::Uncertain | HarnessReconciliation::NotApplied => None,
    };
    if let Some(receipt) = receipt
        && (value.submission_id.as_ref() != Some(&receipt.submission_id)
            || value.attempt_id.as_ref() != Some(&receipt.attempt_id))
    {
        return Err(HarnessContractError::WrongOwner);
    }
    Ok(())
}

fn validate_target_instance(
    target: &HarnessSessionTarget,
    instance: &HarnessInstance,
) -> Result<(), HarnessContractError> {
    target.validate()?;
    if target.instance_id() != instance.instance_id {
        return Err(HarnessContractError::WrongOwner);
    }
    Ok(())
}
fn validate_receipt(value: &HarnessReceipt) -> Result<(), HarnessContractError> {
    id(&value.submission_id)?;
    id(&value.attempt_id)?;
    id(&value.session_id)?;
    if value.generation == 0 {
        return Err(HarnessContractError::InvalidShape);
    }
    if let Some(native) = &value.native_request_id {
        id(native)?;
    }
    Ok(())
}
fn validate_auth_status(value: &ManagedAuthStatus) -> Result<(), HarnessContractError> {
    if !value.namespace.is_valid() {
        return Err(HarnessContractError::InvalidIdentity);
    }
    Ok(())
}
fn validate_auth_owner(
    value: &ManagedAuthStatus,
    request: &HarnessRequest,
) -> Result<(), HarnessContractError> {
    if value.namespace.plugin_id != request.instance.plugin_id
        || value.namespace.credential_namespace != request.instance.auth.credential_namespace
        || value.custody != request.instance.auth.custody
    {
        return Err(HarnessContractError::WrongOwner);
    }
    Ok(())
}
