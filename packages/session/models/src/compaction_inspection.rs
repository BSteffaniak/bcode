//! Secret-safe, versioned compaction observations and bounded inspection contracts.
//!
//! Observations are diagnostic evidence, never execution or resume authority. The host emits
//! them only after validating its source manifest, checkpoint and cumulative allowances.
//! Existing unstructured trace messages and opaque checkpoint payloads are not progress counters.

use crate::{SessionHistoryCursor, SessionId};
use serde::{Deserialize, Serialize};

/// Reserved provider-event type carrying a JSON [`CompactionProgressObservation`].
/// This adds an optional trace convention, not a new canonical event schema or writer epoch.
pub const COMPACTION_PROGRESS_OBSERVATION_EVENT: &str = "compaction_progress_v1";
/// Maximum encoded observation size. Unknown versions fail closed without echoing detail.
pub const MAX_COMPACTION_PROGRESS_OBSERVATION_BYTES: usize = 4096;
/// Maximum canonical candidates examined by one inspection call, unrelated to history size.
pub const MAX_COMPACTION_INSPECTION_CANDIDATES: usize = 200;

/// Historical evidence phase; this is not a claim that a worker is currently running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionInspectionPhase {
    Started,
    Synthesizing,
    Finished,
    Skipped,
    Blocked,
    /// A valid session envelope exists; its host payload has NOT been validated here.
    CheckpointRetained,
    CheckpointRetired,
    Committed,
}

/// Host-validated cumulative count. `done <= total`; zero total is valid for empty work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionInspectionCount {
    pub done: u64,
    pub total: u64,
}

/// Diagnostic resume evidence, not permission to restore or dispatch work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionInspectionResume {
    NotReported,
    /// Host verified the retained checkpoint against the selected source and policy.
    ValidatedCheckpoint,
    /// Host explicitly reported starting over; absence of a checkpoint does not imply this.
    Restarted,
}

/// Only allowances for which the host has trustworthy durable cumulative evidence.
/// `None` means unavailable, never zero or an unlimited allowance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionInspectionBudgets {
    pub provider_requests: Option<CompactionInspectionCount>,
    pub source_events: Option<CompactionInspectionCount>,
    pub decoded_bytes: Option<CompactionInspectionCount>,
}

/// Closed, secret-safe blocker vocabulary. No provider errors, source text or keys are returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionInspectionBlocker {
    UnsupportedFormat,
    InvalidEvidence,
    /// Existing lifecycle messages have no structured validated progress.
    ProgressNotRecorded,
    /// An envelope alone cannot establish host payload compatibility or resumability.
    CheckpointNotValidated,
    SourceChanged,
    BudgetExhausted,
    /// Inspection byte/structural allowance, not a production compaction budget.
    InspectionBudgetExceeded,
    Cancelled,
    OwnershipUnverified,
    MissingSource,
}

/// Version-1 persisted/public observation.
///
/// New incompatible formats require a new version.
/// Unknown fields/variants/versions are rejected, not interpreted as an older representation.
///
/// No operation key, summary, manifest, arbitrary message or opaque payload is permitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionProgressObservation {
    pub version: u32,
    pub phase: CompactionInspectionPhase,
    pub chunks: Option<CompactionInspectionCount>,
    pub resume: CompactionInspectionResume,
    pub budgets: Option<CompactionInspectionBudgets>,
    pub blocker: Option<CompactionInspectionBlocker>,
}

impl CompactionProgressObservation {
    /// Check compatibility and count consistency without interpreting host checkpoint payloads.
    ///
    /// # Errors
    /// Returns a content-free blocker for unknown versions or inconsistent counts.
    pub fn validate(&self) -> Result<(), CompactionInspectionBlocker> {
        if self.version != 1 {
            return Err(CompactionInspectionBlocker::UnsupportedFormat);
        }
        let mut counts = vec![self.chunks];
        if let Some(budgets) = &self.budgets {
            counts.extend([
                budgets.provider_requests,
                budgets.source_events,
                budgets.decoded_bytes,
            ]);
        }
        if counts
            .into_iter()
            .flatten()
            .any(|count| count.done > count.total)
        {
            return Err(CompactionInspectionBlocker::InvalidEvidence);
        }
        Ok(())
    }

    /// Encode a validated observation as canonical diagnostic evidence, not resume authority.
    /// The server appends this through its normal authorized session publication path.
    ///
    /// # Errors
    /// Rejects incompatible, inconsistent or oversized observations without exposing contents.
    pub fn into_event_kind(
        self,
        timestamp_ms: u64,
    ) -> Result<crate::SessionEventKind, CompactionInspectionBlocker> {
        let detail = self.encode()?;
        Ok(crate::SessionEventKind::TraceEvent {
            trace: Box::new(crate::SessionTraceEvent {
                timestamp_ms,
                turn_id: None,
                phase: crate::SessionTracePhase::ModelProviderEvent,
                payload: crate::SessionTracePayload::ProviderEvent {
                    event_type: COMPACTION_PROGRESS_OBSERVATION_EVENT.into(),
                    detail: Some(detail),
                },
            }),
        })
    }

    /// Encode a bounded observation for the reserved provider-event type.
    ///
    /// # Errors
    /// Rejects incompatible, inconsistent or oversized evidence without exposing its contents.
    pub fn encode(&self) -> Result<String, CompactionInspectionBlocker> {
        self.validate()?;
        let detail = serde_json::to_string(self)
            .map_err(|_| CompactionInspectionBlocker::InvalidEvidence)?;
        if detail.len() > MAX_COMPACTION_PROGRESS_OBSERVATION_BYTES {
            return Err(CompactionInspectionBlocker::InvalidEvidence);
        }
        Ok(detail)
    }
}

/// Latest-first indexed candidate scan. Cursor is inclusive, as for session history.
/// Consumers follow `next_cursor` unchanged; concurrent arrivals require a fresh scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCompactionInspectionQuery {
    pub before: Option<SessionHistoryCursor>,
    /// Candidate allowance, 1..=200. This is not a matching-result count.
    /// Ephemeral sessions count every examined event, including unrelated history, to bound work.
    pub limit: usize,
}

/// Normalized historical evidence; never a raw canonical event or checkpoint payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCompactionInspectionEntry {
    pub sequence: u64,
    /// Absent for invalid/unsupported evidence; do not infer a phase from its message.
    pub phase: Option<CompactionInspectionPhase>,
    pub progress: Option<CompactionProgressObservation>,
    pub blocker: Option<CompactionInspectionBlocker>,
}

/// Bounded historical evidence page, not replayable execution state.
///
/// A terminal entry stays historical and cannot be reopened by a subsequent diagnostic entry.
///
/// Empty entries mean missing evidence ONLY when `has_more` is false and `degraded` is false.
/// `has_more` means the scan is incomplete, including when all candidates were unrelated traces.
///
/// Old producers expose lifecycle/checkpoint evidence but not fabricated counters or restart state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCompactionInspectionPage {
    /// Public contract version; consumers must reject versions other than 1.
    pub version: u32,
    pub session_id: SessionId,
    pub entries: Vec<SessionCompactionInspectionEntry>,
    pub scanned_events: usize,
    /// Set when canonical evidence cannot be completely validated within the inspection bounds.
    /// This includes unsupported/invalid evidence and inspection byte/structural exhaustion.
    pub degraded: bool,
    pub next_cursor: Option<SessionHistoryCursor>,
    pub has_more: bool,
}
