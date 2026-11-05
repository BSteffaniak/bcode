//! Read-only canonical retention handoff. Acknowledgment transfers storage responsibility,
//! never execution authority or proof that pending work/notifications have completed.
use crate::{
    HarnessContractError, HarnessObservation, MAX_HARNESS_ITEMS, MAX_HARNESS_WIRE_BYTES, id, wire,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeSet;

/// Native correlation namespace, scoped by the enclosing session binding/generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessNativeReferenceKind {
    /// Native turn.
    Turn,
    /// Native item; optionally turn-scoped when the protocol supplies that scope.
    Item,
    /// Native managed process.
    Process,
    /// Native delegated agent.
    Agent,
}
/// Genuine native reference; never an adapter-created execution identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessNativeReference {
    /// Native identity namespace.
    pub kind: HarnessNativeReferenceKind,
    /// Native identity in that namespace.
    pub native_id: String,
    /// Turn discriminator when native item identities are turn-scoped.
    pub native_turn_id: Option<String>,
}
impl HarnessNativeReference {
    /// Match normalized evidence in this native namespace, never adapter IDs.
    ///
    /// Item references require an exact turn correlation only when supplied by the
    /// native protocol. This reusable join predicate is not authorization or completeness.
    #[must_use]
    pub fn matches_observation(&self, observation: &HarnessObservation) -> bool {
        use crate::{
            HarnessExecutionEvent as Event, HarnessObservationKind as Kind, HarnessWorkKind,
        };
        let turn = match &observation.kind {
            Kind::SubmissionAccepted { native_turn_id, .. }
            | Kind::NativeTurnTerminal { native_turn_id, .. } => Some(native_turn_id),
            Kind::Execution {
                event:
                    Event::SubmissionTerminal { native_turn_id, .. }
                    | Event::Continuation { native_turn_id, .. },
            } => native_turn_id.as_ref(),
            _ => None,
        };
        if self.native_turn_id.as_ref().is_some_and(|expected| {
            turn != Some(expected) && !observation.native_correlations.contains(expected)
        }) {
            return false;
        }
        match self.kind {
            HarnessNativeReferenceKind::Turn => turn == Some(&self.native_id),
            HarnessNativeReferenceKind::Process | HarnessNativeReferenceKind::Agent => {
                if let Kind::Execution {
                    event:
                        Event::BackgroundWork {
                            native_task_id,
                            work_kind,
                            ..
                        },
                } = &observation.kind
                {
                    let expected = if self.kind == HarnessNativeReferenceKind::Process {
                        HarnessWorkKind::Command
                    } else {
                        HarnessWorkKind::Subagent
                    };
                    *work_kind == expected && native_task_id.as_ref() == Some(&self.native_id)
                } else {
                    false
                }
            }
            HarnessNativeReferenceKind::Item => {
                matches!(
                    observation.kind,
                    Kind::Execution {
                        event: Event::BackgroundWork { .. }
                    }
                ) && observation.native_correlations.contains(&self.native_id)
            }
        }
    }

    /// Validate bounded identity shape. This establishes no native authority.
    /// # Errors
    /// Rejects malformed native identifiers or supplied turn discriminators.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        id(&self.native_id)?;
        if let Some(turn) = &self.native_turn_id {
            id(turn)?;
        }
        Ok(())
    }
}
/// Exact delivered page retained by the adapter until post-commit acknowledgment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRetentionPage {
    /// Ephemeral adapter connection identity, not a native turn.
    pub connection_id: String,
    /// Stable retryable delivered-page identity.
    pub page_id: String,
    /// SHA-256 of the ordered observations encoded by `harness_observations_digest`.
    pub observations_digest: String,
}
impl HarnessRetentionPage {
    /// Validate page identities and digest encoding.
    /// # Errors
    /// Rejects malformed identities or a digest not encoded as 64 lowercase hex digits.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        id(&self.connection_id)?;
        id(&self.page_id)?;
        digest_shape(&self.observations_digest)
    }
}
/// Authenticated host assertion of canonical recording, not cryptographic attestation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessCanonicalAck {
    /// Exact adapter page recorded by the host.
    pub page_id: String,
    /// Canonical committed revision containing the exact observations.
    pub canonical_revision: u64,
    /// Exact ordered-observation digest from the retained page.
    pub observations_digest: String,
}
impl HarnessCanonicalAck {
    /// Validate structural shape; adapters separately compare the retained connection/page.
    /// # Errors
    /// Rejects malformed identities, digest or zero revision.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        id(&self.page_id)?;
        revision(self.canonical_revision)?;
        digest_shape(&self.observations_digest)
    }
}
/// Adapter request for bounded canonical native-correlation hydration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessHydrationRequest {
    /// Connection-local stable lookup identity.
    pub request_id: String,
    /// Exact native correlation to resolve in the enclosing target only.
    pub reference: HarnessNativeReference,
}
impl HarnessHydrationRequest {
    /// Validate read-only lookup shape.
    /// # Errors
    /// Rejects malformed lookup or native identities.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        id(&self.request_id)?;
        self.reference.validate()
    }
}
/// Bounded result of a canonical lookup. Absence is not lifecycle completion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "resolution", rename_all = "snake_case", deny_unknown_fields)]
pub enum HarnessHydrationResolution {
    /// Canonical correlated observations, including native acceptance when needed.
    Found {
        /// Ordered canonical evidence; never an invented reconstructed outcome.
        observations: Vec<HarnessObservation>,
    },
    /// No matching canonical evidence at the supplied revision.
    Absent,
    /// Bounded authoritative lookup was unavailable; do not silently create a new identity.
    Unavailable,
}
/// Read-only host reply, correlated to a specific outstanding adapter request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessHydrationReply {
    /// Outstanding connection-local request identity.
    pub request_id: String,
    /// Must equal the outstanding native reference.
    pub reference: HarnessNativeReference,
    /// Canonical revision at which lookup was performed.
    pub canonical_revision: u64,
    /// Canonical evidence or explicit incomplete lookup result.
    pub resolution: HarnessHydrationResolution,
}
impl HarnessHydrationReply {
    /// Validate bounded evidence and target scope; semantic association remains host-owned.
    /// # Errors
    /// Rejects malformed IDs, zero revision, excessive/duplicate evidence or wrong owner.
    pub fn validate_for(
        &self,
        session_id: &str,
        generation: u64,
    ) -> Result<(), HarnessContractError> {
        id(&self.request_id)?;
        self.reference.validate()?;
        revision(self.canonical_revision)?;
        if let HarnessHydrationResolution::Found { observations } = &self.resolution {
            if observations.is_empty() || observations.len() > MAX_HARNESS_ITEMS {
                return Err(HarnessContractError::BoundsExceeded);
            }
            let mut seen = BTreeSet::new();
            for observation in observations {
                observation.validate()?;
                if observation.session_id != session_id || observation.generation != generation {
                    return Err(HarnessContractError::WrongOwner);
                }
                if !seen.insert(&observation.observation_id) {
                    return Err(HarnessContractError::InvalidShape);
                }
            }
            let correlated = observations
                .iter()
                .any(|observation| self.reference.matches_observation(observation));
            if !correlated {
                return Err(HarnessContractError::InvalidShape);
            }
        }
        wire(self, MAX_HARNESS_WIRE_BYTES)
    }
}
/// Optional host-supplied retention context for one exact observe target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessRetentionContext {
    /// Exact adapter connection; host restart cannot adopt another transport.
    pub connection_id: String,
    /// Only supplied after canonical commit of the exact delivered page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledgment: Option<HarnessCanonicalAck>,
    /// Bounded replies to outstanding canonical lookup requests.
    #[serde(default)]
    pub hydration: Vec<HarnessHydrationReply>,
}
impl HarnessRetentionContext {
    /// Validate target-scoped read-only evidence.
    /// # Errors
    /// Rejects excessive/duplicate replies, malformed acknowledgement or wrong owners.
    pub fn validate_for(
        &self,
        session_id: &str,
        generation: u64,
    ) -> Result<(), HarnessContractError> {
        id(&self.connection_id)?;
        if let Some(ack) = &self.acknowledgment {
            ack.validate()?;
        }
        if self.hydration.len() > MAX_HARNESS_ITEMS {
            return Err(HarnessContractError::BoundsExceeded);
        }
        let mut seen = BTreeSet::new();
        let mut observation_count = 0;
        for reply in &self.hydration {
            if let HarnessHydrationResolution::Found { observations } = &reply.resolution {
                observation_count += observations.len();
                if observation_count > MAX_HARNESS_ITEMS {
                    return Err(HarnessContractError::BoundsExceeded);
                }
            }
            reply.validate_for(session_id, generation)?;
            if !seen.insert(&reply.request_id) {
                return Err(HarnessContractError::InvalidShape);
            }
        }
        wire(self, MAX_HARNESS_WIRE_BYTES)
    }
}
/// Native inventory consistency. Traversal completion is not comprehensive coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessInventoryConsistency {
    /// Each native page reads a changing inventory independently.
    NonAtomic,
}
/// Progress of one bounded positive-evidence inventory traversal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessInventoryPage {
    /// Adapter-owned scan identity, not a native snapshot token.
    pub scan_id: String,
    /// Zero-based page ordinal in this traversal.
    pub page_index: u64,
    /// Opaque native continuation cursor; absent means traversal ended only.
    pub next_cursor: Option<String>,
    /// Explicit native consistency semantics.
    pub consistency: HarnessInventoryConsistency,
}
impl HarnessInventoryPage {
    /// Validate bounded scan/cursor shape.
    /// # Errors
    /// Rejects malformed scan identities or native cursors.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        id(&self.scan_id)?;
        if let Some(cursor) = &self.next_cursor {
            id(cursor)?;
        }
        Ok(())
    }
}
/// Compute the exact digest of an ordered, bounded observation vector.
///
/// Uses the domain prefix `bcode.harness/ordered-observations/v1\0`, followed by
/// UTF-8 `serde_json` serialization of the observation slice without whitespace and
/// with recursively sorted object keys. IDs and payloads are structurally framed by
/// JSON; observation order is significant. Target/connection headers are validated
/// separately. This digest is content binding, not cryptographic host attestation.
/// # Errors
/// Rejects invalid/duplicate observations, oversized pages or serialization failure.
pub fn harness_observations_digest(
    observations: &[HarnessObservation],
) -> Result<String, HarnessContractError> {
    if observations.len() > crate::MAX_HARNESS_OBSERVATIONS {
        return Err(HarnessContractError::BoundsExceeded);
    }
    let mut seen = BTreeSet::new();
    for observation in observations {
        observation.validate()?;
        if !seen.insert(&observation.observation_id) {
            return Err(HarnessContractError::InvalidShape);
        }
    }
    wire(&observations, MAX_HARNESS_WIRE_BYTES)?;
    let mut value =
        serde_json::to_value(observations).map_err(|_| HarnessContractError::InvalidShape)?;
    sort_objects(&mut value);
    let bytes = serde_json::to_vec(&value).map_err(|_| HarnessContractError::InvalidShape)?;
    let mut hash = Sha256::new();
    hash.update(b"bcode.harness/ordered-observations/v1\0");
    hash.update(bytes);
    Ok(hash
        .finalize()
        .iter()
        .flat_map(|byte| {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            [
                char::from(HEX[usize::from(byte >> 4)]),
                char::from(HEX[usize::from(byte & 15)]),
            ]
        })
        .collect())
}
fn sort_objects(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            object.sort_keys();
            for value in object.values_mut() {
                sort_objects(value);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                sort_objects(value);
            }
        }
        _ => {}
    }
}
fn digest_shape(digest: &str) -> Result<(), HarnessContractError> {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(HarnessContractError::InvalidShape);
    }
    Ok(())
}
const fn revision(revision: u64) -> Result<(), HarnessContractError> {
    if revision == 0 {
        Err(HarnessContractError::InvalidShape)
    } else {
        Ok(())
    }
}
