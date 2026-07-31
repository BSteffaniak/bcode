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
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fmt::Write as _;
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
const MAX_DEFINITION_NODES: usize = 10_000;
const MAX_DEFINITION_EDGES: usize = 100_000;
const MAX_DEFINITION_BOUNDARIES: usize = 10_000;

/// Stable workflow definition schema version.
pub const WORKFLOW_DEFINITION_SCHEMA_VERSION: u32 = 1;

/// Stable durable-production capability contract version.
pub const WORKFLOW_PRODUCTION_CAPABILITY_VERSION: u32 = 1;

/// Stable current-host requirement availability report version.
pub const WORKFLOW_REQUIREMENT_AVAILABILITY_VERSION: u32 = 1;

/// Stable deterministic predicate contract version.
pub const WORKFLOW_PREDICATE_VERSION: u32 = 2;
/// Earliest deterministic predicate contract version retained for compatibility.
pub const WORKFLOW_PREDICATE_MIN_VERSION: u32 = 1;

const MAX_PREDICATE_DEPTH: usize = 16;
const MAX_PREDICATE_OPERATIONS: usize = 256;
const MAX_PREDICATE_PATH_BYTES: usize = 512;
const MAX_PREDICATE_PATH_SEGMENT_BYTES: usize = 256;
const MAX_PREDICATE_VALUE_BYTES: usize = 65_536;

/// Stable plugin workflow-block interface version.
pub const WORKFLOW_BLOCK_INTERFACE_VERSION: u32 = 1;

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
    /// A typed durable run input could not be serialized or did not match its schema.
    #[error("workflow '{workflow}' received invalid input: {message}")]
    InvalidInput {
        /// Stable logical workflow kind.
        workflow: String,
        /// Serialization or schema-validation failure.
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

/// Exact identity for one immutable compiled workflow definition variant.
///
/// `kind` is the stable product-facing workflow identity. `definition_id` includes a digest of the
/// normalized compiled definition, so topology or policy changes cannot accidentally reuse one
/// durable definition slot. The schema version remains explicit for future incompatible compiled
/// definition formats.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowDefinitionIdentity {
    /// Stable plugin or product-owned logical workflow kind.
    pub kind: String,
    /// Collision-resistant exact identity for the compiled definition content using the complete
    /// SHA-256 digest.
    pub definition_id: String,
    /// Compiled workflow definition schema version.
    pub definition_version: u32,
}

impl WorkflowDefinitionIdentity {
    /// Derive the exact durable identity for one logical kind and compiled definition.
    ///
    /// # Errors
    ///
    /// Returns an error when the kind is empty or too large, the definition is invalid, or its
    /// normalized representation cannot be serialized.
    pub fn for_definition(
        kind: impl Into<String>,
        definition: &WorkflowDefinition,
    ) -> Result<Self, WorkflowError> {
        let kind = kind.into();
        if kind.trim().is_empty() || kind.len() > 256 {
            return Err(WorkflowError::Build {
                path: kind,
                message: "workflow kind must contain 1..=256 bytes".to_string(),
            });
        }
        definition.validate()?;
        let encoded = serde_json::to_vec(definition).map_err(|error| WorkflowError::Build {
            path: kind.clone(),
            message: format!("compiled definition cannot be serialized: {error}"),
        })?;
        let digest = Sha256::digest(encoded);
        let mut suffix = String::with_capacity(64);
        for byte in digest {
            write!(&mut suffix, "{byte:02x}").expect("writing to a String cannot fail");
        }
        Ok(Self {
            definition_id: format!("{kind}@{suffix}"),
            kind,
            definition_version: WORKFLOW_DEFINITION_SCHEMA_VERSION,
        })
    }
}

/// Reusable typed durable workflow specification.
///
/// This packages the validated compiled definition together with its logical and exact identities.
/// Per-run input is intentionally separate: input changes do not create definition variants unless
/// they also change compiled topology or policy.
#[derive(Debug, Clone)]
pub struct WorkflowSpec<I> {
    identity: WorkflowDefinitionIdentity,
    definition: WorkflowDefinition,
    _input: PhantomData<fn(I)>,
}

impl<I> WorkflowSpec<I>
where
    I: Serialize + DeserializeOwned + JsonSchema + Send + 'static,
{
    /// Build a durable specification from a typed compiled workflow.
    ///
    /// # Errors
    ///
    /// Returns an error when the logical kind is empty or too large, the definition is invalid, or
    /// its normalized representation cannot be serialized.
    pub fn new<O>(kind: impl Into<String>, workflow: &Workflow<I, O>) -> Result<Self, WorkflowError>
    where
        O: Serialize + DeserializeOwned + JsonSchema + Send + 'static,
    {
        Self::from_definition(kind, workflow.definition().clone())
    }

    /// Build a durable specification from an already compiled definition.
    ///
    /// This is useful at plugin ABI boundaries where only the serializable definition is retained.
    /// The definition input schema must exactly match `I`.
    ///
    /// # Errors
    ///
    /// Returns an error when the kind or definition is invalid, the input schema differs from `I`,
    /// or the normalized definition cannot be serialized.
    pub fn from_definition(
        kind: impl Into<String>,
        definition: WorkflowDefinition,
    ) -> Result<Self, WorkflowError> {
        let kind = kind.into();
        definition.validate()?;
        if definition.input != ValueSchema::of::<I>() {
            return Err(WorkflowError::Build {
                path: kind,
                message: format!(
                    "workflow input schema does not match {}",
                    std::any::type_name::<I>()
                ),
            });
        }
        let identity = WorkflowDefinitionIdentity::for_definition(kind, &definition)?;
        if identity.definition_id.len() > 512 {
            return Err(WorkflowError::Build {
                path: identity.kind,
                message: "exact workflow definition identity exceeds 512 bytes".to_string(),
            });
        }
        Ok(Self {
            identity,
            definition,
            _input: PhantomData,
        })
    }

    /// Return the logical and exact durable identity.
    #[must_use]
    pub const fn identity(&self) -> &WorkflowDefinitionIdentity {
        &self.identity
    }

    /// Return the validated compiled definition.
    #[must_use]
    pub const fn definition(&self) -> &WorkflowDefinition {
        &self.definition
    }

    /// Validate and serialize one typed run input.
    ///
    /// # Errors
    ///
    /// Returns an error when serialization fails or the value does not satisfy its generated JSON
    /// schema.
    pub fn serialize_input(&self, input: &I) -> Result<serde_json::Value, WorkflowError> {
        validate_typed_value(&self.identity.kind, input)
    }
}

fn validate_typed_value<T>(location: &str, value: &T) -> Result<serde_json::Value, WorkflowError>
where
    T: Serialize + DeserializeOwned + JsonSchema,
{
    let value = serde_json::to_value(value).map_err(|error| WorkflowError::InvalidInput {
        workflow: location.to_string(),
        message: error.to_string(),
    })?;
    let validator = jsonschema::validator_for(&ValueSchema::of::<T>().schema).map_err(|error| {
        WorkflowError::InvalidInput {
            workflow: location.to_string(),
            message: format!("invalid generated schema: {error}"),
        }
    })?;
    if let Err(error) = validator.validate(&value) {
        return Err(WorkflowError::InvalidInput {
            workflow: location.to_string(),
            message: error.to_string(),
        });
    }
    Ok(value)
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

/// Validate, deduplicate, and deterministically order resource claims.
///
/// # Errors
///
/// Returns an error when a resource identity is empty.
pub fn normalize_resource_claims(
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

/// Stable durable workflow-state envelope contract version.
pub const WORKFLOW_STATE_ENVELOPE_VERSION: u32 = 1;
/// Maximum serialized retained-state bytes for one state-envelope boundary.
pub const MAX_WORKFLOW_STATE_ENVELOPE_STATE_BYTES: usize = 262_144;
/// Maximum artifact references carried by one state-envelope boundary.
pub const MAX_WORKFLOW_STATE_ENVELOPE_ARTIFACTS: usize = 128;
/// Maximum bytes for one artifact-reference string field.
pub const MAX_WORKFLOW_ARTIFACT_REFERENCE_FIELD_BYTES: usize = 1_024;

/// Versioned node input/output adaptation policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowNodeDataflowPolicy {
    /// Dispatch and persist the complete typed value unchanged.
    #[default]
    Direct,
    /// Dispatch only `value` while retaining explicit state and artifact references.
    StateEnvelopeV1,
}

impl WorkflowNodeDataflowPolicy {
    // Serde's `skip_serializing_if` callback receives a reference even for copy types.
    #[allow(clippy::trivially_copy_pass_by_ref)]
    const fn is_direct(policy: &Self) -> bool {
        matches!(policy, Self::Direct)
    }
}

/// Validated owned parts of one state-envelope boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowStateEnvelopeParts {
    /// Explicit retained state.
    pub state: serde_json::Value,
    /// Narrow operation request or result.
    pub value: serde_json::Value,
    /// Explicit artifact references.
    pub artifacts: Vec<ArtifactReference>,
}

/// Dispatch-ready owner input plus state retained explicitly by the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowPreparedDataflow {
    /// Complete node input is also the owner input.
    Direct(serde_json::Value),
    /// Owner receives only `value`; state and artifacts remain explicit for result rewrapping.
    StateEnvelope(WorkflowStateEnvelopeParts),
}

impl WorkflowPreparedDataflow {
    /// Return the canonical operation input used for authorization and owner dispatch.
    #[must_use]
    pub const fn owner_input(&self) -> &serde_json::Value {
        match self {
            Self::Direct(value) => value,
            Self::StateEnvelope(parts) => &parts.value,
        }
    }
}

/// Validate a complete node input and prepare its canonical owner-operation input.
///
/// # Errors
///
/// Returns an error when the complete input, envelope, or unwrapped owner input is invalid.
pub fn prepare_workflow_node_dataflow(
    policy: WorkflowNodeDataflowPolicy,
    complete_input: &ValueSchema,
    owner_input: &ValueSchema,
    value: &serde_json::Value,
) -> Result<WorkflowPreparedDataflow, WorkflowError> {
    complete_input.validate_value("node.input", value)?;
    match policy {
        WorkflowNodeDataflowPolicy::Direct => {
            owner_input.validate_value("owner.input", value)?;
            Ok(WorkflowPreparedDataflow::Direct(value.clone()))
        }
        WorkflowNodeDataflowPolicy::StateEnvelopeV1 => {
            let parts = validate_workflow_state_envelope(value)?;
            owner_input.validate_value("owner.input", &parts.value)?;
            Ok(WorkflowPreparedDataflow::StateEnvelope(parts))
        }
    }
}

/// Validate an owner result and adapt it to the complete node output boundary.
///
/// # Errors
///
/// Returns an error when the owner result or complete adapted output is invalid.
pub fn complete_workflow_node_dataflow(
    prepared: WorkflowPreparedDataflow,
    owner_output_schema: &ValueSchema,
    complete_output_schema: &ValueSchema,
    owner_output: serde_json::Value,
) -> Result<serde_json::Value, WorkflowError> {
    owner_output_schema.validate_value("owner.output", &owner_output)?;
    match prepared {
        WorkflowPreparedDataflow::Direct(_) => {
            complete_output_schema.validate_value("node.output", &owner_output)?;
            Ok(owner_output)
        }
        WorkflowPreparedDataflow::StateEnvelope(parts) => {
            rewrap_workflow_state_envelope(&parts, &owner_output, complete_output_schema)
        }
    }
}

/// Validate and split one serialized state envelope.
///
/// # Errors
///
/// Returns an error when the envelope version, shape, retained-state bound, artifact count, or
/// artifact-reference fields are invalid.
pub fn validate_workflow_state_envelope(
    envelope: &serde_json::Value,
) -> Result<WorkflowStateEnvelopeParts, WorkflowError> {
    let object = envelope.as_object().ok_or_else(|| WorkflowError::Build {
        path: "state_envelope".to_string(),
        message: "state envelope must be an object".to_string(),
    })?;
    if object.len() > 4
        || !object.contains_key("schema_version")
        || !object.contains_key("state")
        || !object.contains_key("value")
        || object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "schema_version" | "state" | "value" | "artifacts"
            )
        })
    {
        return Err(WorkflowError::Build {
            path: "state_envelope".to_string(),
            message:
                "state envelope must contain schema_version, state, value, and optional artifacts"
                    .to_string(),
        });
    }
    if object["schema_version"].as_u64() != Some(u64::from(WORKFLOW_STATE_ENVELOPE_VERSION)) {
        return Err(WorkflowError::Build {
            path: "state_envelope.schema_version".to_string(),
            message: format!(
                "unsupported state envelope version; expected {WORKFLOW_STATE_ENVELOPE_VERSION}"
            ),
        });
    }
    let state = &object["state"];
    let encoded_state = serde_json::to_vec(state).map_err(|error| WorkflowError::Build {
        path: "state_envelope.state".to_string(),
        message: format!("retained state cannot be serialized: {error}"),
    })?;
    if encoded_state.len() > MAX_WORKFLOW_STATE_ENVELOPE_STATE_BYTES {
        return Err(WorkflowError::Build {
            path: "state_envelope.state".to_string(),
            message: format!(
                "retained state exceeds {MAX_WORKFLOW_STATE_ENVELOPE_STATE_BYTES} bytes"
            ),
        });
    }
    let artifacts_value = object
        .get("artifacts")
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));
    let artifacts: Vec<ArtifactReference> =
        serde_json::from_value(artifacts_value).map_err(|error| WorkflowError::Build {
            path: "state_envelope.artifacts".to_string(),
            message: format!("artifact references are invalid: {error}"),
        })?;
    if artifacts.len() > MAX_WORKFLOW_STATE_ENVELOPE_ARTIFACTS {
        return Err(WorkflowError::Build {
            path: "state_envelope.artifacts".to_string(),
            message: format!(
                "artifact references exceed {MAX_WORKFLOW_STATE_ENVELOPE_ARTIFACTS} entries"
            ),
        });
    }
    for artifact in &artifacts {
        for (field, value) in [
            ("artifact_id", artifact.artifact_id.as_str()),
            ("schema", artifact.schema.as_str()),
            ("content_type", artifact.content_type.as_str()),
            ("reference_key", artifact.reference_key.as_str()),
        ] {
            if value.is_empty() || value.len() > MAX_WORKFLOW_ARTIFACT_REFERENCE_FIELD_BYTES {
                return Err(WorkflowError::Build {
                    path: format!("state_envelope.artifacts.{field}"),
                    message: format!(
                        "artifact reference field must contain 1 to {MAX_WORKFLOW_ARTIFACT_REFERENCE_FIELD_BYTES} bytes"
                    ),
                });
            }
        }
    }
    Ok(WorkflowStateEnvelopeParts {
        state: state.clone(),
        value: object["value"].clone(),
        artifacts,
    })
}

/// Explicit typed dataflow envelope carrying retained workflow state beside a narrow value.
///
/// Hosts persist this value like any other node input/output. Retention is therefore visible in
/// schemas, transforms, checksums, and event history rather than hidden mutable host context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowStateEnvelope<T, R> {
    /// Envelope contract version.
    pub schema_version: u32,
    /// Retained workflow state forwarded explicitly between nodes.
    pub state: T,
    /// Narrow request or result owned by the current node boundary.
    pub value: R,
    /// Large retained values represented by typed references instead of inline copies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactReference>,
}

impl<T, R> WorkflowStateEnvelope<T, R> {
    /// Construct the current state envelope without artifact references.
    #[must_use]
    pub const fn new(state: T, value: R) -> Self {
        Self {
            schema_version: WORKFLOW_STATE_ENVELOPE_VERSION,
            state,
            value,
            artifacts: Vec::new(),
        }
    }

    /// Attach explicit artifact references for large retained values.
    #[must_use]
    pub fn with_artifacts(mut self, artifacts: Vec<ArtifactReference>) -> Self {
        self.artifacts = artifacts;
        self
    }
}

/// Rewrap one validated owner result with retained state and artifacts.
///
/// # Errors
///
/// Returns an error when the complete result does not match `complete_output`.
pub fn rewrap_workflow_state_envelope(
    parts: &WorkflowStateEnvelopeParts,
    owner_output: &serde_json::Value,
    complete_output: &ValueSchema,
) -> Result<serde_json::Value, WorkflowError> {
    let value = serde_json::json!({
        "schema_version": WORKFLOW_STATE_ENVELOPE_VERSION,
        "state": parts.state.clone(),
        "value": owner_output.clone(),
        "artifacts": parts.artifacts.clone(),
    });
    complete_output.validate_value("state_envelope.output", &value)?;
    Ok(value)
}

/// Current typed repeat-outcome contract version.
pub const WORKFLOW_REPEAT_OUTCOME_VERSION: u32 = 1;

/// Repeat behavior when its effective durable iteration bound is reached.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRepeatExhaustionPolicy {
    /// Preserve existing behavior by failing the run.
    #[default]
    Fail,
    /// Complete with a typed `iteration_limit_reached` result.
    EmitOutcome,
}

/// Stable reason carried by a typed repeat outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRepeatOutcomeKind {
    /// The predicate cleared before the effective iteration bound.
    ConditionCleared,
    /// The predicate remained true at the effective iteration bound.
    IterationLimitReached,
}

/// Generic typed result emitted by repeat outcome mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowRepeatOutcome<T> {
    /// Repeat-outcome contract version.
    pub version: u32,
    /// Stable terminal repeat result.
    pub outcome: WorkflowRepeatOutcomeKind,
    /// Runtime-owned number of completed body iterations.
    pub iterations_completed: u64,
    /// Definition-level maximum iterations.
    pub max_iterations: u64,
    /// Effective run-level cycle cap.
    pub cycle_cap: u64,
    /// Minimum of definition and run-level limits.
    pub effective_iteration_bound: u64,
    /// Last retained typed body value.
    pub value: T,
}

/// Stable fan-out result envelope contract version.
pub const WORKFLOW_FAN_OUT_RESULT_VERSION: u32 = 1;

/// One homogeneous fan-out member, canonically ordered by original input index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowFanOutMember<T> {
    /// Zero-based original input index.
    pub index: u32,
    /// Typed member output.
    pub value: T,
}

/// Canonical durable fan-out output shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowFanOutResult<T> {
    /// Fan-out result contract version.
    pub version: u32,
    /// Members sorted strictly by ascending original input index.
    pub members: Vec<WorkflowFanOutMember<T>>,
}

impl<T> WorkflowFanOutResult<T> {
    /// Construct and validate canonical fan-out ordering.
    ///
    /// # Errors
    ///
    /// Returns an error when indices are not the exact contiguous sequence `0..members.len()`.
    pub fn new(members: Vec<WorkflowFanOutMember<T>>) -> Result<Self, WorkflowError> {
        for (expected, member) in members.iter().enumerate() {
            if usize::try_from(member.index).ok() != Some(expected) {
                return Err(WorkflowError::Build {
                    path: "fan_out.members".to_string(),
                    message: "fan-out members must be contiguous and ordered by input index"
                        .to_string(),
                });
            }
        }
        Ok(Self {
            version: WORKFLOW_FAN_OUT_RESULT_VERSION,
            members,
        })
    }
}

/// Stable durable transform contract version.
pub const WORKFLOW_TRANSFORM_VERSION: u32 = 1;

/// Durable transform source containing the output that selected the successor edge.
pub const WORKFLOW_TRANSFORM_SOURCE_CURRENT: &str = "current";
/// Durable transform source containing the immutable workflow run input.
pub const WORKFLOW_TRANSFORM_SOURCE_STATE: &str = "state";
/// Durable transform source containing the left member of a completed parallel join.
pub const WORKFLOW_TRANSFORM_SOURCE_JOIN_LEFT: &str = "join.left";
/// Durable transform source containing the right member of a completed parallel join.
pub const WORKFLOW_TRANSFORM_SOURCE_JOIN_RIGHT: &str = "join.right";

const MAX_TRANSFORM_DEPTH: usize = 16;
const MAX_TRANSFORM_OPERATIONS: usize = 256;
const MAX_TRANSFORM_FIELDS: usize = 256;
const MAX_TRANSFORM_VALUE_BYTES: usize = 1_048_576;

/// Stable plugin workflow-block service interface.
pub const WORKFLOW_BLOCK_INTERFACE_ID: &str = "bcode.workflow-block/v1";

/// Versioned host envelope for one exact plugin-owned workflow-block invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowBlockInvocation {
    pub version: u32,
    pub dispatch_identity: String,
    pub workspace_root: std::path::PathBuf,
    pub input: serde_json::Value,
}

impl WorkflowBlockInvocation {
    /// Current workflow-block invocation envelope version.
    pub const VERSION: u32 = 1;

    /// Decode and validate a typed block input.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsupported envelope version, non-absolute workspace root, invalid
    /// dispatch identity, or input schema mismatch.
    pub fn typed_input<T: serde::de::DeserializeOwned>(&self) -> Result<T, String> {
        if self.version != Self::VERSION
            || self.dispatch_identity.trim().is_empty()
            || self.dispatch_identity.len() > 4_096
            || !self.workspace_root.is_absolute()
        {
            return Err("workflow block invocation envelope is invalid".to_string());
        }
        serde_json::from_value(self.input.clone()).map_err(|error| error.to_string())
    }
}

/// Side-effect declaration for one plugin-owned workflow block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowBlockEffect {
    ReadOnly,
    Mutating,
}

/// Authorization facts required before a block may be invoked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowBlockAuthorization {
    /// Maximum tool capability required by the block.
    pub capability: WorkflowToolCapability,
    /// Whether an exact scoped grant is required even when the initiating context is sufficient.
    #[serde(default)]
    pub explicit_grant_required: bool,
}

/// Durable idempotency and restart reconciliation declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowBlockReconciliation {
    /// Repeating the same dispatch identity is safe and byte-equivalent.
    IdempotentReplay,
    /// The owner returns a durable receipt that can be observed after restart.
    ReceiptStatus,
    /// An unknown outcome must stop for explicit repair.
    RepairRequired,
}

/// Stable owner-neutral automatic retry eligibility contract version.
pub const WORKFLOW_AUTOMATIC_RETRY_POLICY_VERSION: u32 = 1;

/// Durable failure classification used only to decide whether automatic retry is safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomaticRetryFailureKind {
    /// The owner was unavailable before accepting external work.
    OwnerUnavailableBeforeAcceptance,
    /// A trustworthy owner receipt reports a terminal failure that the owner declares retryable.
    OwnerReportedRetryable,
    /// Cancellation is terminal and requires no retry.
    Cancellation,
    /// The configured timeout class is terminal.
    TerminalTimeout,
    /// Approval was denied.
    ApprovalDenied,
    /// Input or output failed its declared schema.
    SchemaFailure,
    /// A mutation may have happened but cannot be proved either way.
    AmbiguousMutation,
    /// The owner reports a non-retryable terminal failure.
    TerminalFailure,
}

/// Persisted facts required to decide automatic retry eligibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomaticRetryEligibility {
    /// Retry policy contract version.
    pub version: u32,
    /// Side-effect class of the failed node.
    pub effect: WorkflowBlockEffect,
    /// Owner reconciliation contract.
    pub reconciliation: WorkflowBlockReconciliation,
    /// Stable failure classification.
    pub failure: AutomaticRetryFailureKind,
    /// Number of attempts already admitted for this activation.
    pub attempts_completed: u32,
    /// Definition-level maximum attempts.
    pub max_attempts: u32,
    /// Run-level maximum attempts per activation.
    pub retry_cap: u32,
}

/// Stable reason an activation cannot be retried automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomaticRetryIneligibleReason {
    UnsupportedPolicyVersion,
    InvalidLimit,
    AttemptsExhausted,
    Cancellation,
    TerminalTimeout,
    ApprovalDenied,
    SchemaFailure,
    AmbiguousMutation,
    TerminalFailure,
    UnsafeEffectOrReconciliation,
}

/// Result of evaluating the finite owner-neutral automatic retry policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum AutomaticRetryDecision {
    /// Retry is safe; a durable scheduler may create exactly this next attempt later.
    Eligible { next_attempt: u32 },
    /// Automatic retry is forbidden for the stable reason.
    Ineligible {
        reason: AutomaticRetryIneligibleReason,
    },
}

/// Evaluate automatic retry safety from persisted bounded facts.
///
/// This function does not schedule or sleep. Production capabilities continue to reject automatic
/// retry until durable next-attempt/backoff scheduling exists.
#[must_use]
pub const fn automatic_retry_decision(
    eligibility: AutomaticRetryEligibility,
) -> AutomaticRetryDecision {
    use AutomaticRetryFailureKind as Failure;
    use AutomaticRetryIneligibleReason as Reason;
    if eligibility.version != WORKFLOW_AUTOMATIC_RETRY_POLICY_VERSION {
        return AutomaticRetryDecision::Ineligible {
            reason: Reason::UnsupportedPolicyVersion,
        };
    }
    if eligibility.max_attempts == 0 || eligibility.retry_cap == 0 {
        return AutomaticRetryDecision::Ineligible {
            reason: Reason::InvalidLimit,
        };
    }
    let effective_limit = if eligibility.max_attempts < eligibility.retry_cap {
        eligibility.max_attempts
    } else {
        eligibility.retry_cap
    };
    let Some(next_attempt) = eligibility.attempts_completed.checked_add(1) else {
        return AutomaticRetryDecision::Ineligible {
            reason: Reason::AttemptsExhausted,
        };
    };
    if next_attempt > effective_limit {
        return AutomaticRetryDecision::Ineligible {
            reason: Reason::AttemptsExhausted,
        };
    }
    let excluded = match eligibility.failure {
        Failure::Cancellation => Some(Reason::Cancellation),
        Failure::TerminalTimeout => Some(Reason::TerminalTimeout),
        Failure::ApprovalDenied => Some(Reason::ApprovalDenied),
        Failure::SchemaFailure => Some(Reason::SchemaFailure),
        Failure::AmbiguousMutation => Some(Reason::AmbiguousMutation),
        Failure::TerminalFailure => Some(Reason::TerminalFailure),
        Failure::OwnerUnavailableBeforeAcceptance | Failure::OwnerReportedRetryable => None,
    };
    if let Some(reason) = excluded {
        return AutomaticRetryDecision::Ineligible { reason };
    }
    let safe = match eligibility.failure {
        Failure::OwnerUnavailableBeforeAcceptance => true,
        Failure::OwnerReportedRetryable => match eligibility.effect {
            WorkflowBlockEffect::ReadOnly => matches!(
                eligibility.reconciliation,
                WorkflowBlockReconciliation::IdempotentReplay
                    | WorkflowBlockReconciliation::ReceiptStatus
            ),
            WorkflowBlockEffect::Mutating => matches!(
                eligibility.reconciliation,
                WorkflowBlockReconciliation::ReceiptStatus
            ),
        },
        Failure::Cancellation
        | Failure::TerminalTimeout
        | Failure::ApprovalDenied
        | Failure::SchemaFailure
        | Failure::AmbiguousMutation
        | Failure::TerminalFailure => false,
    };
    if safe {
        AutomaticRetryDecision::Eligible { next_attempt }
    } else {
        AutomaticRetryDecision::Ineligible {
            reason: Reason::UnsafeEffectOrReconciliation,
        }
    }
}

/// One real plugin-owned workflow block contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowBlockDefinition {
    pub block_id: String,
    pub block_version: u32,
    pub plugin_id: String,
    pub operation: String,
    pub input: ValueSchema,
    pub output: ValueSchema,
    pub effect: WorkflowBlockEffect,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<ResourceClaim>,
    pub authorization: WorkflowBlockAuthorization,
    pub timeout_ms: u64,
    pub cancellation_supported: bool,
    pub reconciliation: WorkflowBlockReconciliation,
}

impl WorkflowBlockDefinition {
    /// Validate bounded identity, policy, timeout, and reconciliation invariants.
    ///
    /// # Errors
    ///
    /// Returns an error when identity/version/timeout is invalid, resource declarations conflict,
    /// cancellation is unsupported, or a mutating block claims unsafe replay.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        for (label, value) in [
            ("block_id", self.block_id.as_str()),
            ("plugin_id", self.plugin_id.as_str()),
            ("operation", self.operation.as_str()),
        ] {
            if value.trim().is_empty() || value.len() > 256 {
                return Err(WorkflowError::Build {
                    path: self.block_id.clone(),
                    message: format!("{label} must contain 1..=256 bytes"),
                });
            }
        }
        if self.block_version == 0 || self.timeout_ms == 0 {
            return Err(WorkflowError::Build {
                path: self.block_id.clone(),
                message: "block version and timeout must be positive".to_string(),
            });
        }
        if !self.cancellation_supported {
            return Err(WorkflowError::Build {
                path: self.block_id.clone(),
                message: "workflow blocks must support cancellation".to_string(),
            });
        }
        normalize_resource_claims(self.resources.clone())?;
        if self.effect == WorkflowBlockEffect::Mutating
            && self.reconciliation == WorkflowBlockReconciliation::IdempotentReplay
        {
            return Err(WorkflowError::Build {
                path: self.block_id.clone(),
                message:
                    "mutating blocks must use receipt status or repair-required reconciliation"
                        .to_string(),
            });
        }
        Ok(())
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
    /// Construct the durable schema identity for a Rust boundary type.
    ///
    /// # Panics
    ///
    /// Panics only if the generated `schemars` schema cannot be represented as JSON.
    #[must_use]
    pub fn of<T: JsonSchema>() -> Self {
        Self {
            type_name: std::any::type_name::<T>().to_string(),
            schema: serde_json::to_value(schemars::schema_for!(T))
                .expect("schemars workflow schema should serialize to JSON"),
        }
    }

    /// Validate one serialized value against this exact schema.
    ///
    /// # Errors
    ///
    /// Returns an error when the schema is unsupported or malformed, or the value does not match.
    pub fn validate_value(
        &self,
        path: &str,
        value: &serde_json::Value,
    ) -> Result<(), WorkflowError> {
        validate_value_against_schema(path, value, self)
    }
}

/// Current portable runtime-workflow authoring document version.
pub const WORKFLOW_AUTHORING_DOCUMENT_VERSION: u32 = 1;
/// Current generic authoring configuration-binding contract version.
pub const WORKFLOW_CONFIGURATION_BINDING_VERSION: u32 = 1;
/// Current optional authoring-presentation contract version.
pub const WORKFLOW_AUTHORING_PRESENTATION_VERSION: u32 = 1;
/// Supported runtime-authored JSON Schema dialect.
pub const WORKFLOW_AUTHORING_JSON_SCHEMA_DIALECT: &str =
    "https://json-schema.org/draft/2020-12/schema";
/// Maximum serialized authoring-document size.
pub const MAX_WORKFLOW_AUTHORING_DOCUMENT_BYTES: usize = 1_048_576;
/// Maximum serialized size of one runtime-defined schema.
pub const MAX_WORKFLOW_AUTHORING_SCHEMA_BYTES: usize = 131_072;
/// Maximum nesting depth of a runtime-defined schema or authoring JSON value.
pub const MAX_WORKFLOW_AUTHORING_JSON_DEPTH: usize = 64;
/// Maximum combined object-property declarations in one runtime-defined schema.
pub const MAX_WORKFLOW_AUTHORING_SCHEMA_PROPERTIES: usize = 4_096;
/// Maximum combined enum members in one runtime-defined schema.
pub const MAX_WORKFLOW_AUTHORING_SCHEMA_ENUM_VALUES: usize = 4_096;
/// Maximum local reference occurrences in one runtime-defined schema.
pub const MAX_WORKFLOW_AUTHORING_SCHEMA_REFERENCES: usize = 4_096;

const MAX_WORKFLOW_AUTHORING_ID_BYTES: usize = 256;
const MAX_WORKFLOW_AUTHORING_TITLE_BYTES: usize = 256;
const MAX_WORKFLOW_AUTHORING_DESCRIPTION_BYTES: usize = 4_096;
const MAX_WORKFLOW_AUTHORING_LABELS: usize = 32;
const MAX_WORKFLOW_AUTHORING_BINDINGS: usize = 4_096;
const MAX_WORKFLOW_AUTHORING_REQUIREMENTS: usize = 4_096;
const MAX_WORKFLOW_AUTHORING_PRESENTATION_NAMESPACES: usize = 32;
const MAX_WORKFLOW_AUTHORING_PRESENTATION_BYTES: usize = 131_072;
/// Portable workflow-authoring catalog contract version.
pub const WORKFLOW_AUTHORING_CATALOG_VERSION: u32 = 1;
/// Portable workflow compilation preview contract version.
pub const WORKFLOW_COMPILATION_PREVIEW_VERSION: u32 = 1;
/// Portable authored-workflow export bundle contract version.
pub const WORKFLOW_EXPORT_BUNDLE_VERSION: u32 = 1;
/// Portable authored-workflow import preview contract version.
pub const WORKFLOW_IMPORT_PREVIEW_VERSION: u32 = 1;
/// Normalized authored-workflow application-operation fact version.
pub const WORKFLOW_APPLICATION_OPERATION_FACTS_VERSION: u32 = 1;

/// Authenticated class of actor requesting an authored-workflow application operation.
///
/// This identity is assigned by the application boundary. It is distinct from untrusted producer
/// provenance embedded in authored content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowApplicationActorKind {
    /// A client connected through the authenticated local application boundary.
    LocalClient,
    /// A loaded plugin acting through its declared application capability.
    Plugin,
    /// A daemon-owned maintenance or lifecycle service.
    Service,
}

/// Authenticated actor identity used for authored-workflow application authorization.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowApplicationActor {
    /// Actor class assigned by the application boundary.
    pub kind: WorkflowApplicationActorKind,
    /// Stable bounded identity in the actor class' namespace.
    pub actor_id: String,
}

impl WorkflowApplicationActor {
    /// Validate this normalized actor identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the actor identity is malformed.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        validate_authoring_id("actor.actor_id", &self.actor_id)
    }
}

/// Side-effecting authored-workflow operation evaluated by application policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowApplicationOperation {
    CreateWorkflow,
    UpdateWorkflowMetadata,
    ArchiveWorkflow,
    UnarchiveWorkflow,
    CreateDraft,
    ForkDraft,
    UpdateDraft,
    DiscardDraft,
    PublishDraft,
    ActivateRevision,
    CreatePreset,
    UpdatePreset,
    DeletePreset,
    ImportWorkflow,
    ImportDraft,
    ImportRevision,
    StartRevision,
    StartActiveRevision,
    StartPreset,
    PublishAndStart,
}

impl WorkflowApplicationOperation {
    const fn requires_draft(self) -> bool {
        matches!(
            self,
            Self::CreateDraft
                | Self::ForkDraft
                | Self::UpdateDraft
                | Self::DiscardDraft
                | Self::ImportDraft
                | Self::ImportRevision
                | Self::PublishDraft
                | Self::PublishAndStart
        )
    }

    const fn requires_revision(self) -> bool {
        matches!(self, Self::ActivateRevision | Self::StartRevision)
    }

    const fn requires_preset(self) -> bool {
        matches!(
            self,
            Self::CreatePreset | Self::UpdatePreset | Self::DeletePreset | Self::StartPreset
        )
    }

    const fn permits_activation(self) -> bool {
        matches!(
            self,
            Self::PublishDraft
                | Self::ActivateRevision
                | Self::ImportWorkflow
                | Self::PublishAndStart
        )
    }

    const fn executes(self) -> bool {
        matches!(
            self,
            Self::StartRevision
                | Self::StartActiveRevision
                | Self::StartPreset
                | Self::PublishAndStart
        )
    }
}

/// Canonical policy input for one side-effecting authored-workflow application operation.
///
/// Facts are normalized by the application boundary and do not contain renderer, transport,
/// persistence, provider-private, or tool-call types. Producer provenance remains diagnostic and
/// cannot replace the authenticated actor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowApplicationOperationFacts {
    /// Fact schema version.
    pub version: u32,
    /// Exact requested operation.
    pub operation: WorkflowApplicationOperation,
    /// Authenticated actor assigned by the application boundary.
    pub actor: WorkflowApplicationActor,
    /// Target logical workflow.
    pub workflow_id: String,
    /// Exact draft target when the operation acts on a draft.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_id: Option<String>,
    /// Exact immutable revision target when known before mutation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    /// Exact preset target when the operation acts on a preset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset_id: Option<String>,
    /// Untrusted producer provenance retained only as a policy fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer: Option<WorkflowProducerProvenance>,
    /// Exact referenced capabilities and contracts known before the side effect.
    #[serde(default)]
    pub requirements: WorkflowRequirementSummary,
    /// Aggregate effects, reconciliation classes, and resources known before the side effect.
    #[serde(default)]
    pub effects: WorkflowEffectSummary,
    /// Whether the request can update the active revision pointer.
    pub activates: bool,
    /// Whether the request can admit execution after any preceding mutation completes.
    pub executes: bool,
}

impl WorkflowApplicationOperationFacts {
    /// Validate canonical authored-workflow application-operation facts.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsupported fact version, malformed identity, missing or unrelated
    /// target identity, invalid aggregate facts, or activation/execution flags inconsistent with
    /// the exact operation.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if self.version != WORKFLOW_APPLICATION_OPERATION_FACTS_VERSION {
            return Err(authoring_error(
                "application_operation.version",
                format!(
                    "unsupported application operation fact version {}; expected {}",
                    self.version, WORKFLOW_APPLICATION_OPERATION_FACTS_VERSION
                ),
            ));
        }
        self.actor.validate()?;
        validate_authoring_id("application_operation.workflow_id", &self.workflow_id)?;
        validate_optional_operation_id(
            "application_operation.draft_id",
            self.draft_id.as_deref(),
            self.operation.requires_draft(),
        )?;
        validate_optional_operation_id(
            "application_operation.preset_id",
            self.preset_id.as_deref(),
            self.operation.requires_preset(),
        )?;
        if self.operation.requires_revision() != self.revision.is_some() {
            return Err(authoring_error(
                "application_operation.revision",
                if self.operation.requires_revision() {
                    "this operation requires an exact revision"
                } else {
                    "this operation must not include an unrelated revision"
                },
            ));
        }
        if self.revision == Some(0) {
            return Err(authoring_error(
                "application_operation.revision",
                "published revision must be greater than zero",
            ));
        }
        if self.activates && !self.operation.permits_activation() {
            return Err(authoring_error(
                "application_operation.activates",
                "this operation cannot activate a revision",
            ));
        }
        if self.operation == WorkflowApplicationOperation::ActivateRevision && !self.activates {
            return Err(authoring_error(
                "application_operation.activates",
                "activate_revision must declare activation",
            ));
        }
        if self.executes != self.operation.executes() {
            return Err(authoring_error(
                "application_operation.executes",
                "execution intent does not match the exact operation",
            ));
        }
        if let Some(producer) = &self.producer {
            producer.validate()?;
        }
        self.requirements.validate()?;
        self.effects.validate()?;
        Ok(())
    }
}

fn validate_optional_operation_id(
    path: &str,
    value: Option<&str>,
    required: bool,
) -> Result<(), WorkflowError> {
    match (value, required) {
        (Some(value), true) => validate_authoring_id(path, value),
        (None, true) => Err(authoring_error(
            path,
            "this operation requires an exact identity",
        )),
        (Some(_), false) => Err(authoring_error(
            path,
            "this operation must not include an unrelated identity",
        )),
        (None, false) => Ok(()),
    }
}

/// Stable keyset cursor for authored lists ordered by newest update then identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowAuthoringListCursor {
    /// Update timestamp of the final item in the previous page.
    pub updated_at_ms: u64,
    /// Stable identity tie-breaker of the final item in the previous page.
    pub entity_id: String,
}

impl WorkflowAuthoringListCursor {
    /// Validate this portable list cursor.
    ///
    /// # Errors
    ///
    /// Returns an error when the stable entity identity is malformed.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        validate_authoring_id("cursor.entity_id", &self.entity_id)
    }
}

/// Stable keyset cursor for immutable revisions ordered newest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRevisionListCursor {
    /// Final revision returned by the previous page.
    pub revision: u64,
}

impl WorkflowRevisionListCursor {
    /// Validate this portable revision cursor.
    ///
    /// # Errors
    ///
    /// Returns an error when the revision is zero.
    pub fn validate(self) -> Result<(), WorkflowError> {
        if self.revision == 0 {
            return Err(authoring_error(
                "cursor.revision",
                "revision cursor must be positive",
            ));
        }
        Ok(())
    }
}

/// Portable exact immutable authored-workflow revision used by export/import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPortableRevision {
    pub identity: WorkflowRevisionIdentity,
    pub source_checksum_sha256: String,
    pub executable_source_checksum_sha256: String,
    pub definition_identity: WorkflowDefinitionIdentity,
    pub document: WorkflowAuthoringDocument,
    pub producer: WorkflowProducerProvenance,
    pub published_at_ms: u64,
}

impl WorkflowPortableRevision {
    /// Validate exact immutable revision identity and content digests.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity/content or mismatched source/definition digests.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        self.identity.validate()?;
        self.document.validate()?;
        self.producer.validate()?;
        if self.document.workflow_id != self.identity.workflow_id
            || self.document.source_digest_sha256()? != self.source_checksum_sha256
            || self.document.executable_source_digest_sha256()?
                != self.executable_source_checksum_sha256
        {
            return Err(authoring_error(
                "revision",
                "portable revision identity or source digests are inconsistent",
            ));
        }
        if self.definition_identity.kind.trim().is_empty()
            || self.definition_identity.definition_id.trim().is_empty()
            || self.definition_identity.definition_version == 0
        {
            return Err(authoring_error(
                "revision.definition_identity",
                "portable revision definition identity is malformed",
            ));
        }
        Ok(())
    }
}

/// Exact immutable dependency carried by portable export/import contracts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDependencyManifestEntry {
    /// Calling node identity in the exported definition.
    pub node_id: String,
    /// Exact immutable target.
    pub target: WorkflowCallTarget,
}

/// Derive the exact dependency manifest from one validated definition.
///
/// # Errors
///
/// Returns an error when the definition or a workflow-call configuration is invalid.
pub fn workflow_dependency_manifest(
    definition: &WorkflowDefinition,
) -> Result<Vec<WorkflowDependencyManifestEntry>, WorkflowError> {
    definition.validate()?;
    definition
        .nodes
        .values()
        .filter(|node| node.kind == NodeKind::WorkflowCall)
        .map(|node| {
            let call: WorkflowCallConfiguration =
                serde_json::from_value(node.configuration.clone()).map_err(|error| {
                    authoring_error(
                        format!("definition.nodes.{}.configuration", node.id),
                        format!("workflow call configuration is invalid: {error}"),
                    )
                })?;
            call.validate()?;
            Ok(WorkflowDependencyManifestEntry {
                node_id: node.id.clone(),
                target: call.target,
            })
        })
        .collect()
}

/// Canonical portable export bundle for one exact immutable revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowExportBundle {
    pub version: u32,
    pub revision: WorkflowPortableRevision,
    /// Exact dependency graph roots referenced by the exported revision.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<WorkflowDependencyManifestEntry>,
}

impl WorkflowExportBundle {
    /// Validate the bundle version and exact immutable content.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported future versions or inconsistent revision content.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if self.version != WORKFLOW_EXPORT_BUNDLE_VERSION {
            return Err(authoring_error(
                "export.version",
                format!(
                    "unsupported workflow export version {}; expected {}",
                    self.version, WORKFLOW_EXPORT_BUNDLE_VERSION
                ),
            ));
        }
        self.revision.validate()?;
        let expected = workflow_dependency_manifest(&self.revision.document.definition)?;
        if self.dependencies != expected {
            return Err(authoring_error(
                "export.dependencies",
                "export dependency manifest must exactly match workflow call nodes",
            ));
        }
        Ok(())
    }
}

/// Side-effect-free import validation and compilation result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowImportPreview {
    pub version: u32,
    pub bundle_version: u32,
    pub source_identity: WorkflowRevisionIdentity,
    pub target_workflow_id: String,
    pub compilation: WorkflowCompilationPreview,
}

/// Stable logical identity for one runtime-authored workflow.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowIdentity {
    /// Opaque stable workflow identity.
    pub workflow_id: String,
}

impl WorkflowIdentity {
    /// Validate this logical workflow identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the identity is empty, too large, or contains unsupported bytes.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        validate_authoring_id("workflow_id", &self.workflow_id)
    }
}

/// Stable identity for one mutable authored-workflow draft.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDraftIdentity {
    /// Owning logical workflow identity.
    pub workflow_id: String,
    /// Opaque draft identity within the logical workflow.
    pub draft_id: String,
}

impl WorkflowDraftIdentity {
    /// Validate this draft identity.
    ///
    /// # Errors
    ///
    /// Returns an error when either identity is malformed.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        validate_authoring_id("workflow_id", &self.workflow_id)?;
        validate_authoring_id("draft_id", &self.draft_id)
    }
}

/// Exact immutable published revision identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRevisionIdentity {
    /// Owning logical workflow identity.
    pub workflow_id: String,
    /// Monotonically increasing published revision number.
    pub revision: u64,
}

impl WorkflowRevisionIdentity {
    /// Validate this published revision identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the workflow identity is malformed or the revision is zero.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        validate_authoring_id("workflow_id", &self.workflow_id)?;
        if self.revision == 0 {
            return Err(authoring_error(
                "revision",
                "published revision must be greater than zero",
            ));
        }
        Ok(())
    }
}

/// Normalized class of producer that created authoring state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowProducerKind {
    /// Direct human-authored input.
    #[default]
    Human,
    /// Bcode command-line producer.
    Cli,
    /// Portable frontend producer.
    Frontend,
    /// SDK client producer.
    Sdk,
    /// Plugin producer.
    Plugin,
    /// Untrusted generated producer, including AI-backed generation.
    Generated,
}

/// Bounded diagnostic provenance for authored workflow state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowProducerProvenance {
    /// Producer class. This value never grants trust or authorization.
    pub kind: WorkflowProducerKind,
    /// Optional bounded producer identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer_id: Option<String>,
    /// Optional source revision from which this state was derived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<WorkflowRevisionIdentity>,
}

impl WorkflowProducerProvenance {
    /// Validate bounded diagnostic provenance.
    ///
    /// # Errors
    ///
    /// Returns an error when a producer ID or source revision is malformed.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if let Some(producer_id) = &self.producer_id {
            validate_authoring_id("producer.producer_id", producer_id)?;
        }
        if let Some(source_revision) = &self.source_revision {
            source_revision.validate()?;
        }
        Ok(())
    }
}

/// User-facing metadata that does not determine execution semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowAuthoringMetadata {
    /// Bounded display title.
    pub title: String,
    /// Optional bounded description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Bounded deterministic labels for discovery.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

impl WorkflowAuthoringMetadata {
    fn validate(&self) -> Result<(), WorkflowError> {
        if self.title.trim().is_empty() || self.title.len() > MAX_WORKFLOW_AUTHORING_TITLE_BYTES {
            return Err(authoring_error(
                "metadata.title",
                format!("title must contain 1..={MAX_WORKFLOW_AUTHORING_TITLE_BYTES} bytes"),
            ));
        }
        if self
            .description
            .as_ref()
            .is_some_and(|description| description.len() > MAX_WORKFLOW_AUTHORING_DESCRIPTION_BYTES)
        {
            return Err(authoring_error(
                "metadata.description",
                format!("description exceeds {MAX_WORKFLOW_AUTHORING_DESCRIPTION_BYTES} bytes"),
            ));
        }
        if self.labels.len() > MAX_WORKFLOW_AUTHORING_LABELS {
            return Err(authoring_error(
                "metadata.labels",
                format!("labels exceed {MAX_WORKFLOW_AUTHORING_LABELS} entries"),
            ));
        }
        for (key, value) in &self.labels {
            validate_authoring_id("metadata.labels.key", key)?;
            if value.trim().is_empty() || value.len() > MAX_WORKFLOW_AUTHORING_ID_BYTES {
                return Err(authoring_error(
                    "metadata.labels.value",
                    format!(
                        "label values must contain 1..={MAX_WORKFLOW_AUTHORING_ID_BYTES} bytes"
                    ),
                ));
            }
        }
        Ok(())
    }
}

/// Optional namespaced authoring hints that never affect executable identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowAuthoringPresentation {
    /// Presentation contract version.
    pub version: u32,
    /// Producer-owned portable or ignorable presentation payloads.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub namespaces: BTreeMap<String, serde_json::Value>,
}

impl WorkflowAuthoringPresentation {
    fn validate(&self) -> Result<(), WorkflowError> {
        if self.version != WORKFLOW_AUTHORING_PRESENTATION_VERSION {
            return Err(authoring_error(
                "presentation.version",
                format!(
                    "unsupported presentation version {}; expected {}",
                    self.version, WORKFLOW_AUTHORING_PRESENTATION_VERSION
                ),
            ));
        }
        if self.namespaces.len() > MAX_WORKFLOW_AUTHORING_PRESENTATION_NAMESPACES {
            return Err(authoring_error(
                "presentation.namespaces",
                format!(
                    "presentation namespaces exceed {MAX_WORKFLOW_AUTHORING_PRESENTATION_NAMESPACES} entries"
                ),
            ));
        }
        for (namespace, value) in &self.namespaces {
            validate_authoring_id("presentation.namespace", namespace)?;
            validate_authoring_json_value("presentation.namespaces", value)?;
        }
        let bytes = serde_json::to_vec(self).map_err(|error| {
            authoring_error(
                "presentation",
                format!("presentation cannot be serialized: {error}"),
            )
        })?;
        if bytes.len() > MAX_WORKFLOW_AUTHORING_PRESENTATION_BYTES {
            return Err(authoring_error(
                "presentation",
                format!("presentation exceeds {MAX_WORKFLOW_AUTHORING_PRESENTATION_BYTES} bytes"),
            ));
        }
        Ok(())
    }
}

/// Portable upper bounds applied when a published workflow run is admitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRunLimitPolicy {
    /// Maximum optional run duration in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum_duration_ms: Option<u64>,
    /// Maximum total node attempts in a run.
    pub node_execution_cap: u32,
    /// Maximum concurrently running nodes.
    pub concurrency_cap: u32,
    /// Maximum cycle/repeat activations.
    pub cycle_cap: u32,
    /// Maximum attempts per activation.
    pub retry_cap: u32,
}

impl Default for WorkflowRunLimitPolicy {
    fn default() -> Self {
        Self {
            maximum_duration_ms: None,
            node_execution_cap: 1_000,
            concurrency_cap: 8,
            cycle_cap: 100,
            retry_cap: 3,
        }
    }
}

impl WorkflowRunLimitPolicy {
    fn validate(&self) -> Result<(), WorkflowError> {
        if self.maximum_duration_ms == Some(0)
            || self.node_execution_cap == 0
            || self.concurrency_cap == 0
            || self.cycle_cap == 0
            || self.retry_cap == 0
            || self.concurrency_cap > self.node_execution_cap
        {
            return Err(authoring_error(
                "run_limits",
                "run-limit values must be non-zero and concurrency cannot exceed the node-execution cap",
            ));
        }
        Ok(())
    }
}

/// Declared target populated from runtime workflow configuration during compilation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowConfigurationTarget {
    /// Populate a declared field in one node's configuration object.
    NodeConfiguration { node_id: String, path: String },
    /// Populate a declared agent selection field.
    AgentSelection { node_id: String, field: String },
    /// Populate one declared indexed skill selection.
    SkillSelection { node_id: String, index: usize },
    /// Populate a plugin-block input default field.
    PluginBlockInput { node_id: String, path: String },
    /// Populate a declared edge predicate or transform field.
    EdgeConfiguration { edge_index: usize, path: String },
    /// Populate one portable run-limit field.
    RunLimit { field: String },
    /// Populate one initial workflow input-default field.
    InputDefault { path: String },
}

/// One bounded versioned generic configuration binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowConfigurationBinding {
    /// Binding contract version.
    pub version: u32,
    /// Dotted path in the validated configuration value.
    pub configuration_path: String,
    /// Explicit declared compilation target.
    pub target: WorkflowConfigurationTarget,
    /// Optional bounded deterministic transform.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<WorkflowTransform>,
}

impl WorkflowConfigurationBinding {
    fn validate(&self, definition: &WorkflowDefinition) -> Result<(), WorkflowError> {
        if self.version != WORKFLOW_CONFIGURATION_BINDING_VERSION {
            return Err(authoring_error(
                "bindings.version",
                format!(
                    "unsupported binding version {}; expected {}",
                    self.version, WORKFLOW_CONFIGURATION_BINDING_VERSION
                ),
            ));
        }
        validate_authoring_path("bindings.configuration_path", &self.configuration_path)?;
        match &self.target {
            WorkflowConfigurationTarget::NodeConfiguration { node_id, path } => {
                validate_authoring_node_target(definition, node_id, path)?;
            }
            WorkflowConfigurationTarget::PluginBlockInput { node_id, path } => {
                validate_authoring_node_target(definition, node_id, path)?;
                if definition
                    .node(node_id)
                    .is_none_or(|node| node.kind != NodeKind::PluginBlock)
                {
                    return Err(authoring_error(
                        "bindings.target.node_id",
                        format!("binding target '{node_id}' is not a plugin-block node"),
                    ));
                }
            }
            WorkflowConfigurationTarget::AgentSelection { node_id, field } => {
                validate_authoring_node_target(definition, node_id, field)?;
                if !matches!(field.as_str(), "agent_profile" | "provider" | "model") {
                    return Err(authoring_error(
                        "bindings.target.field",
                        format!("unsupported agent selection field '{field}'"),
                    ));
                }
                if definition
                    .node(node_id)
                    .is_none_or(|node| node.kind != NodeKind::Agent)
                {
                    return Err(authoring_error(
                        "bindings.target.node_id",
                        format!("binding target '{node_id}' is not an agent node"),
                    ));
                }
            }
            WorkflowConfigurationTarget::SkillSelection { node_id, index } => {
                validate_authoring_id("bindings.target.node_id", node_id)?;
                if *index >= 32 {
                    return Err(authoring_error(
                        "bindings.target.index",
                        "skill selection index must be less than 32",
                    ));
                }
                if definition
                    .node(node_id)
                    .is_none_or(|node| node.kind != NodeKind::Agent)
                {
                    return Err(authoring_error(
                        "bindings.target.node_id",
                        format!("binding target '{node_id}' is not an agent node"),
                    ));
                }
            }
            WorkflowConfigurationTarget::EdgeConfiguration { edge_index, path } => {
                if *edge_index >= definition.edges.len() {
                    return Err(authoring_error(
                        "bindings.target.edge_index",
                        format!("binding references unknown edge index {edge_index}"),
                    ));
                }
                validate_authoring_path("bindings.target.path", path)?;
            }
            WorkflowConfigurationTarget::RunLimit { field } => {
                if !matches!(
                    field.as_str(),
                    "maximum_duration_ms"
                        | "node_execution_cap"
                        | "concurrency_cap"
                        | "cycle_cap"
                        | "retry_cap"
                ) {
                    return Err(authoring_error(
                        "bindings.target.field",
                        format!("unknown run-limit field '{field}'"),
                    ));
                }
            }
            WorkflowConfigurationTarget::InputDefault { path } => {
                validate_authoring_path("bindings.target.path", path)?;
            }
        }
        if let Some(transform) = &self.transform {
            transform.validate()?;
            validate_runtime_value_schema("bindings.transform.output", &transform.output)?;
        }
        Ok(())
    }
}

/// Exact normalized requirements declared by an authored workflow.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRequirementSummary {
    /// Required production capability contract versions or named capabilities.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub capabilities: BTreeSet<String>,
    /// Required plugin identities.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub plugins: BTreeSet<String>,
    /// Required exact block references, encoded as stable owner/block/version identities.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub blocks: BTreeSet<String>,
    /// Required portable agent profile identities.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub agents: BTreeSet<String>,
    /// Required skill identities.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub skills: BTreeSet<String>,
}

impl WorkflowRequirementSummary {
    fn validate(&self) -> Result<(), WorkflowError> {
        let count = self.capabilities.len()
            + self.plugins.len()
            + self.blocks.len()
            + self.agents.len()
            + self.skills.len();
        if count > MAX_WORKFLOW_AUTHORING_REQUIREMENTS {
            return Err(authoring_error(
                "requirements",
                format!("requirements exceed {MAX_WORKFLOW_AUTHORING_REQUIREMENTS} entries"),
            ));
        }
        for value in self
            .capabilities
            .iter()
            .chain(&self.plugins)
            .chain(&self.blocks)
            .chain(&self.agents)
            .chain(&self.skills)
        {
            validate_authoring_id("requirements", value)?;
        }
        Ok(())
    }
}

/// Renderer-neutral aggregate effects exposed before publication or start.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowEffectSummary {
    /// Maximum tool capability requested by the compiled workflow.
    pub maximum_capability: WorkflowToolCapability,
    /// Exact block effect classes present in the workflow.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub block_effects: BTreeSet<WorkflowBlockEffect>,
    /// Exact restart reconciliation classes present in the workflow.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub reconciliation: BTreeSet<WorkflowBlockReconciliation>,
    /// Normalized aggregate resource claims.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<ResourceClaim>,
}

impl WorkflowEffectSummary {
    /// Return an effect summary with deterministic, duplicate-free resource claims.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.resources.sort();
        self.resources.dedup();
        self
    }

    /// Validate bounded normalized effect facts.
    ///
    /// # Errors
    ///
    /// Returns an error when a resource identity is empty, oversized, duplicated, or out of
    /// canonical order.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if self.resources.len() > MAX_WORKFLOW_AUTHORING_REQUIREMENTS {
            return Err(authoring_error(
                "effects.resources",
                format!("resource claims exceed {MAX_WORKFLOW_AUTHORING_REQUIREMENTS} entries"),
            ));
        }
        for resource in &self.resources {
            validate_authoring_id("effects.resources.resource", &resource.resource)?;
        }
        if self.resources.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(authoring_error(
                "effects.resources",
                "resource claims must be unique and in canonical order",
            ));
        }
        Ok(())
    }
}

/// Severity of one portable workflow-authoring validation diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowValidationSeverity {
    /// Publication or compilation cannot proceed.
    Error,
    /// Valid source has a non-blocking compatibility or availability concern.
    Warning,
}

/// One renderer-neutral source-addressed authoring diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowValidationDiagnostic {
    /// Stable machine-readable diagnostic code.
    pub code: String,
    /// Diagnostic severity.
    pub severity: WorkflowValidationSeverity,
    /// Dotted source-document path associated with the diagnostic.
    pub document_path: String,
    /// Bounded human-readable explanation.
    pub message: String,
    /// Bounded producer-neutral remediation guidance.
    pub remediation: String,
}

/// Side-effect-free validation result for one portable authoring document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowValidationReport {
    /// Authoring contract version validated by this report.
    pub authoring_version: u32,
    /// Whether no error-severity diagnostics were produced.
    pub valid: bool,
    /// Canonical complete-source digest when validation succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_digest_sha256: Option<String>,
    /// Canonical executable-source digest when validation succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable_source_digest_sha256: Option<String>,
    /// Stable diagnostics in deterministic order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<WorkflowValidationDiagnostic>,
}

impl WorkflowValidationReport {
    /// Return whether publication may continue to catalog resolution and compilation.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.valid
    }
}

/// Portable renderer-neutral production capability summary for authoring clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowAuthoringCapabilitySummary {
    pub capability_version: u32,
    pub definition_schema_version: u32,
    pub predicate_version: u32,
    pub transform_version: Option<u32>,
    pub automatic_retry_policy_version: Option<u32>,
    pub agent_configuration_version: u32,
    pub workflow_block_interface_version: u32,
    pub node_kinds: BTreeMap<String, WorkflowCapabilitySupport>,
    pub edge_kinds: BTreeMap<String, WorkflowCapabilitySupport>,
    pub parallel_join_policies: BTreeSet<ParallelFailurePolicy>,
    pub automatic_retry: WorkflowCapabilitySupport,
    pub fan_out: WorkflowCapabilitySupport,
    pub transforms: WorkflowCapabilitySupport,
    pub artifact_references: WorkflowCapabilitySupport,
    pub agent_execution_targets: BTreeSet<AgentExecutionTarget>,
    pub schema_dialects: BTreeSet<String>,
}

impl From<&WorkflowProductionCapabilities> for WorkflowAuthoringCapabilitySummary {
    fn from(capabilities: &WorkflowProductionCapabilities) -> Self {
        Self {
            capability_version: capabilities.capability_version,
            definition_schema_version: capabilities.definition_schema_version,
            predicate_version: capabilities.predicate_version,
            transform_version: capabilities.transform_version,
            automatic_retry_policy_version: capabilities.automatic_retry_policy_version,
            agent_configuration_version: capabilities.agent_configuration_version,
            workflow_block_interface_version: capabilities.workflow_block_interface_version,
            node_kinds: capabilities
                .node_kinds
                .iter()
                .map(|(kind, support)| (node_kind_name(*kind).to_string(), *support))
                .collect(),
            edge_kinds: capabilities
                .edge_kinds
                .iter()
                .map(|(kind, support)| (workflow_edge_kind_name(*kind).to_string(), *support))
                .collect(),
            parallel_join_policies: capabilities.parallel_join_policies.clone(),
            automatic_retry: capabilities.automatic_retry,
            fan_out: capabilities.fan_out,
            transforms: capabilities.transforms,
            artifact_references: capabilities.artifact_references,
            agent_execution_targets: capabilities.agent_execution_targets.clone(),
            schema_dialects: BTreeSet::from([WORKFLOW_AUTHORING_JSON_SCHEMA_DIALECT.to_string()]),
        }
    }
}

/// Portable catalog snapshot consumed by pure workflow authoring validation and compilation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowAuthoringCatalogSnapshot {
    /// Catalog contract version.
    pub version: u32,
    /// Portable durable-production capabilities represented by this snapshot.
    pub capabilities: WorkflowAuthoringCapabilitySummary,
    /// Loaded plugin identities available for authored references.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub plugins: BTreeSet<String>,
    /// Exact plugin block contracts keyed by [`workflow_block_catalog_key`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub blocks: BTreeMap<String, WorkflowBlockDefinition>,
    /// Exact immutable definitions available for child-call preview, keyed by compiled identity.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub workflow_definitions: BTreeMap<String, WorkflowDefinition>,
    /// Portable configured agent profile identities.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub agent_profiles: BTreeSet<String>,
    /// Portable available skill identities.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub skills: BTreeSet<String>,
}

/// Kind of authored requirement evaluated against a current catalog snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRequirementKind {
    Capability,
    Plugin,
    Block,
    Agent,
    Skill,
}

/// One missing current-host requirement, separate from immutable publication facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowUnavailableRequirement {
    pub kind: WorkflowRequirementKind,
    pub identity: String,
}

/// Bounded current-host availability report for immutable declared requirements.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRequirementAvailabilityReport {
    pub version: u32,
    pub available: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unavailable: Vec<WorkflowUnavailableRequirement>,
}

impl WorkflowAuthoringCatalogSnapshot {
    fn production_capabilities(&self) -> Result<WorkflowProductionCapabilities, WorkflowError> {
        let current = WorkflowProductionCapabilities::current();
        let expected = WorkflowAuthoringCapabilitySummary::from(&current);
        if self.capabilities != expected {
            return Err(authoring_error(
                "catalog.capabilities",
                "catalog production capabilities do not match the exact supported contract",
            ));
        }
        Ok(current)
    }

    /// Validate bounded catalog identity and exact block-key consistency.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsupported catalog version, malformed identities, excessive
    /// entries, invalid block contracts, or block keys that do not match their exact contract.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if self.version != WORKFLOW_AUTHORING_CATALOG_VERSION {
            return Err(authoring_error(
                "catalog.version",
                format!(
                    "unsupported workflow authoring catalog version {}; expected {}",
                    self.version, WORKFLOW_AUTHORING_CATALOG_VERSION
                ),
            ));
        }
        self.production_capabilities()?;
        let entry_count = self.plugins.len()
            + self.blocks.len()
            + self.workflow_definitions.len()
            + self.agent_profiles.len()
            + self.skills.len();
        if entry_count > MAX_WORKFLOW_AUTHORING_REQUIREMENTS {
            return Err(authoring_error(
                "catalog",
                format!("catalog exceeds {MAX_WORKFLOW_AUTHORING_REQUIREMENTS} entries"),
            ));
        }
        for value in self
            .plugins
            .iter()
            .chain(self.agent_profiles.iter())
            .chain(self.skills.iter())
        {
            validate_authoring_id("catalog.identity", value)?;
        }
        for (key, block) in &self.blocks {
            block.validate()?;
            validate_runtime_value_schema("catalog.blocks.input", &block.input)?;
            validate_runtime_value_schema("catalog.blocks.output", &block.output)?;
            if key != &workflow_block_catalog_key(block) {
                return Err(authoring_error(
                    "catalog.blocks",
                    format!("catalog block key '{key}' does not match its exact contract"),
                ));
            }
            if !self.plugins.contains(&block.plugin_id) {
                return Err(authoring_error(
                    "catalog.blocks",
                    format!(
                        "catalog block '{}' references unavailable plugin '{}'",
                        block.block_id, block.plugin_id
                    ),
                ));
            }
        }
        for (key, definition) in &self.workflow_definitions {
            definition.validate()?;
            let identity =
                WorkflowDefinitionIdentity::for_definition(definition.name.clone(), definition)?;
            if key != &identity.definition_id {
                return Err(authoring_error(
                    "catalog.workflow_definitions",
                    format!("catalog definition key '{key}' does not match exact content identity"),
                ));
            }
        }
        Ok(())
    }
}

/// Return the stable exact catalog key for one plugin workflow block.
#[must_use]
pub fn workflow_block_catalog_key(block: &WorkflowBlockDefinition) -> String {
    format!(
        "{}/{}@{}",
        block.plugin_id, block.block_id, block.block_version
    )
}

/// Portable renderer-neutral production admission result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowAuthoringProductionAdmission {
    pub capabilities: WorkflowAuthoringCapabilitySummary,
    pub diagnostics: Vec<WorkflowCapabilityDiagnostic>,
}

impl WorkflowAuthoringProductionAdmission {
    /// Return whether the exact compiled definition is fully supported.
    #[must_use]
    pub const fn is_supported(&self) -> bool {
        self.diagnostics.is_empty()
    }
}

impl From<&WorkflowProductionAdmission> for WorkflowAuthoringProductionAdmission {
    fn from(admission: &WorkflowProductionAdmission) -> Self {
        Self {
            capabilities: WorkflowAuthoringCapabilitySummary::from(&admission.capabilities),
            diagnostics: admission.diagnostics.clone(),
        }
    }
}

/// Exact authorization implications exposed before publication or execution.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPermissionPreview {
    /// Maximum tool capability requested by any compiled node.
    pub maximum_capability: WorkflowToolCapability,
    /// Exact parent/child node paths whose owner contract requires an explicit grant.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub explicit_grant_nodes: Vec<String>,
    /// Exact parent/child node paths that retain runtime mutation approval.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mutation_approval_nodes: Vec<String>,
}

/// Successful side-effect-free compilation result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCompiledAuthoringPreview {
    /// Canonical validated runtime configuration after defaults are applied.
    pub configuration: serde_json::Value,
    /// Exact normalized compiled definition.
    pub definition: WorkflowDefinition,
    /// Exact compiled definition identity.
    pub definition_identity: WorkflowDefinitionIdentity,
    /// Production admission result for the exact definition.
    pub production_admission: WorkflowAuthoringProductionAdmission,
    /// Exact resolved requirements, including references derived from compiled nodes.
    pub requirements: WorkflowRequirementSummary,
    /// Aggregate effect, resource, and reconciliation facts.
    pub effects: WorkflowEffectSummary,
    /// Exact authorization implications.
    pub permissions: WorkflowPermissionPreview,
    /// Bound portable run-limit policy.
    pub run_limits: WorkflowRunLimitPolicy,
    /// Per-node plugin input defaults that remain execution input rather than node policy.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugin_input_defaults: BTreeMap<String, serde_json::Value>,
    /// Bound initial workflow input defaults.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub input_defaults: serde_json::Value,
}

/// Side-effect-free portable compilation preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCompilationPreview {
    /// Preview contract version.
    pub version: u32,
    /// Source validation and compilation diagnostics.
    pub validation: WorkflowValidationReport,
    /// Successful exact compilation details, absent when diagnostics contain errors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compiled: Option<WorkflowCompiledAuthoringPreview>,
}

impl WorkflowCompilationPreview {
    /// Return whether this preview contains an admitted exact definition.
    #[must_use]
    pub const fn is_compiled(&self) -> bool {
        self.compiled.is_some() && self.validation.is_valid()
    }
}

/// Portable source contract for one runtime-authored workflow.
///
/// The embedded [`WorkflowDefinition`] is the single declarative graph model. Publication may apply
/// the declared bindings and then validates the resulting exact definition; authoring does not
/// introduce a parallel graph interpretation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowAuthoringDocument {
    /// Authoring source schema version.
    pub schema_version: u32,
    /// Stable logical workflow identity.
    pub workflow_id: String,
    /// User-facing non-authoritative metadata.
    pub metadata: WorkflowAuthoringMetadata,
    /// Runtime configuration schema.
    pub configuration_schema: ValueSchema,
    /// Optional bounded defaults validated against `configuration_schema`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration_defaults: Option<serde_json::Value>,
    /// Existing host-neutral declarative graph contract.
    pub definition: WorkflowDefinition,
    /// Bounded generic runtime configuration bindings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<WorkflowConfigurationBinding>,
    /// Exact declared requirements used for catalog resolution.
    #[serde(default)]
    pub requirements: WorkflowRequirementSummary,
    /// Portable run-admission limit policy.
    #[serde(default)]
    pub run_limits: WorkflowRunLimitPolicy,
    /// Diagnostic producer provenance that never changes trust or authorization.
    pub producer: WorkflowProducerProvenance,
    /// Optional non-semantic producer-owned presentation hints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<WorkflowAuthoringPresentation>,
}

/// Explicit renderer-neutral projection of authored fields that can affect executable behavior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowExecutableAuthoringSemantics {
    pub schema_version: u32,
    pub workflow_id: String,
    pub configuration_schema: ValueSchema,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration_defaults: Option<serde_json::Value>,
    pub definition: WorkflowDefinition,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<WorkflowConfigurationBinding>,
    #[serde(default)]
    pub requirements: WorkflowRequirementSummary,
    #[serde(default)]
    pub run_limits: WorkflowRunLimitPolicy,
}

impl WorkflowAuthoringDocument {
    /// Return a structured, source-addressed validation report without external side effects.
    #[must_use]
    pub fn validation_report(&self) -> WorkflowValidationReport {
        match self.validate() {
            Ok(()) => WorkflowValidationReport {
                authoring_version: self.schema_version,
                valid: true,
                source_digest_sha256: self.source_digest_sha256().ok(),
                executable_source_digest_sha256: self.executable_source_digest_sha256().ok(),
                diagnostics: Vec::new(),
            },
            Err(WorkflowError::Build { path, message }) => WorkflowValidationReport {
                authoring_version: self.schema_version,
                valid: false,
                source_digest_sha256: None,
                executable_source_digest_sha256: None,
                diagnostics: vec![WorkflowValidationDiagnostic {
                    code: authoring_diagnostic_code(&message).to_string(),
                    severity: WorkflowValidationSeverity::Error,
                    document_path: path,
                    remediation: authoring_remediation(&message).to_string(),
                    message,
                }],
            },
            Err(error) => WorkflowValidationReport {
                authoring_version: self.schema_version,
                valid: false,
                source_digest_sha256: None,
                executable_source_digest_sha256: None,
                diagnostics: vec![WorkflowValidationDiagnostic {
                    code: "invalid_authoring_document".to_string(),
                    severity: WorkflowValidationSeverity::Error,
                    document_path: "workflow".to_string(),
                    remediation: "Correct the source document and validate it again.".to_string(),
                    message: error.to_string(),
                }],
            },
        }
    }

    /// Compile and preview this authored workflow using only portable catalog and configuration data.
    ///
    /// This operation is deterministic and side-effect free. It performs no persistence, dispatch,
    /// model, tool, shell, Git, or network operation.
    #[must_use]
    pub fn compilation_preview(
        &self,
        catalog: &WorkflowAuthoringCatalogSnapshot,
        configuration: Option<&serde_json::Value>,
    ) -> WorkflowCompilationPreview {
        let mut validation = self.validation_report();
        if !validation.is_valid() {
            return WorkflowCompilationPreview {
                version: WORKFLOW_COMPILATION_PREVIEW_VERSION,
                validation,
                compiled: None,
            };
        }
        match self.compile_for_preview(catalog, configuration) {
            Ok(compiled) => WorkflowCompilationPreview {
                version: WORKFLOW_COMPILATION_PREVIEW_VERSION,
                validation,
                compiled: Some(compiled),
            },
            Err(error) => {
                validation.valid = false;
                validation.source_digest_sha256 = None;
                validation.executable_source_digest_sha256 = None;
                validation.diagnostics.push(validation_diagnostic(error));
                WorkflowCompilationPreview {
                    version: WORKFLOW_COMPILATION_PREVIEW_VERSION,
                    validation,
                    compiled: None,
                }
            }
        }
    }

    fn compile_for_preview(
        &self,
        catalog: &WorkflowAuthoringCatalogSnapshot,
        configuration: Option<&serde_json::Value>,
    ) -> Result<WorkflowCompiledAuthoringPreview, WorkflowError> {
        self.validate()?;
        catalog.validate()?;
        let configuration =
            merge_authoring_configuration(self.configuration_defaults.as_ref(), configuration)?;
        validate_value_against_schema("configuration", &configuration, &self.configuration_schema)?;
        let normalized = self.normalized()?;
        let mut definition = normalized.definition;
        let mut run_limits = normalized.run_limits;
        let mut plugin_input_defaults = BTreeMap::new();
        let mut input_defaults = serde_json::json!({});
        for binding in &normalized.bindings {
            let source =
                authoring_value_at_path(&configuration, &binding.configuration_path)?.clone();
            let value = if let Some(transform) = &binding.transform {
                transform.evaluate(&[WorkflowTransformInput {
                    name: WORKFLOW_TRANSFORM_SOURCE_CURRENT,
                    value: &source,
                }])?
            } else {
                source
            };
            apply_authoring_binding(
                &mut definition,
                &mut run_limits,
                &mut plugin_input_defaults,
                &mut input_defaults,
                binding,
                value,
            )?;
        }
        let definition = normalize_authored_definition(definition)?;
        if input_defaults == serde_json::json!({}) {
            input_defaults = serde_json::Value::Null;
        } else {
            validate_value_against_schema("input_defaults", &input_defaults, &definition.input)?;
        }
        run_limits.validate()?;
        let (requirements, effects, permissions) = resolve_authoring_catalog(
            &definition,
            &normalized.requirements,
            &plugin_input_defaults,
            catalog,
        )?;
        let production_capabilities = catalog.production_capabilities()?;
        let production_admission = definition.production_admission(&production_capabilities)?;
        if !production_admission.is_supported() {
            let diagnostic = production_admission
                .diagnostics
                .first()
                .expect("unsupported admission must include a diagnostic");
            return Err(authoring_error(
                diagnostic.node_id.as_ref().map_or_else(
                    || "definition".to_string(),
                    |node| format!("definition.nodes.{node}"),
                ),
                format!("{}: {}", diagnostic.code, diagnostic.message),
            ));
        }
        let definition_identity =
            WorkflowDefinitionIdentity::for_definition(self.workflow_id.clone(), &definition)?;
        Ok(WorkflowCompiledAuthoringPreview {
            configuration,
            definition,
            definition_identity,
            production_admission: WorkflowAuthoringProductionAdmission::from(&production_admission),
            requirements,
            effects,
            permissions,
            run_limits,
            plugin_input_defaults,
            input_defaults,
        })
    }

    /// Validate this portable authoring source without persistence or external side effects.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, malformed identities, invalid graph structure,
    /// invalid or unbounded schemas, duplicate binding targets, unknown references, remote schema
    /// references, oversized content, invalid defaults, or unsupported future state.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if self.schema_version != WORKFLOW_AUTHORING_DOCUMENT_VERSION {
            return Err(authoring_error(
                "schema_version",
                format!(
                    "unsupported workflow authoring document version {}; expected {}",
                    self.schema_version, WORKFLOW_AUTHORING_DOCUMENT_VERSION
                ),
            ));
        }
        validate_authoring_id("workflow_id", &self.workflow_id)?;
        self.metadata.validate()?;
        self.producer.validate()?;
        self.run_limits.validate()?;
        self.requirements.validate()?;
        self.definition.validate()?;
        validate_runtime_value_schema("configuration_schema", &self.configuration_schema)?;
        validate_runtime_value_schema("definition.input", &self.definition.input)?;
        validate_runtime_value_schema("definition.output", &self.definition.output)?;
        for (node_id, node) in &self.definition.nodes {
            validate_runtime_value_schema(
                &format!("definition.nodes.{node_id}.input"),
                &node.input,
            )?;
            validate_runtime_value_schema(
                &format!("definition.nodes.{node_id}.output"),
                &node.output,
            )?;
            validate_authoring_json_value(
                &format!("definition.nodes.{node_id}.configuration"),
                &node.configuration,
            )?;
            validate_persistable_authoring_value(
                &format!("definition.nodes.{node_id}.configuration"),
                &node.configuration,
            )?;
        }
        for (edge_index, edge) in self.definition.edges.iter().enumerate() {
            if let Some(transform) = &edge.transform {
                validate_runtime_value_schema(
                    &format!("definition.edges.{edge_index}.transform.output"),
                    &transform.output,
                )?;
            }
        }
        if let Some(defaults) = &self.configuration_defaults {
            validate_authoring_json_value("configuration_defaults", defaults)?;
            validate_persistable_authoring_value("configuration_defaults", defaults)?;
            let validator =
                jsonschema::validator_for(&self.configuration_schema.schema).map_err(|error| {
                    authoring_error(
                        "configuration_schema",
                        format!("invalid configuration schema: {error}"),
                    )
                })?;
            if let Err(error) = validator.validate(defaults) {
                return Err(authoring_error(
                    "configuration_defaults",
                    format!("defaults do not match configuration schema: {error}"),
                ));
            }
        }
        if self.bindings.len() > MAX_WORKFLOW_AUTHORING_BINDINGS {
            return Err(authoring_error(
                "bindings",
                format!("bindings exceed {MAX_WORKFLOW_AUTHORING_BINDINGS} entries"),
            ));
        }
        let mut binding_targets = BTreeSet::new();
        for binding in &self.bindings {
            binding.validate(&self.definition)?;
            let target = serde_json::to_string(&binding.target).map_err(|error| {
                authoring_error(
                    "bindings.target",
                    format!("binding target cannot be serialized: {error}"),
                )
            })?;
            if !binding_targets.insert(target) {
                return Err(authoring_error(
                    "bindings.target",
                    "configuration binding targets must be unique",
                ));
            }
        }
        if let Some(presentation) = &self.presentation {
            presentation.validate()?;
        }
        let bytes = serde_json::to_vec(self).map_err(|error| {
            authoring_error(
                "workflow",
                format!("authoring document cannot be serialized: {error}"),
            )
        })?;
        if bytes.len() > MAX_WORKFLOW_AUTHORING_DOCUMENT_BYTES {
            return Err(authoring_error(
                "workflow",
                format!("authoring document exceeds {MAX_WORKFLOW_AUTHORING_DOCUMENT_BYTES} bytes"),
            ));
        }
        Ok(())
    }

    /// Return a normalized source document with every semantically unordered collection in stable
    /// order. Edge-index binding targets are remapped to the normalized edge order.
    ///
    /// # Errors
    ///
    /// Returns an error when the source is invalid or an edge target cannot be remapped uniquely.
    pub fn normalized(&self) -> Result<Self, WorkflowError> {
        self.validate()?;
        let mut normalized = self.clone();
        normalized.definition.entries.sort();
        normalized.definition.entries.dedup();
        normalized.definition.exits.sort();
        normalized.definition.exits.dedup();
        for node in normalized.definition.nodes.values_mut() {
            node.resources.sort();
            node.resources.dedup();
        }
        let mut indexed_edges = normalized
            .definition
            .edges
            .drain(..)
            .enumerate()
            .map(|(index, edge)| {
                canonical_json_value(&edge, "definition.edges")
                    .map(|sort_key| (index, sort_key, edge))
            })
            .collect::<Result<Vec<_>, _>>()?;
        indexed_edges.sort_by(|(_, left_key, _), (_, right_key, _)| left_key.cmp(right_key));
        let mut remap = BTreeMap::new();
        for (new_index, (old_index, _, _)) in indexed_edges.iter().enumerate() {
            remap.insert(*old_index, new_index);
        }
        normalized.definition.edges = indexed_edges.into_iter().map(|(_, _, edge)| edge).collect();
        for binding in &mut normalized.bindings {
            if let WorkflowConfigurationTarget::EdgeConfiguration { edge_index, .. } =
                &mut binding.target
            {
                *edge_index = *remap.get(edge_index).ok_or_else(|| {
                    authoring_error(
                        "bindings.target.edge_index",
                        "edge binding target could not be normalized",
                    )
                })?;
            }
        }
        let mut indexed_bindings = normalized
            .bindings
            .drain(..)
            .map(|binding| {
                canonical_json_value(&binding.target, "bindings.target")
                    .map(|sort_key| (sort_key, binding))
            })
            .collect::<Result<Vec<_>, _>>()?;
        indexed_bindings.sort_by(|(left_key, left), (right_key, right)| {
            left_key
                .cmp(right_key)
                .then_with(|| left.configuration_path.cmp(&right.configuration_path))
        });
        normalized.bindings = indexed_bindings
            .into_iter()
            .map(|(_, binding)| binding)
            .collect();
        normalized.validate()?;
        Ok(normalized)
    }

    /// Return the canonical digest of the complete validated source document.
    ///
    /// # Errors
    ///
    /// Returns an error when validation, normalization, or canonical serialization fails.
    pub fn source_digest_sha256(&self) -> Result<String, WorkflowError> {
        canonical_sha256(&self.normalized()?, "workflow")
    }

    /// Return a projection containing only executable authoring semantics.
    ///
    /// This deliberately omits user-facing metadata, producer provenance, and presentation hints
    /// so callers cannot accidentally use those fields to derive authorization, dispatch, or
    /// compiled identity.
    #[must_use]
    pub fn executable_semantics(&self) -> WorkflowExecutableAuthoringSemantics {
        WorkflowExecutableAuthoringSemantics {
            schema_version: self.schema_version,
            workflow_id: self.workflow_id.clone(),
            configuration_schema: self.configuration_schema.clone(),
            configuration_defaults: self.configuration_defaults.clone(),
            definition: self.definition.clone(),
            bindings: self.bindings.clone(),
            requirements: self.requirements.clone(),
            run_limits: self.run_limits.clone(),
        }
    }

    /// Return the canonical digest of executable source semantics.
    ///
    /// User-facing metadata, producer provenance, and presentation hints are deliberately excluded.
    /// Configuration schemas, defaults, graph semantics, bindings, requirements, and run limits are
    /// included. Publication later derives the exact compiled-definition identity after applying
    /// bindings.
    ///
    /// # Errors
    ///
    /// Returns an error when validation, normalization, or canonical serialization fails.
    pub fn executable_source_digest_sha256(&self) -> Result<String, WorkflowError> {
        let normalized = self.normalized()?;
        canonical_sha256(
            &normalized.executable_semantics(),
            "workflow.executable_source",
        )
    }

    /// Return the current exact base-definition identity before configuration bindings are applied.
    ///
    /// This is useful for source diagnostics only. Publication must derive its final executable
    /// identity from the fully bound compiled definition.
    ///
    /// # Errors
    ///
    /// Returns an error when the document or embedded base definition is invalid.
    pub fn base_definition_identity(&self) -> Result<WorkflowDefinitionIdentity, WorkflowError> {
        self.validate()?;
        WorkflowDefinitionIdentity::for_definition(self.workflow_id.clone(), &self.definition)
    }
}

fn validation_diagnostic(error: WorkflowError) -> WorkflowValidationDiagnostic {
    match error {
        WorkflowError::Build { path, message } => WorkflowValidationDiagnostic {
            code: authoring_diagnostic_code(&message).to_string(),
            severity: WorkflowValidationSeverity::Error,
            document_path: path,
            remediation: authoring_remediation(&message).to_string(),
            message,
        },
        error => WorkflowValidationDiagnostic {
            code: "authoring_compilation_failed".to_string(),
            severity: WorkflowValidationSeverity::Error,
            document_path: "workflow".to_string(),
            remediation: "Correct the source document and compile it again.".to_string(),
            message: error.to_string(),
        },
    }
}

fn merge_authoring_configuration(
    defaults: Option<&serde_json::Value>,
    supplied: Option<&serde_json::Value>,
) -> Result<serde_json::Value, WorkflowError> {
    fn merge(target: &mut serde_json::Value, supplied: &serde_json::Value) {
        match (target, supplied) {
            (serde_json::Value::Object(target), serde_json::Value::Object(supplied)) => {
                for (key, value) in supplied {
                    if let Some(existing) = target.get_mut(key) {
                        merge(existing, value);
                    } else {
                        target.insert(key.clone(), value.clone());
                    }
                }
            }
            (target, supplied) => *target = supplied.clone(),
        }
    }
    let mut configuration = defaults.cloned().unwrap_or_else(|| serde_json::json!({}));
    if let Some(supplied) = supplied {
        validate_authoring_json_value("configuration", supplied)?;
        merge(&mut configuration, supplied);
    }
    validate_authoring_json_value("configuration", &configuration)?;
    Ok(configuration)
}

fn validate_value_against_schema(
    path: &str,
    value: &serde_json::Value,
    schema: &ValueSchema,
) -> Result<(), WorkflowError> {
    validate_runtime_value_schema(path, schema)?;
    let validator = jsonschema::validator_for(&schema.schema)
        .map_err(|error| authoring_error(path, format!("invalid schema: {error}")))?;
    validator
        .validate(value)
        .map_err(|error| authoring_error(path, format!("value does not match schema: {error}")))
}

fn authoring_value_at_path<'a>(
    value: &'a serde_json::Value,
    path: &str,
) -> Result<&'a serde_json::Value, WorkflowError> {
    path.split('.').try_fold(value, |current, component| {
        current.get(component).ok_or_else(|| {
            authoring_error(
                format!("configuration.{path}"),
                format!("configuration path '{path}' is missing"),
            )
        })
    })
}

fn set_authoring_json_path(
    root: &mut serde_json::Value,
    path: &str,
    value: serde_json::Value,
) -> Result<(), WorkflowError> {
    let components = path.split('.').collect::<Vec<_>>();
    let Some((last, parents)) = components.split_last() else {
        return Err(authoring_error(
            "bindings.target.path",
            "target path is empty",
        ));
    };
    let mut current = root;
    for component in parents {
        let object = current.as_object_mut().ok_or_else(|| {
            authoring_error(
                "bindings.target.path",
                format!("target path '{path}' traverses a non-object value"),
            )
        })?;
        current = object
            .entry((*component).to_string())
            .or_insert_with(|| serde_json::json!({}));
    }
    current
        .as_object_mut()
        .ok_or_else(|| {
            authoring_error(
                "bindings.target.path",
                format!("target path '{path}' has a non-object parent"),
            )
        })?
        .insert((*last).to_string(), value);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_authoring_binding(
    definition: &mut WorkflowDefinition,
    run_limits: &mut WorkflowRunLimitPolicy,
    plugin_input_defaults: &mut BTreeMap<String, serde_json::Value>,
    input_defaults: &mut serde_json::Value,
    binding: &WorkflowConfigurationBinding,
    value: serde_json::Value,
) -> Result<(), WorkflowError> {
    match &binding.target {
        WorkflowConfigurationTarget::NodeConfiguration { node_id, path } => {
            let node = definition.nodes.get_mut(node_id).ok_or_else(|| {
                authoring_error(
                    "bindings.target.node_id",
                    format!("binding references unknown node '{node_id}'"),
                )
            })?;
            set_authoring_json_path(&mut node.configuration, path, value)?;
        }
        WorkflowConfigurationTarget::AgentSelection { node_id, field } => {
            apply_authoring_agent_selection(definition, node_id, field, &value)?;
        }
        WorkflowConfigurationTarget::SkillSelection { node_id, index } => {
            apply_authoring_skill_selection(definition, node_id, *index, value)?;
        }
        WorkflowConfigurationTarget::PluginBlockInput { node_id, path } => {
            let defaults = plugin_input_defaults
                .entry(node_id.clone())
                .or_insert_with(|| serde_json::json!({}));
            set_authoring_json_path(defaults, path, value)?;
        }
        WorkflowConfigurationTarget::EdgeConfiguration { edge_index, path } => {
            let edge = definition.edges.get_mut(*edge_index).ok_or_else(|| {
                authoring_error(
                    "bindings.target.edge_index",
                    format!("binding references unknown edge index {edge_index}"),
                )
            })?;
            let mut encoded = serde_json::to_value(&*edge).map_err(|error| {
                authoring_error(
                    "bindings.target.edge",
                    format!("edge cannot be serialized: {error}"),
                )
            })?;
            set_authoring_json_path(&mut encoded, path, value)?;
            *edge = serde_json::from_value(encoded).map_err(|error| {
                authoring_error(
                    "bindings.target.edge",
                    format!("bound edge is invalid: {error}"),
                )
            })?;
        }
        WorkflowConfigurationTarget::RunLimit { field } => {
            let value = value.as_u64().ok_or_else(|| {
                authoring_error(
                    "bindings.target.run_limit",
                    format!("run-limit field '{field}' requires an unsigned integer"),
                )
            })?;
            match field.as_str() {
                "maximum_duration_ms" => run_limits.maximum_duration_ms = Some(value),
                "node_execution_cap" => run_limits.node_execution_cap = bounded_u32(field, value)?,
                "concurrency_cap" => run_limits.concurrency_cap = bounded_u32(field, value)?,
                "cycle_cap" => run_limits.cycle_cap = bounded_u32(field, value)?,
                "retry_cap" => run_limits.retry_cap = bounded_u32(field, value)?,
                _ => {
                    return Err(authoring_error(
                        "bindings.target.field",
                        format!("unknown run-limit field '{field}'"),
                    ));
                }
            }
        }
        WorkflowConfigurationTarget::InputDefault { path } => {
            set_authoring_json_path(input_defaults, path, value)?;
        }
    }
    Ok(())
}

fn apply_authoring_agent_selection(
    definition: &mut WorkflowDefinition,
    node_id: &str,
    field: &str,
    value: &serde_json::Value,
) -> Result<(), WorkflowError> {
    let node = definition.nodes.get_mut(node_id).ok_or_else(|| {
        authoring_error(
            "bindings.target.node_id",
            format!("binding references unknown node '{node_id}'"),
        )
    })?;
    let mut agent: WorkflowAgentConfiguration = serde_json::from_value(node.configuration.clone())
        .map_err(|error| {
            authoring_error(
                format!("definition.nodes.{node_id}.configuration"),
                format!("agent configuration is invalid: {error}"),
            )
        })?;
    match field {
        "agent_profile" => agent.agent_profile = authoring_non_empty_string(field, value)?,
        "provider" => agent.provider = authoring_optional_string(field, value)?,
        "model" => agent.model = authoring_optional_string(field, value)?,
        _ => {
            return Err(authoring_error(
                "bindings.target.field",
                format!("unsupported agent selection field '{field}'"),
            ));
        }
    }
    agent.validate()?;
    node.configuration = serde_json::to_value(agent).map_err(|error| {
        authoring_error(
            format!("definition.nodes.{node_id}.configuration"),
            format!("agent configuration cannot be serialized: {error}"),
        )
    })?;
    Ok(())
}

fn apply_authoring_skill_selection(
    definition: &mut WorkflowDefinition,
    node_id: &str,
    index: usize,
    value: serde_json::Value,
) -> Result<(), WorkflowError> {
    let node = definition.nodes.get_mut(node_id).ok_or_else(|| {
        authoring_error(
            "bindings.target.node_id",
            format!("binding references unknown node '{node_id}'"),
        )
    })?;
    let mut agent: WorkflowAgentConfiguration = serde_json::from_value(node.configuration.clone())
        .map_err(|error| {
            authoring_error(
                format!("definition.nodes.{node_id}.configuration"),
                format!("agent configuration is invalid: {error}"),
            )
        })?;
    let selection: AgentSkillSelection = serde_json::from_value(value).map_err(|error| {
        authoring_error(
            "bindings.target.skill",
            format!("skill selection is invalid: {error}"),
        )
    })?;
    if index > agent.skills.len() {
        return Err(authoring_error(
            "bindings.target.index",
            "skill selection index cannot leave gaps",
        ));
    }
    if index == agent.skills.len() {
        agent.skills.push(selection);
    } else {
        agent.skills[index] = selection;
    }
    agent.validate()?;
    node.configuration = serde_json::to_value(agent).map_err(|error| {
        authoring_error(
            format!("definition.nodes.{node_id}.configuration"),
            format!("agent configuration cannot be serialized: {error}"),
        )
    })?;
    Ok(())
}

fn authoring_non_empty_string(
    field: &str,
    value: &serde_json::Value,
) -> Result<String, WorkflowError> {
    let value = value.as_str().ok_or_else(|| {
        authoring_error(
            "bindings.target",
            format!("binding field '{field}' requires a string"),
        )
    })?;
    if value.trim().is_empty() {
        return Err(authoring_error(
            "bindings.target",
            format!("binding field '{field}' requires a non-empty string"),
        ));
    }
    Ok(value.to_string())
}

fn authoring_optional_string(
    field: &str,
    value: &serde_json::Value,
) -> Result<Option<String>, WorkflowError> {
    if value.is_null() {
        Ok(None)
    } else {
        authoring_non_empty_string(field, value).map(Some)
    }
}

fn bounded_u32(field: &str, value: u64) -> Result<u32, WorkflowError> {
    u32::try_from(value).map_err(|_| {
        authoring_error(
            "bindings.target.run_limit",
            format!("run-limit field '{field}' exceeds u32"),
        )
    })
}

fn normalize_authored_definition(
    mut definition: WorkflowDefinition,
) -> Result<WorkflowDefinition, WorkflowError> {
    definition.entries.sort();
    definition.entries.dedup();
    definition.exits.sort();
    definition.exits.dedup();
    for node in definition.nodes.values_mut() {
        node.resources = normalize_resource_claims(node.resources.clone())?;
    }
    let mut keyed_edges = definition
        .edges
        .drain(..)
        .map(|edge| canonical_json_value(&edge, "definition.edges").map(|key| (key, edge)))
        .collect::<Result<Vec<_>, _>>()?;
    keyed_edges.sort_by(|(left, _), (right, _)| left.cmp(right));
    keyed_edges.dedup_by(|(left, _), (right, _)| left == right);
    definition.edges = keyed_edges.into_iter().map(|(_, edge)| edge).collect();
    definition.validate()?;
    Ok(definition)
}

fn supported_authoring_capabilities(
    catalog: &WorkflowAuthoringCatalogSnapshot,
) -> BTreeSet<String> {
    BTreeSet::from([
        format!(
            "workflow-production/v{}",
            catalog.capabilities.capability_version
        ),
        format!(
            "workflow-block/v{}",
            catalog.capabilities.workflow_block_interface_version
        ),
    ])
}

/// Evaluate immutable declared requirements against one current portable catalog snapshot.
///
/// This operation is side-effect free and returns only bounded normalized requirement identities.
/// It does not mutate the authored document, published revision, catalog, or persistence.
///
/// # Errors
///
/// Returns an error when the requirements or catalog are malformed.
pub fn workflow_requirement_availability(
    declared: &WorkflowRequirementSummary,
    catalog: &WorkflowAuthoringCatalogSnapshot,
) -> Result<WorkflowRequirementAvailabilityReport, WorkflowError> {
    declared.validate()?;
    catalog.validate()?;
    let supported_capabilities = supported_authoring_capabilities(catalog);
    let mut unavailable = Vec::new();
    let mut append_missing = |kind: WorkflowRequirementKind,
                              required: &BTreeSet<String>,
                              available: &BTreeSet<String>| {
        unavailable.extend(
            required
                .difference(available)
                .cloned()
                .map(|identity| WorkflowUnavailableRequirement { kind, identity }),
        );
    };
    append_missing(
        WorkflowRequirementKind::Capability,
        &declared.capabilities,
        &supported_capabilities,
    );
    append_missing(
        WorkflowRequirementKind::Plugin,
        &declared.plugins,
        &catalog.plugins,
    );
    let available_blocks: BTreeSet<String> = catalog.blocks.keys().cloned().collect();
    append_missing(
        WorkflowRequirementKind::Block,
        &declared.blocks,
        &available_blocks,
    );
    append_missing(
        WorkflowRequirementKind::Agent,
        &declared.agents,
        &catalog.agent_profiles,
    );
    append_missing(
        WorkflowRequirementKind::Skill,
        &declared.skills,
        &catalog.skills,
    );
    Ok(WorkflowRequirementAvailabilityReport {
        version: WORKFLOW_REQUIREMENT_AVAILABILITY_VERSION,
        available: unavailable.is_empty(),
        unavailable,
    })
}

fn validate_declared_authoring_requirements(
    declared: &WorkflowRequirementSummary,
    catalog: &WorkflowAuthoringCatalogSnapshot,
) -> Result<(), WorkflowError> {
    let report = workflow_requirement_availability(declared, catalog)?;
    if let Some(unavailable) = report.unavailable.first() {
        let path = match unavailable.kind {
            WorkflowRequirementKind::Capability => "requirements.capabilities",
            WorkflowRequirementKind::Plugin => "requirements.plugins",
            WorkflowRequirementKind::Block => "requirements.blocks",
            WorkflowRequirementKind::Agent => "requirements.agents",
            WorkflowRequirementKind::Skill => "requirements.skills",
        };
        let label = match unavailable.kind {
            WorkflowRequirementKind::Capability => "capability",
            WorkflowRequirementKind::Plugin => "plugin",
            WorkflowRequirementKind::Block => "block",
            WorkflowRequirementKind::Agent => "agent profile",
            WorkflowRequirementKind::Skill => "skill",
        };
        return Err(authoring_error(
            path,
            format!("required {label} '{}' is unavailable", unavailable.identity),
        ));
    }
    Ok(())
}

fn resolve_authoring_catalog(
    definition: &WorkflowDefinition,
    declared: &WorkflowRequirementSummary,
    plugin_input_defaults: &BTreeMap<String, serde_json::Value>,
    catalog: &WorkflowAuthoringCatalogSnapshot,
) -> Result<
    (
        WorkflowRequirementSummary,
        WorkflowEffectSummary,
        WorkflowPermissionPreview,
    ),
    WorkflowError,
> {
    resolve_authoring_catalog_inner(
        definition,
        declared,
        plugin_input_defaults,
        catalog,
        &mut BTreeSet::new(),
        1,
    )
}

fn resolve_authoring_catalog_inner(
    definition: &WorkflowDefinition,
    declared: &WorkflowRequirementSummary,
    plugin_input_defaults: &BTreeMap<String, serde_json::Value>,
    catalog: &WorkflowAuthoringCatalogSnapshot,
    visited: &mut BTreeSet<String>,
    depth: u32,
) -> Result<
    (
        WorkflowRequirementSummary,
        WorkflowEffectSummary,
        WorkflowPermissionPreview,
    ),
    WorkflowError,
> {
    validate_declared_authoring_requirements(declared, catalog)?;

    let mut requirements = declared.clone();
    let mut effects = WorkflowEffectSummary::default();
    let mut permissions = WorkflowPermissionPreview::default();
    for (node_id, node) in &definition.nodes {
        effects.resources.extend(node.resources.clone());
        match node.kind {
            NodeKind::Agent => {
                let agent: WorkflowAgentConfiguration =
                    serde_json::from_value(node.configuration.clone()).map_err(|error| {
                        authoring_error(
                            format!("definition.nodes.{node_id}.configuration"),
                            format!("agent configuration is invalid: {error}"),
                        )
                    })?;
                agent.validate()?;
                if !catalog.agent_profiles.contains(&agent.agent_profile) {
                    return Err(authoring_error(
                        format!("definition.nodes.{node_id}.configuration.agent_profile"),
                        format!("agent profile '{}' is unavailable", agent.agent_profile),
                    ));
                }
                requirements.agents.insert(agent.agent_profile.clone());
                for skill in &agent.skills {
                    if skill.mode != AgentSkillActivationMode::Disabled
                        && !catalog.skills.contains(&skill.skill_id)
                    {
                        return Err(authoring_error(
                            format!("definition.nodes.{node_id}.configuration.skills"),
                            format!("skill '{}' is unavailable", skill.skill_id),
                        ));
                    }
                    if skill.mode != AgentSkillActivationMode::Disabled {
                        requirements.skills.insert(skill.skill_id.clone());
                    }
                }
                effects.maximum_capability = effects.maximum_capability.max(agent.tool_capability);
                permissions.maximum_capability =
                    permissions.maximum_capability.max(agent.tool_capability);
            }
            NodeKind::PluginBlock => resolve_authoring_plugin_block(
                node_id,
                node,
                plugin_input_defaults.get(node_id),
                catalog,
                &mut requirements,
                &mut effects,
                &mut permissions,
            )?,
            NodeKind::WorkflowCall => resolve_authoring_workflow_call(
                node_id,
                node,
                catalog,
                &mut requirements,
                &mut effects,
                &mut permissions,
                visited,
                depth,
            )?,
            NodeKind::Task
            | NodeKind::Branch
            | NodeKind::Repeat
            | NodeKind::Retry
            | NodeKind::Parallel
            | NodeKind::FanOut
            | NodeKind::Input
            | NodeKind::Approval => {}
        }
    }
    for node_id in plugin_input_defaults.keys() {
        if definition
            .node(node_id)
            .is_none_or(|node| node.kind != NodeKind::PluginBlock)
        {
            return Err(authoring_error(
                format!("plugin_input_defaults.{node_id}"),
                "plugin input defaults reference a non-plugin-block node",
            ));
        }
    }
    let effects = effects.normalized();
    effects.validate()?;
    permissions.explicit_grant_nodes.sort();
    permissions.explicit_grant_nodes.dedup();
    permissions.mutation_approval_nodes.sort();
    permissions.mutation_approval_nodes.dedup();
    requirements.validate()?;
    Ok((requirements, effects, permissions))
}

fn merge_child_preview(
    prefix: &str,
    child_requirements: WorkflowRequirementSummary,
    child_effects: WorkflowEffectSummary,
    child_permissions: WorkflowPermissionPreview,
    requirements: &mut WorkflowRequirementSummary,
    effects: &mut WorkflowEffectSummary,
    permissions: &mut WorkflowPermissionPreview,
) {
    requirements
        .capabilities
        .extend(child_requirements.capabilities);
    requirements.plugins.extend(child_requirements.plugins);
    requirements.blocks.extend(child_requirements.blocks);
    requirements.agents.extend(child_requirements.agents);
    requirements.skills.extend(child_requirements.skills);
    effects.maximum_capability = effects
        .maximum_capability
        .max(child_effects.maximum_capability);
    effects.block_effects.extend(child_effects.block_effects);
    effects.reconciliation.extend(child_effects.reconciliation);
    effects.resources.extend(child_effects.resources);
    permissions.maximum_capability = permissions
        .maximum_capability
        .max(child_permissions.maximum_capability);
    permissions.explicit_grant_nodes.extend(
        child_permissions
            .explicit_grant_nodes
            .into_iter()
            .map(|node| format!("{prefix}/{node}")),
    );
    permissions.mutation_approval_nodes.extend(
        child_permissions
            .mutation_approval_nodes
            .into_iter()
            .map(|node| format!("{prefix}/{node}")),
    );
}

#[allow(clippy::too_many_arguments)]
fn resolve_authoring_workflow_call(
    node_id: &str,
    node: &NodeDefinition,
    catalog: &WorkflowAuthoringCatalogSnapshot,
    requirements: &mut WorkflowRequirementSummary,
    effects: &mut WorkflowEffectSummary,
    permissions: &mut WorkflowPermissionPreview,
    visited: &mut BTreeSet<String>,
    depth: u32,
) -> Result<(), WorkflowError> {
    if depth >= MAX_WORKFLOW_CALL_DEPTH {
        return Err(authoring_error(
            format!("definition.nodes.{node_id}.configuration"),
            "workflow call dependency depth exceeds the supported bound",
        ));
    }
    let call: WorkflowCallConfiguration = serde_json::from_value(node.configuration.clone())
        .map_err(|error| {
            authoring_error(
                format!("definition.nodes.{node_id}.configuration"),
                format!("workflow call configuration is invalid: {error}"),
            )
        })?;
    call.validate()?;
    let identity = call.target.definition_identity();
    if !visited.insert(identity.definition_id.clone()) {
        return Err(authoring_error(
            format!("definition.nodes.{node_id}.configuration"),
            "workflow call dependency graph is recursive",
        ));
    }
    let child = catalog
        .workflow_definitions
        .get(&identity.definition_id)
        .ok_or_else(|| {
            authoring_error(
                format!("definition.nodes.{node_id}.configuration.target"),
                format!(
                    "exact child definition '{}' is unavailable",
                    identity.definition_id
                ),
            )
        })?;
    let actual = WorkflowDefinitionIdentity::for_definition(identity.kind.clone(), child)?;
    if &actual != identity {
        return Err(authoring_error(
            format!("definition.nodes.{node_id}.configuration.target"),
            "exact child definition identity does not match catalog content",
        ));
    }
    let empty_defaults = BTreeMap::new();
    let (child_requirements, child_effects, child_permissions) = resolve_authoring_catalog_inner(
        child,
        &WorkflowRequirementSummary::default(),
        &empty_defaults,
        catalog,
        visited,
        depth + 1,
    )?;
    merge_child_preview(
        node_id,
        child_requirements,
        child_effects,
        child_permissions,
        requirements,
        effects,
        permissions,
    );
    visited.remove(&identity.definition_id);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn resolve_authoring_plugin_block(
    node_id: &str,
    node: &NodeDefinition,
    input_defaults: Option<&serde_json::Value>,
    catalog: &WorkflowAuthoringCatalogSnapshot,
    requirements: &mut WorkflowRequirementSummary,
    effects: &mut WorkflowEffectSummary,
    permissions: &mut WorkflowPermissionPreview,
) -> Result<(), WorkflowError> {
    let block: WorkflowBlockDefinition = serde_json::from_value(node.configuration.clone())
        .map_err(|error| {
            authoring_error(
                format!("definition.nodes.{node_id}.configuration"),
                format!("plugin block configuration is invalid: {error}"),
            )
        })?;
    block.validate()?;
    let key = workflow_block_catalog_key(&block);
    if catalog.blocks.get(&key) != Some(&block) {
        return Err(authoring_error(
            format!("definition.nodes.{node_id}.configuration"),
            format!("exact plugin block '{key}' is unavailable"),
        ));
    }
    if let Some(defaults) = input_defaults {
        prepare_workflow_node_dataflow(node.dataflow, &node.input, &block.input, defaults)?;
    }
    finish_authoring_plugin_block_resolution(
        node_id,
        &block,
        key,
        requirements,
        effects,
        permissions,
    );
    Ok(())
}

fn finish_authoring_plugin_block_resolution(
    node_id: &str,
    block: &WorkflowBlockDefinition,
    key: String,
    requirements: &mut WorkflowRequirementSummary,
    effects: &mut WorkflowEffectSummary,
    permissions: &mut WorkflowPermissionPreview,
) {
    requirements.plugins.insert(block.plugin_id.clone());
    requirements.blocks.insert(key);
    effects.block_effects.insert(block.effect);
    effects.reconciliation.insert(block.reconciliation);
    effects.resources.extend(block.resources.clone());
    effects.maximum_capability = effects
        .maximum_capability
        .max(block.authorization.capability);
    permissions.maximum_capability = permissions
        .maximum_capability
        .max(block.authorization.capability);
    if block.authorization.explicit_grant_required {
        permissions.explicit_grant_nodes.push(node_id.to_string());
    }
    if block.effect == WorkflowBlockEffect::Mutating {
        permissions
            .mutation_approval_nodes
            .push(node_id.to_string());
    }
}

fn authoring_diagnostic_code(message: &str) -> &'static str {
    if message.contains("unsupported") && message.contains("version") {
        "unsupported_version"
    } else if message.contains("schema") || message.contains("Schema") {
        "invalid_schema"
    } else if message.contains("unknown") || message.contains("missing") {
        "unknown_reference"
    } else if message.contains("cycle") || message.contains("iteration") {
        "invalid_control_flow"
    } else if message.contains("exceed") || message.contains("too large") {
        "authoring_bound_exceeded"
    } else if message.contains("identity") || message.contains("name") {
        "invalid_identity"
    } else {
        "invalid_authoring_document"
    }
}

fn authoring_remediation(message: &str) -> &'static str {
    if message.contains("unsupported") && message.contains("version") {
        "Use a schema and contract version supported by the current workflow catalog."
    } else if message.contains("schema") || message.contains("Schema") {
        "Correct the declared schema or value so every workflow boundary is type compatible."
    } else if message.contains("unknown") || message.contains("missing") {
        "Reference an existing node, edge, schema target, or catalog contract."
    } else if message.contains("cycle") || message.contains("iteration") {
        "Use an explicit bounded back edge for cyclic control flow."
    } else if message.contains("exceed") || message.contains("too large") {
        "Reduce the document to the documented authoring bounds."
    } else {
        "Correct the source document at the reported path and validate it again."
    }
}

fn authoring_error(path: impl Into<String>, message: impl Into<String>) -> WorkflowError {
    WorkflowError::Build {
        path: path.into(),
        message: message.into(),
    }
}

fn validate_authoring_id(path: &str, value: &str) -> Result<(), WorkflowError> {
    if value.trim().is_empty()
        || value.len() > MAX_WORKFLOW_AUTHORING_ID_BYTES
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':' | b'/' | b'@')
        })
    {
        return Err(authoring_error(
            path,
            format!(
                "identity must contain 1..={MAX_WORKFLOW_AUTHORING_ID_BYTES} ASCII identity bytes"
            ),
        ));
    }
    Ok(())
}

fn validate_authoring_path(path: &str, value: &str) -> Result<(), WorkflowError> {
    if value.trim().is_empty()
        || value.len() > MAX_WORKFLOW_AUTHORING_ID_BYTES
        || value.split('.').any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
    {
        return Err(authoring_error(
            path,
            "path must be a bounded dotted sequence of identity components",
        ));
    }
    Ok(())
}

fn validate_authoring_node_target(
    definition: &WorkflowDefinition,
    node_id: &str,
    path: &str,
) -> Result<(), WorkflowError> {
    validate_authoring_id("bindings.target.node_id", node_id)?;
    validate_authoring_path("bindings.target.path", path)?;
    if definition.node(node_id).is_none() {
        return Err(authoring_error(
            "bindings.target.node_id",
            format!("binding references unknown node '{node_id}'"),
        ));
    }
    Ok(())
}

#[derive(Default)]
struct RuntimeSchemaCounts {
    properties: usize,
    enum_values: usize,
    references: usize,
}

fn validate_runtime_value_schema(path: &str, schema: &ValueSchema) -> Result<(), WorkflowError> {
    if schema.type_name.trim().is_empty()
        || schema.type_name.len() > MAX_WORKFLOW_AUTHORING_ID_BYTES
    {
        return Err(authoring_error(
            format!("{path}.type_name"),
            format!("schema type name must contain 1..={MAX_WORKFLOW_AUTHORING_ID_BYTES} bytes"),
        ));
    }
    let bytes = serde_json::to_vec(&schema.schema)
        .map_err(|error| authoring_error(path, format!("schema cannot be serialized: {error}")))?;
    if bytes.len() > MAX_WORKFLOW_AUTHORING_SCHEMA_BYTES {
        return Err(authoring_error(
            path,
            format!("schema exceeds {MAX_WORKFLOW_AUTHORING_SCHEMA_BYTES} bytes"),
        ));
    }
    if let Some(dialect) = schema.schema.get("$schema")
        && dialect.as_str() != Some(WORKFLOW_AUTHORING_JSON_SCHEMA_DIALECT)
    {
        return Err(authoring_error(
            format!("{path}.$schema"),
            format!(
                "unsupported JSON Schema dialect; expected {WORKFLOW_AUTHORING_JSON_SCHEMA_DIALECT}"
            ),
        ));
    }
    let mut counts = RuntimeSchemaCounts::default();
    validate_runtime_schema_value(path, &schema.schema, 0, &mut counts)?;
    validate_local_schema_references(path, &schema.schema)?;
    jsonschema::validator_for(&schema.schema)
        .map_err(|error| authoring_error(path, format!("invalid JSON Schema: {error}")))?;
    Ok(())
}

fn validate_runtime_schema_value(
    path: &str,
    value: &serde_json::Value,
    depth: usize,
    counts: &mut RuntimeSchemaCounts,
) -> Result<(), WorkflowError> {
    if depth > MAX_WORKFLOW_AUTHORING_JSON_DEPTH {
        return Err(authoring_error(
            path,
            format!("JSON depth exceeds {MAX_WORKFLOW_AUTHORING_JSON_DEPTH}"),
        ));
    }
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                validate_runtime_schema_value(path, item, depth + 1, counts)?;
            }
        }
        serde_json::Value::Object(fields) => {
            if let Some(properties) = fields
                .get("properties")
                .and_then(serde_json::Value::as_object)
            {
                counts.properties = counts.properties.saturating_add(properties.len());
            }
            if let Some(values) = fields.get("enum").and_then(serde_json::Value::as_array) {
                counts.enum_values = counts.enum_values.saturating_add(values.len());
            }
            if let Some(reference) = fields.get("$ref") {
                let reference = reference
                    .as_str()
                    .ok_or_else(|| authoring_error(path, "schema $ref values must be strings"))?;
                if !reference.starts_with("#/") {
                    return Err(authoring_error(
                        path,
                        "remote and non-local schema references are unsupported",
                    ));
                }
                counts.references = counts.references.saturating_add(1);
            }
            if counts.properties > MAX_WORKFLOW_AUTHORING_SCHEMA_PROPERTIES
                || counts.enum_values > MAX_WORKFLOW_AUTHORING_SCHEMA_ENUM_VALUES
                || counts.references > MAX_WORKFLOW_AUTHORING_SCHEMA_REFERENCES
            {
                return Err(authoring_error(
                    path,
                    format!(
                        "schema exceeds property, enum, or local-reference bounds ({MAX_WORKFLOW_AUTHORING_SCHEMA_PROPERTIES}/{MAX_WORKFLOW_AUTHORING_SCHEMA_ENUM_VALUES}/{MAX_WORKFLOW_AUTHORING_SCHEMA_REFERENCES})"
                    ),
                ));
            }
            for item in fields.values() {
                validate_runtime_schema_value(path, item, depth + 1, counts)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_local_schema_references(
    path: &str,
    root: &serde_json::Value,
) -> Result<(), WorkflowError> {
    fn decode_pointer_component(component: &str) -> String {
        component.replace("~1", "/").replace("~0", "~")
    }

    fn resolve<'a>(root: &'a serde_json::Value, reference: &str) -> Option<&'a serde_json::Value> {
        let pointer = reference.strip_prefix('#')?;
        if pointer.is_empty() {
            return Some(root);
        }
        root.pointer(pointer)
    }

    fn walk(
        path: &str,
        root: &serde_json::Value,
        value: &serde_json::Value,
        active: &mut BTreeSet<String>,
        expansions: &mut usize,
    ) -> Result<(), WorkflowError> {
        match value {
            serde_json::Value::Array(items) => {
                for item in items {
                    walk(path, root, item, active, expansions)?;
                }
            }
            serde_json::Value::Object(fields) => {
                if let Some(reference) = fields.get("$ref").and_then(serde_json::Value::as_str) {
                    *expansions = expansions.saturating_add(1);
                    if *expansions > MAX_WORKFLOW_AUTHORING_SCHEMA_REFERENCES {
                        return Err(authoring_error(
                            path,
                            format!(
                                "local schema reference expansion exceeds {MAX_WORKFLOW_AUTHORING_SCHEMA_REFERENCES}"
                            ),
                        ));
                    }
                    let target = resolve(root, reference).ok_or_else(|| {
                        authoring_error(
                            path,
                            format!("unresolved local schema reference '{reference}'"),
                        )
                    })?;
                    if !active.insert(reference.to_string()) {
                        return Err(authoring_error(
                            path,
                            format!(
                                "recursive local schema reference '{reference}' is unsupported"
                            ),
                        ));
                    }
                    walk(path, root, target, active, expansions)?;
                    active.remove(reference);
                }
                for (key, item) in fields {
                    if key != "$ref" {
                        walk(path, root, item, active, expansions)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    // Validate pointer escaping explicitly so malformed `~` sequences cannot be accepted by a
    // tolerant resolver and interpreted differently by another implementation.
    fn validate_pointer_syntax(path: &str, value: &serde_json::Value) -> Result<(), WorkflowError> {
        match value {
            serde_json::Value::Array(items) => {
                for item in items {
                    validate_pointer_syntax(path, item)?;
                }
            }
            serde_json::Value::Object(fields) => {
                if let Some(reference) = fields.get("$ref").and_then(serde_json::Value::as_str) {
                    for component in reference.strip_prefix("#/").unwrap_or_default().split('/') {
                        let decoded = decode_pointer_component(component);
                        let reencoded = decoded.replace('~', "~0").replace('/', "~1");
                        if reencoded != component {
                            return Err(authoring_error(
                                path,
                                format!("malformed local schema reference '{reference}'"),
                            ));
                        }
                    }
                }
                for item in fields.values() {
                    validate_pointer_syntax(path, item)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    validate_pointer_syntax(path, root)?;
    let mut expansions = 0;
    walk(path, root, root, &mut BTreeSet::new(), &mut expansions)
}

/// Validate that a value is safe for durable authored-workflow persistence.
///
/// Authored state may contain ordinary configuration, but not inline credentials or invocation-time
/// secret references. Secret resolution remains an execution-request concern until a separately
/// versioned persistence contract explicitly permits a reference form.
///
/// # Errors
///
/// Returns an error when the value contains an explicit secret reference or a sensitive field.
pub fn validate_persistable_authoring_value(
    path: &str,
    value: &serde_json::Value,
) -> Result<(), WorkflowError> {
    fn sensitive_field(field: &str) -> bool {
        matches!(
            field.to_ascii_lowercase().as_str(),
            "api_key"
                | "access_token"
                | "refresh_token"
                | "password"
                | "private_key"
                | "client_secret"
                | "credential"
                | "credentials"
                | "secret"
                | "secrets"
        )
    }

    fn walk(path: &str, value: &serde_json::Value) -> Result<(), WorkflowError> {
        match value {
            serde_json::Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    walk(&format!("{path}.{index}"), item)?;
                }
            }
            serde_json::Value::Object(fields) => {
                let explicit_secret_reference = fields
                    .get("backend")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|backend| matches!(backend, "env" | "sshenv"))
                    && (fields.contains_key("name")
                        || fields.contains_key("key")
                        || fields.contains_key("profile"));
                if explicit_secret_reference {
                    return Err(authoring_error(
                        path,
                        "invocation-time secret references cannot be persisted in authored state",
                    ));
                }
                for (field, item) in fields {
                    let item_path = format!("{path}.{field}");
                    if sensitive_field(field) && !item.is_null() {
                        return Err(authoring_error(
                            item_path,
                            "inline secret-bearing fields cannot be persisted in authored state",
                        ));
                    }
                    walk(&item_path, item)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    walk(path, value)
}

fn validate_authoring_json_value(
    path: &str,
    value: &serde_json::Value,
) -> Result<(), WorkflowError> {
    fn walk(path: &str, value: &serde_json::Value, depth: usize) -> Result<(), WorkflowError> {
        if depth > MAX_WORKFLOW_AUTHORING_JSON_DEPTH {
            return Err(authoring_error(
                path,
                format!("JSON depth exceeds {MAX_WORKFLOW_AUTHORING_JSON_DEPTH}"),
            ));
        }
        match value {
            serde_json::Value::Array(items) => {
                for item in items {
                    walk(path, item, depth + 1)?;
                }
            }
            serde_json::Value::Object(fields) => {
                for item in fields.values() {
                    walk(path, item, depth + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    walk(path, value, 0)
}

fn canonical_json_value<T: Serialize>(value: &T, path: &str) -> Result<String, WorkflowError> {
    fn normalize(value: serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Array(items) => {
                serde_json::Value::Array(items.into_iter().map(normalize).collect())
            }
            serde_json::Value::Object(fields) => {
                let sorted = fields
                    .into_iter()
                    .map(|(key, value)| (key, normalize(value)))
                    .collect::<BTreeMap<_, _>>();
                serde_json::Value::Object(sorted.into_iter().collect())
            }
            scalar => scalar,
        }
    }
    let value = serde_json::to_value(value).map_err(|error| {
        authoring_error(path, format!("value cannot be canonicalized: {error}"))
    })?;
    serde_json::to_string(&normalize(value)).map_err(|error| {
        authoring_error(
            path,
            format!("canonical value cannot be serialized: {error}"),
        )
    })
}

fn canonical_sha256<T: Serialize>(value: &T, path: &str) -> Result<String, WorkflowError> {
    let encoded = canonical_json_value(value, path)?;
    let digest = Sha256::digest(encoded.as_bytes());
    let mut result = String::with_capacity(64);
    for byte in digest {
        write!(&mut result, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(result)
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

/// Stable exact mutation-grant scope contract version.
pub const WORKFLOW_MUTATION_GRANT_SCOPE_VERSION: u32 = 1;

/// Immutable facts binding one durable workflow mutation approval to exactly one dispatch input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowMutationGrantScope {
    pub version: u32,
    pub definition_id: String,
    pub definition_version: u32,
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub workspace_snapshot: String,
    pub plugin_id: String,
    pub block_id: String,
    pub block_version: u32,
    pub operation: String,
    pub input_checksum_sha256: String,
    /// Bounded renderer-neutral summary of the immutable dispatch input.
    pub input_summary: serde_json::Value,
    /// Exact normalized resource claims required by the dispatch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resource_claims: Vec<ResourceClaim>,
    /// Owner-declared restart reconciliation behavior for the external operation.
    pub reconciliation: WorkflowBlockReconciliation,
    pub capability: WorkflowToolCapability,
}

impl WorkflowMutationGrantScope {
    /// Validate complete bounded exact-grant identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the contract version, identity, capability, or checksum is invalid.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        let identities = [
            self.definition_id.as_str(),
            self.run_id.as_str(),
            self.node_id.as_str(),
            self.activation_id.as_str(),
            self.workspace_snapshot.as_str(),
            self.plugin_id.as_str(),
            self.block_id.as_str(),
            self.operation.as_str(),
        ];
        if self.version != WORKFLOW_MUTATION_GRANT_SCOPE_VERSION
            || self.definition_version == 0
            || self.block_version == 0
            || self.capability != WorkflowToolCapability::Mutating
            || normalize_resource_claims(self.resource_claims.clone()).is_err()
            || serde_json::to_vec(&self.input_summary)
                .map_or(true, |summary| summary.len() > 16_384)
            || identities
                .iter()
                .any(|value| value.trim().is_empty() || value.len() > 4_096)
            || self.input_checksum_sha256.len() != 64
            || !self
                .input_checksum_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(WorkflowError::Build {
                path: "workflow.mutation_grant".to_string(),
                message: "mutation grant scope is invalid or unbounded".to_string(),
            });
        }
        Ok(())
    }
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
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ResourceAccess {
    Read,
    Write,
}

/// One named workflow resource claim.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
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

/// Stable durable agent-node configuration version.
pub const WORKFLOW_AGENT_CONFIGURATION_VERSION: u32 = 1;

/// Skill activation behavior for one durable agent node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSkillActivationMode {
    Required,
    Preferred,
    Disabled,
}

/// Exact skill selection embedded in durable definition identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSkillSelection {
    pub skill_id: String,
    pub mode: AgentSkillActivationMode,
}

/// Typed structured-output policy for a durable agent node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentStructuredOutputPolicy {
    pub schema: ValueSchema,
    pub strict: bool,
}

/// Versioned serializable durable agent-node configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowAgentConfiguration {
    pub version: u32,
    pub execution_target: AgentExecutionTarget,
    pub agent_profile: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub structured_output: AgentStructuredOutputPolicy,
    pub read_only: bool,
    pub tool_capability: WorkflowToolCapability,
    pub tool_allowlist: Vec<String>,
    pub timeout_ms: u64,
    pub skills: Vec<AgentSkillSelection>,
    pub prompt_mode: String,
    pub system_prompt: String,
}

impl WorkflowAgentConfiguration {
    /// Validate bounded identity, policy, and skill-isolation rules.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, empty identities, invalid timeout/prompt mode,
    /// duplicate skills/tools, or read-only policy escalation.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if self.version != WORKFLOW_AGENT_CONFIGURATION_VERSION {
            return Err(WorkflowError::Build {
                path: "agent.configuration.version".to_string(),
                message: format!(
                    "unsupported agent configuration version {}; expected {WORKFLOW_AGENT_CONFIGURATION_VERSION}",
                    self.version
                ),
            });
        }
        if self.agent_profile.trim().is_empty()
            || self.prompt_mode != "json_input"
            || self.timeout_ms == 0
            || self.timeout_ms > 3_600_000
            || self.system_prompt.len() > 262_144
        {
            return Err(WorkflowError::Build {
                path: "agent.configuration".to_string(),
                message: "agent profile, prompt mode, timeout, or system prompt is invalid"
                    .to_string(),
            });
        }
        if self.read_only && self.tool_capability != WorkflowToolCapability::ReadOnly {
            return Err(WorkflowError::Build {
                path: "agent.tool_capability".to_string(),
                message: "read-only agent must request the read-only tool capability".to_string(),
            });
        }
        if !self.read_only && self.tool_capability == WorkflowToolCapability::ReadOnly {
            return Err(WorkflowError::Build {
                path: "agent.tool_capability".to_string(),
                message: "read-write agent cannot claim a read-only tool capability".to_string(),
            });
        }
        if self
            .provider
            .as_ref()
            .is_some_and(|value| value.trim().is_empty())
            || self
                .model
                .as_ref()
                .is_some_and(|value| value.trim().is_empty())
            || self.agent_profile.len() > 256
            || self
                .provider
                .as_ref()
                .is_some_and(|value| value.len() > 256)
            || self.model.as_ref().is_some_and(|value| value.len() > 512)
            || self.tool_allowlist.len() > 256
            || self.skills.len() > 32
            || self
                .tool_allowlist
                .iter()
                .any(|tool| tool.trim().is_empty() || tool.len() > 256)
            || self.skills.iter().any(|skill| skill.skill_id.len() > 256)
        {
            return Err(WorkflowError::Build {
                path: "agent.configuration".to_string(),
                message: "agent identity, tool, or skill fields exceed durable bounds".to_string(),
            });
        }
        let tools = self.tool_allowlist.iter().collect::<BTreeSet<_>>();
        let skills = self
            .skills
            .iter()
            .map(|skill| skill.skill_id.as_str())
            .collect::<BTreeSet<_>>();
        if tools.len() != self.tool_allowlist.len()
            || skills.len() != self.skills.len()
            || self
                .skills
                .iter()
                .any(|skill| skill.skill_id.trim().is_empty())
        {
            return Err(WorkflowError::Build {
                path: "agent.configuration".to_string(),
                message: "agent tools and skill IDs must be non-empty and unique".to_string(),
            });
        }
        jsonschema::validator_for(&self.structured_output.schema.schema).map_err(|error| {
            WorkflowError::Build {
                path: "agent.structured_output".to_string(),
                message: format!("invalid structured output schema: {error}"),
            }
        })?;
        Ok(())
    }
}

/// Execution target for a daemon-hosted workflow agent node.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AgentExecutionTarget {
    /// Execute in a fresh isolated child session.
    #[default]
    FreshIsolated,
    /// Execute sequentially in the workflow run's parent session.
    SharedParentSequential,
}

/// Current exact child-workflow call contract version.
pub const WORKFLOW_CALL_VERSION: u32 = 1;
/// Maximum supported workflow-call nesting depth, including the root run.
pub const MAX_WORKFLOW_CALL_DEPTH: u32 = 4;
/// Maximum descendants admitted beneath one root run.
pub const MAX_WORKFLOW_CALL_DESCENDANTS: u32 = 64;

/// Exact optional preset selected for one authored child call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCallPreset {
    /// Stable preset identity beneath the authored workflow.
    pub preset_id: String,
    /// Exact immutable preset generation.
    pub generation: u64,
}

/// Exact immutable target for one synchronous child-workflow call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowCallTarget {
    /// One exact registered compiled definition.
    Definition {
        /// Product identity plus content-derived compiled definition identity.
        identity: WorkflowDefinitionIdentity,
    },
    /// One exact published authored revision and its expected compiled definition.
    AuthoredRevision {
        /// Stable logical authored workflow identity.
        workflow_id: String,
        /// Exact immutable published revision.
        revision: u64,
        /// Expected compiled identity resolved during validation/publication.
        definition_identity: WorkflowDefinitionIdentity,
        /// Optional exact preset generation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preset: Option<WorkflowCallPreset>,
    },
}

impl WorkflowCallTarget {
    /// Return the exact compiled definition identity required at child admission.
    #[must_use]
    pub const fn definition_identity(&self) -> &WorkflowDefinitionIdentity {
        match self {
            Self::Definition { identity } => identity,
            Self::AuthoredRevision {
                definition_identity,
                ..
            } => definition_identity,
        }
    }

    /// Validate bounded identity and exact target invariants.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, malformed identities, zero revisions or preset
    /// generations, or authored/compiled logical identity mismatch.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        let identity = self.definition_identity();
        validate_workflow_call_id("workflow_call.target.kind", &identity.kind)?;
        validate_workflow_call_id(
            "workflow_call.target.definition_id",
            &identity.definition_id,
        )?;
        if identity.definition_version == 0 {
            return Err(WorkflowError::Build {
                path: "workflow_call.target.definition_version".to_string(),
                message: "workflow call definition version must be greater than zero".to_string(),
            });
        }
        if let Self::AuthoredRevision {
            workflow_id,
            revision,
            preset,
            ..
        } = self
        {
            validate_workflow_call_id("workflow_call.target.workflow_id", workflow_id)?;
            if workflow_id != &identity.kind || *revision == 0 {
                return Err(WorkflowError::Build {
                    path: "workflow_call.target.authored_revision".to_string(),
                    message: "authored call target must use a nonzero revision and matching logical identity"
                        .to_string(),
                });
            }
            if let Some(preset) = preset {
                validate_workflow_call_id("workflow_call.target.preset_id", &preset.preset_id)?;
                if preset.generation == 0 {
                    return Err(WorkflowError::Build {
                        path: "workflow_call.target.preset_generation".to_string(),
                        message: "workflow call preset generation must be greater than zero"
                            .to_string(),
                    });
                }
            }
        }
        Ok(())
    }
}

/// Portable synchronous child-workflow call configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCallConfiguration {
    /// Call contract version.
    pub version: u32,
    /// Exact immutable target.
    pub target: WorkflowCallTarget,
}

impl WorkflowCallConfiguration {
    /// Validate the current exact child-call contract.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions or invalid exact targets.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if self.version != WORKFLOW_CALL_VERSION {
            return Err(WorkflowError::Build {
                path: "workflow_call.version".to_string(),
                message: format!(
                    "unsupported workflow call version {}; expected {WORKFLOW_CALL_VERSION}",
                    self.version
                ),
            });
        }
        self.target.validate()
    }
}

fn validate_workflow_call_id(path: &str, value: &str) -> Result<(), WorkflowError> {
    if value.trim().is_empty() || value.len() > 512 {
        return Err(WorkflowError::Build {
            path: path.to_string(),
            message: "workflow call identity must contain 1 to 512 bytes".to_string(),
        });
    }
    Ok(())
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
    /// Versioned adaptation between the complete node boundary and owner operation boundary.
    #[serde(default, skip_serializing_if = "WorkflowNodeDataflowPolicy::is_direct")]
    pub dataflow: WorkflowNodeDataflowPolicy,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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
    /// Typed operation owned by a manifest-declared plugin workflow block.
    PluginBlock,
    /// Durable external input gate resolved by the workflow host.
    Input,
    /// Durable human approval gate resolved by the workflow host.
    Approval,
    /// Synchronous exact immutable child-workflow call.
    WorkflowCall,
}

/// Every public serialized node kind, used to enforce exhaustive production capability coverage.
pub const ALL_NODE_KINDS: [NodeKind; 11] = [
    NodeKind::Task,
    NodeKind::Agent,
    NodeKind::Branch,
    NodeKind::Repeat,
    NodeKind::Retry,
    NodeKind::Parallel,
    NodeKind::FanOut,
    NodeKind::PluginBlock,
    NodeKind::Input,
    NodeKind::Approval,
    NodeKind::WorkflowCall,
];

/// Every public serialized edge kind, used to enforce exhaustive production capability coverage.
pub const ALL_WORKFLOW_EDGE_KINDS: [WorkflowEdgeKind; 4] = [
    WorkflowEdgeKind::Direct,
    WorkflowEdgeKind::Conditional,
    WorkflowEdgeKind::Back,
    WorkflowEdgeKind::Retry,
];

/// Return the stable serialized name for a node kind.
///
/// This exhaustive match intentionally makes enum growth fail compilation until production
/// capability coverage is reviewed.
#[must_use]
pub const fn node_kind_name(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Task => "task",
        NodeKind::Agent => "agent",
        NodeKind::Branch => "branch",
        NodeKind::Repeat => "repeat",
        NodeKind::Retry => "retry",
        NodeKind::Parallel => "parallel",
        NodeKind::FanOut => "fan_out",
        NodeKind::PluginBlock => "plugin_block",
        NodeKind::Input => "input",
        NodeKind::Approval => "approval",
        NodeKind::WorkflowCall => "workflow_call",
    }
}

/// Durable support level for one serialized workflow construct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowCapabilitySupport {
    /// The production host implements and owns this construct.
    Supported,
    /// The construct is available only to the lean in-process SDK.
    InProcessOnly,
    /// The serialized contract is reserved until production behavior is complete.
    Unsupported,
}

/// Exact versioned capability set accepted by the durable production host.
///
/// The lean in-process SDK intentionally supports additional closure-backed and ephemeral
/// constructs. Durable registration must validate against this contract before persistence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowProductionCapabilities {
    /// Capability contract version.
    pub capability_version: u32,
    /// Compiled workflow definition schema version.
    pub definition_schema_version: u32,
    /// Deterministic predicate contract version.
    pub predicate_version: u32,
    /// Durable declarative transform contract version, when implemented.
    pub transform_version: Option<u32>,
    /// Durable automatic retry policy version, when implemented.
    pub automatic_retry_policy_version: Option<u32>,
    /// Durable agent configuration contract version.
    pub agent_configuration_version: u32,
    /// Plugin workflow-block interface version.
    pub workflow_block_interface_version: u32,
    /// Durable support classification for every public node kind.
    pub node_kinds: BTreeMap<NodeKind, WorkflowCapabilitySupport>,
    /// Durable support classification for every serialized edge kind.
    pub edge_kinds: BTreeMap<WorkflowEdgeKind, WorkflowCapabilitySupport>,
    /// Durable two-branch join policies.
    pub parallel_join_policies: BTreeSet<ParallelFailurePolicy>,
    /// Durable support classification for automatic retry scheduling.
    pub automatic_retry: WorkflowCapabilitySupport,
    /// Durable support classification for homogeneous fan-out.
    pub fan_out: WorkflowCapabilitySupport,
    /// Durable support classification for declarative transforms.
    pub transforms: WorkflowCapabilitySupport,
    /// Durable support classification for opaque artifact references.
    pub artifact_references: WorkflowCapabilitySupport,
    /// Supported durable agent execution targets.
    pub agent_execution_targets: BTreeSet<AgentExecutionTarget>,
}

impl WorkflowProductionCapabilities {
    /// Return the exact capability set currently implemented by the production daemon.
    #[must_use]
    pub fn current() -> Self {
        let node_kinds = BTreeMap::from([
            (NodeKind::Task, WorkflowCapabilitySupport::InProcessOnly),
            (NodeKind::Agent, WorkflowCapabilitySupport::Supported),
            (NodeKind::Branch, WorkflowCapabilitySupport::Supported),
            (NodeKind::Repeat, WorkflowCapabilitySupport::Supported),
            (NodeKind::Retry, WorkflowCapabilitySupport::Unsupported),
            (NodeKind::Parallel, WorkflowCapabilitySupport::Supported),
            (NodeKind::FanOut, WorkflowCapabilitySupport::Unsupported),
            (NodeKind::PluginBlock, WorkflowCapabilitySupport::Supported),
            (NodeKind::Input, WorkflowCapabilitySupport::Supported),
            (NodeKind::Approval, WorkflowCapabilitySupport::Supported),
            (NodeKind::WorkflowCall, WorkflowCapabilitySupport::Supported),
        ]);
        let edge_kinds = BTreeMap::from([
            (
                WorkflowEdgeKind::Direct,
                WorkflowCapabilitySupport::Supported,
            ),
            (
                WorkflowEdgeKind::Conditional,
                WorkflowCapabilitySupport::Supported,
            ),
            (WorkflowEdgeKind::Back, WorkflowCapabilitySupport::Supported),
            (
                WorkflowEdgeKind::Retry,
                WorkflowCapabilitySupport::Unsupported,
            ),
        ]);
        Self {
            capability_version: WORKFLOW_PRODUCTION_CAPABILITY_VERSION,
            definition_schema_version: WORKFLOW_DEFINITION_SCHEMA_VERSION,
            predicate_version: WORKFLOW_PREDICATE_VERSION,
            transform_version: Some(WORKFLOW_TRANSFORM_VERSION),
            automatic_retry_policy_version: None,
            agent_configuration_version: 1,
            workflow_block_interface_version: WORKFLOW_BLOCK_INTERFACE_VERSION,
            node_kinds,
            edge_kinds,
            parallel_join_policies: BTreeSet::from([
                ParallelFailurePolicy::WaitAll,
                ParallelFailurePolicy::FailFast,
            ]),
            automatic_retry: WorkflowCapabilitySupport::Unsupported,
            fan_out: WorkflowCapabilitySupport::Unsupported,
            transforms: WorkflowCapabilitySupport::Supported,
            artifact_references: WorkflowCapabilitySupport::Supported,
            agent_execution_targets: BTreeSet::from([
                AgentExecutionTarget::FreshIsolated,
                AgentExecutionTarget::SharedParentSequential,
            ]),
        }
    }

    /// Return the durable support classification for a node kind.
    #[must_use]
    pub fn node_support(&self, kind: NodeKind) -> WorkflowCapabilitySupport {
        self.node_kinds
            .get(&kind)
            .copied()
            .unwrap_or(WorkflowCapabilitySupport::Unsupported)
    }
    /// Return the durable support classification for an edge kind.
    #[must_use]
    pub fn edge_support(&self, kind: WorkflowEdgeKind) -> WorkflowCapabilitySupport {
        self.edge_kinds
            .get(&kind)
            .copied()
            .unwrap_or(WorkflowCapabilitySupport::Unsupported)
    }
}

/// One stable production-admission diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowCapabilityDiagnostic {
    /// Machine-readable diagnostic code.
    pub code: String,
    /// Node associated with the diagnostic, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// Human-readable explanation.
    pub message: String,
}

/// Result of validating a definition against durable production capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowProductionAdmission {
    /// Capability contract used for validation.
    pub capabilities: WorkflowProductionCapabilities,
    /// Stable diagnostics in deterministic definition order.
    pub diagnostics: Vec<WorkflowCapabilityDiagnostic>,
}

impl WorkflowProductionAdmission {
    /// Return whether the definition is fully supported by the production host.
    #[must_use]
    pub const fn is_supported(&self) -> bool {
        self.diagnostics.is_empty()
    }
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
    /// Optional bounded declarative mapping evaluated before target activation insertion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<WorkflowTransform>,
}

/// Serializable workflow edge behavior category used by production capability admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowEdgeKind {
    /// Unconditional forward control flow.
    Direct,
    /// Predicate-selected forward control flow.
    Conditional,
    /// Bounded repeat back edge.
    Back,
    /// Automatic retry edge.
    Retry,
}

/// Return the stable serialized name for an edge kind.
///
/// This exhaustive match intentionally makes enum growth fail compilation until production
/// capability coverage is reviewed.
#[must_use]
pub const fn workflow_edge_kind_name(kind: WorkflowEdgeKind) -> &'static str {
    match kind {
        WorkflowEdgeKind::Direct => "direct",
        WorkflowEdgeKind::Conditional => "conditional",
        WorkflowEdgeKind::Back => "back",
        WorkflowEdgeKind::Retry => "retry",
    }
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

impl EdgeKind {
    /// Return the stable capability category for this edge.
    #[must_use]
    pub const fn capability_kind(&self) -> WorkflowEdgeKind {
        match self {
            Self::Direct => WorkflowEdgeKind::Direct,
            Self::Conditional { .. } => WorkflowEdgeKind::Conditional,
            Self::Back { .. } => WorkflowEdgeKind::Back,
            Self::Retry { .. } => WorkflowEdgeKind::Retry,
        }
    }
}

/// Numeric ordering operation used by deterministic predicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PredicateNumericComparison {
    /// The left number is less than the right number.
    LessThan,
    /// The left number is less than or equal to the right number.
    LessThanOrEqual,
    /// The left number is greater than the right number.
    GreaterThan,
    /// The left number is greater than or equal to the right number.
    GreaterThanOrEqual,
}

/// Serializable deterministic predicate over a structured workflow value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum PredicateExpression {
    /// Compare the value at a dotted field path with one constant for equality.
    Equals {
        /// Predicate contract version.
        version: u32,
        /// Dotted object field path. An empty path addresses the complete value.
        path: String,
        /// Expected JSON value.
        value: serde_json::Value,
    },
    /// Compare two values selected from the same structured input for equality.
    FieldsEqual {
        /// Predicate contract version.
        version: u32,
        /// Left dotted object field path.
        left_path: String,
        /// Right dotted object field path.
        right_path: String,
    },
    /// Require every bounded child predicate to match.
    All {
        /// Predicate contract version.
        version: u32,
        /// Non-empty child predicate list.
        predicates: Vec<Self>,
    },
    /// Require at least one bounded child predicate to match.
    Any {
        /// Predicate contract version.
        version: u32,
        /// Non-empty child predicate list.
        predicates: Vec<Self>,
    },
    /// Negate one bounded child predicate.
    Not {
        /// Predicate contract version.
        version: u32,
        /// Child predicate.
        predicate: Box<Self>,
    },
    /// Compare two finite JSON numbers selected from the same structured input.
    NumericCompare {
        /// Predicate contract version.
        version: u32,
        /// Left dotted object field path.
        left_path: String,
        /// Right dotted object field path.
        right_path: String,
        /// Ordering relation that must hold.
        comparison: PredicateNumericComparison,
    },
}

impl PredicateExpression {
    fn evaluate<T: Serialize>(&self, input: &T) -> Result<bool, WorkflowError> {
        let value = serde_json::to_value(input).map_err(|error| WorkflowError::Build {
            path: "predicate".to_string(),
            message: format!("failed to serialize predicate input: {error}"),
        })?;
        self.evaluate_value(&value)
    }

    /// Return this expression's declared contract version.
    #[must_use]
    pub const fn version(&self) -> u32 {
        match self {
            Self::Equals { version, .. }
            | Self::FieldsEqual { version, .. }
            | Self::All { version, .. }
            | Self::Any { version, .. }
            | Self::Not { version, .. }
            | Self::NumericCompare { version, .. } => *version,
        }
    }

    /// Evaluate this predicate against an already serialized workflow value.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsupported version, an invalid bounded expression, a missing field,
    /// incompatible JSON value categories, or non-finite/inexact numeric comparison.
    pub fn evaluate_value(&self, value: &serde_json::Value) -> Result<bool, WorkflowError> {
        validate_predicate_expression(self)?;
        self.evaluate_validated(value)
    }

    fn evaluate_validated(&self, value: &serde_json::Value) -> Result<bool, WorkflowError> {
        match self {
            Self::Equals {
                version: _,
                path,
                value: expected,
            } => {
                let actual = predicate_value_at_path(value, path)?;
                if !predicate_values_compatible(actual, expected) {
                    return Err(predicate_type_mismatch(path, actual, expected));
                }
                Ok(actual == expected)
            }
            Self::FieldsEqual {
                version: _,
                left_path,
                right_path,
            } => {
                let left = predicate_value_at_path(value, left_path)?;
                let right = predicate_value_at_path(value, right_path)?;
                if !predicate_values_compatible(left, right) {
                    return Err(predicate_type_mismatch(left_path, left, right));
                }
                Ok(left == right)
            }
            Self::All {
                version: _,
                predicates,
            } => {
                for predicate in predicates {
                    if !predicate.evaluate_validated(value)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Self::Any {
                version: _,
                predicates,
            } => {
                for predicate in predicates {
                    if predicate.evaluate_validated(value)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Self::Not {
                version: _,
                predicate,
            } => Ok(!predicate.evaluate_validated(value)?),
            Self::NumericCompare {
                version: _,
                left_path,
                right_path,
                comparison,
            } => {
                let left_value = predicate_value_at_path(value, left_path)?;
                let right_value = predicate_value_at_path(value, right_path)?;
                let serde_json::Value::Number(left) = left_value else {
                    return Err(WorkflowError::Build {
                        path: left_path.clone(),
                        message: format!(
                            "numeric predicate expected number, found {}",
                            predicate_value_kind(left_value)
                        ),
                    });
                };
                let serde_json::Value::Number(right) = right_value else {
                    return Err(WorkflowError::Build {
                        path: right_path.clone(),
                        message: format!(
                            "numeric predicate expected number, found {}",
                            predicate_value_kind(right_value)
                        ),
                    });
                };
                let ordering =
                    compare_json_numbers(left, right).ok_or_else(|| WorkflowError::Build {
                        path: format!("{left_path},{right_path}"),
                        message: "numeric predicate values cannot be compared exactly".to_string(),
                    })?;
                Ok(match comparison {
                    PredicateNumericComparison::LessThan => ordering.is_lt(),
                    PredicateNumericComparison::LessThanOrEqual => !ordering.is_gt(),
                    PredicateNumericComparison::GreaterThan => ordering.is_gt(),
                    PredicateNumericComparison::GreaterThanOrEqual => !ordering.is_lt(),
                })
            }
        }
    }
}

fn predicate_value_at_path<'a>(
    value: &'a serde_json::Value,
    path: &str,
) -> Result<&'a serde_json::Value, WorkflowError> {
    path.split('.')
        .filter(|part| !part.is_empty())
        .try_fold(value, |current, part| current.get(part))
        .ok_or_else(|| WorkflowError::Build {
            path: path.to_string(),
            message: "predicate field was not present in the structured value".to_string(),
        })
}

fn predicate_type_mismatch(
    path: &str,
    actual: &serde_json::Value,
    expected: &serde_json::Value,
) -> WorkflowError {
    WorkflowError::Build {
        path: path.to_string(),
        message: format!(
            "predicate value type {} is incompatible with expected type {}",
            predicate_value_kind(actual),
            predicate_value_kind(expected)
        ),
    }
}

fn compare_json_numbers(
    left: &serde_json::Number,
    right: &serde_json::Number,
) -> Option<std::cmp::Ordering> {
    match (left.as_i64(), right.as_i64()) {
        (Some(left), Some(right)) => return Some(left.cmp(&right)),
        (Some(left), None) if left >= 0 => {
            return right.as_u64().map(|right| left.cast_unsigned().cmp(&right));
        }
        (None, Some(right)) if right >= 0 => {
            return left.as_u64().map(|left| left.cmp(&right.cast_unsigned()));
        }
        _ => {}
    }
    match (left.as_u64(), right.as_u64()) {
        (Some(left), Some(right)) => return Some(left.cmp(&right)),
        (Some(_), None) if right.as_i64().is_some_and(|right| right < 0) => {
            return Some(std::cmp::Ordering::Greater);
        }
        (None, Some(_)) if left.as_i64().is_some_and(|left| left < 0) => {
            return Some(std::cmp::Ordering::Less);
        }
        _ => {}
    }
    left.as_f64()?.partial_cmp(&right.as_f64()?)
}

fn predicate_values_compatible(actual: &serde_json::Value, expected: &serde_json::Value) -> bool {
    std::mem::discriminant(actual) == std::mem::discriminant(expected)
}

const fn predicate_value_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
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
                version: WORKFLOW_PREDICATE_VERSION,
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

/// Conflict behavior for deterministic object merge transforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransformMergeConflict {
    /// Reject duplicate object fields.
    Reject,
    /// Keep the value from the first object containing the field.
    KeepFirst,
    /// Keep the value from the last object containing the field.
    KeepLast,
}

/// One named durable transform input.
#[derive(Debug, Clone, Copy)]
pub struct WorkflowTransformInput<'a> {
    /// Stable source name referenced by [`WorkflowTransformExpression::Input`].
    ///
    /// Durable hosts expose [`WORKFLOW_TRANSFORM_SOURCE_CURRENT`],
    /// [`WORKFLOW_TRANSFORM_SOURCE_STATE`], and, for a completed parallel join,
    /// [`WORKFLOW_TRANSFORM_SOURCE_JOIN_LEFT`] and [`WORKFLOW_TRANSFORM_SOURCE_JOIN_RIGHT`].
    pub name: &'a str,
    /// Source value.
    pub value: &'a serde_json::Value,
}

/// Finite declarative workflow transform expression.
///
/// The contract contains no script evaluation, recursion, filesystem access, or network access.
/// References are resolved only from the explicitly supplied named inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum WorkflowTransformExpression {
    /// Select a complete named input or one dotted object path below it.
    Input {
        /// Stable input source name.
        source: String,
        /// Dotted object path; empty selects the complete source value.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        path: String,
    },
    /// Embed one bounded JSON constant.
    Constant { value: serde_json::Value },
    /// Construct an object from explicit deterministic field expressions.
    Object { fields: BTreeMap<String, Self> },
    /// Construct an array in declaration order.
    Array { items: Vec<Self> },
    /// Merge object expressions using an explicit conflict policy.
    Merge {
        objects: Vec<Self>,
        conflict: TransformMergeConflict,
    },
    /// Add a signed integer delta to one integer expression.
    Increment { value: Box<Self>, by: i64 },
    /// Select an optional value and evaluate a deterministic default when absent or null.
    Default {
        value: Box<Self>,
        default: Box<Self>,
    },
}

/// One versioned bounded transform producing a declared schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowTransform {
    /// Transform contract version.
    pub version: u32,
    /// Declarative expression root.
    pub expression: WorkflowTransformExpression,
    /// Exact schema of the produced value.
    pub output: ValueSchema,
}

impl WorkflowTransform {
    /// Validate and evaluate this transform from explicit named inputs.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, unknown or ambiguous sources, missing fields,
    /// excessive size/depth/operations, merge conflicts, non-object merges, invalid output
    /// schemas, or output schema mismatch.
    pub fn evaluate(
        &self,
        inputs: &[WorkflowTransformInput<'_>],
    ) -> Result<serde_json::Value, WorkflowError> {
        self.validate()?;
        let mut named = BTreeMap::new();
        for input in inputs {
            if input.name.trim().is_empty() || named.insert(input.name, input.value).is_some() {
                return Err(WorkflowError::Build {
                    path: "transform.inputs".to_string(),
                    message: "transform input names must be non-empty and unique".to_string(),
                });
            }
        }
        let mut operations = 0_usize;
        let value = evaluate_transform_expression(&self.expression, &named, 0, &mut operations)?;
        ensure_transform_value_bound("transform.output", &value)?;
        let validator = jsonschema::validator_for(&self.output.schema).map_err(|error| {
            WorkflowError::Build {
                path: "transform.output".to_string(),
                message: format!("invalid transform output schema: {error}"),
            }
        })?;
        if let Err(error) = validator.validate(&value) {
            return Err(WorkflowError::Build {
                path: "transform.output".to_string(),
                message: format!("transform output does not match its declared schema: {error}"),
            });
        }
        Ok(value)
    }

    /// Validate the transform contract without evaluating it.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, invalid output schemas, or expression bounds.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if self.version != WORKFLOW_TRANSFORM_VERSION {
            return Err(WorkflowError::Build {
                path: "transform.version".to_string(),
                message: format!(
                    "unsupported workflow transform version {}; expected {}",
                    self.version, WORKFLOW_TRANSFORM_VERSION
                ),
            });
        }
        jsonschema::validator_for(&self.output.schema).map_err(|error| WorkflowError::Build {
            path: "transform.output".to_string(),
            message: format!("invalid transform output schema: {error}"),
        })?;
        let mut operations = 0_usize;
        validate_transform_expression(&self.expression, 0, &mut operations)
    }
}

fn validate_transform_expression(
    expression: &WorkflowTransformExpression,
    depth: usize,
    operations: &mut usize,
) -> Result<(), WorkflowError> {
    check_transform_budget(depth, operations)?;
    match expression {
        WorkflowTransformExpression::Input { source, path } => {
            if source.trim().is_empty() || source.len() > 256 || path.len() > 512 {
                return Err(WorkflowError::Build {
                    path: "transform.input".to_string(),
                    message: "transform input source/path exceeds bounds".to_string(),
                });
            }
        }
        WorkflowTransformExpression::Constant { value } => {
            ensure_transform_value_bound("transform.constant", value)?;
        }
        WorkflowTransformExpression::Object { fields } => {
            if fields.len() > MAX_TRANSFORM_FIELDS
                || fields
                    .keys()
                    .any(|field| field.is_empty() || field.len() > 256)
            {
                return Err(WorkflowError::Build {
                    path: "transform.object".to_string(),
                    message: format!("transform object fields exceed {MAX_TRANSFORM_FIELDS}"),
                });
            }
            for value in fields.values() {
                validate_transform_expression(value, depth + 1, operations)?;
            }
        }
        WorkflowTransformExpression::Array { items }
        | WorkflowTransformExpression::Merge { objects: items, .. } => {
            if items.len() > MAX_TRANSFORM_FIELDS {
                return Err(WorkflowError::Build {
                    path: "transform.array".to_string(),
                    message: format!("transform items exceed {MAX_TRANSFORM_FIELDS}"),
                });
            }
            for value in items {
                validate_transform_expression(value, depth + 1, operations)?;
            }
        }
        WorkflowTransformExpression::Increment { value, .. } => {
            validate_transform_expression(value, depth + 1, operations)?;
        }
        WorkflowTransformExpression::Default { value, default } => {
            validate_transform_expression(value, depth + 1, operations)?;
            validate_transform_expression(default, depth + 1, operations)?;
        }
    }
    Ok(())
}

fn check_transform_budget(depth: usize, operations: &mut usize) -> Result<(), WorkflowError> {
    if depth > MAX_TRANSFORM_DEPTH {
        return Err(WorkflowError::Build {
            path: "transform".to_string(),
            message: format!("transform depth exceeds {MAX_TRANSFORM_DEPTH}"),
        });
    }
    *operations = operations
        .checked_add(1)
        .ok_or_else(|| WorkflowError::Build {
            path: "transform".to_string(),
            message: "transform operation count overflow".to_string(),
        })?;
    if *operations > MAX_TRANSFORM_OPERATIONS {
        return Err(WorkflowError::Build {
            path: "transform".to_string(),
            message: format!("transform operations exceed {MAX_TRANSFORM_OPERATIONS}"),
        });
    }
    Ok(())
}

fn evaluate_transform_expression(
    expression: &WorkflowTransformExpression,
    inputs: &BTreeMap<&str, &serde_json::Value>,
    depth: usize,
    operations: &mut usize,
) -> Result<serde_json::Value, WorkflowError> {
    check_transform_budget(depth, operations)?;
    match expression {
        WorkflowTransformExpression::Input { source, path } => {
            let source = inputs
                .get(source.as_str())
                .ok_or_else(|| WorkflowError::Build {
                    path: source.clone(),
                    message: "transform references an unknown named input".to_string(),
                })?;
            path.split('.')
                .filter(|part| !part.is_empty())
                .try_fold(*source, |current, part| current.get(part))
                .cloned()
                .ok_or_else(|| WorkflowError::Build {
                    path: path.clone(),
                    message: "transform input field was not present".to_string(),
                })
        }
        WorkflowTransformExpression::Constant { value } => Ok(value.clone()),
        WorkflowTransformExpression::Object { fields } => fields
            .iter()
            .map(|(field, expression)| {
                evaluate_transform_expression(expression, inputs, depth + 1, operations)
                    .map(|value| (field.clone(), value))
            })
            .collect::<Result<serde_json::Map<_, _>, _>>()
            .map(serde_json::Value::Object),
        WorkflowTransformExpression::Array { items } => items
            .iter()
            .map(|item| evaluate_transform_expression(item, inputs, depth + 1, operations))
            .collect::<Result<Vec<_>, _>>()
            .map(serde_json::Value::Array),
        WorkflowTransformExpression::Merge { objects, conflict } => {
            let mut merged = serde_json::Map::new();
            for expression in objects {
                let value =
                    evaluate_transform_expression(expression, inputs, depth + 1, operations)?;
                let object = value.as_object().ok_or_else(|| WorkflowError::Build {
                    path: "transform.merge".to_string(),
                    message: "transform merge inputs must be objects".to_string(),
                })?;
                for (field, value) in object {
                    match (merged.contains_key(field), conflict) {
                        (true, TransformMergeConflict::Reject) => {
                            return Err(WorkflowError::Build {
                                path: field.clone(),
                                message: "transform merge field conflict".to_string(),
                            });
                        }
                        (true, TransformMergeConflict::KeepFirst) => {}
                        _ => {
                            merged.insert(field.clone(), value.clone());
                        }
                    }
                }
            }
            Ok(serde_json::Value::Object(merged))
        }
        WorkflowTransformExpression::Increment { value, by } => {
            let value = evaluate_transform_expression(value, inputs, depth + 1, operations)?;
            let value = value.as_i64().ok_or_else(|| WorkflowError::Build {
                path: "transform.increment".to_string(),
                message: "transform increment input must be a signed integer".to_string(),
            })?;
            value
                .checked_add(*by)
                .map(serde_json::Value::from)
                .ok_or_else(|| WorkflowError::Build {
                    path: "transform.increment".to_string(),
                    message: "transform increment overflow".to_string(),
                })
        }
        WorkflowTransformExpression::Default { value, default } => {
            match evaluate_transform_expression(value, inputs, depth + 1, operations) {
                Ok(serde_json::Value::Null) => {
                    evaluate_transform_expression(default, inputs, depth + 1, operations)
                }
                Ok(value) => Ok(value),
                Err(WorkflowError::Build { message, .. })
                    if message == "transform input field was not present" =>
                {
                    evaluate_transform_expression(default, inputs, depth + 1, operations)
                }
                Err(error) => Err(error),
            }
        }
    }
}

fn ensure_transform_value_bound(
    path: &str,
    value: &serde_json::Value,
) -> Result<(), WorkflowError> {
    validate_transform_json_depth(path, value, 0)?;
    let bytes = serde_json::to_vec(value).map_err(|error| WorkflowError::Build {
        path: path.to_string(),
        message: error.to_string(),
    })?;
    if bytes.len() > MAX_TRANSFORM_VALUE_BYTES {
        return Err(WorkflowError::Build {
            path: path.to_string(),
            message: format!("transform value exceeds {MAX_TRANSFORM_VALUE_BYTES} bytes"),
        });
    }
    Ok(())
}

fn validate_transform_json_depth(
    path: &str,
    value: &serde_json::Value,
    depth: usize,
) -> Result<(), WorkflowError> {
    if depth > MAX_TRANSFORM_DEPTH {
        return Err(WorkflowError::Build {
            path: path.to_string(),
            message: format!("transform JSON depth exceeds {MAX_TRANSFORM_DEPTH}"),
        });
    }
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                validate_transform_json_depth(path, item, depth + 1)?;
            }
        }
        serde_json::Value::Object(fields) => {
            for item in fields.values() {
                validate_transform_json_depth(path, item, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
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

    /// Validate this definition against the exact current durable-production capability set.
    ///
    /// This is intentionally stricter than [`Self::validate`], which validates the host-neutral
    /// serialized graph shared with the in-process SDK.
    ///
    /// # Errors
    ///
    /// Returns an error when the host-neutral definition itself is malformed.
    #[allow(clippy::too_many_lines)]
    pub fn production_admission(
        &self,
        capabilities: &WorkflowProductionCapabilities,
    ) -> Result<WorkflowProductionAdmission, WorkflowError> {
        self.validate()?;
        let mut diagnostics = Vec::new();
        if capabilities.capability_version != WORKFLOW_PRODUCTION_CAPABILITY_VERSION {
            diagnostics.push(WorkflowCapabilityDiagnostic {
                code: "unsupported_capability_version".to_string(),
                node_id: None,
                message: format!(
                    "workflow capability version {} is unsupported; expected {}",
                    capabilities.capability_version, WORKFLOW_PRODUCTION_CAPABILITY_VERSION
                ),
            });
        }
        if capabilities.definition_schema_version != self.schema_version {
            diagnostics.push(WorkflowCapabilityDiagnostic {
                code: "unsupported_definition_schema".to_string(),
                node_id: None,
                message: format!(
                    "definition schema version {} is unsupported by production capability version {}",
                    self.schema_version, capabilities.capability_version
                ),
            });
        }
        for node in self.nodes.values() {
            match capabilities.node_support(node.kind) {
                WorkflowCapabilitySupport::Supported => {}
                WorkflowCapabilitySupport::InProcessOnly => {
                    diagnostics.push(WorkflowCapabilityDiagnostic {
                        code: "in_process_only_node".to_string(),
                        node_id: Some(node.id.clone()),
                        message: format!(
                            "node '{}' uses {:?}, which is closure-backed and available only to the in-process SDK",
                            node.id, node.kind
                        ),
                    });
                }
                WorkflowCapabilitySupport::Unsupported => {
                    diagnostics.push(WorkflowCapabilityDiagnostic {
                        code: "unsupported_node_kind".to_string(),
                        node_id: Some(node.id.clone()),
                        message: format!(
                            "node '{}' uses {:?}, which has no complete durable production implementation",
                            node.id, node.kind
                        ),
                    });
                }
            }
            if node.kind == NodeKind::Parallel {
                if node.input != node.output {
                    diagnostics.push(WorkflowCapabilityDiagnostic {
                        code: "invalid_parallel_join_schema".to_string(),
                        node_id: Some(node.id.clone()),
                        message: format!(
                            "parallel node '{}' must declare its deterministic join tuple as both input and output",
                            node.id
                        ),
                    });
                }
                if let Err(error) = validate_parallel_join_configuration(self, node) {
                    diagnostics.push(WorkflowCapabilityDiagnostic {
                        code: "invalid_parallel_join_configuration".to_string(),
                        node_id: Some(node.id.clone()),
                        message: error.to_string(),
                    });
                }
                match serde_json::from_value::<ParallelFailurePolicy>(
                    node.configuration
                        .get("failure_policy")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!("wait_all")),
                ) {
                    Ok(policy) if capabilities.parallel_join_policies.contains(&policy) => {}
                    Ok(policy) => diagnostics.push(WorkflowCapabilityDiagnostic {
                        code: "unsupported_parallel_policy".to_string(),
                        node_id: Some(node.id.clone()),
                        message: format!(
                            "parallel node '{}' uses unsupported durable policy {policy:?}",
                            node.id
                        ),
                    }),
                    Err(error) => diagnostics.push(WorkflowCapabilityDiagnostic {
                        code: "invalid_parallel_policy".to_string(),
                        node_id: Some(node.id.clone()),
                        message: format!(
                            "parallel node '{}' has an invalid failure policy: {error}",
                            node.id
                        ),
                    }),
                }
            }
            if node.kind == NodeKind::Agent {
                validate_production_agent_node(node, capabilities, &mut diagnostics);
            }
        }
        for edge in &self.edges {
            if let Some(transform) = &edge.transform {
                if capabilities.transforms != WorkflowCapabilitySupport::Supported {
                    diagnostics.push(WorkflowCapabilityDiagnostic {
                        code: "unsupported_transform".to_string(),
                        node_id: Some(edge.from.clone()),
                        message: format!(
                            "edge '{} -> {}' uses transforms but the production host does not support them",
                            edge.from, edge.to
                        ),
                    });
                }
                if capabilities.transform_version != Some(transform.version) {
                    diagnostics.push(WorkflowCapabilityDiagnostic {
                        code: "unsupported_transform_version".to_string(),
                        node_id: Some(edge.from.clone()),
                        message: format!(
                            "edge '{} -> {}' uses unsupported transform version {}",
                            edge.from, edge.to, transform.version
                        ),
                    });
                }
                if let Err(error) = transform.validate() {
                    diagnostics.push(WorkflowCapabilityDiagnostic {
                        code: "invalid_transform".to_string(),
                        node_id: Some(edge.from.clone()),
                        message: error.to_string(),
                    });
                }
                if let Some(target) = self.node(&edge.to)
                    && transform.output != target.input
                {
                    diagnostics.push(WorkflowCapabilityDiagnostic {
                        code: "transform_target_schema_mismatch".to_string(),
                        node_id: Some(edge.from.clone()),
                        message: format!(
                            "edge '{} -> {}' transform output '{}' does not exactly match target input '{}'",
                            edge.from, edge.to, transform.output.type_name, target.input.type_name
                        ),
                    });
                }
            }
            validate_production_edge_schema(self, edge, &mut diagnostics);
            if capabilities.edge_support(edge.kind.capability_kind())
                != WorkflowCapabilitySupport::Supported
            {
                diagnostics.push(WorkflowCapabilityDiagnostic {
                    code: "unsupported_edge_kind".to_string(),
                    node_id: Some(edge.from.clone()),
                    message: format!(
                        "edge '{} -> {}' uses unsupported durable kind {:?}",
                        edge.from,
                        edge.to,
                        edge.kind.capability_kind()
                    ),
                });
            }
            match &edge.kind {
                EdgeKind::Retry { .. }
                    if capabilities.automatic_retry != WorkflowCapabilitySupport::Supported =>
                {
                    diagnostics.push(WorkflowCapabilityDiagnostic {
                        code: "unsupported_retry_edge".to_string(),
                        node_id: Some(edge.from.clone()),
                        message: format!(
                            "retry edge '{} -> {}' has no complete durable scheduler",
                            edge.from, edge.to
                        ),
                    });
                }
                EdgeKind::Conditional { predicate, .. } | EdgeKind::Back { predicate, .. } => {
                    if capabilities.predicate_version != WORKFLOW_PREDICATE_VERSION {
                        diagnostics.push(WorkflowCapabilityDiagnostic {
                            code: "unsupported_predicate_version".to_string(),
                            node_id: Some(edge.from.clone()),
                            message: format!(
                                "predicate contract version {} is unsupported",
                                capabilities.predicate_version
                            ),
                        });
                    }
                    if let Err(error) = validate_predicate_expression(predicate) {
                        diagnostics.push(WorkflowCapabilityDiagnostic {
                            code: "invalid_predicate".to_string(),
                            node_id: Some(edge.from.clone()),
                            message: error.to_string(),
                        });
                    }
                }
                EdgeKind::Direct | EdgeKind::Retry { .. } => {}
            }
        }
        Ok(WorkflowProductionAdmission {
            capabilities: capabilities.clone(),
            diagnostics,
        })
    }

    /// Validate a deserialized compiled workflow definition.
    ///
    /// This applies the same host-neutral structural invariants as SDK compilation so durable
    /// registration never trusts hand-edited or stale JSON solely because it deserializes.
    ///
    /// # Errors
    ///
    /// Returns an error when the schema version, identities, entry/exit sets, edges, bounded
    /// control-flow configuration, or cycle structure is invalid.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        validate_compiled_definition(self)
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
                    transform: None,
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
    /// Create a typed plugin-owned workflow block step.
    ///
    /// The in-process operation is supplied explicitly for local SDK execution. Durable hosts use
    /// only the validated serializable block contract and invoke the declared plugin operation.
    ///
    /// # Panics
    ///
    /// Panics when the block contract is invalid or its schemas do not match `I` and `O`.
    #[must_use]
    pub fn plugin_block<F, Fut>(block: WorkflowBlockDefinition, operation: F) -> Self
    where
        F: Fn(I, StepContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, WorkflowError>> + Send + 'static,
    {
        block
            .validate()
            .expect("plugin workflow block must be valid");
        assert_eq!(
            block.input,
            ValueSchema::of::<I>(),
            "plugin block input schema mismatch"
        );
        assert_eq!(
            block.output,
            ValueSchema::of::<O>(),
            "plugin block output schema mismatch"
        );
        let name = block.block_id.clone();
        let resources = block.resources.clone();
        Self::configured_task(
            name,
            NodeKind::PluginBlock,
            serde_json::to_value(block).expect("workflow block contract should serialize"),
            operation,
        )
        .resources(resources)
    }

    /// Create a durable external-input gate.
    ///
    /// The in-process operation is an identity mapping so the same definition remains executable
    /// without a daemon. Durable hosts recognize the `Input` node kind and wait for schema-validated
    /// external input before completing the activation.
    #[must_use]
    pub fn input(name: impl Into<String>) -> Step<I, I>
    where
        I: Serialize + DeserializeOwned,
    {
        Step::configured_task(
            name,
            NodeKind::Input,
            serde_json::json!({"gate_version": 1}),
            |input, _context| async move { Ok(input) },
        )
    }

    /// Create a durable approval gate.
    ///
    /// Durable hosts require an explicit boolean decision. Approval forwards the typed input;
    /// denial terminates the activation without enabling downstream work.
    #[must_use]
    pub fn approval(name: impl Into<String>) -> Step<I, I>
    where
        I: Serialize + DeserializeOwned,
    {
        Step::configured_task(
            name,
            NodeKind::Approval,
            serde_json::json!({"gate_version": 1}),
            |input, _context| async move { Ok(input) },
        )
    }

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
            dataflow: WorkflowNodeDataflowPolicy::Direct,
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

    /// Select where a daemon-hosted agent leaf executes.
    ///
    /// # Panics
    ///
    /// Panics when called on a composed flow or a non-agent leaf.
    #[must_use]
    pub fn agent_execution_target(mut self, target: AgentExecutionTarget) -> Self {
        let leaf = self
            .leaf_node_id
            .as_ref()
            .expect("agent execution target can only be set on a leaf step");
        let node = self
            .fragment
            .nodes
            .iter_mut()
            .find(|node| &node.id == leaf)
            .expect("leaf node should be present");
        assert_eq!(
            node.kind,
            NodeKind::Agent,
            "agent execution target requires an agent node"
        );
        let configuration = node
            .configuration
            .as_object_mut()
            .expect("agent node configuration must be an object");
        configuration.insert(
            "execution_target".to_string(),
            serde_json::to_value(target).expect("agent execution target should serialize"),
        );
        self
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
    #[allow(clippy::too_many_lines)]
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
            dataflow: WorkflowNodeDataflowPolicy::Direct,
            input: ValueSchema::of::<O>(),
            output: ValueSchema::of::<O>(),
            resources: Vec::new(),
            configuration: serde_json::json!({
                "predicate_version": WORKFLOW_PREDICATE_VERSION,
                "predicate": expression,
                "max_iterations": max_iterations,
                "iteration_state": "explicit_back_edge_transform",
            }),
        });
        for exit in &body_exits {
            fragment.edges.push(EdgeDefinition {
                from: exit.clone(),
                to: repeat_id.clone(),
                kind: EdgeKind::Direct,
                transform: None,
            });
        }
        let iteration_transform =
            (ValueSchema::of::<O>() == ValueSchema::of::<I>()).then(|| WorkflowTransform {
                version: WORKFLOW_TRANSFORM_VERSION,
                expression: WorkflowTransformExpression::Merge {
                    objects: vec![
                        WorkflowTransformExpression::Input {
                            source: "current".to_string(),
                            path: String::new(),
                        },
                        WorkflowTransformExpression::Object {
                            fields: BTreeMap::from([(
                                "iteration".to_string(),
                                WorkflowTransformExpression::Increment {
                                    value: Box::new(WorkflowTransformExpression::Input {
                                        source: "current".to_string(),
                                        path: "iteration".to_string(),
                                    }),
                                    by: 1,
                                },
                            )]),
                        },
                    ],
                    conflict: TransformMergeConflict::KeepLast,
                },
                output: ValueSchema::of::<I>(),
            });
        for entry in &body_entries {
            fragment.edges.push(EdgeDefinition {
                from: repeat_id.clone(),
                to: entry.clone(),
                kind: EdgeKind::Back {
                    predicate: expression.clone(),
                    max_iterations,
                },
                transform: iteration_transform.clone(),
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
    #[allow(clippy::too_many_lines)]
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
        let prior_run = Arc::clone(&self.run);
        let true_run = Arc::clone(&when_true.run);
        let false_run = Arc::clone(&when_false.run);
        let true_entries = when_true.fragment.entries.clone();
        let false_entries = when_false.fragment.entries.clone();
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
                transform: None,
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
                transform: None,
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
                transform: None,
            });
        }
        fragment.nodes.push(NodeDefinition {
            id: branch_id,
            name,
            kind: NodeKind::Branch,
            dataflow: WorkflowNodeDataflowPolicy::Direct,
            input: ValueSchema::of::<O>(),
            output: ValueSchema::of::<O>(),
            resources: Vec::new(),
            configuration: serde_json::json!({
                "predicate_version": WORKFLOW_PREDICATE_VERSION,
                "predicate": expression,
                "true_entries": &true_entries,
                "false_entries": &false_entries,
                "true_nodes": &true_nodes,
                "false_nodes": &false_nodes,
            }),
        });
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
                transform: None,
            });
        }
        for entry in &body_entries {
            fragment.edges.push(EdgeDefinition {
                from: retry_id.clone(),
                to: entry.clone(),
                kind: EdgeKind::Retry { max_attempts },
                transform: None,
            });
        }
        fragment.nodes.push(NodeDefinition {
            id: retry_id.clone(),
            name,
            kind: NodeKind::Retry,
            dataflow: WorkflowNodeDataflowPolicy::Direct,
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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
#[allow(clippy::too_many_lines)]
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
            transform: None,
        });
    }
    body.nodes.push(NodeDefinition {
        id: fan_out_id.clone(),
        name,
        kind: NodeKind::FanOut,
        dataflow: WorkflowNodeDataflowPolicy::Direct,
        input: ValueSchema::of::<Vec<I>>(),
        output: ValueSchema::of::<Vec<O>>(),
        resources: Vec::new(),
        configuration: serde_json::json!({
            "fan_out_version": WORKFLOW_FAN_OUT_RESULT_VERSION,
            "max_concurrency": max_concurrency,
            "ordering": "input_index_ascending",
            "member_shape": {"index": "u32", "value": "typed_output"},
            "body_entries": body_entries,
            "body_exits": body_exits,
        }),
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
        dataflow: WorkflowNodeDataflowPolicy::Direct,
        input: ValueSchema::of::<(A, B)>(),
        output: ValueSchema::of::<(A, B)>(),
        resources: Vec::new(),
        configuration: serde_json::json!({
            "failure_policy": failure_policy,
            "left_exits": &left_fragment.exits,
            "right_exits": &right_fragment.exits,
        }),
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
            transform: None,
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
                            | NodeKind::PluginBlock
                            | NodeKind::Input
                            | NodeKind::Approval
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
        if let Some(transform) = &edge.transform {
            transform.validate()?;
            if transform.output != nodes[&edge.to].input {
                return Err(WorkflowError::Build {
                    path: edge.from.clone(),
                    message: format!(
                        "edge transform output does not match target input for '{} -> {}'",
                        edge.from, edge.to
                    ),
                });
            }
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
    let definition = WorkflowDefinition {
        schema_version: WORKFLOW_DEFINITION_SCHEMA_VERSION,
        name: name.to_string(),
        input: ValueSchema::of::<I>(),
        output: ValueSchema::of::<O>(),
        nodes,
        entries: fragment.entries.clone(),
        exits: fragment.exits.clone(),
        edges,
    };
    definition.validate()?;
    Ok(definition)
}

#[allow(clippy::too_many_lines)]
fn validate_compiled_definition(definition: &WorkflowDefinition) -> Result<(), WorkflowError> {
    if definition.schema_version != WORKFLOW_DEFINITION_SCHEMA_VERSION {
        return Err(WorkflowError::Build {
            path: definition.name.clone(),
            message: format!(
                "unsupported workflow definition schema version {}; expected {}",
                definition.schema_version, WORKFLOW_DEFINITION_SCHEMA_VERSION
            ),
        });
    }
    if definition.name.trim().is_empty() {
        return Err(WorkflowError::Build {
            path: "workflow".to_string(),
            message: "name must not be empty".to_string(),
        });
    }
    if definition.nodes.is_empty() {
        return Err(WorkflowError::Build {
            path: definition.name.clone(),
            message: "workflow must contain at least one step".to_string(),
        });
    }
    if definition.nodes.len() > MAX_DEFINITION_NODES
        || definition.edges.len() > MAX_DEFINITION_EDGES
        || definition.entries.len() > MAX_DEFINITION_BOUNDARIES
        || definition.exits.len() > MAX_DEFINITION_BOUNDARIES
    {
        return Err(WorkflowError::Build {
            path: definition.name.clone(),
            message: format!(
                "workflow exceeds definition bounds: nodes<={MAX_DEFINITION_NODES}, edges<={MAX_DEFINITION_EDGES}, entries/exits<={MAX_DEFINITION_BOUNDARIES}"
            ),
        });
    }
    if definition.entries.is_empty() || definition.exits.is_empty() {
        return Err(WorkflowError::Build {
            path: definition.name.clone(),
            message: "workflow must have at least one entry and one exit".to_string(),
        });
    }
    for (id, node) in &definition.nodes {
        if id.trim().is_empty() || node.id.trim().is_empty() || id != &node.id {
            return Err(WorkflowError::Build {
                path: definition.name.clone(),
                message: format!("node map identity does not match node identity: '{id}'"),
            });
        }
        if node.name.trim().is_empty() {
            return Err(WorkflowError::Build {
                path: node.id.clone(),
                message: "step name must not be empty".to_string(),
            });
        }
        if node.kind == NodeKind::WorkflowCall && !node.resources.is_empty() {
            return Err(WorkflowError::Build {
                path: node.id.clone(),
                message:
                    "workflow call nodes must not retain resource leases while awaiting children"
                        .to_string(),
            });
        }
        validate_control_node(node)?;
        if node.kind == NodeKind::WorkflowCall {
            let call: WorkflowCallConfiguration =
                serde_json::from_value(node.configuration.clone()).map_err(|error| {
                    WorkflowError::Build {
                        path: node.id.clone(),
                        message: format!("workflow call configuration is invalid: {error}"),
                    }
                })?;
            call.validate()?;
        }
    }
    for id in definition.entries.iter().chain(&definition.exits) {
        if !definition.nodes.contains_key(id) {
            return Err(WorkflowError::Build {
                path: definition.name.clone(),
                message: format!("entry/exit references unknown step '{id}'"),
            });
        }
    }
    for edge in &definition.edges {
        if !definition.nodes.contains_key(&edge.from) || !definition.nodes.contains_key(&edge.to) {
            return Err(WorkflowError::Build {
                path: definition.name.clone(),
                message: format!(
                    "edge references unknown step '{} -> {}'",
                    edge.from, edge.to
                ),
            });
        }
        if let Some(transform) = &edge.transform {
            transform.validate()?;
            if transform.output != definition.nodes[&edge.to].input {
                return Err(WorkflowError::Build {
                    path: edge.from.clone(),
                    message: format!(
                        "edge transform output does not match target input for '{} -> {}'",
                        edge.from, edge.to
                    ),
                });
            }
        }
        if matches!(
            edge.kind,
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
        match &edge.kind {
            EdgeKind::Conditional { predicate, .. } | EdgeKind::Back { predicate, .. } => {
                validate_predicate_expression(predicate)?;
            }
            EdgeKind::Direct | EdgeKind::Retry { .. } => {}
        }
    }
    ensure_acyclic(&definition.name, &definition.nodes, &definition.edges)
}

fn validate_production_edge_schema(
    definition: &WorkflowDefinition,
    edge: &EdgeDefinition,
    diagnostics: &mut Vec<WorkflowCapabilityDiagnostic>,
) {
    let Some(source) = definition.node(&edge.from) else {
        return;
    };
    let Some(target) = definition.node(&edge.to) else {
        return;
    };
    if edge.transform.is_some() {
        return;
    }
    let compatible = match edge.kind {
        EdgeKind::Direct | EdgeKind::Conditional { .. } => {
            if target.kind == NodeKind::Parallel {
                return;
            }
            source.output == target.input
        }
        EdgeKind::Back { .. } => source.output == target.input,
        EdgeKind::Retry { .. } => false,
    };
    if !compatible && !matches!(edge.kind, EdgeKind::Retry { .. }) {
        diagnostics.push(WorkflowCapabilityDiagnostic {
            code: "incompatible_edge_schema".to_string(),
            node_id: Some(edge.from.clone()),
            message: format!(
                "edge '{} -> {}' requires an explicit typed transform: source output '{}' does not exactly match target input '{}'",
                edge.from, edge.to, source.output.type_name, target.input.type_name
            ),
        });
    }
}

fn validate_production_agent_node(
    node: &NodeDefinition,
    capabilities: &WorkflowProductionCapabilities,
    diagnostics: &mut Vec<WorkflowCapabilityDiagnostic>,
) {
    let Some(configuration) = node.configuration.as_object() else {
        diagnostics.push(WorkflowCapabilityDiagnostic {
            code: "invalid_agent_configuration".to_string(),
            node_id: Some(node.id.clone()),
            message: format!("agent node '{}' configuration must be an object", node.id),
        });
        return;
    };
    if configuration.contains_key("version") {
        let contract = match serde_json::from_value::<WorkflowAgentConfiguration>(
            node.configuration.clone(),
        ) {
            Ok(contract) => contract,
            Err(error) => {
                diagnostics.push(WorkflowCapabilityDiagnostic {
                    code: "invalid_agent_configuration".to_string(),
                    node_id: Some(node.id.clone()),
                    message: format!(
                        "agent node '{}' has an invalid versioned configuration: {error}",
                        node.id
                    ),
                });
                return;
            }
        };
        if let Err(error) = contract.validate() {
            diagnostics.push(WorkflowCapabilityDiagnostic {
                code: "invalid_agent_configuration".to_string(),
                node_id: Some(node.id.clone()),
                message: error.to_string(),
            });
        }
    } else if configuration
        .get("configuration_version")
        .is_some_and(|value| {
            value.as_u64() != Some(u64::from(capabilities.agent_configuration_version))
        })
    {
        diagnostics.push(WorkflowCapabilityDiagnostic {
            code: "unsupported_agent_configuration_version".to_string(),
            node_id: Some(node.id.clone()),
            message: format!(
                "agent node '{}' must use configuration version {}",
                node.id, capabilities.agent_configuration_version
            ),
        });
    }
    if configuration
        .get("prompt_mode")
        .and_then(serde_json::Value::as_str)
        != Some("json_input")
    {
        diagnostics.push(WorkflowCapabilityDiagnostic {
            code: "unsupported_agent_prompt_mode".to_string(),
            node_id: Some(node.id.clone()),
            message: format!(
                "agent node '{}' must use the durable json_input prompt mode",
                node.id
            ),
        });
    }
    let execution_target = configuration.get("execution_target").map_or_else(
        || Ok(AgentExecutionTarget::FreshIsolated),
        |value| serde_json::from_value(value.clone()),
    );
    match execution_target {
        Ok(target) if capabilities.agent_execution_targets.contains(&target) => {}
        Ok(target) => diagnostics.push(WorkflowCapabilityDiagnostic {
            code: "unsupported_agent_execution_target".to_string(),
            node_id: Some(node.id.clone()),
            message: format!(
                "agent node '{}' uses unsupported execution target {target:?}",
                node.id
            ),
        }),
        Err(error) => diagnostics.push(WorkflowCapabilityDiagnostic {
            code: "invalid_agent_execution_target".to_string(),
            node_id: Some(node.id.clone()),
            message: format!(
                "agent node '{}' has an invalid execution target: {error}",
                node.id
            ),
        }),
    }
}

fn validate_predicate_expression(expression: &PredicateExpression) -> Result<(), WorkflowError> {
    let mut operations = 0;
    validate_predicate_expression_inner(expression, 1, &mut operations)
}

fn validate_predicate_expression_inner(
    expression: &PredicateExpression,
    depth: usize,
    operations: &mut usize,
) -> Result<(), WorkflowError> {
    *operations = operations.saturating_add(1);
    if depth > MAX_PREDICATE_DEPTH {
        return Err(WorkflowError::Build {
            path: "predicate".to_string(),
            message: format!("predicate depth exceeds {MAX_PREDICATE_DEPTH}"),
        });
    }
    if *operations > MAX_PREDICATE_OPERATIONS {
        return Err(WorkflowError::Build {
            path: "predicate".to_string(),
            message: format!("predicate operations exceed {MAX_PREDICATE_OPERATIONS}"),
        });
    }

    let version = expression.version();
    if !(WORKFLOW_PREDICATE_MIN_VERSION..=WORKFLOW_PREDICATE_VERSION).contains(&version) {
        return Err(WorkflowError::Build {
            path: "predicate".to_string(),
            message: format!(
                "unsupported workflow predicate version {version}; expected {WORKFLOW_PREDICATE_MIN_VERSION} through {WORKFLOW_PREDICATE_VERSION}"
            ),
        });
    }

    match expression {
        PredicateExpression::Equals { path, value, .. } => {
            validate_predicate_path(path)?;
            let encoded = serde_json::to_vec(value).map_err(|error| WorkflowError::Build {
                path: path.clone(),
                message: format!("predicate value cannot be serialized: {error}"),
            })?;
            if encoded.len() > MAX_PREDICATE_VALUE_BYTES {
                return Err(WorkflowError::Build {
                    path: path.clone(),
                    message: format!("predicate value exceeds {MAX_PREDICATE_VALUE_BYTES} bytes"),
                });
            }
        }
        PredicateExpression::FieldsEqual {
            left_path,
            right_path,
            ..
        }
        | PredicateExpression::NumericCompare {
            left_path,
            right_path,
            ..
        } => {
            if version < 2 {
                return Err(predicate_operation_requires_version_two(expression));
            }
            validate_predicate_path(left_path)?;
            validate_predicate_path(right_path)?;
        }
        PredicateExpression::All { predicates, .. }
        | PredicateExpression::Any { predicates, .. } => {
            if version < 2 {
                return Err(predicate_operation_requires_version_two(expression));
            }
            if predicates.is_empty() || predicates.len() > MAX_PREDICATE_OPERATIONS {
                return Err(WorkflowError::Build {
                    path: "predicate".to_string(),
                    message: format!(
                        "predicate child count must be between 1 and {MAX_PREDICATE_OPERATIONS}"
                    ),
                });
            }
            for predicate in predicates {
                if predicate.version() != version {
                    return Err(WorkflowError::Build {
                        path: "predicate".to_string(),
                        message: "nested predicate versions must match their parent".to_string(),
                    });
                }
                validate_predicate_expression_inner(predicate, depth + 1, operations)?;
            }
        }
        PredicateExpression::Not { predicate, .. } => {
            if version < 2 {
                return Err(predicate_operation_requires_version_two(expression));
            }
            if predicate.version() != version {
                return Err(WorkflowError::Build {
                    path: "predicate".to_string(),
                    message: "nested predicate versions must match their parent".to_string(),
                });
            }
            validate_predicate_expression_inner(predicate, depth + 1, operations)?;
        }
    }
    Ok(())
}

fn predicate_operation_requires_version_two(expression: &PredicateExpression) -> WorkflowError {
    WorkflowError::Build {
        path: "predicate".to_string(),
        message: format!(
            "predicate operation requires version 2, found {}",
            expression.version()
        ),
    }
}

fn validate_predicate_path(path: &str) -> Result<(), WorkflowError> {
    if path.len() > MAX_PREDICATE_PATH_BYTES
        || path
            .split('.')
            .any(|part| part.len() > MAX_PREDICATE_PATH_SEGMENT_BYTES)
    {
        return Err(WorkflowError::Build {
            path: path.to_string(),
            message: "predicate path exceeds durable bounds".to_string(),
        });
    }
    Ok(())
}

/// Validate one parallel node's canonical left/right join membership declaration.
///
/// # Errors
///
/// Returns an error when member lists are missing, empty, duplicated, overlapping, reference
/// unknown nodes, or do not correspond to direct member-to-join edges.
pub fn validate_parallel_join_configuration(
    definition: &WorkflowDefinition,
    node: &NodeDefinition,
) -> Result<(), WorkflowError> {
    let invalid = |message: String| WorkflowError::Build {
        path: node.id.clone(),
        message,
    };
    let configured = |field: &str| -> Result<Vec<&str>, WorkflowError> {
        let values = node
            .configuration
            .get(field)
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                invalid(format!(
                    "parallel join must declare '{field}' as a non-empty array"
                ))
            })?;
        if values.is_empty() {
            return Err(invalid(format!(
                "parallel join '{field}' must not be empty"
            )));
        }
        values
            .iter()
            .map(|value| {
                value.as_str().ok_or_else(|| {
                    invalid(format!("parallel join '{field}' must contain node IDs"))
                })
            })
            .collect()
    };
    let left = configured("left_exits")?;
    let right = configured("right_exits")?;
    let left_set = left.iter().copied().collect::<BTreeSet<_>>();
    let right_set = right.iter().copied().collect::<BTreeSet<_>>();
    if left_set.len() != left.len()
        || right_set.len() != right.len()
        || !left_set.is_disjoint(&right_set)
    {
        return Err(invalid(
            "parallel join members must be unique and belong to exactly one side".to_string(),
        ));
    }
    for member in left.into_iter().chain(right) {
        if !definition.nodes.contains_key(member)
            || !definition.edges.iter().any(|edge| {
                edge.from == member && edge.to == node.id && matches!(edge.kind, EdgeKind::Direct)
            })
        {
            return Err(invalid(format!(
                "parallel join member '{member}' must exist and have a direct edge to the join"
            )));
        }
    }
    Ok(())
}

fn validate_repeat_outcome_configuration(node: &NodeDefinition) -> Result<(), WorkflowError> {
    let policy = node
        .configuration
        .get("exhaustion_policy")
        .cloned()
        .map_or(Ok(WorkflowRepeatExhaustionPolicy::Fail), |value| {
            serde_json::from_value(value).map_err(|error| WorkflowError::Build {
                path: node.id.clone(),
                message: format!("repeat exhaustion_policy is invalid: {error}"),
            })
        })?;
    if policy == WorkflowRepeatExhaustionPolicy::EmitOutcome
        && node
            .configuration
            .get("repeat_outcome_version")
            .and_then(serde_json::Value::as_u64)
            != Some(u64::from(WORKFLOW_REPEAT_OUTCOME_VERSION))
    {
        return Err(WorkflowError::Build {
            path: node.id.clone(),
            message: format!(
                "emit_outcome repeat must declare repeat_outcome_version {WORKFLOW_REPEAT_OUTCOME_VERSION}"
            ),
        });
    }
    Ok(())
}

fn validate_control_node(node: &NodeDefinition) -> Result<(), WorkflowError> {
    if matches!(node.kind, NodeKind::Branch | NodeKind::Repeat) {
        let declared_version = node
            .configuration
            .get("predicate_version")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| {
                (WORKFLOW_PREDICATE_MIN_VERSION..=WORKFLOW_PREDICATE_VERSION).contains(value)
            })
            .ok_or_else(|| WorkflowError::Build {
                path: node.id.clone(),
                message: format!(
                    "control node must declare predicate_version between {WORKFLOW_PREDICATE_MIN_VERSION} and {WORKFLOW_PREDICATE_VERSION}"
                ),
            })?;
        let predicate = node
            .configuration
            .get("predicate")
            .cloned()
            .ok_or_else(|| WorkflowError::Build {
                path: node.id.clone(),
                message: "control node must declare a predicate".to_string(),
            })?;
        let predicate: PredicateExpression =
            serde_json::from_value(predicate).map_err(|error| WorkflowError::Build {
                path: node.id.clone(),
                message: format!("control node predicate is invalid: {error}"),
            })?;
        if predicate.version() != declared_version {
            return Err(WorkflowError::Build {
                path: node.id.clone(),
                message: "control node predicate_version must match the predicate expression"
                    .to_string(),
            });
        }
        validate_predicate_expression(&predicate)?;
        if matches!(node.kind, NodeKind::Repeat) {
            validate_repeat_outcome_configuration(node)?;
        }
    }
    if node.kind == NodeKind::FanOut
        && (node
            .configuration
            .get("fan_out_version")
            .and_then(serde_json::Value::as_u64)
            != Some(u64::from(WORKFLOW_FAN_OUT_RESULT_VERSION))
            || node
                .configuration
                .get("ordering")
                .and_then(serde_json::Value::as_str)
                != Some("input_index_ascending"))
    {
        return Err(WorkflowError::Build {
            path: node.id.clone(),
            message: format!(
                "fan_out must declare version {WORKFLOW_FAN_OUT_RESULT_VERSION} and input-index ordering"
            ),
        });
    }
    let invalid = match node.kind {
        NodeKind::Repeat => node
            .configuration
            .get("max_iterations")
            .and_then(serde_json::Value::as_u64)
            .is_none_or(|value| value == 0),
        NodeKind::Retry => node
            .configuration
            .get("max_attempts")
            .and_then(serde_json::Value::as_u64)
            .is_none_or(|value| value == 0),
        _ => false,
    };
    if invalid {
        return Err(WorkflowError::Build {
            path: node.id.clone(),
            message: match node.kind {
                NodeKind::Repeat => "repeat max_iterations must be greater than zero".to_string(),
                NodeKind::Retry => "retry max_attempts must be greater than zero".to_string(),
                _ => unreachable!(),
            },
        });
    }
    Ok(())
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

    #[allow(clippy::too_many_lines)]
    fn authored_document() -> WorkflowAuthoringDocument {
        let value_schema = ValueSchema {
            type_name: "example.value/v1".to_string(),
            schema: serde_json::json!({
                "$schema": WORKFLOW_AUTHORING_JSON_SCHEMA_DIALECT,
                "type": "object",
                "additionalProperties": false,
                "required": ["message"],
                "properties": {
                    "message": {"type": "string"},
                    "duration_ms": {"type": "integer", "minimum": 1}
                }
            }),
        };
        WorkflowAuthoringDocument {
            schema_version: WORKFLOW_AUTHORING_DOCUMENT_VERSION,
            workflow_id: "workflow/example".to_string(),
            metadata: WorkflowAuthoringMetadata {
                title: "Example workflow".to_string(),
                description: Some("Portable authored workflow fixture".to_string()),
                labels: BTreeMap::from([("purpose".to_string(), "test".to_string())]),
            },
            configuration_schema: value_schema.clone(),
            configuration_defaults: Some(serde_json::json!({
                "message": "review",
                "duration_ms": 60000
            })),
            definition: WorkflowDefinition {
                schema_version: WORKFLOW_DEFINITION_SCHEMA_VERSION,
                name: "example".to_string(),
                input: value_schema.clone(),
                output: value_schema.clone(),
                nodes: BTreeMap::from([(
                    "agent".to_string(),
                    NodeDefinition {
                        id: "agent".to_string(),
                        name: "Agent".to_string(),
                        kind: NodeKind::Agent,
                        dataflow: WorkflowNodeDataflowPolicy::Direct,
                        input: value_schema.clone(),
                        output: value_schema.clone(),
                        resources: vec![ResourceClaim::read("repository")],
                        configuration: serde_json::to_value(WorkflowAgentConfiguration {
                            version: WORKFLOW_AGENT_CONFIGURATION_VERSION,
                            execution_target: AgentExecutionTarget::FreshIsolated,
                            agent_profile: "review".to_string(),
                            provider: None,
                            model: None,
                            structured_output: AgentStructuredOutputPolicy {
                                schema: value_schema,
                                strict: true,
                            },
                            read_only: true,
                            tool_capability: WorkflowToolCapability::ReadOnly,
                            tool_allowlist: Vec::new(),
                            timeout_ms: 30_000,
                            skills: Vec::new(),
                            prompt_mode: "json_input".to_string(),
                            system_prompt: "Review the structured input.".to_string(),
                        })
                        .expect("agent configuration"),
                    },
                )]),
                entries: vec!["agent".to_string()],
                exits: vec!["agent".to_string()],
                edges: Vec::new(),
            },
            bindings: vec![
                WorkflowConfigurationBinding {
                    version: WORKFLOW_CONFIGURATION_BINDING_VERSION,
                    configuration_path: "message".to_string(),
                    target: WorkflowConfigurationTarget::AgentSelection {
                        node_id: "agent".to_string(),
                        field: "agent_profile".to_string(),
                    },
                    transform: None,
                },
                WorkflowConfigurationBinding {
                    version: WORKFLOW_CONFIGURATION_BINDING_VERSION,
                    configuration_path: "duration_ms".to_string(),
                    target: WorkflowConfigurationTarget::RunLimit {
                        field: "maximum_duration_ms".to_string(),
                    },
                    transform: None,
                },
                WorkflowConfigurationBinding {
                    version: WORKFLOW_CONFIGURATION_BINDING_VERSION,
                    configuration_path: "message".to_string(),
                    target: WorkflowConfigurationTarget::InputDefault {
                        path: "message".to_string(),
                    },
                    transform: None,
                },
            ],
            requirements: WorkflowRequirementSummary {
                capabilities: BTreeSet::from(["workflow-production/v1".to_string()]),
                plugins: BTreeSet::new(),
                blocks: BTreeSet::new(),
                agents: BTreeSet::from(["review".to_string()]),
                skills: BTreeSet::new(),
            },
            run_limits: WorkflowRunLimitPolicy::default(),
            producer: WorkflowProducerProvenance {
                kind: WorkflowProducerKind::Generated,
                producer_id: Some("test-generator".to_string()),
                source_revision: Some(WorkflowRevisionIdentity {
                    workflow_id: "workflow/source".to_string(),
                    revision: 1,
                }),
            },
            presentation: Some(WorkflowAuthoringPresentation {
                version: WORKFLOW_AUTHORING_PRESENTATION_VERSION,
                namespaces: BTreeMap::from([(
                    "bcode.graph".to_string(),
                    serde_json::json!({"agent": {"x": 10, "y": 20}}),
                )]),
            }),
        }
    }

    fn authoring_catalog() -> WorkflowAuthoringCatalogSnapshot {
        WorkflowAuthoringCatalogSnapshot {
            version: WORKFLOW_AUTHORING_CATALOG_VERSION,
            capabilities: WorkflowAuthoringCapabilitySummary::from(
                &WorkflowProductionCapabilities::current(),
            ),
            plugins: BTreeSet::new(),
            blocks: BTreeMap::new(),
            workflow_definitions: BTreeMap::new(),
            agent_profiles: BTreeSet::from(["build".to_string(), "review".to_string()]),
            skills: BTreeSet::new(),
        }
    }

    fn authored_mutating_block_document() -> (
        WorkflowAuthoringDocument,
        WorkflowAuthoringCatalogSnapshot,
        WorkflowBlockDefinition,
    ) {
        let mut document = authored_document();
        let block = WorkflowBlockDefinition {
            block_id: "example.commit".to_string(),
            block_version: 1,
            plugin_id: "bcode.example".to_string(),
            operation: "example.commit".to_string(),
            input: document.definition.input.clone(),
            output: document.definition.output.clone(),
            effect: WorkflowBlockEffect::Mutating,
            resources: vec![ResourceClaim::write("repository")],
            authorization: WorkflowBlockAuthorization {
                capability: WorkflowToolCapability::Mutating,
                explicit_grant_required: true,
            },
            timeout_ms: 30_000,
            cancellation_supported: true,
            reconciliation: WorkflowBlockReconciliation::RepairRequired,
        };
        document.definition.nodes = BTreeMap::from([(
            "commit".to_string(),
            NodeDefinition {
                id: "commit".to_string(),
                name: "Commit".to_string(),
                kind: NodeKind::PluginBlock,
                dataflow: WorkflowNodeDataflowPolicy::Direct,
                input: block.input.clone(),
                output: block.output.clone(),
                resources: block.resources.clone(),
                configuration: serde_json::to_value(&block).expect("block configuration"),
            },
        )]);
        document.definition.entries = vec!["commit".to_string()];
        document.definition.exits = vec!["commit".to_string()];
        document.bindings = vec![WorkflowConfigurationBinding {
            version: WORKFLOW_CONFIGURATION_BINDING_VERSION,
            configuration_path: "message".to_string(),
            target: WorkflowConfigurationTarget::PluginBlockInput {
                node_id: "commit".to_string(),
                path: "message".to_string(),
            },
            transform: None,
        }];
        document.requirements = WorkflowRequirementSummary::default();
        let mut catalog = authoring_catalog();
        catalog.plugins.insert(block.plugin_id.clone());
        catalog
            .blocks
            .insert(workflow_block_catalog_key(&block), block.clone());
        (document, catalog, block)
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn portable_json_catalog_fixture_composes_agent_control_and_exact_block_safety() {
        let mut document = authored_document();
        document.workflow_id = "workflow/portable-composed".to_string();
        document.definition.name = "portable-composed".to_string();
        document.bindings.clear();
        document.configuration_defaults = Some(serde_json::json!({"message": "review"}));
        document.configuration_schema.schema = serde_json::json!({
            "$schema": WORKFLOW_AUTHORING_JSON_SCHEMA_DIALECT,
            "type": "object",
            "additionalProperties": false,
            "required": ["message"],
            "properties": {"message": {"type": "string"}}
        });
        let schema = document.definition.input.clone();
        let read = WorkflowBlockDefinition {
            block_id: "example.read".to_string(),
            block_version: 1,
            plugin_id: "bcode.example".to_string(),
            operation: "example.read".to_string(),
            input: schema.clone(),
            output: schema.clone(),
            effect: WorkflowBlockEffect::ReadOnly,
            resources: vec![ResourceClaim::read("repository")],
            authorization: WorkflowBlockAuthorization {
                capability: WorkflowToolCapability::ReadOnly,
                explicit_grant_required: false,
            },
            timeout_ms: 30_000,
            cancellation_supported: true,
            reconciliation: WorkflowBlockReconciliation::IdempotentReplay,
        };
        let mutate = WorkflowBlockDefinition {
            block_id: "example.mutate".to_string(),
            block_version: 1,
            plugin_id: "bcode.example".to_string(),
            operation: "example.mutate".to_string(),
            input: schema.clone(),
            output: schema.clone(),
            effect: WorkflowBlockEffect::Mutating,
            resources: vec![ResourceClaim::write("repository")],
            authorization: WorkflowBlockAuthorization {
                capability: WorkflowToolCapability::Mutating,
                explicit_grant_required: true,
            },
            timeout_ms: 30_000,
            cancellation_supported: true,
            reconciliation: WorkflowBlockReconciliation::RepairRequired,
        };
        let predicate = PredicateExpression::Equals {
            version: WORKFLOW_PREDICATE_VERSION,
            path: "message".to_string(),
            value: serde_json::json!("review"),
        };
        let agent = document
            .definition
            .nodes
            .get("agent")
            .expect("agent")
            .clone();
        let branch = NodeDefinition {
            id: "branch".to_string(),
            name: "Branch".to_string(),
            kind: NodeKind::Branch,
            dataflow: WorkflowNodeDataflowPolicy::Direct,
            input: schema.clone(),
            output: schema.clone(),
            resources: Vec::new(),
            configuration: serde_json::json!({
                "predicate_version": WORKFLOW_PREDICATE_VERSION,
                "predicate": predicate,
                "true_entries": ["read"],
                "false_entries": ["read"],
                "true_nodes": ["read"],
                "false_nodes": []
            }),
        };
        let repeat = NodeDefinition {
            id: "repeat".to_string(),
            name: "Repeat".to_string(),
            kind: NodeKind::Repeat,
            dataflow: WorkflowNodeDataflowPolicy::Direct,
            input: schema.clone(),
            output: schema.clone(),
            resources: Vec::new(),
            configuration: serde_json::json!({
                "predicate_version": WORKFLOW_PREDICATE_VERSION,
                "predicate": PredicateExpression::Equals {
                    version: WORKFLOW_PREDICATE_VERSION,
                    path: "message".to_string(),
                    value: serde_json::json!("repeat")
                },
                "max_iterations": 2,
                "iteration_state": "explicit_back_edge_transform"
            }),
        };
        document.definition.nodes = BTreeMap::from([
            ("agent".to_string(), agent),
            ("branch".to_string(), branch),
            (
                "read".to_string(),
                NodeDefinition {
                    id: "read".to_string(),
                    name: "Read".to_string(),
                    kind: NodeKind::PluginBlock,
                    dataflow: WorkflowNodeDataflowPolicy::Direct,
                    input: schema.clone(),
                    output: schema.clone(),
                    resources: read.resources.clone(),
                    configuration: serde_json::to_value(&read).expect("read block"),
                },
            ),
            ("repeat".to_string(), repeat),
            (
                "mutate".to_string(),
                NodeDefinition {
                    id: "mutate".to_string(),
                    name: "Mutate".to_string(),
                    kind: NodeKind::PluginBlock,
                    dataflow: WorkflowNodeDataflowPolicy::Direct,
                    input: schema.clone(),
                    output: schema,
                    resources: mutate.resources.clone(),
                    configuration: serde_json::to_value(&mutate).expect("mutating block"),
                },
            ),
        ]);
        document.definition.entries = vec!["agent".to_string()];
        document.definition.exits = vec!["mutate".to_string()];
        document.definition.edges = vec![
            EdgeDefinition {
                from: "agent".to_string(),
                to: "branch".to_string(),
                kind: EdgeKind::Direct,
                transform: None,
            },
            EdgeDefinition {
                from: "branch".to_string(),
                to: "read".to_string(),
                kind: EdgeKind::Conditional {
                    predicate: PredicateExpression::Equals {
                        version: WORKFLOW_PREDICATE_VERSION,
                        path: "message".to_string(),
                        value: serde_json::json!("review"),
                    },
                    expected: true,
                },
                transform: None,
            },
            EdgeDefinition {
                from: "read".to_string(),
                to: "repeat".to_string(),
                kind: EdgeKind::Direct,
                transform: None,
            },
            EdgeDefinition {
                from: "repeat".to_string(),
                to: "mutate".to_string(),
                kind: EdgeKind::Direct,
                transform: None,
            },
        ];
        document.requirements = WorkflowRequirementSummary::default();

        let portable_json = serde_json::to_string(&document).expect("portable JSON");
        let decoded: WorkflowAuthoringDocument =
            serde_json::from_str(&portable_json).expect("portable JSON document");
        let mut catalog = authoring_catalog();
        catalog.plugins.insert("bcode.example".to_string());
        catalog
            .blocks
            .insert(workflow_block_catalog_key(&read), read.clone());
        catalog
            .blocks
            .insert(workflow_block_catalog_key(&mutate), mutate.clone());
        let preview = decoded.compilation_preview(&catalog, None);
        assert!(
            preview.is_compiled(),
            "{:?}",
            preview.validation.diagnostics
        );
        let compiled = preview.compiled.expect("compiled");
        assert_eq!(
            compiled
                .definition
                .nodes
                .values()
                .map(|node| node.kind)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                NodeKind::Agent,
                NodeKind::Branch,
                NodeKind::Repeat,
                NodeKind::PluginBlock,
            ])
        );
        assert!(
            compiled
                .effects
                .block_effects
                .contains(&WorkflowBlockEffect::ReadOnly)
        );
        assert!(
            compiled
                .effects
                .block_effects
                .contains(&WorkflowBlockEffect::Mutating)
        );
        assert_eq!(compiled.permissions.explicit_grant_nodes, ["mutate"]);
        assert_eq!(compiled.permissions.mutation_approval_nodes, ["mutate"]);
        assert_eq!(
            compiled.effects.maximum_capability,
            WorkflowToolCapability::Mutating
        );
    }

    #[test]
    fn authoring_compilation_preview_binds_and_admits_exact_definition() {
        let document = authored_document();
        let catalog = authoring_catalog();
        let preview = document.compilation_preview(
            &catalog,
            Some(&serde_json::json!({
                "message": "build",
                "duration_ms": 45000
            })),
        );
        assert!(
            preview.is_compiled(),
            "{:?}",
            preview.validation.diagnostics
        );
        let compiled = preview.compiled.as_ref().expect("compiled preview");
        let agent: WorkflowAgentConfiguration = serde_json::from_value(
            compiled
                .definition
                .node("agent")
                .expect("agent")
                .configuration
                .clone(),
        )
        .expect("agent configuration");
        assert_eq!(agent.agent_profile, "build");
        assert_eq!(compiled.run_limits.maximum_duration_ms, Some(45_000));
        assert_eq!(
            compiled.input_defaults,
            serde_json::json!({"message": "build"})
        );
        assert!(compiled.production_admission.is_supported());
        assert!(compiled.requirements.agents.contains("build"));
        assert_eq!(
            compiled.effects.maximum_capability,
            WorkflowToolCapability::ReadOnly
        );
        assert_eq!(
            compiled.permissions.maximum_capability,
            WorkflowToolCapability::ReadOnly
        );
        assert_eq!(
            compiled.definition_identity,
            WorkflowDefinitionIdentity::for_definition(
                document.workflow_id.clone(),
                &compiled.definition
            )
            .expect("definition identity")
        );
        round_trip(&catalog);
        round_trip(&preview);

        let mut presented = document;
        presented
            .presentation
            .as_mut()
            .expect("presentation")
            .namespaces
            .insert("other.editor".to_string(), serde_json::json!({"x": 99}));
        presented.metadata.title = "Different display title".to_string();
        presented.metadata.description = Some("Presentation-only description".to_string());
        presented.metadata.labels.insert(
            "reviewer-visible".to_string(),
            "presentation-only".to_string(),
        );
        presented.producer = WorkflowProducerProvenance {
            kind: WorkflowProducerKind::Cli,
            producer_id: Some("different-producer".to_string()),
            source_revision: None,
        };
        let presented_preview = presented.compilation_preview(
            &catalog,
            Some(&serde_json::json!({
                "message": "build",
                "duration_ms": 45000
            })),
        );
        assert_eq!(
            presented_preview.compiled.expect("presented compiled"),
            compiled.clone()
        );
    }

    #[test]
    fn producer_kind_does_not_change_compilation_or_local_authorization() {
        let catalog = authoring_catalog();
        let mut sdk = authored_document();
        sdk.producer = WorkflowProducerProvenance {
            kind: WorkflowProducerKind::Sdk,
            producer_id: Some("portable-sdk".to_string()),
            source_revision: None,
        };
        let mut plugin = sdk.clone();
        plugin.producer = WorkflowProducerProvenance {
            kind: WorkflowProducerKind::Plugin,
            producer_id: Some("example.generator".to_string()),
            source_revision: None,
        };
        let sdk_compiled = sdk
            .compilation_preview(&catalog, None)
            .compiled
            .expect("sdk compilation");
        let plugin_compiled = plugin
            .compilation_preview(&catalog, None)
            .compiled
            .expect("plugin compilation");
        assert_eq!(sdk_compiled, plugin_compiled);
        assert_ne!(
            sdk.source_digest_sha256().expect("sdk source"),
            plugin.source_digest_sha256().expect("plugin source")
        );
        assert_eq!(
            sdk.executable_source_digest_sha256().expect("sdk identity"),
            plugin
                .executable_source_digest_sha256()
                .expect("plugin identity")
        );
    }

    #[test]
    fn authoring_compilation_preview_resolves_exact_mutation_facts() {
        let (document, catalog, block) = authored_mutating_block_document();
        let preview = document.compilation_preview(&catalog, None);
        assert!(
            preview.is_compiled(),
            "{:?}",
            preview.validation.diagnostics
        );
        let compiled = preview.compiled.expect("compiled preview");
        assert_eq!(
            compiled.plugin_input_defaults["commit"],
            serde_json::json!({"message": "review"})
        );
        assert!(compiled.requirements.plugins.contains(&block.plugin_id));
        assert!(
            compiled
                .requirements
                .blocks
                .contains(&workflow_block_catalog_key(&block))
        );
        assert_eq!(
            compiled.effects.block_effects,
            BTreeSet::from([WorkflowBlockEffect::Mutating])
        );
        assert_eq!(
            compiled.effects.reconciliation,
            BTreeSet::from([WorkflowBlockReconciliation::RepairRequired])
        );
        assert_eq!(
            compiled.effects.resources,
            vec![ResourceClaim::write("repository")]
        );
        assert_eq!(
            compiled.permissions.explicit_grant_nodes,
            vec!["commit".to_string()]
        );
        assert_eq!(
            compiled.permissions.mutation_approval_nodes,
            vec!["commit".to_string()]
        );
        assert_eq!(
            compiled.permissions.maximum_capability,
            WorkflowToolCapability::Mutating
        );
    }

    #[test]
    fn requirement_availability_is_structured_bounded_and_non_mutating() {
        let document = authored_document();
        let before = document.clone();
        let mut catalog = authoring_catalog();
        catalog.agent_profiles.clear();
        let report = workflow_requirement_availability(&document.requirements, &catalog)
            .expect("availability");
        assert!(!report.available);
        assert_eq!(
            report.unavailable,
            vec![WorkflowUnavailableRequirement {
                kind: WorkflowRequirementKind::Agent,
                identity: "review".to_string(),
            }]
        );
        assert_eq!(document, before);
        round_trip(&report);
    }

    #[test]
    fn authoring_compilation_preview_fails_closed_for_catalog_and_admission() {
        let document = authored_document();
        let mut missing = authoring_catalog();
        missing.agent_profiles.remove("review");
        let preview = document.compilation_preview(&missing, None);
        assert!(!preview.is_compiled());
        assert!(
            preview.validation.diagnostics[0]
                .message
                .contains("unavailable")
        );

        let mut unsupported = authored_document();
        unsupported.bindings.retain(|binding| {
            !matches!(
                binding.target,
                WorkflowConfigurationTarget::AgentSelection { .. }
                    | WorkflowConfigurationTarget::SkillSelection { .. }
            )
        });
        unsupported.requirements.agents.clear();
        unsupported
            .definition
            .nodes
            .get_mut("agent")
            .expect("node")
            .kind = NodeKind::Task;
        let preview = unsupported.compilation_preview(&authoring_catalog(), None);
        assert!(!preview.is_compiled());
        assert!(
            preview.validation.diagnostics[0]
                .message
                .contains("in_process_only_node"),
            "{:?}",
            preview.validation.diagnostics
        );

        let (block_document, mut mismatched, block) = authored_mutating_block_document();
        let mut changed = block;
        changed.operation = "example.changed".to_string();
        mismatched
            .blocks
            .insert(workflow_block_catalog_key(&changed), changed);
        let preview = block_document.compilation_preview(&mismatched, None);
        assert!(!preview.is_compiled());
        assert!(
            preview.validation.diagnostics[0]
                .message
                .contains("exact plugin block")
        );
    }

    #[test]
    fn application_operation_facts_are_portable_and_fail_closed() {
        let document = authored_document();
        let preview = document.compilation_preview(&authoring_catalog(), None);
        let compiled = preview.compiled.expect("compiled preview");
        let facts = WorkflowApplicationOperationFacts {
            version: WORKFLOW_APPLICATION_OPERATION_FACTS_VERSION,
            operation: WorkflowApplicationOperation::PublishDraft,
            actor: WorkflowApplicationActor {
                kind: WorkflowApplicationActorKind::LocalClient,
                actor_id: "local-client/1".to_string(),
            },
            workflow_id: document.workflow_id.clone(),
            draft_id: Some("draft/1".to_string()),
            revision: None,
            preset_id: None,
            producer: Some(document.producer),
            requirements: compiled.requirements,
            effects: compiled.effects,
            activates: true,
            executes: false,
        };
        facts.validate().expect("valid operation facts");
        let encoded = serde_json::to_value(&facts).expect("serialize operation facts");
        assert_eq!(
            serde_json::from_value::<WorkflowApplicationOperationFacts>(encoded.clone())
                .expect("deserialize operation facts"),
            facts
        );

        let mut future = facts.clone();
        future.version += 1;
        assert!(future.validate().is_err());

        let mut missing_draft = facts.clone();
        missing_draft.draft_id = None;
        assert!(missing_draft.validate().is_err());

        let mut forged_execution = facts;
        forged_execution.executes = true;
        assert!(forged_execution.validate().is_err());

        let mut unknown = encoded;
        unknown["transport_private"] = serde_json::json!(true);
        assert!(serde_json::from_value::<WorkflowApplicationOperationFacts>(unknown).is_err());
    }

    #[test]
    fn authoring_document_round_trips_and_rejects_unknown_or_future_state() {
        let document = authored_document();
        document.validate().expect("valid authoring document");
        let encoded = serde_json::to_value(&document).expect("serialize");
        assert_eq!(
            serde_json::from_value::<WorkflowAuthoringDocument>(encoded.clone())
                .expect("deserialize"),
            document
        );

        let mut future = encoded.clone();
        future["schema_version"] = serde_json::json!(WORKFLOW_AUTHORING_DOCUMENT_VERSION + 1);
        let error = serde_json::from_value::<WorkflowAuthoringDocument>(future)
            .expect("deserialize future version")
            .validate()
            .expect_err("future version must fail closed");
        assert!(error.to_string().contains("unsupported workflow authoring"));

        let mut unknown = encoded;
        unknown["unknown_future_field"] = serde_json::json!(true);
        assert!(serde_json::from_value::<WorkflowAuthoringDocument>(unknown).is_err());
    }

    #[test]
    fn authoring_digest_is_stable_and_presentation_is_not_executable_identity() {
        let document = authored_document();
        let encoded = serde_json::to_string(&document).expect("serialize");
        let decoded: WorkflowAuthoringDocument =
            serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(
            document.source_digest_sha256().expect("source digest"),
            decoded.source_digest_sha256().expect("decoded digest")
        );

        let mut presented = document.clone();
        presented
            .presentation
            .as_mut()
            .expect("presentation")
            .namespaces
            .insert(
                "other.editor".to_string(),
                serde_json::json!({"collapsed": true}),
            );
        presented.metadata.title = "Different title".to_string();
        presented.metadata.description = Some("Different description".to_string());
        presented
            .metadata
            .labels
            .insert("display".to_string(), "different".to_string());
        presented.producer = WorkflowProducerProvenance {
            kind: WorkflowProducerKind::Plugin,
            producer_id: Some("different.plugin".to_string()),
            source_revision: None,
        };
        assert_ne!(
            document.source_digest_sha256().expect("source digest"),
            presented.source_digest_sha256().expect("presented digest")
        );
        assert_eq!(
            document
                .executable_source_digest_sha256()
                .expect("identity"),
            presented
                .executable_source_digest_sha256()
                .expect("presented identity")
        );

        let mut semantic = document.clone();
        semantic
            .definition
            .nodes
            .get_mut("agent")
            .expect("node")
            .name = "Changed".to_string();
        assert_ne!(
            document
                .executable_source_digest_sha256()
                .expect("identity"),
            semantic
                .executable_source_digest_sha256()
                .expect("changed identity")
        );
    }

    #[test]
    fn authoring_normalization_canonicalizes_unordered_graph_source() {
        let mut first = authored_document();
        first
            .definition
            .nodes
            .get_mut("agent")
            .expect("node")
            .resources = vec![ResourceClaim::write("z"), ResourceClaim::read("a")];
        let mut second = first.clone();
        second
            .definition
            .nodes
            .get_mut("agent")
            .expect("node")
            .resources
            .reverse();
        assert_eq!(
            first.source_digest_sha256().expect("first digest"),
            second.source_digest_sha256().expect("second digest")
        );
        assert_eq!(
            first.normalized().expect("first normalized"),
            second.normalized().expect("second normalized")
        );
    }

    #[test]
    fn authoring_schema_local_references_resolve_and_fail_closed() {
        let mut local = authored_document();
        local.configuration_schema.schema = serde_json::json!({
            "$schema": WORKFLOW_AUTHORING_JSON_SCHEMA_DIALECT,
            "$defs": {"message": {"type": "string"}},
            "type": "object",
            "required": ["message"],
            "properties": {"message": {"$ref": "#/$defs/message"}}
        });
        local.validate().expect("bounded local reference");

        let mut missing = local;
        missing.configuration_schema.schema["properties"]["message"]["$ref"] =
            serde_json::json!("#/$defs/missing");
        assert!(missing.validate().is_err());

        let mut recursive = authored_document();
        recursive.configuration_defaults = None;
        recursive.configuration_schema.schema = serde_json::json!({
            "$defs": {"recursive": {"$ref": "#/$defs/recursive"}},
            "$ref": "#/$defs/recursive"
        });
        let error = recursive
            .validate()
            .expect_err("recursive local reference must fail closed");
        assert!(error.to_string().contains("recursive"));
    }

    fn round_trip<T>(value: &T)
    where
        T: Serialize + DeserializeOwned + PartialEq + fmt::Debug,
    {
        let encoded = serde_json::to_vec(value).expect("serialize contract");
        assert_eq!(
            serde_json::from_slice::<T>(&encoded).expect("deserialize contract"),
            *value
        );
    }

    #[test]
    fn authoring_public_contracts_round_trip_independently() {
        let document = authored_document();
        let workflow_identity = WorkflowIdentity {
            workflow_id: document.workflow_id.clone(),
        };
        let draft_identity = WorkflowDraftIdentity {
            workflow_id: document.workflow_id.clone(),
            draft_id: "draft/one".to_string(),
        };
        let revision_identity = WorkflowRevisionIdentity {
            workflow_id: document.workflow_id.clone(),
            revision: 1,
        };
        let effect_summary = WorkflowEffectSummary {
            maximum_capability: WorkflowToolCapability::Mutating,
            block_effects: BTreeSet::from([WorkflowBlockEffect::Mutating]),
            reconciliation: BTreeSet::from([WorkflowBlockReconciliation::RepairRequired]),
            resources: vec![ResourceClaim::write("repository")],
        }
        .normalized();
        effect_summary.validate().expect("effect summary");

        round_trip(&workflow_identity);
        round_trip(&draft_identity);
        round_trip(&revision_identity);
        round_trip(&WorkflowProducerKind::Generated);
        round_trip(&document.producer);
        round_trip(&document.metadata);
        round_trip(document.presentation.as_ref().expect("presentation"));
        round_trip(&document.run_limits);
        round_trip(&document.bindings[0].target);
        round_trip(&document.bindings[0]);
        round_trip(&document.requirements);
        round_trip(&effect_summary);
        round_trip(&document);
    }

    #[test]
    fn authoring_validation_report_is_structured_source_addressed_and_bounded() {
        let valid = authored_document().validation_report();
        assert!(valid.is_valid());
        assert!(valid.source_digest_sha256.is_some());
        assert!(valid.executable_source_digest_sha256.is_some());
        assert!(valid.diagnostics.is_empty());
        round_trip(&valid);

        let mut invalid = authored_document();
        invalid.bindings[0].target = WorkflowConfigurationTarget::NodeConfiguration {
            node_id: "missing".to_string(),
            path: "prompt".to_string(),
        };
        let report = invalid.validation_report();
        assert!(!report.is_valid());
        assert_eq!(report.diagnostics.len(), 1);
        assert_eq!(
            report.diagnostics[0].severity,
            WorkflowValidationSeverity::Error
        );
        assert_eq!(report.diagnostics[0].code, "unknown_reference");
        assert_eq!(
            report.diagnostics[0].document_path,
            "bindings.target.node_id"
        );
        assert!(!report.diagnostics[0].remediation.is_empty());
        round_trip(&report);
    }

    #[test]
    fn authoring_validation_rejects_malformed_identity_graph_and_binding_references() {
        let mut malformed = authored_document();
        malformed.workflow_id = "bad identity".to_string();
        assert!(malformed.validate().is_err());

        let mut mismatched_node = authored_document();
        mismatched_node
            .definition
            .nodes
            .get_mut("agent")
            .expect("node")
            .id = "other".to_string();
        assert!(mismatched_node.validate().is_err());

        let mut unknown_node = authored_document();
        unknown_node.bindings[0].target = WorkflowConfigurationTarget::NodeConfiguration {
            node_id: "missing".to_string(),
            path: "prompt".to_string(),
        };
        assert!(unknown_node.validate().is_err());

        let mut duplicate_target = authored_document();
        duplicate_target
            .bindings
            .push(duplicate_target.bindings[0].clone());
        assert!(duplicate_target.validate().is_err());

        let mut unbounded_cycle = authored_document();
        unbounded_cycle.definition.edges.push(EdgeDefinition {
            from: "agent".to_string(),
            to: "agent".to_string(),
            kind: EdgeKind::Direct,
            transform: None,
        });
        assert!(unbounded_cycle.validate().is_err());
    }

    #[test]
    fn authoring_validation_bounds_dynamic_schemas_and_json_content() {
        let mut remote_reference = authored_document();
        remote_reference.configuration_schema.schema =
            serde_json::json!({"$ref": "https://example.invalid/schema.json"});
        let error = remote_reference
            .validate()
            .expect_err("remote reference must fail");
        assert!(error.to_string().contains("remote"));

        let mut unsupported_dialect = authored_document();
        unsupported_dialect.configuration_schema.schema["$schema"] =
            serde_json::json!("https://json-schema.org/draft/2019-09/schema");
        assert!(unsupported_dialect.validate().is_err());

        let mut too_many_properties = authored_document();
        let properties = (0..=MAX_WORKFLOW_AUTHORING_SCHEMA_PROPERTIES)
            .map(|index| {
                (
                    format!("property_{index}"),
                    serde_json::json!({"type": "string"}),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        too_many_properties.configuration_schema.schema = serde_json::json!({
            "type": "object",
            "properties": properties
        });
        assert!(too_many_properties.validate().is_err());

        let mut too_deep = serde_json::json!(null);
        for _ in 0..=MAX_WORKFLOW_AUTHORING_JSON_DEPTH {
            too_deep = serde_json::json!([too_deep]);
        }
        let mut deep_presentation = authored_document();
        deep_presentation
            .presentation
            .as_mut()
            .expect("presentation")
            .namespaces
            .insert("deep".to_string(), too_deep);
        assert!(deep_presentation.validate().is_err());

        let mut oversized = authored_document();
        oversized.metadata.description =
            Some("x".repeat(MAX_WORKFLOW_AUTHORING_DESCRIPTION_BYTES + 1));
        assert!(oversized.validate().is_err());
    }

    #[test]
    fn authoring_identity_and_provenance_contracts_are_bounded() {
        WorkflowIdentity {
            workflow_id: "workflow/one".to_string(),
        }
        .validate()
        .expect("workflow identity");
        WorkflowDraftIdentity {
            workflow_id: "workflow/one".to_string(),
            draft_id: "draft/one".to_string(),
        }
        .validate()
        .expect("draft identity");
        WorkflowRevisionIdentity {
            workflow_id: "workflow/one".to_string(),
            revision: 1,
        }
        .validate()
        .expect("revision identity");
        assert!(
            WorkflowRevisionIdentity {
                workflow_id: "workflow/one".to_string(),
                revision: 0,
            }
            .validate()
            .is_err()
        );

        let mut provenance = authored_document().producer;
        provenance.producer_id = Some("x".repeat(MAX_WORKFLOW_AUTHORING_ID_BYTES + 1));
        assert!(provenance.validate().is_err());
    }

    #[test]
    fn workflow_call_preview_aggregates_child_requirements_effects_and_permissions() {
        let mut document = authored_document();
        document.bindings.clear();
        document.requirements.agents.clear();
        let mut catalog = authoring_catalog();
        let child_block = WorkflowBlockDefinition {
            block_id: "child.commit".to_string(),
            block_version: 1,
            plugin_id: "bcode.child".to_string(),
            operation: "child.commit".to_string(),
            input: document.definition.input.clone(),
            output: document.definition.output.clone(),
            effect: WorkflowBlockEffect::Mutating,
            resources: vec![ResourceClaim::write("repository")],
            authorization: WorkflowBlockAuthorization {
                capability: WorkflowToolCapability::Mutating,
                explicit_grant_required: true,
            },
            timeout_ms: 1_000,
            cancellation_supported: true,
            reconciliation: WorkflowBlockReconciliation::RepairRequired,
        };
        child_block.validate().expect("block");
        catalog.plugins.insert("bcode.child".to_string());
        catalog.blocks.insert(
            workflow_block_catalog_key(&child_block),
            child_block.clone(),
        );
        let child = WorkflowDefinition {
            schema_version: WORKFLOW_DEFINITION_SCHEMA_VERSION,
            name: "workflow/child".to_string(),
            input: document.definition.input.clone(),
            output: document.definition.output.clone(),
            nodes: BTreeMap::from([(
                "commit".to_string(),
                NodeDefinition {
                    id: "commit".to_string(),
                    name: "commit".to_string(),
                    kind: NodeKind::PluginBlock,
                    dataflow: WorkflowNodeDataflowPolicy::Direct,
                    input: child_block.input.clone(),
                    output: child_block.output.clone(),
                    resources: child_block.resources.clone(),
                    configuration: serde_json::to_value(&child_block).expect("block"),
                },
            )]),
            entries: vec!["commit".to_string()],
            exits: vec!["commit".to_string()],
            edges: Vec::new(),
        };
        child.validate().expect("child");
        let identity =
            WorkflowDefinitionIdentity::for_definition("workflow/child", &child).expect("identity");
        catalog
            .workflow_definitions
            .insert(identity.definition_id.clone(), child);
        let node = document.definition.nodes.get_mut("agent").expect("agent");
        node.kind = NodeKind::WorkflowCall;
        node.resources.clear();
        node.configuration = serde_json::to_value(WorkflowCallConfiguration {
            version: WORKFLOW_CALL_VERSION,
            target: WorkflowCallTarget::Definition { identity },
        })
        .expect("call");
        let preview = document.compilation_preview(&catalog, None);
        let compiled = preview.compiled.expect("compiled");
        assert!(compiled.requirements.plugins.contains("bcode.child"));
        assert_eq!(
            compiled.effects.maximum_capability,
            WorkflowToolCapability::Mutating
        );
        assert!(
            compiled
                .effects
                .resources
                .contains(&ResourceClaim::write("repository"))
        );
        assert_eq!(compiled.permissions.explicit_grant_nodes, ["agent/commit"]);
        assert_eq!(
            compiled.permissions.mutation_approval_nodes,
            ["agent/commit"]
        );
    }

    #[test]
    fn workflow_call_preview_rejects_an_unavailable_child() {
        let mut document = authored_document();
        document.bindings.clear();
        document.requirements.agents.clear();
        let mut child = document.definition.clone();
        child.name = "workflow/child".to_string();
        let child_identity =
            WorkflowDefinitionIdentity::for_definition(child.name.clone(), &child).expect("child");
        let call = WorkflowCallConfiguration {
            version: WORKFLOW_CALL_VERSION,
            target: WorkflowCallTarget::Definition {
                identity: child_identity,
            },
        };
        let node = document.definition.nodes.get_mut("agent").expect("agent");
        node.kind = NodeKind::WorkflowCall;
        node.resources.clear();
        node.configuration = serde_json::to_value(&call).expect("call");

        let catalog = authoring_catalog();
        let unavailable = document.compilation_preview(&catalog, None);
        assert!(unavailable.compiled.is_none());
        assert!(unavailable.validation.diagnostics.iter().any(|diagnostic| {
            diagnostic.document_path == "definition.nodes.agent.configuration.target"
                && diagnostic.message.contains("unavailable")
        }));
    }

    #[test]
    fn export_bundle_carries_exact_immutable_dependencies() {
        let mut document = authored_document();
        let child = WorkflowDefinitionIdentity {
            kind: "workflow/child".to_string(),
            definition_id: "workflow/child:sha256".to_string(),
            definition_version: 1,
        };
        document.bindings.clear();
        document.requirements.agents.clear();
        let node = document.definition.nodes.get_mut("agent").expect("agent");
        node.kind = NodeKind::WorkflowCall;
        node.resources.clear();
        node.configuration = serde_json::to_value(WorkflowCallConfiguration {
            version: WORKFLOW_CALL_VERSION,
            target: WorkflowCallTarget::Definition { identity: child },
        })
        .expect("call");
        document.definition.validate().expect("definition");
        let dependencies = workflow_dependency_manifest(&document.definition).expect("manifest");
        let bundle = WorkflowExportBundle {
            version: WORKFLOW_EXPORT_BUNDLE_VERSION,
            revision: WorkflowPortableRevision {
                identity: WorkflowRevisionIdentity {
                    workflow_id: document.workflow_id.clone(),
                    revision: 1,
                },
                source_checksum_sha256: document.source_digest_sha256().expect("source digest"),
                executable_source_checksum_sha256: document
                    .executable_source_digest_sha256()
                    .expect("executable digest"),
                definition_identity: WorkflowDefinitionIdentity::for_definition(
                    document.workflow_id.clone(),
                    &document.definition,
                )
                .expect("definition identity"),
                producer: document.producer.clone(),
                document,
                published_at_ms: 1,
            },
            dependencies,
        };
        bundle.validate().expect("bundle");
        assert_eq!(bundle.dependencies.len(), 1);
        let mut missing = bundle;
        missing.dependencies.clear();
        assert!(missing.validate().is_err());
    }

    #[test]
    fn export_bundle_contains_only_immutable_portable_authoring_facts() {
        let document = authored_document();
        let bundle = WorkflowExportBundle {
            version: WORKFLOW_EXPORT_BUNDLE_VERSION,
            revision: WorkflowPortableRevision {
                identity: WorkflowRevisionIdentity {
                    workflow_id: document.workflow_id.clone(),
                    revision: 1,
                },
                source_checksum_sha256: document.source_digest_sha256().expect("source digest"),
                executable_source_checksum_sha256: document
                    .executable_source_digest_sha256()
                    .expect("executable digest"),
                definition_identity: WorkflowDefinitionIdentity::for_definition(
                    document.workflow_id.clone(),
                    &document.definition,
                )
                .expect("definition identity"),
                producer: document.producer.clone(),
                document,
                published_at_ms: 1,
            },
            dependencies: Vec::new(),
        };
        bundle.validate().expect("bundle");
        let encoded = serde_json::to_value(bundle).expect("bundle JSON");
        let fields = encoded.as_object().expect("bundle object");
        assert_eq!(fields.keys().collect::<Vec<_>>(), ["revision", "version"]);
        let revision = fields["revision"].as_object().expect("revision object");
        assert_eq!(
            revision.keys().cloned().collect::<BTreeSet<_>>(),
            [
                "definition_identity",
                "document",
                "executable_source_checksum_sha256",
                "identity",
                "producer",
                "published_at_ms",
                "source_checksum_sha256",
            ]
            .into_iter()
            .map(ToString::to_string)
            .collect::<BTreeSet<_>>()
        );
        for forbidden in [
            "grant",
            "secret",
            "provider_metadata",
            "receipt",
            "runtime",
            "renderer",
        ] {
            assert!(!revision.contains_key(forbidden));
        }
    }

    #[test]
    fn authored_persistence_rejects_inline_secrets_and_request_scoped_references() {
        let mut document = authored_document();
        document.configuration_defaults = Some(serde_json::json!({
            "api_key": "must-not-persist"
        }));
        let error = document.validate().expect_err("inline secret must fail");
        assert!(error.to_string().contains("inline secret-bearing"));

        let mut reference = authored_document();
        reference.configuration_defaults = Some(serde_json::json!({
            "token": {"backend": "env", "name": "WORKFLOW_TOKEN"}
        }));
        let error = reference
            .validate()
            .expect_err("request-scoped reference must fail");
        assert!(error.to_string().contains("cannot be persisted"));

        let mut node_secret = authored_document();
        node_secret
            .definition
            .nodes
            .get_mut("agent")
            .expect("agent")
            .configuration["client_secret"] = serde_json::json!("must-not-persist");
        assert!(node_secret.validate().is_err());
    }

    #[test]
    #[should_panic(expected = "agent execution target requires an agent node")]
    fn agent_execution_target_rejects_non_agent_leaf() {
        let _ = Step::map("task", |value: u32| Ok(value))
            .agent_execution_target(AgentExecutionTarget::SharedParentSequential);
    }

    #[test]
    fn production_capability_matrix_classifies_every_node_kind() {
        let capabilities = WorkflowProductionCapabilities::current();
        assert_eq!(capabilities.node_kinds.len(), ALL_NODE_KINDS.len());
        assert_eq!(
            capabilities
                .node_kinds
                .keys()
                .copied()
                .collect::<BTreeSet<_>>(),
            ALL_NODE_KINDS.into_iter().collect()
        );
        assert_eq!(
            capabilities.node_support(NodeKind::Task),
            WorkflowCapabilitySupport::InProcessOnly
        );
        for kind in [
            NodeKind::Agent,
            NodeKind::Branch,
            NodeKind::Repeat,
            NodeKind::Parallel,
            NodeKind::PluginBlock,
            NodeKind::Input,
            NodeKind::Approval,
        ] {
            assert_eq!(
                capabilities.node_support(kind),
                WorkflowCapabilitySupport::Supported
            );
        }
        for kind in [NodeKind::Retry, NodeKind::FanOut] {
            assert_eq!(
                capabilities.node_support(kind),
                WorkflowCapabilitySupport::Unsupported
            );
        }
        assert_eq!(capabilities.edge_kinds.len(), ALL_WORKFLOW_EDGE_KINDS.len());
        assert_eq!(
            capabilities
                .edge_kinds
                .keys()
                .copied()
                .collect::<BTreeSet<_>>(),
            ALL_WORKFLOW_EDGE_KINDS.into_iter().collect()
        );
        for kind in [
            WorkflowEdgeKind::Direct,
            WorkflowEdgeKind::Conditional,
            WorkflowEdgeKind::Back,
        ] {
            assert_eq!(
                capabilities.edge_support(kind),
                WorkflowCapabilitySupport::Supported
            );
        }
        assert_eq!(
            capabilities.node_support(NodeKind::WorkflowCall),
            WorkflowCapabilitySupport::Supported
        );
        assert_eq!(
            capabilities.edge_support(WorkflowEdgeKind::Retry),
            WorkflowCapabilitySupport::Unsupported
        );
        assert_eq!(
            capabilities.parallel_join_policies,
            BTreeSet::from([
                ParallelFailurePolicy::WaitAll,
                ParallelFailurePolicy::FailFast,
            ])
        );
        assert_eq!(
            capabilities.automatic_retry,
            WorkflowCapabilitySupport::Unsupported
        );
        assert_eq!(capabilities.fan_out, WorkflowCapabilitySupport::Unsupported);
        assert_eq!(
            capabilities.transforms,
            WorkflowCapabilitySupport::Supported
        );
        assert_eq!(
            capabilities.artifact_references,
            WorkflowCapabilitySupport::Supported
        );
    }

    #[test]
    fn workflow_call_targets_are_exact_versioned_and_fail_closed() {
        let identity = WorkflowDefinitionIdentity {
            kind: "child".to_string(),
            definition_id: "child:sha256".to_string(),
            definition_version: 1,
        };
        let configuration = WorkflowCallConfiguration {
            version: WORKFLOW_CALL_VERSION,
            target: WorkflowCallTarget::AuthoredRevision {
                workflow_id: "child".to_string(),
                revision: 3,
                definition_identity: identity,
                preset: Some(WorkflowCallPreset {
                    preset_id: "strict".to_string(),
                    generation: 2,
                }),
            },
        };
        configuration.validate().expect("exact target");
        let encoded = serde_json::to_value(&configuration).expect("serialize");
        assert_eq!(encoded["target"]["kind"], "authored_revision");
        assert_eq!(encoded["target"]["revision"], 3);
        assert_eq!(encoded["target"]["preset"]["generation"], 2);

        let mut future = encoded.clone();
        future["version"] = serde_json::json!(WORKFLOW_CALL_VERSION + 1);
        let future: WorkflowCallConfiguration = serde_json::from_value(future).expect("decode");
        assert!(future.validate().is_err());

        let mut mutable_lookup = encoded;
        mutable_lookup["target"]
            .as_object_mut()
            .expect("target")
            .remove("revision");
        assert!(serde_json::from_value::<WorkflowCallConfiguration>(mutable_lookup).is_err());
    }

    #[test]
    fn production_admission_rejects_ownerless_and_incomplete_nodes() {
        let schema = ValueSchema::of::<u32>();
        for (kind, configuration, code) in [
            (
                NodeKind::Task,
                serde_json::Value::Null,
                "in_process_only_node",
            ),
            (
                NodeKind::Retry,
                serde_json::json!({"max_attempts": 2}),
                "unsupported_node_kind",
            ),
            (
                NodeKind::FanOut,
                serde_json::json!({
                    "fan_out_version": WORKFLOW_FAN_OUT_RESULT_VERSION,
                    "max_concurrency": 2,
                    "ordering": "input_index_ascending",
                    "member_shape": {"index": "u32", "value": "typed_output"},
                    "body_entries": ["node"],
                    "body_exits": ["node"]
                }),
                "unsupported_node_kind",
            ),
        ] {
            let definition = WorkflowDefinition {
                schema_version: WORKFLOW_DEFINITION_SCHEMA_VERSION,
                name: "unsupported".to_string(),
                input: schema.clone(),
                output: schema.clone(),
                nodes: BTreeMap::from([(
                    "node".to_string(),
                    NodeDefinition {
                        id: "node".to_string(),
                        name: "node".to_string(),
                        kind,
                        dataflow: WorkflowNodeDataflowPolicy::Direct,
                        input: schema.clone(),
                        output: schema.clone(),
                        resources: Vec::new(),
                        configuration,
                    },
                )]),
                entries: vec!["node".to_string()],
                exits: vec!["node".to_string()],
                edges: Vec::new(),
            };
            let admission = definition
                .production_admission(&WorkflowProductionCapabilities::current())
                .expect("structurally valid definition");
            assert!(admission.diagnostics.iter().any(|item| item.code == code));
        }
    }

    #[test]
    fn automatic_retry_policy_is_bounded_effect_aware_and_excludes_terminal_failures() {
        let base = AutomaticRetryEligibility {
            version: WORKFLOW_AUTOMATIC_RETRY_POLICY_VERSION,
            effect: WorkflowBlockEffect::ReadOnly,
            reconciliation: WorkflowBlockReconciliation::ReceiptStatus,
            failure: AutomaticRetryFailureKind::OwnerReportedRetryable,
            attempts_completed: 1,
            max_attempts: 3,
            retry_cap: 2,
        };
        assert_eq!(
            automatic_retry_decision(base),
            AutomaticRetryDecision::Eligible { next_attempt: 2 }
        );
        assert_eq!(
            automatic_retry_decision(AutomaticRetryEligibility {
                effect: WorkflowBlockEffect::Mutating,
                reconciliation: WorkflowBlockReconciliation::RepairRequired,
                ..base
            }),
            AutomaticRetryDecision::Ineligible {
                reason: AutomaticRetryIneligibleReason::UnsafeEffectOrReconciliation
            }
        );
        assert_eq!(
            automatic_retry_decision(AutomaticRetryEligibility {
                attempts_completed: 2,
                ..base
            }),
            AutomaticRetryDecision::Ineligible {
                reason: AutomaticRetryIneligibleReason::AttemptsExhausted
            }
        );
        for (failure, reason) in [
            (
                AutomaticRetryFailureKind::Cancellation,
                AutomaticRetryIneligibleReason::Cancellation,
            ),
            (
                AutomaticRetryFailureKind::TerminalTimeout,
                AutomaticRetryIneligibleReason::TerminalTimeout,
            ),
            (
                AutomaticRetryFailureKind::ApprovalDenied,
                AutomaticRetryIneligibleReason::ApprovalDenied,
            ),
            (
                AutomaticRetryFailureKind::SchemaFailure,
                AutomaticRetryIneligibleReason::SchemaFailure,
            ),
            (
                AutomaticRetryFailureKind::AmbiguousMutation,
                AutomaticRetryIneligibleReason::AmbiguousMutation,
            ),
        ] {
            assert_eq!(
                automatic_retry_decision(AutomaticRetryEligibility { failure, ..base }),
                AutomaticRetryDecision::Ineligible { reason }
            );
        }
        assert_eq!(
            WorkflowProductionCapabilities::current().automatic_retry,
            WorkflowCapabilitySupport::Unsupported
        );
        assert_eq!(
            WorkflowProductionCapabilities::current().automatic_retry_policy_version,
            None
        );
    }

    #[test]
    fn explicit_operator_retry_is_distinct_from_automatic_and_repair_retry() {
        let capabilities = WorkflowProductionCapabilities::current();
        assert_eq!(
            capabilities.node_support(NodeKind::Retry),
            WorkflowCapabilitySupport::Unsupported
        );
        assert_eq!(
            capabilities.edge_support(WorkflowEdgeKind::Retry),
            WorkflowCapabilitySupport::Unsupported
        );
        assert_eq!(
            capabilities.automatic_retry,
            WorkflowCapabilitySupport::Unsupported
        );
    }

    #[test]
    fn production_admission_rejects_retry_edges() {
        let schema = ValueSchema::of::<u32>();
        let node = |id: &str| NodeDefinition {
            id: id.to_string(),
            name: id.to_string(),
            kind: NodeKind::Input,
            dataflow: WorkflowNodeDataflowPolicy::Direct,
            input: schema.clone(),
            output: schema.clone(),
            resources: Vec::new(),
            configuration: serde_json::json!({"gate_version": 1}),
        };
        let definition = WorkflowDefinition {
            schema_version: WORKFLOW_DEFINITION_SCHEMA_VERSION,
            name: "retry-edge".to_string(),
            input: schema.clone(),
            output: schema.clone(),
            nodes: BTreeMap::from([
                ("first".to_string(), node("first")),
                ("second".to_string(), node("second")),
            ]),
            entries: vec!["first".to_string()],
            exits: vec!["second".to_string()],
            edges: vec![EdgeDefinition {
                from: "first".to_string(),
                to: "second".to_string(),
                kind: EdgeKind::Retry { max_attempts: 2 },
                transform: None,
            }],
        };
        let admission = definition
            .production_admission(&WorkflowProductionCapabilities::current())
            .expect("structurally valid definition");
        assert!(admission.diagnostics.iter().any(|item| {
            item.code == "unsupported_edge_kind" && item.node_id.as_deref() == Some("first")
        }));
        assert!(
            admission
                .diagnostics
                .iter()
                .any(|item| item.code == "unsupported_retry_edge")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn declarative_transforms_cover_projection_construction_merge_defaults_and_bounds() {
        let state = serde_json::json!({
            "git": {"expected_head": "abc", "paths": ["src/lib.rs"]},
            "message": null
        });
        let generated = serde_json::json!({"message": "Implement workflow transforms"});
        let transform = WorkflowTransform {
            version: WORKFLOW_TRANSFORM_VERSION,
            expression: WorkflowTransformExpression::Object {
                fields: BTreeMap::from([
                    (
                        "expected_head".to_string(),
                        WorkflowTransformExpression::Input {
                            source: "state".to_string(),
                            path: "git.expected_head".to_string(),
                        },
                    ),
                    (
                        "message".to_string(),
                        WorkflowTransformExpression::Default {
                            value: Box::new(WorkflowTransformExpression::Input {
                                source: "state".to_string(),
                                path: "message".to_string(),
                            }),
                            default: Box::new(WorkflowTransformExpression::Input {
                                source: "generated".to_string(),
                                path: "message".to_string(),
                            }),
                        },
                    ),
                    (
                        "paths".to_string(),
                        WorkflowTransformExpression::Input {
                            source: "state".to_string(),
                            path: "git.paths".to_string(),
                        },
                    ),
                    (
                        "repo_path".to_string(),
                        WorkflowTransformExpression::Constant {
                            value: serde_json::json!("."),
                        },
                    ),
                ]),
            },
            output: ValueSchema {
                type_name: "bcode.git.commit-request/v1".to_string(),
                schema: serde_json::json!({
                    "type": "object",
                    "required": ["repo_path", "expected_head", "message", "paths"],
                    "properties": {
                        "repo_path": {"type": "string"},
                        "expected_head": {"type": "string"},
                        "message": {"type": "string"},
                        "paths": {"type": "array", "items": {"type": "string"}}
                    }
                }),
            },
        };
        assert_eq!(
            transform
                .evaluate(&[
                    WorkflowTransformInput {
                        name: "state",
                        value: &state,
                    },
                    WorkflowTransformInput {
                        name: "generated",
                        value: &generated,
                    },
                ])
                .expect("transform"),
            serde_json::json!({
                "repo_path": ".",
                "expected_head": "abc",
                "message": "Implement workflow transforms",
                "paths": ["src/lib.rs"]
            })
        );

        let merge = WorkflowTransform {
            version: WORKFLOW_TRANSFORM_VERSION,
            expression: WorkflowTransformExpression::Merge {
                objects: vec![
                    WorkflowTransformExpression::Constant {
                        value: serde_json::json!({"a": 1, "same": "first"}),
                    },
                    WorkflowTransformExpression::Constant {
                        value: serde_json::json!({"b": 2, "same": "last"}),
                    },
                ],
                conflict: TransformMergeConflict::KeepLast,
            },
            output: ValueSchema {
                type_name: "object".to_string(),
                schema: serde_json::json!({"type": "object"}),
            },
        };
        assert_eq!(
            merge.evaluate(&[]).expect("merge"),
            serde_json::json!({"a": 1, "b": 2, "same": "last"})
        );
        let array = WorkflowTransform {
            version: WORKFLOW_TRANSFORM_VERSION,
            expression: WorkflowTransformExpression::Array {
                items: vec![
                    WorkflowTransformExpression::Constant {
                        value: serde_json::json!(1),
                    },
                    WorkflowTransformExpression::Constant {
                        value: serde_json::json!(2),
                    },
                ],
            },
            output: ValueSchema {
                type_name: "array".to_string(),
                schema: serde_json::json!({"type": "array", "items": {"type": "integer"}}),
            },
        };
        assert_eq!(
            array.evaluate(&[]).expect("array"),
            serde_json::json!([1, 2])
        );

        let merge_objects = match &merge.expression {
            WorkflowTransformExpression::Merge { objects, .. } => objects.clone(),
            _ => unreachable!(),
        };
        let keep_first = WorkflowTransform {
            version: WORKFLOW_TRANSFORM_VERSION,
            expression: WorkflowTransformExpression::Merge {
                objects: merge_objects.clone(),
                conflict: TransformMergeConflict::KeepFirst,
            },
            output: merge.output.clone(),
        };
        assert_eq!(
            keep_first.evaluate(&[]).expect("keep first")["same"],
            "first"
        );
        let reject = WorkflowTransform {
            version: WORKFLOW_TRANSFORM_VERSION,
            expression: WorkflowTransformExpression::Merge {
                objects: merge_objects,
                conflict: TransformMergeConflict::Reject,
            },
            output: merge.output,
        };
        assert!(reject.evaluate(&[]).is_err());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn declarative_transform_rejects_unknown_sources_versions_depth_and_output_mismatch() {
        let output = ValueSchema::of::<u32>();
        let unknown = WorkflowTransform {
            version: WORKFLOW_TRANSFORM_VERSION,
            expression: WorkflowTransformExpression::Input {
                source: "missing".to_string(),
                path: String::new(),
            },
            output: output.clone(),
        };
        assert!(unknown.evaluate(&[]).is_err());
        let duplicate = serde_json::json!(1);
        assert!(
            unknown
                .evaluate(&[
                    WorkflowTransformInput {
                        name: "same",
                        value: &duplicate,
                    },
                    WorkflowTransformInput {
                        name: "same",
                        value: &duplicate,
                    },
                ])
                .is_err()
        );
        let unsupported = WorkflowTransform {
            version: 2,
            expression: unknown.expression.clone(),
            output: unknown.output,
        };
        assert!(unsupported.validate().is_err());
        assert!(
            WorkflowTransform {
                version: WORKFLOW_TRANSFORM_VERSION,
                expression: WorkflowTransformExpression::Constant {
                    value: serde_json::json!("wrong")
                },
                output,
            }
            .evaluate(&[])
            .is_err()
        );

        let oversized_constant = WorkflowTransform {
            version: WORKFLOW_TRANSFORM_VERSION,
            expression: WorkflowTransformExpression::Constant {
                value: serde_json::json!("x".repeat(MAX_TRANSFORM_VALUE_BYTES)),
            },
            output: ValueSchema::of::<String>(),
        };
        assert!(oversized_constant.validate().is_err());

        let too_many_fields = WorkflowTransform {
            version: WORKFLOW_TRANSFORM_VERSION,
            expression: WorkflowTransformExpression::Object {
                fields: (0..=MAX_TRANSFORM_FIELDS)
                    .map(|index| {
                        (
                            format!("field-{index}"),
                            WorkflowTransformExpression::Constant {
                                value: serde_json::Value::Null,
                            },
                        )
                    })
                    .collect(),
            },
            output: ValueSchema {
                type_name: "object".to_string(),
                schema: serde_json::json!({"type": "object"}),
            },
        };
        assert!(too_many_fields.validate().is_err());

        let too_many_items = WorkflowTransform {
            version: WORKFLOW_TRANSFORM_VERSION,
            expression: WorkflowTransformExpression::Array {
                items: (0..=MAX_TRANSFORM_FIELDS)
                    .map(|_| WorkflowTransformExpression::Constant {
                        value: serde_json::Value::Null,
                    })
                    .collect(),
            },
            output: ValueSchema {
                type_name: "array".to_string(),
                schema: serde_json::json!({"type": "array"}),
            },
        };
        assert!(too_many_items.validate().is_err());

        let too_many_operations = WorkflowTransform {
            version: WORKFLOW_TRANSFORM_VERSION,
            expression: WorkflowTransformExpression::Object {
                fields: (0..MAX_TRANSFORM_FIELDS)
                    .map(|index| {
                        (
                            format!("field-{index}"),
                            WorkflowTransformExpression::Default {
                                value: Box::new(WorkflowTransformExpression::Constant {
                                    value: serde_json::Value::Null,
                                }),
                                default: Box::new(WorkflowTransformExpression::Constant {
                                    value: serde_json::Value::Null,
                                }),
                            },
                        )
                    })
                    .collect(),
            },
            output: ValueSchema {
                type_name: "object".to_string(),
                schema: serde_json::json!({"type": "object"}),
            },
        };
        assert!(too_many_operations.validate().is_err());

        let overflow = WorkflowTransform {
            version: WORKFLOW_TRANSFORM_VERSION,
            expression: WorkflowTransformExpression::Increment {
                value: Box::new(WorkflowTransformExpression::Constant {
                    value: serde_json::json!(i64::MAX),
                }),
                by: 1,
            },
            output: ValueSchema::of::<i64>(),
        };
        assert!(overflow.evaluate(&[]).is_err());

        let mut nested_json = serde_json::Value::Null;
        for _ in 0..=MAX_TRANSFORM_DEPTH {
            nested_json = serde_json::Value::Array(vec![nested_json]);
        }
        assert!(
            WorkflowTransform {
                version: WORKFLOW_TRANSFORM_VERSION,
                expression: WorkflowTransformExpression::Constant { value: nested_json },
                output: ValueSchema {
                    type_name: "array".to_string(),
                    schema: serde_json::json!({"type": "array"}),
                },
            }
            .validate()
            .is_err()
        );

        let mut expression = WorkflowTransformExpression::Constant {
            value: serde_json::Value::Null,
        };
        for _ in 0..=MAX_TRANSFORM_DEPTH {
            expression = WorkflowTransformExpression::Array {
                items: vec![expression],
            };
        }
        assert!(
            WorkflowTransform {
                version: WORKFLOW_TRANSFORM_VERSION,
                expression,
                output: ValueSchema {
                    type_name: "array".to_string(),
                    schema: serde_json::json!({"type": "array"}),
                },
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn production_admission_rejects_incompatible_direct_edge_schemas() {
        let number = ValueSchema::of::<u32>();
        let text = ValueSchema::of::<String>();
        let definition = WorkflowDefinition {
            schema_version: WORKFLOW_DEFINITION_SCHEMA_VERSION,
            name: "incompatible-edge".to_string(),
            input: number.clone(),
            output: text.clone(),
            nodes: BTreeMap::from([
                (
                    "source".to_string(),
                    NodeDefinition {
                        id: "source".to_string(),
                        name: "source".to_string(),
                        kind: NodeKind::Input,
                        dataflow: WorkflowNodeDataflowPolicy::Direct,
                        input: number.clone(),
                        output: number,
                        resources: Vec::new(),
                        configuration: serde_json::json!({"gate_version": 1}),
                    },
                ),
                (
                    "target".to_string(),
                    NodeDefinition {
                        id: "target".to_string(),
                        name: "target".to_string(),
                        kind: NodeKind::Input,
                        dataflow: WorkflowNodeDataflowPolicy::Direct,
                        input: text.clone(),
                        output: text,
                        resources: Vec::new(),
                        configuration: serde_json::json!({"gate_version": 1}),
                    },
                ),
            ]),
            entries: vec!["source".to_string()],
            exits: vec!["target".to_string()],
            edges: vec![EdgeDefinition {
                from: "source".to_string(),
                to: "target".to_string(),
                kind: EdgeKind::Direct,
                transform: None,
            }],
        };
        let admission = definition
            .production_admission(&WorkflowProductionCapabilities::current())
            .expect("structurally valid definition");
        assert!(admission.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "incompatible_edge_schema"
                && diagnostic.node_id.as_deref() == Some("source")
        }));
    }

    #[test]
    fn production_admission_rejects_ambiguous_parallel_join_members() {
        let left = Step::task("left", |value: u32, _context| async move { Ok(value) });
        let right = Step::task("right", |value: u32, _context| async move { Ok(value) });
        let mut definition = WorkflowBuilder::new("parallel", parallel_named("join", left, right))
            .build()
            .expect("workflow")
            .definition()
            .clone();
        definition
            .nodes
            .get_mut("join")
            .expect("join")
            .configuration["right_exits"] = serde_json::json!(["left"]);
        let admission = definition
            .production_admission(&WorkflowProductionCapabilities::current())
            .expect("admission");
        assert!(admission.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == "invalid_parallel_join_configuration"
                && diagnostic.node_id.as_deref() == Some("join")
        }));
    }

    #[test]
    fn compiled_definition_rejects_excessive_topology() {
        let schema = ValueSchema::of::<u32>();
        let node = NodeDefinition {
            id: "node".to_string(),
            name: "node".to_string(),
            kind: NodeKind::Input,
            dataflow: WorkflowNodeDataflowPolicy::Direct,
            input: schema.clone(),
            output: schema.clone(),
            resources: Vec::new(),
            configuration: serde_json::json!({"version": 1}),
        };
        let definition = WorkflowDefinition {
            schema_version: WORKFLOW_DEFINITION_SCHEMA_VERSION,
            name: "oversized-topology".to_string(),
            input: schema.clone(),
            output: schema,
            nodes: BTreeMap::from([("node".to_string(), node)]),
            entries: vec!["node".to_string(); MAX_DEFINITION_BOUNDARIES + 1],
            exits: vec!["node".to_string()],
            edges: Vec::new(),
        };
        assert!(definition.validate().is_err());
    }

    #[test]
    fn production_admission_rejects_unsupported_agent_and_parallel_options() {
        let schema = ValueSchema::of::<u32>();
        let definition = WorkflowDefinition {
            schema_version: WORKFLOW_DEFINITION_SCHEMA_VERSION,
            name: "unsupported-options".to_string(),
            input: schema.clone(),
            output: ValueSchema::of::<(u32, u32)>(),
            nodes: BTreeMap::from([
                (
                    "agent".to_string(),
                    NodeDefinition {
                        id: "agent".to_string(),
                        name: "agent".to_string(),
                        kind: NodeKind::Agent,
                        dataflow: WorkflowNodeDataflowPolicy::Direct,
                        input: schema.clone(),
                        output: schema.clone(),
                        resources: Vec::new(),
                        configuration: serde_json::json!({
                            "configuration_version": 2,
                            "prompt_mode": "custom"
                        }),
                    },
                ),
                (
                    "parallel".to_string(),
                    NodeDefinition {
                        id: "parallel".to_string(),
                        name: "parallel".to_string(),
                        kind: NodeKind::Parallel,
                        dataflow: WorkflowNodeDataflowPolicy::Direct,
                        input: schema,
                        output: ValueSchema::of::<(u32, u32)>(),
                        resources: Vec::new(),
                        configuration: serde_json::json!({
                            "failure_policy": "fail_fast",
                            "left_exits": ["agent"],
                            "right_exits": ["agent"]
                        }),
                    },
                ),
            ]),
            entries: vec!["agent".to_string()],
            exits: vec!["parallel".to_string()],
            edges: vec![EdgeDefinition {
                from: "agent".to_string(),
                to: "parallel".to_string(),
                kind: EdgeKind::Direct,
                transform: None,
            }],
        };
        let mut capabilities = WorkflowProductionCapabilities::current();
        capabilities
            .parallel_join_policies
            .remove(&ParallelFailurePolicy::FailFast);
        let admission = definition
            .production_admission(&capabilities)
            .expect("structurally valid definition");
        assert!(admission.diagnostics.iter().any(|item| {
            item.code == "unsupported_agent_configuration_version"
                && item.node_id.as_deref() == Some("agent")
        }));
        assert!(admission.diagnostics.iter().any(|item| {
            item.code == "unsupported_agent_prompt_mode" && item.node_id.as_deref() == Some("agent")
        }));
        assert!(admission.diagnostics.iter().any(|item| {
            item.code == "invalid_parallel_join_schema"
                && item.node_id.as_deref() == Some("parallel")
        }));
        assert!(admission.diagnostics.iter().any(|item| {
            item.code == "unsupported_parallel_policy"
                && item.node_id.as_deref() == Some("parallel")
        }));
    }

    #[test]
    fn versioned_agent_configuration_validates_skills_identity_and_read_only_policy() {
        let contract = WorkflowAgentConfiguration {
            version: WORKFLOW_AGENT_CONFIGURATION_VERSION,
            execution_target: AgentExecutionTarget::FreshIsolated,
            agent_profile: "build".to_string(),
            provider: Some("configured".to_string()),
            model: Some("configured".to_string()),
            structured_output: AgentStructuredOutputPolicy {
                schema: ValueSchema::of::<serde_json::Value>(),
                strict: true,
            },
            read_only: true,
            tool_capability: WorkflowToolCapability::ReadOnly,
            tool_allowlist: vec!["filesystem.read".to_string()],
            timeout_ms: 30_000,
            skills: vec![
                AgentSkillSelection {
                    skill_id: "commit-message".to_string(),
                    mode: AgentSkillActivationMode::Required,
                },
                AgentSkillSelection {
                    skill_id: "style-guide".to_string(),
                    mode: AgentSkillActivationMode::Preferred,
                },
                AgentSkillSelection {
                    skill_id: "legacy".to_string(),
                    mode: AgentSkillActivationMode::Disabled,
                },
            ],
            prompt_mode: "json_input".to_string(),
            system_prompt: "Return structured output.".to_string(),
        };
        contract.validate().expect("valid contract");
        let encoded = serde_json::to_value(&contract).expect("serialize");
        assert_eq!(encoded["skills"][0]["skill_id"], "commit-message");
        assert!(
            serde_json::from_value::<WorkflowAgentConfiguration>(serde_json::json!({
                "version": 1,
                "execution_target": "fresh_isolated",
                "agent_profile": "build",
                "provider": null,
                "model": null,
                "structured_output": {"schema": {"type": "object"}, "strict": true},
                "read_only": true,
                "tool_capability": "mutating",
                "tool_allowlist": [],
                "timeout_ms": 30000,
                "skills": [],
                "prompt_mode": "json_input",
                "system_prompt": "test",
                "unknown": true
            }))
            .is_err()
        );
        let mut escalated = contract;
        escalated.tool_capability = WorkflowToolCapability::Mutating;
        assert!(escalated.validate().is_err());
    }

    #[test]
    fn agent_execution_target_serializes_explicitly() {
        let step = Step::configured_task(
            "agent",
            NodeKind::Agent,
            serde_json::json!({"prompt_mode": "json_input"}),
            |value: u32, _context| async move { Ok(value) },
        )
        .agent_execution_target(AgentExecutionTarget::SharedParentSequential);
        let workflow = WorkflowBuilder::new("target", step)
            .build()
            .expect("workflow");
        assert_eq!(
            workflow.definition().nodes["agent"].configuration["execution_target"],
            "shared_parent_sequential"
        );
    }

    #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
    struct Input {
        value: u64,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
    struct Doubled {
        value: u64,
    }

    #[test]
    fn durable_spec_identity_is_stable_and_topology_sensitive() {
        let compile = |max_iterations| {
            WorkflowBuilder::new(
                "loop",
                Step::map("work", |input: Input| Ok(input)).repeat_while(
                    "repeat",
                    field::<Input>("value").eq(0_u64),
                    max_iterations,
                ),
            )
            .build()
            .expect("workflow")
        };
        let first = WorkflowSpec::new("bcode.loop", &compile(2)).expect("spec");
        let same = WorkflowSpec::new("bcode.loop", &compile(2)).expect("same spec");
        let variant = WorkflowSpec::new("bcode.loop", &compile(3)).expect("variant");

        assert_eq!(first.identity(), same.identity());
        assert_ne!(
            first.identity().definition_id,
            variant.identity().definition_id
        );
        assert_eq!(first.identity().kind, "bcode.loop");
        assert_eq!(
            first.identity().definition_version,
            WORKFLOW_DEFINITION_SCHEMA_VERSION
        );
        assert_eq!(
            first.serialize_input(&Input { value: 1 }).expect("input"),
            serde_json::json!({"value": 1})
        );
    }

    #[test]
    fn durable_spec_rejects_mismatched_typed_input() {
        let workflow = WorkflowBuilder::new("typed", Step::map("work", |input: Input| Ok(input)))
            .build()
            .expect("workflow");
        let error =
            WorkflowSpec::<Doubled>::from_definition("bcode.typed", workflow.definition().clone())
                .expect_err("input mismatch");
        assert!(error.to_string().contains("input schema does not match"));
    }

    #[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
    enum BunScenario {
        Normal,
        ReviewerFailure,
        ReviewerDisagreement,
        CancelDuringReview,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
    struct BunWorkState {
        fix_round: u32,
        scenario: BunScenario,
        accepted: bool,
        report: ArtifactReference,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
    struct BunReview {
        reviewer: String,
        accepted: bool,
        report: ArtifactReference,
        state: BunWorkState,
    }

    fn bun_reference_workflow(
        reviews_started: Option<&Arc<AtomicUsize>>,
    ) -> Workflow<BunWorkState, BunWorkState> {
        let reviews_started = reviews_started.map(Arc::clone);
        let implement = Step::map("implement", |state: BunWorkState| Ok(state));
        let reviewer = |name: &'static str, left: bool| {
            let reviews_started = reviews_started.clone();
            Step::task(name, move |state: BunWorkState, context| {
                let reviews_started = reviews_started.clone();
                async move {
                    if let Some(started) = reviews_started {
                        started.fetch_add(1, Ordering::SeqCst);
                    }
                    if state.scenario == BunScenario::CancelDuringReview {
                        context.cancellation().cancel();
                        context.ensure_active(name)?;
                    }
                    if left && state.scenario == BunScenario::ReviewerFailure {
                        return Err(WorkflowError::step(name, "reviewer failed"));
                    }
                    let accepted = if state.scenario == BunScenario::ReviewerDisagreement {
                        left
                    } else {
                        state.fix_round > 0
                    };
                    Ok(BunReview {
                        reviewer: name.to_string(),
                        accepted,
                        report: ArtifactReference::new(
                            format!("{name}-report-{}", state.fix_round),
                            "bcode.review.report",
                            1,
                            "application/json",
                            format!("{name}.json"),
                        ),
                        state,
                    })
                }
            })
        };
        let review = parallel_named_with_policy(
            "parallel-review",
            ParallelFailurePolicy::WaitAll,
            reviewer("reviewer-a", true),
            reviewer("reviewer-b", false),
        );
        let adjudicate = Step::map("adjudicate", |(left, right): (BunReview, BunReview)| {
            let mut state = left.state;
            state.accepted = left.accepted && right.accepted;
            state.report = left.report;
            Ok(state)
        });
        let fix = Step::map("fix", |mut state: BunWorkState| {
            if !state.accepted {
                state.fix_round = state.fix_round.saturating_add(1);
                state.scenario = BunScenario::Normal;
            }
            Ok(state)
        });
        let cycle = review.then(adjudicate).then(fix).repeat_while(
            "bounded-fix-review",
            field::<BunWorkState>("accepted").eq(false),
            2,
        );
        WorkflowBuilder::new("bun-reference", implement.then(cycle))
            .build()
            .expect("reference workflow")
    }

    fn bun_state(scenario: BunScenario) -> BunWorkState {
        BunWorkState {
            fix_round: 0,
            scenario,
            accepted: false,
            report: ArtifactReference::new(
                "implementation-report",
                "bcode.implementation.report",
                1,
                "application/json",
                "implementation.json",
            ),
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
    struct Labelled {
        label: String,
    }

    #[tokio::test]
    async fn bun_reference_workflow_covers_parallel_review_adjudication_fix_and_failures() {
        let completed = bun_reference_workflow(None)
            .run(bun_state(BunScenario::Normal))
            .await
            .expect("fix then accept");
        assert!(completed.accepted);
        assert_eq!(completed.fix_round, 1);
        assert_eq!(completed.report.schema, "bcode.review.report");

        let disagreed = bun_reference_workflow(None)
            .run(bun_state(BunScenario::ReviewerDisagreement))
            .await
            .expect("disagreement is adjudicated and fixed");
        assert!(disagreed.accepted);
        assert_eq!(disagreed.fix_round, 1);

        let failure = bun_reference_workflow(None)
            .run(bun_state(BunScenario::ReviewerFailure))
            .await
            .expect_err("reviewer failure");
        assert!(failure.to_string().contains("reviewer-a"));

        let exhaustion = WorkflowBuilder::new(
            "bun-exhausted",
            Step::map("never-accepted", |mut state: BunWorkState| {
                state.fix_round = state.fix_round.saturating_add(1);
                Ok(state)
            })
            .repeat_while(
                "bounded-exhaustion",
                field::<BunWorkState>("accepted").eq(false),
                2,
            ),
        )
        .build()
        .expect("exhaustion workflow")
        .run(bun_state(BunScenario::Normal))
        .await
        .expect_err("bounded fixes exhaust");
        assert!(
            exhaustion
                .to_string()
                .contains("remained true after 2 iterations")
        );

        let reviews_started = Arc::new(AtomicUsize::new(0));
        let cancellation = bun_reference_workflow(Some(&reviews_started))
            .run(bun_state(BunScenario::CancelDuringReview))
            .await
            .expect_err("cancel during parallel review");
        assert!(matches!(cancellation, WorkflowError::Cancelled { .. }));
        assert!(reviews_started.load(Ordering::SeqCst) >= 1);
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
    fn state_envelope_dataflow_validates_and_isolates_owner_input() {
        let owner_input = ValueSchema {
            type_name: "owner.input/v1".to_string(),
            schema: serde_json::json!({
                "type": "object",
                "required": ["command"],
                "properties": {"command": {"type": "string"}},
                "additionalProperties": false
            }),
        };
        let owner_output = ValueSchema {
            type_name: "owner.output/v1".to_string(),
            schema: serde_json::json!({"type": "boolean"}),
        };
        let envelope_schema = ValueSchema {
            type_name: "state-envelope/v1".to_string(),
            schema: serde_json::json!({
                "type": "object",
                "required": ["schema_version", "state", "value"],
                "properties": {
                    "schema_version": {"const": WORKFLOW_STATE_ENVELOPE_VERSION},
                    "state": {"type": "object"},
                    "value": {},
                    "artifacts": {"type": "array"}
                },
                "additionalProperties": false
            }),
        };
        let input = serde_json::json!({
            "schema_version": WORKFLOW_STATE_ENVELOPE_VERSION,
            "state": {"secret_looking_but_not_authority": "retained"},
            "value": {"command": "check"},
            "artifacts": []
        });
        let prepared = prepare_workflow_node_dataflow(
            WorkflowNodeDataflowPolicy::StateEnvelopeV1,
            &envelope_schema,
            &owner_input,
            &input,
        )
        .expect("prepared");
        assert_eq!(
            prepared.owner_input(),
            &serde_json::json!({"command": "check"})
        );
        let completed = complete_workflow_node_dataflow(
            prepared,
            &owner_output,
            &envelope_schema,
            serde_json::json!(true),
        )
        .expect("completed");
        assert_eq!(completed["state"], input["state"]);
        assert_eq!(completed["value"], serde_json::json!(true));

        let future = serde_json::json!({
            "schema_version": WORKFLOW_STATE_ENVELOPE_VERSION + 1,
            "state": {},
            "value": {"command": "check"},
            "artifacts": []
        });
        assert!(
            prepare_workflow_node_dataflow(
                WorkflowNodeDataflowPolicy::StateEnvelopeV1,
                &envelope_schema,
                &owner_input,
                &future,
            )
            .is_err()
        );

        let oversized = serde_json::json!({
            "schema_version": WORKFLOW_STATE_ENVELOPE_VERSION,
            "state": {"value": "x".repeat(MAX_WORKFLOW_STATE_ENVELOPE_STATE_BYTES)},
            "value": {"command": "check"},
            "artifacts": []
        });
        let permissive_envelope = ValueSchema {
            type_name: "state-envelope/permissive".to_string(),
            schema: serde_json::json!({"type": "object"}),
        };
        assert!(
            prepare_workflow_node_dataflow(
                WorkflowNodeDataflowPolicy::StateEnvelopeV1,
                &permissive_envelope,
                &owner_input,
                &oversized,
            )
            .is_err()
        );
    }

    #[test]
    fn durable_predicates_support_bounded_versioned_composition() {
        let input = serde_json::json!({
            "current": 4,
            "limit": 5,
            "status": "ready",
            "expected_status": "ready"
        });
        let predicate = PredicateExpression::All {
            version: WORKFLOW_PREDICATE_VERSION,
            predicates: vec![
                PredicateExpression::NumericCompare {
                    version: WORKFLOW_PREDICATE_VERSION,
                    left_path: "current".to_string(),
                    right_path: "limit".to_string(),
                    comparison: PredicateNumericComparison::LessThan,
                },
                PredicateExpression::FieldsEqual {
                    version: WORKFLOW_PREDICATE_VERSION,
                    left_path: "status".to_string(),
                    right_path: "expected_status".to_string(),
                },
                PredicateExpression::Not {
                    version: WORKFLOW_PREDICATE_VERSION,
                    predicate: Box::new(PredicateExpression::Equals {
                        version: WORKFLOW_PREDICATE_VERSION,
                        path: "status".to_string(),
                        value: serde_json::json!("failed"),
                    }),
                },
            ],
        };
        assert!(predicate.evaluate_value(&input).expect("predicate"));

        let version_one = PredicateExpression::Equals {
            version: WORKFLOW_PREDICATE_MIN_VERSION,
            path: "status".to_string(),
            value: serde_json::json!("ready"),
        };
        assert!(version_one.evaluate_value(&input).expect("version one"));

        let mixed_versions = PredicateExpression::All {
            version: WORKFLOW_PREDICATE_VERSION,
            predicates: vec![version_one],
        };
        assert!(mixed_versions.evaluate_value(&input).is_err());

        let mut too_deep = PredicateExpression::Equals {
            version: WORKFLOW_PREDICATE_VERSION,
            path: "status".to_string(),
            value: serde_json::json!("ready"),
        };
        for _ in 0..MAX_PREDICATE_DEPTH {
            too_deep = PredicateExpression::Not {
                version: WORKFLOW_PREDICATE_VERSION,
                predicate: Box::new(too_deep),
            };
        }
        assert!(too_deep.evaluate_value(&input).is_err());
    }

    #[test]
    fn durable_predicates_require_a_supported_version_and_bounds() {
        let workflow = WorkflowBuilder::new(
            "branch",
            Step::map("source", |value: u32| Ok(value)).branch(
                "choose",
                field::<u32>("").eq(1_u32),
                Step::map("selected", |value: u32| Ok(value)),
                Step::map("other", |value: u32| Ok(value)),
            ),
        )
        .build()
        .expect("workflow");
        let definition = workflow.definition();
        assert_eq!(
            definition.nodes["choose"].configuration["predicate_version"],
            WORKFLOW_PREDICATE_VERSION
        );
        assert!(
            definition
                .edges
                .iter()
                .filter_map(|edge| match &edge.kind {
                    EdgeKind::Conditional { predicate, .. } => Some(predicate),
                    _ => None,
                })
                .all(|predicate| matches!(
                    predicate,
                    PredicateExpression::Equals { version, .. }
                        if *version == WORKFLOW_PREDICATE_VERSION
                ))
        );

        let mut compatible = definition.clone();
        compatible
            .nodes
            .get_mut("choose")
            .expect("choose")
            .configuration["predicate_version"] = serde_json::json!(WORKFLOW_PREDICATE_MIN_VERSION);
        compatible
            .nodes
            .get_mut("choose")
            .expect("choose")
            .configuration["predicate"]["version"] =
            serde_json::json!(WORKFLOW_PREDICATE_MIN_VERSION);
        for edge in &mut compatible.edges {
            if let EdgeKind::Conditional { predicate, .. } = &mut edge.kind {
                *predicate = PredicateExpression::Equals {
                    version: WORKFLOW_PREDICATE_MIN_VERSION,
                    path: String::new(),
                    value: serde_json::json!(1),
                };
            }
        }
        compatible.validate().expect("version one remains valid");

        let mut unsupported = definition.clone();
        unsupported
            .nodes
            .get_mut("choose")
            .expect("choose")
            .configuration["predicate_version"] = serde_json::json!(WORKFLOW_PREDICATE_VERSION + 1);
        assert!(unsupported.validate().is_err());

        let mut unbounded = definition.clone();
        for edge in &mut unbounded.edges {
            if let EdgeKind::Conditional { predicate, .. } = &mut edge.kind {
                *predicate = PredicateExpression::Equals {
                    version: WORKFLOW_PREDICATE_VERSION,
                    path: "x".repeat(513),
                    value: serde_json::Value::Null,
                };
                break;
            }
        }
        assert!(unbounded.validate().is_err());
    }

    #[test]
    fn deserialized_definition_validation_rejects_invalid_structure() {
        let workflow =
            WorkflowBuilder::new("validated", Step::map("first", |value: u32| Ok(value)))
                .build()
                .expect("workflow");
        let mut definition = workflow.definition().clone();
        definition.entries = vec!["missing".to_string()];
        let error = definition.validate().expect_err("invalid entry");
        assert!(error.to_string().contains("unknown step 'missing'"));

        let mut definition = workflow.definition().clone();
        definition.schema_version = WORKFLOW_DEFINITION_SCHEMA_VERSION.saturating_add(1);
        let error = definition.validate().expect_err("unknown schema version");
        assert!(
            error
                .to_string()
                .contains("unsupported workflow definition schema version")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn exact_git_commit_request_is_built_from_retained_state_git_metadata_and_message() {
        let retained = serde_json::to_value(
            WorkflowStateEnvelope::new(
                serde_json::json!({
                    "git": {
                        "repo_path": ".",
                        "expected_head": "abc123",
                        "paths": ["src/lib.rs", "Cargo.toml"]
                    }
                }),
                serde_json::json!({"message": "Implement durable state envelope"}),
            )
            .with_artifacts(vec![ArtifactReference::new(
                "verification-1",
                "bcode.verification-result",
                1,
                "application/json",
                "verification/result.json",
            )]),
        )
        .expect("envelope");
        let transform = WorkflowTransform {
            version: WORKFLOW_TRANSFORM_VERSION,
            expression: WorkflowTransformExpression::Object {
                fields: BTreeMap::from([
                    (
                        "repo_path".to_string(),
                        WorkflowTransformExpression::Input {
                            source: "retained".to_string(),
                            path: "state.git.repo_path".to_string(),
                        },
                    ),
                    (
                        "expected_head".to_string(),
                        WorkflowTransformExpression::Input {
                            source: "retained".to_string(),
                            path: "state.git.expected_head".to_string(),
                        },
                    ),
                    (
                        "paths".to_string(),
                        WorkflowTransformExpression::Input {
                            source: "retained".to_string(),
                            path: "state.git.paths".to_string(),
                        },
                    ),
                    (
                        "message".to_string(),
                        WorkflowTransformExpression::Input {
                            source: "retained".to_string(),
                            path: "value.message".to_string(),
                        },
                    ),
                ]),
            },
            output: ValueSchema {
                type_name: "bcode.git.commit-request/v1".to_string(),
                schema: serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["repo_path", "expected_head", "message", "paths"],
                    "properties": {
                        "repo_path": {"type": "string"},
                        "expected_head": {"type": "string"},
                        "message": {"type": "string"},
                        "paths": {"type": "array", "items": {"type": "string"}}
                    }
                }),
            },
        };
        assert_eq!(
            transform
                .evaluate(&[WorkflowTransformInput {
                    name: "retained",
                    value: &retained,
                }])
                .expect("commit request"),
            serde_json::json!({
                "repo_path": ".",
                "expected_head": "abc123",
                "message": "Implement durable state envelope",
                "paths": ["src/lib.rs", "Cargo.toml"]
            })
        );
    }

    #[test]
    fn state_envelope_carries_retained_state_value_and_artifact_references_explicitly() {
        let reference = ArtifactReference::new(
            "artifact-1",
            "bcode.verification-output",
            1,
            "application/json",
            "verification/output.json",
        );
        let envelope = WorkflowStateEnvelope::new(
            serde_json::json!({
                "git": {"expected_head": "abc", "paths": ["src/lib.rs"]}
            }),
            serde_json::json!({"message": "Implement retained state"}),
        )
        .with_artifacts(vec![reference]);
        let value = serde_json::to_value(&envelope).expect("serialize");
        assert_eq!(value["schema_version"], WORKFLOW_STATE_ENVELOPE_VERSION);
        assert_eq!(value["state"]["git"]["expected_head"], "abc");
        assert_eq!(value["value"]["message"], "Implement retained state");
        assert_eq!(value["artifacts"][0]["artifact_id"], "artifact-1");
        let schema =
            ValueSchema::of::<WorkflowStateEnvelope<serde_json::Value, serde_json::Value>>();
        jsonschema::validator_for(&schema.schema)
            .expect("schema")
            .validate(&value)
            .expect("valid envelope");
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

    #[test]
    fn branch_predicates_reject_missing_incompatible_and_bad_nested_paths() {
        let predicate = PredicateExpression::Equals {
            version: WORKFLOW_PREDICATE_VERSION,
            path: "review.result.passed".to_string(),
            value: serde_json::json!(true),
        };
        assert!(
            predicate
                .evaluate_value(&serde_json::json!({
                    "review": {"result": {"passed": true}}
                }))
                .expect("nested predicate")
        );
        let missing = predicate
            .evaluate_value(&serde_json::json!({"review": {"result": {}}}))
            .expect_err("missing field");
        assert!(missing.to_string().contains("was not present"));
        let incompatible = predicate
            .evaluate_value(&serde_json::json!({
                "review": {"result": {"passed": "true"}}
            }))
            .expect_err("incompatible value");
        assert!(incompatible.to_string().contains("incompatible"));
        let non_object = predicate
            .evaluate_value(&serde_json::json!({"review": []}))
            .expect_err("nested non-object");
        assert!(non_object.to_string().contains("was not present"));
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
        let controller = &workflow.definition().nodes["workers"];
        assert_eq!(
            controller.configuration["fan_out_version"],
            WORKFLOW_FAN_OUT_RESULT_VERSION
        );
        assert_eq!(
            controller.configuration["ordering"],
            "input_index_ascending"
        );
        let canonical = WorkflowFanOutResult::new(vec![
            WorkflowFanOutMember {
                index: 0,
                value: 6_u32,
            },
            WorkflowFanOutMember {
                index: 1,
                value: 2_u32,
            },
            WorkflowFanOutMember {
                index: 2,
                value: 4_u32,
            },
        ])
        .expect("canonical output");
        assert_eq!(
            serde_json::to_value(canonical).expect("serialize"),
            serde_json::json!({
                "version": WORKFLOW_FAN_OUT_RESULT_VERSION,
                "members": [
                    {"index": 0, "value": 6},
                    {"index": 1, "value": 2},
                    {"index": 2, "value": 4}
                ]
            })
        );
        assert!(
            WorkflowFanOutResult::new(vec![
                WorkflowFanOutMember {
                    index: 1,
                    value: 2_u32,
                },
                WorkflowFanOutMember {
                    index: 0,
                    value: 6_u32,
                },
            ])
            .is_err()
        );
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
