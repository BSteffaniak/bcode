//! Fail-closed applicability and page validation for retained workflow session captures.
use bcode_session_models::{CURRENT_SESSION_EVENT_SCHEMA_VERSION, SessionEvent, SessionEventKind};
use serde_json::Value;

pub fn first_or_fetch<E>(
    cached: &mut Option<Value>,
    fetch: impl FnOnce() -> Result<Value, E>,
) -> Result<Value, E> {
    cached.take().map_or_else(fetch, Ok)
}

pub struct HistoryPage {
    pub events: Vec<SessionEvent>,
    /// First sequence after this verified prefix, including at read-time EOF.
    pub tail: u64,
    pub next: Option<u64>,
}

pub fn continuation(page: &HistoryPage, cached_prefix: bool) -> Option<u64> {
    page.next.or_else(|| cached_prefix.then_some(page.tail))
}

pub fn history_page(value: &Value, identity: &str, after: u64) -> Option<HistoryPage> {
    if value["session_id"].as_str() != Some(identity)
        || !value["compatibility_issues"].as_array()?.is_empty()
    {
        return None;
    }
    let events: Vec<SessionEvent> = serde_json::from_value(value["events"].clone()).ok()?;
    if events.len() > 100 {
        return None;
    }
    let mut expected = after;
    for event in &events {
        if event.session_id.to_string() != identity
            || event.schema_version != CURRENT_SESSION_EVENT_SCHEMA_VERSION
            || event.sequence != expected
        {
            return None;
        }
        expected = expected.checked_add(1)?;
    }
    let more = value["has_more"].as_bool()?;
    let next = if more {
        if events.is_empty()
            || value
                .pointer("/next_cursor/sequence")
                .and_then(Value::as_u64)
                != Some(expected)
        {
            return None;
        }
        Some(expected)
    } else {
        if !value["next_cursor"].is_null() {
            return None;
        }
        None
    };
    Some(HistoryPage {
        events,
        tail: expected,
        next,
    })
}

/// Finite, secret-safe reasons why retained model evidence is incomplete.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ModelCoverageFailure {
    MissingRequestBoundary,
    IncompleteHistory,
    MissingOrUnverifiedModelInputExport,
    UnknownModelApplicability,
    NoObservedModelSession,
}

#[derive(Default)]
pub struct ModelCoverage {
    pub applicable_sessions: usize,
    pub complete_sessions: usize,
    pub non_model_sessions: usize,
    pub unknown_sessions: usize,
    pub missing_boundaries: usize,
    failures: std::collections::BTreeSet<ModelCoverageFailure>,
}

impl ModelCoverage {
    pub fn observe(
        &mut self,
        events: &[SessionEvent],
        summary: &Value,
        unknown_child: bool,
        history_complete: bool,
    ) {
        let requests = summary["model_requests_observed"].as_u64().unwrap_or(0);
        let execution = events.iter().any(|event| {
            matches!(
                event.kind,
                SessionEventKind::ModelTurnStarted { .. }
                    | SessionEventKind::RequestContextObserved { .. }
            )
        });
        let execution_identity = events
            .iter()
            .any(|event| matches!(event.kind, SessionEventKind::ExecutionSessionCreated { .. }));
        let parent_observed = events
            .iter()
            .any(|event| matches!(event.kind, SessionEventKind::SessionCreated { .. }));
        if requests > 0 || execution {
            self.applicable_sessions += 1;
            if !history_complete {
                self.failures
                    .insert(ModelCoverageFailure::IncompleteHistory);
            }
            if requests == 0 {
                self.missing_boundaries += 1;
                self.failures
                    .insert(ModelCoverageFailure::MissingRequestBoundary);
            } else if history_complete && summary["complete_model_inputs_exported"] == true {
                self.complete_sessions += 1;
            } else if summary["complete_model_inputs_exported"] != true {
                self.failures
                    .insert(ModelCoverageFailure::MissingOrUnverifiedModelInputExport);
            }
        } else if unknown_child || execution_identity || !history_complete || !parent_observed {
            self.unknown_sessions += 1;
            self.failures
                .insert(ModelCoverageFailure::UnknownModelApplicability);
        } else {
            self.non_model_sessions += 1;
        }
    }

    /// Return bounded reason categories, never request content or provider diagnostics.
    #[must_use]
    pub fn failure_reasons(&self) -> Vec<ModelCoverageFailure> {
        let mut reasons = self.failures.clone();
        if self.applicable_sessions == 0 {
            reasons.insert(ModelCoverageFailure::NoObservedModelSession);
        }
        reasons.into_iter().collect()
    }

    #[must_use]
    pub const fn complete(&self) -> bool {
        self.unknown_sessions == 0
            && self.applicable_sessions > 0
            && self.complete_sessions == self.applicable_sessions
    }
}

mod committed;
pub use committed::capture as capture_committed;
mod inventory;
pub use inventory::capture as capture_inventory;

#[cfg(test)]
mod tests;
