//! Portable durable milestone contracts. Generation prompts are request-only, never event payloads.
use serde::{Deserialize, Serialize};

/// Shared character-based normalized-text accounting estimate.
#[must_use]
pub fn estimated_normalized_text_tokens_from_chars(chars: usize) -> u64 {
    u64::try_from(chars).unwrap_or(u64::MAX).saturating_add(3) / 4
}
/// Shared normalized-text estimate used for eligible milestone trigger accounting.
#[must_use]
pub fn estimated_normalized_text_tokens(text: &str) -> u64 {
    estimated_normalized_text_tokens_from_chars(text.chars().count())
}

/// Request-only bounded generation input; never append this value to session history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMilestoneContext {
    /// Validated settled conversational range, absent when there is no new settled input.
    pub source: Option<SessionMilestoneSource>,
    /// Canonical tail used to fence admission.
    pub generation: u64,
    /// Explicit name observed at capture.
    pub explicit_name: Option<String>,
    /// Bounded conversational text, excluding generated annotations and tool bodies.
    pub text: String,
    /// Input was truncated by a configured budget.
    pub truncated: bool,
    /// Settled turns since the last successful evaluation checkpoint.
    pub new_turns: u32,
    /// Normalized metered tokens since the last successful evaluation checkpoint.
    pub new_tokens: u64,
    /// Last successful evaluation time, in Unix milliseconds.
    pub last_evaluation_at_ms: Option<u64>,
    /// Total durable evaluation admissions, including failed or ambiguous jobs.
    pub evaluation_count: u64,
}
/// Inclusive canonical source range and a navigation target within it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMilestoneSource {
    /// First conversational event included.
    pub start_sequence: u64,
    /// Last settled conversational event included.
    pub end_sequence: u64,
    /// Conversational event to navigate to.
    pub anchor_sequence: u64,
}
impl SessionMilestoneSource {
    /// Check range geometry; storage also verifies event identity and settled boundaries.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.start_sequence > 0
            && self.start_sequence <= self.anchor_sequence
            && self.anchor_sequence <= self.end_sequence
    }
}
/// Durable generated annotation, never model input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMilestone {
    /// Retry-safe producer-owned evaluation identity.
    pub evaluation_id: String,
    /// Validated canonical source.
    pub source: SessionMilestoneSource,
    /// Short heading.
    pub header: String,
    /// Human-readable description.
    pub description: String,
    /// Normalized generation provenance; no credentials or opaque provider metadata.
    pub provenance: SessionMilestoneProvenance,
}
/// Normalized provenance of the generating request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMilestoneProvenance {
    /// Resolved provider identifier.
    pub provider_id: String,
    /// Resolved model identifier.
    pub model_id: String,
    /// Whether bounded generation input omitted source content.
    pub input_truncated: bool,
}
/// Durable evaluation scheduling fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMilestoneEvaluationStatus {
    /// Canonical admission event sequence.
    pub admitted_sequence: u64,
    /// Immutable admission fence and source.
    pub request: SessionMilestoneEvaluationRequest,
    /// Successful terminal checkpoint, including no-result evaluations.
    pub completed: bool,
}
/// Durable evaluation admission and stale-result fence (contains no generation prompt).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMilestoneEvaluationRequest {
    /// Retry-safe producer-owned identity (at most 256 bytes).
    pub evaluation_id: String,
    /// Source to evaluate.
    pub source: SessionMilestoneSource,
    /// Canonical tail observed before admission.
    pub expected_generation: u64,
    /// Explicit name observed before admission; never overwritten by generation.
    pub expected_explicit_name: Option<String>,
    /// Serialized durable admission cap; absent means no additional caller cap.
    #[serde(default)]
    pub max_requests_per_session: Option<u64>,
}
/// Generated result; optional display title is not an explicit rename.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMilestonePublicationRequest {
    /// Durable annotation.
    pub milestone: SessionMilestone,
    /// Optional generated display title.
    pub automatic_title: Option<String>,
}
/// Successful evaluation checkpoint, including evaluations producing no milestone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMilestoneEvaluationCompletion {
    /// Admitted evaluation being completed.
    pub evaluation_id: String,
}
/// Bounded publication-ordered query (maximum 200 entries).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMilestoneQuery {
    /// Exclusive publication sequence cursor.
    pub after_sequence: Option<u64>,
    /// Requested page size, from 1 through 200.
    pub limit: usize,
}
/// One milestone and its canonical publication sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMilestoneEntry {
    /// Canonical publication sequence.
    pub event_sequence: u64,
    /// Canonical publication time in Unix milliseconds.
    pub timestamp_ms: u64,
    /// Generated annotation.
    pub milestone: SessionMilestone,
}
/// Bounded milestone page, independent of generation plugin availability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMilestonePage {
    /// Entries in ascending publication sequence order.
    pub milestones: Vec<SessionMilestoneEntry>,
    /// Exclusive cursor when another page may exist.
    pub next_after_sequence: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_geometry_rejects_unanchored_and_reversed_ranges() {
        assert!(
            SessionMilestoneSource {
                start_sequence: 2,
                end_sequence: 4,
                anchor_sequence: 3
            }
            .is_valid()
        );
        for (start_sequence, end_sequence, anchor_sequence) in [(0, 4, 2), (4, 2, 3), (2, 4, 5)] {
            assert!(
                !SessionMilestoneSource {
                    start_sequence,
                    end_sequence,
                    anchor_sequence
                }
                .is_valid()
            );
        }
    }
}
