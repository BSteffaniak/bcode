//! Daemon-local view of live native work, derived only from committed canonical evidence.
//!
//! Idle shutdown must never kill an active native turn or unsettled background work. The
//! tracker is advanced after a submission receipt or an observation page has been committed,
//! so it never runs ahead of canonical storage. It is conservative: unknown or overflowing
//! evidence keeps a session busy, and only explicit native terminal or settlement evidence (or
//! an acknowledged close) makes it idle again. It grants no authority and dispatches nothing.

use bcode_harness_models::{
    HarnessActivity, HarnessCapabilities, HarnessExecutionEvent, HarnessObservation,
    HarnessObservationKind, HarnessSupportLevel,
};
use bcode_session_models::SessionId;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

/// Background identities tracked per session before the session is pinned busy.
const MAX_BACKGROUND_WORK: usize = 256;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct SessionActivity {
    foreground: bool,
    awaiting_settlement: bool,
    background: BTreeSet<String>,
    /// Background evidence overflowed; only settlement or close clears it.
    saturated: bool,
}

impl SessionActivity {
    fn busy(&self) -> bool {
        self.foreground || self.awaiting_settlement || self.saturated || !self.background.is_empty()
    }

    fn settle(&mut self) {
        *self = Self::default();
    }

    fn observe(&mut self, observation: &HarnessObservation, settlement_supported: bool) {
        match &observation.kind {
            HarnessObservationKind::SubmissionAccepted { .. }
            | HarnessObservationKind::SubmissionConsumed { .. } => self.foreground = true,
            HarnessObservationKind::Result { .. }
            | HarnessObservationKind::SubmissionResultObserved { .. }
            | HarnessObservationKind::NativeTurnTerminal { .. }
            | HarnessObservationKind::Execution {
                event: HarnessExecutionEvent::SubmissionTerminal { .. },
            } => {
                if self.foreground {
                    self.awaiting_settlement |= settlement_supported;
                }
                self.foreground = false;
            }
            HarnessObservationKind::Settlement { .. } => self.settle(),
            HarnessObservationKind::Status { status } => match status.activity {
                HarnessActivity::Running => self.foreground = true,
                HarnessActivity::Settled => self.settle(),
                HarnessActivity::Idle | HarnessActivity::Interrupted | HarnessActivity::Unknown => {
                }
            },
            HarnessObservationKind::Execution {
                event:
                    HarnessExecutionEvent::BackgroundWork { work_id, state, .. }
                    | HarnessExecutionEvent::Continuation {
                        continuation_id: work_id,
                        state,
                        ..
                    },
            } => {
                if state.is_terminal() {
                    self.background.remove(work_id);
                } else if self.background.len() < MAX_BACKGROUND_WORK {
                    self.background.insert(work_id.clone());
                } else {
                    self.saturated = true;
                }
            }
            _ => {}
        }
    }
}

/// Live native work per managed session, advanced only after canonical commits.
#[derive(Debug, Default)]
pub struct NativeActivity {
    sessions: Mutex<BTreeMap<SessionId, SessionActivity>>,
}

impl NativeActivity {
    /// Empty tracker: no live native work is known.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            sessions: Mutex::new(BTreeMap::new()),
        }
    }

    fn update(&self, session: SessionId, apply: impl FnOnce(&mut SessionActivity)) {
        let Ok(mut sessions) = self.sessions.lock() else {
            return;
        };
        // Only busy sessions are retained, so the map is bounded by live managed sessions.
        let entry = sessions.entry(session).or_default();
        apply(entry);
        if !entry.busy() {
            sessions.remove(&session);
        }
    }

    /// Record a committed native submission receipt: a foreground turn is now live.
    pub fn submitted(&self, session: SessionId) {
        self.update(session, |activity| activity.foreground = true);
    }

    /// Advance from one committed observation page.
    pub fn observed(
        &self,
        session: SessionId,
        capabilities: &HarnessCapabilities,
        observations: &[HarnessObservation],
    ) {
        if observations.is_empty() {
            return;
        }
        let settlement_supported =
            capabilities.authoritative_settlement.support == HarnessSupportLevel::Supported;
        self.update(session, |activity| {
            for observation in observations {
                activity.observe(observation, settlement_supported);
            }
        });
    }

    /// An acknowledged close released the native transport; no live work remains attached.
    pub fn closed(&self, session: SessionId) {
        self.update(session, SessionActivity::settle);
    }

    /// Number of managed sessions with live foreground or unsettled background work.
    #[must_use]
    pub fn busy_sessions(&self) -> usize {
        // A poisoned tracker cannot prove idleness.
        self.sessions.lock().map_or(1, |sessions| {
            sessions.values().filter(|activity| activity.busy()).count()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_harness_models::{HarnessCompleteness, HarnessWorkState};

    fn observation(kind: HarnessObservationKind) -> HarnessObservation {
        HarnessObservation {
            observation_id: "observation".into(),
            session_id: "session".into(),
            generation: 1,
            native_correlations: Vec::new(),
            completeness: HarnessCompleteness::Complete,
            kind,
        }
    }

    fn capabilities(settlement: HarnessSupportLevel) -> HarnessCapabilities {
        let capability = |support| bcode_harness_models::HarnessCapability {
            support,
            constraints: Vec::new(),
        };
        HarnessCapabilities {
            resume: capability(HarnessSupportLevel::Unsupported),
            steering: capability(HarnessSupportLevel::Unsupported),
            approval: capability(HarnessSupportLevel::Unsupported),
            questions: capability(HarnessSupportLevel::Unsupported),
            assistance: capability(HarnessSupportLevel::Unsupported),
            interrupt: capability(HarnessSupportLevel::Unsupported),
            authoritative_settlement: capability(settlement),
            background_continuation: capability(HarnessSupportLevel::Unsupported),
            authentication: capability(HarnessSupportLevel::Unsupported),
            model_switch: capability(HarnessSupportLevel::Unsupported),
            permission_mode: capability(HarnessSupportLevel::Unsupported),
        }
    }

    fn background(state: HarnessWorkState) -> HarnessObservation {
        observation(HarnessObservationKind::Execution {
            event: HarnessExecutionEvent::BackgroundWork {
                work_id: "task".into(),
                native_task_id: None,
                origin: None,
                parent_work_id: None,
                work_kind: bcode_harness_models::HarnessWorkKind::Command,
                label: "task".into(),
                state,
                notification: bcode_harness_models::HarnessNotificationState::Unknown,
            },
        })
    }

    #[test]
    fn foreground_result_ends_turn_without_authoritative_settlement() {
        let activity = NativeActivity::default();
        let session = SessionId::new();
        let caps = capabilities(HarnessSupportLevel::Constrained);
        activity.submitted(session);
        assert_eq!(activity.busy_sessions(), 1);
        activity.observed(
            session,
            &caps,
            &[observation(HarnessObservationKind::Text {
                text: "partial".into(),
                segment_id: None,
            })],
        );
        assert_eq!(
            activity.busy_sessions(),
            1,
            "display text is not completion"
        );
        activity.observed(
            session,
            &caps,
            &[observation(HarnessObservationKind::Result {
                text: "done".into(),
            })],
        );
        assert_eq!(activity.busy_sessions(), 0);
    }

    #[test]
    fn background_work_and_supported_settlement_keep_session_busy_until_settled() {
        let activity = NativeActivity::default();
        let session = SessionId::new();
        let caps = capabilities(HarnessSupportLevel::Supported);
        activity.submitted(session);
        activity.observed(
            session,
            &caps,
            &[
                background(HarnessWorkState::Running),
                observation(HarnessObservationKind::Result {
                    text: "done".into(),
                }),
            ],
        );
        assert_eq!(activity.busy_sessions(), 1, "background work is unsettled");
        activity.observed(session, &caps, &[background(HarnessWorkState::Completed)]);
        assert_eq!(activity.busy_sessions(), 1, "settlement is still awaited");
        activity.observed(
            session,
            &caps,
            &[observation(HarnessObservationKind::Settlement {
                native_turn_id: "turn".into(),
            })],
        );
        assert_eq!(activity.busy_sessions(), 0);
    }

    #[test]
    fn close_clears_all_live_work() {
        let activity = NativeActivity::default();
        let session = SessionId::new();
        activity.submitted(session);
        activity.closed(session);
        assert_eq!(activity.busy_sessions(), 0);
    }
}
