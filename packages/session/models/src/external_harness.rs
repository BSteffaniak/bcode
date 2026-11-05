//! Actor-owned managed harness operation records. Auth transport is deliberately unrepresentable.
use crate::SessionId;
use bcode_harness_models::{
    HARNESS_SCHEMA_VERSION, HarnessCapabilities, HarnessControlRequest, HarnessFailure,
    HarnessInstance, HarnessObservationPage, HarnessOpenRequest, HarnessOperation,
    HarnessPreparedSession, HarnessReceipt, HarnessReconcileRequest, HarnessReconciliation,
    HarnessRequest, HarnessRequestKind, HarnessResponse, HarnessServiceResponse,
    HarnessServiceResponseKind, HarnessSessionBinding, HarnessSessionTarget, HarnessSubmission,
    MAX_HARNESS_ID_BYTES, MAX_HARNESS_OBSERVATIONS, MAX_HARNESS_WIRE_BYTES,
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

/// Secret-free native request retained before attempted delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", content = "request", rename_all = "snake_case")]
pub enum ExternalHarnessRequestKind {
    Open(HarnessOpenRequest),
    Submit {
        target: HarnessSessionTarget,
        submission: HarnessSubmission,
    },
    Respond {
        target: HarnessSessionTarget,
        response: HarnessResponse,
    },
    Interrupt(HarnessControlRequest),
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
    #[must_use]
    pub fn request(&self) -> HarnessRequest {
        let kind = match &self.kind {
            ExternalHarnessRequestKind::Open(v) => HarnessRequestKind::Open(v.clone()),
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
            ExternalHarnessRequestKind::Close(v) => HarnessRequestKind::Close(v.clone()),
            ExternalHarnessRequestKind::Reconcile(v) => HarnessRequestKind::Reconcile(v.clone()),
        };
        HarnessRequest {
            schema_version: self.schema_version,
            request_id: self.request_id.clone(),
            instance: self.instance.clone(),
            kind,
        }
    }
    /// Session and ownership generation claimed by the exact request.
    #[must_use]
    pub fn target(&self) -> (&str, u64) {
        match &self.kind {
            ExternalHarnessRequestKind::Open(v) => (&v.session_id, v.generation),
            ExternalHarnessRequestKind::Submit { submission, .. } => {
                (&submission.session_id, submission.generation)
            }
            ExternalHarnessRequestKind::Respond { response, .. } => {
                (&response.session_id, response.generation)
            }
            ExternalHarnessRequestKind::Interrupt(v) | ExternalHarnessRequestKind::Close(v) => {
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
    Prepared(HarnessPreparedSession),
    Opened(HarnessSessionBinding),
    Receipt(HarnessReceipt),
    Acknowledged { mutation_id: String },
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
    #[must_use]
    pub fn response(&self) -> HarnessServiceResponse {
        let kind = match &self.kind {
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
        HarnessServiceResponse {
            schema_version: self.schema_version,
            request_id: self.request_id.clone(),
            operation: self.operation,
            kind,
        }
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
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExternalHarnessOperationRecord {
    Configured {
        instance: HarnessInstance,
        capabilities: HarnessCapabilities,
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
        self.validate_record()?;
        serde_json::to_writer(Counter(0), self)
            .map_err(|_| "canonical operation budget exceeded".into())
    }
    fn validate_record(&self) -> Result<(), String> {
        match &self.record {
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
                        || !matches!(request.kind, ExternalHarnessRequestKind::Open(_))
                        || request.target()
                            != (
                                self.owner.session_id.to_string().as_str(),
                                self.owner.generation,
                            )
                    {
                        return Err("invalid retained pre-open preparation".into());
                    }
                    request
                        .request()
                        .validate()
                        .map_err(|_| "invalid retained open request".to_owned())?;
                }
            }
            ExternalHarnessOperationRecord::DeliveryClaimed { request }
            | ExternalHarnessOperationRecord::DeliveryCompleted { request, .. } => {
                request
                    .request()
                    .validate()
                    .map_err(|_| "invalid retained harness request".to_owned())?;
                let (session, generation) = request.target();
                if session != self.owner.session_id.to_string()
                    || generation != self.owner.generation
                {
                    return Err("harness request owner mismatch".into());
                }
                if let ExternalHarnessOperationRecord::DeliveryCompleted { response, .. } =
                    &self.record
                {
                    response
                        .response()
                        .validate_for(&request.request())
                        .map_err(|_| "invalid retained response".to_owned())?;
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
