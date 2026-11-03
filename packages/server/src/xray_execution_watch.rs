//! Explicitly authorized, ephemeral workflow execution observation.
//!
//! This registry is not canonical run membership or execution authority. The workflow
//! owner supplies verified ancestry and session links before dispatch. Observers cannot
//! steer execution; capacity failures disable diagnostics, never the observed work.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bcode_session_models::SessionId;
use bcode_workflow_store::WorkflowExecutionAuthority;
use bcode_xray::{XrayHub, XrayObservation};
use bcode_xray_models::{XrayEnvelope, XrayObserveRequest};
use futures::FutureExt;

mod application;
#[cfg(test)]
#[path = "xray_execution_watch/application_tests.rs"]
mod application_tests;

/// Configurable bounds on concurrent diagnostic resources, not workflow lifetime.
#[derive(Clone, Debug)]
pub struct ExecutionWatchPolicy {
    /// Concurrent watches in this daemon.
    pub concurrent_watches: usize,
    /// Concurrent retained session subscriptions per watch.
    pub max_sessions_per_watch: usize,
    /// Maximum envelopes inspected by one page, including filtered events.
    pub max_page_events: usize,
    /// Maximum serialized event bytes returned by one page.
    pub max_page_bytes: usize,
    /// Maximum canonical parent lookups per materialization hook.
    pub max_ancestry_lookups: usize,
}

impl Default for ExecutionWatchPolicy {
    fn default() -> Self {
        Self {
            concurrent_watches: 32,
            max_sessions_per_watch: 256,
            max_page_events: 128,
            max_page_bytes: 1024 * 1024,
            max_ancestry_lookups: 128,
        }
    }
}

/// An explicit run/recursive-descendant diagnostic authorization, not session access.
/// Construct only after the authenticated application boundary authorizes this scope.
#[derive(Clone)]
pub struct ExecutionWatchGrant {
    principal: String,
    root_run_id: String,
    descendants: bool,
    deep_capture: bool,
    revoked: Arc<AtomicBool>,
    deadline: Option<std::time::Instant>,
}

impl ExecutionWatchGrant {
    /// Record an already completed run-scope and optional sensitive-capture decision.
    ///
    /// # Errors
    /// Rejects empty/oversized identities. Does not perform authorization itself.
    pub(crate) fn authorized(
        principal: String,
        root_run_id: String,
        descendants: bool,
        deep_capture: bool,
    ) -> Result<Self, ExecutionWatchError> {
        if !valid_identity(&principal) || !valid_identity(&root_run_id) {
            return Err(ExecutionWatchError::InvalidRequest);
        }
        Ok(Self {
            principal,
            root_run_id,
            descendants,
            deep_capture,
            revoked: Arc::new(AtomicBool::new(false)),
            deadline: None,
        })
    }

    #[must_use]
    pub(crate) fn principal(&self) -> &str {
        &self.principal
    }

    #[must_use]
    pub(crate) fn scope(&self) -> bcode_xray_models::execution_watch::ExecutionWatchScope {
        bcode_xray_models::execution_watch::ExecutionWatchScope {
            root_run_id: self.root_run_id.clone(),
            recursive: self.descendants,
            deep_capture: self.deep_capture,
        }
    }

    /// Revoke this grant and all clones. Call registry `revoke` to immediately release captures.
    fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
    }
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

/// Fixed, secret-safe admission failures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionWatchError {
    /// Malformed identity, unsupported version, cursor, or page bound.
    InvalidRequest,
    /// No matching independently authorized run grant.
    Forbidden,
    /// Configured concurrent resource capacity reached.
    Capacity,
    /// Registry is unavailable; no capture was admitted.
    Unavailable,
}

/// Segment state. Terminal diagnostics never imply terminal execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionWatchStatus {
    /// Future links are subscribed synchronously before dispatch.
    Ready,
    /// Capture cut off; retained data remains available for draining/export.
    Finalized,
    /// Authorization withdrawn; pending data is discarded.
    Revoked,
    /// Resource overload; all subscriptions dropped.
    Overloaded,
    /// Owner changed; a fresh authorized segment is required.
    OwnershipChanged,
}

/// A run-labelled envelope; the engine sequence remains observation-local.
pub struct ExecutionWatchEvent {
    /// Exact execution run established by the canonical linkage hook.
    pub run_id: String,
    /// Exact linked session.
    pub session_id: SessionId,
    /// Engine envelope, including readiness and explicit engine loss.
    pub envelope: XrayEnvelope,
}

/// Bounded, destructive live page. Not a durable cursor or restart replay.
pub struct ExecutionWatchPage {
    /// Unique ephemeral segment identity.
    pub watch_id: String,
    /// Cursor required by the next page; retry does not replay consumed events.
    pub next_cursor: u64,
    /// Current diagnostic state, independent of execution outcome.
    pub status: ExecutionWatchStatus,
    /// Every new segment is gapful with respect to earlier/restarted execution.
    pub prior_execution_gap: bool,
    /// Hooks skipped on registry contention (conservative daemon-wide count).
    pub contention_gaps: u64,
    /// Links omitted due to unsupported scope, ancestry budget, or admission failure.
    pub omitted_links: u64,
    /// Oversized or foreign-run envelopes omitted on this page.
    pub omitted_events: u64,
    /// Envelopes bounded by configured count and serialized envelope bytes.
    pub events: Vec<ExecutionWatchEvent>,
}

type CutoffResult = Result<bcode_xray_models::XrayFinalization, bcode_xray::XrayError>;
type CutoffFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = (XrayObservation, CutoffResult)> + Send>>;

#[derive(Default)]
struct FinalizationProgress(tokio::sync::Notify);

impl futures::task::ArcWake for FinalizationProgress {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.0.notify_waiters();
    }
}

struct Subscription {
    run_id: String,
    authority: WorkflowExecutionAuthority,
    observation: Option<XrayObservation>,
    observation_id: String,
    finalizing: Option<CutoffFuture>,
    ended: bool,
    // None is resumable pending/not requested, never successful absent evidence.
    finalization: Option<Result<bcode_xray_models::XrayFinalization, ()>>,
}

impl Subscription {
    /// Settle terminal evidence only; a pending barrier remains resumable.
    fn settle_finalization(
        &mut self,
        result: Result<bcode_xray_models::XrayFinalization, bcode_xray::XrayError>,
    ) -> bool {
        match result {
            Ok(receipt) => self.finalization = Some(Ok(receipt)),
            Err(bcode_xray::XrayError::FinalizePending) => return false,
            Err(_) => self.finalization = Some(Err(())),
        }
        true
    }
}

struct Watch {
    grant: ExecutionWatchGrant,
    status: ExecutionWatchStatus,
    sessions: BTreeMap<(SessionId, String), Subscription>,
    cursor: u64,
    after_session: Option<(SessionId, String)>,
    omitted_links: u64,
    omitted_events: u64,
    contention_start: u64,
    prior_execution_gap: bool,
    cutoffs: Option<Vec<bcode_xray_models::execution_watch::ExecutionWatchCutoff>>,
}

/// Daemon-local read-only observer registry. No durable claims or execution handles.
pub struct ExecutionWatchRegistry {
    policy: ExecutionWatchPolicy,
    watches: Mutex<BTreeMap<String, Watch>>,
    finalization_progress: Arc<FinalizationProgress>,
    contention: AtomicU64,
}

impl std::fmt::Debug for ExecutionWatchRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never lock or expose grants, principals, run/session identities or captures.
        formatter
            .debug_struct("ExecutionWatchRegistry")
            .finish_non_exhaustive()
    }
}

impl Default for ExecutionWatchRegistry {
    fn default() -> Self {
        Self::new(ExecutionWatchPolicy::default()).expect("valid execution watch defaults")
    }
}

impl ExecutionWatchRegistry {
    /// Create a registry with explicit resource policy.
    ///
    /// # Errors
    /// Rejects zero bounds and pages too small for meaningful envelopes.
    pub fn new(policy: ExecutionWatchPolicy) -> Result<Self, ExecutionWatchError> {
        if policy.concurrent_watches == 0
            || policy.max_sessions_per_watch == 0
            || policy.max_page_events == 0
            || policy.max_page_bytes < 1024
            || policy.max_ancestry_lookups == 0
        {
            return Err(ExecutionWatchError::InvalidRequest);
        }
        Ok(Self {
            policy,
            watches: Mutex::new(BTreeMap::new()),
            finalization_progress: Arc::default(),
            contention: AtomicU64::new(0),
        })
    }

    /// Whether hooks need bounded canonical ancestry inspection. Contention is explicit loss.
    pub(crate) fn active(&self) -> bool {
        let Ok(watches) = self.watches.try_lock() else {
            self.contention.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        watches
            .values()
            .any(|watch| watch.status == ExecutionWatchStatus::Ready)
    }

    /// Canonical ancestry inspection budget, not a workflow recursion limit.
    #[must_use]
    pub const fn ancestry_budget(&self) -> usize {
        self.policy.max_ancestry_lookups
    }

    /// Admit an explicitly authorized live segment. Ordinary attachment remains gapful
    /// relative to earlier execution; only the fresh admission reservation clears it.
    ///
    /// # Errors
    /// Rejects unsupported versions, revoked grants, capacity and unavailable state.
    pub fn open(
        &self,
        schema_version: u16,
        grant: &ExecutionWatchGrant,
    ) -> Result<String, ExecutionWatchError> {
        if schema_version != 1 {
            return Err(ExecutionWatchError::InvalidRequest);
        }
        if grant.revoked.load(Ordering::Acquire) {
            return Err(ExecutionWatchError::Forbidden);
        }
        let mut watches = self
            .watches
            .lock()
            .map_err(|_| ExecutionWatchError::Unavailable)?;
        if watches.len() >= self.policy.concurrent_watches {
            return Err(ExecutionWatchError::Capacity);
        }
        let id = uuid::Uuid::new_v4().to_string();
        watches.insert(
            id.clone(),
            Watch {
                grant: grant.clone(),
                status: ExecutionWatchStatus::Ready,
                sessions: BTreeMap::new(),
                cursor: 0,
                after_session: None,
                omitted_links: 0,
                omitted_events: 0,
                contention_start: self.contention.load(Ordering::Relaxed),
                prior_execution_gap: true,
                cutoffs: None,
            },
        );
        drop(watches);
        Ok(id)
    }

    /// Install capture synchronously before dispatch, using verified canonical ancestry.
    /// `ancestors` excludes `run_id`; incomplete ancestry never confers descendant access.
    /// The hub's admission-time run filter must cover events AND payload retention.
    /// This is essential even for initially isolated sessions later reused by descendants.
    pub(crate) fn before_dispatch(
        &self,
        hub: &XrayHub,
        run_id: &str,
        session_id: SessionId,
        authority: &WorkflowExecutionAuthority,
        ancestors: &[String],
        ancestry_complete: bool,
    ) {
        let Ok(mut watches) = self.watches.try_lock() else {
            self.contention.fetch_add(1, Ordering::Relaxed);
            return;
        };
        for watch in watches.values_mut() {
            if watch.grant.revoked.load(Ordering::Acquire) {
                watch.status = ExecutionWatchStatus::Revoked;
                watch.sessions.clear();
            }
            if watch.status != ExecutionWatchStatus::Ready {
                continue;
            }
            let direct = watch.grant.root_run_id == run_id;
            let descendant =
                watch.grant.descendants && ancestors.contains(&watch.grant.root_run_id);
            if !direct && !descendant {
                if !ancestry_complete && watch.grant.descendants {
                    watch.omitted_links = watch.omitted_links.saturating_add(1);
                }
                continue;
            }
            let key = (session_id, run_id.to_owned());
            if let Some(existing) = watch.sessions.get(&key) {
                if existing.authority == *authority {
                    continue;
                }
                watch.status = ExecutionWatchStatus::OwnershipChanged;
                watch.sessions.clear();
                continue;
            }
            if watch
                .sessions
                .values()
                .any(|entry| entry.run_id == run_id && entry.authority != *authority)
            {
                watch.status = ExecutionWatchStatus::OwnershipChanged;
                watch.sessions.clear();
                continue;
            }
            if watch.sessions.len() >= self.policy.max_sessions_per_watch {
                watch.status = ExecutionWatchStatus::Overloaded;
                watch.sessions.clear();
                continue;
            }
            match hub.observe_run_with_deadline(
                XrayObserveRequest {
                    schema_version: 1,
                    session_id: session_id.to_string(),
                    deep_capture: watch.grant.deep_capture,
                },
                run_id.to_owned(),
                watch.grant.deadline,
            ) {
                Ok(observation) => {
                    watch.sessions.insert(
                        key,
                        Subscription {
                            run_id: run_id.to_owned(),
                            authority: authority.clone(),
                            observation_id: observation.id().to_owned(),
                            observation: Some(observation),
                            finalizing: None,
                            ended: false,
                            finalization: None,
                        },
                    );
                }
                Err(_) => {
                    watch.omitted_links = watch.omitted_links.saturating_add(1);
                }
            }
        }
    }

    /// Pull a bounded live page. No await, producer wait, storage replay or execution action.
    ///
    /// # Errors
    /// Rejects foreign/revoked grants, stale cursors, invalid bounds and unknown watches.
    pub fn page(
        &self,
        grant: &ExecutionWatchGrant,
        watch_id: &str,
        cursor: u64,
        limit: usize,
    ) -> Result<ExecutionWatchPage, ExecutionWatchError> {
        if limit == 0 || limit > self.policy.max_page_events {
            return Err(ExecutionWatchError::InvalidRequest);
        }
        let mut watches = self
            .watches
            .lock()
            .map_err(|_| ExecutionWatchError::Unavailable)?;
        let watch = authorized_watch(&mut watches, grant, watch_id)?;
        if watch.cursor != cursor || cursor == u64::MAX {
            return Err(ExecutionWatchError::InvalidRequest);
        }
        let mut events = Vec::new();
        let mut omitted_events = 0_u64;
        let mut bytes = 0_usize;
        // Round-robin bounded polling avoids a noisy session starving later descendants.
        let mut keys: Vec<_> = watch.sessions.keys().cloned().collect();
        if let Some(after) = watch.after_session.as_ref() {
            let pivot = keys.partition_point(|key| key <= after);
            keys.rotate_left(pivot);
        }
        for (inspected, key) in keys.into_iter().enumerate() {
            if inspected >= limit {
                break;
            }
            watch.after_session = Some(key.clone());
            let Some(subscription) = watch.sessions.get_mut(&key) else {
                continue;
            };
            if subscription.ended {
                continue;
            }
            let Some(observation) = subscription.observation.as_mut() else {
                continue;
            };
            match observation.recv().now_or_never() {
                Some(Some(envelope)) => {
                    let control = envelope.event.domain == "xray";
                    if !control
                        && envelope.event.correlation.run_id.as_deref()
                            != Some(subscription.run_id.as_str())
                    {
                        omitted_events = omitted_events.saturating_add(1);
                        continue;
                    }
                    let size =
                        serde_json::to_vec(&envelope).map_or(usize::MAX, |value| value.len());
                    if size > self.policy.max_page_bytes.saturating_sub(bytes) {
                        omitted_events = omitted_events.saturating_add(1);
                        continue;
                    }
                    bytes += size;
                    events.push(ExecutionWatchEvent {
                        run_id: subscription.run_id.clone(),
                        session_id: key.0,
                        envelope,
                    });
                }
                Some(None) => {
                    subscription.ended = true;
                }
                None => {}
            }
        }
        watch.omitted_events = watch.omitted_events.saturating_add(omitted_events);
        watch.cursor += 1;
        let page = ExecutionWatchPage {
            watch_id: watch_id.to_owned(),
            next_cursor: watch.cursor,
            status: watch.status,
            prior_execution_gap: watch.prior_execution_gap,
            contention_gaps: self
                .contention
                .load(Ordering::Relaxed)
                .saturating_sub(watch.contention_start),
            omitted_links: watch.omitted_links,
            omitted_events,
            events,
        };
        drop(watches);
        Ok(page)
    }

    /// Read retained bytes only from this grant's exact run/session observation.
    /// No bearer/capture credential is accepted or exported. Payload ranges remain
    /// bounded by the engine policy and unavailable after release/revocation.
    ///
    /// # Errors
    /// Rejects foreign grants, shallow captures, foreign observation/session identities,
    /// missing payloads and invalid ranges with fixed secret-safe errors.
    #[cfg(test)]
    pub fn read_payload(
        &self,
        hub: &XrayHub,
        grant: &ExecutionWatchGrant,
        watch_id: &str,
        run_id: &str,
        request: &bcode_xray_models::XrayReadPayloadRequest,
    ) -> Result<bcode_xray_models::XrayPayloadRange, ExecutionWatchError> {
        let session_id = request
            .session_id
            .parse::<SessionId>()
            .map_err(|_| ExecutionWatchError::InvalidRequest)?;
        if session_id.to_string() != request.session_id {
            return Err(ExecutionWatchError::InvalidRequest);
        }
        let mut watches = self
            .watches
            .lock()
            .map_err(|_| ExecutionWatchError::Unavailable)?;
        let watch = authorized_watch(&mut watches, grant, watch_id)?;
        let subscription = watch
            .sessions
            .get(&(session_id, run_id.to_owned()))
            .ok_or(ExecutionWatchError::Forbidden)?;
        if !grant.deep_capture || subscription.observation_id != request.observation_id {
            return Err(ExecutionWatchError::Forbidden);
        }
        let result = hub
            .read_payload(request)
            .map_err(|_| ExecutionWatchError::Unavailable);
        drop(watches);
        result
    }

    /// Await only the engine's finite prior-publication boundary, checking exact
    /// grant/run/session/observation membership before and after suspension.
    /// Callers must bound the request and supply their authorized grant expiry.
    /// No registry lock or execution authority is retained across the await.
    ///
    /// # Errors
    /// Rejects expiry/revocation, foreign scopes, invalid requests and missing bytes.
    pub async fn read_payload_complete(
        &self,
        hub: &XrayHub,
        grant: &ExecutionWatchGrant,
        request: &bcode_xray_models::execution_watch::ExportExecutionWatchPayloadRequest,
        expires_at: Option<std::time::Instant>,
    ) -> Result<bcode_xray_models::XrayPayloadRange, ExecutionWatchError> {
        if request.schema_version != 1 {
            return Err(ExecutionWatchError::InvalidRequest);
        }
        let watch_id = &request.watch_id;
        let run_id = &request.run_id;
        let request = &request.payload;
        let session_id = request
            .session_id
            .parse::<SessionId>()
            .map_err(|_| ExecutionWatchError::InvalidRequest)?;
        if session_id.to_string() != request.session_id {
            return Err(ExecutionWatchError::InvalidRequest);
        }
        let authorize = || {
            if expires_at.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                return Err(ExecutionWatchError::Forbidden);
            }
            let mut watches = self
                .watches
                .lock()
                .map_err(|_| ExecutionWatchError::Unavailable)?;
            let watch = authorized_watch(&mut watches, grant, watch_id)?;
            let subscription = watch
                .sessions
                .get(&(session_id, run_id.to_owned()))
                .ok_or(ExecutionWatchError::Forbidden)?;
            if !grant.deep_capture || subscription.observation_id != request.observation_id {
                return Err(ExecutionWatchError::Forbidden);
            }
            drop(watches);
            Ok(())
        };
        authorize()?;
        let result = hub.read_payload_complete(request).await;
        authorize()?;
        result.map_err(|_| ExecutionWatchError::Unavailable)
    }

    /// Revoke a grant and immediately discard its pending captures, including clones.
    ///
    /// # Errors
    /// Returns unavailable if registry state cannot be locked.
    pub fn revoke(&self, grant: &ExecutionWatchGrant) -> Result<(), ExecutionWatchError> {
        grant.revoke();
        let mut watches = self
            .watches
            .lock()
            .map_err(|_| ExecutionWatchError::Unavailable)?;
        watches.retain(|_, watch| !Arc::ptr_eq(&watch.grant.revoked, &grant.revoked));
        drop(watches);
        self.finalization_progress.0.notify_waiters();
        Ok(())
    }

    /// End affected segments when the application verifies an ownership transfer/release.
    /// The owner must call this before permitting further work under another authority.
    /// No fencing token or capture credentials are returned to observers.
    ///
    /// # Errors
    /// Returns unavailable if registry state cannot be locked.
    pub fn ownership_changed(&self, run_id: &str) -> Result<(), ExecutionWatchError> {
        let mut watches = self
            .watches
            .lock()
            .map_err(|_| ExecutionWatchError::Unavailable)?;
        for watch in watches.values_mut() {
            if watch.status != ExecutionWatchStatus::Ready {
                continue;
            }
            if watch.grant.root_run_id == run_id
                || watch.sessions.values().any(|entry| entry.run_id == run_id)
            {
                watch.sessions.clear();
                watch.status = ExecutionWatchStatus::OwnershipChanged;
            }
        }
        drop(watches);
        Ok(())
    }

    /// Stop one capture with an engine cutoff; keep paging until its finalization arrives.
    /// This does not settle or cancel the session's execution.
    ///
    /// # Errors
    /// Rejects foreign/revoked grants, unknown sessions and unavailable engine state.
    #[cfg(test)]
    pub fn finalize_session(
        &self,
        grant: &ExecutionWatchGrant,
        watch_id: &str,
        session_id: SessionId,
        run_id: &str,
    ) -> Result<bcode_xray_models::XrayFinalization, ExecutionWatchError> {
        let mut watches = self
            .watches
            .lock()
            .map_err(|_| ExecutionWatchError::Unavailable)?;
        let watch = authorized_watch(&mut watches, grant, watch_id)?;
        let result = watch
            .sessions
            .get_mut(&(session_id, run_id.to_owned()))
            .ok_or(ExecutionWatchError::Forbidden)?
            .observation
            .as_mut()
            .ok_or(ExecutionWatchError::Unavailable)?
            .finalize()
            .map_err(|_| ExecutionWatchError::Unavailable);
        drop(watches);
        result
    }

    /// Drop one session's diagnostic subscription after its final drain, freeing concurrent
    /// capacity for future descendants. This never releases canonical session ownership.
    ///
    /// # Errors
    /// Rejects foreign/revoked grants and unavailable/unknown watches.
    #[cfg(test)]
    pub fn release_session(
        &self,
        grant: &ExecutionWatchGrant,
        watch_id: &str,
        session_id: SessionId,
        run_id: &str,
    ) -> Result<(), ExecutionWatchError> {
        let mut watches = self
            .watches
            .lock()
            .map_err(|_| ExecutionWatchError::Unavailable)?;
        let watch = authorized_watch(&mut watches, grant, watch_id)?;
        watch.sessions.remove(&(session_id, run_id.to_owned()));
        drop(watches);
        Ok(())
    }

    /// End and remove a diagnostic segment. Dropping captures never cancels work.
    ///
    /// # Errors
    /// Rejects foreign/revoked grants and unavailable/unknown watches.
    #[cfg(test)]
    pub fn close(
        &self,
        grant: &ExecutionWatchGrant,
        watch_id: &str,
    ) -> Result<(), ExecutionWatchError> {
        let mut watches = self
            .watches
            .lock()
            .map_err(|_| ExecutionWatchError::Unavailable)?;
        authorized_watch(&mut watches, grant, watch_id)?;
        watches.remove(watch_id);
        drop(watches);
        Ok(())
    }
}

fn authorized_watch<'a>(
    watches: &'a mut BTreeMap<String, Watch>,
    grant: &ExecutionWatchGrant,
    id: &str,
) -> Result<&'a mut Watch, ExecutionWatchError> {
    let watch = watches.get_mut(id).ok_or(ExecutionWatchError::Forbidden)?;
    if watch.grant.principal != grant.principal
        || watch.grant.root_run_id != grant.root_run_id
        || watch.grant.descendants != grant.descendants
        || watch.grant.deep_capture != grant.deep_capture
        || !Arc::ptr_eq(&watch.grant.revoked, &grant.revoked)
    {
        return Err(ExecutionWatchError::Forbidden);
    }
    if grant.revoked.load(Ordering::Acquire) {
        watch.sessions.clear();
        watch.status = ExecutionWatchStatus::Revoked;
        return Err(ExecutionWatchError::Forbidden);
    }
    Ok(watch)
}

#[cfg(test)]
#[path = "xray_execution_watch/tests.rs"]
mod tests;
