//! Disposable, content-free progress observations. No reads acquire execution authority.
//!
//! The coordinator supplies recognized substantive model/tool/stage signals. Polls,
//! heartbeats, status observations, retries and duplicate delivery must not call
//! `observe`. Sequenced methods suppress duplicates within their retained scope.
//! A retained marker does not certify complete live-hook coverage.

use bcode_session_models::SessionId;
use bcode_workflow_view_models::{
    SESSION_PROGRESS_VERSION, SessionProgressObservation, SessionProgressSnapshot,
    SessionProgressUnavailableReason,
};
use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroUsize;

#[derive(Debug)]
struct Progress {
    epoch: String,
    counter: u64,
    last_progress_at_ms: Option<u64>,
    event_sequence: Option<u64>,
    tools: BTreeMap<String, u64>,
    tool_order: VecDeque<String>,
    dedup_complete: bool,
}

/// Bounded session-scoped observations, reusable by standalone agents and workflows.
///
/// The caller owns synchronization and chooses the retention budget. FIFO eviction
/// discards observation continuity, not session state. Snapshot reads neither insert
/// entries nor refresh retention. Missing and evicted entries are both unavailable;
/// there is no unbounded tombstone inventory or implicit idle baseline.
#[derive(Debug)]
pub struct SessionProgressRegistry {
    capacity: NonZeroUsize,
    tool_capacity: NonZeroUsize,
    sessions: BTreeMap<SessionId, Progress>,
    insertion_order: VecDeque<SessionId>,
}

impl SessionProgressRegistry {
    /// Create an empty registry with an explicit, positive retention budget.
    #[must_use]
    pub const fn new(capacity: NonZeroUsize) -> Self {
        Self::with_tool_capacity(
            capacity,
            NonZeroUsize::new(64).expect("positive tool budget"),
        )
    }

    /// Create a registry with independent positive session and per-session tool budgets.
    ///
    /// These are retention budgets, not limits on workflow size or lifetime. Tool
    /// identities are bounded to 256 UTF-8 bytes; invalid identities fail closed.
    #[must_use]
    pub const fn with_tool_capacity(capacity: NonZeroUsize, tool_capacity: NonZeroUsize) -> Self {
        Self {
            capacity,
            tool_capacity,
            sessions: BTreeMap::new(),
            insertion_order: VecDeque::new(),
        }
    }

    /// Record one host-verified substantive progress signal, without storing content.
    ///
    /// Call only after domain-owned classification and duplicate suppression. The
    /// first signal establishes a baseline, not proof of progress since an unknown
    /// observation. Overflow starts a new epoch rather than wrapping the counter.
    pub fn observe(&mut self, session_id: SessionId) {
        let last_progress_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|duration| u64::try_from(duration.as_millis()).ok());
        if let Some(progress) = self.sessions.get_mut(&session_id) {
            progress.last_progress_at_ms = last_progress_at_ms;
            if let Some(counter) = progress.counter.checked_add(1) {
                progress.counter = counter;
            } else {
                progress.epoch = uuid::Uuid::new_v4().to_string();
                progress.counter = 1;
            }
            return;
        }
        if self.sessions.len() >= self.capacity.get()
            && let Some(evicted) = self.insertion_order.pop_front()
        {
            self.sessions.remove(&evicted);
        }
        self.sessions.insert(
            session_id,
            Progress {
                epoch: uuid::Uuid::new_v4().to_string(),
                counter: 1,
                last_progress_at_ms,
                event_sequence: None,
                tools: BTreeMap::new(),
                tool_order: VecDeque::new(),
                dedup_complete: true,
            },
        );
        self.insertion_order.push_back(session_id);
    }

    /// Record a classified canonical stage event only once per increasing session sequence.
    ///
    /// The caller accepts only substantive model-start/model-finish/tool-result events.
    /// Gaps between accepted sequences are expected because other canonical events
    /// are not progress. Same/lower sequences never advance the marker.
    pub fn observe_event(&mut self, session_id: SessionId, sequence: u64) {
        if self.sessions.get(&session_id).is_some_and(|progress| {
            progress
                .event_sequence
                .is_some_and(|previous| sequence <= previous)
        }) {
            return;
        }
        self.observe(session_id);
        if let Some(progress) = self.sessions.get_mut(&session_id) {
            progress.event_sequence = Some(sequence);
        }
    }

    /// Record one accepted nonempty normalized tool lifecycle `Progress` signal.
    ///
    /// The coordinator verifies the lifecycle stage and nonempty progress before
    /// calling this method; this registry never examines content or metadata. The
    /// producer sequence must strictly increase for the exact invocation identity.
    /// Duplicate/lower sequences are ignored. FIFO tool eviction invalidates the
    /// epoch and leaves coverage unavailable for this retained session, since an
    /// evicted invocation cannot be deduplicated safely. Plain model/stage progress
    /// cannot restore that lost coverage.
    pub fn observe_tool_progress(
        &mut self,
        session_id: SessionId,
        invocation_id: &str,
        producer_sequence: u64,
    ) {
        if invocation_id.is_empty() || invocation_id.len() > 256 {
            if !self.sessions.contains_key(&session_id) {
                self.observe(session_id);
            }
            if let Some(progress) = self.sessions.get_mut(&session_id) {
                progress.epoch = uuid::Uuid::new_v4().to_string();
                progress.dedup_complete = false;
            }
            return;
        }
        if let Some(progress) = self.sessions.get(&session_id)
            && (!progress.dedup_complete
                || progress
                    .tools
                    .get(invocation_id)
                    .is_some_and(|previous| producer_sequence <= *previous))
        {
            return;
        }
        self.observe(session_id);
        if let Some(progress) = self.sessions.get_mut(&session_id) {
            if !progress.tools.contains_key(invocation_id) {
                if progress.tools.len() >= self.tool_capacity.get() {
                    if let Some(evicted) = progress.tool_order.pop_front() {
                        progress.tools.remove(&evicted);
                    }
                    progress.epoch = uuid::Uuid::new_v4().to_string();
                    progress.dedup_complete = false;
                }
                progress.tool_order.push_back(invocation_id.to_owned());
            }
            progress
                .tools
                .insert(invocation_id.to_owned(), producer_sequence);
        }
    }

    /// Read one own-session marker without recording progress or touching retention.
    ///
    /// The application must additionally fail closed when live-hook coverage is
    /// unavailable (for example, a foreign/nonresident owner or subscription gap).
    /// This method never certifies subtree discovery or execution ownership.
    #[must_use]
    pub fn snapshot(&self, session_id: SessionId) -> SessionProgressSnapshot {
        SessionProgressSnapshot {
            version: SESSION_PROGRESS_VERSION,
            session_id: session_id.to_string(),
            observation: self.sessions.get(&session_id).map_or(
                SessionProgressObservation::Unavailable {
                    reason: SessionProgressUnavailableReason::NotObservedOrEvicted,
                },
                |progress| {
                    if progress.dedup_complete {
                        SessionProgressObservation::Available {
                            epoch: progress.epoch.clone(),
                            counter: progress.counter,
                            last_progress_at_ms: progress.last_progress_at_ms,
                        }
                    } else {
                        SessionProgressObservation::Unavailable {
                            reason: SessionProgressUnavailableReason::CoverageUnavailable,
                        }
                    }
                },
            ),
        }
    }
}

#[cfg(test)]
#[path = "activity_tests.rs"]
mod tests;
