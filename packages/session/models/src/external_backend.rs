//! Canonical typed evidence. Native observations never become tool requests.

use crate::external_harness::{
    ExternalHarnessOperationRecord as R, ExternalHarnessResponseKind as S,
};
use crate::{SessionEvent, SessionEventKind, SessionId};
use bcode_harness_models::{
    HarnessInstance, HarnessInteraction, HarnessObservation, HarnessReceipt, HarnessResponse,
    HarnessSessionBinding, HarnessSubmission,
};
use serde::{Deserialize, Serialize};

/// Maximum encoded durable evidence. Raw protocol and auth callback material are not accepted.
pub const MAX_EXTERNAL_RECORD_BYTES: usize = 256 * 1024;
/// Maximum indexed page size.
pub const MAX_EXTERNAL_PAGE_EVENTS: usize = 128;
/// Maximum retained bytes returned by one indexed page.
pub const MAX_EXTERNAL_PAGE_BYTES: usize = 1024 * 1024;

/// Sanitized, typed host evidence. Recording this does not authorize native execution.
///
/// State is canonical only as retained evidence. The persistence boundary is not a
/// secret detector: callers must sanitize observations before append. Auth callback
/// material has deliberately no variant here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExternalBackendRecord {
    /// Binding is immutable except for a verified ownership generation increment.
    Binding {
        instance: HarnessInstance,
        binding: HarnessSessionBinding,
    },
    /// Accepted input must be committed before native delivery is attempted.
    Submission { submission: HarnessSubmission },
    /// Delivery/settlement evidence, not an instruction to replay.
    Receipt { receipt: HarnessReceipt },
    /// Normalized plugin output; tools/subagents remain observations only.
    Observation { observation: HarnessObservation },
    /// Explicit interaction response, correlated to its observed request.
    Response {
        interaction: HarnessInteraction,
        response: HarnessResponse,
    },
}

/// Versioned API adapter over explicit semantic event variants; never persisted as a trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalBackendEnvelope {
    pub version: u16,
    pub record: ExternalBackendRecord,
}

impl ExternalBackendRecord {
    /// Canonical Bcode session correlation.
    #[must_use]
    pub fn session_id(&self) -> &str {
        match self {
            Self::Binding { binding, .. } => &binding.session_id,
            Self::Submission { submission } => &submission.session_id,
            Self::Receipt { receipt } => &receipt.session_id,
            Self::Observation { observation } => &observation.session_id,
            Self::Response { response, .. } => &response.session_id,
        }
    }

    /// Ownership generation, independent of event sequence.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        match self {
            Self::Binding { binding, .. } => binding.generation,
            Self::Submission { submission } => submission.generation,
            Self::Receipt { receipt } => receipt.generation,
            Self::Observation { observation } => observation.generation,
            Self::Response { response, .. } => response.generation,
        }
    }

    /// Validate bounded shape and identity, not execution authority.
    /// # Errors
    /// Rejects unsupported or invalid harness contracts without echoing native content.
    pub fn validate(&self) -> Result<(), String> {
        let result = match self {
            Self::Binding { instance, binding } => instance.validate().and_then(|()| {
                binding.validate()?;
                if instance.instance_id != binding.instance_id {
                    return Err(bcode_harness_models::HarnessContractError::WrongOwner);
                }
                Ok(())
            }),
            Self::Submission { submission } => submission.validate(),
            Self::Receipt { receipt } => {
                // Receipt has no standalone validator in the portable contract yet.
                // Reuse submission identity validation without inventing a parallel type.
                HarnessSubmission {
                    submission_id: receipt.submission_id.clone(),
                    attempt_id: receipt.attempt_id.clone(),
                    session_id: receipt.session_id.clone(),
                    generation: receipt.generation,
                    text: String::new(),
                    placement: bcode_harness_models::HarnessPlacement::NextTurn,
                }
                .validate()
                .and_then(|()| {
                    if receipt.native_request_id.as_ref().is_some_and(|id| {
                        id.is_empty() || id.len() > 256 || id.chars().any(char::is_control)
                    }) {
                        return Err(bcode_harness_models::HarnessContractError::InvalidIdentity);
                    }
                    Ok(())
                })
            }
            Self::Observation { observation } => observation.validate(),
            Self::Response {
                interaction,
                response,
            } => response.validate_for(interaction),
        };
        result.map_err(|_| "invalid external backend record".to_owned())?;
        self.session_id()
            .parse::<SessionId>()
            .map_err(|_| "invalid external session identity".to_owned())?;
        Ok(())
    }

    /// Stable exact identity key. A conflicting retry is rejected, never silently replaced.
    #[must_use]
    pub fn identity_key(&self) -> String {
        match self {
            Self::Binding { .. } => key("binding", &self.generation().to_string()),
            Self::Submission { submission } => key("submission", &submission.submission_id),
            Self::Receipt { receipt } => key(
                "receipt",
                &format!(
                    "{}:{}:{:?}",
                    receipt.submission_id.len(),
                    receipt.submission_id,
                    receipt.state
                ),
            ),
            Self::Observation { observation } => key("observation", &observation.observation_id),
            Self::Response { response, .. } => key("response", &response.response_id),
        }
    }

    /// Exact correlation namespaces to materialize alongside the all-record stream.
    #[must_use]
    pub fn correlation_keys(&self) -> Vec<String> {
        match self {
            Self::Binding { .. } => vec![key("binding", "latest")],
            Self::Submission { submission } => {
                vec![key("submission_history", &submission.submission_id)]
            }
            Self::Receipt { receipt } => {
                let mut keys = vec![key("submission_history", &receipt.submission_id)];
                if let Some(native_request_id) = &receipt.native_request_id {
                    keys.push(key("native_request", native_request_id));
                }
                keys
            }
            Self::Response { response, .. } => vec![key("interaction", &response.request_id)],
            Self::Observation { observation } => {
                let mut keys = observation
                    .native_correlations
                    .iter()
                    .map(|id| key("native", id))
                    .collect::<Vec<_>>();
                match &observation.kind {
                    bcode_harness_models::HarnessObservationKind::Interaction { interaction } => {
                        keys.push(key("interaction", &interaction.request_id));
                    }
                    bcode_harness_models::HarnessObservationKind::Settlement { native_turn_id } => {
                        keys.push(key("native", native_turn_id));
                    }
                    bcode_harness_models::HarnessObservationKind::Tool {
                        native_tool_id, ..
                    } => keys.push(key("native_tool", native_tool_id)),
                    bcode_harness_models::HarnessObservationKind::Subagent {
                        native_agent_id,
                        ..
                    } => keys.push(key("native_subagent", native_agent_id)),
                    _ => {}
                }
                keys.sort();
                keys.dedup();
                keys
            }
        }
    }
}

/// Construct a collision-free reserved lookup key; user turn IDs cannot share evidence kinds.
#[must_use]
pub fn external_backend_key(namespace: &str, identity: &str) -> String {
    key(namespace, identity)
}
fn key(namespace: &str, identity: &str) -> String {
    format!(
        "__bcode_external_v1:{}:{namespace}:{identity}",
        namespace.len()
    )
}

impl ExternalBackendEnvelope {
    /// Convert typed evidence to its explicit canonical semantic event variant.
    /// # Errors
    /// Rejects invalid contracts, unsupported versions and excessive payloads.
    pub fn into_event_kind(self) -> Result<SessionEventKind, String> {
        self.validate()?;
        let version = self.version;
        Ok(match self.record {
            ExternalBackendRecord::Binding { instance, binding } => {
                SessionEventKind::ExternalBackendBound {
                    version,
                    instance: Box::new(instance),
                    binding: Box::new(binding),
                }
            }
            ExternalBackendRecord::Submission { submission } => {
                SessionEventKind::ExternalSubmissionAccepted {
                    version,
                    submission: Box::new(submission),
                }
            }
            ExternalBackendRecord::Receipt { receipt } => {
                SessionEventKind::ExternalDeliveryObserved {
                    version,
                    receipt: Box::new(receipt),
                }
            }
            ExternalBackendRecord::Observation { observation } => {
                SessionEventKind::ExternalObservationRecorded {
                    version,
                    observation: Box::new(observation),
                }
            }
            ExternalBackendRecord::Response {
                interaction,
                response,
            } => SessionEventKind::ExternalInteractionResponded {
                version,
                interaction: Box::new(interaction),
                response: Box::new(response),
            },
        })
    }

    /// Validate current compatibility and portable shape.
    /// # Errors
    /// Rejects unknown versions and malformed records.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err("unsupported external backend envelope".into());
        }
        self.record.validate()?;
        // Count serialization without allocating a second arbitrarily large payload.
        serde_json::to_writer(Counter(0), self)
            .map_err(|_| "external record byte budget exceeded".into())
    }

    /// Decode only reserved normalized evidence; unrelated history returns `None`.
    /// # Errors
    /// Rejects oversized, malformed, wrong-session or unsupported evidence.
    pub fn from_event(event: &SessionEvent) -> Result<Option<Self>, String> {
        let result = Self::from_event_kind(&event.kind)?;
        if let Some(envelope) = &result {
            if event.schema_version != crate::CURRENT_SESSION_EVENT_SCHEMA_VERSION {
                return Err("unsupported external event schema".into());
            }
            if envelope.record.session_id() != event.session_id.to_string() {
                return Err("external event session mismatch".into());
            }
        }
        Ok(result)
    }

    /// Read typed canonical evidence without interpreting diagnostic trace events.
    /// # Errors
    /// Rejects unsupported versions, invalid contracts and excessive encoded size.
    pub fn from_event_kind(kind: &SessionEventKind) -> Result<Option<Self>, String> {
        let (version, record) = match kind {
            SessionEventKind::ExternalHarnessOperation { operation } => {
                operation.validate()?;
                if let R::ObservationPage { instance, page, .. } = &operation.record {
                    let Some(binding) = page.observations.iter().find_map(|v| match &v.kind {
                        bcode_harness_models::HarnessObservationKind::BindingConfirmed {
                            binding,
                            ..
                        } => Some(binding.clone()),
                        _ => None,
                    }) else {
                        return Ok(None);
                    };
                    let envelope = Self {
                        version: 1,
                        record: ExternalBackendRecord::Binding {
                            instance: instance.clone(),
                            binding,
                        },
                    };
                    envelope.validate()?;
                    return Ok(Some(envelope));
                }
                let R::DeliveryCompleted { request, response } = &operation.record else {
                    return Ok(None);
                };
                let (S::Opened(binding)
                | S::Reconciled(bcode_harness_models::HarnessReconciliation::Known {
                    binding,
                    ..
                })) = &response.kind
                else {
                    return Ok(None);
                };
                (
                    1,
                    ExternalBackendRecord::Binding {
                        instance: request.instance.clone(),
                        binding: binding.clone(),
                    },
                )
            }
            SessionEventKind::ExternalBackendBound {
                version,
                instance,
                binding,
            } => (
                *version,
                ExternalBackendRecord::Binding {
                    instance: instance.as_ref().clone(),
                    binding: binding.as_ref().clone(),
                },
            ),
            SessionEventKind::ExternalSubmissionAccepted {
                version,
                submission,
            } => (
                *version,
                ExternalBackendRecord::Submission {
                    submission: submission.as_ref().clone(),
                },
            ),
            SessionEventKind::ExternalDeliveryObserved { version, receipt } => (
                *version,
                ExternalBackendRecord::Receipt {
                    receipt: receipt.as_ref().clone(),
                },
            ),
            SessionEventKind::ExternalObservationRecorded {
                version,
                observation,
            } => (
                *version,
                ExternalBackendRecord::Observation {
                    observation: observation.as_ref().clone(),
                },
            ),
            SessionEventKind::ExternalInteractionResponded {
                version,
                interaction,
                response,
            } => (
                *version,
                ExternalBackendRecord::Response {
                    interaction: interaction.as_ref().clone(),
                    response: response.as_ref().clone(),
                },
            ),
            _ => return Ok(None),
        };
        let envelope = Self { version, record };
        envelope.validate()?;
        Ok(Some(envelope))
    }
}

/// Explicit bounded cursor. Sequence is exclusive and is a canonical event sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalBackendQuery {
    pub after_sequence: Option<u64>,
    pub limit: usize,
    pub max_bytes: usize,
    /// `None` selects all external evidence; otherwise an exact reserved correlation key.
    pub correlation_key: Option<String>,
}
impl ExternalBackendQuery {
    /// Validate bounds without clamping or silently changing the requested window.
    /// # Errors
    /// Rejects invalid page budgets and malformed keys.
    pub fn validate(&self) -> Result<(), String> {
        if self
            .after_sequence
            .is_some_and(|sequence| sequence > i64::MAX as u64)
            || self.limit == 0
            || self.limit > MAX_EXTERNAL_PAGE_EVENTS
            || self.max_bytes == 0
            || self.max_bytes > MAX_EXTERNAL_PAGE_BYTES
        {
            return Err("invalid external page bounds".into());
        }
        if let Some(key) = &self.correlation_key
            && (!key.starts_with("__bcode_external_v1:")
                || key.len() > 1024
                || key.chars().any(char::is_control))
        {
            return Err("invalid external correlation key".into());
        }
        Ok(())
    }
}

/// Canonical evidence page; resource exhaustion never masquerades as end-of-history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalBackendPage {
    pub events: Vec<SessionEvent>,
    pub next_after_sequence: Option<u64>,
    pub has_more: bool,
}

struct Counter(usize);
impl std::io::Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        if self.0 > MAX_EXTERNAL_RECORD_BYTES {
            return Err(std::io::Error::other(
                "external record byte budget exceeded",
            ));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
