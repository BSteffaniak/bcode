//! Turn-scoped lifecycle, cancellation, and event publication primitives.
//!
//! # Scope invariants
//!
//! * A [`TurnScopeOwner`] allocates monotonic generations and permits only its current generation
//!   to publish, complete, or cancel; stale scopes cannot affect a replacement turn.
//! * Cancellation closes local progression and normal publication under one gate before opaque
//!   active handles are signalled. Normal events from closed or superseded scopes are rejected and
//!   counted without decoding their payloads.
//! * An [`InvocationScope`] preserves one exact invocation ID and accepts correlated lifecycle,
//!   contribution, exchange, input, service, artifact, and cancellation operations only while its
//!   parent turn owns the active generation.
//!
//! # Channel invariants
//!
//! * Lifecycle and contribution payloads remain schema-bearing or opaque runtime values; this
//!   module validates correlation and lifecycle state but does not select renderers or apply tool
//!   domain policy.
//! * Exchange, input, and nested-service waits race the same turn cancellation token and cannot
//!   return a normal response after the scope closes. Duplicate exchange IDs fail locally.
//! * Artifact sinks may stage privately, but externally visible publication must pass through
//!   [`ArtifactCommitGuard`], which linearizes commit against cancellation and generation changes.
//! * Normal event sinks are non-blocking. Any adapter that publishes asynchronously must retain the
//!   originating scope and re-check it at its final publication boundary. After closure, only an
//!   invocation-correlated `Cancelled` lifecycle bookkeeping event is accepted.

use crate::{AgentRuntimeEvent, CancellationToken};
use bcode_tool::{
    ToolArtifactWriteRequest, ToolArtifactWriteResolution, ToolContributionEvent,
    ToolExchangeRequest, ToolExchangeResolution, ToolInvocationInputResolution,
    ToolInvocationLifecycleEvent, ToolInvocationServiceRequest, ToolInvocationServiceResolution,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

struct InvocationOperationDuration {
    operation: &'static str,
    started: Instant,
}

impl InvocationOperationDuration {
    fn start(operation: &'static str) -> Self {
        Self {
            operation,
            started: Instant::now(),
        }
    }
}

impl Drop for InvocationOperationDuration {
    fn drop(&mut self) {
        tracing::debug!(
            operation = self.operation,
            duration_ms = self.started.elapsed().as_millis(),
            "neutral invocation operation completed"
        );
    }
}

/// Monotonic identity assigned by a host to one turn within its owning runtime/session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TurnGeneration(u64);

impl TurnGeneration {
    /// Create a generation from a host-owned monotonic value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the underlying monotonic value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Runtime/session-owned allocator and active-generation authority for turn scopes.
#[derive(Clone)]
pub struct TurnScopeOwner {
    next_generation: Arc<AtomicU64>,
    active_generation: Arc<AtomicU64>,
    active_control: Arc<Mutex<Option<Arc<TurnControl>>>>,
}

impl fmt::Debug for TurnScopeOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnScopeOwner")
            .field(
                "active_generation",
                &self.active_generation.load(Ordering::Acquire),
            )
            .finish_non_exhaustive()
    }
}

impl Default for TurnScopeOwner {
    fn default() -> Self {
        Self {
            next_generation: Arc::new(AtomicU64::new(1)),
            active_generation: Arc::new(AtomicU64::new(0)),
            active_control: Arc::new(Mutex::new(None)),
        }
    }
}

impl TurnScopeOwner {
    /// Create an owner with no active turn and a first generation of one.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate and activate the next monotonic turn generation.
    ///
    /// Any previously active turn is synchronously closed before this method returns.
    ///
    /// # Panics
    ///
    /// Panics if the owner's `u64` generation space is exhausted.
    #[must_use]
    pub fn begin_turn(
        &self,
        turn_id: impl Into<Arc<str>>,
        events: Arc<dyn TurnEventSink>,
        capabilities: InvocationCapabilities,
    ) -> TurnScope {
        let control = Arc::new(TurnControl::new());
        let (generation, previous, cancellation_handles) = {
            let mut active = self
                .active_control
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let cancellation_handles = active
                .as_ref()
                .and_then(|previous| previous.close_for_cancellation())
                .unwrap_or_default();
            let generation = self
                .next_generation
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    current.checked_add(1)
                })
                .expect("turn generation exhausted");
            let previous = active.replace(Arc::clone(&control));
            self.active_generation.store(generation, Ordering::Release);
            drop(active);
            (generation, previous, cancellation_handles)
        };
        drop(previous);
        TurnControl::signal_cancellation_handles(cancellation_handles);
        TurnScope::with_owner(
            turn_id,
            TurnGeneration::new(generation),
            control,
            events,
            capabilities,
            Arc::clone(&self.active_generation),
        )
    }

    /// Return the currently active generation, when any turn has been allocated.
    #[must_use]
    pub fn active_generation(&self) -> Option<TurnGeneration> {
        let generation = self.active_generation.load(Ordering::Acquire);
        (generation != 0).then_some(TurnGeneration::new(generation))
    }

    /// Cancel `scope` only if it is still this owner's active turn.
    ///
    /// Local progression and every invocation channel close before cancellation handles are
    /// signalled. A stale scope cannot cancel a newer turn.
    pub fn cancel_turn(&self, scope: &TurnScope) -> bool {
        let handles = {
            let active = self
                .active_control
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(control) = active.as_ref() else {
                return false;
            };
            if scope.generation
                != TurnGeneration::new(self.active_generation.load(Ordering::Acquire))
                || !Arc::ptr_eq(control, &scope.control)
            {
                return false;
            }
            let handles = control.close_for_cancellation();
            drop(active);
            handles
        };
        let Some(handles) = handles else {
            return false;
        };
        TurnControl::signal_cancellation_handles(handles);
        true
    }

    /// Complete and remove `scope` only if it is still this owner's active turn.
    ///
    /// The scope is closed before the owner clears its active-generation marker. A stale scope
    /// cannot complete or remove a newer turn.
    pub fn complete_turn(&self, scope: &TurnScope) -> bool {
        let mut active = self
            .active_control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(control) = active.as_ref() else {
            return false;
        };
        if scope.generation != TurnGeneration::new(self.active_generation.load(Ordering::Acquire))
            || !Arc::ptr_eq(control, &scope.control)
            || !control.complete()
        {
            return false;
        }
        active.take();
        self.active_generation.store(0, Ordering::Release);
        drop(active);
        true
    }

    /// Remove a terminal `scope` only if it is still this owner's active turn.
    ///
    /// Running and cancelling scopes cannot be released; callers must first complete them or mark
    /// cancellation terminal. A stale scope cannot remove a newer turn.
    pub fn release_terminal_turn(&self, scope: &TurnScope) -> bool {
        let mut active = self
            .active_control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(control) = active.as_ref() else {
            return false;
        };
        if scope.generation != TurnGeneration::new(self.active_generation.load(Ordering::Acquire))
            || !Arc::ptr_eq(control, &scope.control)
            || !matches!(
                control.lifecycle(),
                TurnLifecycle::Cancelled | TurnLifecycle::Completed
            )
        {
            return false;
        }
        active.take();
        self.active_generation.store(0, Ordering::Release);
        drop(active);
        true
    }
}

/// Current lifecycle of a turn scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnLifecycle {
    /// The turn accepts normal work and output.
    Running,
    /// Local progression is closed and external cleanup is being signalled.
    Cancelling,
    /// Cancellation cleanup has reached its host-defined terminal boundary.
    Cancelled,
    /// The turn completed normally.
    Completed,
}

impl TurnLifecycle {
    const RUNNING: u8 = 0;
    const CANCELLING: u8 = 1;
    const CANCELLED: u8 = 2;
    const COMPLETED: u8 = 3;

    const fn decode(value: u8) -> Self {
        match value {
            Self::RUNNING => Self::Running,
            Self::CANCELLING => Self::Cancelling,
            Self::COMPLETED => Self::Completed,
            _ => Self::Cancelled,
        }
    }
}

/// Opaque tool/provider cancellation signal registered with a turn.
///
/// Implementations must make `request_cancel` non-blocking. Domain-specific asynchronous cleanup
/// belongs to the implementation and must not delay the runtime's local cancellation boundary.
pub trait InvocationCancellation: Send + Sync {
    /// Request cancellation without waiting for external cleanup to finish.
    fn request_cancel(&self);
}

/// Event emitted through a generation-scoped runtime sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopedTurnEvent {
    /// Provider/runtime orchestration event.
    Runtime(AgentRuntimeEvent),
    /// Renderer-independent invocation lifecycle event.
    InvocationLifecycle(ToolInvocationLifecycleEvent),
    /// Opaque schema-versioned renderer contribution.
    Contribution(ToolContributionEvent),
}

/// Non-blocking destination for accepted turn events.
///
/// Implementations should synchronously enqueue or publish the event. If an adapter performs later
/// asynchronous publication, it must carry the originating `TurnScope` and re-check it at its own
/// final publication boundary.
pub trait TurnEventSink: Send + Sync {
    /// Accept an event whose turn scope is currently running.
    ///
    /// Returns `false` when the sink can no longer accept publication.
    fn emit(&self, event: ScopedTurnEvent) -> bool;
}

/// Neutral host persistence seam for accepted scoped runtime events.
///
/// The host sink calls this seam for runtime events, lifecycle events, and durable contributions.
/// Transient contributions bypass persistence and are never passed to implementations.
///
/// Implementations must synchronously accept or reject the event. Durable storage may complete
/// asynchronously after acceptance, but the implementation is responsible for owning any data it
/// needs before this method returns.
pub trait TurnEventPersistence: Send + Sync {
    /// Accept one event for persistence.
    ///
    /// Returns `false` when persistence admission has closed. Rejection prevents downstream
    /// observability and publication through [`HostTurnEventSink`].
    fn persist(&self, event: &ScopedTurnEvent) -> bool;
}

/// Neutral host observability seam for accepted scoped runtime events.
pub trait TurnEventObservability: Send + Sync {
    /// Observe an event after persistence admission and before final publication.
    fn observe(&self, event: &ScopedTurnEvent);
}

/// Composed neutral host event sink for persistence, observability, and publication.
///
/// Events are admitted in persistence, observability, and publication order. Transient
/// contributions bypass persistence by construction but still reach observability and publication.
/// A persistence rejection stops any other event before observation or publication. Publication
/// rejection is returned to the originating turn scope; an event that was already admitted for
/// persistence remains durable host work and is not rolled back.
pub struct HostTurnEventSink {
    publication: Arc<dyn TurnEventSink>,
    persistence: Option<Arc<dyn TurnEventPersistence>>,
    observability: Option<Arc<dyn TurnEventObservability>>,
}

impl HostTurnEventSink {
    /// Create a host event sink with publication only.
    #[must_use]
    pub fn new(publication: Arc<dyn TurnEventSink>) -> Self {
        Self {
            publication,
            persistence: None,
            observability: None,
        }
    }

    /// Attach host persistence admission.
    #[must_use]
    pub fn with_persistence(mut self, persistence: Arc<dyn TurnEventPersistence>) -> Self {
        self.persistence = Some(persistence);
        self
    }

    /// Attach host observability.
    #[must_use]
    pub fn with_observability(mut self, observability: Arc<dyn TurnEventObservability>) -> Self {
        self.observability = Some(observability);
        self
    }
}

impl TurnEventSink for HostTurnEventSink {
    fn emit(&self, event: ScopedTurnEvent) -> bool {
        let is_transient_contribution = matches!(
            &event,
            ScopedTurnEvent::Contribution(event)
                if event.persistence == bcode_tool::ToolContributionPersistence::Transient
        );
        if !is_transient_contribution
            && self
                .persistence
                .as_ref()
                .is_some_and(|persistence| !persistence.persist(&event))
        {
            return false;
        }
        if let Some(observability) = &self.observability {
            observability.observe(&event);
        }
        self.publication.emit(event)
    }
}

#[derive(Debug, Default)]
struct DiscardingTurnEventSink;

impl TurnEventSink for DiscardingTurnEventSink {
    fn emit(&self, _event: ScopedTurnEvent) -> bool {
        true
    }
}

/// Boxed asynchronous invocation capability operation.
pub type InvocationCapabilityFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Host broker for correlated renderer-neutral invocation exchanges.
pub trait InvocationExchangeBroker: Send + Sync {
    /// Resolve one exchange request exactly once.
    fn request(
        &self,
        request: ToolExchangeRequest,
    ) -> InvocationCapabilityFuture<'_, ToolExchangeResolution>;
}

/// Host router for unsolicited inputs addressed to active invocations.
pub trait InvocationInputRouter: Send + Sync {
    /// Wait for the next input addressed to `invocation_id`.
    fn receive(
        &self,
        invocation_id: &str,
    ) -> InvocationCapabilityFuture<'_, ToolInvocationInputResolution>;
}

/// Host router for nested services requested by active invocations.
pub trait InvocationServiceRouter: Send + Sync {
    /// Route one opaque service request.
    fn invoke(
        &self,
        request: ToolInvocationServiceRequest,
    ) -> InvocationCapabilityFuture<'_, ToolInvocationServiceResolution>;
}

/// Runtime-owned final-commit gate for one invocation artifact operation.
///
/// Artifact sinks may prepare non-public staging before calling [`Self::commit`], but must remove
/// that staging if the gate returns `None`. The commit closure must perform the externally visible
/// publication. The gate holds the owning turn's publication lock across the closure, so
/// cancellation or generation supersession linearizes strictly before or after the artifact
/// commit. Commit closures must not synchronously request cancellation or begin a new generation
/// for the same turn owner because those transitions wait for this boundary.
pub struct ArtifactCommitGuard {
    turn: TurnScope,
    invocation_id: Arc<str>,
}

impl fmt::Debug for ArtifactCommitGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ArtifactCommitGuard")
            .field("turn", &self.turn)
            .field("invocation_id", &self.invocation_id)
            .finish()
    }
}

impl ArtifactCommitGuard {
    const fn new(turn: TurnScope, invocation_id: Arc<str>) -> Self {
        Self {
            turn,
            invocation_id,
        }
    }

    /// Return the invocation that owns this final-commit gate.
    #[must_use]
    pub fn invocation_id(&self) -> &str {
        &self.invocation_id
    }

    /// Publish one staged artifact at the owning turn's atomic final-commit boundary.
    ///
    /// Returns `None` without calling `commit` when the turn is already closed or superseded.
    /// Cancellation and generation supersession cannot complete while the closure is running.
    pub fn commit<T>(self, commit: impl FnOnce() -> T) -> Option<T> {
        let _gate = self
            .turn
            .control
            .publication_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.turn.accepts_work() {
            Some(commit())
        } else {
            None
        }
    }
}

/// Host-owned bounded artifact sink for active invocations.
pub trait InvocationArtifactSink: Send + Sync {
    /// Persist one complete bounded artifact through the runtime-owned final-commit gate.
    fn write(
        &self,
        request: ToolArtifactWriteRequest,
        commit: ArtifactCommitGuard,
    ) -> InvocationCapabilityFuture<'_, ToolArtifactWriteResolution>;
}

#[derive(Debug, Default)]
struct UnsupportedInvocationCapabilities;

impl InvocationExchangeBroker for UnsupportedInvocationCapabilities {
    fn request(
        &self,
        _request: ToolExchangeRequest,
    ) -> InvocationCapabilityFuture<'_, ToolExchangeResolution> {
        Box::pin(async { ToolExchangeResolution::NoCompatibleConsumer })
    }
}

impl InvocationInputRouter for UnsupportedInvocationCapabilities {
    fn receive(
        &self,
        _invocation_id: &str,
    ) -> InvocationCapabilityFuture<'_, ToolInvocationInputResolution> {
        Box::pin(async { ToolInvocationInputResolution::Closed })
    }
}

impl InvocationServiceRouter for UnsupportedInvocationCapabilities {
    fn invoke(
        &self,
        _request: ToolInvocationServiceRequest,
    ) -> InvocationCapabilityFuture<'_, ToolInvocationServiceResolution> {
        Box::pin(async { ToolInvocationServiceResolution::Unsupported })
    }
}

impl InvocationArtifactSink for UnsupportedInvocationCapabilities {
    fn write(
        &self,
        _request: ToolArtifactWriteRequest,
        _commit: ArtifactCommitGuard,
    ) -> InvocationCapabilityFuture<'_, ToolArtifactWriteResolution> {
        Box::pin(async {
            ToolArtifactWriteResolution::Failed {
                code: "artifact_sink_unavailable".to_string(),
                message: "host does not provide an invocation artifact sink".to_string(),
            }
        })
    }
}

/// Host capabilities shared by invocation scopes under one turn.
#[derive(Clone)]
pub struct InvocationCapabilities {
    exchanges: Arc<dyn InvocationExchangeBroker>,
    inputs: Arc<dyn InvocationInputRouter>,
    services: Arc<dyn InvocationServiceRouter>,
    artifacts: Arc<dyn InvocationArtifactSink>,
}

impl fmt::Debug for InvocationCapabilities {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InvocationCapabilities")
            .finish_non_exhaustive()
    }
}

impl Default for InvocationCapabilities {
    fn default() -> Self {
        let unsupported = Arc::new(UnsupportedInvocationCapabilities);
        Self {
            exchanges: unsupported.clone(),
            inputs: unsupported.clone(),
            services: unsupported.clone(),
            artifacts: unsupported,
        }
    }
}

impl InvocationCapabilities {
    /// Create host invocation capabilities.
    #[must_use]
    pub fn new(
        exchanges: Arc<dyn InvocationExchangeBroker>,
        inputs: Arc<dyn InvocationInputRouter>,
        services: Arc<dyn InvocationServiceRouter>,
        artifacts: Arc<dyn InvocationArtifactSink>,
    ) -> Self {
        Self {
            exchanges,
            inputs,
            services,
            artifacts,
        }
    }

    /// Replace the exchange broker.
    #[must_use]
    pub fn with_exchange_broker(mut self, exchanges: Arc<dyn InvocationExchangeBroker>) -> Self {
        self.exchanges = exchanges;
        self
    }

    /// Replace the invocation input router.
    #[must_use]
    pub fn with_input_router(mut self, inputs: Arc<dyn InvocationInputRouter>) -> Self {
        self.inputs = inputs;
        self
    }

    /// Replace the nested service router.
    #[must_use]
    pub fn with_service_router(mut self, services: Arc<dyn InvocationServiceRouter>) -> Self {
        self.services = services;
        self
    }

    /// Replace the artifact sink.
    #[must_use]
    pub fn with_artifact_sink(mut self, artifacts: Arc<dyn InvocationArtifactSink>) -> Self {
        self.artifacts = artifacts;
        self
    }
}

/// Shared lifecycle and cancellation state for one turn generation.
pub struct TurnControl {
    lifecycle: AtomicU8,
    cancellation: CancellationToken,
    publication_gate: Mutex<()>,
    cancellations: Mutex<BTreeMap<String, Arc<dyn InvocationCancellation>>>,
    queued_cancellations: AtomicU64,
    running_cancellations: AtomicU64,
    discarded_normal_events: AtomicU64,
}

impl fmt::Debug for TurnControl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnControl")
            .field("lifecycle", &self.lifecycle())
            .field("registered_cancellations", &self.cancellation_count())
            .field("queued_cancellations", &self.queued_cancellation_count())
            .field("running_cancellations", &self.running_cancellation_count())
            .field(
                "discarded_normal_events",
                &self.discarded_normal_event_count(),
            )
            .finish_non_exhaustive()
    }
}

fn usize_to_u64_saturating(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

impl Default for TurnControl {
    fn default() -> Self {
        Self {
            lifecycle: AtomicU8::new(TurnLifecycle::RUNNING),
            cancellation: CancellationToken::new(),
            publication_gate: Mutex::new(()),
            cancellations: Mutex::new(BTreeMap::new()),
            queued_cancellations: AtomicU64::new(0),
            running_cancellations: AtomicU64::new(0),
            discarded_normal_events: AtomicU64::new(0),
        }
    }
}

impl TurnControl {
    /// Create running turn control.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the current lifecycle.
    #[must_use]
    pub fn lifecycle(&self) -> TurnLifecycle {
        TurnLifecycle::decode(self.lifecycle.load(Ordering::Acquire))
    }

    /// Return the cooperative cancellation token shared with turn work.
    #[must_use]
    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Return whether normal work and output remain accepted.
    #[must_use]
    pub fn accepts_normal_output(&self) -> bool {
        self.lifecycle() == TurnLifecycle::Running
    }

    /// Atomically close local progression and synchronously signal every registered opaque handle.
    ///
    /// Returns `true` only for the caller that transitions this turn from running to cancelling.
    pub fn begin_cancellation(&self) -> bool {
        let started = Instant::now();
        let Some(handles) = self.close_for_cancellation() else {
            return false;
        };
        Self::signal_cancellation_handles(handles);
        tracing::info!(
            target: "bcode::sdk",
            event = "bcode.cancellation",
            queued_cancellations = self.queued_cancellation_count(),
            running_cancellations = self.running_cancellation_count(),
        );
        tracing::debug!(
            duration_ms = started.elapsed().as_millis(),
            queued_cancellations = self.queued_cancellation_count(),
            running_cancellations = self.running_cancellation_count(),
            "neutral turn cancellation signalled"
        );
        true
    }

    fn close_for_cancellation(&self) -> Option<Vec<Arc<dyn InvocationCancellation>>> {
        let _gate = self
            .publication_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self
            .lifecycle
            .compare_exchange(
                TurnLifecycle::RUNNING,
                TurnLifecycle::CANCELLING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return None;
        }
        self.cancellation.cancel();
        let mut cancellations = self
            .cancellations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Some(std::mem::take(&mut *cancellations).into_values().collect())
    }

    fn signal_cancellation_handles(handles: Vec<Arc<dyn InvocationCancellation>>) {
        for handle in handles {
            handle.request_cancel();
        }
    }

    /// Mark a cancelling turn as terminally cancelled.
    pub fn mark_cancelled(&self) -> bool {
        self.lifecycle
            .compare_exchange(
                TurnLifecycle::CANCELLING,
                TurnLifecycle::CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Mark a running turn as normally completed and close normal output.
    pub fn complete(&self) -> bool {
        let _gate = self
            .publication_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.lifecycle
            .compare_exchange(
                TurnLifecycle::RUNNING,
                TurnLifecycle::COMPLETED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Register an opaque cancellation handle while the turn remains running.
    ///
    /// If cancellation already won the race, the handle is signalled immediately and is not
    /// retained.
    pub fn register_cancellation(
        &self,
        invocation_id: impl Into<String>,
        handle: Arc<dyn InvocationCancellation>,
    ) -> bool {
        let invocation_id = invocation_id.into();
        let _gate = self
            .publication_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.lifecycle() != TurnLifecycle::Running {
            handle.request_cancel();
            return false;
        }
        self.cancellations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(invocation_id, handle);
        true
    }

    /// Remove a completed invocation's cancellation handle.
    pub fn unregister_cancellation(&self, invocation_id: &str) -> bool {
        self.cancellations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(invocation_id)
            .is_some()
    }

    fn cancellation_count(&self) -> usize {
        self.cancellations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Return the number of admitted invocations cancelled before they started running.
    #[must_use]
    pub fn queued_cancellation_count(&self) -> u64 {
        self.queued_cancellations.load(Ordering::Acquire)
    }

    /// Return the number of running invocations cancelled by the scheduler.
    #[must_use]
    pub fn running_cancellation_count(&self) -> u64 {
        self.running_cancellations.load(Ordering::Acquire)
    }

    pub(crate) fn record_queued_cancellations(&self, count: usize) {
        self.queued_cancellations
            .fetch_add(usize_to_u64_saturating(count), Ordering::Relaxed);
    }

    pub(crate) fn record_running_cancellations(&self, count: usize) {
        self.running_cancellations
            .fetch_add(usize_to_u64_saturating(count), Ordering::Relaxed);
    }

    /// Return the number of normal events rejected after closure or supersession.
    #[must_use]
    pub fn discarded_normal_event_count(&self) -> u64 {
        self.discarded_normal_events.load(Ordering::Acquire)
    }

    fn record_discarded_normal_event(&self) {
        self.discarded_normal_events.fetch_add(1, Ordering::Relaxed);
    }

    fn emit(&self, sink: &dyn TurnEventSink, event: ScopedTurnEvent) -> bool {
        let _gate = self
            .publication_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.lifecycle() != TurnLifecycle::Running {
            self.record_discarded_normal_event();
            return false;
        }
        sink.emit(event)
    }

    fn emit_invocation_terminal(
        &self,
        sink: &dyn TurnEventSink,
        mut event: ToolInvocationLifecycleEvent,
    ) -> bool {
        let _gate = self
            .publication_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match self.lifecycle() {
            TurnLifecycle::Running => {
                if !matches!(
                    event.stage,
                    bcode_tool::ToolInvocationLifecycleStage::Completed
                        | bcode_tool::ToolInvocationLifecycleStage::Failed
                ) {
                    return false;
                }
            }
            TurnLifecycle::Cancelling | TurnLifecycle::Cancelled => {
                event.stage = bcode_tool::ToolInvocationLifecycleStage::Cancelled;
            }
            TurnLifecycle::Completed => return false,
        }
        sink.emit(ScopedTurnEvent::InvocationLifecycle(event))
    }

    fn emit_cancellation_lifecycle(
        &self,
        sink: &dyn TurnEventSink,
        event: ToolInvocationLifecycleEvent,
    ) -> bool {
        let _gate = self
            .publication_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !matches!(
            self.lifecycle(),
            TurnLifecycle::Cancelling | TurnLifecycle::Cancelled
        ) || event.stage != bcode_tool::ToolInvocationLifecycleStage::Cancelled
        {
            return false;
        }
        sink.emit(ScopedTurnEvent::InvocationLifecycle(event))
    }
}

/// Cloneable context shared by all work and output belonging to one turn generation.
#[derive(Clone)]
pub struct TurnScope {
    turn_id: Arc<str>,
    generation: TurnGeneration,
    control: Arc<TurnControl>,
    events: Arc<dyn TurnEventSink>,
    capabilities: InvocationCapabilities,
    active_generation: Option<Arc<AtomicU64>>,
}

impl fmt::Debug for TurnScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnScope")
            .field("turn_id", &self.turn_id)
            .field("generation", &self.generation)
            .field("lifecycle", &self.control.lifecycle())
            .finish_non_exhaustive()
    }
}

impl TurnScope {
    /// Create a scope with an explicit event sink.
    #[must_use]
    pub fn new(
        turn_id: impl Into<Arc<str>>,
        generation: TurnGeneration,
        events: Arc<dyn TurnEventSink>,
    ) -> Self {
        Self::with_capabilities(
            turn_id,
            generation,
            events,
            InvocationCapabilities::default(),
        )
    }

    /// Create a scope with explicit event and invocation capability adapters.
    #[must_use]
    pub fn with_capabilities(
        turn_id: impl Into<Arc<str>>,
        generation: TurnGeneration,
        events: Arc<dyn TurnEventSink>,
        capabilities: InvocationCapabilities,
    ) -> Self {
        Self {
            turn_id: turn_id.into(),
            generation,
            control: Arc::new(TurnControl::new()),
            events,
            capabilities,
            active_generation: None,
        }
    }

    fn with_owner(
        turn_id: impl Into<Arc<str>>,
        generation: TurnGeneration,
        control: Arc<TurnControl>,
        events: Arc<dyn TurnEventSink>,
        capabilities: InvocationCapabilities,
        active_generation: Arc<AtomicU64>,
    ) -> Self {
        Self {
            turn_id: turn_id.into(),
            generation,
            control,
            events,
            capabilities,
            active_generation: Some(active_generation),
        }
    }

    /// Create a scope that discards accepted events.
    #[must_use]
    pub fn without_events(turn_id: impl Into<Arc<str>>, generation: TurnGeneration) -> Self {
        Self::new(turn_id, generation, Arc::new(DiscardingTurnEventSink))
    }

    /// Return the host-assigned turn ID.
    #[must_use]
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    /// Return the host-assigned monotonic generation.
    #[must_use]
    pub const fn generation(&self) -> TurnGeneration {
        self.generation
    }

    /// Return shared turn lifecycle control.
    #[must_use]
    pub fn control(&self) -> Arc<TurnControl> {
        Arc::clone(&self.control)
    }

    /// Return whether this scope still owns the active generation and accepts normal work.
    #[must_use]
    pub fn accepts_work(&self) -> bool {
        let generation_is_active = self
            .active_generation
            .as_ref()
            .is_none_or(|active| active.load(Ordering::Acquire) == self.generation.get());
        generation_is_active && self.control.accepts_normal_output()
    }

    /// Emit a normal event if this generation remains active and running.
    #[must_use]
    pub fn emit(&self, event: ScopedTurnEvent) -> bool {
        if !self.accepts_work() {
            self.control.record_discarded_normal_event();
            return false;
        }
        self.control.emit(self.events.as_ref(), event)
    }

    pub(crate) fn emit_invocation_terminal(&self, event: ToolInvocationLifecycleEvent) -> bool {
        let generation_is_active = self
            .active_generation
            .as_ref()
            .is_none_or(|active| active.load(Ordering::Acquire) == self.generation.get());
        if !generation_is_active {
            self.control.record_discarded_normal_event();
            return false;
        }
        self.control
            .emit_invocation_terminal(self.events.as_ref(), event)
    }

    /// Emit only a cancelled invocation lifecycle event after normal output has closed.
    ///
    /// This explicit bookkeeping path cannot publish normal runtime events or contributions.
    #[must_use]
    pub(crate) fn emit_cancellation_lifecycle(&self, event: ToolInvocationLifecycleEvent) -> bool {
        self.control
            .emit_cancellation_lifecycle(self.events.as_ref(), event)
    }
}

/// Side-effect-free preparation context derived from one turn generation.
#[derive(Debug, Clone)]
pub struct PreparationScope {
    turn: TurnScope,
    host_context: Arc<[bcode_tool::ToolHostContextEntry]>,
}

impl PreparationScope {
    /// Create a preparation scope for one turn and opaque host context set.
    #[must_use]
    pub fn new(
        turn: TurnScope,
        host_context: impl Into<Arc<[bcode_tool::ToolHostContextEntry]>>,
    ) -> Self {
        Self {
            turn,
            host_context: host_context.into(),
        }
    }

    /// Return the parent turn scope.
    #[must_use]
    pub const fn turn(&self) -> &TurnScope {
        &self.turn
    }

    /// Return opaque host context forwarded to preparation.
    #[must_use]
    pub fn host_context(&self) -> &[bcode_tool::ToolHostContextEntry] {
        &self.host_context
    }

    /// Return cancellation state shared with the parent turn.
    #[must_use]
    pub fn cancellation(&self) -> CancellationToken {
        self.turn.control().cancellation()
    }

    /// Return whether preparation may continue producing normal work.
    #[must_use]
    pub fn accepts_work(&self) -> bool {
        self.turn.accepts_work()
    }
}

/// Active invocation context derived from one turn generation.
#[derive(Clone)]
pub struct InvocationScope {
    turn: TurnScope,
    invocation_id: Arc<str>,
    exchange_ids: Arc<Mutex<BTreeSet<String>>>,
}

impl fmt::Debug for InvocationScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InvocationScope")
            .field("invocation_id", &self.invocation_id)
            .field("turn", &self.turn)
            .field(
                "submitted_exchange_ids",
                &self
                    .exchange_ids
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .len(),
            )
            .finish()
    }
}

impl InvocationScope {
    /// Create an invocation scope under `turn`.
    #[must_use]
    pub fn new(turn: TurnScope, invocation_id: impl Into<Arc<str>>) -> Self {
        Self {
            turn,
            invocation_id: invocation_id.into(),
            exchange_ids: Arc::new(Mutex::new(BTreeSet::new())),
        }
    }

    /// Return the invocation identifier.
    #[must_use]
    pub fn invocation_id(&self) -> &str {
        &self.invocation_id
    }

    /// Return the parent turn scope.
    #[must_use]
    pub const fn turn(&self) -> &TurnScope {
        &self.turn
    }

    /// Return cancellation state shared with the parent turn.
    #[must_use]
    pub fn cancellation(&self) -> CancellationToken {
        self.turn.control().cancellation()
    }

    /// Return whether invocation work and normal output remain accepted.
    #[must_use]
    pub fn accepts_work(&self) -> bool {
        self.turn.accepts_work()
    }

    /// Register this invocation's opaque cancellation handle.
    pub fn register_cancellation(&self, handle: Arc<dyn InvocationCancellation>) -> bool {
        self.turn
            .control()
            .register_cancellation(self.invocation_id.to_string(), handle)
    }

    /// Remove this invocation's cancellation handle after terminal completion.
    #[must_use]
    pub fn unregister_cancellation(&self) -> bool {
        self.turn
            .control()
            .unregister_cancellation(&self.invocation_id)
    }

    /// Emit a tool-owned non-terminal lifecycle update.
    ///
    /// Started and terminal stages are orchestration-owned and rejected here so every invocation
    /// has exactly one canonical lifecycle. Tool owners may emit only progress or waiting updates.
    #[must_use]
    pub fn emit_lifecycle(&self, event: ToolInvocationLifecycleEvent) -> bool {
        event.invocation_id == self.invocation_id.as_ref()
            && matches!(
                event.stage,
                bcode_tool::ToolInvocationLifecycleStage::Progress
                    | bcode_tool::ToolInvocationLifecycleStage::Waiting
            )
            && self.turn.emit(ScopedTurnEvent::InvocationLifecycle(event))
    }

    #[must_use]
    pub(crate) fn emit_invocation_terminal(&self, event: ToolInvocationLifecycleEvent) -> bool {
        event.invocation_id == self.invocation_id.as_ref()
            && self.turn.emit_invocation_terminal(event)
    }

    /// Emit cancelled lifecycle bookkeeping after normal invocation output has closed.
    #[must_use]
    pub(crate) fn emit_cancellation_lifecycle(&self, event: ToolInvocationLifecycleEvent) -> bool {
        event.invocation_id == self.invocation_id.as_ref()
            && self.turn.emit_cancellation_lifecycle(event)
    }

    /// Emit a contribution only when it belongs to this active invocation.
    #[must_use]
    pub fn emit_contribution(&self, event: ToolContributionEvent) -> bool {
        event.invocation_id == self.invocation_id.as_ref()
            && self.turn.emit(ScopedTurnEvent::Contribution(event))
    }

    /// Request one correlated external exchange, bounded by turn cancellation.
    ///
    /// Each non-empty exchange ID may be submitted exactly once per invocation scope, including
    /// across clones. Duplicate IDs fail locally and are never forwarded to the host broker. The
    /// returned resolution is terminal for this request.
    pub async fn request_exchange(&self, request: ToolExchangeRequest) -> ToolExchangeResolution {
        let _duration = InvocationOperationDuration::start("exchange");
        if request.invocation_id != self.invocation_id.as_ref() {
            return ToolExchangeResolution::Failed {
                code: "invocation_id_mismatch".to_string(),
                message: "exchange request does not belong to this invocation scope".to_string(),
            };
        }
        if request.exchange_id.is_empty() {
            return ToolExchangeResolution::Failed {
                code: "invalid_exchange_id".to_string(),
                message: "exchange request ID must not be empty".to_string(),
            };
        }
        if !self.accepts_work() {
            return ToolExchangeResolution::Cancelled;
        }
        let inserted = self
            .exchange_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(request.exchange_id.clone());
        if !inserted {
            return ToolExchangeResolution::Failed {
                code: "duplicate_exchange_id".to_string(),
                message: "exchange request ID was already submitted by this invocation".to_string(),
            };
        }
        let cancellation = self.cancellation();
        let resolution = tokio::select! {
            biased;
            () = cancellation.cancelled() => ToolExchangeResolution::Cancelled,
            resolution = self.turn.capabilities.exchanges.request(request) => resolution,
        };
        if self.accepts_work() {
            resolution
        } else {
            ToolExchangeResolution::Cancelled
        }
    }

    /// Wait for one unsolicited input, bounded by turn cancellation.
    pub async fn receive_input(&self) -> ToolInvocationInputResolution {
        let _duration = InvocationOperationDuration::start("input_wait");
        if !self.accepts_work() {
            return ToolInvocationInputResolution::Cancelled;
        }
        let cancellation = self.cancellation();
        let resolution = tokio::select! {
            biased;
            () = cancellation.cancelled() => ToolInvocationInputResolution::Cancelled,
            resolution = self.turn.capabilities.inputs.receive(self.invocation_id()) => {
                match resolution {
                    ToolInvocationInputResolution::Received { input }
                        if input.invocation_id != self.invocation_id.as_ref() =>
                    {
                        ToolInvocationInputResolution::Failed {
                            code: "invocation_id_mismatch".to_string(),
                            message: "received input does not belong to this invocation scope".to_string(),
                        }
                    }
                    resolution => resolution,
                }
            },
        };
        if self.accepts_work() {
            resolution
        } else {
            ToolInvocationInputResolution::Cancelled
        }
    }

    /// Invoke one nested host service, bounded by turn cancellation.
    pub async fn invoke_service(
        &self,
        request: ToolInvocationServiceRequest,
    ) -> ToolInvocationServiceResolution {
        let _duration = InvocationOperationDuration::start("service");
        if request.invocation_id != self.invocation_id.as_ref() {
            return ToolInvocationServiceResolution::Failed {
                code: "invocation_id_mismatch".to_string(),
                message: "service request does not belong to this invocation scope".to_string(),
            };
        }
        if !self.accepts_work() {
            return ToolInvocationServiceResolution::Cancelled;
        }
        let cancellation = self.cancellation();
        let resolution = tokio::select! {
            biased;
            () = cancellation.cancelled() => ToolInvocationServiceResolution::Cancelled,
            resolution = self.turn.capabilities.services.invoke(request) => resolution,
        };
        if self.accepts_work() {
            resolution
        } else {
            ToolInvocationServiceResolution::Cancelled
        }
    }

    /// Create the runtime-owned final-commit gate for this invocation's artifact sink adapter.
    #[must_use]
    pub fn artifact_commit_guard(&self) -> ArtifactCommitGuard {
        ArtifactCommitGuard::new(self.turn.clone(), Arc::clone(&self.invocation_id))
    }

    /// Write one bounded host artifact, bounded by turn cancellation.
    pub async fn write_artifact(
        &self,
        request: ToolArtifactWriteRequest,
    ) -> ToolArtifactWriteResolution {
        let _duration = InvocationOperationDuration::start("artifact");
        if request.invocation_id != self.invocation_id.as_ref() {
            return ToolArtifactWriteResolution::Failed {
                code: "invocation_id_mismatch".to_string(),
                message: "artifact request does not belong to this invocation scope".to_string(),
            };
        }
        if !self.accepts_work() {
            return ToolArtifactWriteResolution::Cancelled;
        }
        let cancellation = self.cancellation();
        let commit = self.artifact_commit_guard();
        let resolution = tokio::select! {
            biased;
            () = cancellation.cancelled() => ToolArtifactWriteResolution::Cancelled,
            resolution = self.turn.capabilities.artifacts.write(request, commit) => resolution,
        };
        if self.accepts_work() {
            resolution
        } else {
            ToolArtifactWriteResolution::Cancelled
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    #[derive(Debug, Default)]
    struct CountingSink(AtomicUsize);

    impl TurnEventSink for CountingSink {
        fn emit(&self, _event: ScopedTurnEvent) -> bool {
            self.0.fetch_add(1, Ordering::SeqCst);
            true
        }
    }

    impl InvocationCancellation for AtomicUsize {
        fn request_cancel(&self) {
            self.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct OrderedPublication {
        calls: Arc<Mutex<Vec<&'static str>>>,
        accepts: bool,
    }

    impl TurnEventSink for OrderedPublication {
        fn emit(&self, _event: ScopedTurnEvent) -> bool {
            self.calls.lock().expect("calls lock").push("publish");
            self.accepts
        }
    }

    struct OrderedPersistence {
        calls: Arc<Mutex<Vec<&'static str>>>,
        accepts: bool,
    }

    impl TurnEventPersistence for OrderedPersistence {
        fn persist(&self, _event: &ScopedTurnEvent) -> bool {
            self.calls.lock().expect("calls lock").push("persist");
            self.accepts
        }
    }

    struct OrderedObservability(Arc<Mutex<Vec<&'static str>>>);

    impl TurnEventObservability for OrderedObservability {
        fn observe(&self, _event: &ScopedTurnEvent) {
            self.0.lock().expect("calls lock").push("observe");
        }
    }

    #[test]
    fn host_event_sink_persists_observes_and_publishes_in_order() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let sink = HostTurnEventSink::new(Arc::new(OrderedPublication {
            calls: Arc::clone(&calls),
            accepts: true,
        }))
        .with_persistence(Arc::new(OrderedPersistence {
            calls: Arc::clone(&calls),
            accepts: true,
        }))
        .with_observability(Arc::new(OrderedObservability(Arc::clone(&calls))));

        assert!(sink.emit(ScopedTurnEvent::Runtime(AgentRuntimeEvent::TurnStarted)));
        assert_eq!(
            *calls.lock().expect("calls lock"),
            vec!["persist", "observe", "publish"]
        );
    }

    fn contribution(persistence: bcode_tool::ToolContributionPersistence) -> ScopedTurnEvent {
        ScopedTurnEvent::Contribution(ToolContributionEvent {
            invocation_id: "invoke".to_string(),
            contribution_id: "surface".to_string(),
            sequence: 1,
            producer_id: "producer".to_string(),
            schema: "example.opaque".to_string(),
            schema_version: 9,
            operation: bcode_tool::ToolContributionOperation::Upsert,
            persistence,
            artifact: None,
            payload: serde_json::json!({"unknown": [1, 2, 3]}),
        })
    }

    #[test]
    fn transient_contribution_bypasses_persistence_but_remains_observable_and_published() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let sink = HostTurnEventSink::new(Arc::new(OrderedPublication {
            calls: Arc::clone(&calls),
            accepts: true,
        }))
        .with_persistence(Arc::new(OrderedPersistence {
            calls: Arc::clone(&calls),
            accepts: false,
        }))
        .with_observability(Arc::new(OrderedObservability(Arc::clone(&calls))));

        assert!(sink.emit(contribution(
            bcode_tool::ToolContributionPersistence::Transient,
        )));
        assert_eq!(
            *calls.lock().expect("calls lock"),
            vec!["observe", "publish"]
        );
    }

    #[test]
    fn durable_contribution_requires_persistence_admission() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let sink = HostTurnEventSink::new(Arc::new(OrderedPublication {
            calls: Arc::clone(&calls),
            accepts: true,
        }))
        .with_persistence(Arc::new(OrderedPersistence {
            calls: Arc::clone(&calls),
            accepts: false,
        }))
        .with_observability(Arc::new(OrderedObservability(Arc::clone(&calls))));

        assert!(!sink.emit(contribution(
            bcode_tool::ToolContributionPersistence::Durable,
        )));
        assert_eq!(*calls.lock().expect("calls lock"), vec!["persist"]);
    }

    #[test]
    fn persistence_rejection_stops_observation_and_publication() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let sink = HostTurnEventSink::new(Arc::new(OrderedPublication {
            calls: Arc::clone(&calls),
            accepts: true,
        }))
        .with_persistence(Arc::new(OrderedPersistence {
            calls: Arc::clone(&calls),
            accepts: false,
        }))
        .with_observability(Arc::new(OrderedObservability(Arc::clone(&calls))));

        assert!(!sink.emit(ScopedTurnEvent::Runtime(AgentRuntimeEvent::TurnStarted)));
        assert_eq!(*calls.lock().expect("calls lock"), vec!["persist"]);
    }

    #[test]
    fn publication_rejection_is_reported_after_persistence_and_observation() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let sink = HostTurnEventSink::new(Arc::new(OrderedPublication {
            calls: Arc::clone(&calls),
            accepts: false,
        }))
        .with_persistence(Arc::new(OrderedPersistence {
            calls: Arc::clone(&calls),
            accepts: true,
        }))
        .with_observability(Arc::new(OrderedObservability(Arc::clone(&calls))));

        assert!(!sink.emit(ScopedTurnEvent::Runtime(AgentRuntimeEvent::TurnStarted)));
        assert_eq!(
            *calls.lock().expect("calls lock"),
            vec!["persist", "observe", "publish"]
        );
    }

    #[test]
    fn cancellation_closes_output_and_signals_all_handles() {
        let sink = Arc::new(CountingSink::default());
        let scope = TurnScope::new("turn", TurnGeneration::new(1), sink.clone());
        let first = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));
        assert!(scope.control.register_cancellation("one", first.clone()));
        assert!(scope.control.register_cancellation("two", second.clone()));
        assert!(scope.emit(ScopedTurnEvent::Runtime(AgentRuntimeEvent::TurnStarted)));

        assert!(scope.control.begin_cancellation());
        assert!(!scope.emit(ScopedTurnEvent::Runtime(AgentRuntimeEvent::TurnStarted)));
        assert_eq!(sink.0.load(Ordering::SeqCst), 1);
        assert_eq!(first.load(Ordering::SeqCst), 1);
        assert_eq!(second.load(Ordering::SeqCst), 1);
        assert!(scope.control.cancellation().is_cancelled());
    }

    #[test]
    fn registration_losing_cancellation_race_is_signalled_immediately() {
        let scope = TurnScope::without_events("turn", TurnGeneration::new(7));
        assert!(scope.control.begin_cancellation());
        let late = Arc::new(AtomicUsize::new(0));

        assert!(!scope.control.register_cancellation("late", late.clone()));
        assert_eq!(late.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn tool_owned_started_and_terminal_lifecycle_stages_are_rejected() {
        let turn = TurnScope::new(
            "turn",
            TurnGeneration::new(1),
            Arc::new(DiscardingTurnEventSink),
        );
        let invocation = InvocationScope::new(turn, "invoke");
        for stage in [
            bcode_tool::ToolInvocationLifecycleStage::Started,
            bcode_tool::ToolInvocationLifecycleStage::Completed,
            bcode_tool::ToolInvocationLifecycleStage::Cancelled,
            bcode_tool::ToolInvocationLifecycleStage::Failed,
        ] {
            assert!(!invocation.emit_lifecycle(ToolInvocationLifecycleEvent {
                invocation_id: "invoke".to_string(),
                sequence: 1,
                stage,
                message: None,
                metadata: serde_json::Value::Null,
            }));
        }
        for stage in [
            bcode_tool::ToolInvocationLifecycleStage::Progress,
            bcode_tool::ToolInvocationLifecycleStage::Waiting,
        ] {
            assert!(invocation.emit_lifecycle(ToolInvocationLifecycleEvent {
                invocation_id: "invoke".to_string(),
                sequence: 1,
                stage,
                message: None,
                metadata: serde_json::Value::Null,
            }));
        }
    }

    #[tokio::test]
    async fn default_invocation_capabilities_are_explicitly_unsupported() {
        let scope = InvocationScope::new(
            TurnScope::without_events("turn", TurnGeneration::new(3)),
            "invoke",
        );

        let exchange = scope
            .request_exchange(ToolExchangeRequest {
                invocation_id: "invoke".to_string(),
                exchange_id: "exchange".to_string(),
                producer_id: "producer".to_string(),
                schema: "example.exchange".to_string(),
                schema_version: 1,
                payload: serde_json::Value::Null,
                response_policy: bcode_tool::ToolExchangeResponsePolicy::Required,
            })
            .await;
        let input = scope.receive_input().await;
        let service = scope
            .invoke_service(ToolInvocationServiceRequest {
                invocation_id: "invoke".to_string(),
                request_id: "request".to_string(),
                route_id: None,
                interface_id: "example.service/v1".to_string(),
                operation: "run".to_string(),
                payload: serde_json::Value::Null,
            })
            .await;
        let artifact = scope
            .write_artifact(ToolArtifactWriteRequest {
                invocation_id: "invoke".to_string(),
                artifact_id: "artifact".to_string(),
                content_type: "application/octet-stream".to_string(),
                bytes: vec![1, 2, 3],
                metadata: serde_json::Value::Null,
            })
            .await;

        assert_eq!(exchange, ToolExchangeResolution::NoCompatibleConsumer);
        assert_eq!(input, ToolInvocationInputResolution::Closed);
        assert_eq!(service, ToolInvocationServiceResolution::Unsupported);
        assert!(matches!(
            artifact,
            ToolArtifactWriteResolution::Failed { ref code, .. }
                if code == "artifact_sink_unavailable"
        ));
    }

    #[derive(Debug, Default)]
    struct BlockingExchangeBroker;

    impl InvocationExchangeBroker for BlockingExchangeBroker {
        fn request(
            &self,
            _request: ToolExchangeRequest,
        ) -> InvocationCapabilityFuture<'_, ToolExchangeResolution> {
            Box::pin(std::future::pending())
        }
    }

    #[tokio::test]
    async fn cancellation_wakes_blocked_exchange_without_broker_completion() {
        let scope = TurnScope::with_capabilities(
            "turn",
            TurnGeneration::new(4),
            Arc::new(DiscardingTurnEventSink),
            InvocationCapabilities::new(
                Arc::new(BlockingExchangeBroker),
                Arc::new(UnsupportedInvocationCapabilities),
                Arc::new(UnsupportedInvocationCapabilities),
                Arc::new(UnsupportedInvocationCapabilities),
            ),
        );
        let invocation = InvocationScope::new(scope.clone(), "invoke");
        let waiting = tokio::spawn(async move {
            invocation
                .request_exchange(ToolExchangeRequest {
                    invocation_id: "invoke".to_string(),
                    exchange_id: "exchange".to_string(),
                    producer_id: "producer".to_string(),
                    schema: "example.exchange".to_string(),
                    schema_version: 1,
                    payload: serde_json::Value::Null,
                    response_policy: bcode_tool::ToolExchangeResponsePolicy::Required,
                })
                .await
        });
        tokio::task::yield_now().await;

        assert!(scope.control().begin_cancellation());
        let resolution = tokio::time::timeout(std::time::Duration::from_millis(100), waiting)
            .await
            .expect("cancellation should wake the exchange")
            .expect("exchange task should not panic");
        assert_eq!(resolution, ToolExchangeResolution::Cancelled);
    }

    #[test]
    fn invocation_scope_rejects_mismatched_event_identity() {
        let sink = Arc::new(CountingSink::default());
        let scope = InvocationScope::new(
            TurnScope::new("turn", TurnGeneration::new(5), sink.clone()),
            "invoke",
        );
        let event = ToolContributionEvent {
            invocation_id: "other".to_string(),
            contribution_id: "contribution".to_string(),
            sequence: 1,
            producer_id: "producer".to_string(),
            schema: "example.contribution".to_string(),
            schema_version: 1,
            operation: bcode_tool::ToolContributionOperation::Upsert,
            persistence: bcode_tool::ToolContributionPersistence::Transient,
            artifact: None,
            payload: serde_json::Value::Null,
        };

        assert!(!scope.emit_contribution(event));
        assert_eq!(sink.0.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn owner_allocates_monotonic_generations_and_closes_previous_scope() {
        let owner = TurnScopeOwner::new();
        let first_sink = Arc::new(CountingSink::default());
        let second_sink = Arc::new(CountingSink::default());
        let first = owner.begin_turn(
            "first",
            first_sink.clone(),
            InvocationCapabilities::default(),
        );
        assert_eq!(first.generation(), TurnGeneration::new(1));
        assert!(first.emit(ScopedTurnEvent::Runtime(AgentRuntimeEvent::TurnStarted)));

        let second = owner.begin_turn(
            "second",
            second_sink.clone(),
            InvocationCapabilities::default(),
        );

        assert_eq!(second.generation(), TurnGeneration::new(2));
        assert_eq!(owner.active_generation(), Some(TurnGeneration::new(2)));
        assert_eq!(first.control().lifecycle(), TurnLifecycle::Cancelling);
        assert!(first.control().cancellation().is_cancelled());
        assert!(!first.accepts_work());
        assert!(!first.emit(ScopedTurnEvent::Runtime(AgentRuntimeEvent::TurnStarted)));
        assert!(second.emit(ScopedTurnEvent::Runtime(AgentRuntimeEvent::TurnStarted)));
        assert_eq!(first_sink.0.load(Ordering::SeqCst), 1);
        assert_eq!(second_sink.0.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn superseding_generation_cancels_blocked_exchange_and_rejects_response() {
        let owner = TurnScopeOwner::new();
        let first = owner.begin_turn(
            "first",
            Arc::new(DiscardingTurnEventSink),
            InvocationCapabilities::new(
                Arc::new(BlockingExchangeBroker),
                Arc::new(UnsupportedInvocationCapabilities),
                Arc::new(UnsupportedInvocationCapabilities),
                Arc::new(UnsupportedInvocationCapabilities),
            ),
        );
        let invocation = InvocationScope::new(first, "invoke");
        let waiting = tokio::spawn(async move {
            invocation
                .request_exchange(ToolExchangeRequest {
                    invocation_id: "invoke".to_string(),
                    exchange_id: "exchange".to_string(),
                    producer_id: "producer".to_string(),
                    schema: "example.exchange".to_string(),
                    schema_version: 1,
                    payload: serde_json::Value::Null,
                    response_policy: bcode_tool::ToolExchangeResponsePolicy::Required,
                })
                .await
        });
        tokio::task::yield_now().await;

        let _second = owner.begin_turn(
            "second",
            Arc::new(DiscardingTurnEventSink),
            InvocationCapabilities::default(),
        );
        let resolution = tokio::time::timeout(std::time::Duration::from_millis(100), waiting)
            .await
            .expect("superseding generation should wake the exchange")
            .expect("exchange task should not panic");

        assert_eq!(resolution, ToolExchangeResolution::Cancelled);
    }

    #[derive(Debug, Default)]
    struct CountingExchangeBroker(AtomicUsize);

    impl InvocationExchangeBroker for CountingExchangeBroker {
        fn request(
            &self,
            request: ToolExchangeRequest,
        ) -> InvocationCapabilityFuture<'_, ToolExchangeResolution> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                ToolExchangeResolution::Responded {
                    payload: serde_json::json!({"exchange_id": request.exchange_id}),
                }
            })
        }
    }

    #[tokio::test]
    async fn exchange_ids_are_forwarded_exactly_once_across_scope_clones() {
        let broker = Arc::new(CountingExchangeBroker::default());
        let scope = InvocationScope::new(
            TurnScope::with_capabilities(
                "turn",
                TurnGeneration::new(6),
                Arc::new(DiscardingTurnEventSink),
                InvocationCapabilities::new(
                    broker.clone(),
                    Arc::new(UnsupportedInvocationCapabilities),
                    Arc::new(UnsupportedInvocationCapabilities),
                    Arc::new(UnsupportedInvocationCapabilities),
                ),
            ),
            "invoke",
        );
        let request = ToolExchangeRequest {
            invocation_id: "invoke".to_string(),
            exchange_id: "exchange".to_string(),
            producer_id: "producer".to_string(),
            schema: "example.exchange".to_string(),
            schema_version: 1,
            payload: serde_json::Value::Null,
            response_policy: bcode_tool::ToolExchangeResponsePolicy::Required,
        };

        assert!(matches!(
            scope.request_exchange(request.clone()).await,
            ToolExchangeResolution::Responded { .. }
        ));
        let duplicate = scope.clone().request_exchange(request).await;

        assert!(matches!(
            duplicate,
            ToolExchangeResolution::Failed { ref code, .. }
                if code == "duplicate_exchange_id"
        ));
        assert_eq!(broker.0.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn empty_exchange_id_is_rejected_without_broker_invocation() {
        let broker = Arc::new(CountingExchangeBroker::default());
        let scope = InvocationScope::new(
            TurnScope::with_capabilities(
                "turn",
                TurnGeneration::new(7),
                Arc::new(DiscardingTurnEventSink),
                InvocationCapabilities::new(
                    broker.clone(),
                    Arc::new(UnsupportedInvocationCapabilities),
                    Arc::new(UnsupportedInvocationCapabilities),
                    Arc::new(UnsupportedInvocationCapabilities),
                ),
            ),
            "invoke",
        );

        let resolution = scope
            .request_exchange(ToolExchangeRequest {
                invocation_id: "invoke".to_string(),
                exchange_id: String::new(),
                producer_id: "producer".to_string(),
                schema: "example.exchange".to_string(),
                schema_version: 1,
                payload: serde_json::Value::Null,
                response_policy: bcode_tool::ToolExchangeResponsePolicy::Required,
            })
            .await;

        assert!(matches!(
            resolution,
            ToolExchangeResolution::Failed { ref code, .. }
                if code == "invalid_exchange_id"
        ));
        assert_eq!(broker.0.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn removing_active_turn_keeps_scope_closed_and_stale_scope_cannot_remove_newer_turn() {
        let owner = TurnScopeOwner::new();
        let first = owner.begin_turn(
            "first",
            Arc::new(DiscardingTurnEventSink),
            InvocationCapabilities::default(),
        );

        assert!(owner.complete_turn(&first));
        assert_eq!(first.control().lifecycle(), TurnLifecycle::Completed);
        assert_eq!(owner.active_generation(), None);
        assert!(!first.accepts_work());
        assert!(!first.emit(ScopedTurnEvent::Runtime(AgentRuntimeEvent::TurnStarted)));

        let second = owner.begin_turn(
            "second",
            Arc::new(DiscardingTurnEventSink),
            InvocationCapabilities::default(),
        );
        assert!(!owner.complete_turn(&first));
        assert_eq!(owner.active_generation(), Some(second.generation()));
        assert!(second.accepts_work());
    }

    #[test]
    fn terminal_cancellation_must_precede_owner_release() {
        let owner = TurnScopeOwner::new();
        let scope = owner.begin_turn(
            "turn",
            Arc::new(DiscardingTurnEventSink),
            InvocationCapabilities::default(),
        );

        assert!(!owner.release_terminal_turn(&scope));
        assert!(scope.control().begin_cancellation());
        assert!(!owner.release_terminal_turn(&scope));
        assert!(scope.control().mark_cancelled());
        assert!(owner.release_terminal_turn(&scope));
        assert_eq!(owner.active_generation(), None);
        assert!(!scope.accepts_work());
    }

    #[test]
    fn cancellation_bookkeeping_is_the_only_event_allowed_after_normal_closure() {
        let sink = Arc::new(CountingSink::default());
        let turn = TurnScope::new("turn", TurnGeneration::new(8), sink.clone());
        let invocation = InvocationScope::new(turn.clone(), "invoke");
        assert!(turn.control().begin_cancellation());

        assert!(!invocation.emit_contribution(ToolContributionEvent {
            invocation_id: "invoke".to_string(),
            contribution_id: "late".to_string(),
            sequence: 1,
            producer_id: "producer".to_string(),
            schema: "example.contribution".to_string(),
            schema_version: 1,
            operation: bcode_tool::ToolContributionOperation::Upsert,
            persistence: bcode_tool::ToolContributionPersistence::Transient,
            artifact: None,
            payload: serde_json::Value::Null,
        }));
        assert!(
            !invocation.emit_cancellation_lifecycle(ToolInvocationLifecycleEvent {
                invocation_id: "invoke".to_string(),
                sequence: 1,
                stage: bcode_tool::ToolInvocationLifecycleStage::Completed,
                message: None,
                metadata: serde_json::Value::Null,
            })
        );
        assert!(
            invocation.emit_cancellation_lifecycle(ToolInvocationLifecycleEvent {
                invocation_id: "invoke".to_string(),
                sequence: 2,
                stage: bcode_tool::ToolInvocationLifecycleStage::Cancelled,
                message: None,
                metadata: serde_json::Value::Null,
            })
        );

        assert_eq!(sink.0.load(Ordering::SeqCst), 1);
        assert_eq!(turn.control().discarded_normal_event_count(), 1);
    }

    #[test]
    fn stale_generation_rejections_increment_discard_count_without_payload_inspection() {
        let owner = TurnScopeOwner::new();
        let first = owner.begin_turn(
            "first",
            Arc::new(DiscardingTurnEventSink),
            InvocationCapabilities::default(),
        );
        let _second = owner.begin_turn(
            "second",
            Arc::new(DiscardingTurnEventSink),
            InvocationCapabilities::default(),
        );

        assert!(!first.emit(ScopedTurnEvent::Runtime(AgentRuntimeEvent::TurnStarted)));
        assert_eq!(first.control().discarded_normal_event_count(), 1);
    }

    #[derive(Debug, Default)]
    struct BlockingInvocationCapabilities;

    impl InvocationInputRouter for BlockingInvocationCapabilities {
        fn receive(
            &self,
            _invocation_id: &str,
        ) -> InvocationCapabilityFuture<'_, ToolInvocationInputResolution> {
            Box::pin(std::future::pending())
        }
    }

    impl InvocationServiceRouter for BlockingInvocationCapabilities {
        fn invoke(
            &self,
            _request: ToolInvocationServiceRequest,
        ) -> InvocationCapabilityFuture<'_, ToolInvocationServiceResolution> {
            Box::pin(std::future::pending())
        }
    }

    impl InvocationArtifactSink for BlockingInvocationCapabilities {
        fn write(
            &self,
            _request: ToolArtifactWriteRequest,
            _commit: ArtifactCommitGuard,
        ) -> InvocationCapabilityFuture<'_, ToolArtifactWriteResolution> {
            Box::pin(std::future::pending())
        }
    }

    #[test]
    fn artifact_commit_guard_rejects_commit_after_turn_closes() {
        let turn = TurnScope::without_events("turn", TurnGeneration::new(20));
        let invocation = InvocationScope::new(turn.clone(), "invoke");
        let guard = invocation.artifact_commit_guard();
        assert!(turn.control().begin_cancellation());
        let commits = AtomicUsize::new(0);

        let result = guard.commit(|| commits.fetch_add(1, Ordering::SeqCst));

        assert_eq!(result, None);
        assert_eq!(commits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn artifact_commit_guard_serializes_cancellation_after_publication() {
        let turn = TurnScope::without_events("turn", TurnGeneration::new(21));
        let invocation = InvocationScope::new(turn.clone(), "invoke");
        let guard = invocation.artifact_commit_guard();
        let control = turn.control();
        let (started, started_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let commit = std::thread::spawn(move || {
            guard.commit(|| {
                started.send(()).expect("signal commit start");
                release_rx.recv().expect("release commit");
                "published"
            })
        });
        started_rx.recv().expect("commit should start");
        let (attempting, attempting_rx) = std::sync::mpsc::channel();
        let cancellation = std::thread::spawn(move || {
            attempting.send(()).expect("signal cancellation attempt");
            control.begin_cancellation()
        });
        attempting_rx
            .recv()
            .expect("cancellation should reach publication gate");
        std::thread::yield_now();
        assert!(!cancellation.is_finished());

        release.send(()).expect("release commit");
        assert_eq!(commit.join().expect("commit thread"), Some("published"));
        assert!(cancellation.join().expect("cancellation thread"));
    }

    #[test]
    fn artifact_commit_guard_serializes_generation_supersession_after_publication() {
        let owner = TurnScopeOwner::new();
        let turn = owner.begin_turn(
            "old",
            Arc::new(DiscardingTurnEventSink),
            InvocationCapabilities::default(),
        );
        let invocation = InvocationScope::new(turn, "invoke");
        let guard = invocation.artifact_commit_guard();
        let (started, started_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let commit = std::thread::spawn(move || {
            guard.commit(|| {
                started.send(()).expect("signal commit start");
                release_rx.recv().expect("release commit");
                "published"
            })
        });
        started_rx.recv().expect("commit should start");
        let superseding_owner = owner.clone();
        let (attempting, attempting_rx) = std::sync::mpsc::channel();
        let supersede = std::thread::spawn(move || {
            attempting.send(()).expect("signal supersession attempt");
            superseding_owner.begin_turn(
                "new",
                Arc::new(DiscardingTurnEventSink),
                InvocationCapabilities::default(),
            )
        });
        attempting_rx
            .recv()
            .expect("supersession should reach publication gate");
        std::thread::yield_now();
        assert!(!supersede.is_finished());

        release.send(()).expect("release commit");
        assert_eq!(commit.join().expect("commit thread"), Some("published"));
        let new_scope = supersede.join().expect("supersession thread");
        assert_eq!(
            new_scope.generation(),
            owner.active_generation().expect("active generation")
        );
    }

    #[tokio::test]
    async fn cancellation_wakes_blocked_input_service_and_artifact_operations() {
        let blocking = Arc::new(BlockingInvocationCapabilities);
        let turn = TurnScope::with_capabilities(
            "turn",
            TurnGeneration::new(9),
            Arc::new(DiscardingTurnEventSink),
            InvocationCapabilities::new(
                Arc::new(UnsupportedInvocationCapabilities),
                blocking.clone(),
                blocking.clone(),
                blocking,
            ),
        );
        let input_scope = InvocationScope::new(turn.clone(), "invoke");
        let service_scope = input_scope.clone();
        let artifact_scope = input_scope.clone();
        let input = tokio::spawn(async move { input_scope.receive_input().await });
        let service = tokio::spawn(async move {
            service_scope
                .invoke_service(ToolInvocationServiceRequest {
                    invocation_id: "invoke".to_string(),
                    request_id: "service".to_string(),
                    route_id: None,
                    interface_id: "example.service/v1".to_string(),
                    operation: "run".to_string(),
                    payload: serde_json::Value::Null,
                })
                .await
        });
        let artifact = tokio::spawn(async move {
            artifact_scope
                .write_artifact(ToolArtifactWriteRequest {
                    invocation_id: "invoke".to_string(),
                    artifact_id: "artifact".to_string(),
                    content_type: "application/octet-stream".to_string(),
                    bytes: vec![1, 2, 3],
                    metadata: serde_json::Value::Null,
                })
                .await
        });
        tokio::task::yield_now().await;

        assert!(turn.control().begin_cancellation());
        let input = tokio::time::timeout(std::time::Duration::from_millis(100), input)
            .await
            .expect("input wait should wake")
            .expect("input task should not panic");
        let service = tokio::time::timeout(std::time::Duration::from_millis(100), service)
            .await
            .expect("service wait should wake")
            .expect("service task should not panic");
        let artifact = tokio::time::timeout(std::time::Duration::from_millis(100), artifact)
            .await
            .expect("artifact wait should wake")
            .expect("artifact task should not panic");

        assert_eq!(input, ToolInvocationInputResolution::Cancelled);
        assert_eq!(service, ToolInvocationServiceResolution::Cancelled);
        assert_eq!(artifact, ToolArtifactWriteResolution::Cancelled);
    }

    struct LateResolutionCapabilities {
        exchange: Mutex<Option<tokio::sync::oneshot::Receiver<ToolExchangeResolution>>>,
        input: Mutex<Option<tokio::sync::oneshot::Receiver<ToolInvocationInputResolution>>>,
        service: Mutex<Option<tokio::sync::oneshot::Receiver<ToolInvocationServiceResolution>>>,
    }

    impl InvocationExchangeBroker for LateResolutionCapabilities {
        fn request(
            &self,
            _request: ToolExchangeRequest,
        ) -> InvocationCapabilityFuture<'_, ToolExchangeResolution> {
            let receiver = self
                .exchange
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .expect("one exchange wait");
            Box::pin(async move {
                receiver
                    .await
                    .unwrap_or(ToolExchangeResolution::ConsumerDetached)
            })
        }
    }

    impl InvocationInputRouter for LateResolutionCapabilities {
        fn receive(
            &self,
            _invocation_id: &str,
        ) -> InvocationCapabilityFuture<'_, ToolInvocationInputResolution> {
            let receiver = self
                .input
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .expect("one input wait");
            Box::pin(async move {
                receiver
                    .await
                    .unwrap_or(ToolInvocationInputResolution::Closed)
            })
        }
    }

    impl InvocationServiceRouter for LateResolutionCapabilities {
        fn invoke(
            &self,
            _request: ToolInvocationServiceRequest,
        ) -> InvocationCapabilityFuture<'_, ToolInvocationServiceResolution> {
            let receiver = self
                .service
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .expect("one service wait");
            Box::pin(async move {
                receiver
                    .await
                    .unwrap_or(ToolInvocationServiceResolution::Unsupported)
            })
        }
    }

    #[tokio::test]
    async fn late_exchange_input_and_service_responses_cannot_revive_cancelled_turn() {
        let (exchange_tx, exchange_rx) = tokio::sync::oneshot::channel();
        let (input_tx, input_rx) = tokio::sync::oneshot::channel();
        let (service_tx, service_rx) = tokio::sync::oneshot::channel();
        let capabilities = Arc::new(LateResolutionCapabilities {
            exchange: Mutex::new(Some(exchange_rx)),
            input: Mutex::new(Some(input_rx)),
            service: Mutex::new(Some(service_rx)),
        });
        let turn = TurnScope::with_capabilities(
            "turn",
            TurnGeneration::new(1),
            Arc::new(DiscardingTurnEventSink),
            InvocationCapabilities::new(
                capabilities.clone(),
                capabilities.clone(),
                capabilities,
                Arc::new(UnsupportedInvocationCapabilities),
            ),
        );
        let invocation = InvocationScope::new(turn.clone(), "invoke");
        let exchange_scope = invocation.clone();
        let exchange = tokio::spawn(async move {
            exchange_scope
                .request_exchange(ToolExchangeRequest {
                    invocation_id: "invoke".to_string(),
                    exchange_id: "exchange".to_string(),
                    producer_id: "producer".to_string(),
                    schema: "example.exchange".to_string(),
                    schema_version: 1,
                    payload: serde_json::Value::Null,
                    response_policy: bcode_tool::ToolExchangeResponsePolicy::Required,
                })
                .await
        });
        let input_scope = invocation.clone();
        let input = tokio::spawn(async move { input_scope.receive_input().await });
        let service = tokio::spawn(async move {
            invocation
                .invoke_service(ToolInvocationServiceRequest {
                    invocation_id: "invoke".to_string(),
                    request_id: "service".to_string(),
                    route_id: None,
                    interface_id: "example.service/v1".to_string(),
                    operation: "run".to_string(),
                    payload: serde_json::Value::Null,
                })
                .await
        });
        tokio::task::yield_now().await;

        assert!(turn.control().begin_cancellation());
        assert_eq!(
            exchange.await.expect("exchange task"),
            ToolExchangeResolution::Cancelled
        );
        assert_eq!(
            input.await.expect("input task"),
            ToolInvocationInputResolution::Cancelled
        );
        assert_eq!(
            service.await.expect("service task"),
            ToolInvocationServiceResolution::Cancelled
        );
        assert!(!turn.accepts_work());

        assert!(
            exchange_tx
                .send(ToolExchangeResolution::Responded {
                    payload: serde_json::json!({"late": true}),
                })
                .is_err()
        );
        assert!(
            input_tx
                .send(ToolInvocationInputResolution::Received {
                    input: bcode_tool::ToolInvocationInput {
                        invocation_id: "invoke".to_string(),
                        input_id: "late".to_string(),
                        producer_id: "producer".to_string(),
                        schema: "example.input".to_string(),
                        schema_version: 1,
                        payload: serde_json::Value::Null,
                    },
                })
                .is_err()
        );
        assert!(
            service_tx
                .send(ToolInvocationServiceResolution::Responded {
                    payload: serde_json::json!({"late": true}),
                })
                .is_err()
        );
    }
    #[test]
    fn normal_completion_closes_output_without_requesting_cancellation() {
        let sink = Arc::new(CountingSink::default());
        let scope = TurnScope::new("turn", TurnGeneration::new(2), sink);

        assert!(scope.control.complete());
        assert_eq!(scope.control.lifecycle(), TurnLifecycle::Completed);
        assert!(!scope.control.cancellation().is_cancelled());
        assert!(!scope.emit(ScopedTurnEvent::Runtime(AgentRuntimeEvent::TurnStarted)));
    }
}
