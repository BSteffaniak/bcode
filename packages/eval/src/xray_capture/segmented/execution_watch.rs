//! Normalization from D's portable application receipts. No server internals.
use super::{EvalXrayAggregation, ExecutionReadinessReceipt};
use bcode_xray_models::XRAY_SCHEMA_VERSION;
use bcode_xray_models::execution_watch::{
    EXECUTION_WATCH_SCHEMA_VERSION, ExecutionWatchEvent, ExecutionWatchFinalEvidence,
    ExecutionWatchPage, ExecutionWatchReady, ExecutionWatchStatus,
};

struct HookReadiness<'a> {
    watch_id: &'a str,
    event: &'a ExecutionWatchEvent,
}
impl ExecutionReadinessReceipt for HookReadiness<'_> {
    fn schema_version(&self) -> u16 {
        EXECUTION_WATCH_SCHEMA_VERSION
    }
    fn watch_id(&self) -> &str {
        self.watch_id
    }
    fn run_id(&self) -> &str {
        &self.event.run_id
    }
    fn session_id(&self) -> &str {
        &self.event.session_id
    }
    fn observation_id(&self) -> &str {
        &self.event.envelope.observation_id
    }
    fn ready_before_dispatch(&self) -> bool {
        true
    }
    // The hook observation begins before its dispatch. This makes no assertion
    // that the watch covered execution before its admission.
    fn prior_execution_gap(&self) -> bool {
        false
    }
}

impl EvalXrayAggregation {
    /// Admit D's portable readiness response before starting future execution.
    /// `prior_execution_gap` is deliberately retained: even a watch admitted now
    /// does not certify earlier execution. A fresh root source must be bound before
    /// dispatch by the application, not repaired by eval timestamps.
    pub fn execution_watch_ready(&mut self, ready: &ExecutionWatchReady) {
        if ready.schema_version != EXECUTION_WATCH_SCHEMA_VERSION
            || ready.cursor != 0
            || ready.max_page_items == 0
            || ready.max_page_bytes == 0
        {
            self.issue("unsupported_execution_watch_ready");
            return;
        }
        self.watch_ready(&ready.watch_id, ready.prior_execution_gap);
    }

    /// Reduce already delivered D pages; caller retains envelope recording and
    /// payload export responsibility. Hook-generated `xray.ready` identifies the
    /// exact pre-dispatch observation, not a second late local observation.
    pub fn execution_watch_page(&mut self, page: &ExecutionWatchPage) {
        if page.schema_version != EXECUTION_WATCH_SCHEMA_VERSION
            || !self.watches.contains_key(&page.watch_id)
        {
            self.issue("unsupported_or_foreign_execution_watch_page");
            return;
        }
        self.watch_loss(
            page.omitted_links,
            page.omitted_events,
            page.contention_gaps,
        );
        if page.prior_execution_gap {
            self.issue("prior_execution_gap");
        }
        self.execution_status(page.status);
        for event in &page.events {
            if !self.expect_session(&event.run_id, &event.session_id) {
                continue;
            }
            let envelope = &event.envelope;
            if envelope.schema_version != XRAY_SCHEMA_VERSION
                || envelope.event.correlation.session_id != event.session_id
            {
                self.issue("foreign_or_unsupported_watch_envelope");
                continue;
            }
            // Run identity comes from D's canonical linkage, not optional control
            // correlation; a supplied conflicting run identity still fails closed.
            if envelope
                .event
                .correlation
                .run_id
                .as_ref()
                .is_some_and(|run| run != &event.run_id)
            {
                self.issue("foreign_watch_run_correlation");
                continue;
            }
            if envelope.event.domain == "xray" && envelope.event.name == "ready" {
                if envelope.sequence != 1 {
                    self.issue("invalid_watch_ready_sequence");
                    continue;
                }
                self.readiness(&HookReadiness {
                    watch_id: &page.watch_id,
                    event,
                });
            }
        }
    }

    /// Reduce the final expected-link inventory and cutoff failures. This is NOT a
    /// drain acknowledgement: the caller must consume every cutoff page, matching
    /// finalized envelopes, payload exports and sink closes before `watch_drained`.
    pub fn execution_watch_final_evidence(&mut self, evidence: &ExecutionWatchFinalEvidence) {
        if evidence.schema_version != EXECUTION_WATCH_SCHEMA_VERSION
            || !self.watches.contains_key(&evidence.watch_id)
        {
            self.issue("unsupported_or_foreign_watch_final_evidence");
            return;
        }
        self.watch_loss(
            evidence.omitted_links,
            evidence.omitted_events,
            evidence.contention_gaps,
        );
        if evidence.prior_execution_gap {
            self.issue("prior_execution_gap");
        }
        self.execution_status(evidence.status);
        for cutoff in &evidence.cutoffs {
            self.expect_session(&cutoff.run_id, &cutoff.session_id);
            if cutoff.finalization.is_none() {
                self.issue("watch_cutoff_unavailable");
            }
        }
    }

    fn execution_status(&mut self, status: ExecutionWatchStatus) {
        match status {
            ExecutionWatchStatus::Ready
            | ExecutionWatchStatus::Finalized
            | ExecutionWatchStatus::Closed => {}
            ExecutionWatchStatus::Revoked => self.revoked(),
            ExecutionWatchStatus::Overloaded => self.issue("watch_overloaded"),
            ExecutionWatchStatus::OwnershipChanged => self.issue("watch_ownership_changed"),
        }
    }
}
