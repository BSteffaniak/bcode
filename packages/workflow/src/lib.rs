#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Typed workflow composition and lean in-process execution for Bcode.
//!
//! Workflows are assembled from typed [`Step`] values. The type system checks data flow while the
//! builder records a serializable [`WorkflowDefinition`] for inspection and future durable hosts.
//! Execution is intentionally host-neutral: agent, plugin, and application behavior enters through
//! ordinary typed steps instead of scheduler-specific branches.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::task::{JoinHandle, JoinSet};

/// Boxed asynchronous workflow operation.
pub type StepFuture<T> = Pin<Box<dyn Future<Output = Result<T, WorkflowError>> + Send>>;

type StepFn<I, O> = dyn Fn(I, StepContext) -> StepFuture<O> + Send + Sync;

const DEFAULT_MAX_CONCURRENCY: usize = Semaphore::MAX_PERMITS;

/// Stable workflow definition schema version.
pub const WORKFLOW_DEFINITION_SCHEMA_VERSION: u32 = 1;

/// Error returned while compiling or running a workflow.
#[derive(Debug, Error)]
pub enum WorkflowError {
    /// The workflow definition is invalid.
    #[error("workflow build failed at '{path}': {message}")]
    Build {
        /// Logical location associated with the error.
        path: String,
        /// Actionable validation message.
        message: String,
    },
    /// A named step failed.
    #[error("workflow step '{step}' failed: {message}")]
    Step {
        /// Stable step name.
        step: String,
        /// Step-owned failure message.
        message: String,
    },
    /// A step returned data that did not match its declared schema or Rust output type.
    #[error("workflow step '{step}' returned invalid output: {message}")]
    InvalidOutput {
        /// Stable step name.
        step: String,
        /// Validation or decoding failure.
        message: String,
    },
    /// Workflow cancellation was observed at a step boundary.
    #[error("workflow cancelled before step '{step}'")]
    Cancelled {
        /// Step that could not start or complete normally.
        step: String,
    },
    /// A step exceeded its configured timeout.
    #[error("workflow step '{step}' timed out after {timeout:?}")]
    TimedOut {
        /// Stable step name.
        step: String,
        /// Configured timeout.
        timeout: Duration,
    },
    /// A bounded retry policy exhausted all attempts.
    #[error("workflow step '{step}' exhausted {attempts} attempts: {errors:?}")]
    RetryExhausted {
        /// Stable retry-controller name.
        step: String,
        /// Total attempts executed.
        attempts: u32,
        /// Ordered error messages from each failed attempt.
        errors: Vec<String>,
    },
}

impl WorkflowError {
    /// Create a step-scoped application error.
    #[must_use]
    pub fn step(step: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Step {
            step: step.into(),
            message: message.into(),
        }
    }
}

/// Cloneable cancellation state shared by a workflow and all of its steps.
#[derive(Debug, Clone, Default)]
pub struct WorkflowCancellation {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl WorkflowCancellation {
    /// Create an uncancelled workflow token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Return whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Wait until cancellation is requested.
    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let notified = self.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// Event emitted by a running in-process workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowEvent {
    /// A named step started.
    StepStarted { step: String },
    /// A named step completed.
    StepCompleted { step: String },
    /// A named step is waiting to acquire its declared resources.
    StepWaitingForResources {
        step: String,
        resources: Vec<ResourceClaim>,
    },
    /// A named step is waiting for workflow execution capacity.
    StepWaitingForConcurrency { step: String },
    /// A named step failed.
    StepFailed { step: String, message: String },
    /// A retry attempt started.
    RetryAttempt {
        step: String,
        attempt: u32,
        max_attempts: u32,
    },
    /// One bounded-cycle iteration started.
    IterationStarted {
        step: String,
        iteration: u32,
        max_iterations: u32,
    },
    /// The complete workflow reached a terminal outcome.
    WorkflowFinished { outcome: WorkflowOutcome },
}

/// Current lifecycle state for one compiled workflow node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeRunState {
    Pending,
    Ready,
    WaitingForConcurrency,
    WaitingForResources,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    Skipped,
}

/// Incrementally maintained in-memory workflow run snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRunSnapshot {
    /// Current state for every compiled node.
    pub nodes: BTreeMap<String, NodeRunState>,
    /// Ready node identities.
    pub ready: BTreeSet<String>,
    /// Nodes waiting for resources.
    pub waiting: BTreeSet<String>,
    /// Running node identities.
    pub running: BTreeSet<String>,
    /// Terminal node identities.
    pub terminal: BTreeSet<String>,
    /// Current holder count for each resource.
    pub resource_holders: BTreeMap<String, usize>,
}

impl WorkflowRunSnapshot {
    fn new(plan: &WorkflowPlan) -> Self {
        let nodes = plan
            .dependencies
            .iter()
            .map(|(id, dependencies)| {
                (
                    id.clone(),
                    if *dependencies == 0 {
                        NodeRunState::Ready
                    } else {
                        NodeRunState::Pending
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let ready = plan
            .dependencies
            .iter()
            .filter_map(|(id, dependencies)| (*dependencies == 0).then_some(id.clone()))
            .collect();
        Self {
            nodes,
            ready,
            waiting: BTreeSet::new(),
            running: BTreeSet::new(),
            terminal: BTreeSet::new(),
            resource_holders: BTreeMap::new(),
        }
    }
}

#[derive(Debug)]
struct RunStateTracker {
    snapshot: StdMutex<WorkflowRunSnapshot>,
    incomplete: StdMutex<BTreeSet<String>>,
}

impl RunStateTracker {
    fn new(plan: &WorkflowPlan) -> Self {
        Self {
            snapshot: StdMutex::new(WorkflowRunSnapshot::new(plan)),
            incomplete: StdMutex::new(plan.dependencies.keys().cloned().collect()),
        }
    }

    fn snapshot(&self) -> WorkflowRunSnapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn transition(&self, node: &str, state: NodeRunState) {
        let mut snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snapshot.ready.remove(node);
        snapshot.waiting.remove(node);
        snapshot.running.remove(node);
        snapshot.terminal.remove(node);
        match state {
            NodeRunState::Pending => {}
            NodeRunState::Ready => {
                snapshot.ready.insert(node.to_string());
            }
            NodeRunState::WaitingForConcurrency | NodeRunState::WaitingForResources => {
                snapshot.waiting.insert(node.to_string());
            }
            NodeRunState::Running => {
                snapshot.running.insert(node.to_string());
            }
            NodeRunState::Succeeded
            | NodeRunState::Failed
            | NodeRunState::Cancelled
            | NodeRunState::TimedOut
            | NodeRunState::Skipped => {
                snapshot.terminal.insert(node.to_string());
                self.incomplete
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(node);
            }
        }
        snapshot.nodes.insert(node.to_string(), state);
    }

    fn finish_incomplete(&self, outcome: WorkflowOutcome) {
        let incomplete = std::mem::take(
            &mut *self
                .incomplete
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let mut snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let replacement = match outcome {
            WorkflowOutcome::Succeeded | WorkflowOutcome::Failed => NodeRunState::Skipped,
            WorkflowOutcome::Cancelled => NodeRunState::Cancelled,
            WorkflowOutcome::TimedOut => NodeRunState::TimedOut,
        };
        for node in incomplete {
            snapshot.ready.remove(&node);
            snapshot.waiting.remove(&node);
            snapshot.running.remove(&node);
            snapshot.terminal.insert(node.clone());
            snapshot.nodes.insert(node, replacement);
        }
    }

    fn resource_acquired(&self, resources: &[ResourceClaim]) {
        let mut snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for resource in resources {
            *snapshot
                .resource_holders
                .entry(resource.resource.clone())
                .or_default() += 1;
        }
    }

    fn resource_released(&self, resources: &[ResourceClaim]) {
        let mut snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for resource in resources {
            let remove = snapshot
                .resource_holders
                .get_mut(&resource.resource)
                .is_some_and(|holders| {
                    *holders = holders.saturating_sub(1);
                    *holders == 0
                });
            if remove {
                snapshot.resource_holders.remove(&resource.resource);
            }
        }
    }
}

/// Cloneable observer for one in-process workflow run.
#[derive(Debug, Clone)]
pub struct WorkflowRunObserver {
    plan: WorkflowPlan,
    tracker: Arc<RunStateTracker>,
}

impl WorkflowRunObserver {
    fn new(plan: &WorkflowPlan) -> Self {
        Self {
            plan: plan.clone(),
            tracker: Arc::new(RunStateTracker::new(plan)),
        }
    }

    /// Return the current incrementally maintained run snapshot.
    #[must_use]
    pub fn snapshot(&self) -> WorkflowRunSnapshot {
        self.tracker.snapshot()
    }
}

#[derive(Debug)]
struct ConcurrencyCoordinator {
    permits: Arc<Semaphore>,
}

impl ConcurrencyCoordinator {
    fn new(max_concurrency: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(max_concurrency)),
        }
    }

    async fn acquire(
        &self,
        node: &str,
        context: &StepContext,
    ) -> Result<OwnedSemaphorePermit, WorkflowError> {
        context.ensure_active(node.to_string())?;
        let permit = match Arc::clone(&self.permits).try_acquire_owned() {
            Ok(permit) => permit,
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                context.transition(node, NodeRunState::WaitingForConcurrency);
                context.emit(WorkflowEvent::StepWaitingForConcurrency {
                    step: node.to_string(),
                });
                tokio::select! {
                    result = Arc::clone(&self.permits).acquire_owned() => {
                        result.expect("workflow concurrency semaphore remains open")
                    }
                    () = context.cancellation.cancelled() => {
                        return Err(WorkflowError::Cancelled { step: node.to_string() });
                    }
                }
            }
            Err(tokio::sync::TryAcquireError::Closed) => {
                unreachable!("workflow concurrency semaphore remains open")
            }
        };
        context.ensure_active(node.to_string())?;
        Ok(permit)
    }
}

#[derive(Debug, Default)]
struct ResourceState {
    readers: usize,
    writer: bool,
}

#[derive(Debug, Default)]
struct ResourceCoordinator {
    state: StdMutex<BTreeMap<String, ResourceState>>,
    changed: Notify,
}

impl ResourceCoordinator {
    async fn acquire(
        self: &Arc<Self>,
        node: &str,
        claims: &[ResourceClaim],
        context: &StepContext,
    ) -> Result<Option<ResourceLease>, WorkflowError> {
        if claims.is_empty() {
            return Ok(None);
        }
        loop {
            context.ensure_active(node.to_string())?;
            let notified = self.changed.notified();
            let acquired = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if claims_available(&state, claims) {
                    apply_claims(&mut state, claims);
                    true
                } else {
                    false
                }
            };
            if acquired {
                context.tracker.resource_acquired(claims);
                return Ok(Some(ResourceLease {
                    coordinator: Some(Arc::clone(self)),
                    tracker: Arc::clone(&context.tracker),
                    claims: claims.to_vec(),
                }));
            }
            context.transition(node, NodeRunState::WaitingForResources);
            context.emit(WorkflowEvent::StepWaitingForResources {
                step: node.to_string(),
                resources: claims.to_vec(),
            });
            tokio::select! {
                () = notified => {}
                () = context.cancellation.cancelled() => {
                    return Err(WorkflowError::Cancelled { step: node.to_string() });
                }
            }
        }
    }

    fn release(&self, claims: &[ResourceClaim], tracker: &RunStateTracker) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for claim in claims {
            if let Some(resource) = state.get_mut(&claim.resource) {
                match claim.access {
                    ResourceAccess::Read => resource.readers = resource.readers.saturating_sub(1),
                    ResourceAccess::Write => resource.writer = false,
                }
                if resource.readers == 0 && !resource.writer {
                    state.remove(&claim.resource);
                }
            }
        }
        drop(state);
        tracker.resource_released(claims);
        self.changed.notify_waiters();
    }
}

fn normalize_resource_claims(
    claims: impl IntoIterator<Item = ResourceClaim>,
) -> Result<Vec<ResourceClaim>, WorkflowError> {
    let mut normalized = BTreeMap::<String, ResourceAccess>::new();
    for claim in claims {
        let resource = claim.resource.trim();
        if resource.is_empty() {
            return Err(WorkflowError::Build {
                path: "resource".to_string(),
                message: "resource identity must not be empty".to_string(),
            });
        }
        normalized
            .entry(resource.to_string())
            .and_modify(|access| {
                if claim.access == ResourceAccess::Write {
                    *access = ResourceAccess::Write;
                }
            })
            .or_insert(claim.access);
    }
    Ok(normalized
        .into_iter()
        .map(|(resource, access)| ResourceClaim { resource, access })
        .collect())
}

fn claims_available(state: &BTreeMap<String, ResourceState>, claims: &[ResourceClaim]) -> bool {
    claims.iter().all(|claim| {
        state
            .get(&claim.resource)
            .is_none_or(|resource| match claim.access {
                ResourceAccess::Read => !resource.writer,
                ResourceAccess::Write => !resource.writer && resource.readers == 0,
            })
    })
}

fn apply_claims(state: &mut BTreeMap<String, ResourceState>, claims: &[ResourceClaim]) {
    for claim in claims {
        let resource = state.entry(claim.resource.clone()).or_default();
        match claim.access {
            ResourceAccess::Read => resource.readers = resource.readers.saturating_add(1),
            ResourceAccess::Write => resource.writer = true,
        }
    }
}

#[derive(Debug)]
struct ResourceLease {
    coordinator: Option<Arc<ResourceCoordinator>>,
    tracker: Arc<RunStateTracker>,
    claims: Vec<ResourceClaim>,
}

impl Drop for ResourceLease {
    fn drop(&mut self) {
        let Some(coordinator) = self.coordinator.take() else {
            return;
        };
        coordinator.release(&self.claims, &self.tracker);
    }
}

/// Terminal workflow outcome used by observation events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowOutcome {
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
}

/// Bounded receiver for in-process workflow events.
#[derive(Debug)]
pub struct WorkflowEventReceiver {
    receiver: mpsc::Receiver<WorkflowEvent>,
    dropped: Arc<AtomicU64>,
}

impl WorkflowEventReceiver {
    /// Receive the next available event.
    pub async fn recv(&mut self) -> Option<WorkflowEvent> {
        self.receiver.recv().await
    }

    /// Try to receive one immediately available event.
    ///
    /// # Errors
    ///
    /// Returns Tokio's empty or disconnected status when no event can be returned immediately.
    pub fn try_recv(&mut self) -> Result<WorkflowEvent, mpsc::error::TryRecvError> {
        self.receiver.try_recv()
    }

    /// Return the number of events dropped because the bounded channel was full.
    #[must_use]
    pub fn dropped_events(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Non-blocking workflow event sink backed by a bounded channel.
#[derive(Debug, Clone)]
pub struct WorkflowEventSender {
    sender: mpsc::Sender<WorkflowEvent>,
    dropped: Arc<AtomicU64>,
}

impl WorkflowEventSender {
    fn emit(&self, event: WorkflowEvent) {
        if self.sender.try_send(event).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Create a bounded non-blocking workflow event channel.
///
/// # Panics
///
/// Panics when `capacity` is zero.
#[must_use]
pub fn workflow_event_channel(capacity: usize) -> (WorkflowEventSender, WorkflowEventReceiver) {
    assert!(capacity > 0, "workflow event capacity must be positive");
    let (sender, receiver) = mpsc::channel(capacity);
    let dropped = Arc::new(AtomicU64::new(0));
    (
        WorkflowEventSender {
            sender,
            dropped: Arc::clone(&dropped),
        },
        WorkflowEventReceiver { receiver, dropped },
    )
}

/// Guard that aborts a Tokio task when its owning operation exits early.
#[derive(Debug)]
pub struct AbortTaskOnDrop<T> {
    handle: JoinHandle<T>,
}

impl<T> AbortTaskOnDrop<T> {
    /// Wrap a spawned task.
    #[must_use]
    pub const fn new(handle: JoinHandle<T>) -> Self {
        Self { handle }
    }
}

impl<T> Drop for AbortTaskOnDrop<T> {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Context supplied to an executing workflow step.
#[derive(Debug, Clone)]
pub struct StepContext {
    cancellation: WorkflowCancellation,
    events: Option<WorkflowEventSender>,
    tracker: Arc<RunStateTracker>,
    concurrency: Arc<ConcurrencyCoordinator>,
    concurrency_held: bool,
    resources: Arc<ResourceCoordinator>,
}

impl StepContext {
    /// Return the workflow cancellation token.
    #[must_use]
    pub fn cancellation(&self) -> WorkflowCancellation {
        self.cancellation.clone()
    }

    /// Return an incrementally maintained run snapshot.
    #[must_use]
    pub fn snapshot(&self) -> WorkflowRunSnapshot {
        self.tracker.snapshot()
    }

    fn transition(&self, node: &str, state: NodeRunState) {
        self.tracker.transition(node, state);
    }

    fn controller_started(&self, node: &str) {
        self.transition(node, NodeRunState::Running);
        self.emit(WorkflowEvent::StepStarted {
            step: node.to_string(),
        });
    }

    fn controller_finished(&self, node: &str, error: Option<&WorkflowError>) {
        match error {
            None => {
                self.transition(node, NodeRunState::Succeeded);
                self.emit(WorkflowEvent::StepCompleted {
                    step: node.to_string(),
                });
            }
            Some(error) => {
                self.transition(node, node_state_for_error(error));
                self.emit(WorkflowEvent::StepFailed {
                    step: node.to_string(),
                    message: error.to_string(),
                });
            }
        }
    }

    fn skip_nodes(&self, nodes: impl IntoIterator<Item = String>) {
        for node in nodes {
            self.transition(&node, NodeRunState::Skipped);
        }
    }

    async fn acquire_concurrency(
        &self,
        node: &str,
    ) -> Result<Option<OwnedSemaphorePermit>, WorkflowError> {
        if self.concurrency_held {
            Ok(None)
        } else {
            self.concurrency.acquire(node, self).await.map(Some)
        }
    }

    fn with_concurrency_held(&self) -> Self {
        let mut context = self.clone();
        context.concurrency_held = true;
        context
    }

    async fn acquire_resources(
        &self,
        node: &str,
        claims: &[ResourceClaim],
    ) -> Result<Option<ResourceLease>, WorkflowError> {
        self.resources.acquire(node, claims, self).await
    }

    fn emit(&self, event: WorkflowEvent) {
        if let Some(events) = &self.events {
            events.emit(event);
        }
    }

    /// Return an error when workflow cancellation has been requested.
    ///
    /// # Errors
    ///
    /// Returns [`WorkflowError::Cancelled`] when the workflow is cancelled.
    pub fn ensure_active(&self, step: impl Into<String>) -> Result<(), WorkflowError> {
        if self.cancellation.is_cancelled() {
            Err(WorkflowError::Cancelled { step: step.into() })
        } else {
            Ok(())
        }
    }
}

/// Durable-friendly reference to a large workflow value owned by an external artifact store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ArtifactReference {
    /// Stable artifact identity.
    pub artifact_id: String,
    /// Producer-owned schema identity.
    pub schema: String,
    /// Producer-owned schema version.
    pub schema_version: u32,
    /// Media type of the referenced bytes.
    pub content_type: String,
    /// Opaque host-resolvable reference key.
    pub reference_key: String,
}

impl ArtifactReference {
    /// Create a typed artifact reference without loading its bytes into workflow state.
    #[must_use]
    pub fn new(
        artifact_id: impl Into<String>,
        schema: impl Into<String>,
        schema_version: u32,
        content_type: impl Into<String>,
        reference_key: impl Into<String>,
    ) -> Self {
        Self {
            artifact_id: artifact_id.into(),
            schema: schema.into(),
            schema_version,
            content_type: content_type.into(),
            reference_key: reference_key.into(),
        }
    }
}

/// Serializable schema identity for one typed workflow boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValueSchema {
    /// Rust type name used as a diagnostic identity.
    pub type_name: String,
    /// Generated JSON Schema.
    pub schema: serde_json::Value,
}

impl ValueSchema {
    fn of<T: JsonSchema>() -> Self {
        Self {
            type_name: std::any::type_name::<T>().to_string(),
            schema: serde_json::to_value(schemars::schema_for!(T))
                .expect("schemars workflow schema should serialize to JSON"),
        }
    }
}

/// Maximum tool capability a workflow node may request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowToolCapability {
    /// No tool calls are permitted.
    #[default]
    Disabled,
    /// Only tools declared read-only by their owners are permitted.
    ReadOnly,
    /// Mutating tools may be permitted by the configured profile and grant.
    Mutating,
}

/// Bounded grant scope used by workflow policy preflight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowGrantScope {
    /// Stable workflow definition identity.
    pub definition: String,
    /// Definition schema/version identity covered by the grant.
    pub definition_version: u32,
    /// Stable workspace identity covered by the grant.
    pub workspace: String,
    /// Stable node identity covered by the grant.
    pub node: String,
    /// Optional run identity narrowing the grant to one run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
}

/// Auditable grant that can widen an initiating context only within its bounded scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowPolicyGrant {
    /// Opaque non-secret grant identity retained for audit.
    pub grant_id: String,
    /// Exact grant scope.
    pub scope: WorkflowGrantScope,
    /// Maximum capability approved by the grant.
    pub capability: WorkflowToolCapability,
}

/// Immutable policy inputs for one workflow-node preflight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowPolicyRequest {
    /// Capability available to the initiating context.
    pub initiating: WorkflowToolCapability,
    /// Maximum capability permitted by the selected configured profile.
    pub profile: WorkflowToolCapability,
    /// Capability requested by the node restriction.
    pub node: WorkflowToolCapability,
    /// Scope that an optional grant must exactly match.
    pub scope: WorkflowGrantScope,
    /// Optional bounded approved grant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<WorkflowPolicyGrant>,
}

/// Result of policy preflight before node execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WorkflowPolicyPreflight {
    /// Node policy is authorized and immutable for execution.
    Authorized {
        /// Effective maximum capability after intersection.
        effective: WorkflowToolCapability,
        /// Stable non-secret identity suitable for audit records.
        audit_identity: String,
    },
    /// Node requires a bounded grant before it may execute.
    ApprovalRequired {
        /// Capability requested by the node.
        requested: WorkflowToolCapability,
        /// Exact scope an approval must cover.
        scope: WorkflowGrantScope,
    },
    /// Supplied policy inputs are invalid and must not execute.
    Rejected { reason: String },
}

/// Host callback for resolving workflow elevation through the normal permission path.
pub trait WorkflowApprovalResolver: Send + Sync {
    /// Request approval for one exact scope and capability.
    fn request_approval<'a>(
        &'a self,
        requested: WorkflowToolCapability,
        scope: &'a WorkflowGrantScope,
    ) -> Pin<Box<dyn Future<Output = Result<Option<WorkflowPolicyGrant>, WorkflowError>> + Send + 'a>>;
}

/// Resolve policy preflight, requesting approval only when elevation is required.
///
/// # Errors
///
/// Returns an error when the approval host fails, returns no grant, or returns a malformed,
/// mismatched, or insufficient grant.
pub async fn authorize_workflow_policy<R>(
    request: &WorkflowPolicyRequest,
    resolver: &R,
) -> Result<(WorkflowToolCapability, String), WorkflowError>
where
    R: WorkflowApprovalResolver + ?Sized,
{
    match preflight_workflow_policy(request) {
        WorkflowPolicyPreflight::Authorized {
            effective,
            audit_identity,
        } => Ok((effective, audit_identity)),
        WorkflowPolicyPreflight::Rejected { reason } => Err(WorkflowError::Build {
            path: request.scope.node.clone(),
            message: reason,
        }),
        WorkflowPolicyPreflight::ApprovalRequired { requested, scope } => {
            let grant = resolver
                .request_approval(requested, &scope)
                .await?
                .ok_or_else(|| WorkflowError::Build {
                    path: scope.node.clone(),
                    message: "workflow elevation was not approved".to_string(),
                })?;
            let granted = WorkflowPolicyRequest {
                grant: Some(grant),
                ..request.clone()
            };
            match preflight_workflow_policy(&granted) {
                WorkflowPolicyPreflight::Authorized {
                    effective,
                    audit_identity,
                } => Ok((effective, audit_identity)),
                WorkflowPolicyPreflight::Rejected { reason } => Err(WorkflowError::Build {
                    path: granted.scope.node,
                    message: reason,
                }),
                WorkflowPolicyPreflight::ApprovalRequired { .. } => {
                    unreachable!("a supplied grant must authorize or reject")
                }
            }
        }
    }
}

/// Evaluate workflow policy intersection and explicit elevation.
///
/// The configured profile always caps authority. Without a grant, the initiating context also
/// caps authority. A grant may widen beyond the initiating context only when its scope exactly
/// matches the node and its capability covers the request.
#[must_use]
pub fn preflight_workflow_policy(request: &WorkflowPolicyRequest) -> WorkflowPolicyPreflight {
    if let Err(reason) = validate_grant_scope(&request.scope) {
        return WorkflowPolicyPreflight::Rejected { reason };
    }
    if request.node > request.profile {
        return WorkflowPolicyPreflight::Rejected {
            reason: "node requests capability broader than its configured profile".to_string(),
        };
    }
    if request.node <= request.initiating {
        return WorkflowPolicyPreflight::Authorized {
            effective: request.node,
            audit_identity: policy_audit_identity(request, None),
        };
    }
    let Some(grant) = &request.grant else {
        return WorkflowPolicyPreflight::ApprovalRequired {
            requested: request.node,
            scope: request.scope.clone(),
        };
    };
    if grant.grant_id.trim().is_empty() {
        return WorkflowPolicyPreflight::Rejected {
            reason: "grant identity must not be empty".to_string(),
        };
    }
    if grant.grant_id.len() > MAX_POLICY_GRANT_ID_BYTES {
        return WorkflowPolicyPreflight::Rejected {
            reason: format!("grant identity exceeds {MAX_POLICY_GRANT_ID_BYTES} bytes"),
        };
    }
    if grant.scope != request.scope {
        return WorkflowPolicyPreflight::Rejected {
            reason: "grant scope does not match the requested workflow node".to_string(),
        };
    }
    if grant.capability < request.node {
        return WorkflowPolicyPreflight::Rejected {
            reason: "grant capability does not cover the node request".to_string(),
        };
    }
    WorkflowPolicyPreflight::Authorized {
        effective: request.node,
        audit_identity: policy_audit_identity(request, Some(grant.grant_id.as_str())),
    }
}

const MAX_POLICY_SCOPE_ID_BYTES: usize = 512;
const MAX_POLICY_GRANT_ID_BYTES: usize = 512;

fn validate_grant_scope(scope: &WorkflowGrantScope) -> Result<(), String> {
    for (label, value) in [
        ("definition", scope.definition.as_str()),
        ("workspace", scope.workspace.as_str()),
        ("node", scope.node.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(format!("grant scope {label} must not be empty"));
        }
        if value.len() > MAX_POLICY_SCOPE_ID_BYTES {
            return Err(format!(
                "grant scope {label} exceeds {MAX_POLICY_SCOPE_ID_BYTES} bytes"
            ));
        }
    }
    if scope.definition_version == 0 {
        return Err("grant scope definition version must be positive".to_string());
    }
    if let Some(run) = &scope.run {
        if run.trim().is_empty() {
            return Err("grant scope run must not be empty".to_string());
        }
        if run.len() > MAX_POLICY_SCOPE_ID_BYTES {
            return Err(format!(
                "grant scope run exceeds {MAX_POLICY_SCOPE_ID_BYTES} bytes"
            ));
        }
    }
    Ok(())
}

fn policy_audit_identity(request: &WorkflowPolicyRequest, grant_id: Option<&str>) -> String {
    format!(
        "workflow={};version={};workspace={};node={};run={};profile={:?};node_capability={:?};grant={}",
        request.scope.definition,
        request.scope.definition_version,
        request.scope.workspace,
        request.scope.node,
        request.scope.run.as_deref().unwrap_or("*"),
        request.profile,
        request.node,
        grant_id.unwrap_or("none")
    )
}

/// Shared read or exclusive write claim for a workflow resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResourceAccess {
    Read,
    Write,
}

/// One named workflow resource claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResourceClaim {
    /// Stable resource identity such as `repository` or `worktree:review-1`.
    pub resource: String,
    /// Requested access mode.
    pub access: ResourceAccess,
}

impl ResourceClaim {
    /// Create a shared read claim.
    #[must_use]
    pub fn read(resource: impl Into<String>) -> Self {
        Self {
            resource: resource.into(),
            access: ResourceAccess::Read,
        }
    }

    /// Create an exclusive write claim.
    #[must_use]
    pub fn write(resource: impl Into<String>) -> Self {
        Self {
            resource: resource.into(),
            access: ResourceAccess::Write,
        }
    }
}

/// Serializable description of one workflow node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeDefinition {
    /// Stable node identity within the definition.
    pub id: String,
    /// Human-readable node name.
    pub name: String,
    /// Generic node kind interpreted by the workflow host.
    pub kind: NodeKind,
    /// Typed input schema.
    pub input: ValueSchema,
    /// Typed output schema.
    pub output: ValueSchema,
    /// Resources acquired atomically before this node executes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<ResourceClaim>,
    /// Node-specific declarative configuration.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub configuration: serde_json::Value,
}

/// Generic workflow node kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// Application-supplied typed Rust operation.
    Task,
    /// Bcode agent operation.
    Agent,
    /// Deterministic conditional routing.
    Branch,
    /// Explicit bounded cycle controller.
    Repeat,
    /// Bounded retry controller.
    Retry,
    /// Parallel fan-out and typed join.
    Parallel,
    /// Homogeneous bounded fan-out.
    FanOut,
}

/// Serializable directed workflow edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeDefinition {
    /// Source node identity.
    pub from: String,
    /// Target node identity.
    pub to: String,
    /// Control-flow behavior for this edge.
    #[serde(default)]
    pub kind: EdgeKind,
}

/// Serializable workflow edge behavior.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EdgeKind {
    /// Unconditional forward control flow.
    #[default]
    Direct,
    /// Forward control flow selected by a deterministic predicate.
    Conditional {
        /// Predicate evaluated against the branch input.
        predicate: PredicateExpression,
        /// Whether this edge is selected when the predicate matches or does not match.
        expected: bool,
    },
    /// Explicit bounded cycle edge.
    Back {
        /// Predicate evaluated after each body execution.
        predicate: PredicateExpression,
        /// Maximum number of body executions, including the initial execution.
        max_iterations: u32,
    },
    /// Retry a failed body from its entry nodes.
    Retry {
        /// Maximum number of body attempts, including the initial attempt.
        max_attempts: u32,
    },
}

/// Serializable deterministic predicate over a structured workflow value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum PredicateExpression {
    /// Compare the value at a dotted field path for equality.
    Equals {
        /// Dotted object field path. An empty path addresses the complete value.
        path: String,
        /// Expected JSON value.
        value: serde_json::Value,
    },
}

impl PredicateExpression {
    fn evaluate<T: Serialize>(&self, input: &T) -> Result<bool, WorkflowError> {
        let value = serde_json::to_value(input).map_err(|error| WorkflowError::Build {
            path: "predicate".to_string(),
            message: format!("failed to serialize predicate input: {error}"),
        })?;
        match self {
            Self::Equals {
                path,
                value: expected,
            } => {
                let actual = path
                    .split('.')
                    .filter(|part| !part.is_empty())
                    .try_fold(&value, |current, part| current.get(part))
                    .ok_or_else(|| WorkflowError::Build {
                        path: path.clone(),
                        message: "predicate field was not present in the structured value"
                            .to_string(),
                    })?;
                Ok(actual == expected)
            }
        }
    }
}

/// Typed builder for a serializable structured-value predicate.
#[derive(Debug, Clone)]
pub struct Field<T> {
    path: String,
    _input: PhantomData<fn(&T)>,
}

impl<T> Field<T> {
    /// Compare this field with a serializable value.
    ///
    /// # Panics
    ///
    /// Panics when `expected` cannot be represented as JSON.
    #[must_use]
    pub fn eq<V: Serialize>(self, expected: V) -> Predicate<T> {
        Predicate {
            expression: PredicateExpression::Equals {
                path: self.path,
                value: serde_json::to_value(expected)
                    .expect("workflow predicate value should serialize to JSON"),
            },
            _input: PhantomData,
        }
    }
}

/// Typed serializable workflow predicate.
#[derive(Debug, Clone)]
pub struct Predicate<T> {
    expression: PredicateExpression,
    _input: PhantomData<fn(&T)>,
}

impl<T> Predicate<T> {
    /// Return the host-neutral predicate expression.
    #[must_use]
    pub const fn expression(&self) -> &PredicateExpression {
        &self.expression
    }
}

/// Address a structured field using a dotted path.
#[must_use]
pub fn field<T>(path: impl Into<String>) -> Field<T> {
    Field {
        path: path.into(),
        _input: PhantomData,
    }
}

/// Serializable, host-neutral compiled workflow definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowDefinition {
    /// Definition schema version.
    pub schema_version: u32,
    /// Stable workflow identity.
    pub name: String,
    /// Workflow input schema.
    pub input: ValueSchema,
    /// Workflow output schema.
    pub output: ValueSchema,
    /// Nodes in deterministic identity order.
    pub nodes: BTreeMap<String, NodeDefinition>,
    /// Logical entry node identities.
    pub entries: Vec<String>,
    /// Logical exit node identities.
    pub exits: Vec<String>,
    /// Edges in deterministic order.
    pub edges: Vec<EdgeDefinition>,
}

impl WorkflowDefinition {
    /// Look up one node by stable ID.
    #[must_use]
    pub fn node(&self, id: &str) -> Option<&NodeDefinition> {
        self.nodes.get(id)
    }
}

#[derive(Debug, Clone, Default)]
struct DefinitionFragment {
    nodes: Vec<NodeDefinition>,
    edges: Vec<EdgeDefinition>,
    entries: Vec<String>,
    exits: Vec<String>,
}

impl DefinitionFragment {
    fn sequence(mut self, next: Self) -> Self {
        for from in &self.exits {
            for to in &next.entries {
                self.edges.push(EdgeDefinition {
                    from: from.clone(),
                    to: to.clone(),
                    kind: EdgeKind::Direct,
                });
            }
        }
        self.nodes.extend(next.nodes);
        self.edges.extend(next.edges);
        self.exits = next.exits;
        self
    }
}

/// Bounded retry policy for one composed step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    max_attempts: u32,
    backoff: Duration,
}

impl RetryPolicy {
    /// Create a retry policy with no delay between attempts.
    #[must_use]
    pub const fn new(max_attempts: u32) -> Self {
        Self {
            max_attempts,
            backoff: Duration::ZERO,
        }
    }

    /// Configure a fixed delay between attempts.
    #[must_use]
    pub const fn backoff(mut self, backoff: Duration) -> Self {
        self.backoff = backoff;
        self
    }
}

/// One typed, composable workflow operation.
pub struct Step<I, O> {
    run: Arc<StepFn<I, O>>,
    fragment: DefinitionFragment,
    leaf_node_id: Option<String>,
    _types: PhantomData<fn(I) -> O>,
}

impl<I, O> Clone for Step<I, O> {
    fn clone(&self) -> Self {
        Self {
            run: Arc::clone(&self.run),
            fragment: self.fragment.clone(),
            leaf_node_id: self.leaf_node_id.clone(),
            _types: PhantomData,
        }
    }
}

impl<I, O> fmt::Debug for Step<I, O> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Step")
            .field("entries", &self.fragment.entries)
            .field("exits", &self.fragment.exits)
            .finish_non_exhaustive()
    }
}

impl<I, O> Step<I, O>
where
    I: JsonSchema + Send + 'static,
    O: Serialize + DeserializeOwned + JsonSchema + Send + 'static,
{
    /// Create an asynchronous typed application step.
    #[must_use]
    pub fn task<F, Fut>(name: impl Into<String>, operation: F) -> Self
    where
        F: Fn(I, StepContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, WorkflowError>> + Send + 'static,
    {
        Self::configured_task(name, NodeKind::Task, serde_json::Value::Null, operation)
    }

    /// Create a synchronous typed application step.
    #[must_use]
    pub fn map<F>(name: impl Into<String>, operation: F) -> Self
    where
        F: Fn(I) -> Result<O, WorkflowError> + Send + Sync + 'static,
    {
        Self::task(name, move |input, _context| {
            let output = operation(input);
            async move { output }
        })
    }

    /// Create a typed step with serializable host configuration.
    #[must_use]
    pub fn configured_task<F, Fut>(
        name: impl Into<String>,
        kind: NodeKind,
        configuration: serde_json::Value,
        operation: F,
    ) -> Self
    where
        F: Fn(I, StepContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, WorkflowError>> + Send + 'static,
    {
        let name = name.into();
        let node = NodeDefinition {
            id: name.clone(),
            name,
            kind,
            input: ValueSchema::of::<I>(),
            output: ValueSchema::of::<O>(),
            resources: Vec::new(),
            configuration,
        };
        let id = node.id.clone();
        let operation = Arc::new(operation);
        let step_id = id.clone();
        let run = Arc::new(move |input, context: StepContext| {
            let operation = Arc::clone(&operation);
            let step_id = step_id.clone();
            Box::pin(async move {
                context.transition(&step_id, NodeRunState::Ready);
                if let Err(error) = context.ensure_active(step_id.clone()) {
                    context.transition(&step_id, NodeRunState::Cancelled);
                    return Err(error);
                }
                let _concurrency_permit = match context.acquire_concurrency(&step_id).await {
                    Ok(permit) => permit,
                    Err(error) => {
                        context.transition(&step_id, node_state_for_error(&error));
                        context.emit(WorkflowEvent::StepFailed {
                            step: step_id,
                            message: error.to_string(),
                        });
                        return Err(error);
                    }
                };
                let context = context.with_concurrency_held();
                context.transition(&step_id, NodeRunState::Running);
                context.emit(WorkflowEvent::StepStarted {
                    step: step_id.clone(),
                });
                let result = operation(input, context.clone()).await.and_then(|output| {
                    validate_output(&step_id, &output)?;
                    Ok(output)
                });
                match &result {
                    Ok(_) => {
                        context.transition(&step_id, NodeRunState::Succeeded);
                        context.emit(WorkflowEvent::StepCompleted {
                            step: step_id.clone(),
                        });
                    }
                    Err(error) => {
                        context.transition(&step_id, node_state_for_error(error));
                        context.emit(WorkflowEvent::StepFailed {
                            step: step_id.clone(),
                            message: error.to_string(),
                        });
                    }
                }
                result
            }) as StepFuture<O>
        });
        Self {
            run,
            fragment: DefinitionFragment {
                nodes: vec![node],
                edges: Vec::new(),
                entries: vec![id.clone()],
                exits: vec![id.clone()],
            },
            leaf_node_id: Some(id),
            _types: PhantomData,
        }
    }

    /// Declare resources acquired atomically before this leaf step executes.
    ///
    /// # Panics
    ///
    /// Panics when called on a composed flow rather than one leaf task or agent step. Apply
    /// resource claims before `then`, `branch`, `repeat`, `retry`, or parallel composition.
    #[must_use]
    pub fn resources(mut self, claims: impl IntoIterator<Item = ResourceClaim>) -> Self {
        let leaf = self
            .leaf_node_id
            .as_ref()
            .expect("resource claims can only be added to a leaf step");
        let node = self
            .fragment
            .nodes
            .iter_mut()
            .find(|node| &node.id == leaf)
            .expect("leaf workflow node exists");
        let claims =
            normalize_resource_claims(claims).expect("workflow resource claims must be valid");
        node.resources.clone_from(&claims);
        let run = Arc::clone(&self.run);
        let step_id = leaf.clone();
        self.run = Arc::new(move |input, context| {
            let run = Arc::clone(&run);
            let claims = claims.clone();
            let step_id = step_id.clone();
            Box::pin(async move {
                let _resource_lease = match context.acquire_resources(&step_id, &claims).await {
                    Ok(lease) => lease,
                    Err(error) => {
                        context.transition(&step_id, node_state_for_error(&error));
                        context.emit(WorkflowEvent::StepFailed {
                            step: step_id,
                            message: error.to_string(),
                        });
                        return Err(error);
                    }
                };
                run(input, context).await
            })
        });
        self
    }

    /// Run `next` after this step and carry its typed output into `next`.
    #[must_use]
    pub fn then<N>(self, next: Step<O, N>) -> Step<I, N>
    where
        N: Serialize + DeserializeOwned + JsonSchema + Send + 'static,
    {
        let first = Arc::clone(&self.run);
        let second = Arc::clone(&next.run);
        Step {
            run: Arc::new(move |input, context| {
                let first = Arc::clone(&first);
                let second = Arc::clone(&second);
                Box::pin(async move {
                    let intermediate = first(input, context.clone()).await?;
                    second(intermediate, context).await
                })
            }),
            fragment: self.fragment.sequence(next.fragment),
            leaf_node_id: None,
            _types: PhantomData,
        }
    }

    /// Execute this step at least once and repeat it while `predicate` matches its output.
    ///
    /// The output type is also the next iteration's input, and `max_iterations` includes the
    /// initial execution. A zero iteration limit is rejected when the workflow is built.
    #[must_use]
    pub fn repeat_while(
        self,
        name: impl Into<String>,
        predicate: Predicate<O>,
        max_iterations: u32,
    ) -> Self
    where
        O: Clone + Serialize + Into<I>,
    {
        let name = name.into();
        let repeat_id = name.clone();
        let expression = predicate.expression;
        let run_expression = expression.clone();
        let body_run = Arc::clone(&self.run);
        let body_entries = self.fragment.entries.clone();
        let body_exits = self.fragment.exits.clone();
        let mut fragment = self.fragment;
        fragment.nodes.push(NodeDefinition {
            id: repeat_id.clone(),
            name,
            kind: NodeKind::Repeat,
            input: ValueSchema::of::<O>(),
            output: ValueSchema::of::<O>(),
            resources: Vec::new(),
            configuration: serde_json::json!({
                "predicate": expression,
                "max_iterations": max_iterations,
            }),
        });
        for exit in &body_exits {
            fragment.edges.push(EdgeDefinition {
                from: exit.clone(),
                to: repeat_id.clone(),
                kind: EdgeKind::Direct,
            });
        }
        for entry in &body_entries {
            fragment.edges.push(EdgeDefinition {
                from: repeat_id.clone(),
                to: entry.clone(),
                kind: EdgeKind::Back {
                    predicate: expression.clone(),
                    max_iterations,
                },
            });
        }
        fragment.exits = vec![repeat_id.clone()];
        Self {
            run: Arc::new(move |input, context| {
                let body_run = Arc::clone(&body_run);
                let expression = run_expression.clone();
                let repeat_id = repeat_id.clone();
                Box::pin(async move {
                    context.controller_started(&repeat_id);
                    let result = async {
                        if max_iterations == 0 {
                            return Err(WorkflowError::Build {
                                path: repeat_id.clone(),
                                message: "repeat max_iterations must be greater than zero"
                                    .to_string(),
                            });
                        }
                        let mut output = body_run(input, context.clone()).await?;
                        context.emit(WorkflowEvent::IterationStarted {
                            step: repeat_id.clone(),
                            iteration: 1,
                            max_iterations,
                        });
                        for iteration in 2..=max_iterations {
                            if !expression.evaluate(&output)? {
                                return Ok(output);
                            }
                            context.ensure_active(repeat_id.clone())?;
                            context.emit(WorkflowEvent::IterationStarted {
                                step: repeat_id.clone(),
                                iteration,
                                max_iterations,
                            });
                            output = body_run(output.clone().into(), context.clone()).await?;
                        }
                        if expression.evaluate(&output)? {
                            return Err(WorkflowError::Step {
                                step: repeat_id.clone(),
                                message: format!(
                                    "repeat condition remained true after {max_iterations} iterations"
                                ),
                            });
                        }
                        Ok(output)
                    }
                    .await;
                    context.controller_finished(&repeat_id, result.as_ref().err());
                    result
                })
            }),
            fragment,
            leaf_node_id: None,
            _types: PhantomData,
        }
    }

    /// Return the logical entry node identities.
    #[must_use]
    pub fn entries(&self) -> &[String] {
        &self.fragment.entries
    }

    /// Return the logical exit node identities.
    #[must_use]
    pub fn exits(&self) -> &[String] {
        &self.fragment.exits
    }

    /// Select one of two typed flows with a deterministic serializable predicate.
    ///
    /// # Panics
    ///
    /// Panics only if the internally generated predicate configuration cannot be serialized.
    #[must_use]
    pub fn branch<N>(
        self,
        name: impl Into<String>,
        predicate: Predicate<O>,
        when_true: Step<O, N>,
        when_false: Step<O, N>,
    ) -> Step<I, N>
    where
        O: Clone + Serialize,
        N: Serialize + DeserializeOwned + JsonSchema + Send + 'static,
    {
        let name = name.into();
        let branch_id = name.clone();
        let branch_node = NodeDefinition {
            id: branch_id.clone(),
            name,
            kind: NodeKind::Branch,
            input: ValueSchema::of::<O>(),
            output: ValueSchema::of::<O>(),
            resources: Vec::new(),
            configuration: serde_json::to_value(predicate.expression())
                .expect("workflow predicate should serialize to JSON"),
        };
        let prior_run = Arc::clone(&self.run);
        let true_run = Arc::clone(&when_true.run);
        let false_run = Arc::clone(&when_false.run);
        let true_nodes = when_true
            .fragment
            .nodes
            .iter()
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        let false_nodes = when_false
            .fragment
            .nodes
            .iter()
            .map(|node| node.id.clone())
            .collect::<Vec<_>>();
        let expression = predicate.expression;
        let run_expression = expression.clone();
        let run_branch_id = branch_id.clone();
        let mut fragment = self.fragment;
        for exit in &fragment.exits {
            fragment.edges.push(EdgeDefinition {
                from: exit.clone(),
                to: branch_id.clone(),
                kind: EdgeKind::Direct,
            });
        }
        for entry in &when_true.fragment.entries {
            fragment.edges.push(EdgeDefinition {
                from: branch_id.clone(),
                to: entry.clone(),
                kind: EdgeKind::Conditional {
                    predicate: expression.clone(),
                    expected: true,
                },
            });
        }
        for entry in &when_false.fragment.entries {
            fragment.edges.push(EdgeDefinition {
                from: branch_id.clone(),
                to: entry.clone(),
                kind: EdgeKind::Conditional {
                    predicate: expression.clone(),
                    expected: false,
                },
            });
        }
        fragment.nodes.push(branch_node);
        fragment.nodes.extend(when_true.fragment.nodes);
        fragment.nodes.extend(when_false.fragment.nodes);
        fragment.edges.extend(when_true.fragment.edges);
        fragment.edges.extend(when_false.fragment.edges);
        fragment.exits = when_true
            .fragment
            .exits
            .into_iter()
            .chain(when_false.fragment.exits)
            .collect();
        Step {
            run: Arc::new(move |input, context| {
                let prior_run = Arc::clone(&prior_run);
                let true_run = Arc::clone(&true_run);
                let false_run = Arc::clone(&false_run);
                let true_nodes = true_nodes.clone();
                let false_nodes = false_nodes.clone();
                let expression = run_expression.clone();
                let branch_id = run_branch_id.clone();
                Box::pin(async move {
                    let branch_input = prior_run(input, context.clone()).await?;
                    context.controller_started(&branch_id);
                    let result = if expression.evaluate(&branch_input)? {
                        context.skip_nodes(false_nodes);
                        true_run(branch_input, context.clone()).await
                    } else {
                        context.skip_nodes(true_nodes);
                        false_run(branch_input, context.clone()).await
                    };
                    context.controller_finished(&branch_id, result.as_ref().err());
                    result
                })
            }),
            fragment,
            leaf_node_id: None,
            _types: PhantomData,
        }
    }

    /// Retry this composed step after failures, up to `max_attempts` total attempts.
    ///
    /// A zero attempt limit is rejected when the workflow is built. Cancellation and timeout
    /// failures are terminal and are never retried.
    #[must_use]
    pub fn retry(self, name: impl Into<String>, max_attempts: u32) -> Self
    where
        I: Clone + Sync,
    {
        self.retry_with_policy(name, RetryPolicy::new(max_attempts))
    }

    /// Retry this composed step using an explicit bounded policy.
    #[must_use]
    pub fn retry_with_policy(self, name: impl Into<String>, policy: RetryPolicy) -> Self
    where
        I: Clone + Sync,
    {
        let max_attempts = policy.max_attempts;
        let backoff = policy.backoff;
        let name = name.into();
        let retry_id = name.clone();
        let run = Arc::clone(&self.run);
        let body_entries = self.fragment.entries.clone();
        let mut fragment = self.fragment;
        for exit in &fragment.exits {
            fragment.edges.push(EdgeDefinition {
                from: exit.clone(),
                to: retry_id.clone(),
                kind: EdgeKind::Direct,
            });
        }
        for entry in &body_entries {
            fragment.edges.push(EdgeDefinition {
                from: retry_id.clone(),
                to: entry.clone(),
                kind: EdgeKind::Retry { max_attempts },
            });
        }
        fragment.nodes.push(NodeDefinition {
            id: retry_id.clone(),
            name,
            kind: NodeKind::Retry,
            input: ValueSchema::of::<I>(),
            output: ValueSchema::of::<O>(),
            resources: Vec::new(),
            configuration: serde_json::json!({
                "max_attempts": max_attempts,
                "backoff_ms": u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX),
            }),
        });
        fragment.exits = vec![retry_id.clone()];
        Self {
            run: Arc::new(move |input, context| {
                let run = Arc::clone(&run);
                let retry_id = retry_id.clone();
                Box::pin(async move {
                    context.controller_started(&retry_id);
                    let result = async {
                        if max_attempts == 0 {
                            return Err(WorkflowError::Build {
                                path: retry_id.clone(),
                                message: "retry max_attempts must be greater than zero".to_string(),
                            });
                        }
                        let mut errors = Vec::new();
                        for attempt in 1..=max_attempts {
                            context.ensure_active(retry_id.clone())?;
                            context.emit(WorkflowEvent::RetryAttempt {
                                step: retry_id.clone(),
                                attempt,
                                max_attempts,
                            });
                            match run(input.clone(), context.clone()).await {
                                Ok(output) => return Ok(output),
                                Err(
                                    error @ (WorkflowError::Cancelled { .. }
                                    | WorkflowError::TimedOut { .. }),
                                ) => return Err(error),
                                Err(error) => {
                                    errors.push(error.to_string());
                                    if attempt < max_attempts {
                                        if backoff.is_zero() {
                                            tokio::task::yield_now().await;
                                        } else {
                                            tokio::time::sleep(backoff).await;
                                        }
                                    }
                                }
                            }
                        }
                        Err(WorkflowError::RetryExhausted {
                            step: retry_id.clone(),
                            attempts: max_attempts,
                            errors,
                        })
                    }
                    .await;
                    context.controller_finished(&retry_id, result.as_ref().err());
                    result
                })
            }),
            fragment,
            leaf_node_id: None,
            _types: PhantomData,
        }
    }

    /// Apply one timeout to this composed step.
    #[must_use]
    pub fn timeout(self, timeout: Duration) -> Self {
        let run = Arc::clone(&self.run);
        let step = self
            .fragment
            .entries
            .first()
            .cloned()
            .unwrap_or_else(|| "workflow".to_string());
        Self {
            run: Arc::new(move |input, context| {
                let run = Arc::clone(&run);
                let step = step.clone();
                Box::pin(async move {
                    tokio::time::timeout(timeout, run(input, context))
                        .await
                        .map_err(|_| WorkflowError::TimedOut { step, timeout })?
                })
            }),
            fragment: self.fragment,
            leaf_node_id: self.leaf_node_id,
            _types: PhantomData,
        }
    }
}

/// Failure behavior for a two-branch parallel join.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParallelFailurePolicy {
    /// Wait for both branches to settle before returning the first branch-ordered failure.
    #[default]
    WaitAll,
    /// Return the first observed failure and request cooperative sibling cancellation.
    FailFast,
}

/// Execute a homogeneous collection through one cloned step with bounded concurrency.
///
/// Results preserve input order regardless of completion order. The first observed failure aborts
/// unfinished sibling tasks.
#[must_use]
pub fn fan_out<I, O>(
    name: impl Into<String>,
    step: Step<I, O>,
    max_concurrency: usize,
) -> Step<Vec<I>, Vec<O>>
where
    I: JsonSchema + Send + 'static,
    O: Serialize + DeserializeOwned + JsonSchema + Send + 'static,
{
    let name = name.into();
    let fan_out_id = name.clone();
    let Step {
        run,
        fragment: mut body,
        leaf_node_id: _,
        _types: _,
    } = step;
    let body_entries = body.entries.clone();
    let body_exits = body.exits.clone();
    for exit in &body_exits {
        body.edges.push(EdgeDefinition {
            from: exit.clone(),
            to: fan_out_id.clone(),
            kind: EdgeKind::Direct,
        });
    }
    body.nodes.push(NodeDefinition {
        id: fan_out_id.clone(),
        name,
        kind: NodeKind::FanOut,
        input: ValueSchema::of::<Vec<I>>(),
        output: ValueSchema::of::<Vec<O>>(),
        resources: Vec::new(),
        configuration: serde_json::json!({"max_concurrency": max_concurrency}),
    });
    body.entries = body_entries;
    body.exits = vec![fan_out_id.clone()];
    Step {
        run: Arc::new(move |inputs, context| {
            let run = Arc::clone(&run);
            let fan_out_id = fan_out_id.clone();
            Box::pin(async move {
                context.controller_started(&fan_out_id);
                let result = async {
                    if max_concurrency == 0 {
                        return Err(WorkflowError::Build {
                            path: fan_out_id.clone(),
                            message: "fan_out max_concurrency must be greater than zero"
                                .to_string(),
                        });
                    }
                    context.ensure_active(fan_out_id.clone())?;
                    let mut inputs = inputs.into_iter().enumerate();
                    let mut tasks = JoinSet::new();
                    for _ in 0..max_concurrency {
                        let Some((index, input)) = inputs.next() else {
                            break;
                        };
                        spawn_fan_out_task(
                            &mut tasks,
                            Arc::clone(&run),
                            context.clone(),
                            index,
                            input,
                        );
                    }
                    let mut outputs = BTreeMap::new();
                    while let Some(result) = tasks.join_next().await {
                        match result {
                            Ok(Ok((index, output))) => {
                                outputs.insert(index, output);
                                if let Some((next_index, input)) = inputs.next() {
                                    spawn_fan_out_task(
                                        &mut tasks,
                                        Arc::clone(&run),
                                        context.clone(),
                                        next_index,
                                        input,
                                    );
                                }
                            }
                            Ok(Err(error)) => {
                                tasks.abort_all();
                                while tasks.join_next().await.is_some() {}
                                return Err(error);
                            }
                            Err(error) => {
                                tasks.abort_all();
                                while tasks.join_next().await.is_some() {}
                                return Err(WorkflowError::step(
                                    &fan_out_id,
                                    format!("fan-out task failed to join: {error}"),
                                ));
                            }
                        }
                    }
                    Ok(outputs.into_values().collect())
                }
                .await;
                context.controller_finished(&fan_out_id, result.as_ref().err());
                result
            })
        }),
        fragment: body,
        leaf_node_id: None,
        _types: PhantomData,
    }
}

fn spawn_fan_out_task<I, O>(
    tasks: &mut JoinSet<Result<(usize, O), WorkflowError>>,
    run: Arc<StepFn<I, O>>,
    context: StepContext,
    index: usize,
    input: I,
) where
    I: Send + 'static,
    O: Send + 'static,
{
    tasks.spawn(async move { run(input, context).await.map(|output| (index, output)) });
}

/// Compose two independent typed steps and join their outputs as a tuple.
#[must_use]
pub fn parallel<I, A, B>(left: Step<I, A>, right: Step<I, B>) -> Step<I, (A, B)>
where
    I: Clone + JsonSchema + Send + 'static,
    A: Serialize + DeserializeOwned + JsonSchema + Send + 'static,
    B: Serialize + DeserializeOwned + JsonSchema + Send + 'static,
{
    let name = generated_parallel_name(&left, &right);
    parallel_named_with_policy(name, ParallelFailurePolicy::WaitAll, left, right)
}

/// Compose two independent typed steps with an explicit stable join identity.
#[must_use]
pub fn parallel_named<I, A, B>(
    name: impl Into<String>,
    left: Step<I, A>,
    right: Step<I, B>,
) -> Step<I, (A, B)>
where
    I: Clone + JsonSchema + Send + 'static,
    A: Serialize + DeserializeOwned + JsonSchema + Send + 'static,
    B: Serialize + DeserializeOwned + JsonSchema + Send + 'static,
{
    parallel_named_with_policy(name, ParallelFailurePolicy::WaitAll, left, right)
}

/// Compose two independent typed steps with explicit join identity and failure behavior.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn parallel_named_with_policy<I, A, B>(
    name: impl Into<String>,
    failure_policy: ParallelFailurePolicy,
    left: Step<I, A>,
    right: Step<I, B>,
) -> Step<I, (A, B)>
where
    I: Clone + JsonSchema + Send + 'static,
    A: Serialize + DeserializeOwned + JsonSchema + Send + 'static,
    B: Serialize + DeserializeOwned + JsonSchema + Send + 'static,
{
    let join_id = name.into();
    let Step {
        run: left_run,
        fragment: left_fragment,
        leaf_node_id: _,
        _types: _,
    } = left;
    let Step {
        run: right_run,
        fragment: right_fragment,
        leaf_node_id: _,
        _types: _,
    } = right;
    let mut nodes = Vec::with_capacity(left_fragment.nodes.len() + right_fragment.nodes.len() + 1);
    nodes.extend(left_fragment.nodes.clone());
    nodes.extend(right_fragment.nodes.clone());
    nodes.push(NodeDefinition {
        id: join_id.clone(),
        name: "parallel join".to_string(),
        kind: NodeKind::Parallel,
        input: ValueSchema::of::<I>(),
        output: ValueSchema::of::<(A, B)>(),
        resources: Vec::new(),
        configuration: serde_json::json!({"failure_policy": failure_policy}),
    });
    let mut edges = left_fragment.edges.clone();
    edges.extend(right_fragment.edges.clone());
    for exit in left_fragment
        .exits
        .iter()
        .chain(right_fragment.exits.iter())
    {
        edges.push(EdgeDefinition {
            from: exit.clone(),
            to: join_id.clone(),
            kind: EdgeKind::Direct,
        });
    }
    let mut entries = left_fragment.entries;
    entries.extend(right_fragment.entries);
    entries.sort();
    entries.dedup();
    let run_join_id = join_id.clone();
    Step {
        run: Arc::new(move |input, context| {
            let left_run = Arc::clone(&left_run);
            let right_run = Arc::clone(&right_run);
            let right_input = input.clone();
            let right_context = context.clone();
            let join_id = run_join_id.clone();
            Box::pin(async move {
                context.controller_started(&join_id);
                let result = match failure_policy {
                    ParallelFailurePolicy::WaitAll => {
                        let (left, right) = tokio::join!(
                            left_run(input, context.clone()),
                            right_run(right_input, right_context)
                        );
                        Ok((left?, right?))
                    }
                    ParallelFailurePolicy::FailFast => {
                        let sibling_cancellation = WorkflowCancellation::new();
                        let parent_cancellation = context.cancellation();
                        let sibling_signal = sibling_cancellation.clone();
                        let _parent_bridge = AbortTaskOnDrop::new(tokio::spawn(async move {
                            parent_cancellation.cancelled().await;
                            sibling_signal.cancel();
                        }));
                        let branch_context = StepContext {
                            cancellation: sibling_cancellation.clone(),
                            events: context.events.clone(),
                            tracker: Arc::clone(&context.tracker),
                            concurrency: Arc::clone(&context.concurrency),
                            concurrency_held: context.concurrency_held,
                            resources: Arc::clone(&context.resources),
                        };
                        let mut left = Box::pin(left_run(input, branch_context.clone()));
                        let mut right = Box::pin(right_run(right_input, branch_context));
                        tokio::select! {
                            left_result = &mut left => match left_result {
                                Ok(left_output) => Ok((left_output, right.await?)),
                                Err(error) => {
                                    sibling_cancellation.cancel();
                                    Err(error)
                                }
                            },
                            right_result = &mut right => match right_result {
                                Ok(right_output) => Ok((left.await?, right_output)),
                                Err(error) => {
                                    sibling_cancellation.cancel();
                                    Err(error)
                                }
                            },
                        }
                    }
                };
                context.controller_finished(&join_id, result.as_ref().err());
                result
            })
        }),
        fragment: DefinitionFragment {
            nodes,
            edges,
            entries,
            exits: vec![join_id],
        },
        leaf_node_id: None,
        _types: PhantomData,
    }
}

fn generated_parallel_name<I, A, B>(left: &Step<I, A>, right: &Step<I, B>) -> String {
    let left = left.fragment.entries.first().map_or("left", String::as_str);
    let right = right
        .fragment
        .entries
        .first()
        .map_or("right", String::as_str);
    format!("parallel:{left}+{right}")
}

/// Precomputed immutable indexes for scheduling a compiled definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowPlan {
    dependencies: BTreeMap<String, usize>,
    outgoing: BTreeMap<String, Vec<String>>,
}

impl WorkflowPlan {
    fn compile(definition: &WorkflowDefinition) -> Self {
        let mut dependencies = definition
            .nodes
            .keys()
            .map(|id| (id.clone(), 0_usize))
            .collect::<BTreeMap<_, _>>();
        let mut outgoing = definition
            .nodes
            .keys()
            .map(|id| (id.clone(), Vec::new()))
            .collect::<BTreeMap<_, _>>();
        for edge in definition.edges.iter().filter(|edge| {
            matches!(edge.kind, EdgeKind::Direct)
                && definition.nodes.get(&edge.from).is_none_or(|node| {
                    !matches!(
                        node.kind,
                        NodeKind::Parallel
                            | NodeKind::Branch
                            | NodeKind::Repeat
                            | NodeKind::Retry
                            | NodeKind::FanOut
                    )
                })
        }) {
            *dependencies
                .get_mut(&edge.to)
                .expect("validated workflow edge target exists") += 1;
            outgoing
                .get_mut(&edge.from)
                .expect("validated workflow edge source exists")
                .push(edge.to.clone());
        }
        for targets in outgoing.values_mut() {
            targets.sort();
            targets.dedup();
        }
        Self {
            dependencies,
            outgoing,
        }
    }

    /// Return the number of forward dependencies for one node.
    #[must_use]
    pub fn dependency_count(&self, node_id: &str) -> Option<usize> {
        self.dependencies.get(node_id).copied()
    }

    /// Return deterministic forward targets for one node.
    #[must_use]
    pub fn outgoing(&self, node_id: &str) -> Option<&[String]> {
        self.outgoing.get(node_id).map(Vec::as_slice)
    }
}

/// Builder for one typed workflow.
#[derive(Debug)]
pub struct WorkflowBuilder<I, O> {
    name: String,
    step: Step<I, O>,
}

impl<I, O> WorkflowBuilder<I, O>
where
    I: JsonSchema + Send + 'static,
    O: Serialize + DeserializeOwned + JsonSchema + Send + 'static,
{
    /// Create a workflow from one typed step or composed flow.
    #[must_use]
    pub fn new(name: impl Into<String>, step: Step<I, O>) -> Self {
        Self {
            name: name.into(),
            step,
        }
    }

    /// Compile and validate the workflow.
    ///
    /// # Errors
    ///
    /// Returns an error when the workflow name or node identities are empty, node identities are
    /// duplicated, an edge references a missing node, or the graph is cyclic.
    pub fn build(self) -> Result<Workflow<I, O>, WorkflowError> {
        let definition = compile_definition::<I, O>(&self.name, &self.step.fragment)?;
        let plan = WorkflowPlan::compile(&definition);
        Ok(Workflow {
            definition,
            plan,
            run: self.step.run,
            _types: PhantomData,
        })
    }
}

/// A validated typed workflow ready for execution.
pub struct Workflow<I, O> {
    definition: WorkflowDefinition,
    plan: WorkflowPlan,
    run: Arc<StepFn<I, O>>,
    _types: PhantomData<fn(I) -> O>,
}

impl<I, O> fmt::Debug for Workflow<I, O> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Workflow")
            .field("definition", &self.definition)
            .field("plan", &self.plan)
            .finish_non_exhaustive()
    }
}

impl<I, O> Workflow<I, O>
where
    I: Serialize + JsonSchema + Send + 'static,
    O: Serialize + DeserializeOwned + JsonSchema + Send + 'static,
{
    /// Return the compiled serializable definition.
    #[must_use]
    pub const fn definition(&self) -> &WorkflowDefinition {
        &self.definition
    }

    /// Return precomputed scheduling indexes for the compiled definition.
    #[must_use]
    pub const fn plan(&self) -> &WorkflowPlan {
        &self.plan
    }

    /// Run the workflow with a new cancellation token.
    ///
    /// # Errors
    ///
    /// Returns an error when a step fails, times out, observes cancellation, or produces output
    /// that cannot be validated and decoded against the workflow output type.
    pub async fn run(&self, input: I) -> Result<O, WorkflowError> {
        self.run_with_cancellation(input, WorkflowCancellation::new())
            .await
    }

    /// Run the workflow with caller-owned cancellation.
    ///
    /// # Errors
    ///
    /// Returns an error when a step fails, times out, observes cancellation, or produces output
    /// that cannot be validated and decoded against the workflow output type.
    pub async fn run_with_cancellation(
        &self,
        input: I,
        cancellation: WorkflowCancellation,
    ) -> Result<O, WorkflowError> {
        self.run_observed(input, cancellation, None, None, DEFAULT_MAX_CONCURRENCY)
            .await
    }

    /// Run the workflow with caller-owned cancellation and bounded non-blocking observation.
    ///
    /// # Errors
    ///
    /// Returns an error when a step fails, times out, observes cancellation, or produces invalid
    /// output. A terminal event is emitted for every outcome while the receiver remains open.
    pub async fn run_with_events(
        &self,
        input: I,
        cancellation: WorkflowCancellation,
        events: WorkflowEventSender,
    ) -> Result<O, WorkflowError> {
        self.run_observed(
            input,
            cancellation,
            Some(events),
            None,
            DEFAULT_MAX_CONCURRENCY,
        )
        .await
    }

    /// Create an observer initialized for this workflow's compiled plan.
    #[must_use]
    pub fn observer(&self) -> WorkflowRunObserver {
        WorkflowRunObserver::new(&self.plan)
    }

    /// Run with caller-owned cancellation, bounded events, and a live run observer.
    ///
    /// # Errors
    ///
    /// Returns an error when the observer was created for a different workflow definition or when
    /// normal workflow execution fails.
    pub async fn run_with_observer(
        &self,
        input: I,
        cancellation: WorkflowCancellation,
        events: Option<WorkflowEventSender>,
        observer: WorkflowRunObserver,
    ) -> Result<O, WorkflowError> {
        if observer.plan != self.plan {
            return Err(WorkflowError::Build {
                path: self.definition.name.clone(),
                message: "workflow observer belongs to a different compiled plan".to_string(),
            });
        }
        self.run_observed(
            input,
            cancellation,
            events,
            Some(observer),
            DEFAULT_MAX_CONCURRENCY,
        )
        .await
    }

    /// Run with a workflow-wide bound on concurrently executing leaf steps.
    ///
    /// # Errors
    ///
    /// Returns an error when `max_concurrency` is zero or normal workflow execution fails.
    pub async fn run_with_concurrency_limit(
        &self,
        input: I,
        cancellation: WorkflowCancellation,
        max_concurrency: usize,
    ) -> Result<O, WorkflowError> {
        if max_concurrency == 0 || max_concurrency > Semaphore::MAX_PERMITS {
            return Err(WorkflowError::Build {
                path: self.definition.name.clone(),
                message: format!(
                    "workflow max_concurrency must be between 1 and {}",
                    Semaphore::MAX_PERMITS
                ),
            });
        }
        self.run_observed(input, cancellation, None, None, max_concurrency)
            .await
    }

    async fn run_observed(
        &self,
        input: I,
        cancellation: WorkflowCancellation,
        events: Option<WorkflowEventSender>,
        observer: Option<WorkflowRunObserver>,
        max_concurrency: usize,
    ) -> Result<O, WorkflowError> {
        let first = self
            .definition
            .nodes
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| self.definition.name.clone());
        let tracker = observer.map_or_else(
            || Arc::new(RunStateTracker::new(&self.plan)),
            |observer| observer.tracker,
        );
        let context = StepContext {
            cancellation,
            events,
            tracker,
            concurrency: Arc::new(ConcurrencyCoordinator::new(max_concurrency)),
            concurrency_held: false,
            resources: Arc::new(ResourceCoordinator::default()),
        };
        let result = async {
            context.ensure_active(first)?;
            let output = (self.run)(input, context.clone()).await?;
            context.ensure_active(self.definition.name.clone())?;
            validate_output(&self.definition.name, &output)?;
            Ok(output)
        }
        .await;
        let outcome = match &result {
            Ok(_) => WorkflowOutcome::Succeeded,
            Err(WorkflowError::Cancelled { .. }) => WorkflowOutcome::Cancelled,
            Err(WorkflowError::TimedOut { .. }) => WorkflowOutcome::TimedOut,
            Err(_) => WorkflowOutcome::Failed,
        };
        context.tracker.finish_incomplete(outcome);
        context.emit(WorkflowEvent::WorkflowFinished { outcome });
        result
    }
}

const fn node_state_for_error(error: &WorkflowError) -> NodeRunState {
    match error {
        WorkflowError::Cancelled { .. } => NodeRunState::Cancelled,
        WorkflowError::TimedOut { .. } => NodeRunState::TimedOut,
        _ => NodeRunState::Failed,
    }
}

fn validate_output<T>(step: &str, output: &T) -> Result<(), WorkflowError>
where
    T: Serialize + DeserializeOwned + JsonSchema,
{
    let value = serde_json::to_value(output).map_err(|error| WorkflowError::InvalidOutput {
        step: step.to_string(),
        message: error.to_string(),
    })?;
    let validator = jsonschema::validator_for(&ValueSchema::of::<T>().schema).map_err(|error| {
        WorkflowError::InvalidOutput {
            step: step.to_string(),
            message: format!("invalid generated schema: {error}"),
        }
    })?;
    if let Err(error) = validator.validate(&value) {
        return Err(WorkflowError::InvalidOutput {
            step: step.to_string(),
            message: error.to_string(),
        });
    }
    serde_json::from_value::<T>(value)
        .map(|_| ())
        .map_err(|error| WorkflowError::InvalidOutput {
            step: step.to_string(),
            message: error.to_string(),
        })
}

#[allow(clippy::too_many_lines)]
fn compile_definition<I, O>(
    name: &str,
    fragment: &DefinitionFragment,
) -> Result<WorkflowDefinition, WorkflowError>
where
    I: JsonSchema,
    O: JsonSchema,
{
    if name.trim().is_empty() {
        return Err(WorkflowError::Build {
            path: "workflow".to_string(),
            message: "name must not be empty".to_string(),
        });
    }
    if fragment.nodes.is_empty() {
        return Err(WorkflowError::Build {
            path: name.to_string(),
            message: "workflow must contain at least one step".to_string(),
        });
    }
    if fragment.entries.is_empty() || fragment.exits.is_empty() {
        return Err(WorkflowError::Build {
            path: name.to_string(),
            message: "workflow must have at least one entry and one exit".to_string(),
        });
    }
    let mut nodes = BTreeMap::new();
    for node in &fragment.nodes {
        if node.id.trim().is_empty() {
            return Err(WorkflowError::Build {
                path: name.to_string(),
                message: "step name must not be empty".to_string(),
            });
        }
        if node.kind == NodeKind::Repeat
            && node
                .configuration
                .get("max_iterations")
                .and_then(serde_json::Value::as_u64)
                == Some(0)
        {
            return Err(WorkflowError::Build {
                path: node.id.clone(),
                message: "repeat max_iterations must be greater than zero".to_string(),
            });
        }
        if node.kind == NodeKind::Retry
            && node
                .configuration
                .get("max_attempts")
                .and_then(serde_json::Value::as_u64)
                == Some(0)
        {
            return Err(WorkflowError::Build {
                path: node.id.clone(),
                message: "retry max_attempts must be greater than zero".to_string(),
            });
        }
        if node.kind == NodeKind::FanOut
            && node
                .configuration
                .get("max_concurrency")
                .and_then(serde_json::Value::as_u64)
                == Some(0)
        {
            return Err(WorkflowError::Build {
                path: node.id.clone(),
                message: "fan_out max_concurrency must be greater than zero".to_string(),
            });
        }
        if nodes.insert(node.id.clone(), node.clone()).is_some() {
            return Err(WorkflowError::Build {
                path: node.id.clone(),
                message: "step name is duplicated; choose unique names".to_string(),
            });
        }
    }
    for boundary in fragment.entries.iter().chain(&fragment.exits) {
        if !nodes.contains_key(boundary) {
            return Err(WorkflowError::Build {
                path: name.to_string(),
                message: format!("workflow boundary references missing step '{boundary}'"),
            });
        }
    }
    for edge in &fragment.edges {
        if !nodes.contains_key(&edge.from) || !nodes.contains_key(&edge.to) {
            return Err(WorkflowError::Build {
                path: name.to_string(),
                message: format!(
                    "edge '{} -> {}' references a missing step",
                    edge.from, edge.to
                ),
            });
        }
        if matches!(
            &edge.kind,
            EdgeKind::Back {
                max_iterations: 0,
                ..
            }
        ) {
            return Err(WorkflowError::Build {
                path: edge.from.clone(),
                message: "repeat max_iterations must be greater than zero".to_string(),
            });
        }
    }
    ensure_acyclic(name, &nodes, &fragment.edges)?;
    let mut edges = fragment.edges.clone();
    edges.sort_by(|left, right| (&left.from, &left.to).cmp(&(&right.from, &right.to)));
    edges.dedup();
    Ok(WorkflowDefinition {
        schema_version: WORKFLOW_DEFINITION_SCHEMA_VERSION,
        name: name.to_string(),
        input: ValueSchema::of::<I>(),
        output: ValueSchema::of::<O>(),
        nodes,
        entries: fragment.entries.clone(),
        exits: fragment.exits.clone(),
        edges,
    })
}

fn ensure_acyclic(
    workflow: &str,
    nodes: &BTreeMap<String, NodeDefinition>,
    edges: &[EdgeDefinition],
) -> Result<(), WorkflowError> {
    let mut indegree = nodes
        .keys()
        .map(|id| (id.clone(), 0_usize))
        .collect::<BTreeMap<_, _>>();
    let mut outgoing = BTreeMap::<String, Vec<String>>::new();
    for edge in edges
        .iter()
        .filter(|edge| !matches!(&edge.kind, EdgeKind::Back { .. } | EdgeKind::Retry { .. }))
    {
        *indegree
            .get_mut(&edge.to)
            .expect("edges were checked against nodes") += 1;
        outgoing
            .entry(edge.from.clone())
            .or_default()
            .push(edge.to.clone());
    }
    let mut ready = indegree
        .iter()
        .filter_map(|(id, degree)| (*degree == 0).then_some(id.clone()))
        .collect::<BTreeSet<_>>();
    let mut visited = 0_usize;
    while let Some(id) = ready.pop_first() {
        visited = visited.saturating_add(1);
        if let Some(targets) = outgoing.get(&id) {
            for target in targets {
                let degree = indegree
                    .get_mut(target)
                    .expect("edges were checked against nodes");
                *degree = degree.saturating_sub(1);
                if *degree == 0 {
                    ready.insert(target.clone());
                }
            }
        }
    }
    if visited == nodes.len() {
        Ok(())
    } else {
        Err(WorkflowError::Build {
            path: workflow.to_string(),
            message: "workflow graph contains an unbounded cycle".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
    struct Input {
        value: u64,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
    struct Doubled {
        value: u64,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
    struct Labelled {
        label: String,
    }

    #[tokio::test]
    async fn approval_resolver_is_used_only_for_required_elevation() {
        #[derive(Debug)]
        struct Resolver {
            calls: Arc<AtomicUsize>,
            grant: Option<WorkflowPolicyGrant>,
        }

        impl WorkflowApprovalResolver for Resolver {
            fn request_approval<'a>(
                &'a self,
                _requested: WorkflowToolCapability,
                _scope: &'a WorkflowGrantScope,
            ) -> Pin<
                Box<
                    dyn Future<Output = Result<Option<WorkflowPolicyGrant>, WorkflowError>>
                        + Send
                        + 'a,
                >,
            > {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let grant = self.grant.clone();
                Box::pin(async move { Ok(grant) })
            }
        }

        let scope = WorkflowGrantScope {
            definition: "review-flow".to_string(),
            definition_version: 1,
            workspace: "workspace-1".to_string(),
            node: "commit".to_string(),
            run: Some("run-1".to_string()),
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let resolver = Resolver {
            calls: Arc::clone(&calls),
            grant: Some(WorkflowPolicyGrant {
                grant_id: "approval-1".to_string(),
                scope: scope.clone(),
                capability: WorkflowToolCapability::Mutating,
            }),
        };
        let elevated = WorkflowPolicyRequest {
            initiating: WorkflowToolCapability::ReadOnly,
            profile: WorkflowToolCapability::Mutating,
            node: WorkflowToolCapability::Mutating,
            scope: scope.clone(),
            grant: None,
        };
        let (effective, audit) = authorize_workflow_policy(&elevated, &resolver)
            .await
            .expect("approved elevation");
        assert_eq!(effective, WorkflowToolCapability::Mutating);
        assert!(audit.contains("grant=approval-1"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let narrowed = WorkflowPolicyRequest {
            initiating: WorkflowToolCapability::ReadOnly,
            profile: WorkflowToolCapability::ReadOnly,
            node: WorkflowToolCapability::ReadOnly,
            scope,
            grant: None,
        };
        authorize_workflow_policy(&narrowed, &resolver)
            .await
            .expect("no approval needed");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn approval_resolver_cannot_authorize_mismatched_grant() {
        #[derive(Debug)]
        struct Resolver(WorkflowPolicyGrant);

        impl WorkflowApprovalResolver for Resolver {
            fn request_approval<'a>(
                &'a self,
                _requested: WorkflowToolCapability,
                _scope: &'a WorkflowGrantScope,
            ) -> Pin<
                Box<
                    dyn Future<Output = Result<Option<WorkflowPolicyGrant>, WorkflowError>>
                        + Send
                        + 'a,
                >,
            > {
                let grant = self.0.clone();
                Box::pin(async move { Ok(Some(grant)) })
            }
        }

        let scope = WorkflowGrantScope {
            definition: "review-flow".to_string(),
            definition_version: 1,
            workspace: "workspace-1".to_string(),
            node: "commit".to_string(),
            run: None,
        };
        let request = WorkflowPolicyRequest {
            initiating: WorkflowToolCapability::ReadOnly,
            profile: WorkflowToolCapability::Mutating,
            node: WorkflowToolCapability::Mutating,
            scope: scope.clone(),
            grant: None,
        };
        let resolver = Resolver(WorkflowPolicyGrant {
            grant_id: "approval-1".to_string(),
            scope: WorkflowGrantScope {
                node: "other".to_string(),
                ..scope
            },
            capability: WorkflowToolCapability::Mutating,
        });
        let error = authorize_workflow_policy(&request, &resolver)
            .await
            .expect_err("mismatched grant rejected");
        assert!(error.to_string().contains("scope"));
    }

    #[test]
    fn policy_preflight_intersects_profile_initiator_node_and_grant() {
        let scope = WorkflowGrantScope {
            definition: "review-flow".to_string(),
            definition_version: 1,
            workspace: "workspace-1".to_string(),
            node: "commit".to_string(),
            run: Some("run-1".to_string()),
        };
        let request = WorkflowPolicyRequest {
            initiating: WorkflowToolCapability::ReadOnly,
            profile: WorkflowToolCapability::Mutating,
            node: WorkflowToolCapability::Mutating,
            scope: scope.clone(),
            grant: None,
        };
        assert_eq!(
            preflight_workflow_policy(&request),
            WorkflowPolicyPreflight::ApprovalRequired {
                requested: WorkflowToolCapability::Mutating,
                scope: scope.clone(),
            }
        );

        let authorized = preflight_workflow_policy(&WorkflowPolicyRequest {
            grant: Some(WorkflowPolicyGrant {
                grant_id: "approval-1".to_string(),
                scope,
                capability: WorkflowToolCapability::Mutating,
            }),
            ..request
        });
        assert!(matches!(
            authorized,
            WorkflowPolicyPreflight::Authorized {
                effective: WorkflowToolCapability::Mutating,
                audit_identity,
            } if audit_identity.contains("grant=approval-1")
        ));
    }

    #[test]
    fn policy_preflight_rejects_self_elevation_and_mismatched_grants() {
        let scope = WorkflowGrantScope {
            definition: "review-flow".to_string(),
            definition_version: 1,
            workspace: "workspace-1".to_string(),
            node: "review".to_string(),
            run: None,
        };
        let broader_than_profile = WorkflowPolicyRequest {
            initiating: WorkflowToolCapability::ReadOnly,
            profile: WorkflowToolCapability::ReadOnly,
            node: WorkflowToolCapability::Mutating,
            scope: scope.clone(),
            grant: None,
        };
        assert!(matches!(
            preflight_workflow_policy(&broader_than_profile),
            WorkflowPolicyPreflight::Rejected { reason }
                if reason.contains("configured profile")
        ));

        let mismatched = WorkflowPolicyRequest {
            initiating: WorkflowToolCapability::ReadOnly,
            profile: WorkflowToolCapability::Mutating,
            node: WorkflowToolCapability::Mutating,
            scope: scope.clone(),
            grant: Some(WorkflowPolicyGrant {
                grant_id: "approval-1".to_string(),
                scope: WorkflowGrantScope {
                    node: "other".to_string(),
                    ..scope
                },
                capability: WorkflowToolCapability::Mutating,
            }),
        };
        assert!(matches!(
            preflight_workflow_policy(&mismatched),
            WorkflowPolicyPreflight::Rejected { reason } if reason.contains("scope")
        ));
    }

    #[test]
    fn artifact_references_are_small_typed_values() {
        let reference = ArtifactReference::new(
            "artifact-1",
            "bcode.review.report",
            1,
            "application/json",
            "report.json",
        );
        let value = serde_json::to_value(&reference).expect("serializes");
        assert_eq!(value["artifact_id"], "artifact-1");
        assert_eq!(value["schema_version"], 1);
    }

    #[tokio::test]
    async fn sequential_workflow_compiles_and_runs() {
        let double = Step::map("double", |input: Input| {
            Ok(Doubled {
                value: input.value * 2,
            })
        });
        let label = Step::map("label", |input: Doubled| {
            Ok(Labelled {
                label: input.value.to_string(),
            })
        });
        let workflow = WorkflowBuilder::new("sequential", double.then(label))
            .build()
            .expect("workflow builds");

        assert_eq!(
            workflow.run(Input { value: 4 }).await.expect("run"),
            Labelled {
                label: "8".to_string()
            }
        );
        assert_eq!(workflow.definition().nodes.len(), 2);
        assert_eq!(workflow.definition().edges.len(), 1);
        assert_eq!(workflow.plan().dependency_count("double"), Some(0));
        assert_eq!(workflow.plan().dependency_count("label"), Some(1));
        assert_eq!(
            workflow.plan().outgoing("double"),
            Some(["label".to_string()].as_slice())
        );
    }

    #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
    struct ReviewState {
        needs_fixes: bool,
        attempts: u32,
    }

    #[tokio::test]
    async fn deterministic_branch_runs_only_the_selected_flow() {
        let inspect = Step::map("inspect", |state: ReviewState| Ok(state));
        let fix = Step::map("fix", |mut state: ReviewState| {
            state.attempts += 1;
            Ok(state)
        });
        let clean = Step::map("clean", |state: ReviewState| Ok(state));
        let workflow = WorkflowBuilder::new(
            "branch",
            inspect.branch(
                "needs-fixes?",
                field::<ReviewState>("needs_fixes").eq(true),
                fix,
                clean,
            ),
        )
        .build()
        .expect("workflow builds");

        let observer = workflow.observer();
        let output = workflow
            .run_with_observer(
                ReviewState {
                    needs_fixes: true,
                    attempts: 0,
                },
                WorkflowCancellation::new(),
                None,
                observer.clone(),
            )
            .await
            .expect("run");
        assert_eq!(output.attempts, 1);
        let snapshot = observer.snapshot();
        assert_eq!(snapshot.nodes["needs-fixes?"], NodeRunState::Succeeded);
        assert_eq!(snapshot.nodes["fix"], NodeRunState::Succeeded);
        assert_eq!(snapshot.nodes["clean"], NodeRunState::Skipped);
        assert!(
            workflow
                .definition()
                .edges
                .iter()
                .any(|edge| matches!(edge.kind, EdgeKind::Conditional { .. }))
        );
    }

    #[tokio::test]
    async fn bounded_repeat_stops_when_the_predicate_clears() {
        let cycle = Step::map("fix-and-review", |mut state: ReviewState| {
            state.attempts += 1;
            state.needs_fixes = state.attempts < 3;
            Ok(state)
        })
        .repeat_while(
            "review-cycle",
            field::<ReviewState>("needs_fixes").eq(true),
            3,
        );
        let workflow = WorkflowBuilder::new("repeat", cycle)
            .build()
            .expect("workflow builds");

        let output = workflow
            .run(ReviewState {
                needs_fixes: true,
                attempts: 0,
            })
            .await
            .expect("run");
        assert_eq!(output.attempts, 3);
        assert!(!output.needs_fixes);
        assert!(workflow.definition().edges.iter().any(|edge| matches!(
            edge.kind,
            EdgeKind::Back {
                max_iterations: 3,
                ..
            }
        )));
    }

    #[test]
    fn zero_iteration_repeat_is_rejected_at_build_time() {
        let cycle = Step::map("work", |state: ReviewState| Ok(state)).repeat_while(
            "cycle",
            field::<ReviewState>("needs_fixes").eq(true),
            0,
        );
        let error = WorkflowBuilder::new("invalid-repeat", cycle)
            .build()
            .expect_err("zero bound should fail");
        assert!(error.to_string().contains("greater than zero"));
    }

    #[tokio::test]
    async fn workflow_concurrency_limit_bounds_parallel_leaf_execution() {
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let worker = |name: &'static str| {
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            Step::task(name, move |input: Input, _| {
                let active = Arc::clone(&active);
                let maximum = Arc::clone(&maximum);
                async move {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(current, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(input)
                }
            })
        };
        let workflow = WorkflowBuilder::new(
            "bounded-parallel",
            parallel_named("join", worker("left"), worker("right")),
        )
        .build()
        .expect("workflow builds");

        workflow
            .run_with_concurrency_limit(Input { value: 1 }, WorkflowCancellation::new(), 1)
            .await
            .expect("workflow runs");

        assert_eq!(maximum.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancellation_interrupts_concurrency_wait() {
        let holder_started = Arc::new(Notify::new());
        let release_holder = Arc::new(Notify::new());
        let holder_started_for_step = Arc::clone(&holder_started);
        let release_holder_for_step = Arc::clone(&release_holder);
        let holder = Step::task("holder", move |input: Input, _| {
            let started = Arc::clone(&holder_started_for_step);
            let release = Arc::clone(&release_holder_for_step);
            async move {
                started.notify_one();
                release.notified().await;
                Ok(input)
            }
        });
        let waiting = Step::task("waiting", |input: Input, _| async move { Ok(input) });
        let workflow = Arc::new(
            WorkflowBuilder::new(
                "cancel-concurrency",
                parallel_named("join", holder, waiting),
            )
            .build()
            .expect("workflow builds"),
        );
        let observer = workflow.observer();
        let cancellation = WorkflowCancellation::new();
        let run_cancellation = cancellation.clone();
        let run_workflow = Arc::clone(&workflow);
        let run_observer = observer.clone();
        let task = tokio::spawn(async move {
            run_workflow
                .run_observed(
                    Input { value: 1 },
                    run_cancellation,
                    None,
                    Some(run_observer),
                    1,
                )
                .await
        });

        holder_started.notified().await;
        for _ in 0..20 {
            if observer.snapshot().waiting.contains("waiting") {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            observer.snapshot().nodes["waiting"],
            NodeRunState::WaitingForConcurrency
        );
        cancellation.cancel();
        release_holder.notify_one();
        let error = task
            .await
            .expect("workflow task joins")
            .expect_err("workflow cancels");
        assert!(matches!(error, WorkflowError::Cancelled { .. }));
        assert!(observer.snapshot().running.is_empty());
    }

    #[tokio::test]
    async fn concurrency_and_resource_limits_compose_without_deadlock() {
        let writer_active = Arc::new(AtomicUsize::new(0));
        let writer_maximum = Arc::new(AtomicUsize::new(0));
        let writer = |name: &'static str| {
            let active = Arc::clone(&writer_active);
            let maximum = Arc::clone(&writer_maximum);
            Step::task(name, move |input: Input, _| {
                let active = Arc::clone(&active);
                let maximum = Arc::clone(&maximum);
                async move {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(current, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(input)
                }
            })
            .resources([ResourceClaim::write("repository")])
        };
        let workflow = WorkflowBuilder::new(
            "bounded-resources",
            parallel_named("join", writer("left"), writer("right")),
        )
        .build()
        .expect("workflow builds");

        workflow
            .run_with_concurrency_limit(Input { value: 1 }, WorkflowCancellation::new(), 1)
            .await
            .expect("workflow runs");

        assert_eq!(writer_maximum.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn zero_workflow_concurrency_limit_is_rejected() {
        let workflow =
            WorkflowBuilder::new("invalid-limit", Step::map("step", Ok::<_, WorkflowError>))
                .build()
                .expect("workflow builds");
        let error = workflow
            .run_with_concurrency_limit(Input { value: 1 }, WorkflowCancellation::new(), 0)
            .await
            .expect_err("zero bound fails");
        assert!(error.to_string().contains("between 1"));
    }

    #[tokio::test]
    async fn deterministic_ready_set_is_independent_of_parallel_completion_order() {
        async fn run_with_delays(
            left_delay: Duration,
            right_delay: Duration,
        ) -> WorkflowRunSnapshot {
            let left = Step::task("left", move |input: Input, _| async move {
                tokio::time::sleep(left_delay).await;
                Ok(input)
            });
            let right = Step::task("right", move |input: Input, _| async move {
                tokio::time::sleep(right_delay).await;
                Ok(input)
            });
            let workflow =
                WorkflowBuilder::new("deterministic-ready", parallel_named("join", left, right))
                    .build()
                    .expect("workflow builds");
            let observer = workflow.observer();
            workflow
                .run_with_observer(
                    Input { value: 1 },
                    WorkflowCancellation::new(),
                    None,
                    observer.clone(),
                )
                .await
                .expect("workflow runs");
            observer.snapshot()
        }

        let left_first = run_with_delays(Duration::from_millis(1), Duration::from_millis(5)).await;
        let right_first = run_with_delays(Duration::from_millis(5), Duration::from_millis(1)).await;

        assert_eq!(left_first, right_first);
        assert!(left_first.ready.is_empty());
        assert!(left_first.waiting.is_empty());
        assert!(left_first.running.is_empty());
        assert_eq!(
            left_first.terminal,
            BTreeSet::from(["join".to_string(), "left".to_string(), "right".to_string(),])
        );
    }

    #[test]
    fn terminal_cleanup_tracks_only_incremental_incomplete_nodes() {
        let mut dependencies = BTreeMap::new();
        dependencies.insert("pending".to_string(), 1);
        dependencies.insert("ready".to_string(), 0);
        dependencies.insert("succeeded".to_string(), 0);
        let plan = WorkflowPlan {
            dependencies,
            outgoing: BTreeMap::new(),
        };
        let tracker = RunStateTracker::new(&plan);
        tracker.transition("succeeded", NodeRunState::Succeeded);
        tracker.finish_incomplete(WorkflowOutcome::Failed);

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.nodes["succeeded"], NodeRunState::Succeeded);
        assert_eq!(snapshot.nodes["pending"], NodeRunState::Skipped);
        assert_eq!(snapshot.nodes["ready"], NodeRunState::Skipped);
        assert_eq!(
            snapshot.terminal,
            BTreeSet::from([
                "pending".to_string(),
                "ready".to_string(),
                "succeeded".to_string(),
            ])
        );
    }

    #[tokio::test]
    async fn conflicting_resource_writes_serialize_and_expose_waiting_state() {
        let left_started = Arc::new(Notify::new());
        let release_left = Arc::new(Notify::new());
        let left_started_for_step = Arc::clone(&left_started);
        let release_left_for_step = Arc::clone(&release_left);
        let left = Step::task("left-writer", move |input: Input, _| {
            let started = Arc::clone(&left_started_for_step);
            let release = Arc::clone(&release_left_for_step);
            async move {
                started.notify_one();
                release.notified().await;
                Ok(input)
            }
        })
        .resources([ResourceClaim::write("repository")]);
        let right = Step::task("right-writer", |input: Input, _| async move { Ok(input) })
            .resources([ResourceClaim::write("repository")]);
        let workflow = Arc::new(
            WorkflowBuilder::new("serialized-writes", parallel_named("join", left, right))
                .build()
                .expect("workflow builds"),
        );
        let observer = workflow.observer();
        let run_observer = observer.clone();
        let run_workflow = Arc::clone(&workflow);
        let task = tokio::spawn(async move {
            run_workflow
                .run_with_observer(
                    Input { value: 1 },
                    WorkflowCancellation::new(),
                    None,
                    run_observer,
                )
                .await
        });

        left_started.notified().await;
        for _ in 0..20 {
            let snapshot = observer.snapshot();
            if snapshot.waiting.contains("right-writer") {
                assert!(snapshot.running.contains("left-writer"));
                assert_eq!(snapshot.resource_holders.get("repository"), Some(&1));
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(observer.snapshot().waiting.contains("right-writer"));
        release_left.notify_one();
        task.await
            .expect("workflow task joins")
            .expect("workflow succeeds");
        let snapshot = observer.snapshot();
        assert_eq!(snapshot.nodes["left-writer"], NodeRunState::Succeeded);
        assert_eq!(snapshot.nodes["right-writer"], NodeRunState::Succeeded);
        assert!(snapshot.resource_holders.is_empty());
    }

    #[tokio::test]
    async fn shared_resource_reads_overlap() {
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let reader = |name: &'static str| {
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            Step::task(name, move |input: Input, _| {
                let active = Arc::clone(&active);
                let maximum = Arc::clone(&maximum);
                async move {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(current, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(input)
                }
            })
            .resources([ResourceClaim::read("repository")])
        };
        let workflow = WorkflowBuilder::new(
            "parallel-readers",
            parallel_named("join", reader("reader-a"), reader("reader-b")),
        )
        .build()
        .expect("workflow builds");

        workflow
            .run(Input { value: 1 })
            .await
            .expect("workflow runs");
        assert_eq!(maximum.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cloned_step_resource_configuration_is_value_semantic() {
        let base = Step::map("worker", |input: Input| Ok(input));
        let writer = base.clone().resources([ResourceClaim::write("repository")]);
        assert!(base.fragment.nodes[0].resources.is_empty());
        assert_eq!(writer.fragment.nodes[0].resources.len(), 1);
    }

    #[tokio::test]
    async fn multi_resource_claims_are_atomic_and_order_independent() {
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let worker = |name: &'static str, claims: [ResourceClaim; 2]| {
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            Step::task(name, move |input: Input, _| {
                let active = Arc::clone(&active);
                let maximum = Arc::clone(&maximum);
                async move {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    maximum.fetch_max(current, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(input)
                }
            })
            .resources(claims)
        };
        let workflow = WorkflowBuilder::new(
            "atomic-resources",
            parallel_named(
                "join",
                worker(
                    "first",
                    [ResourceClaim::write("a"), ResourceClaim::write("b")],
                ),
                worker(
                    "second",
                    [ResourceClaim::write("b"), ResourceClaim::write("a")],
                ),
            ),
        )
        .build()
        .expect("workflow builds");

        tokio::time::timeout(Duration::from_secs(1), workflow.run(Input { value: 1 }))
            .await
            .expect("no deadlock")
            .expect("workflow runs");
        assert_eq!(maximum.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancellation_interrupts_resource_wait() {
        let left_started = Arc::new(Notify::new());
        let release_left = Arc::new(Notify::new());
        let left_started_for_step = Arc::clone(&left_started);
        let release_left_for_step = Arc::clone(&release_left);
        let left = Step::task("holder", move |input: Input, _| {
            let started = Arc::clone(&left_started_for_step);
            let release = Arc::clone(&release_left_for_step);
            async move {
                started.notify_one();
                release.notified().await;
                Ok(input)
            }
        })
        .resources([ResourceClaim::write("repository")]);
        let waiting = Step::task("waiting", |input: Input, _| async move { Ok(input) })
            .resources([ResourceClaim::write("repository")]);
        let workflow = Arc::new(
            WorkflowBuilder::new("cancel-wait", parallel_named("join", left, waiting))
                .build()
                .expect("workflow builds"),
        );
        let cancellation = WorkflowCancellation::new();
        let run_cancellation = cancellation.clone();
        let run_workflow = Arc::clone(&workflow);
        let task = tokio::spawn(async move {
            run_workflow
                .run_with_cancellation(Input { value: 1 }, run_cancellation)
                .await
        });

        left_started.notified().await;
        tokio::task::yield_now().await;
        cancellation.cancel();
        release_left.notify_one();
        let error = task
            .await
            .expect("workflow task joins")
            .expect_err("workflow cancels");
        assert!(matches!(error, WorkflowError::Cancelled { .. }));
    }

    #[tokio::test]
    async fn homogeneous_fan_out_is_bounded_and_preserves_input_order() {
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let active_for_step = Arc::clone(&active);
        let maximum_for_step = Arc::clone(&maximum);
        let step = Step::task("worker", move |input: Input, _| {
            let active = Arc::clone(&active_for_step);
            let maximum = Arc::clone(&maximum_for_step);
            async move {
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(current, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(2)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(Doubled {
                    value: input.value * 2,
                })
            }
        });
        let workflow = WorkflowBuilder::new("fan-out", fan_out("workers", step, 2))
            .build()
            .expect("workflow builds");

        let output = workflow
            .run(vec![
                Input { value: 3 },
                Input { value: 1 },
                Input { value: 2 },
            ])
            .await
            .expect("run");
        assert_eq!(
            output
                .into_iter()
                .map(|item| item.value)
                .collect::<Vec<_>>(),
            vec![6, 2, 4]
        );
        assert!(maximum.load(Ordering::SeqCst) <= 2);
    }

    #[tokio::test]
    async fn fan_out_failure_does_not_admit_more_work() {
        let started = Arc::new(AtomicUsize::new(0));
        let started_for_step = Arc::clone(&started);
        let step = Step::task("worker", move |input: Input, _| {
            let started = Arc::clone(&started_for_step);
            async move {
                started.fetch_add(1, Ordering::SeqCst);
                if input.value == 0 {
                    Err(WorkflowError::step("worker", "failed"))
                } else {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Ok(input)
                }
            }
        });
        let workflow = WorkflowBuilder::new("fail-fast", fan_out("workers", step, 2))
            .build()
            .expect("workflow builds");

        let error = workflow
            .run(vec![
                Input { value: 0 },
                Input { value: 1 },
                Input { value: 2 },
                Input { value: 3 },
            ])
            .await
            .expect_err("first task fails");
        assert!(error.to_string().contains("failed"));
        assert_eq!(started.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn zero_concurrency_fan_out_is_rejected_at_build_time() {
        let step = Step::map("worker", |input: Input| Ok(input));
        let error = WorkflowBuilder::new("invalid-fan-out", fan_out("workers", step, 0))
            .build()
            .expect_err("zero concurrency should fail");
        assert!(error.to_string().contains("greater than zero"));
    }

    #[tokio::test]
    async fn bounded_observation_reports_steps_iterations_and_terminal_outcome() {
        let cycle = Step::map("work", |mut state: ReviewState| {
            state.attempts += 1;
            state.needs_fixes = false;
            Ok(state)
        })
        .repeat_while("cycle", field::<ReviewState>("needs_fixes").eq(true), 2);
        let workflow = WorkflowBuilder::new("observed", cycle)
            .build()
            .expect("workflow builds");
        let (events, mut receiver) = workflow_event_channel(16);

        workflow
            .run_with_events(
                ReviewState {
                    needs_fixes: true,
                    attempts: 0,
                },
                WorkflowCancellation::new(),
                events,
            )
            .await
            .expect("workflow runs");
        let mut observed = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            observed.push(event);
        }
        assert!(
            observed
                .iter()
                .any(|event| matches!(event, WorkflowEvent::IterationStarted { iteration: 1, .. }))
        );
        assert!(observed.iter().any(|event| matches!(
            event,
            WorkflowEvent::WorkflowFinished {
                outcome: WorkflowOutcome::Succeeded
            }
        )));
        assert_eq!(receiver.dropped_events(), 0);
    }

    #[tokio::test]
    async fn bounded_retry_reexecutes_failures_and_returns_success() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&attempts);
        let step = Step::map("flaky", move |input: Input| {
            let attempt = observed.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt < 3 {
                Err(WorkflowError::step("flaky", "try again"))
            } else {
                Ok(input)
            }
        })
        .retry("retry-flaky", 3);
        let workflow = WorkflowBuilder::new("retry", step)
            .build()
            .expect("workflow builds");

        let output = workflow
            .run(Input { value: 9 })
            .await
            .expect("third succeeds");
        assert_eq!(output.value, 9);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn exhausted_retry_preserves_ordered_error_history() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&attempts);
        let step = Step::map("always-fails", move |_input: Input| {
            let attempt = observed.fetch_add(1, Ordering::SeqCst) + 1;
            Err::<Input, _>(WorkflowError::step(
                "always-fails",
                format!("failure-{attempt}"),
            ))
        })
        .retry_with_policy(
            "retry-failure",
            RetryPolicy::new(2).backoff(Duration::from_millis(1)),
        );
        let workflow = WorkflowBuilder::new("retry-history", step)
            .build()
            .expect("workflow builds");

        let error = workflow
            .run(Input { value: 0 })
            .await
            .expect_err("retry exhausts");
        let WorkflowError::RetryExhausted {
            attempts, errors, ..
        } = error
        else {
            panic!("expected retry exhaustion");
        };
        assert_eq!(attempts, 2);
        assert_eq!(errors.len(), 2);
        assert!(errors[0].contains("failure-1"));
        assert!(errors[1].contains("failure-2"));
    }

    #[tokio::test]
    async fn timeout_returns_step_scoped_terminal_error() {
        let step = Step::task("slow", |input: Input, _| async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok(input)
        })
        .timeout(Duration::from_millis(1));
        let workflow = WorkflowBuilder::new("timeout", step)
            .build()
            .expect("workflow builds");

        let error = workflow
            .run(Input { value: 1 })
            .await
            .expect_err("times out");
        assert!(matches!(error, WorkflowError::TimedOut { .. }));
    }

    #[tokio::test]
    async fn parallel_fail_fast_drops_unfinished_sibling() {
        struct DropSignal(Arc<AtomicBool>);

        impl Drop for DropSignal {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let sibling_dropped = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&sibling_dropped);
        let failing = Step::task("failing", |_input: Input, _| async {
            tokio::task::yield_now().await;
            Err::<Doubled, _>(WorkflowError::step("failing", "boom"))
        });
        let sibling = Step::task("sibling", move |_input: Input, context| {
            let observed = Arc::clone(&observed);
            async move {
                let _drop_signal = DropSignal(observed);
                context.cancellation().cancelled().await;
                Err::<Labelled, _>(WorkflowError::Cancelled {
                    step: "sibling".to_string(),
                })
            }
        });
        let workflow = WorkflowBuilder::new(
            "parallel-fail-fast",
            parallel_named_with_policy(
                "parallel",
                ParallelFailurePolicy::FailFast,
                failing,
                sibling,
            ),
        )
        .build()
        .expect("workflow builds");

        let error = workflow
            .run(Input { value: 1 })
            .await
            .expect_err("branch fails");
        assert!(error.to_string().contains("boom"));
        assert!(sibling_dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn parallel_workflow_joins_in_declaration_order() {
        let left = Step::task("left", |input: Input, _| async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Ok(Doubled {
                value: input.value * 2,
            })
        });
        let right = Step::map("right", |input: Input| {
            Ok(Labelled {
                label: input.value.to_string(),
            })
        });
        let workflow = WorkflowBuilder::new("parallel", parallel(left, right))
            .build()
            .expect("workflow builds");

        let output = workflow.run(Input { value: 3 }).await.expect("run");
        assert_eq!(output.0.value, 6);
        assert_eq!(output.1.label, "3");
    }

    #[test]
    fn duplicate_step_names_are_actionable_build_errors() {
        let first = Step::map("same", |input: Input| Ok(Doubled { value: input.value }));
        let second = Step::map("same", |input: Doubled| Ok(input));
        let error = WorkflowBuilder::new("duplicate", first.then(second))
            .build()
            .expect_err("duplicate should fail");

        assert!(error.to_string().contains("same"));
        assert!(error.to_string().contains("duplicated"));
    }

    #[tokio::test]
    async fn cancellation_prevents_step_start() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let step = Step::map("never", move |input: Input| {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok(input)
        });
        let workflow = WorkflowBuilder::new("cancelled", step)
            .build()
            .expect("workflow builds");
        let cancellation = WorkflowCancellation::new();
        cancellation.cancel();

        let error = workflow
            .run_with_cancellation(Input { value: 1 }, cancellation)
            .await
            .expect_err("cancelled");
        assert!(matches!(error, WorkflowError::Cancelled { .. }));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
