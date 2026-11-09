//! Actor-owned managed harness operation records. Auth transport is deliberately unrepresentable.
use crate::SessionId;
use bcode_harness_models::{
    HARNESS_SCHEMA_VERSION, HarnessCapabilities, HarnessConfigureRequest, HarnessControlRequest,
    HarnessFailure, HarnessInstance, HarnessObservationKind, HarnessObservationPage,
    HarnessOpenRequest, HarnessOperation, HarnessPreparedSession, HarnessReceipt,
    HarnessReconcileRequest, HarnessReconciliation, HarnessRequest, HarnessRequestKind,
    HarnessResponse, HarnessServiceResponse, HarnessServiceResponseKind, HarnessSessionBinding,
    HarnessSessionTarget, HarnessSubmission, MAX_HARNESS_ID_BYTES, MAX_HARNESS_OBSERVATIONS,
    MAX_HARNESS_WIRE_BYTES,
};
use serde::{Deserialize, Serialize};

/// Authenticated local daemon identity; caller must derive this from its serving runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalHarnessOwner {
    pub session_id: SessionId,
    pub generation: u64,
    pub artifact_id: String,
    pub daemon_id: String,
}
impl ExternalHarnessOwner {
    /// Validate bounded shape only, never daemon authority.
    /// # Errors
    /// Rejects zero generations and malformed identities.
    pub fn validate(&self) -> Result<(), String> {
        if self.generation == 0
            || [&self.artifact_id, &self.daemon_id]
                .iter()
                .any(|v| v.is_empty() || v.len() > 256 || v.chars().any(char::is_control))
        {
            return Err("invalid managed harness owner".into());
        }
        Ok(())
    }
}

/// Recovery storage (transport release and recovery intent records) is published.
///
/// Recovery records require the native-parity fence
/// ([`NATIVE_PARITY_EVENT_SCHEMA_VERSION`] / [`NATIVE_PARITY_STORAGE_WRITER_EPOCH`]), so older
/// writers can never observe or extend a released or recovered generation.
#[must_use]
pub const fn recovery_storage_available() -> bool {
    true
}

/// First event schema that may carry selected-open operations.
pub const SELECTED_OPEN_EVENT_SCHEMA_VERSION: u16 = 57;
/// First writer epoch that may write selected-open operations (old-writer fence).
pub const SELECTED_OPEN_STORAGE_WRITER_EPOCH: u32 = 25;
/// First event schema that may carry native-parity records.
///
/// These are reasoning, native tool-result and usage observations, text/tool/interaction display
/// metadata, configuration requests, and (once published) recovery records.
pub const NATIVE_PARITY_EVENT_SCHEMA_VERSION: u16 = 58;
/// First writer epoch that may write native-parity records (old-writer fence).
pub const NATIVE_PARITY_STORAGE_WRITER_EPOCH: u32 = 26;

/// Whether one observation uses a representation introduced by the native-parity schema.
#[must_use]
pub const fn observation_requires_native_parity(kind: &HarnessObservationKind) -> bool {
    match kind {
        HarnessObservationKind::Reasoning { .. }
        | HarnessObservationKind::ToolResult { .. }
        | HarnessObservationKind::Usage { .. }
        | HarnessObservationKind::InteractionWithdrawn { .. } => true,
        HarnessObservationKind::Text { segment_id, .. } => segment_id.is_some(),
        HarnessObservationKind::Tool { input_json, .. } => input_json.is_some(),
        HarnessObservationKind::Interaction { interaction } => {
            interaction.tool_name.is_some() || interaction.input_preview.is_some()
        }
        _ => false,
    }
}

/// Exact extension envelope retained in configuration, claim and completion.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalHarnessSelectedOpen {
    pub version: u16,
    pub request: bcode_harness_models::HarnessModelsRequest,
}

fn clone_models_request(
    request: &bcode_harness_models::HarnessModelsRequest,
) -> bcode_harness_models::HarnessModelsRequest {
    let bcode_harness_models::HarnessModelsRequest {
        schema_version,
        request_id,
        instance,
        kind,
    } = request;
    bcode_harness_models::HarnessModelsRequest {
        schema_version: *schema_version,
        request_id: request_id.clone(),
        instance: instance.clone(),
        kind: kind.clone(),
    }
}

impl Clone for ExternalHarnessSelectedOpen {
    fn clone(&self) -> Self {
        Self {
            version: self.version,
            request: clone_models_request(&self.request),
        }
    }
}

impl PartialEq for ExternalHarnessSelectedOpen {
    fn eq(&self, other: &Self) -> bool {
        let bcode_harness_models::HarnessModelsRequest {
            schema_version,
            request_id,
            instance,
            kind,
        } = &self.request;
        let bcode_harness_models::HarnessModelsRequest {
            schema_version: other_schema,
            request_id: other_id,
            instance: other_instance,
            kind: other_kind,
        } = &other.request;
        self.version == other.version
            && schema_version == other_schema
            && request_id == other_id
            && instance == other_instance
            && kind == other_kind
    }
}
impl Eq for ExternalHarnessSelectedOpen {}

/// Secret-free native request retained before attempted delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", content = "request", rename_all = "snake_case")]
pub enum ExternalHarnessRequestKind {
    Open(HarnessOpenRequest),
    /// Independently fenced selected-open durable family v1; never a legacy Open.
    OpenSelected(ExternalHarnessSelectedOpen),
    Submit {
        target: HarnessSessionTarget,
        submission: HarnessSubmission,
    },
    Respond {
        target: HarnessSessionTarget,
        response: HarnessResponse,
    },
    Interrupt(HarnessControlRequest),
    /// Native-parity mid-session setting change (schema 58 / writer 26).
    Configure(HarnessConfigureRequest),
    Close(HarnessControlRequest),
    Reconcile(HarnessReconcileRequest),
}
/// Exact typed mutation identity/content, excluding all authentication operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalHarnessRequest {
    pub schema_version: u16,
    pub request_id: String,
    pub instance: HarnessInstance,
    pub kind: ExternalHarnessRequestKind,
}
impl ExternalHarnessRequest {
    /// Retain a complete selected-open extension without reinterpreting it as legacy Open.
    /// # Errors
    /// Rejects discovery, malformed envelopes and mismatched Open identities.
    pub fn from_models_request(
        value: &bcode_harness_models::HarnessModelsRequest,
    ) -> Result<Self, String> {
        value
            .validate()
            .map_err(|_| "invalid selected-open request")?;
        let bcode_harness_models::HarnessModelsRequestKind::OpenSelected { open, .. } = &value.kind
        else {
            return Err("only selected open can enter canonical storage".into());
        };
        if value.request_id != open.open_id || open.resume.is_some() {
            return Err("selected-open identity mismatch or unsupported resume".into());
        }
        Ok(Self {
            schema_version: value.schema_version,
            request_id: value.request_id.clone(),
            instance: value.instance.clone(),
            kind: ExternalHarnessRequestKind::OpenSelected(ExternalHarnessSelectedOpen {
                version: 1,
                request: clone_models_request(value),
            }),
        })
    }

    /// Validate the retained family and redundant correlation fields exactly.
    /// # Errors
    /// Rejects malformed requests, unsupported versions and envelope divergence.
    pub fn validate(&self) -> Result<(), String> {
        if let ExternalHarnessRequestKind::OpenSelected(selected) = &self.kind {
            if selected.version != 1 || Self::from_models_request(&selected.request)? != *self {
                return Err("invalid selected-open durable envelope".into());
            }
            Ok(())
        } else {
            self.request()?
                .validate()
                .map_err(|_| "invalid retained harness request".into())
        }
    }

    /// Open payload for either family. This is not delivery authority.
    #[must_use]
    pub fn open(&self) -> Option<&HarnessOpenRequest> {
        match &self.kind {
            ExternalHarnessRequestKind::Open(open) => Some(open),
            ExternalHarnessRequestKind::OpenSelected(selected) => match &selected.request.kind {
                bcode_harness_models::HarnessModelsRequestKind::OpenSelected { open, .. } => {
                    Some(open)
                }
                bcode_harness_models::HarnessModelsRequestKind::DiscoverModels => None,
            },
            _ => None,
        }
    }

    /// Whether this request uses the independently fenced selected-open family.
    #[must_use]
    pub const fn is_selected_open(&self) -> bool {
        matches!(self.kind, ExternalHarnessRequestKind::OpenSelected(_))
    }

    /// Whether this request uses a family introduced by the native-parity schema.
    #[must_use]
    pub const fn requires_native_parity(&self) -> bool {
        matches!(self.kind, ExternalHarnessRequestKind::Configure(_))
    }

    /// Retain only supported non-auth operation families.
    /// # Errors
    /// Rejects malformed, read-only and authentication requests before persistence.
    pub fn from_request(value: &HarnessRequest) -> Result<Self, String> {
        value
            .validate()
            .map_err(|_| "invalid harness request".to_owned())?;
        let kind = match &value.kind {
            HarnessRequestKind::Open(v) => ExternalHarnessRequestKind::Open(v.clone()),
            HarnessRequestKind::Submit { target, submission } => {
                ExternalHarnessRequestKind::Submit {
                    target: target.clone(),
                    submission: submission.clone(),
                }
            }
            HarnessRequestKind::Respond { target, response } => {
                ExternalHarnessRequestKind::Respond {
                    target: target.clone(),
                    response: response.clone(),
                }
            }
            HarnessRequestKind::Interrupt(v) => ExternalHarnessRequestKind::Interrupt(v.clone()),
            HarnessRequestKind::Configure(v) => ExternalHarnessRequestKind::Configure(v.clone()),
            HarnessRequestKind::Close(v) => ExternalHarnessRequestKind::Close(v.clone()),
            HarnessRequestKind::Reconcile(v) => ExternalHarnessRequestKind::Reconcile(v.clone()),
            _ => return Err("operation cannot enter canonical mutation store".into()),
        };
        Ok(Self {
            schema_version: value.schema_version,
            request_id: value.request_id.clone(),
            instance: value.instance.clone(),
            kind,
        })
    }
    /// Reconstruct bounded non-secret plugin request without consulting current config.
    /// # Errors
    /// Rejects selected requests; use their retained extension envelope instead.
    pub fn request(&self) -> Result<HarnessRequest, String> {
        let kind = match &self.kind {
            ExternalHarnessRequestKind::Open(v) => HarnessRequestKind::Open(v.clone()),
            ExternalHarnessRequestKind::OpenSelected(_) => {
                return Err("selected open cannot be downgraded to legacy Open".into());
            }
            ExternalHarnessRequestKind::Submit { target, submission } => {
                HarnessRequestKind::Submit {
                    target: target.clone(),
                    submission: submission.clone(),
                }
            }
            ExternalHarnessRequestKind::Respond { target, response } => {
                HarnessRequestKind::Respond {
                    target: target.clone(),
                    response: response.clone(),
                }
            }
            ExternalHarnessRequestKind::Interrupt(v) => HarnessRequestKind::Interrupt(v.clone()),
            ExternalHarnessRequestKind::Configure(v) => HarnessRequestKind::Configure(v.clone()),
            ExternalHarnessRequestKind::Close(v) => HarnessRequestKind::Close(v.clone()),
            ExternalHarnessRequestKind::Reconcile(v) => HarnessRequestKind::Reconcile(v.clone()),
        };
        Ok(HarnessRequest {
            schema_version: self.schema_version,
            request_id: self.request_id.clone(),
            instance: self.instance.clone(),
            kind,
        })
    }
    /// Session and ownership generation claimed by the exact request.
    #[must_use]
    pub fn target(&self) -> (&str, u64) {
        match &self.kind {
            ExternalHarnessRequestKind::Open(v) => (&v.session_id, v.generation),
            ExternalHarnessRequestKind::OpenSelected(_) => self
                .open()
                .map_or(("", 0), |open| (open.session_id.as_str(), open.generation)),
            ExternalHarnessRequestKind::Submit { submission, .. } => {
                (&submission.session_id, submission.generation)
            }
            ExternalHarnessRequestKind::Respond { response, .. } => {
                (&response.session_id, response.generation)
            }
            ExternalHarnessRequestKind::Interrupt(v) | ExternalHarnessRequestKind::Close(v) => {
                (v.target.session_id(), v.target.generation())
            }
            ExternalHarnessRequestKind::Configure(v) => {
                (v.target.session_id(), v.target.generation())
            }
            ExternalHarnessRequestKind::Reconcile(v) => (&v.session_id, v.generation),
        }
    }
}

/// Retained service outcome. No auth status, challenge, callback, or secret-bearing type exists here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "response", rename_all = "snake_case")]
pub enum ExternalHarnessResponseKind {
    /// Native result evidence, not inspected capabilities.
    Selected {
        target: HarnessSessionTarget,
        evidence: bcode_harness_models::HarnessModelEvidence,
    },
    Prepared(HarnessPreparedSession),
    Opened(HarnessSessionBinding),
    Receipt(HarnessReceipt),
    Acknowledged {
        mutation_id: String,
    },
    Reconciled(HarnessReconciliation),
    Failure(HarnessFailure),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalHarnessResponse {
    pub schema_version: u16,
    pub request_id: String,
    pub operation: HarnessOperation,
    pub kind: ExternalHarnessResponseKind,
}
impl ExternalHarnessResponse {
    /// Retain selected evidence against its exact extension request.
    /// # Errors
    /// Rejects discovery, wrong families and uncorrelated results.
    pub fn from_models_response(
        request: &bcode_harness_models::HarnessModelsRequest,
        response: &bcode_harness_models::HarnessModelsResponse,
    ) -> Result<Self, String> {
        ExternalHarnessRequest::from_models_request(request)?;
        response
            .validate_for(request)
            .map_err(|_| "invalid selected-open response")?;
        let kind = match &response.kind {
            bcode_harness_models::HarnessModelsResponseKind::Selected { target, evidence } => {
                ExternalHarnessResponseKind::Selected {
                    target: target.clone(),
                    evidence: evidence.clone(),
                }
            }
            bcode_harness_models::HarnessModelsResponseKind::Failure(failure) => {
                ExternalHarnessResponseKind::Failure(*failure)
            }
            bcode_harness_models::HarnessModelsResponseKind::Models { .. } => {
                return Err("discovery cannot enter mutation storage".into());
            }
        };
        Ok(Self {
            schema_version: response.schema_version,
            request_id: response.request_id.clone(),
            operation: HarnessOperation::Open,
            kind,
        })
    }

    /// Reconstruct selected evidence without native redispatch.
    /// # Errors
    /// Rejects legacy requests and mismatched result evidence.
    pub fn models_response(
        &self,
        request: &ExternalHarnessRequest,
    ) -> Result<bcode_harness_models::HarnessModelsResponse, String> {
        request.validate()?;
        let ExternalHarnessRequestKind::OpenSelected(selected) = &request.kind else {
            return Err("not a selected-open request".into());
        };
        if self.operation != HarnessOperation::Open {
            return Err("selected-open operation mismatch".into());
        }
        let kind = match &self.kind {
            ExternalHarnessResponseKind::Selected { target, evidence } => {
                bcode_harness_models::HarnessModelsResponseKind::Selected {
                    target: target.clone(),
                    evidence: evidence.clone(),
                }
            }
            ExternalHarnessResponseKind::Failure(failure) => {
                bcode_harness_models::HarnessModelsResponseKind::Failure(*failure)
            }
            _ => return Err("selected open requires selected result evidence".into()),
        };
        let response = bcode_harness_models::HarnessModelsResponse {
            schema_version: self.schema_version,
            request_id: self.request_id.clone(),
            instance_id: request.instance.instance_id.clone(),
            kind,
        };
        response
            .validate_for(&selected.request)
            .map_err(|_| "invalid selected-open result")?;
        Ok(response)
    }

    /// Validate against the exact retained family, never a downgraded Open.
    /// # Errors
    /// Rejects mismatched families, identities, targets and model evidence.
    pub fn validate_for(&self, request: &ExternalHarnessRequest) -> Result<(), String> {
        request.validate()?;
        if request.is_selected_open() {
            self.models_response(request).map(|_| ())
        } else {
            self.response()?
                .validate_for(&request.request()?)
                .map_err(|_| "invalid retained harness response".into())
        }
    }
    /// Validate a non-auth result against the exact retained request.
    /// # Errors
    /// Rejects unsupported families, mismatches and malformed native responses.
    pub fn from_response(
        request: &HarnessRequest,
        response: &HarnessServiceResponse,
    ) -> Result<Self, String> {
        response
            .validate_for(request)
            .map_err(|_| "invalid retained harness response".to_owned())?;
        let kind = match &response.kind {
            HarnessServiceResponseKind::Prepared(v) => {
                ExternalHarnessResponseKind::Prepared(v.clone())
            }
            HarnessServiceResponseKind::Opened(v) => ExternalHarnessResponseKind::Opened(v.clone()),
            HarnessServiceResponseKind::Receipt(v) => {
                ExternalHarnessResponseKind::Receipt(v.clone())
            }
            HarnessServiceResponseKind::Acknowledged { mutation_id } => {
                ExternalHarnessResponseKind::Acknowledged {
                    mutation_id: mutation_id.clone(),
                }
            }
            HarnessServiceResponseKind::Reconciled(v) => {
                ExternalHarnessResponseKind::Reconciled(v.clone())
            }
            HarnessServiceResponseKind::Failure(v) => ExternalHarnessResponseKind::Failure(*v),
            _ => return Err("response cannot enter canonical mutation store".into()),
        };
        Ok(Self {
            schema_version: response.schema_version,
            request_id: response.request_id.clone(),
            operation: response.operation,
            kind,
        })
    }
    /// Reproduce the committed result without another native invocation.
    /// # Errors
    /// Rejects selected results; use `models_response` with the retained request.
    pub fn response(&self) -> Result<HarnessServiceResponse, String> {
        let kind = match &self.kind {
            ExternalHarnessResponseKind::Selected { .. } => {
                return Err("selected result cannot be downgraded".into());
            }
            ExternalHarnessResponseKind::Prepared(v) => {
                HarnessServiceResponseKind::Prepared(v.clone())
            }
            ExternalHarnessResponseKind::Opened(v) => HarnessServiceResponseKind::Opened(v.clone()),
            ExternalHarnessResponseKind::Receipt(v) => {
                HarnessServiceResponseKind::Receipt(v.clone())
            }
            ExternalHarnessResponseKind::Acknowledged { mutation_id } => {
                HarnessServiceResponseKind::Acknowledged {
                    mutation_id: mutation_id.clone(),
                }
            }
            ExternalHarnessResponseKind::Reconciled(v) => {
                HarnessServiceResponseKind::Reconciled(v.clone())
            }
            ExternalHarnessResponseKind::Failure(v) => HarnessServiceResponseKind::Failure(*v),
        };
        Ok(HarnessServiceResponse {
            schema_version: self.schema_version,
            request_id: self.request_id.clone(),
            operation: self.operation,
            kind,
        })
    }
}

/// Current admitted config and inspected capabilities. No native session is invented before open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalHarnessState {
    pub owner: ExternalHarnessOwner,
    pub instance: HarnessInstance,
    pub capabilities: HarnessCapabilities,
    pub binding: Option<HarnessSessionBinding>,
    pub target: Option<HarnessSessionTarget>,
    /// Last atomically committed native poll cursor, distinct from offline history cursors.
    pub observation_after: Option<String>,
    pub prepared_open: Option<ExternalHarnessRequest>,
}
/// One independently versioned canonical managed-operation envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalHarnessOperation {
    pub version: u16,
    pub owner: ExternalHarnessOwner,
    pub record: ExternalHarnessOperationRecord,
    /// Host-computed current-inventory commitments; never supplied by a native adapter.
    #[serde(default)]
    pub inventory_commitments: Vec<HarnessInventoryCommitment>,
}

/// Counters committed by the host over validated canonical lifecycle observations.
///
/// These establish historical lifecycle facts at the committed revision, not availability
/// or integrity of every derived proof descendant. They do not establish coverage, complete
/// output, adapter acknowledgement, or workflow success. Output must be selected and verified
/// independently against exact canonical observations. Missing descendants remain repair-required.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessInventorySummary {
    /// Number of retained identities, including terminal failures.
    pub identities: u64,
    /// Nonterminal or unknown work/continuations.
    pub unfinished: u64,
    /// Notification or attribution obligations not yet discharged.
    pub obligations: u64,
    /// Retained immutable failed outcomes.
    pub failed: u64,
    /// Retained immutable interrupted outcomes.
    pub interrupted: u64,
}

/// Canonical commitment for one exact inventory namespace at one event revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessInventoryCommitment {
    /// Generation-scoped namespace, including exact attempt and lifecycle kind.
    pub snapshot_key: String,
    /// Canonical operation sequence that produced this root.
    pub revision: u64,
    /// Content address of a bounded immutable proof node; absent for an empty inventory.
    pub root: Option<String>,
    /// Authenticated subtree totals. Coverage and physical proof availability are separate;
    /// a root hash does not certify that every descendant is present in derived storage.
    pub summary: HarnessInventorySummary,
}

/// Exact portable inventory selection. Unknown-origin obligations are session-wide.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HarnessInventoryScope {
    /// Exact retained submission attempt.
    Attempt(bcode_harness_models::HarnessAttemptIdentity),
    /// Uncorrelated work, continuations, and unanswered interactions.
    Uncorrelated,
}

/// Lifecycle inventory family; no arbitrary namespace is accepted from consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HarnessInventoryKind {
    Terminal,
    Coverage,
    Work,
    Continuation,
    Interaction,
}

/// Restart-portable position inside a canonical, immutable inventory snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessInventoryCursor {
    pub owner: ExternalHarnessOwner,
    pub commitment: HarnessInventoryCommitment,
    /// Number of identities before the next window; not a claim that they were inspected.
    pub offset: u64,
}

/// One bounded canonical membership reference. Resolve only at this retained revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessInventoryEntry {
    pub key: String,
    pub event_sequence: u64,
    /// Exact retained canonical observation; never refreshed from a later lifecycle version.
    pub observation: bcode_harness_models::HarnessObservation,
}

/// Authenticated canonical summary without enumerating physical descendants.
/// This is historical lifecycle truth, not proof of complete output or execution success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessInventorySummaryRead {
    pub owner: ExternalHarnessOwner,
    pub commitment: HarnessInventoryCommitment,
    pub observed_through_sequence: u64,
}

/// Exact identity lookup at a caller-selected canonical commitment, not an adapter ACK.
/// Verifies the selected proof path only, not availability of the complete inventory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessInventoryItemRead {
    pub owner: ExternalHarnessOwner,
    pub commitment: HarnessInventoryCommitment,
    pub observed_through_sequence: u64,
    /// None proves absence only in the selected snapshot, never future silence.
    pub entry: Option<HarnessInventoryEntry>,
}

/// Bounded page from an authenticated snapshot. Exhaustion is not execution success.
///
/// Only accessed proof paths and returned canonical observations are verified; this response
/// does not certify physical integrity of unvisited descendants, even on the final window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessInventoryPage {
    pub owner: ExternalHarnessOwner,
    pub commitment: HarnessInventoryCommitment,
    /// Canonical session tail inspected for this read. A multi-family consumer must pin
    /// or recheck this value before combining independently returned current snapshots.
    pub observed_through_sequence: u64,
    pub entries: Vec<HarnessInventoryEntry>,
    pub next_cursor: Option<HarnessInventoryCursor>,
    /// This window reaches the end; does not certify that a caller inspected earlier windows.
    pub inventory_exhausted: bool,
}
/// Exact daemon lifetime provenance, supplied by the authenticated serving runtime.
/// The artifact identity is deliberately separate from the legacy owner fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalHarnessLifetime {
    pub daemon_instance_id: String,
    pub artifact_id: String,
    pub state_location_id: String,
}
impl ExternalHarnessLifetime {
    /// Validate bounded identity shape, not physical ownership or lock release.
    /// # Errors
    /// Rejects absent, oversized or control-bearing identities.
    pub fn validate(&self) -> Result<(), String> {
        if [
            &self.daemon_instance_id,
            &self.artifact_id,
            &self.state_location_id,
        ]
        .iter()
        .any(|value| {
            value.is_empty()
                || value.len() > MAX_HARNESS_ID_BYTES
                || value.chars().any(char::is_control)
        }) {
            return Err("invalid managed lifetime provenance".into());
        }
        Ok(())
    }
}

/// Host recording of an exact native transport's physical release, not work settlement.
/// A Close acknowledgement alone must never be converted into this record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalHarnessTransportRelease {
    pub close_request_id: String,
    pub instance: HarnessInstance,
    pub binding: HarnessSessionBinding,
    pub lifetime: ExternalHarnessLifetime,
    /// Exact native integration's bounded reap evidence identity.
    pub reap_id: String,
}
impl ExternalHarnessTransportRelease {
    /// Validate shape and owner correlation, never infer a reap or released lifetime.
    /// # Errors
    /// Rejects foreign bindings, daemon identities and malformed evidence.
    pub fn validate_for(&self, owner: &ExternalHarnessOwner) -> Result<(), String> {
        owner.validate()?;
        self.lifetime.validate()?;
        self.instance
            .validate()
            .map_err(|_| "invalid release instance")?;
        self.binding
            .validate_owner(&owner.session_id.to_string(), owner.generation)
            .map_err(|_| "invalid release binding")?;
        if self.binding.instance_id != self.instance.instance_id
            || self.lifetime.daemon_instance_id != owner.daemon_id
            || [&self.close_request_id, &self.reap_id].iter().any(|value| {
                value.is_empty()
                    || value.len() > MAX_HARNESS_ID_BYTES
                    || value.chars().any(char::is_control)
            })
        {
            return Err("invalid transport release correlation".into());
        }
        Ok(())
    }
}

/// Explicit generation-fenced resume intent. It contains no previous submission text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalHarnessRecovery {
    pub previous_owner: ExternalHarnessOwner,
    /// The canonical event the recovery follows: the previous owner's release, or for an
    /// abandoned transport its last canonical harness event.
    pub release_event_sequence: u64,
    /// Transport end evidence. For an abandoned transport it is reconstructed: the previous
    /// daemon's released execution lifetime with [`ABANDONED_TRANSPORT_EVIDENCE`] markers.
    pub release: ExternalHarnessTransportRelease,
    pub capabilities: HarnessCapabilities,
    pub resume_request: ExternalHarnessRequest,
    /// The previous daemon ended without releasing the native transport (for example it was
    /// killed). Its execution lifetime is proven released by the recovering daemon and its
    /// native child cannot outlive it; no previous input is replayed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub abandoned: bool,
}

/// Marker carried as `close_request_id` and `reap_id` by an abandoned transport's
/// reconstructed release evidence; no native Close or reap ever produced it.
pub const ABANDONED_TRANSPORT_EVIDENCE: &str = "daemon-lifetime-released";
impl ExternalHarnessRecovery {
    /// Validate the exact next generation and retained native identity.
    /// # Errors
    /// Rejects overflow, same-daemon transfer, changed instance/native identity and non-resume intent.
    pub fn validate_for(&self, owner: &ExternalHarnessOwner) -> Result<(), String> {
        owner.validate()?;
        self.release.validate_for(&self.previous_owner)?;
        self.capabilities
            .validate()
            .map_err(|_| "invalid recovery capabilities")?;
        self.resume_request
            .request()?
            .validate()
            .map_err(|_| "invalid recovery request")?;
        if self.capabilities.resume.support
            == bcode_harness_models::HarnessSupportLevel::Unsupported
        {
            return Err("native resume is unsupported".into());
        }
        let ExternalHarnessRequestKind::Open(open) = &self.resume_request.kind else {
            return Err("recovery requires explicit resume".into());
        };
        let Some(binding) = &open.resume else {
            return Err("recovery cannot create a native session".into());
        };
        let markers = [&self.release.close_request_id, &self.release.reap_id]
            .iter()
            .filter(|value| value.as_str() == ABANDONED_TRANSPORT_EVIDENCE)
            .count();
        if (self.abandoned && markers != 2) || (!self.abandoned && markers != 0) {
            return Err("recovery evidence does not match its transport end".into());
        }
        let mut expected = self.release.binding.clone();
        expected.generation = owner.generation;
        if self.release_event_sequence == 0
            || self.previous_owner.session_id != owner.session_id
            || self.previous_owner.generation.checked_add(1) != Some(owner.generation)
            || self.previous_owner.daemon_id == owner.daemon_id
            || self.resume_request.instance != self.release.instance
            || self.resume_request.request_id != open.open_id
            || open.open_id == self.release.close_request_id
            || open.session_id != owner.session_id.to_string()
            || open.generation != owner.generation
            || binding != &expected
            || open.parent_assistance.is_some()
        {
            return Err("invalid recovery transition".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExternalHarnessOperationRecord {
    TransportReleased {
        release: ExternalHarnessTransportRelease,
    },
    RecoveryPrepared {
        recovery: Box<ExternalHarnessRecovery>,
    },
    Configured {
        instance: HarnessInstance,
        /// Boxed to keep the record enum compact; serialized transparently.
        capabilities: Box<HarnessCapabilities>,
        open_request: Option<ExternalHarnessRequest>,
    },
    DeliveryClaimed {
        request: ExternalHarnessRequest,
    },
    DeliveryCompleted {
        request: ExternalHarnessRequest,
        response: ExternalHarnessResponse,
    },
    ObservationPage {
        instance: HarnessInstance,
        after: Option<String>,
        page: HarnessObservationPage,
    },
}
impl ExternalHarnessOperation {
    /// Validate version, shape, correlation and bounded serialization without retaining auth data.
    /// # Errors
    /// Rejects invalid owners, mismatched targets, unsupported contracts and excessive payloads.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("unsupported harness operation version".into());
        }
        self.owner.validate()?;
        if self.inventory_commitments.len() > MAX_HARNESS_OBSERVATIONS * 3 + 6
            || self.inventory_commitments.iter().any(|value| {
                value.snapshot_key.len() > MAX_HARNESS_ID_BYTES * 8 + 512
                    || value.root.as_ref().is_some_and(|root| {
                        root.len() != 64 || !root.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
            })
        {
            return Err("invalid host inventory commitments".into());
        }
        self.validate_record()?;
        serde_json::to_writer(Counter(0), self)
            .map_err(|_| "canonical operation budget exceeded".into())
    }
    /// Whether this operation carries selected-open canonical authority.
    #[must_use]
    pub const fn is_selected_open(&self) -> bool {
        match &self.record {
            ExternalHarnessOperationRecord::Configured {
                open_request: Some(request),
                ..
            }
            | ExternalHarnessOperationRecord::DeliveryClaimed { request }
            | ExternalHarnessOperationRecord::DeliveryCompleted { request, .. } => {
                request.is_selected_open()
            }
            _ => false,
        }
    }

    /// Whether this operation carries a representation introduced by the native-parity schema.
    ///
    /// Recovery records are included so that, once published, they inherit this fence.
    #[must_use]
    pub fn requires_native_parity(&self) -> bool {
        match &self.record {
            ExternalHarnessOperationRecord::TransportReleased { .. }
            | ExternalHarnessOperationRecord::RecoveryPrepared { .. } => true,
            ExternalHarnessOperationRecord::DeliveryClaimed { request }
            | ExternalHarnessOperationRecord::DeliveryCompleted { request, .. } => {
                request.requires_native_parity()
            }
            ExternalHarnessOperationRecord::Configured { .. } => false,
            ExternalHarnessOperationRecord::ObservationPage { page, .. } => page
                .observations
                .iter()
                .any(|observation| observation_requires_native_parity(&observation.kind)),
        }
    }

    /// Validate the event representation without inferring writer authority.
    /// # Errors
    /// Rejects selected-open records before schema 57 and native-parity records before 58.
    pub fn validate_event_schema(&self, schema_version: u16) -> Result<(), String> {
        self.validate()?;
        if self.is_selected_open() && schema_version < SELECTED_OPEN_EVENT_SCHEMA_VERSION {
            return Err("selected-open event schema is unavailable".into());
        }
        if self.requires_native_parity() && schema_version < NATIVE_PARITY_EVENT_SCHEMA_VERSION {
            return Err("native-parity event schema is unavailable".into());
        }
        Ok(())
    }

    /// Validate the independently allocated event and writer fence.
    /// # Errors
    /// Rejects selected-open records before schema57/writer25, native-parity records before
    /// schema58/writer26, and all recovery records while recovery storage is unpublished.
    pub fn validate_storage(&self, schema_version: u16, writer_epoch: u32) -> Result<(), String> {
        self.validate()?;
        if self.is_selected_open()
            && (schema_version < SELECTED_OPEN_EVENT_SCHEMA_VERSION
                || writer_epoch < SELECTED_OPEN_STORAGE_WRITER_EPOCH)
        {
            return Err("selected-open storage fence is unavailable".into());
        }
        if self.requires_native_parity()
            && (schema_version < NATIVE_PARITY_EVENT_SCHEMA_VERSION
                || writer_epoch < NATIVE_PARITY_STORAGE_WRITER_EPOCH)
        {
            return Err("native-parity storage fence is unavailable".into());
        }
        Ok(())
    }

    fn validate_record(&self) -> Result<(), String> {
        match &self.record {
            ExternalHarnessOperationRecord::TransportReleased { release } => {
                if !recovery_storage_available() {
                    return Err("managed harness recovery storage is unpublished".into());
                }
                release.validate_for(&self.owner)?;
            }
            ExternalHarnessOperationRecord::RecoveryPrepared { recovery } => {
                if !recovery_storage_available() {
                    return Err("managed harness recovery storage is unpublished".into());
                }
                recovery.validate_for(&self.owner)?;
            }
            ExternalHarnessOperationRecord::Configured {
                instance,
                capabilities,
                open_request,
            } => {
                instance
                    .validate()
                    .map_err(|_| "invalid harness instance".to_owned())?;
                capabilities
                    .validate()
                    .map_err(|_| "invalid harness capabilities".to_owned())?;
                if let Some(request) = open_request {
                    if request.instance != *instance
                        || request.open().is_none()
                        || request.target()
                            != (
                                self.owner.session_id.to_string().as_str(),
                                self.owner.generation,
                            )
                    {
                        return Err("invalid retained pre-open preparation".into());
                    }
                    request.validate()?;
                }
            }
            ExternalHarnessOperationRecord::DeliveryClaimed { request }
            | ExternalHarnessOperationRecord::DeliveryCompleted { request, .. } => {
                request.validate()?;
                let (session, generation) = request.target();
                if session != self.owner.session_id.to_string()
                    || generation != self.owner.generation
                {
                    return Err("harness request owner mismatch".into());
                }
                if let ExternalHarnessOperationRecord::DeliveryCompleted { response, .. } =
                    &self.record
                {
                    response.validate_for(request)?;
                }
            }
            ExternalHarnessOperationRecord::ObservationPage {
                instance,
                after,
                page,
            } => {
                instance
                    .validate()
                    .map_err(|_| "invalid page instance".to_owned())?;
                if !page.live.is_empty() {
                    return Err("transient live sidecar must be stripped before commit".into());
                }
                HarnessServiceResponse {
                    schema_version: HARNESS_SCHEMA_VERSION,
                    request_id: "canonical-page".into(),
                    operation: HarnessOperation::Observe,
                    kind: HarnessServiceResponseKind::Observations(page.clone()),
                }
                .validate()
                .map_err(|_| "invalid bounded native page".to_owned())?;
                if after.iter().chain(page.next_after.iter()).any(|v| {
                    v.is_empty()
                        || v.len() > MAX_HARNESS_ID_BYTES
                        || v.chars().any(char::is_control)
                }) || page.observations.len() > MAX_HARNESS_OBSERVATIONS
                {
                    return Err("invalid native page bounds".into());
                }
                let mut seen = std::collections::BTreeSet::new();
                for observation in &page.observations {
                    if !seen.insert(&observation.observation_id) {
                        return Err("duplicate native observation identity".into());
                    }
                    observation
                        .validate()
                        .map_err(|_| "invalid native page observation".to_owned())?;
                    if observation.session_id != self.owner.session_id.to_string()
                        || observation.generation != self.owner.generation
                    {
                        return Err("native page owner mismatch".into());
                    }
                }
            }
        }
        Ok(())
    }
}
/// Claim result is delivery authority only for `New`, under the held canonical owner guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalHarnessClaim {
    New,
    Recorded(Box<ExternalHarnessResponse>),
    Uncertain,
}

struct Counter(usize);
impl std::io::Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        if self.0 > MAX_HARNESS_WIRE_BYTES * 3 {
            return Err(std::io::Error::other("canonical operation budget exceeded"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod native_parity_tests {
    use super::*;
    use bcode_harness_models::{
        HarnessActivity, HarnessAttention, HarnessAuthBinding, HarnessAuthCustody,
        HarnessCompleteness, HarnessConnectivity, HarnessContinuation, HarnessExecutionStatus,
        HarnessLiveStream, HarnessLiveUpdate, HarnessObservation, HarnessPermissionMode,
        HarnessSessionSetting,
    };

    fn owner() -> ExternalHarnessOwner {
        ExternalHarnessOwner {
            session_id: SessionId::new(),
            generation: 3,
            artifact_id: "artifact".into(),
            daemon_id: "daemon".into(),
        }
    }

    fn instance() -> HarnessInstance {
        HarnessInstance {
            instance_id: "instance".into(),
            plugin_id: "plugin".into(),
            executable: "/bin/native".into(),
            config_namespace: "/config".into(),
            auth: HarnessAuthBinding {
                provider_id: "provider".into(),
                profile: "profile".into(),
                credential_namespace: "/config".into(),
                custody: HarnessAuthCustody::NativeManaged,
            },
        }
    }

    fn target(owner: &ExternalHarnessOwner) -> HarnessSessionTarget {
        HarnessSessionTarget::Confirmed(HarnessSessionBinding {
            schema_version: HARNESS_SCHEMA_VERSION,
            session_id: owner.session_id.to_string(),
            instance_id: "instance".into(),
            native_session_id: "native".into(),
            native_version: "1".into(),
            generation: owner.generation,
        })
    }

    fn operation(
        owner: &ExternalHarnessOwner,
        record: ExternalHarnessOperationRecord,
    ) -> ExternalHarnessOperation {
        ExternalHarnessOperation {
            version: 1,
            owner: owner.clone(),
            record,
            inventory_commitments: Vec::new(),
        }
    }

    fn page(owner: &ExternalHarnessOwner, kind: HarnessObservationKind) -> HarnessObservationPage {
        HarnessObservationPage {
            retention: None,
            hydration_requests: Vec::new(),
            inventory_page: None,
            observations: vec![HarnessObservation {
                observation_id: "observation".into(),
                session_id: owner.session_id.to_string(),
                generation: owner.generation,
                native_correlations: Vec::new(),
                completeness: HarnessCompleteness::Complete,
                kind,
            }],
            next_after: Some("next".into()),
            completeness: HarnessCompleteness::Partial,
            status: HarnessExecutionStatus {
                connectivity: HarnessConnectivity::Connected,
                activity: HarnessActivity::Running,
                attention: HarnessAttention::None,
                continuation: HarnessContinuation::Unknown,
                completeness: HarnessCompleteness::Partial,
            },
            live: Vec::new(),
        }
    }

    #[test]
    fn configure_round_trips_and_requires_the_native_parity_fence() {
        let owner = owner();
        let request = HarnessRequest {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: "configure".into(),
            instance: instance(),
            kind: HarnessRequestKind::Configure(HarnessConfigureRequest {
                target: target(&owner),
                mutation_id: "configure".into(),
                setting: HarnessSessionSetting::PermissionMode {
                    mode: HarnessPermissionMode::Plan,
                },
            }),
        };
        let retained = ExternalHarnessRequest::from_request(&request).unwrap();
        assert!(retained.requires_native_parity());
        assert_eq!(
            retained.target(),
            (owner.session_id.to_string().as_str(), owner.generation)
        );
        assert!(matches!(
            retained.request().unwrap().kind,
            HarnessRequestKind::Configure(_)
        ));
        let claimed = operation(
            &owner,
            ExternalHarnessOperationRecord::DeliveryClaimed { request: retained },
        );
        claimed.validate().unwrap();
        claimed.validate_storage(58, 26).unwrap();
        claimed.validate_event_schema(58).unwrap();
        assert!(claimed.validate_storage(57, 26).is_err());
        assert!(claimed.validate_storage(58, 25).is_err());
        assert!(claimed.validate_event_schema(57).is_err());
    }

    #[test]
    fn native_parity_observations_are_fenced_and_live_is_never_persisted() {
        let owner = owner();
        let reasoning = operation(
            &owner,
            ExternalHarnessOperationRecord::ObservationPage {
                instance: instance(),
                after: None,
                page: page(
                    &owner,
                    HarnessObservationKind::Reasoning {
                        segment_id: None,
                        text: "thinking".into(),
                        opaque: false,
                    },
                ),
            },
        );
        assert!(reasoning.requires_native_parity());
        reasoning.validate_storage(58, 26).unwrap();
        assert!(reasoning.validate_storage(57, 25).is_err());
        let legacy_text = operation(
            &owner,
            ExternalHarnessOperationRecord::ObservationPage {
                instance: instance(),
                after: None,
                page: page(
                    &owner,
                    HarnessObservationKind::Text {
                        text: "plain".into(),
                        segment_id: None,
                    },
                ),
            },
        );
        assert!(!legacy_text.requires_native_parity());
        legacy_text.validate_storage(57, 25).unwrap();
        let mut with_live = page(
            &owner,
            HarnessObservationKind::Text {
                text: "plain".into(),
                segment_id: None,
            },
        );
        with_live.live.push(HarnessLiveUpdate {
            stream: HarnessLiveStream::Text,
            segment_id: "segment".into(),
            offset: 0,
            text: "pla".into(),
        });
        assert!(
            operation(
                &owner,
                ExternalHarnessOperationRecord::ObservationPage {
                    instance: instance(),
                    after: None,
                    page: with_live,
                },
            )
            .validate()
            .is_err()
        );
    }
}
