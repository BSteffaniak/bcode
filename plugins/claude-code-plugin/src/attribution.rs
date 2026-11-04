//! Retained exact-attempt producer for normalized consumed/result observations.
use bcode_claude_code::{AttributionStage, SubmissionAttribution, SubmissionUuid};
use bcode_harness_models::{HarnessContractError, HarnessObservationKind, HarnessSubmission};
use std::collections::BTreeMap;

/// Exact admitted attempt plus sanitized native attribution, never a native turn ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedAttribution {
    /// Host identity retained before the native write.
    pub submission_id: String,
    /// Host attempt identity retained before the native write.
    pub attempt_id: String,
    /// Explicit native reply/result correlation; contains no prompt or result text.
    pub evidence: SubmissionAttribution,
}

pub struct Producer {
    attempts: BTreeMap<SubmissionUuid, (String, String)>,
    evidence: Vec<RetainedAttribution>,
}
impl Producer {
    pub(crate) const fn new() -> Self {
        Self {
            attempts: BTreeMap::new(),
            evidence: Vec::new(),
        }
    }
    pub(crate) fn retain(
        &mut self,
        submission: &HarnessSubmission,
    ) -> Result<SubmissionUuid, HarnessContractError> {
        submission.validate()?;
        if self.attempts.len() >= 256 {
            return Err(HarnessContractError::InvalidShape);
        }
        // Only a globally unique host attempt UUID is safe across native resumes.
        // Never recycle process counters: old native frames can outlive a transport.
        // Arbitrary host IDs require coordinator-provided UUID generation support.
        let uuid = SubmissionUuid::new(submission.attempt_id.clone())
            .map_err(|_| HarnessContractError::InvalidIdentity)?;
        if self.attempts.values().any(|(id, attempt)| {
            id == &submission.submission_id || attempt == &submission.attempt_id
        }) {
            return Err(HarnessContractError::InvalidShape);
        }
        self.attempts.insert(
            uuid.clone(),
            (
                submission.submission_id.clone(),
                submission.attempt_id.clone(),
            ),
        );
        Ok(uuid)
    }
    pub(crate) fn record(
        &mut self,
        evidence: &SubmissionAttribution,
    ) -> Result<Vec<HarnessObservationKind>, HarnessContractError> {
        if evidence.resumed
            || evidence.consumed.is_empty()
            || evidence.consumed.len() > 64
            || !evidence.consumed.contains(&evidence.primary)
            || evidence
                .consumed
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != evidence.consumed.len()
            || bcode_claude_code::validate_session_uuid(&evidence.native_frame_uuid).is_err()
        {
            return Err(HarnessContractError::InvalidShape);
        }
        if self.evidence.iter().any(|previous| {
            previous.evidence.native_frame_uuid == evidence.native_frame_uuid
                && previous.evidence != *evidence
        }) {
            return Err(HarnessContractError::InvalidShape);
        }
        let stage = match evidence.stage {
            AttributionStage::Reply => None,
            AttributionStage::Result {
                is_error,
                queued_turn_count,
            } => Some((
                is_error,
                queued_turn_count
                    .map(u32::try_from)
                    .transpose()
                    .map_err(|_| HarnessContractError::BoundsExceeded)?,
            )),
        };
        let mut additions = Vec::new();
        for uuid in &evidence.consumed {
            let (submission_id, attempt_id) = self
                .attempts
                .get(uuid)
                .ok_or(HarnessContractError::InvalidShape)?;
            let retained = RetainedAttribution {
                submission_id: submission_id.clone(),
                attempt_id: attempt_id.clone(),
                evidence: evidence.clone(),
            };
            if let Some(previous) = self.evidence.iter().find(|old| {
                old.evidence.native_frame_uuid == evidence.native_frame_uuid
                    && old.submission_id == *submission_id
            }) {
                if previous != &retained {
                    return Err(HarnessContractError::InvalidShape);
                }
            } else {
                additions.push(retained);
            }
        }
        if self.evidence.len() + additions.len() > 1024 {
            return Err(HarnessContractError::InvalidShape);
        }
        let observations: Vec<_> = additions
            .iter()
            .map(|retained| {
                let submission_id = retained.submission_id.clone();
                let attempt_id = retained.attempt_id.clone();
                let native_frame_id = retained.evidence.native_frame_uuid.clone();
                match stage {
                    None => HarnessObservationKind::SubmissionConsumed {
                        submission_id,
                        attempt_id,
                        native_frame_id,
                    },
                    Some((is_error, queued_user_sends)) => {
                        HarnessObservationKind::SubmissionResultObserved {
                            submission_id,
                            attempt_id,
                            native_frame_id,
                            is_error,
                            queued_user_sends,
                        }
                    }
                }
            })
            .collect();
        self.evidence.extend(additions);
        Ok(observations)
    }
    pub(crate) fn evidence(&self) -> &[RetainedAttribution] {
        &self.evidence
    }
}
