//! Required canonical writer operations. Implementations must use the session actor, not a journal.

use super::ManagedHarnessError;
use bcode_harness_models::{
    HarnessCapabilities, HarnessInstance, HarnessInteraction, HarnessObservationPage,
    HarnessReceipt, HarnessRequest, HarnessServiceResponse, HarnessSessionBinding,
};
use bcode_session_models::SessionId;
use std::future::Future;

/// Host identity is supplied by the owning daemon, never from a portable client payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessOwner {
    /// Canonical Bcode session identity.
    pub session_id: SessionId,
    /// Durable native ownership generation, distinct from model context generation.
    pub generation: u64,
    /// Artifact holding the session lease.
    pub artifact_id: String,
    /// Exact daemon instance holding the lease.
    pub daemon_id: String,
}

/// Bounded exact-session state, read from freshness-checked canonical projections.
#[derive(Debug, Clone)]
pub struct ManagedSession {
    /// Selected enabled plugin/config/credential namespace.
    pub instance: HarnessInstance,
    /// Native binding, absent before native creation is acknowledged.
    pub binding: Option<HarnessSessionBinding>,
    /// Current canonical routing target, including a live prepared-but-unconfirmed transport.
    pub target: Option<bcode_harness_models::HarnessSessionTarget>,
    /// Previously inspected capabilities, never guessed from vendor identity.
    pub capabilities: HarnessCapabilities,
}

/// Result of an atomic exact-request comparison and delivery claim.
pub enum DeliveryClaim {
    /// Only this result authorizes one native call, and only for the supplied owner.
    New,
    /// Identical retry with an already committed response: return it without native delivery.
    Recorded(Box<HarnessServiceResponse>),
    /// Delivery may have occurred, including a prior process loss. Reconcile; never resend.
    Uncertain,
}

/// Canonical host persistence contract used by the production runtime.
///
/// All writes compare the complete owner inside the serialized writer operation. A process-local
/// mutex or a lease checked before the write is insufficient. Implementations retain the lease
/// for the entire native call and commit. Reads are indexed exact-key/bounded-page operations.
/// No operation scans/replays history, invents a new session location, or dispatches native work.
pub trait ManagedHarnessStore: Send + Sync {
    /// A live canonical session lease and durable execution ownership hold. Dropping it must
    /// release only this hold; it must not terminalize native work or erase uncertainty.
    type OwnershipGuard: Send + Sync;

    /// Acquire verified ownership before any native I/O and hold it until the method returns.
    /// Transfers require positive proof of prior release, not a daemon heartbeat timeout alone.
    fn acquire(
        &self,
        owner: &HarnessOwner,
    ) -> impl Future<Output = Result<Self::OwnershipGuard, ManagedHarnessError>> + Send;

    /// Lease/namespace and durable generation verification; parentage does not pass this check.
    fn load(
        &self,
        owner: &HarnessOwner,
    ) -> impl Future<Output = Result<ManagedSession, ManagedHarnessError>> + Send;

    /// Hold explicit parent-assistance consent and both canonical owners through native delivery.
    /// Implementations without such admission must reject supplied grants and assistance responses.
    /// Controls revoke admission before their native side effect, even if the control later fails.
    fn assistance_guard(
        &self,
        owner: &HarnessOwner,
        request: &HarnessRequest,
    ) -> impl Future<Output = Result<Box<dyn Send + Sync>, ManagedHarnessError>> + Send {
        let _ = owner;
        async move {
            if matches!(&request.kind, bcode_harness_models::HarnessRequestKind::Open(open) if open.parent_assistance.is_some())
                || matches!(&request.kind, bcode_harness_models::HarnessRequestKind::Respond { response, .. }
                    if matches!(&response.value, bcode_harness_models::HarnessResponseValue::Assistance { .. }))
            {
                return Err(ManagedHarnessError::Unsupported);
            }
            Ok(Box::new(()) as Box<dyn Send + Sync>)
        }
    }

    /// Atomically accept the exact bounded request and mark delivery attempted before returning
    /// `New`. Retain canonical request identity/content for conflict detection. If any prior claim
    /// exists, return its committed response or `Uncertain`, never `New`.
    /// Auth requests are forbidden here; sensitive payloads must never enter this store.
    fn claim(
        &self,
        owner: &HarnessOwner,
        request: &HarnessRequest,
    ) -> impl Future<Output = Result<DeliveryClaim, ManagedHarnessError>> + Send;

    /// Persist validated native evidence using an exact retained claim identity. Response commit
    /// must atomically update binding/receipt/interaction projections and reject terminal reopening.
    fn commit(
        &self,
        owner: &HarnessOwner,
        request: &HarnessRequest,
        response: &HarnessServiceResponse,
    ) -> impl Future<Output = Result<(), ManagedHarnessError>> + Send;

    /// Preserve delivery uncertainty after a failed native call. Failure to commit this marker
    /// must still leave the earlier delivery-attempt marker non-replayable.
    fn uncertain(
        &self,
        owner: &HarnessOwner,
        request: &HarnessRequest,
    ) -> impl Future<Output = Result<(), ManagedHarnessError>> + Send;

    /// Resolve exactly one outstanding interaction under the current generation. The native
    /// response claim must consume this interaction atomically; identical retries return evidence.
    fn interaction(
        &self,
        owner: &HarnessOwner,
        request_id: &str,
    ) -> impl Future<Output = Result<HarnessInteraction, ManagedHarnessError>> + Send;

    /// Exact accepted submission receipt lookup. No history replay.
    fn receipt(
        &self,
        owner: &HarnessOwner,
        submission_id: &str,
        attempt_id: &str,
    ) -> impl Future<Output = Result<Option<HarnessReceipt>, ManagedHarnessError>> + Send;

    /// Build exact post-commit retention evidence for the current native connection.
    /// Implementations without canonical retention support must not assert acknowledgment.
    fn retention_context(
        &self,
        owner: &HarnessOwner,
        target: &bcode_harness_models::HarnessSessionTarget,
        after: Option<&str>,
    ) -> impl Future<
        Output = Result<Option<bcode_harness_models::HarnessRetentionContext>, ManagedHarnessError>,
    > + Send {
        let _ = (owner, target, after);
        async { Ok(None) }
    }

    /// Canonical append/dedup by stable observation identity. Conflicting duplicate content is an
    /// error. Persist page and cursor atomically, then broadcast committed observations. Tool and
    /// subagent observations remain inert external evidence. This never marks assignment complete.
    fn observations(
        &self,
        owner: &HarnessOwner,
        page: &HarnessObservationPage,
    ) -> impl Future<Output = Result<(), ManagedHarnessError>> + Send;

    /// Commit a bounded native page with the exact acknowledged input cursor. Keeping cursor
    /// provenance with the page lets canonical storage reject skipped/reordered poll responses.
    /// Existing implementations may delegate to `observations` when their storage derives the
    /// predecessor from immutable page identity; production adapters should override this method.
    fn observation_page(
        &self,
        owner: &HarnessOwner,
        after: Option<&str>,
        page: &HarnessObservationPage,
    ) -> impl Future<Output = Result<(), ManagedHarnessError>> + Send {
        let _ = after;
        self.observations(owner, page)
    }

    /// Indexed bounded canonical history remains available when plugins are disabled/offline.
    fn history(
        &self,
        session_id: SessionId,
        after: Option<String>,
        limit: u16,
    ) -> impl Future<Output = Result<HarnessObservationPage, ManagedHarnessError>> + Send;
}
