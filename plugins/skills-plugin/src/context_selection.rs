//! Ranking and byte-budget admission for real request-only evidence contributions.
//!
//! Producers own provenance and eligibility. This service neither retrieves evidence nor
//! edits chronological messages. The host retains and incorporates original admitted snippets.
use bcode_model::judgement::OptionalResponse;
use serde::{Deserialize, Serialize};

use crate::selection;

/// Independently compatible evidence selection service.
pub const INTERFACE_ID: &str = "bcode.context-selection/v1";

pub use bcode_session_models::{
    JudgementContextCandidate as Candidate, JudgementContextSource as Source,
};

/// Portable application-owned envelope shared by all producers.
pub use bcode_session_models::{
    ContextSelectionRequest as SelectionRequest, ContextSelectionResponse as SelectionResult,
};

/// Apply central access advice; unavailable/error/off outcomes use `response: None`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyRequest {
    /// Exactly the request passed to prepare.
    pub selection: SelectionRequest,
    /// Centrally resolved advice; this plugin has no authentication/network/retry path.
    pub judgement: OptionalResponse,
}

fn ranking(input: &SelectionRequest) -> selection::SelectionRequest {
    selection::SelectionRequest {
        purpose: selection::Purpose::Context,
        task: input.task.clone(),
        candidates: input
            .candidates
            .iter()
            .map(|candidate| selection::Candidate {
                id: candidate.id.clone(),
                description: candidate.snippet.clone(),
                protected: candidate.protected,
            })
            .collect(),
    }
}

fn valid(input: &SelectionRequest) -> bool {
    let mut ids = std::collections::BTreeSet::new();
    input.candidates.len() <= selection::MAX_CANDIDATES
        && input.task.len() <= 8192
        && input
            .candidates
            .iter()
            .all(|candidate| candidate.is_valid() && ids.insert(&candidate.id))
}

/// Prepare relevance advice only under real optional byte-budget pressure.
#[must_use]
pub fn prepare(input: &SelectionRequest) -> selection::PreparedSelection {
    let bytes = input
        .candidates
        .iter()
        .filter(|candidate| !candidate.protected)
        .fold(0usize, |sum, candidate| {
            sum.saturating_add(candidate.snippet.len())
        });
    if !valid(input)
        || input
            .budget_bytes
            .is_some_and(|budget| budget == 0 || bytes <= budget)
    {
        return selection::PreparedSelection { judgement: None };
    }
    selection::prepare(&ranking(input))
}

/// Retain baseline on unavailable/malformed advice; admit whole snippets without truncation.
/// Invalid producer envelopes admit nothing: the host must retain its original baseline.
#[must_use]
pub fn apply(input: &ApplyRequest) -> SelectionResult {
    if !valid(&input.selection) {
        return SelectionResult {
            ordered_ids: input
                .selection
                .candidates
                .iter()
                .map(|c| c.id.clone())
                .collect(),
            included_ids: Vec::new(),
            applied: false,
        };
    }
    let advice = if prepare(&input.selection).judgement.is_some() {
        input.judgement.clone()
    } else {
        OptionalResponse {
            observation: None,
            response: None,
        }
    };
    let order = selection::apply(&selection::ApplyRequest {
        selection: ranking(&input.selection),
        judgement: advice,
    });
    let candidates: std::collections::BTreeMap<_, _> = input
        .selection
        .candidates
        .iter()
        .map(|candidate| (candidate.id.as_str(), candidate))
        .collect();
    let mut remaining = input.selection.budget_bytes.unwrap_or(usize::MAX);
    let included_ids = order
        .ordered_ids
        .iter()
        .filter_map(|id| {
            let candidate = candidates.get(id.as_str())?;
            if candidate.protected {
                return Some(id.clone());
            }
            if candidate.snippet.len() > remaining {
                return None;
            }
            remaining -= candidate.snippet.len();
            Some(id.clone())
        })
        .collect();
    SelectionResult {
        ordered_ids: order.ordered_ids,
        included_ids,
        applied: order.applied,
    }
}

#[cfg(test)]
mod tests;
