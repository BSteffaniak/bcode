//! Turn-scoped lifecycle, cancellation, and event publication primitives.

use crate::{AgentRuntimeEvent, CancellationToken};
use bcode_tool::{
    ToolArtifactWriteRequest, ToolArtifactWriteResolution, ToolContributionEvent,
    ToolExchangeRequest, ToolExchangeResolution, ToolInvocationInputResolution,
    ToolInvocationLifecycleEvent, ToolInvocationServiceRequest, ToolInvocationServiceResolution,
};
use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

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
    fn emit(&self, event: ScopedTurnEvent);
}

#[derive(Debug, Default)]
struct DiscardingTurnEventSink;

impl TurnEventSink for DiscardingTurnEventSink {
    fn emit(&self, _event: ScopedTurnEvent) {}
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

/// Host-owned bounded artifact sink for active invocations.
pub trait InvocationArtifactSink: Send + Sync {
    /// Persist one complete bounded artifact.
    fn write(
        &self,
        request: ToolArtifactWriteRequest,
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
}

/// Shared lifecycle and cancellation state for one turn generation.
pub struct TurnControl {
    lifecycle: AtomicU8,
    cancellation: CancellationToken,
    publication_gate: Mutex<()>,
    cancellations: Mutex<BTreeMap<String, Arc<dyn InvocationCancellation>>>,
}

impl fmt::Debug for TurnControl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnControl")
            .field("lifecycle", &self.lifecycle())
            .field("registered_cancellations", &self.cancellation_count())
            .finish_non_exhaustive()
    }
}

impl Default for TurnControl {
    fn default() -> Self {
        Self {
            lifecycle: AtomicU8::new(TurnLifecycle::RUNNING),
            cancellation: CancellationToken::new(),
            publication_gate: Mutex::new(()),
            cancellations: Mutex::new(BTreeMap::new()),
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
        let handles = {
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
                return false;
            }
            self.cancellation.cancel();
            let mut cancellations = self
                .cancellations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *cancellations)
                .into_values()
                .collect::<Vec<_>>()
        };
        for handle in handles {
            handle.request_cancel();
        }
        true
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

    fn emit(&self, sink: &dyn TurnEventSink, event: ScopedTurnEvent) -> bool {
        let _gate = self
            .publication_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.lifecycle() != TurnLifecycle::Running {
            return false;
        }
        sink.emit(event);
        true
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

    /// Emit a normal event if cancellation or completion has not closed this scope.
    #[must_use]
    pub fn emit(&self, event: ScopedTurnEvent) -> bool {
        self.control.emit(self.events.as_ref(), event)
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
        self.turn.control().accepts_normal_output()
    }
}

/// Active invocation context derived from one turn generation.
#[derive(Clone)]
pub struct InvocationScope {
    turn: TurnScope,
    invocation_id: Arc<str>,
}

impl fmt::Debug for InvocationScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InvocationScope")
            .field("invocation_id", &self.invocation_id)
            .field("turn", &self.turn)
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
        self.turn.control().accepts_normal_output()
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

    /// Emit lifecycle output only when it belongs to this active invocation.
    #[must_use]
    pub fn emit_lifecycle(&self, event: ToolInvocationLifecycleEvent) -> bool {
        event.invocation_id == self.invocation_id.as_ref()
            && self.turn.emit(ScopedTurnEvent::InvocationLifecycle(event))
    }

    /// Emit a contribution only when it belongs to this active invocation.
    #[must_use]
    pub fn emit_contribution(&self, event: ToolContributionEvent) -> bool {
        event.invocation_id == self.invocation_id.as_ref()
            && self.turn.emit(ScopedTurnEvent::Contribution(event))
    }

    /// Request one correlated external exchange, bounded by turn cancellation.
    pub async fn request_exchange(&self, request: ToolExchangeRequest) -> ToolExchangeResolution {
        if request.invocation_id != self.invocation_id.as_ref() {
            return ToolExchangeResolution::Failed {
                code: "invocation_id_mismatch".to_string(),
                message: "exchange request does not belong to this invocation scope".to_string(),
            };
        }
        if !self.accepts_work() {
            return ToolExchangeResolution::Cancelled;
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

    /// Write one bounded host artifact, bounded by turn cancellation.
    pub async fn write_artifact(
        &self,
        request: ToolArtifactWriteRequest,
    ) -> ToolArtifactWriteResolution {
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
        let resolution = tokio::select! {
            biased;
            () = cancellation.cancelled() => ToolArtifactWriteResolution::Cancelled,
            resolution = self.turn.capabilities.artifacts.write(request) => resolution,
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug, Default)]
    struct CountingSink(AtomicUsize);

    impl TurnEventSink for CountingSink {
        fn emit(&self, _event: ScopedTurnEvent) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl InvocationCancellation for AtomicUsize {
        fn request_cancel(&self) {
            self.fetch_add(1, Ordering::SeqCst);
        }
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
            payload: serde_json::Value::Null,
        };

        assert!(!scope.emit_contribution(event));
        assert_eq!(sink.0.load(Ordering::SeqCst), 0);
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
