//! Portable contract for transient, plugin-owned advisory session categorization.

use crate::SessionId;
use serde::{Deserialize, Serialize};

/// Plugin service; not a session persistence or execution operation.
pub const CATEGORIZATION_INTERFACE_ID: &str = "bcode.session-categorization/v1";
/// Categorize one newly activated request without reading session history.
pub const OP_CATEGORIZE: &str = "categorize";
/// Maximum UTF-8 source bytes supplied transiently to the plugin.
pub const MAX_CATEGORIZATION_SOURCE_BYTES: usize = 8192;
/// Maximum encoded service request, allowing JSON escaping of the bounded source.
pub const MAX_CATEGORIZATION_REQUEST_BYTES: usize = 6 * MAX_CATEGORIZATION_SOURCE_BYTES + 1024;

/// One transient trigger. Do not log, persist, or echo its source text.
///
/// Hosts supply only the newest activated normal request, truncated on a UTF-8 boundary to
/// `MAX_CATEGORIZATION_SOURCE_BYTES`. Interpretation and sanitization belong to the plugin;
/// provider requests must contain only plugin-owned allowlisted signals, not this source text.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategorizationRequest {
    /// Session owning the disposable advisory result.
    pub session_id: SessionId,
    /// Canonical activation sequence. Hosts reject stale results.
    pub source_sequence: u64,
    /// Bounded request-only input. Never provider state or durable session content.
    pub newest_request: String,
}

/// Advisory model labels, not task, permission, authority, or completion state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCategorization {
    /// Source activation sequence.
    pub source_sequence: u64,
    /// Fixed-vocabulary task label supplied by the plugin.
    pub task: String,
    /// Fixed-vocabulary domain label supplied by the plugin.
    pub domain: String,
}

/// Absence retains baseline behavior and any existing advisory state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategorizationResponse {
    /// Optional labels only; never source text, extracted facts, or provider metadata.
    pub categorization: Option<SessionCategorization>,
}

/// Rank already eligible session cards without model work.
pub const OP_RANK_CATEGORIZATION: &str = "rank";

/// Bounded title-free caller-eligible session card; labels are advisory only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCategoryRankCandidate {
    /// Original identity; never opened or mutated by this operation.
    pub session_id: SessionId,
    /// Current activation identity against which disposable labels are checked.
    pub source_sequence: u64,
    /// Missing labels preserve baseline; stale labels are not usable matches.
    pub categorization: Option<SessionCategorization>,
}

/// Portable plugin-owned navigation input; no titles or raw requests are accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCategoryRankRequest {
    /// Independently evolving operation version; currently one.
    pub version: u32,
    /// Exact fixed-vocabulary task query, distinct from domain.
    pub task: Option<String>,
    /// Exact fixed-vocabulary domain query, distinct from task.
    pub domain: Option<String>,
    /// Complete caller-eligible bounded page in baseline order, at most 100 cards.
    pub candidates: Vec<SessionCategoryRankCandidate>,
}

/// Stable advisory ordering of every original identity, not discovery or authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCategoryRankResponse {
    /// Compatible operation version.
    pub version: u32,
    /// Full original-ID permutation; ties retain baseline order.
    pub ordered_session_ids: Vec<SessionId>,
    /// Exact matching identities in baseline order; other is not a semantic match.
    pub matched_session_ids: Vec<SessionId>,
    /// Whether the advisory ordering differs from baseline.
    pub applied: bool,
}

impl SessionCategoryRankResponse {
    /// Validate identity correlation and compatibility without interpreting labels.
    ///
    /// This does not certify label truth, stable ranking semantics, or eligibility;
    /// those remain owned by the plugin and caller's public catalog boundary.
    ///
    /// # Errors
    /// Rejects unsupported versions, oversized or duplicate input identities,
    /// non-permutations, duplicate/foreign match identities, and inconsistent applied flags.
    pub fn validate_for(&self, request: &SessionCategoryRankRequest) -> Result<(), &'static str> {
        use std::collections::BTreeSet;
        if self.version != 1 || request.version != 1 || request.candidates.len() > 100 {
            return Err("unsupported ranking version or bound");
        }
        let baseline: Vec<_> = request
            .candidates
            .iter()
            .map(|card| card.session_id)
            .collect();
        let originals: BTreeSet<_> = baseline.iter().copied().collect();
        let ordered: BTreeSet<_> = self.ordered_session_ids.iter().copied().collect();
        if originals.len() != baseline.len()
            || self.ordered_session_ids.len() != baseline.len()
            || ordered.len() != self.ordered_session_ids.len()
            || ordered != originals
        {
            return Err("ranking response is not an exact identity permutation");
        }
        let matched: BTreeSet<_> = self.matched_session_ids.iter().copied().collect();
        if matched.len() != self.matched_session_ids.len() || !matched.is_subset(&originals) {
            return Err("ranking response has duplicate or foreign matches");
        }
        if self.applied != (self.ordered_session_ids != baseline) {
            return Err("ranking response applied flag is inconsistent");
        }
        Ok(())
    }
}

#[cfg(test)]
mod rank_contract_tests {
    use super::*;

    #[test]
    fn response_correlates_exact_identities_without_guessing_labels() {
        let first = SessionId::new();
        let second = SessionId::new();
        let request = SessionCategoryRankRequest {
            version: 1,
            task: None,
            domain: None,
            candidates: [first, second]
                .into_iter()
                .map(|session_id| SessionCategoryRankCandidate {
                    session_id,
                    source_sequence: 1,
                    categorization: None,
                })
                .collect(),
        };
        let baseline = SessionCategoryRankResponse {
            version: 1,
            ordered_session_ids: vec![first, second],
            matched_session_ids: vec![],
            applied: false,
        };
        assert!(baseline.validate_for(&request).is_ok());
        let reordered = SessionCategoryRankResponse {
            ordered_session_ids: vec![second, first],
            matched_session_ids: vec![second],
            applied: true,
            version: 1,
        };
        // Contract validation does not infer whether absent labels justify a match.
        assert!(reordered.validate_for(&request).is_ok());
        for invalid in [
            SessionCategoryRankResponse {
                version: 2,
                ..baseline.clone()
            },
            SessionCategoryRankResponse {
                ordered_session_ids: vec![first, first],
                ..baseline.clone()
            },
            SessionCategoryRankResponse {
                ordered_session_ids: vec![first, SessionId::new()],
                ..baseline.clone()
            },
            SessionCategoryRankResponse {
                ordered_session_ids: vec![first],
                ..baseline.clone()
            },
            SessionCategoryRankResponse {
                matched_session_ids: vec![first, first],
                ..baseline.clone()
            },
            SessionCategoryRankResponse {
                matched_session_ids: vec![SessionId::new()],
                ..baseline.clone()
            },
            SessionCategoryRankResponse {
                applied: true,
                ..baseline.clone()
            },
            SessionCategoryRankResponse {
                applied: false,
                ..reordered
            },
        ] {
            assert!(invalid.validate_for(&request).is_err());
        }
        let mut duplicate_input = request.clone();
        duplicate_input.candidates[1].session_id = first;
        assert!(baseline.validate_for(&duplicate_input).is_err());
        let mut future = request;
        future.version = 2;
        assert!(baseline.validate_for(&future).is_err());
    }
}
