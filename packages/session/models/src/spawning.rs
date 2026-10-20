//! Composed creation and initial admission of an ordinary canonical child session.
//!
//! The retained child identity identifies the entire immutable request, not only creation.
//! Retrying does not authorize a different input or execution selection. Supervision uses
//! ordinary session observation, interaction, messaging, and cancellation operations.

use crate::{SessionChildCreateRequest, SessionSummary, TurnAdmission, TurnAdmissionMetadata};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Immutable composed request retained in the child's canonical creation history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSpawnRequest {
    /// Composed request compatibility version; only version one is supported.
    pub version: u32,
    /// Caller-retained child identity, parent, initial title, and authorized working directory.
    pub creation: SessionChildCreateRequest,
    /// Optional existing worktree directory. Spawning neither creates nor owns its cleanup.
    /// When supplied, it must resolve to the creation working directory.
    pub worktree_directory: Option<PathBuf>,
    /// Initial ordinary turn input. No parent conversation is implicitly copied.
    pub input: String,
    /// Ordinary admission and immutable execution options, including agent/model/reasoning.
    /// Missing producer/key are filled deterministically before canonical creation.
    pub admission: TurnAdmissionMetadata,
    /// Optional configured authentication profile reference, never credentials.
    /// Unsupported or unavailable selections fail before creation or execution.
    pub auth_profile: Option<String>,
}

/// Stage at which a composed operation could not continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSpawnStage {
    /// Structural validation or applicable effect authorization did not complete.
    Preparation,
    /// Canonical creation or publication did not complete or could not be verified.
    Creation,
    /// Ordinary turn admission or queue dispatch did not complete.
    Admission,
}

/// Normalized failure with no private provider, storage, or credential details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSpawnFailure {
    /// Failed composed stage, not a claim that earlier effects were rolled back.
    pub stage: SessionSpawnStage,
    /// Extensible normalized failure code; preserve unknown values.
    pub code: String,
    /// Secret-safe actionable diagnostic.
    pub message: String,
}

/// Inspectable composed outcome, including receipts available after partial failure.
///
/// Creation is not rolled back when admission fails. A missing receipt means it was not
/// verified, not that no effect occurred. An admission receipt does not establish execution
/// or completion. Retry only the identical request with the same retained child identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSpawnResult {
    /// Verified canonical session information, including after a later failure.
    pub session: Option<SessionSummary>,
    /// Exact ordinary admission variant, preserving turn/work/event-sequence identities.
    pub admission: Option<TurnAdmission>,
    /// Stage failure, possibly alongside already durable creation or admission receipts.
    pub failure: Option<SessionSpawnFailure>,
}

impl SessionSpawnResult {
    /// Verify structural receipt consistency without interpreting unknown failure codes.
    ///
    /// # Errors
    /// Rejects admission without creation, an empty outcome, or stage/receipt inconsistencies.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.admission.is_some() && self.session.is_none() {
            return Err("admission requires a verified session");
        }
        if let (Some(session), Some(admission)) = (&self.session, &self.admission) {
            let receipt = match admission {
                TurnAdmission::Accepted(receipt)
                | TurnAdmission::Existing(receipt)
                | TurnAdmission::Deferred(receipt) => Some(receipt),
                _ => None,
            };
            if receipt.is_some_and(|receipt| {
                *receipt
                    != crate::TurnReceipt::from_accepted_event(
                        session.id,
                        receipt.accepted_event_sequence,
                    )
            }) {
                return Err("admission receipt does not belong to the verified canonical session");
            }
        }
        match &self.failure {
            None if self.session.is_none() || self.admission.is_none() => {
                Err("successful spawning requires creation and admission receipts")
            }
            Some(failure) if failure.code.is_empty() || failure.message.is_empty() => {
                Err("failure requires a normalized code and message")
            }
            Some(failure)
                if failure.stage != SessionSpawnStage::Admission && self.admission.is_some() =>
            {
                Err("admission receipt is inconsistent with failure stage")
            }
            Some(failure)
                if failure.stage == SessionSpawnStage::Preparation && self.session.is_some() =>
            {
                Err("preparation failure cannot claim a newly created session")
            }
            Some(failure)
                if failure.stage == SessionSpawnStage::Admission && self.session.is_none() =>
            {
                Err("admission failure requires a verified session")
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_receiptless_admission_failures_are_not_successful_outcomes() {
        let mut result = SessionSpawnResult {
            session: None,
            admission: None,
            failure: None,
        };
        assert!(result.validate().is_err());
        result.failure = Some(SessionSpawnFailure {
            stage: SessionSpawnStage::Preparation,
            code: "denied".into(),
            message: "request was not authorized".into(),
        });
        assert!(result.validate().is_ok());
        result.failure.as_mut().unwrap().stage = SessionSpawnStage::Admission;
        assert!(result.validate().is_err());
        result.failure.as_mut().unwrap().stage = SessionSpawnStage::Creation;
        assert!(result.validate().is_ok());
        result.admission = Some(TurnAdmission::Rejected(
            crate::TurnRejectionReason::ExecutionPolicy,
        ));
        assert!(result.validate().is_err());
    }

    #[test]
    fn composed_request_rejects_unknown_fields_and_preserves_auth_references() {
        let request = SessionSpawnRequest {
            version: 1,
            creation: SessionChildCreateRequest {
                child_id: crate::SessionId::new(),
                parent_id: crate::SessionId::new(),
                name: None,
                working_directory: PathBuf::from("/checkout"),
            },
            worktree_directory: None,
            input: "input".into(),
            admission: TurnAdmissionMetadata::default(),
            auth_profile: Some("configured-profile".into()),
        };
        let mut value = serde_json::to_value(&request).unwrap();
        assert_eq!(
            serde_json::from_value::<SessionSpawnRequest>(value.clone()).unwrap(),
            request
        );
        value
            .as_object_mut()
            .unwrap()
            .insert("unknown".into(), serde_json::Value::Bool(true));
        assert!(serde_json::from_value::<SessionSpawnRequest>(value).is_err());
    }
}
