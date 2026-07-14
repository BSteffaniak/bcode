//! Turn-scoped lifecycle, cancellation, and event publication primitives.

use crate::{AgentRuntimeEvent, CancellationToken};
use bcode_tool::{ToolContributionEvent, ToolInvocationLifecycleEvent};
use std::collections::BTreeMap;
use std::fmt;
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
        Self {
            turn_id: turn_id.into(),
            generation,
            control: Arc::new(TurnControl::new()),
            events,
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
