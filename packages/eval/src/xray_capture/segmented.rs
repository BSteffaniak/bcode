//! Eval-owned evidence reduction. Session/watch lifecycle belongs to application
//! clients; this module never attaches, reattaches, dispatches or cancels execution.
use super::EvalXrayEvidence;
use serde::Serialize;
use std::collections::BTreeMap;

/// Bounded eval receipt retention, not a limit on workflow size or lifetime.
#[derive(Clone, Copy, Debug)]
pub struct CaptureLimits {
    /// Maximum distinct canonical run/session links retained.
    pub sessions: usize,
    /// Maximum distinct observation receipts retained.
    pub segments: usize,
}

impl Default for CaptureLimits {
    fn default() -> Self {
        Self {
            sessions: 256,
            segments: 1024,
        }
    }
}

/// Read-only adapter over the generic application's pre-dispatch readiness DTO.
///
/// Implement for D's portable DTO, not a server registry or canonical store handle.
/// Methods must expose authenticated receipt facts without deriving them from
/// envelope timestamps, descendant discovery, task success or a local reattach.
pub trait ExecutionReadinessReceipt {
    /// Independently checked portable receipt version.
    fn schema_version(&self) -> u16;
    /// Exact authorized ephemeral watch/segment identity.
    fn watch_id(&self) -> &str;
    /// Canonical owning run supplied by the pre-dispatch linkage hook.
    fn run_id(&self) -> &str;
    /// Exact linked session. A shared session can belong to multiple run links.
    fn session_id(&self) -> &str;
    /// Exact pre-dispatch engine observation; later reattach is not a substitute.
    fn observation_id(&self) -> &str;
    /// Owner hook acknowledged observer readiness before execution dispatch.
    fn ready_before_dispatch(&self) -> bool;
    /// True for a fresh gapful watch after prior execution.
    fn prior_execution_gap(&self) -> bool;
}

/// One observation's local export result. Sequences never span observations.
#[derive(Debug, Serialize)]
pub struct EvalXraySegment {
    /// Exact authorized watch identity, not a durable replay cursor.
    pub watch_id: String,
    /// Exact engine observation identity.
    pub observation_id: String,
    /// Readiness acknowledged by the canonical owner before dispatch.
    pub ready_before_submission: bool,
    /// Explicit gap in observation scope before this segment.
    pub restart_gap_before: bool,
    /// Consumer supplied a terminal export result, including explicit failure.
    pub export_finished: bool,
    /// Consumer-provided delivery/export receipt; absent means failed capture.
    pub evidence: Option<EvalXrayEvidence>,
}

/// Canonical run/session link, including missing readiness and missing exports.
#[derive(Debug, Serialize)]
pub struct EvalXraySessionEvidence {
    /// Canonical owning run, not inferred from parent event text.
    pub run_id: String,
    /// Exact linked session, never used as a filesystem component.
    pub session_id: String,
    /// At least one exact observation was ready before owner dispatch.
    pub ready_before_submission: bool,
    /// Distinct observation receipts. Shared sessions retain distinct run links.
    pub segments: Vec<EvalXraySegment>,
}

/// Aggregate sufficiency is independent of task scoring, canonical usage and exit.
#[derive(Debug, Serialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct EvalXrayAggregate {
    /// Eval compatibility boundary.
    pub schema_version: u16,
    /// Watch admission acknowledged before root admission/submission.
    pub watch_ready: bool,
    /// Explicit server cutoff/drain barrier received, never EOF or root terminal.
    pub watch_drained: bool,
    /// Discovery had no omitted links, contention loss, restart or protocol failure.
    pub discovery_complete: bool,
    /// Observation authorization was revoked.
    pub revoked: bool,
    /// Number of distinct expected sessions (not run/session links).
    pub expected_sessions: usize,
    /// Number of distinct sessions with a delivered observation receipt.
    pub observed_sessions: usize,
    /// Expected canonical run/session links, including shared-session reuse.
    pub expected_links: usize,
    /// True only when all expected links and observation receipts are sufficient.
    pub evidence_complete: bool,
    /// Exact run -> session -> evidence mapping. No separator-based identity guesses.
    pub sessions: BTreeMap<String, BTreeMap<String, EvalXraySessionEvidence>>,
    /// Fixed secret-safe categories, never provider or transport error text.
    pub issues: Vec<&'static str>,
}

/// Receipt-only reducer.
///
/// E owns segmented session clients; D owns pre-dispatch
/// watch admission; the coordinator owns polling, recording, draining and wiring.
/// This type has no application client, thread, queue or execution handle.
pub struct EvalXrayAggregation {
    limits: CaptureLimits,
    segments: usize,
    links: usize,
    watches: BTreeMap<String, bool>,
    receipt: EvalXrayAggregate,
}

impl EvalXrayAggregation {
    /// Initialize before source/root admission. Empty expected scope cannot pass.
    #[must_use]
    pub const fn new(limits: CaptureLimits) -> Self {
        Self {
            limits,
            segments: 0,
            links: 0,
            watches: BTreeMap::new(),
            receipt: EvalXrayAggregate {
                schema_version: 1,
                watch_ready: false,
                watch_drained: false,
                discovery_complete: true,
                revoked: false,
                expected_sessions: 0,
                observed_sessions: 0,
                expected_links: 0,
                evidence_complete: false,
                sessions: BTreeMap::new(),
                issues: Vec::new(),
            },
        }
    }

    fn issue(&mut self, category: &'static str) {
        self.receipt.discovery_complete = false;
        if !self.receipt.issues.contains(&category) {
            self.receipt.issues.push(category);
        }
    }

    /// Supply the exact readiness acknowledgement returned before root dispatch.
    /// A restart requires a new watch identity and remains explicitly gapful.
    pub fn watch_ready(&mut self, watch_id: &str, prior_execution_gap: bool) {
        if !valid_id(watch_id) || self.watches.len() >= self.limits.segments {
            self.issue("watch_identity_or_budget_invalid");
            return;
        }
        if self.watches.contains_key(watch_id) {
            self.issue("duplicate_watch_readiness");
            return;
        }
        self.watches.insert(watch_id.into(), false);
        self.receipt.watch_ready = true;
        if prior_execution_gap {
            self.issue("prior_execution_gap");
        }
    }

    /// Add a canonical expected link from D's scope inventory, even when attach
    /// failed. Never omit failed links from the denominator. Returns false if the
    /// identity/budget cannot be retained; aggregate discovery then fails closed.
    pub fn expect_session(&mut self, run_id: &str, session_id: &str) -> bool {
        if !valid_id(run_id) || !valid_id(session_id) {
            self.issue("expected_identity_invalid");
            return false;
        }
        if self
            .receipt
            .sessions
            .get(run_id)
            .is_some_and(|sessions| sessions.contains_key(session_id))
        {
            return true;
        }
        if self.links >= self.limits.sessions {
            self.issue("expected_link_budget_exhausted");
            return false;
        }
        self.links += 1;
        self.receipt
            .sessions
            .entry(run_id.into())
            .or_default()
            .insert(
                session_id.into(),
                EvalXraySessionEvidence {
                    run_id: run_id.into(),
                    session_id: session_id.into(),
                    ready_before_submission: false,
                    segments: Vec::new(),
                },
            );
        true
    }

    /// Reduce D's exact owner-hook receipt. This is sufficient for pre-dispatch
    /// readiness even when eval receives it later: arrival time is not the barrier.
    /// Unsupported/mismatched receipts fail closed and never initiate capture.
    pub fn readiness(&mut self, receipt: &impl ExecutionReadinessReceipt) {
        if !self.expect_session(receipt.run_id(), receipt.session_id()) {
            return;
        }
        if receipt.schema_version() != 1
            || !valid_id(receipt.observation_id())
            || !self.watches.contains_key(receipt.watch_id())
        {
            self.issue("unsupported_or_foreign_readiness");
            return;
        }
        if self.watches.get(receipt.watch_id()) == Some(&true) {
            self.issue("readiness_after_watch_drain");
            return;
        }
        let Some(state) = self
            .receipt
            .sessions
            .get_mut(receipt.run_id())
            .and_then(|sessions| sessions.get_mut(receipt.session_id()))
        else {
            self.issue("expected_link_unavailable");
            return;
        };
        if let Some(existing) = state.segments.iter().find(|segment| {
            segment.watch_id == receipt.watch_id()
                && segment.observation_id == receipt.observation_id()
        }) {
            if existing.ready_before_submission != receipt.ready_before_dispatch()
                || existing.restart_gap_before != receipt.prior_execution_gap()
            {
                self.issue("conflicting_readiness_receipt");
            }
            return;
        }
        if self.segments >= self.limits.segments {
            self.issue("segment_budget_exhausted");
            return;
        }
        self.segments += 1;
        state.ready_before_submission |= receipt.ready_before_dispatch();
        state.segments.push(EvalXraySegment {
            watch_id: receipt.watch_id().into(),
            observation_id: receipt.observation_id().into(),
            ready_before_submission: receipt.ready_before_dispatch(),
            restart_gap_before: receipt.prior_execution_gap(),
            export_finished: false,
            evidence: None,
        });
        if receipt.prior_execution_gap() {
            self.issue("prior_execution_gap");
        }
    }

    /// Record the actual export outcome for the *same* observation D admitted.
    /// E's fresh reattach observation cannot certify D's earlier dispatch capture.
    /// `None` is explicit recording failure and stays in the retained expected set.
    pub fn segment_finished(
        &mut self,
        run_id: &str,
        session_id: &str,
        watch_id: &str,
        observation_id: &str,
        evidence: Option<EvalXrayEvidence>,
    ) {
        let segment = self
            .receipt
            .sessions
            .get_mut(run_id)
            .and_then(|sessions| sessions.get_mut(session_id))
            .and_then(|state| {
                state.segments.iter_mut().find(|segment| {
                    segment.watch_id == watch_id && segment.observation_id == observation_id
                })
            });
        let Some(segment) = segment else {
            self.issue("export_without_exact_readiness");
            return;
        };
        if segment.export_finished {
            self.issue("duplicate_segment_export");
            return;
        }
        if evidence.as_ref().is_some_and(|evidence| {
            evidence.session_id != session_id
                || evidence
                    .finalization
                    .as_ref()
                    .is_some_and(|cutoff| cutoff.observation_id != observation_id)
        }) {
            self.issue("foreign_segment_export");
            return;
        }
        segment.export_finished = true;
        segment.evidence = evidence;
    }

    /// Reduce D's explicit counters. Any observed loss permanently prevents a
    /// completeness claim, even if a later page has zero counters or task success.
    pub fn watch_loss(&mut self, omitted_links: u64, omitted_events: u64, contention_gaps: u64) {
        if omitted_links > 0 {
            self.issue("watch_omitted_links");
        }
        if omitted_events > 0 {
            self.issue("watch_omitted_events");
        }
        if contention_gaps > 0 {
            self.issue("watch_contention_gaps");
        }
    }

    /// Missing page, unsupported DTO, EOF, ownership handoff or deliberate restart.
    /// No durable replay is attempted. A later ready segment cannot repair the gap.
    pub fn gap(&mut self) {
        self.issue("watch_or_restart_gap");
    }

    /// Authorization withdrawal is an evidence outcome, not execution cancellation.
    pub fn revoked(&mut self) {
        self.receipt.revoked = true;
        self.issue("capture_revoked");
    }

    /// Accept only the server's explicit drained cutoff after all readiness,
    /// inventory, loss and envelope pages through that cutoff were consumed.
    pub fn watch_drained(&mut self, watch_id: &str) {
        if let Some(drained) = self.watches.get_mut(watch_id) {
            *drained = true;
        } else {
            self.issue("unknown_watch_drain");
        }
    }

    /// Finish reduction after the coordinator drains all capture layers. This does
    /// not write artifacts, release observers, grade tasks or claim execution ended.
    #[must_use]
    pub fn finish(mut self) -> EvalXrayAggregate {
        use std::collections::BTreeSet;
        let mut expected = BTreeSet::new();
        let mut observed = BTreeSet::new();
        for sessions in self.receipt.sessions.values() {
            for (session_id, state) in sessions {
                expected.insert(session_id);
                if state.segments.iter().any(|segment| {
                    segment
                        .evidence
                        .as_ref()
                        .is_some_and(|evidence| evidence.envelopes > 0)
                }) {
                    observed.insert(session_id);
                }
            }
        }
        self.receipt.expected_sessions = expected.len();
        self.receipt.observed_sessions = observed.len();
        self.receipt.expected_links = self.links;
        self.receipt.watch_drained =
            !self.watches.is_empty() && self.watches.values().all(|drained| *drained);
        self.receipt.evidence_complete = self.receipt.watch_ready
            && self.receipt.watch_drained
            && self.receipt.discovery_complete
            && !self.receipt.revoked
            && self.links > 0
            && self
                .receipt
                .sessions
                .values()
                .flat_map(BTreeMap::values)
                .all(|state| {
                    state.ready_before_submission
                        && !state.segments.is_empty()
                        && state.segments.iter().all(|segment| {
                            segment.ready_before_submission
                                && !segment.restart_gap_before
                                && segment.export_finished
                                && segment.evidence.as_ref().is_some_and(|evidence| {
                                    evidence.session_id == state.session_id
                                        && evidence.delivery_complete
                                        && evidence.payloads_complete
                                        && evidence.continuity_complete
                                        && evidence.evidence_complete
                                        && evidence.finalization.as_ref().is_some_and(|cutoff| {
                                            cutoff.observation_id == segment.observation_id
                                        })
                                })
                        })
                });
        self.receipt
    }
}

const fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256
}

mod execution_watch;

#[cfg(test)]
mod tests;
