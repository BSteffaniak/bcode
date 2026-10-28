//! Narrow final-publication contract for durable compaction synthesis.
use bcode_session_models::{SessionEvent, SessionEventKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[path = "compaction_checkpoint_memory.rs"]
mod memory;
pub use memory::{memory_restart_retry, validate_memory_commit, validate_memory_restart};

/// Maximum source identities accepted by one bounded publication.
pub const MAX_COMPACTION_COMMIT_SOURCE_EVENTS: usize = 4096;

/// Exact canonical event identity, computed by [`compaction_event_identity`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionEventIdentity {
    /// Canonical event sequence.
    pub sequence: u64,
    /// SHA-256 of the event's `serde_json` serialization.
    pub sha256: String,
}

/// Compare-and-publish request; source is the ordered model-visible prefix excluding markers.
#[derive(Debug, Clone)]
pub struct CompactionCheckpointCommit {
    /// Exact operational checkpoint namespace.
    pub operation_key: String,
    /// Latest active checkpoint sequence observed by the host.
    pub expected_checkpoint_sequence: u64,
    /// Exact current prior marker, or absence before the first compaction.
    pub expected_prior_marker: Option<CompactionEventIdentity>,
    /// Exact bounded prefix through the new marker's boundary, excluding the prior marker.
    pub source: Vec<CompactionEventIdentity>,
    /// Existing local or provider compaction marker, never a partial checkpoint.
    pub marker: SessionEventKind,
}

/// Exact compare-and-supersede request for an unpublished current-format operation.
///
/// The host validates opaque payload and rejection semantics. Storage validates canonical
/// identities and fences the exact host-selected tail independently of the rejection event.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionCheckpointRestart {
    /// Old operation namespace; its canonical evidence is never erased.
    pub operation_key: String,
    /// Exact latest active checkpoint for the old operation.
    pub expected_checkpoint_sequence: u64,
    /// Exact published marker, or absence before first publication.
    pub expected_prior_marker: Option<CompactionEventIdentity>,
    /// Ordered bounded model-visible source prefix, excluding markers.
    pub source: Vec<CompactionEventIdentity>,
    /// Exact canonical complete-rejection event validated by the host.
    pub rejection: CompactionEventIdentity,
    /// Exact selected canonical tail. Omission preserves the original rejection-as-tail contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_tail: Option<CompactionEventIdentity>,
    /// Distinct fresh operation namespace; stable across retries.
    pub replacement_key: String,
    /// Opaque host-versioned replacement checkpoint.
    pub replacement_payload: String,
    /// Discovery checkpoint namespace.
    pub pointer_key: String,
    /// Opaque host-versioned discovery pointer.
    pub pointer_payload: String,
}

impl std::fmt::Debug for CompactionCheckpointRestart {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CompactionCheckpointRestart")
            .field(
                "expected_checkpoint_sequence",
                &self.expected_checkpoint_sequence,
            )
            .field("source_count", &self.source.len())
            .finish_non_exhaustive()
    }
}

impl CompactionCheckpointRestart {
    pub(crate) fn invalid() -> String {
        "compaction restart precondition changed or invalid".into()
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        use bcode_session_models::CompactionSynthesisCheckpoint as Checkpoint;
        for key in [
            &self.operation_key,
            &self.replacement_key,
            &self.pointer_key,
        ] {
            Checkpoint::validate_key(key).map_err(|_| Self::invalid())?;
            if key.starts_with("session/compaction-restart/v1/") {
                return Err(Self::invalid());
            }
        }
        if self.operation_key == self.replacement_key
            || self.operation_key == self.pointer_key
            || self.replacement_key == self.pointer_key
            || self.source.is_empty()
            || self.source.len() > MAX_COMPACTION_COMMIT_SOURCE_EVENTS
            || self
                .source
                .last()
                // A fresh replacement may include history appended after the old checkpoint.
                // It must still precede the exact host-validated rejection fence.
                .is_some_and(|source| source.sequence >= self.rejection.sequence)
            || self.rejection.sequence <= self.expected_checkpoint_sequence
            || self
                .expected_prior_marker
                .as_ref()
                .is_some_and(|marker| marker.sequence >= self.expected_checkpoint_sequence)
            || self
                .source
                .windows(2)
                .any(|pair| pair[0].sequence >= pair[1].sequence)
        {
            return Err(Self::invalid());
        }
        if self.tail_identity().sequence < self.rejection.sequence
            || self.tail_identity().sequence < self.expected_checkpoint_sequence
            || (self.tail_identity().sequence == self.rejection.sequence
                && self.tail_identity() != &self.rejection)
        {
            return Err(Self::invalid());
        }
        let identities = self
            .source
            .iter()
            .chain(std::iter::once(&self.rejection))
            .chain(self.expected_prior_marker.iter())
            .chain(self.expected_tail.iter());
        for identity in identities {
            if identity.sha256.len() != 64
                || !identity
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(Self::invalid());
            }
        }
        for (key, payload) in [
            (&self.replacement_key, &self.replacement_payload),
            (&self.pointer_key, &self.pointer_payload),
        ] {
            Checkpoint {
                version: 1,
                operation_key: key.clone(),
                host_payload: Some(payload.clone()),
            }
            .validate()
            .map_err(|_| Self::invalid())?;
        }
        Ok(())
    }

    pub(crate) fn receipt_key(&self) -> String {
        format!(
            "session/compaction-restart/v1/{}",
            hex::encode(Sha256::digest(self.replacement_key.as_bytes()))
        )
    }

    pub(crate) fn fingerprint(&self) -> Result<String, String> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| Self::invalid())?;
        Ok(hex::encode(Sha256::digest(bytes)))
    }

    pub(crate) fn tail_identity(&self) -> &CompactionEventIdentity {
        self.expected_tail.as_ref().unwrap_or(&self.rejection)
    }

    pub(crate) fn fence(&self) -> u64 {
        self.tail_identity().sequence
    }

    pub(crate) fn comparison(&self) -> CompactionCheckpointCommit {
        CompactionCheckpointCommit {
            operation_key: self.operation_key.clone(),
            expected_checkpoint_sequence: self.expected_checkpoint_sequence,
            expected_prior_marker: self.expected_prior_marker.clone(),
            source: self.source.clone(),
            // Only the identity/source comparison helpers consume this request.
            marker: SessionEventKind::ContextCompacted {
                summary: String::new(),
                compacted_through_sequence: self.source.last().map_or(0, |item| item.sequence),
            },
        }
    }

    pub(crate) fn events(&self, template: &SessionEvent) -> Result<[SessionEvent; 4], String> {
        use bcode_session_models::CompactionSynthesisCheckpoint as Checkpoint;
        self.validate()?;
        template.sequence.checked_add(4).ok_or_else(Self::invalid)?;
        let receipt = RestartReceipt {
            version: 1,
            fingerprint: self.fingerprint()?,
            replacement_sequence: template.sequence + 2,
        };
        let records = [
            (
                self.receipt_key(),
                Some(serde_json::to_string(&receipt).map_err(|_| Self::invalid())?),
            ),
            (self.operation_key.clone(), None),
            (
                self.replacement_key.clone(),
                Some(self.replacement_payload.clone()),
            ),
            (self.pointer_key.clone(), Some(self.pointer_payload.clone())),
        ];
        let mut events = Vec::with_capacity(4);
        for (key, payload) in records {
            let mut event = template.clone();
            event.sequence =
                template.sequence + u64::try_from(events.len()).map_err(|_| Self::invalid())?;
            event.kind = Checkpoint {
                version: 1,
                operation_key: key,
                host_payload: payload,
            }
            .into_event_kind(template.timestamp_ms)
            .map_err(|_| Self::invalid())?;
            crate::persisted::encode_session_event(&event).map_err(|_| Self::invalid())?;
            events.push(event);
        }
        events.try_into().map_err(|_| Self::invalid())
    }

    pub(crate) fn retry_sequence(&self, event: &SessionEvent) -> Result<u64, String> {
        use bcode_session_models::CompactionSynthesisCheckpoint as Checkpoint;
        let checkpoint = Checkpoint::from_event(event)
            .map_err(|_| Self::invalid())?
            .ok_or_else(Self::invalid)?;
        if checkpoint.operation_key != self.receipt_key() {
            return Err(Self::invalid());
        }
        let receipt: RestartReceipt = serde_json::from_str(
            checkpoint
                .host_payload
                .as_deref()
                .ok_or_else(Self::invalid)?,
        )
        .map_err(|_| Self::invalid())?;
        if receipt.version != 1
            || receipt.fingerprint != self.fingerprint()?
            || event.sequence.checked_add(2) != Some(receipt.replacement_sequence)
        {
            return Err(Self::invalid());
        }
        Ok(receipt.replacement_sequence)
    }

    pub(crate) fn validate_retry_events(&self, events: &[SessionEvent; 4]) -> Result<(), String> {
        let expected = self.events(&events[0])?;
        if *events != expected {
            return Err(Self::invalid());
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestartReceipt {
    version: u32,
    fingerprint: String,
    replacement_sequence: u64,
}

/// Compute a shared exact-source identity without hashing presentation text.
///
/// # Errors
/// Returns a content-free error if canonical event serialization fails.
pub fn compaction_event_identity(event: &SessionEvent) -> Result<CompactionEventIdentity, String> {
    let event = crate::submission::context_event(event.clone());
    let bytes = serde_json::to_vec(&event)
        .map_err(|_| "cannot encode compaction source identity".to_owned())?;
    Ok(CompactionEventIdentity {
        sequence: event.sequence,
        sha256: hex::encode(Sha256::digest(bytes)),
    })
}
