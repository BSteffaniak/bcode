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
use serde::{
    Deserialize, Serialize,
    de::{DeserializeOwned, MapAccess, SeqAccess, Visitor},
};
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
pub const WORKFLOW_DEFINITION_SCHEMA_VERSION: u32 = 2;

/// Stable durable-production capability contract version.
pub const WORKFLOW_PRODUCTION_CAPABILITY_VERSION: u32 = 1;

/// Stable current-host requirement availability report version.
pub const WORKFLOW_REQUIREMENT_AVAILABILITY_VERSION: u32 = 1;

/// Stable deterministic predicate contract version.
pub const WORKFLOW_PREDICATE_VERSION: u32 = 3;
/// Earliest deterministic predicate contract version retained for compatibility.
pub const WORKFLOW_PREDICATE_MIN_VERSION: u32 = 1;

const MAX_PREDICATE_DEPTH: usize = 16;
const MAX_PREDICATE_OPERATIONS: usize = 256;
const MAX_PREDICATE_PATH_BYTES: usize = 512;
const MAX_PREDICATE_PATH_SEGMENT_BYTES: usize = 256;
const MAX_PREDICATE_VALUE_BYTES: usize = 65_536;
/// Stable typed structured-value selector contract version.
pub const WORKFLOW_VALUE_SELECTOR_VERSION: u32 = 1;
const MAX_VALUE_SELECTOR_SEGMENTS: usize = 64;
const MAX_VALUE_SELECTOR_FIELD_BYTES: usize = 256;
const MAX_VALUE_SELECTOR_INDEX: usize = 1_000_000;
const WORKFLOW_TOML_NULL_MARKER: &str = "$bcode_null";

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

/// Stable durable fan-out controller contract version.
pub const WORKFLOW_FAN_OUT_CONFIGURATION_VERSION: u32 = 1;

/// Durable homogeneous fan-out controller configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowFanOutConfiguration {
    /// Fan-out controller contract version.
    pub version: u32,
    /// Exact virtual member operation dispatched for every input item.
    pub member_node: Box<NodeDefinition>,
    /// Maximum admitted members.
    pub max_members: u32,
    /// Maximum concurrently active member attempts.
    pub max_concurrency: u32,
    /// Failure behavior for sibling members.
    pub failure_policy: ParallelFailurePolicy,
}

impl WorkflowFanOutConfiguration {
    /// Validate bounded member execution and owner configuration.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, invalid bounds, nested controllers, or an
    /// invalid virtual member node.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if self.version != WORKFLOW_FAN_OUT_CONFIGURATION_VERSION
            || self.max_members == 0
            || self.max_concurrency == 0
            || self.max_concurrency > self.max_members
        {
            return Err(WorkflowError::Build {
                path: "fan_out".to_string(),
                message: "fan-out version, member bound, or concurrency bound is invalid"
                    .to_string(),
            });
        }
        if matches!(
            self.member_node.kind,
            NodeKind::FanOut
                | NodeKind::Parallel
                | NodeKind::Repeat
                | NodeKind::Retry
                | NodeKind::Branch
        ) {
            return Err(WorkflowError::Build {
                path: "fan_out.member_node".to_string(),
                message: "fan-out member must be one externally owned leaf operation".to_string(),
            });
        }
        validate_control_node(&self.member_node)
    }
}

/// Stable durable transform contract version.
pub const WORKFLOW_TRANSFORM_VERSION: u32 = 2;
/// Earliest declarative transform contract version retained for compatibility.
pub const WORKFLOW_TRANSFORM_MIN_VERSION: u32 = 1;

/// Durable transform source containing the output that selected the successor edge.
pub const WORKFLOW_TRANSFORM_SOURCE_CURRENT: &str = "current";
/// Durable transform source containing the immutable workflow run input.
pub const WORKFLOW_TRANSFORM_SOURCE_STATE: &str = "state";
/// Durable transform source containing the exact persisted authored-run configuration, or null for
/// a run that was not started from authored state.
pub const WORKFLOW_TRANSFORM_SOURCE_CONFIGURATION: &str = "configuration";
/// Prefix for durable transform sources containing exact named predecessor outputs.
pub const WORKFLOW_TRANSFORM_SOURCE_DEPENDENCY_PREFIX: &str = "dependency.";
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

pub const WORKFLOW_BLOCK_INVOKE_OPERATION: &str = "invoke";
/// Standard plugin workflow-block owner preparation operation.
pub const WORKFLOW_BLOCK_PREPARE_OPERATION: &str = "prepare";

/// Stable owner-prepared workflow dispatch contract version.
pub const WORKFLOW_BLOCK_PREPARATION_VERSION: u32 = 1;
/// Maximum serialized owner-preparation request bytes.
pub const MAX_WORKFLOW_BLOCK_PREPARATION_REQUEST_BYTES: usize = 1_048_576;
/// Maximum serialized owner-preparation payload or diagnostics bytes.
pub const MAX_WORKFLOW_BLOCK_PREPARATION_BYTES: usize = 131_072;
/// Maximum owner-preparation diagnostics.
pub const MAX_WORKFLOW_BLOCK_PREPARATION_DIAGNOSTICS: usize = 64;

/// Generic host context supplied to a block owner before authorization or invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowBlockPreparationContext {
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    /// Attempt is zero during pre-admission preparation; the durable attempt identity is assigned
    /// only when this preparation is atomically persisted.
    pub attempt: u32,
    /// Stable activation-scoped correlation identity available before durable attempt admission.
    pub preparation_identity: String,
    pub workspace_root: std::path::PathBuf,
}

/// Compute the stable SHA-256 identity of an exact workflow-block owner input.
///
/// # Errors
///
/// Returns an error when the JSON value cannot be encoded.
pub fn workflow_block_input_sha256(input: &serde_json::Value) -> Result<String, String> {
    use sha2::Digest as _;

    serde_json::to_vec(input)
        .map(|encoded| format!("{:x}", sha2::Sha256::digest(encoded)))
        .map_err(|error| format!("failed to encode workflow block input: {error}"))
}

/// Compute the stable SHA-256 identity of a canonical JSON value.
///
/// # Errors
///
/// Returns an error when the value cannot be canonicalized.
pub fn workflow_canonical_value_sha256(input: &serde_json::Value) -> Result<String, String> {
    canonical_sha256(input, "workflow.canonical_value").map_err(|error| error.to_string())
}

/// Versioned generic request for owner-provided canonical operation facts and descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowBlockPreparationRequest {
    pub version: u32,
    pub block: WorkflowBlockDefinition,
    pub context: WorkflowBlockPreparationContext,
    pub input: serde_json::Value,
}

impl WorkflowBlockPreparationRequest {
    /// Validate generic request version, identity, path, and payload bounds.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, malformed context, invalid block contracts, or
    /// oversized requests.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        self.block.validate()?;
        let identities = [
            self.context.run_id.as_str(),
            self.context.node_id.as_str(),
            self.context.activation_id.as_str(),
            self.context.preparation_identity.as_str(),
        ];
        if self.version != WORKFLOW_BLOCK_PREPARATION_VERSION
            || self.context.attempt != 0
            || !self.context.workspace_root.is_absolute()
            || identities
                .iter()
                .any(|value| value.trim().is_empty() || value.len() > 4_096)
            || serde_json::to_vec(self).map_or(true, |encoded| {
                encoded.len() > MAX_WORKFLOW_BLOCK_PREPARATION_REQUEST_BYTES
            })
        {
            return Err(WorkflowError::Build {
                path: "block_preparation.request".to_string(),
                message:
                    "workflow block preparation request is unsupported, malformed, or unbounded"
                        .to_string(),
            });
        }
        Ok(())
    }
}

/// Versioned owner preparation result transported opaquely by generic workflow layers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowBlockPreparationResponse {
    pub version: u32,
    /// SHA-256 of the exact canonical owner input prepared by the owner.
    pub input_sha256: String,
    /// Stable owner identity for routing and descriptor provenance.
    pub owner_id: String,
    /// Canonical normalized operation facts consumed by application authorization.
    pub operation_facts: serde_json::Value,
    /// Owner-issued descriptor that invocation must verify against the exact input.
    pub descriptor: serde_json::Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<String>,
}

impl WorkflowBlockPreparationResponse {
    /// Validate version and generic payload bounds without interpreting owner facts.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, oversized payloads or diagnostics, empty
    /// descriptors/facts, or malformed diagnostic text.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if self.version != WORKFLOW_BLOCK_PREPARATION_VERSION
            || self.input_sha256.len() != 64
            || !self
                .input_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || self.owner_id.trim().is_empty()
            || self.owner_id.len() > 256
            || self.operation_facts.is_null()
            || self.descriptor.is_null()
            || self.diagnostics.len() > MAX_WORKFLOW_BLOCK_PREPARATION_DIAGNOSTICS
            || self
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.is_empty() || diagnostic.len() > 4_096)
        {
            return Err(WorkflowError::Build {
                path: "block_preparation".to_string(),
                message: "workflow block preparation is unsupported, empty, or unbounded"
                    .to_string(),
            });
        }
        let bytes = serde_json::to_vec(self).map_err(|error| WorkflowError::Build {
            path: "block_preparation".to_string(),
            message: error.to_string(),
        })?;
        if bytes.len() > MAX_WORKFLOW_BLOCK_PREPARATION_BYTES {
            return Err(WorkflowError::Build {
                path: "block_preparation".to_string(),
                message: format!(
                    "workflow block preparation exceeds {MAX_WORKFLOW_BLOCK_PREPARATION_BYTES} bytes"
                ),
            });
        }
        Ok(())
    }
}

/// Versioned host envelope for one exact plugin-owned workflow-block invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowBlockInvocation {
    pub version: u32,
    pub dispatch_identity: String,
    pub workspace_root: std::path::PathBuf,
    pub input: serde_json::Value,
    /// Exact owner preparation persisted before authorization and invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preparation: Option<WorkflowBlockPreparationResponse>,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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

/// One bounded owner-neutral durable automatic retry policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowAutomaticRetryPolicy {
    /// Retry policy contract version.
    pub version: u32,
    /// Maximum attempts including the initial attempt.
    pub max_attempts: u32,
    /// Explicit owner-neutral failure classes eligible for retry.
    pub eligible_failures: Vec<AutomaticRetryFailureKind>,
    /// Initial deterministic backoff delay.
    pub initial_backoff_ms: u64,
    /// Integer backoff multiplier for later attempts.
    pub backoff_multiplier: u32,
    /// Maximum deterministic backoff delay.
    pub maximum_backoff_ms: u64,
}

impl WorkflowAutomaticRetryPolicy {
    /// Validate finite retry classes and bounded backoff configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when the version, limits, failure inventory, or backoff is invalid.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if self.version != WORKFLOW_AUTOMATIC_RETRY_POLICY_VERSION
            || self.max_attempts == 0
            || self.initial_backoff_ms == 0
            || self.maximum_backoff_ms < self.initial_backoff_ms
            || self.maximum_backoff_ms > 86_400_000
            || self.backoff_multiplier == 0
            || self.backoff_multiplier > 100
            || self.eligible_failures.is_empty()
        {
            return Err(WorkflowError::Build {
                path: "automatic_retry".to_string(),
                message: "retry version, limits, backoff, or failure inventory is invalid"
                    .to_string(),
            });
        }
        let unique = self.eligible_failures.iter().collect::<BTreeSet<_>>();
        if unique.len() != self.eligible_failures.len()
            || self.eligible_failures.iter().any(|failure| {
                !matches!(
                    failure,
                    AutomaticRetryFailureKind::OwnerUnavailableBeforeAcceptance
                        | AutomaticRetryFailureKind::OwnerReportedRetryable
                )
            })
        {
            return Err(WorkflowError::Build {
                path: "automatic_retry.eligible_failures".to_string(),
                message: "retry failure classes must be unique and safely owner-retryable"
                    .to_string(),
            });
        }
        Ok(())
    }

    /// Return whether this policy explicitly admits the failure class.
    #[must_use]
    pub fn admits(&self, failure: AutomaticRetryFailureKind) -> bool {
        self.eligible_failures.contains(&failure)
    }

    /// Calculate deterministic capped exponential backoff for the next attempt.
    ///
    /// # Errors
    ///
    /// Returns an error when arithmetic overflows.
    pub fn backoff_ms(&self, next_attempt: u32) -> Result<u64, WorkflowError> {
        let exponent = next_attempt.saturating_sub(2);
        let multiplier = u64::from(self.backoff_multiplier)
            .checked_pow(exponent)
            .ok_or_else(|| WorkflowError::Build {
                path: "automatic_retry.backoff_multiplier".to_string(),
                message: "retry backoff multiplier overflow".to_string(),
            })?;
        Ok(self
            .initial_backoff_ms
            .saturating_mul(multiplier)
            .min(self.maximum_backoff_ms))
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
    /// Owner-neutral automatic retry policy applied only to explicitly eligible observations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automatic_retry: Option<WorkflowAutomaticRetryPolicy>,
    /// Whether the external owner must prepare canonical operation facts before authorization.
    #[serde(default)]
    pub preparation_required: bool,
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
        if let Some(retry) = &self.automatic_retry {
            retry.validate()?;
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
pub const WORKFLOW_AUTHORING_DOCUMENT_VERSION: u32 = 2;
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
pub const WORKFLOW_AUTHORING_CATALOG_VERSION: u32 = 2;
/// Plugin-contributed workflow-authoring action descriptor contract version.
pub const WORKFLOW_AUTHORING_ACTION_DESCRIPTOR_VERSION: u32 = 1;
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

fn workflow_dynamic_binding_path_allowed(schema: &ValueSchema, path: &str) -> bool {
    let Some(patterns) = schema
        .schema
        .get("x-bcode-dynamic-binding-paths")
        .and_then(serde_json::Value::as_array)
    else {
        return true;
    };
    let path = path.split('.').collect::<Vec<_>>();
    patterns.iter().any(|pattern| {
        let Some(pattern) = pattern.as_str() else {
            return false;
        };
        let pattern = pattern.split('.').collect::<Vec<_>>();
        pattern.len() == path.len()
            && pattern
                .iter()
                .zip(&path)
                .all(|(expected, actual)| *expected == "*" || expected == actual)
    })
}

fn workflow_allows_dynamic_complete_input(schema: &ValueSchema) -> bool {
    schema
        .schema
        .get("x-bcode-allow-dynamic-complete-input")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true)
}

/// Declared target populated from runtime workflow configuration during compilation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowConfigurationTarget {
    /// Populate a declared field in one node's configuration object.
    NodeConfiguration { node_id: String, path: String },
    /// Populate a declared agent selection field.
    AgentSelection { node_id: String, field: String },
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
                let node = definition.node(node_id).ok_or_else(|| {
                    authoring_error(
                        "bindings.target.node_id",
                        format!("binding references unknown node '{node_id}'"),
                    )
                })?;
                if node.kind != NodeKind::PluginBlock {
                    return Err(authoring_error(
                        "bindings.target.node_id",
                        format!("binding target '{node_id}' is not a plugin-block node"),
                    ));
                }
                let block: WorkflowBlockDefinition =
                    serde_json::from_value(node.configuration.clone()).map_err(|error| {
                        authoring_error(
                            "bindings.target.node_id",
                            format!("plugin-block contract is invalid: {error}"),
                        )
                    })?;
                if !workflow_dynamic_binding_path_allowed(&block.input, path) {
                    return Err(authoring_error(
                        "bindings.target.path",
                        format!(
                            "dynamic binding path '{path}' is not authorized by block {}",
                            block.block_id
                        ),
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
    /// Required portable prompt profile identities.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub agents: BTreeSet<String>,
}

impl WorkflowRequirementSummary {
    fn validate(&self) -> Result<(), WorkflowError> {
        let count =
            self.capabilities.len() + self.plugins.len() + self.blocks.len() + self.agents.len();
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

    /// Remap canonical diagnostics through one source map while preserving digest facts.
    #[must_use]
    pub fn remap_diagnostics(&self, source_map: &WorkflowSourceMap) -> Self {
        Self {
            authoring_version: self.authoring_version,
            valid: self.valid,
            source_digest_sha256: self.source_digest_sha256.clone(),
            executable_source_digest_sha256: self.executable_source_digest_sha256.clone(),
            diagnostics: source_map.remap_diagnostics(&self.diagnostics),
        }
    }

    /// Remap and package-qualify canonical diagnostics for one planned member.
    #[must_use]
    pub fn remap_package_member_diagnostics(
        &self,
        source_map: &WorkflowPackageMemberSourceMap,
    ) -> Self {
        Self {
            authoring_version: self.authoring_version,
            valid: self.valid,
            source_digest_sha256: self.source_digest_sha256.clone(),
            executable_source_digest_sha256: self.executable_source_digest_sha256.clone(),
            diagnostics: source_map.remap_diagnostics(&self.diagnostics),
        }
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
    pub agent_execution_targets: BTreeSet<PromptContextTarget>,
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

/// Portable schema-form contract version.
pub const WORKFLOW_SCHEMA_FORM_VERSION: u32 = 1;
/// Maximum fields projected into one catalog-driven form.
pub const MAX_WORKFLOW_SCHEMA_FORM_FIELDS: usize = 4_096;

/// Renderer-neutral control kind derived from a supported JSON Schema field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowSchemaFormControl {
    Object,
    Array,
    Text,
    Number,
    Integer,
    Boolean,
    Choice,
    Json,
}

/// One source-addressed field description for native frontend form controls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSchemaFormField {
    pub path: String,
    pub title: String,
    pub control: WorkflowSchemaFormControl,
    pub required: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One catalog-derived portable form description retaining its authoritative schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSchemaFormDescription {
    pub version: u32,
    pub type_name: String,
    pub schema: ValueSchema,
    pub fields: Vec<WorkflowSchemaFormField>,
}

impl WorkflowSchemaFormDescription {
    /// Derive a bounded native-form description from an authoritative runtime schema.
    ///
    /// Unsupported compositions remain addressable JSON controls rather than being guessed.
    ///
    /// # Errors
    ///
    /// Returns an error when the schema is invalid or the projected field count is excessive.
    pub fn from_schema(schema: &ValueSchema) -> Result<Self, WorkflowError> {
        validate_runtime_value_schema("schema_form.schema", schema)?;
        let mut fields = Vec::new();
        describe_schema_fields(&schema.schema, "", false, &mut fields)?;
        Ok(Self {
            version: WORKFLOW_SCHEMA_FORM_VERSION,
            type_name: schema.type_name.clone(),
            schema: schema.clone(),
            fields,
        })
    }
}

fn describe_schema_fields(
    schema: &serde_json::Value,
    path: &str,
    required: bool,
    fields: &mut Vec<WorkflowSchemaFormField>,
) -> Result<(), WorkflowError> {
    if fields.len() >= MAX_WORKFLOW_SCHEMA_FORM_FIELDS {
        return Err(authoring_error(
            "schema_form.fields",
            format!("schema form exceeds {MAX_WORKFLOW_SCHEMA_FORM_FIELDS} fields"),
        ));
    }
    let object = schema.as_object();
    let choices = object
        .and_then(|value| value.get("enum"))
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let schema_type = object
        .and_then(|value| value.get("type"))
        .and_then(serde_json::Value::as_str);
    let control = if choices.is_empty() {
        match schema_type {
            Some("object") => WorkflowSchemaFormControl::Object,
            Some("array") => WorkflowSchemaFormControl::Array,
            Some("string") => WorkflowSchemaFormControl::Text,
            Some("number") => WorkflowSchemaFormControl::Number,
            Some("integer") => WorkflowSchemaFormControl::Integer,
            Some("boolean") => WorkflowSchemaFormControl::Boolean,
            _ => WorkflowSchemaFormControl::Json,
        }
    } else {
        WorkflowSchemaFormControl::Choice
    };
    fields.push(WorkflowSchemaFormField {
        path: path.to_string(),
        title: object
            .and_then(|value| value.get("title"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| path.rsplit('.').next().unwrap_or("value"))
            .to_string(),
        control,
        required,
        choices,
        description: object
            .and_then(|value| value.get("description"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    });
    if let Some(properties) = object
        .and_then(|value| value.get("properties"))
        .and_then(serde_json::Value::as_object)
    {
        let required_names = object
            .and_then(|value| value.get("required"))
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .collect::<BTreeSet<_>>();
        for (name, child) in properties {
            let child_path = if path.is_empty() {
                name.clone()
            } else {
                format!("{path}.{name}")
            };
            describe_schema_fields(
                child,
                &child_path,
                required_names.contains(name.as_str()),
                fields,
            )?;
        }
    }
    Ok(())
}

/// Versioned plugin-contributed concise workflow action descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowAuthoringActionDescriptor {
    /// Descriptor contract version.
    pub version: u32,
    /// Stable concise action key, such as `run`.
    pub action_key: String,
    /// Exact action version.
    pub action_version: u32,
    /// Owning plugin identity.
    pub plugin_id: String,
    /// Accepted concise payload schema.
    pub input: ValueSchema,
    /// Exact target workflow block catalog identity.
    pub target_block: String,
    /// Deterministic payload adaptation expressed in the generic transform contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_adapter: Option<WorkflowTransform>,
}

impl WorkflowAuthoringActionDescriptor {
    /// Return the exact action catalog identity.
    #[must_use]
    pub fn catalog_key(&self) -> String {
        format!("{}@{}", self.action_key, self.action_version)
    }

    /// Validate this descriptor against the target block catalog.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, malformed identities or schemas, unavailable
    /// target blocks, owner mismatch, or an invalid deterministic adapter.
    pub fn validate(
        &self,
        blocks: &BTreeMap<String, WorkflowBlockDefinition>,
    ) -> Result<(), WorkflowError> {
        if self.version != WORKFLOW_AUTHORING_ACTION_DESCRIPTOR_VERSION || self.action_version == 0
        {
            return Err(authoring_error(
                "catalog.authoring_actions.version",
                "workflow authoring action versions must be current and nonzero",
            ));
        }
        validate_authoring_id("catalog.authoring_actions.action_key", &self.action_key)?;
        validate_authoring_id("catalog.authoring_actions.plugin_id", &self.plugin_id)?;
        validate_runtime_value_schema("catalog.authoring_actions.input", &self.input)?;
        if let Some(adapter) = &self.input_adapter {
            adapter.validate()?;
        }
        let block = blocks.get(&self.target_block).ok_or_else(|| {
            authoring_error(
                "catalog.authoring_actions.target_block",
                format!("target block '{}' is unavailable", self.target_block),
            )
        })?;
        if block.plugin_id != self.plugin_id
            || self
                .input_adapter
                .as_ref()
                .is_some_and(|adapter| adapter.output != block.input)
        {
            return Err(authoring_error(
                "catalog.authoring_actions.target_block",
                "action owner and adapter output must match the exact target block",
            ));
        }
        Ok(())
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
    /// Authoritative generic node-configuration schemas keyed by stable serialized node kind.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub node_configuration_schemas: BTreeMap<String, ValueSchema>,
    /// Exact immutable definitions available for child-call preview, keyed by compiled identity.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub workflow_definitions: BTreeMap<String, WorkflowDefinition>,
    /// Portable configured prompt profile identities.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub agent_profiles: BTreeSet<String>,
    /// Versioned plugin-contributed concise authoring actions keyed by exact action identity.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub authoring_actions: BTreeMap<String, WorkflowAuthoringActionDescriptor>,
}

/// Kind of authored requirement evaluated against a current catalog snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRequirementKind {
    Capability,
    Plugin,
    Block,
    Agent,
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
            + self.node_configuration_schemas.len()
            + self.workflow_definitions.len()
            + self.agent_profiles.len()
            + self.authoring_actions.len();
        if entry_count > MAX_WORKFLOW_AUTHORING_REQUIREMENTS {
            return Err(authoring_error(
                "catalog",
                format!("catalog exceeds {MAX_WORKFLOW_AUTHORING_REQUIREMENTS} entries"),
            ));
        }
        for value in self.plugins.iter().chain(self.agent_profiles.iter()) {
            validate_authoring_id("catalog.identity", value)?;
        }
        for (key, block) in &self.blocks {
            block.validate()?;
            validate_runtime_value_schema("catalog.blocks.input", &block.input)?;
            validate_runtime_value_schema("catalog.blocks.output", &block.output)?;
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
        for (kind, schema) in &self.node_configuration_schemas {
            validate_authoring_id("catalog.node_configuration_schemas.kind", kind)?;
            validate_runtime_value_schema("catalog.node_configuration_schemas.schema", schema)?;
            if !self.capabilities.node_kinds.contains_key(kind) {
                return Err(authoring_error(
                    "catalog.node_configuration_schemas",
                    format!("configuration schema references unknown node kind '{kind}'"),
                ));
            }
        }
        for (key, definition) in &self.workflow_definitions {
            definition.validate()?;
            let (kind, _) = key.rsplit_once('@').ok_or_else(|| {
                authoring_error(
                    "catalog.workflow_definitions",
                    format!("catalog definition key '{key}' is not an exact content identity"),
                )
            })?;
            let identity = WorkflowDefinitionIdentity::for_definition(kind, definition)?;
            if key != &identity.definition_id {
                return Err(authoring_error(
                    "catalog.workflow_definitions",
                    format!("catalog definition key '{key}' does not match exact content identity"),
                ));
            }
        }
        for (key, action) in &self.authoring_actions {
            action.validate(&self.blocks)?;
            if key != &action.catalog_key() {
                return Err(authoring_error(
                    "catalog.authoring_actions",
                    format!("action key '{key}' does not match exact descriptor identity"),
                ));
            }
            if !self.plugins.contains(&action.plugin_id) {
                return Err(authoring_error(
                    "catalog.authoring_actions.plugin_id",
                    format!("action owner '{}' is unavailable", action.plugin_id),
                ));
            }
        }
        Ok(())
    }
}

/// Build the authoritative generic node-configuration schemas exposed to authoring clients.
#[must_use]
pub fn workflow_node_configuration_schemas() -> BTreeMap<String, ValueSchema> {
    let object = |type_name: &str, schema| ValueSchema {
        type_name: type_name.to_string(),
        schema,
    };
    BTreeMap::from([
        (
            "agent".to_string(),
            object(
                "bcode.workflow.agent-configuration/v1",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "agent_profile": {"type": "string", "title": "Agent profile"},
                        "provider": {"type": ["string", "null"], "title": "Provider"},
                        "model": {"type": ["string", "null"], "title": "Model"},
                        "read_only": {"type": "boolean", "title": "Read only"},
                        "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 3_600_000},
                        "system_prompt": {"type": "string", "title": "System prompt"},
                        "tool_allowlist": {"type": "array", "title": "Tool allowlist"}
                    },
                    "required": ["agent_profile", "read_only", "timeout_ms", "system_prompt"]
                }),
            ),
        ),
        (
            "branch".to_string(),
            object(
                "bcode.workflow.branch-configuration/v1",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "predicate": {"type": "object", "title": "Predicate"},
                        "true_entries": {"type": "array"},
                        "false_entries": {"type": "array"}
                    },
                    "required": ["predicate", "true_entries", "false_entries"]
                }),
            ),
        ),
        (
            "repeat".to_string(),
            object(
                "bcode.workflow.repeat-configuration/v1",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "predicate": {"type": "object", "title": "Predicate"},
                        "max_iterations": {"type": "integer", "minimum": 1, "maximum": 1000}
                    },
                    "required": ["predicate", "max_iterations"]
                }),
            ),
        ),
        (
            "parallel".to_string(),
            object(
                "bcode.workflow.parallel-configuration/v1",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "join_policy": {"enum": ["wait_all", "fail_fast"]},
                        "branch_entries": {"type": "array"}
                    },
                    "required": ["join_policy", "branch_entries"]
                }),
            ),
        ),
        (
            "input".to_string(),
            object(
                "bcode.workflow.input-configuration/v1",
                serde_json::json!({
                    "type": "object",
                    "properties": {"gate_version": {"type": "integer", "enum": [1]}},
                    "required": ["gate_version"]
                }),
            ),
        ),
        (
            "approval".to_string(),
            object(
                "bcode.workflow.approval-configuration/v1",
                serde_json::json!({
                    "type": "object",
                    "properties": {"gate_version": {"type": "integer", "enum": [1]}},
                    "required": ["gate_version"]
                }),
            ),
        ),
    ])
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

/// Current renderer-neutral semantic authoring-edit contract version.
pub const WORKFLOW_AUTHORING_EDIT_VERSION: u32 = 1;
/// Maximum operations accepted in one atomic semantic edit batch.
pub const MAX_WORKFLOW_AUTHORING_EDITS_PER_BATCH: usize = 256;

/// Stable selector for one exact edge in the authored graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowAuthoringEdgeSelector {
    /// Source node identity.
    pub from: String,
    /// Target node identity.
    pub to: String,
    /// Zero-based occurrence among edges with this source and target.
    pub occurrence: usize,
}

/// One renderer-neutral semantic edit to a [`WorkflowAuthoringDocument`].
///
/// Operations describe domain intent rather than arbitrary JSON mutation. Presentation edits are
/// confined to producer namespaces and never alter executable workflow identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowAuthoringEdit {
    /// Insert a node whose identity is not already present.
    AddNode { node: NodeDefinition },
    /// Replace one existing node while retaining its stable identity.
    UpdateNode { node: NodeDefinition },
    /// Remove a node and every edge connected to it.
    RemoveNode { node_id: String },
    /// Append one graph edge.
    AddEdge { edge: EdgeDefinition },
    /// Replace one exact graph edge occurrence.
    UpdateEdge {
        selector: WorkflowAuthoringEdgeSelector,
        edge: EdgeDefinition,
    },
    /// Remove one exact graph edge occurrence.
    RemoveEdge {
        selector: WorkflowAuthoringEdgeSelector,
    },
    /// Replace the workflow input schema.
    UpdateWorkflowInput { schema: ValueSchema },
    /// Replace the workflow output schema.
    UpdateWorkflowOutput { schema: ValueSchema },
    /// Replace all generic configuration bindings.
    UpdateBindings {
        bindings: Vec<WorkflowConfigurationBinding>,
    },
    /// Replace exact declared catalog requirements.
    UpdateRequirements {
        requirements: WorkflowRequirementSummary,
    },
    /// Replace authored defaults for one plugin-block operation input.
    UpdatePluginInputDefaults {
        node_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        defaults: Option<serde_json::Value>,
    },
    /// Replace logical graph entry nodes.
    UpdateEntries { entries: Vec<String> },
    /// Replace logical graph exit nodes.
    UpdateExits { exits: Vec<String> },
    /// Replace user-facing workflow metadata.
    UpdateMetadata { metadata: WorkflowAuthoringMetadata },
    /// Set or remove one producer-owned presentation namespace.
    UpdatePresentationNamespace {
        namespace: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<serde_json::Value>,
    },
}

/// One bounded atomic edit batch guarded by an exact draft generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowAuthoringEditBatch {
    /// Edit contract version.
    pub version: u32,
    /// Exact draft generation against which these operations were authored.
    pub expected_generation: u64,
    /// Ordered edits applied atomically by the pure reducer.
    pub edits: Vec<WorkflowAuthoringEdit>,
}

impl WorkflowAuthoringEditBatch {
    /// Validate version, generation, and batch bounds before reduction.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, zero generations, or empty/oversized batches.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if self.version != WORKFLOW_AUTHORING_EDIT_VERSION {
            return Err(authoring_error(
                "edit_batch.version",
                format!(
                    "unsupported authoring edit version {}; expected {WORKFLOW_AUTHORING_EDIT_VERSION}",
                    self.version
                ),
            ));
        }
        if self.expected_generation == 0 {
            return Err(authoring_error(
                "edit_batch.expected_generation",
                "expected draft generation must be positive",
            ));
        }
        if self.edits.is_empty() || self.edits.len() > MAX_WORKFLOW_AUTHORING_EDITS_PER_BATCH {
            return Err(authoring_error(
                "edit_batch.edits",
                format!(
                    "edit batch must contain 1..={MAX_WORKFLOW_AUTHORING_EDITS_PER_BATCH} operations"
                ),
            ));
        }
        Ok(())
    }
}

/// Apply semantic edits to a cloned document without persistence or external effects.
///
/// All operations are applied in order and the complete resulting document is validated. The input
/// document is never modified, so any failure rejects the complete batch.
///
/// # Errors
///
/// Returns a source-addressed build error when the batch is malformed, a target is missing or
/// ambiguous, an insertion conflicts, or the resulting document is invalid.
pub fn apply_workflow_authoring_edits(
    document: &WorkflowAuthoringDocument,
    batch: &WorkflowAuthoringEditBatch,
) -> Result<WorkflowAuthoringDocument, WorkflowError> {
    batch.validate()?;
    let mut updated = document.clone();
    for (index, edit) in batch.edits.iter().enumerate() {
        apply_workflow_authoring_edit(&mut updated, edit).map_err(|error| match error {
            WorkflowError::Build { path, message } => WorkflowError::Build {
                path: format!("edit_batch.edits.{index}.{path}"),
                message,
            },
            other => other,
        })?;
    }
    updated.validate()?;
    Ok(updated)
}

fn apply_workflow_authoring_edit(
    document: &mut WorkflowAuthoringDocument,
    edit: &WorkflowAuthoringEdit,
) -> Result<(), WorkflowError> {
    match edit {
        WorkflowAuthoringEdit::AddNode { node } => {
            if document.definition.nodes.contains_key(&node.id) {
                return Err(authoring_error("node.id", "node identity already exists"));
            }
            document
                .definition
                .nodes
                .insert(node.id.clone(), node.clone());
        }
        WorkflowAuthoringEdit::UpdateNode { node } => {
            let Some(existing) = document.definition.nodes.get_mut(&node.id) else {
                return Err(authoring_error("node.id", "node identity does not exist"));
            };
            *existing = node.clone();
        }
        WorkflowAuthoringEdit::RemoveNode { node_id } => {
            if document.definition.nodes.remove(node_id).is_none() {
                return Err(authoring_error("node_id", "node identity does not exist"));
            }
            document
                .definition
                .edges
                .retain(|edge| edge.from != *node_id && edge.to != *node_id);
            document.definition.entries.retain(|entry| entry != node_id);
            document.definition.exits.retain(|exit| exit != node_id);
            document.plugin_input_defaults.remove(node_id);
        }
        WorkflowAuthoringEdit::AddEdge { edge } => document.definition.edges.push(edge.clone()),
        WorkflowAuthoringEdit::UpdateEdge { selector, edge } => {
            let index = authoring_edge_index(&document.definition.edges, selector)?;
            document.definition.edges[index] = edge.clone();
        }
        WorkflowAuthoringEdit::RemoveEdge { selector } => {
            let index = authoring_edge_index(&document.definition.edges, selector)?;
            document.definition.edges.remove(index);
        }
        WorkflowAuthoringEdit::UpdateWorkflowInput { schema } => {
            document.definition.input = schema.clone();
        }
        WorkflowAuthoringEdit::UpdateWorkflowOutput { schema } => {
            document.definition.output = schema.clone();
        }
        WorkflowAuthoringEdit::UpdateBindings { bindings } => {
            document.bindings.clone_from(bindings);
        }
        WorkflowAuthoringEdit::UpdateRequirements { requirements } => {
            document.requirements.clone_from(requirements);
        }
        WorkflowAuthoringEdit::UpdatePluginInputDefaults { node_id, defaults } => {
            if let Some(defaults) = defaults {
                document
                    .plugin_input_defaults
                    .insert(node_id.clone(), defaults.clone());
            } else {
                document.plugin_input_defaults.remove(node_id);
            }
        }
        WorkflowAuthoringEdit::UpdateEntries { entries } => {
            document.definition.entries.clone_from(entries);
        }
        WorkflowAuthoringEdit::UpdateExits { exits } => {
            document.definition.exits.clone_from(exits);
        }
        WorkflowAuthoringEdit::UpdateMetadata { metadata } => {
            document.metadata.clone_from(metadata);
        }
        WorkflowAuthoringEdit::UpdatePresentationNamespace { namespace, value } => {
            validate_authoring_id("presentation.namespace", namespace)?;
            let presentation =
                document
                    .presentation
                    .get_or_insert_with(|| WorkflowAuthoringPresentation {
                        version: WORKFLOW_AUTHORING_PRESENTATION_VERSION,
                        namespaces: BTreeMap::new(),
                    });
            match value {
                Some(value) => {
                    presentation
                        .namespaces
                        .insert(namespace.clone(), value.clone());
                }
                None => {
                    presentation.namespaces.remove(namespace);
                }
            }
            if presentation.namespaces.is_empty() {
                document.presentation = None;
            }
        }
    }
    Ok(())
}

fn authoring_edge_index(
    edges: &[EdgeDefinition],
    selector: &WorkflowAuthoringEdgeSelector,
) -> Result<usize, WorkflowError> {
    validate_authoring_id("edge_selector.from", &selector.from)?;
    validate_authoring_id("edge_selector.to", &selector.to)?;
    edges
        .iter()
        .enumerate()
        .filter(|(_, edge)| edge.from == selector.from && edge.to == selector.to)
        .nth(selector.occurrence)
        .map(|(index, _)| index)
        .ok_or_else(|| authoring_error("edge_selector", "edge occurrence does not exist"))
}

/// Changed authoring-source dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowAuthoringChangeKind {
    Executable,
    Metadata,
    Presentation,
}

/// Semantic and aggregate effect differences between two admitted authored documents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowAuthoringSemanticDiff {
    pub version: u32,
    pub changes: BTreeSet<WorkflowAuthoringChangeKind>,
    pub added_nodes: BTreeSet<String>,
    pub removed_nodes: BTreeSet<String>,
    pub changed_nodes: BTreeSet<String>,
    pub before_effects: WorkflowEffectSummary,
    pub after_effects: WorkflowEffectSummary,
    pub added_effect_classes: BTreeSet<WorkflowBlockEffect>,
    pub removed_effect_classes: BTreeSet<WorkflowBlockEffect>,
    pub capability_increased: bool,
    pub added_resources: BTreeSet<ResourceClaim>,
    pub removed_resources: BTreeSet<ResourceClaim>,
}

/// Compare two authored documents through the same portable catalog preview path.
///
/// # Errors
///
/// Returns an error when either document cannot be compiled and admitted by the supplied catalog.
pub fn workflow_authoring_semantic_diff(
    before: &WorkflowAuthoringDocument,
    after: &WorkflowAuthoringDocument,
    catalog: &WorkflowAuthoringCatalogSnapshot,
) -> Result<WorkflowAuthoringSemanticDiff, WorkflowError> {
    let before_compiled = before.compile_for_preview(catalog, None)?;
    let after_compiled = after.compile_for_preview(catalog, None)?;
    let before_nodes = &before.definition.nodes;
    let after_nodes = &after.definition.nodes;
    let added_nodes = after_nodes
        .keys()
        .filter(|node| !before_nodes.contains_key(*node))
        .cloned()
        .collect();
    let removed_nodes = before_nodes
        .keys()
        .filter(|node| !after_nodes.contains_key(*node))
        .cloned()
        .collect();
    let changed_nodes = before_nodes
        .iter()
        .filter_map(|(node, definition)| {
            after_nodes
                .get(node)
                .filter(|after_definition| *after_definition != definition)
                .map(|_| node.clone())
        })
        .collect();
    let before_resources = before_compiled
        .effects
        .resources
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let after_resources = after_compiled
        .effects
        .resources
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut changes = BTreeSet::new();
    if before.executable_semantics() != after.executable_semantics() {
        changes.insert(WorkflowAuthoringChangeKind::Executable);
    }
    if before.metadata != after.metadata {
        changes.insert(WorkflowAuthoringChangeKind::Metadata);
    }
    if before.presentation != after.presentation {
        changes.insert(WorkflowAuthoringChangeKind::Presentation);
    }
    Ok(WorkflowAuthoringSemanticDiff {
        version: 1,
        changes,
        added_nodes,
        removed_nodes,
        changed_nodes,
        added_effect_classes: after_compiled
            .effects
            .block_effects
            .difference(&before_compiled.effects.block_effects)
            .copied()
            .collect(),
        removed_effect_classes: before_compiled
            .effects
            .block_effects
            .difference(&after_compiled.effects.block_effects)
            .copied()
            .collect(),
        capability_increased: after_compiled.effects.maximum_capability
            > before_compiled.effects.maximum_capability,
        added_resources: after_resources
            .difference(&before_resources)
            .cloned()
            .collect(),
        removed_resources: before_resources
            .difference(&after_resources)
            .cloned()
            .collect(),
        before_effects: before_compiled.effects,
        after_effects: after_compiled.effects,
    })
}

/// Current structurally explicit workflow source document version.
///
/// Versions 1 and 2 are intentionally unsupported by the clean composable-workflow contract.
pub const WORKFLOW_SOURCE_DOCUMENT_VERSION: u32 = 3;
/// Current portable workflow source map version.
pub const WORKFLOW_SOURCE_MAP_VERSION: u32 = 1;
/// Current portable workflow source lowering result version.
pub const WORKFLOW_SOURCE_LOWERING_VERSION: u32 = 1;
/// Maximum concise steps accepted in one workflow source document.
pub const MAX_WORKFLOW_SOURCE_STEPS: usize = 1_000;

/// Explicit authoring profile selected structurally from a workflow source document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowSourceProfile {
    /// Structurally explicit source that lowers into the canonical authoring graph.
    Structured,
    /// The complete canonical [`WorkflowAuthoringDocument`] contract.
    Canonical,
}

/// One renderer-neutral concise action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WorkflowSourceAction {
    /// Exact plugin workflow block identity and typed input.
    Uses {
        /// Stable `plugin_id/block_id@version` identity.
        uses: String,
        /// Typed block input.
        #[serde(default = "empty_json_object", rename = "with")]
        input: serde_json::Value,
    },
    /// Plugin-contributed shorthand action and opaque typed payload.
    Shorthand(BTreeMap<String, serde_json::Value>),
}

/// One explicit source-v3 reference to a prior step output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStructuredSourceReference {
    /// Prior step identity.
    pub step: String,
    /// Explicit bounded selector. Numeric object fields and array indices are never ambiguous.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub select: Option<WorkflowValueSelector>,
}

/// Deterministic condition used by a source-v3 step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStructuredSourceCondition {
    /// Prior typed output used as predicate input.
    pub source: WorkflowStructuredSourceReference,
    /// Existing bounded canonical predicate contract.
    pub predicate: PredicateExpression,
    /// Select the step when the predicate matches (`true`) or does not match (`false`).
    #[serde(default = "default_true")]
    pub expected: bool,
}

const fn default_true() -> bool {
    true
}

/// One source-v3 structured agent declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStructuredSourcePrompt {
    /// Complete existing canonical agent configuration.
    pub configuration: WorkflowPromptConfiguration,
    /// Exact typed agent input schema.
    pub input: ValueSchema,
    /// Exact typed structured output schema. Must equal `configuration.structured_output.schema`.
    pub output: ValueSchema,
    /// Resources acquired atomically before agent dispatch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<ResourceClaim>,
}

/// Concise source-v3 prompt declaration using safe durable defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStructuredSourceConcisePrompt {
    /// Plain prompt instruction. Skill use is requested through this text, not workflow coupling.
    pub text: String,
    /// Optional exact static structured input. This is materialized as a constant edge transform
    /// when the prompt follows another source step.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "with")]
    pub input_value: Option<serde_json::Value>,
    /// Exact structured input schema.
    pub input: ValueSchema,
    /// Exact structured output schema.
    pub output: ValueSchema,
    /// Agent profile selected through the normal application catalog.
    pub agent_profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub execution_target: PromptContextTarget,
    #[serde(default = "default_true")]
    pub read_only: bool,
    #[serde(default)]
    pub tool_allowlist: Vec<String>,
    #[serde(default = "default_prompt_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<ResourceClaim>,
}

const fn default_prompt_timeout_ms() -> u64 {
    300_000
}

impl WorkflowStructuredSourceConcisePrompt {
    fn expand(&self) -> Result<WorkflowStructuredSourcePrompt, WorkflowError> {
        if self.text.trim().is_empty() || self.text.len() > 262_144 {
            return Err(authoring_error(
                "prompt.text",
                "prompt text must contain 1..=262144 bytes",
            ));
        }
        let tool_capability = if self.read_only {
            WorkflowToolCapability::ReadOnly
        } else {
            WorkflowToolCapability::Mutating
        };
        let configuration = WorkflowPromptConfiguration {
            version: WORKFLOW_PROMPT_CONFIGURATION_VERSION,
            execution_target: self.execution_target,
            agent_profile: self.agent_profile.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            structured_output: PromptStructuredOutputPolicy {
                schema: self.output.clone(),
                strict: true,
            },
            read_only: self.read_only,
            tool_capability,
            tool_allowlist: self.tool_allowlist.clone(),
            timeout_ms: self.timeout_ms,
            prompt_mode: "json_input".to_string(),
            system_prompt: self.text.clone(),
        };
        configuration.validate()?;
        Ok(WorkflowStructuredSourcePrompt {
            configuration,
            input: self.input.clone(),
            output: self.output.clone(),
            resources: self.resources.clone(),
        })
    }
}

/// One source-v3 durable gate declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStructuredSourceGate {
    /// Exact typed value accepted and forwarded by the gate.
    pub schema: ValueSchema,
    /// Resources acquired atomically while resolving the gate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<ResourceClaim>,
}

/// One bounded source-v3 repeat controller over a completed prior step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStructuredSourceRepeat {
    /// Existing bounded deterministic continuation predicate.
    pub while_predicate: PredicateExpression,
    /// Maximum body executions including the initial execution.
    pub max_iterations: u32,
    /// Explicit behavior when the effective iteration bound is reached.
    #[serde(default)]
    pub exhaustion_policy: WorkflowRepeatExhaustionPolicy,
}

/// One source-v3 fixed two-branch typed parallel join.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStructuredSourceParallelJoin {
    /// Prior step forming the left branch exit.
    pub left: String,
    /// Prior step forming the right branch exit.
    pub right: String,
    /// Existing durable parallel failure behavior.
    #[serde(default)]
    pub failure_policy: ParallelFailurePolicy,
}

/// One bounded source-v3 retry declaration. Runtime production admission remains separate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStructuredSourceRetry {
    /// Maximum attempts including the initial attempt.
    pub max_attempts: u32,
    /// Eligible owner-neutral failure classes. Unsafe terminal classes are rejected.
    pub eligible_failures: Vec<AutomaticRetryFailureKind>,
    /// Initial deterministic backoff delay.
    pub initial_backoff_ms: u64,
    /// Integer backoff multiplier for later attempts.
    pub backoff_multiplier: u32,
    /// Maximum deterministic backoff delay.
    pub maximum_backoff_ms: u64,
}

impl From<&WorkflowStructuredSourceRetry> for WorkflowAutomaticRetryPolicy {
    fn from(retry: &WorkflowStructuredSourceRetry) -> Self {
        Self {
            version: WORKFLOW_AUTOMATIC_RETRY_POLICY_VERSION,
            max_attempts: retry.max_attempts,
            eligible_failures: retry.eligible_failures.clone(),
            initial_backoff_ms: retry.initial_backoff_ms,
            backoff_multiplier: retry.backoff_multiplier,
            maximum_backoff_ms: retry.maximum_backoff_ms,
        }
    }
}

/// One bounded source homogeneous fan-out declaration. Runtime admission remains separate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStructuredSourceFanOut {
    /// Exact typed array presented to the fan-out controller.
    pub input: ValueSchema,
    /// Exact typed member selected from the input array.
    pub member: ValueSchema,
    /// Exact typed member output.
    pub output_member: ValueSchema,
    /// Existing generic operation applied independently to each member.
    pub operation: WorkflowStructuredSourceOperation,
    /// Maximum admitted members.
    pub max_members: u32,
    /// Maximum concurrently executing members.
    pub max_concurrency: u32,
    /// Failure behavior for sibling members.
    #[serde(default)]
    pub failure_policy: ParallelFailurePolicy,
}

/// One source-v3 step operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowStructuredSourceOperation {
    /// Homogeneous bounded fan-out over one generic nested operation.
    FanOut(Box<WorkflowStructuredSourceFanOut>),
    /// Fixed two-branch typed parallel join over two prior branch exits.
    Parallel(Box<WorkflowStructuredSourceParallelJoin>),
    /// Exact immutable child-workflow call.
    WorkflowCall(Box<WorkflowCallConfiguration>),
    /// Durable external typed input gate.
    Input(Box<WorkflowStructuredSourceGate>),
    /// Durable explicit human approval gate.
    Approval(Box<WorkflowStructuredSourceGate>),
    /// Concise prompt using safe durable defaults.
    Prompt(Box<WorkflowStructuredSourceConcisePrompt>),
    /// Agent-owned complete structured turn.
    Agent(Box<WorkflowStructuredSourcePrompt>),
    /// Generic exact or shorthand plugin action.
    Action(WorkflowSourceAction),
}

impl Serialize for WorkflowStructuredSourceOperation {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::FanOut(fan_out) => BTreeMap::from([(
                "fan_out".to_string(),
                serde_json::to_value(fan_out).map_err(serde::ser::Error::custom)?,
            )])
            .serialize(serializer),
            Self::Parallel(parallel) => BTreeMap::from([(
                "parallel".to_string(),
                serde_json::to_value(parallel).map_err(serde::ser::Error::custom)?,
            )])
            .serialize(serializer),
            Self::WorkflowCall(call) => BTreeMap::from([(
                "workflow_call".to_string(),
                serde_json::to_value(call).map_err(serde::ser::Error::custom)?,
            )])
            .serialize(serializer),
            Self::Input(input) => BTreeMap::from([(
                "input".to_string(),
                serde_json::to_value(input).map_err(serde::ser::Error::custom)?,
            )])
            .serialize(serializer),
            Self::Approval(approval) => BTreeMap::from([(
                "approval".to_string(),
                serde_json::to_value(approval).map_err(serde::ser::Error::custom)?,
            )])
            .serialize(serializer),
            Self::Prompt(prompt) => BTreeMap::from([(
                "prompt".to_string(),
                serde_json::to_value(prompt).map_err(serde::ser::Error::custom)?,
            )])
            .serialize(serializer),
            Self::Agent(prompt) => BTreeMap::from([(
                "agent".to_string(),
                serde_json::to_value(prompt).map_err(serde::ser::Error::custom)?,
            )])
            .serialize(serializer),
            Self::Action(action) => action.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for WorkflowStructuredSourceOperation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let fields = BTreeMap::<String, serde_json::Value>::deserialize(deserializer)?;
        if fields.len() == 1 {
            if let Some(value) = fields.get("fan_out") {
                return serde_json::from_value(value.clone())
                    .map(|value| Self::FanOut(Box::new(value)))
                    .map_err(serde::de::Error::custom);
            }
            if let Some(value) = fields.get("parallel") {
                return serde_json::from_value(value.clone())
                    .map(|value| Self::Parallel(Box::new(value)))
                    .map_err(serde::de::Error::custom);
            }
            if let Some(value) = fields.get("workflow_call") {
                return serde_json::from_value(value.clone())
                    .map(|value| Self::WorkflowCall(Box::new(value)))
                    .map_err(serde::de::Error::custom);
            }
            if let Some(value) = fields.get("input") {
                return serde_json::from_value(value.clone())
                    .map(|value| Self::Input(Box::new(value)))
                    .map_err(serde::de::Error::custom);
            }
            if let Some(value) = fields.get("approval") {
                return serde_json::from_value(value.clone())
                    .map(|value| Self::Approval(Box::new(value)))
                    .map_err(serde::de::Error::custom);
            }
            if let Some(value) = fields.get("prompt") {
                return serde_json::from_value(value.clone())
                    .map(|value| Self::Prompt(Box::new(value)))
                    .map_err(serde::de::Error::custom);
            }
            if let Some(value) = fields.get("agent") {
                return serde_json::from_value(value.clone())
                    .map(|value| Self::Agent(Box::new(value)))
                    .map_err(serde::de::Error::custom);
            }
        }
        serde_json::from_value(serde_json::Value::Object(fields.into_iter().collect()))
            .map(Self::Action)
            .map_err(serde::de::Error::custom)
    }
}

/// One structurally explicit source-v3 step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowStructuredSourceStep {
    /// Stable explicit identity. Unlike v1, v2 never derives semantic identity from display text.
    pub id: String,
    /// Optional display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Explicit predecessor identities. Omission selects the immediately preceding step unless
    /// `independent` is true.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
    /// Start a new graph branch instead of implicitly depending on the previous source step.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub independent: bool,
    /// Optional exact prior-step output used as this step's input.
    ///
    /// Omission preserves the canonical dependency payload. A selected reference lowers to the
    /// existing bounded `WorkflowTransform`/`WorkflowValueSelector` contracts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_from: Option<WorkflowStructuredSourceReference>,
    /// Optional declarative input expression evaluated from constants, immutable root input, and
    /// exact named predecessor outputs. Its declared output must match the node input interface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_expression: Option<WorkflowTransform>,
    /// Optional deterministic condition over one prior typed output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<WorkflowStructuredSourceCondition>,
    /// Optional bounded retry policy for an external owner operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<WorkflowStructuredSourceRetry>,
    /// Bounded repeat controller applied after this operation completes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<WorkflowStructuredSourceRepeat>,
    /// Exactly one structured agent or generic plugin-owned action.
    #[serde(flatten)]
    pub operation: WorkflowStructuredSourceOperation,
}

/// Versioned structurally explicit workflow source-v3 document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStructuredSourceDocument {
    /// Must equal [`WORKFLOW_SOURCE_DOCUMENT_VERSION`].
    pub workflow_source_version: u32,
    /// Stable logical workflow identity.
    pub workflow_id: String,
    /// User-facing title.
    pub title: String,
    /// Optional user-facing description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Discovery labels.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// Exact versioned workflow input interface. When absent, source-v3 compatibility derives the
    /// interface from all graph entries; new source should always declare it explicitly.
    #[serde(default)]
    pub input: Option<ValueSchema>,
    /// Exact versioned workflow output interface. When absent, source-v3 compatibility derives the
    /// interface from all successful graph exits; new source should declare it explicitly.
    #[serde(default)]
    pub output: Option<ValueSchema>,
    /// Runtime configuration schema.
    #[serde(default = "empty_workflow_source_configuration_schema")]
    pub configuration_schema: ValueSchema,
    /// Optional runtime configuration defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration_defaults: Option<serde_json::Value>,
    /// Portable run limits.
    #[serde(default)]
    pub run_limits: WorkflowRunLimitPolicy,
    /// Explicit ordered steps.
    pub steps: Vec<WorkflowStructuredSourceStep>,
}

/// Kind of canonical source-map target.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowSourceMapTargetKind {
    /// Canonical node identity.
    #[default]
    Node,
    /// Canonical edge selected by exact endpoints.
    Edge,
}

/// One deterministic source-to-canonical mapping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSourceMapEntry {
    /// Zero-based top-level step index.
    pub step_index: usize,
    /// Stable source path, including nested construct paths.
    pub source_path: String,
    /// Target kind. Defaults to node for source-map v1 compatibility.
    #[serde(default)]
    pub target_kind: WorkflowSourceMapTargetKind,
    /// Deterministic canonical node identity or edge source identity.
    pub node_id: String,
    /// Canonical edge target identity when `target_kind` is `edge`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_to: Option<String>,
}

/// Bounded deterministic source map for a lowering operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSourceMap {
    /// Source-map contract version.
    pub version: u32,
    /// Ordered mappings in concise step order.
    pub entries: Vec<WorkflowSourceMapEntry>,
}

impl WorkflowSourceMap {
    /// Remap canonical node/edge-addressed diagnostics to exact source paths.
    #[must_use]
    pub fn remap_diagnostics(
        &self,
        diagnostics: &[WorkflowValidationDiagnostic],
    ) -> Vec<WorkflowValidationDiagnostic> {
        diagnostics
            .iter()
            .cloned()
            .map(|mut diagnostic| {
                if let Some(entry) = self.entries.iter().find(|entry| match entry.target_kind {
                    WorkflowSourceMapTargetKind::Node => {
                        diagnostic.document_path == format!("definition.nodes.{}", entry.node_id)
                            || diagnostic
                                .document_path
                                .starts_with(&format!("definition.nodes.{}.", entry.node_id))
                    }
                    WorkflowSourceMapTargetKind::Edge => entry.edge_to.as_ref().is_some_and(|to| {
                        diagnostic.document_path
                            == format!("definition.edges.{}->{}", entry.node_id, to)
                            || diagnostic.document_path.starts_with(&format!(
                                "definition.edges.{}->{}.",
                                entry.node_id, to
                            ))
                    }),
                }) {
                    let canonical = match (&entry.target_kind, &entry.edge_to) {
                        (WorkflowSourceMapTargetKind::Node, _) => {
                            format!("definition.nodes.{}", entry.node_id)
                        }
                        (WorkflowSourceMapTargetKind::Edge, Some(to)) => {
                            format!("definition.edges.{}->{to}", entry.node_id)
                        }
                        (WorkflowSourceMapTargetKind::Edge, None) => String::new(),
                    };
                    if canonical.is_empty() {
                        return diagnostic;
                    }
                    diagnostic.document_path =
                        diagnostic
                            .document_path
                            .replacen(&canonical, &entry.source_path, 1);
                }
                diagnostic
            })
            .collect()
    }
}

/// Portable source-apply result contract version.
pub const WORKFLOW_SOURCE_APPLY_RESULT_VERSION: u32 = 1;
/// Default mutable draft identity used by source-aware apply.
pub const DEFAULT_WORKFLOW_SOURCE_DRAFT_ID: &str = "source";

/// Stable outcome of one source-aware create-or-replace operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowSourceApplyOutcome {
    /// A new logical workflow and generation-1 draft were created.
    Created,
    /// One exact existing draft generation was replaced.
    Updated,
    /// The single optimistic replacement observed a concurrent generation change.
    Conflict {
        expected_generation: u64,
        current_generation: u64,
    },
}

/// Structured renderer-neutral result returned by source-aware apply producers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSourceApplyResult {
    pub version: u32,
    pub source_format: WorkflowSourceFormat,
    pub source_profile: WorkflowSourceProfile,
    pub workflow_id: String,
    pub draft_id: String,
    pub generation: u64,
    pub canonical_digest_sha256: String,
    pub validation: WorkflowValidationReport,
    pub source_map: WorkflowSourceMap,
    pub requirements: WorkflowRequirementSummary,
    pub effects: WorkflowEffectSummary,
    pub permissions: WorkflowPermissionPreview,
    pub outcome: WorkflowSourceApplyOutcome,
}

/// Portable result of decoding and lowering one workflow source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSourceLoweringResult {
    /// Lowering result contract version.
    pub version: u32,
    /// Structurally selected source profile.
    pub profile: WorkflowSourceProfile,
    /// Canonical authoring document used by all durable application operations.
    pub document: WorkflowAuthoringDocument,
    /// Structured source mapping; empty for canonical source.
    pub source_map: WorkflowSourceMap,
    /// Deterministic canonical source validation report.
    pub validation: WorkflowValidationReport,
}

/// Current portable workflow package manifest version.
pub const WORKFLOW_PACKAGE_MANIFEST_VERSION: u32 = 3;
/// Maximum source members in one package.
pub const MAX_WORKFLOW_PACKAGE_MEMBERS: usize = 64;
/// Maximum direct dependencies declared by one package member.
pub const MAX_WORKFLOW_PACKAGE_MEMBER_DEPENDENCIES: usize = 32;
/// Maximum package dependency depth.
pub const MAX_WORKFLOW_PACKAGE_DEPTH: usize = 8;
/// Maximum aggregate package source bytes.
pub const MAX_WORKFLOW_PACKAGE_SOURCE_BYTES: usize = 4_194_304;
/// Maximum total direct dependency edges in one package.
pub const MAX_WORKFLOW_PACKAGE_EDGES: usize = 1_024;

/// Current pure workflow package planning result version.
pub const WORKFLOW_PACKAGE_PLAN_VERSION: u32 = 1;
/// Current bounded transitive workflow package closure version.
pub const WORKFLOW_PACKAGE_CLOSURE_VERSION: u32 = 1;
/// Maximum packages accepted in one transitive closure.
pub const MAX_WORKFLOW_PACKAGE_CLOSURE_PACKAGES: usize = 128;
/// Current side-effect-free workflow package preview version.
pub const WORKFLOW_PACKAGE_PREVIEW_VERSION: u32 = 1;
/// Current package-member source-map envelope version.
pub const WORKFLOW_PACKAGE_MEMBER_SOURCE_MAP_VERSION: u32 = 1;

/// Package-qualified source map for one successfully planned member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackageMemberSourceMap {
    /// Must equal [`WORKFLOW_PACKAGE_MEMBER_SOURCE_MAP_VERSION`].
    pub version: u32,
    /// Stable package-local member identity.
    pub member_id: String,
    /// Client-relative diagnostic source name.
    pub source_name: String,
    /// Member-local canonical-to-source mapping.
    pub source_map: WorkflowSourceMap,
}

impl WorkflowPackageMemberSourceMap {
    /// Remap canonical diagnostics and qualify every result with its exact package member.
    #[must_use]
    pub fn remap_diagnostics(
        &self,
        diagnostics: &[WorkflowValidationDiagnostic],
    ) -> Vec<WorkflowValidationDiagnostic> {
        self.source_map
            .remap_diagnostics(diagnostics)
            .into_iter()
            .map(|mut diagnostic| {
                diagnostic.document_path = format!(
                    "package.members.{}.source.{}",
                    self.member_id, diagnostic.document_path
                );
                diagnostic
            })
            .collect()
    }
}

/// One child-before-parent package member compilation result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackagePlannedMember {
    /// Stable package-local member identity.
    pub member_id: String,
    /// Canonical lowering result after exact local-call resolution.
    pub lowering: WorkflowSourceLoweringResult,
    /// Package-qualified source-addressing contract for this member.
    pub member_source_map: WorkflowPackageMemberSourceMap,
    /// Exact compiled definition identity used by parent calls.
    pub definition_identity: WorkflowDefinitionIdentity,
    /// Deterministic package-local dependency closure.
    pub dependency_closure: Vec<String>,
}

/// Pure bounded package plan with no persistence or side effects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackagePlan {
    /// Must equal [`WORKFLOW_PACKAGE_PLAN_VERSION`].
    pub version: u32,
    /// Stable package identity.
    pub package_id: String,
    /// Members in deterministic child-before-parent order.
    pub members: Vec<WorkflowPackagePlannedMember>,
    /// Reproducibility result derived from this successful plan.
    pub lock: WorkflowPackageLock,
}

/// One source package supplied to bounded transitive closure planning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackageClosureSource {
    /// Stable package identity; must equal the embedded manifest identity.
    pub package_id: String,
    /// Canonical confined source label for diagnostics and duplicate-path detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_name: Option<String>,
    /// Complete source manifest after local member files were confined and loaded.
    pub manifest: WorkflowPackageManifest,
}

/// Complete portable input for recursively planning an explicit package entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackageClosure {
    /// Must equal [`WORKFLOW_PACKAGE_CLOSURE_VERSION`].
    pub version: u32,
    /// Stable entry package identity.
    pub entry_package_id: String,
    /// Exact bounded package inventory. Ordering is not semantic.
    pub packages: Vec<WorkflowPackageClosureSource>,
}

/// One planned package in a deterministic transitive closure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackageClosurePlanEntry {
    /// Stable package identity.
    pub package_id: String,
    /// Complete package plan with exact imported export targets.
    pub plan: WorkflowPackagePlan,
}

/// Pure complete transitive package plan with dependencies before importers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackageClosurePlan {
    /// Must equal [`WORKFLOW_PACKAGE_CLOSURE_VERSION`].
    pub version: u32,
    /// Stable entry package identity.
    pub entry_package_id: String,
    /// Packages in deterministic dependency-before-importer order.
    pub packages: Vec<WorkflowPackageClosurePlanEntry>,
}

/// One child-before-parent member compilation preview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackageMemberPreview {
    /// Stable package-local member identity.
    pub member_id: String,
    /// Client-relative source name.
    pub source_name: String,
    /// Package-qualified source map retained for diagnostics.
    pub source_map: WorkflowPackageMemberSourceMap,
    /// Exact member compilation result, including recursive child workflow facts.
    pub compilation: WorkflowCompilationPreview,
}

/// Side-effect-free preview of one complete bounded package plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackagePreview {
    /// Must equal [`WORKFLOW_PACKAGE_PREVIEW_VERSION`].
    pub version: u32,
    /// Stable package identity.
    pub package_id: String,
    /// Members in deterministic child-before-parent order.
    pub members: Vec<WorkflowPackageMemberPreview>,
    /// Exact reproducibility candidate from package planning.
    pub lock: WorkflowPackageLock,
}

impl WorkflowPackagePreview {
    /// Return whether every package member compiled and passed production admission.
    #[must_use]
    pub fn is_compiled(&self) -> bool {
        !self.members.is_empty()
            && self
                .members
                .iter()
                .all(|member| member.compilation.is_compiled())
    }

    /// Return package-qualified diagnostics remapped through every member source map.
    #[must_use]
    pub fn remapped_diagnostics(&self) -> Vec<WorkflowValidationDiagnostic> {
        self.members
            .iter()
            .flat_map(|member| {
                member
                    .source_map
                    .remap_diagnostics(&member.compilation.validation.diagnostics)
            })
            .collect()
    }
}

/// Compile-preview every planned package member without persistence or side effects.
///
/// Member definitions are already resolved child-before-parent by package planning. Each preview
/// therefore uses the same exact immutable child definitions as publication and recursively
/// aggregates their requirements, effects, resources, permissions, and run limits through the
/// canonical workflow-call compiler.
///
/// # Errors
///
/// Returns an error for an inconsistent package plan, invalid configuration, unavailable catalog
/// requirements, unsupported production capability, or a malformed member preview.
pub fn preview_workflow_package(
    plan: &WorkflowPackagePlan,
    catalog: &WorkflowAuthoringCatalogSnapshot,
    configurations: &BTreeMap<String, serde_json::Value>,
) -> Result<WorkflowPackagePreview, WorkflowError> {
    validate_workflow_package_plan(plan)?;
    catalog.validate()?;
    let known = plan
        .members
        .iter()
        .map(|member| member.member_id.as_str())
        .collect::<BTreeSet<_>>();
    if configurations
        .keys()
        .any(|member_id| !known.contains(member_id.as_str()))
    {
        return Err(authoring_error(
            "package_preview.configurations",
            "package preview configuration references an unknown member",
        ));
    }
    let mut resolved_catalog = catalog.clone();
    let mut members = Vec::with_capacity(plan.members.len());
    for member in &plan.members {
        let configuration = configurations.get(&member.member_id);
        let mut compilation = member
            .lowering
            .document
            .compilation_preview(&resolved_catalog, configuration);
        compilation.validation = compilation
            .validation
            .remap_package_member_diagnostics(&member.member_source_map);
        if !compilation.is_compiled() {
            return Err(authoring_error(
                format!("package.members.{}.preview", member.member_id),
                compilation
                    .validation
                    .diagnostics
                    .first()
                    .map_or("package member did not compile", |diagnostic| {
                        diagnostic.message.as_str()
                    }),
            ));
        }
        let compiled = compilation.compiled.as_ref().ok_or_else(|| {
            authoring_error(
                format!("package.members.{}.preview", member.member_id),
                "successful package member preview omitted compiled details",
            )
        })?;
        if compiled.definition_identity != member.definition_identity {
            return Err(authoring_error(
                format!("package.members.{}.preview.configuration", member.member_id),
                "package preview configuration changed the locked executable identity",
            ));
        }
        resolved_catalog.workflow_definitions.insert(
            member.definition_identity.definition_id.clone(),
            compiled.definition.clone(),
        );
        members.push(WorkflowPackageMemberPreview {
            member_id: member.member_id.clone(),
            source_name: member.member_source_map.source_name.clone(),
            source_map: member.member_source_map.clone(),
            compilation,
        });
    }
    Ok(WorkflowPackagePreview {
        version: WORKFLOW_PACKAGE_PREVIEW_VERSION,
        package_id: plan.package_id.clone(),
        members,
        lock: plan.lock.clone(),
    })
}

/// Validate and deterministically compile one package without persistence or side effects.
///
/// Package-local source calls use `package_call: { member: <id> }` and must also declare that
/// member in the caller's manifest dependency list. Calls are replaced with exact immutable
/// canonical definition identities before normal source-v3 lowering.
///
/// # Errors
///
/// Returns an error for invalid manifests, malformed member source, missing/undeclared/forward
/// package calls, failed canonical lowering, unavailable catalog requirements, or digest errors.
#[allow(clippy::too_many_lines)]
pub fn plan_workflow_package(
    manifest: &WorkflowPackageManifest,
    catalog: &WorkflowAuthoringCatalogSnapshot,
) -> Result<WorkflowPackagePlan, WorkflowError> {
    manifest.validate()?;
    if manifest
        .imports
        .iter()
        .any(|import| import.target.is_none())
    {
        return Err(authoring_error(
            "package.imports",
            "unresolved package imports require transitive closure planning",
        ));
    }
    catalog.validate()?;
    let members = manifest
        .members
        .iter()
        .map(|member| (member.member_id.as_str(), member))
        .collect::<BTreeMap<_, _>>();
    let order = workflow_package_topological_order(&members)?;
    let external_targets = manifest
        .external_dependencies
        .iter()
        .map(|(name, target)| (name.clone(), target.clone()))
        .chain(manifest.imports.iter().filter_map(|import| {
            import
                .target
                .as_ref()
                .map(|target| (import.import_id.clone(), target.clone()))
        }))
        .collect::<BTreeMap<_, _>>();
    let mut resolved_catalog = catalog.clone();
    for target in external_targets.values() {
        let identity = target.definition_identity();
        if !resolved_catalog
            .workflow_definitions
            .contains_key(&identity.definition_id)
        {
            return Err(authoring_error(
                "package.imports.target",
                format!(
                    "exact imported definition '{}' is unavailable",
                    identity.definition_id
                ),
            ));
        }
    }
    let mut identities = BTreeMap::<String, WorkflowDefinitionIdentity>::new();
    let mut planned = Vec::with_capacity(order.len());
    let mut locked = Vec::with_capacity(order.len());
    for member_id in order {
        let member = members[member_id];
        let member_source_path =
            |path: &str| format!("package.members.{}.source.{path}", member.member_id);
        let mut value = decode_workflow_source_value(&member.source, member.format)
            .map_err(|error| qualify_workflow_package_member_error(member, error))?;
        resolve_package_calls(&mut value, member, &identities, &external_targets)?;
        let normalized = serde_json::to_string(&value).map_err(|error| {
            authoring_error(
                member_source_path("normalized"),
                format!("normalized member source cannot be serialized: {error}"),
            )
        })?;
        let mut lowering = lower_workflow_authoring_source(
            &normalized,
            WorkflowSourceFormat::Json,
            &resolved_catalog,
        )
        .map_err(|error| qualify_workflow_package_member_error(member, error))?;
        let compilation = lowering
            .document
            .compilation_preview(&resolved_catalog, None);
        let compiled = compilation.compiled.ok_or_else(|| {
            let message = compilation
                .validation
                .diagnostics
                .first()
                .map_or("package member did not compile", |diagnostic| {
                    diagnostic.message.as_str()
                });
            qualify_workflow_package_member_error(member, authoring_error("compilation", message))
        })?;
        let identity = compiled.definition_identity;
        lowering.document.definition = compiled.definition.clone();
        let closure = workflow_package_member_closure(member, &members)?;
        let source_digest = lowering.document.source_digest_sha256()?;
        let executable_digest = lowering.document.executable_source_digest_sha256()?;
        identities.insert(member.member_id.clone(), identity.clone());
        resolved_catalog
            .workflow_definitions
            .insert(identity.definition_id.clone(), compiled.definition);
        locked.push(WorkflowPackageLockedMember {
            member_id: member.member_id.clone(),
            source_digest_sha256: source_digest,
            executable_digest_sha256: executable_digest,
            definition_identity: identity.clone(),
            published_revision: None,
            dependency_closure: closure.clone(),
        });
        planned.push(WorkflowPackagePlannedMember {
            member_id: member.member_id.clone(),
            member_source_map: WorkflowPackageMemberSourceMap {
                version: WORKFLOW_PACKAGE_MEMBER_SOURCE_MAP_VERSION,
                member_id: member.member_id.clone(),
                source_name: member.source_name.clone(),
                source_map: lowering.source_map.clone(),
            },
            lowering,
            definition_identity: identity,
            dependency_closure: closure,
        });
    }
    locked.sort_by(|left, right| left.member_id.cmp(&right.member_id));
    let mut locked_imports = manifest
        .imports
        .iter()
        .map(|import| {
            let target = import.target.clone().ok_or_else(|| {
                authoring_error(
                    "package.imports.target",
                    "package imports must be resolved before single-package planning",
                )
            })?;
            let package_lock_digest_sha256 =
                import.package_lock_digest_sha256.clone().ok_or_else(|| {
                    authoring_error(
                        "package.imports.package_lock_digest_sha256",
                        "package imports must be resolved before single-package planning",
                    )
                })?;
            Ok(WorkflowPackageLockedImport {
                import_id: import.import_id.clone(),
                package_id: import.package_id.clone(),
                export: import.export.clone(),
                package_lock_digest_sha256,
                target,
            })
        })
        .collect::<Result<Vec<_>, WorkflowError>>()?;
    locked_imports.sort_by(|left, right| left.import_id.cmp(&right.import_id));
    let locked_by_id = locked
        .iter()
        .map(|member| (member.member_id.as_str(), member))
        .collect::<BTreeMap<_, _>>();
    let exports = manifest
        .exports
        .iter()
        .map(|(export, member_id)| {
            let member = locked_by_id.get(member_id.as_str()).ok_or_else(|| {
                authoring_error(
                    "package.exports",
                    "validated package export references an absent locked member",
                )
            })?;
            Ok(WorkflowPackageLockedExport {
                export: export.clone(),
                member_id: member_id.clone(),
                definition_identity: member.definition_identity.clone(),
                published_revision: member.published_revision.clone(),
            })
        })
        .collect::<Result<Vec<_>, WorkflowError>>()?;
    let lock = WorkflowPackageLock {
        version: WORKFLOW_PACKAGE_LOCK_VERSION,
        package_id: manifest.package_id.clone(),
        package_source_digest_sha256: digest_serializable(manifest)?,
        imports: locked_imports,
        exports,
        members: locked,
    };
    lock.validate()?;
    Ok(WorkflowPackagePlan {
        version: WORKFLOW_PACKAGE_PLAN_VERSION,
        package_id: manifest.package_id.clone(),
        members: planned,
        lock,
    })
}

fn visit_workflow_package_closure(
    package_id: &str,
    depth: usize,
    sources: &BTreeMap<&str, &WorkflowPackageManifest>,
    visiting: &mut BTreeSet<String>,
    planned: &mut BTreeMap<String, WorkflowPackagePlan>,
    order: &mut Vec<String>,
    catalog: &mut WorkflowAuthoringCatalogSnapshot,
) -> Result<(), WorkflowError> {
    if planned.contains_key(package_id) {
        return Ok(());
    }
    if depth > MAX_WORKFLOW_PACKAGE_DEPTH || !visiting.insert(package_id.to_string()) {
        return Err(authoring_error(
            "package_closure.imports",
            "package import graph is cyclic or exceeds its depth bound",
        ));
    }
    let source = sources.get(package_id).ok_or_else(|| {
        authoring_error(
            "package_closure.imports",
            format!("imported package '{package_id}' is absent from the closure"),
        )
    })?;
    let dependencies = source
        .imports
        .iter()
        .filter(|import| sources.contains_key(import.package_id.as_str()))
        .map(|import| import.package_id.as_str())
        .collect::<BTreeSet<_>>();
    for dependency in dependencies {
        visit_workflow_package_closure(
            dependency,
            depth + 1,
            sources,
            visiting,
            planned,
            order,
            catalog,
        )?;
    }

    let mut resolved = (*source).clone();
    for import in &mut resolved.imports {
        if let Some(imported) = planned.get(&import.package_id) {
            let imported_source = sources
                .get(import.package_id.as_str())
                .expect("planned package has closure source");
            let member_id = imported_source.exports.get(&import.export).ok_or_else(|| {
                authoring_error(
                    "package_closure.imports.export",
                    format!(
                        "package '{}' has no export '{}'",
                        import.package_id, import.export
                    ),
                )
            })?;
            let member = imported
                .members
                .iter()
                .find(|member| &member.member_id == member_id)
                .ok_or_else(|| {
                    authoring_error(
                        "package_closure.imports.export",
                        "imported export member is absent from its package plan",
                    )
                })?;
            let target = WorkflowCallTarget::Definition {
                identity: member.definition_identity.clone(),
            };
            let digest = digest_serializable(&imported.lock)?;
            if import
                .target
                .as_ref()
                .is_some_and(|existing| existing != &target)
                || import
                    .package_lock_digest_sha256
                    .as_ref()
                    .is_some_and(|existing| existing != &digest)
            {
                return Err(authoring_error(
                    "package_closure.imports",
                    "exact imported package facts drift from the resolved source closure",
                ));
            }
            import.target = Some(target);
            import.package_lock_digest_sha256 = Some(digest);
        } else if import.target.is_none() {
            return Err(authoring_error(
                "package_closure.imports",
                format!(
                    "imported package '{}' is absent and has no exact published target",
                    import.package_id
                ),
            ));
        }
    }
    let plan = plan_workflow_package(&resolved, catalog)?;
    for member in &plan.members {
        catalog.workflow_definitions.insert(
            member.definition_identity.definition_id.clone(),
            member.lowering.document.definition.clone(),
        );
    }
    visiting.remove(package_id);
    order.push(package_id.to_string());
    planned.insert(package_id.to_string(), plan);
    Ok(())
}

/// Resolve and plan one complete bounded transitive package closure from an explicit entry.
///
/// Imported packages are planned before their importers. Each source-authored import is replaced
/// with the exact exported definition identity and imported lock digest before ordinary package
/// planning. Exact import facts already present in source must match the resolved package and never
/// cause silent relocking.
///
/// # Errors
///
/// Returns an error for unsupported closure versions, missing or duplicate packages, package
/// cycles, excessive depth/bytes/package count, missing exports, stale exact import facts, or any
/// package planning failure.
#[allow(clippy::too_many_lines)]
pub fn plan_workflow_package_closure(
    closure: &WorkflowPackageClosure,
    catalog: &WorkflowAuthoringCatalogSnapshot,
) -> Result<WorkflowPackageClosurePlan, WorkflowError> {
    if closure.version != WORKFLOW_PACKAGE_CLOSURE_VERSION {
        return Err(authoring_error(
            "package_closure.version",
            format!(
                "unsupported workflow package closure version {}; expected {WORKFLOW_PACKAGE_CLOSURE_VERSION}",
                closure.version
            ),
        ));
    }
    validate_authoring_id(
        "package_closure.entry_package_id",
        &closure.entry_package_id,
    )?;
    if closure.packages.is_empty() || closure.packages.len() > MAX_WORKFLOW_PACKAGE_CLOSURE_PACKAGES
    {
        return Err(authoring_error(
            "package_closure.packages",
            format!(
                "package closures require 1..={MAX_WORKFLOW_PACKAGE_CLOSURE_PACKAGES} packages"
            ),
        ));
    }
    catalog.validate()?;
    let mut sources = BTreeMap::new();
    let mut source_names = BTreeSet::new();
    let mut total_bytes = 0_usize;
    let mut total_edges = 0_usize;
    for (index, source) in closure.packages.iter().enumerate() {
        validate_authoring_id(
            &format!("package_closure.packages[{index}].package_id"),
            &source.package_id,
        )?;
        if source.package_id != source.manifest.package_id
            || sources
                .insert(source.package_id.as_str(), &source.manifest)
                .is_some()
            || source
                .source_name
                .as_ref()
                .is_some_and(|name| !source_names.insert(name.as_str()))
        {
            return Err(authoring_error(
                format!("package_closure.packages[{index}]"),
                "closure package identities must be unique and match their manifests",
            ));
        }
        source.manifest.validate()?;
        total_edges = total_edges
            .checked_add(
                source.manifest.imports.len()
                    + source
                        .manifest
                        .members
                        .iter()
                        .map(|member| {
                            member.dependencies.len() + member.external_dependencies.len()
                        })
                        .sum::<usize>(),
            )
            .ok_or_else(|| {
                authoring_error("package_closure.packages", "closure edge count overflow")
            })?;
        total_bytes = total_bytes
            .checked_add(
                source
                    .manifest
                    .members
                    .iter()
                    .map(|member| member.source.len())
                    .sum::<usize>(),
            )
            .ok_or_else(|| {
                authoring_error(
                    "package_closure.packages",
                    "closure source byte count overflow",
                )
            })?;
    }
    if total_edges > MAX_WORKFLOW_PACKAGE_EDGES {
        return Err(authoring_error(
            "package_closure.packages",
            format!("transitive package graph exceeds {MAX_WORKFLOW_PACKAGE_EDGES} edges"),
        ));
    }
    if total_bytes > MAX_WORKFLOW_PACKAGE_SOURCE_BYTES {
        return Err(authoring_error(
            "package_closure.packages",
            format!("transitive package source exceeds {MAX_WORKFLOW_PACKAGE_SOURCE_BYTES} bytes"),
        ));
    }
    if !sources.contains_key(closure.entry_package_id.as_str()) {
        return Err(authoring_error(
            "package_closure.entry_package_id",
            "entry package is absent from the closure",
        ));
    }

    let mut planned = BTreeMap::new();
    let mut order = Vec::new();
    let mut visiting = BTreeSet::new();
    let mut resolved_catalog = catalog.clone();
    visit_workflow_package_closure(
        &closure.entry_package_id,
        1,
        &sources,
        &mut visiting,
        &mut planned,
        &mut order,
        &mut resolved_catalog,
    )?;
    if planned.len() != sources.len() {
        return Err(authoring_error(
            "package_closure.packages",
            "package closure contains unreachable packages outside the entry import graph",
        ));
    }
    let packages = order
        .into_iter()
        .map(|package_id| {
            let plan = planned.remove(&package_id).ok_or_else(|| {
                authoring_error(
                    "package_closure.packages",
                    "closure order references an absent planned package",
                )
            })?;
            Ok(WorkflowPackageClosurePlanEntry { package_id, plan })
        })
        .collect::<Result<Vec<_>, WorkflowError>>()?;
    Ok(WorkflowPackageClosurePlan {
        version: WORKFLOW_PACKAGE_CLOSURE_VERSION,
        entry_package_id: closure.entry_package_id.clone(),
        packages,
    })
}

fn qualify_workflow_package_member_error(
    member: &WorkflowPackageMember,
    error: WorkflowError,
) -> WorkflowError {
    match error {
        WorkflowError::Build { path, message } => authoring_error(
            format!("package.members.{}.source.{path}", member.member_id),
            message,
        ),
        other => other,
    }
}

fn digest_serializable(value: &impl Serialize) -> Result<String, WorkflowError> {
    let encoded = serde_json::to_vec(value).map_err(|error| {
        authoring_error(
            "package.digest",
            format!("value cannot be serialized: {error}"),
        )
    })?;
    let digest = Sha256::digest(encoded);
    let mut output = String::with_capacity(64);
    for byte in digest {
        write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(output)
}

fn workflow_package_topological_order<'a>(
    members: &BTreeMap<&'a str, &'a WorkflowPackageMember>,
) -> Result<Vec<&'a str>, WorkflowError> {
    fn visit<'a>(
        id: &'a str,
        members: &BTreeMap<&'a str, &'a WorkflowPackageMember>,
        visiting: &mut BTreeSet<&'a str>,
        complete: &mut BTreeSet<&'a str>,
        output: &mut Vec<&'a str>,
    ) -> Result<(), WorkflowError> {
        if complete.contains(id) {
            return Ok(());
        }
        if !visiting.insert(id) {
            return Err(authoring_error(
                "package.members.dependencies",
                "package dependency graph is cyclic",
            ));
        }
        for dependency in &members[id].dependencies {
            visit(dependency, members, visiting, complete, output)?;
        }
        visiting.remove(id);
        complete.insert(id);
        output.push(id);
        Ok(())
    }
    let mut visiting = BTreeSet::new();
    let mut complete = BTreeSet::new();
    let mut output = Vec::with_capacity(members.len());
    for id in members.keys().copied() {
        visit(id, members, &mut visiting, &mut complete, &mut output)?;
    }
    Ok(output)
}

fn workflow_package_member_closure(
    member: &WorkflowPackageMember,
    members: &BTreeMap<&str, &WorkflowPackageMember>,
) -> Result<Vec<String>, WorkflowError> {
    fn collect(
        id: &str,
        members: &BTreeMap<&str, &WorkflowPackageMember>,
        closure: &mut BTreeSet<String>,
    ) -> Result<(), WorkflowError> {
        let member = members.get(id).ok_or_else(|| {
            authoring_error(
                "package.members.dependencies",
                format!("missing member '{id}'"),
            )
        })?;
        for dependency in &member.dependencies {
            if closure.insert(dependency.clone()) {
                collect(dependency, members, closure)?;
            }
        }
        Ok(())
    }
    let mut closure = BTreeSet::new();
    for dependency in &member.dependencies {
        closure.insert(dependency.clone());
        collect(dependency, members, &mut closure)?;
    }
    Ok(closure.into_iter().collect())
}

#[allow(clippy::too_many_lines)]
fn resolve_package_calls(
    value: &mut serde_json::Value,
    member: &WorkflowPackageMember,
    identities: &BTreeMap<String, WorkflowDefinitionIdentity>,
    external_targets: &BTreeMap<String, WorkflowCallTarget>,
) -> Result<(), WorkflowError> {
    let Some(steps) = value
        .get_mut("steps")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return Ok(());
    };
    for (index, step) in steps.iter_mut().enumerate() {
        let Some(object) = step.as_object_mut() else {
            continue;
        };
        let Some(call) = object.remove("package_call") else {
            continue;
        };
        let call = call.as_object().ok_or_else(|| {
            authoring_error(
                format!(
                    "package.members.{}.steps[{index}].package_call",
                    member.member_id
                ),
                "package_call must be an object",
            )
        })?;
        if call.len() != 1 {
            return Err(authoring_error(
                format!(
                    "package.members.{}.steps[{index}].package_call",
                    member.member_id
                ),
                "package_call accepts exactly one of member or external",
            ));
        }
        let target = if let Some(target) = call.get("member").and_then(serde_json::Value::as_str) {
            if !member
                .dependencies
                .iter()
                .any(|dependency| dependency == target)
            {
                return Err(authoring_error(
                    format!(
                        "package.members.{}.steps[{index}].package_call.member",
                        member.member_id
                    ),
                    "package_call target must be a declared direct dependency",
                ));
            }
            let identity = identities.get(target).ok_or_else(|| {
                authoring_error(
                    format!(
                        "package.members.{}.steps[{index}].package_call.member",
                        member.member_id
                    ),
                    "package_call target has not compiled successfully",
                )
            })?;
            WorkflowCallTarget::Definition {
                identity: identity.clone(),
            }
        } else if let Some(target) = call.get("external").and_then(serde_json::Value::as_str) {
            if !member
                .external_dependencies
                .iter()
                .any(|dependency| dependency == target)
            {
                return Err(authoring_error(
                    format!(
                        "package.members.{}.steps[{index}].package_call.external",
                        member.member_id
                    ),
                    "package_call external target must be a declared direct dependency",
                ));
            }
            external_targets.get(target).cloned().ok_or_else(|| {
                authoring_error(
                    format!(
                        "package.members.{}.steps[{index}].package_call.external",
                        member.member_id
                    ),
                    "package_call external target is unavailable",
                )
            })?
        } else {
            return Err(authoring_error(
                format!(
                    "package.members.{}.steps[{index}].package_call",
                    member.member_id
                ),
                "package_call requires a string member or external field",
            ));
        };
        object.insert(
            "workflow_call".to_string(),
            serde_json::to_value(WorkflowCallConfiguration {
                version: WORKFLOW_CALL_VERSION,
                target,
                input: None,
                output: None,
            })
            .map_err(|error| authoring_error("package_call", error.to_string()))?,
        );
    }
    Ok(())
}

/// Current portable package mutation contract version.
pub const WORKFLOW_PACKAGE_MUTATION_VERSION: u32 = 1;

/// Optimistic mutable-generation fact for one existing package member draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackageExpectedGeneration {
    pub member_id: String,
    pub expected_generation: u64,
}

/// Atomic package application request over one previously validated pure plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackageApplyRequest {
    pub version: u32,
    pub plan: WorkflowPackagePlan,
    /// Exact expected generations for existing members; omitted members must not already exist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_generations: Vec<WorkflowPackageExpectedGeneration>,
}

/// Atomic package publication request over successfully applied canonical drafts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackagePublishRequest {
    pub version: u32,
    pub package_id: String,
    /// Exact applied lock candidate whose member definitions must still match canonical drafts.
    pub expected_lock: WorkflowPackageLock,
    /// Exact draft generations to publish in one transaction.
    pub expected_generations: Vec<WorkflowPackageExpectedGeneration>,
}

/// Stable package mutation outcome. `Conflict` and `Rejected` never imply partial success.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowPackageMutationOutcome {
    Applied,
    Published,
    Conflict,
    Rejected,
}

/// One member result returned only as part of an authoritative package mutation result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackageMutationMemberResult {
    pub member_id: String,
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<WorkflowRevisionIdentity>,
    pub definition_identity: WorkflowDefinitionIdentity,
}

/// Typed atomic package mutation result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackageMutationResult {
    pub version: u32,
    pub package_id: String,
    pub outcome: WorkflowPackageMutationOutcome,
    /// Empty on conflict/rejection; complete and identity-ordered on success.
    pub members: Vec<WorkflowPackageMutationMemberResult>,
    /// Generated only from a complete successful canonical result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<WorkflowPackageLock>,
    /// Bounded normalized conflict/rejection diagnostics.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<WorkflowValidationDiagnostic>,
}

impl WorkflowPackageApplyRequest {
    /// Validate optimistic package apply facts without performing mutation.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, invalid plans, duplicate/unknown member facts,
    /// zero generations, or lock/plan inconsistency.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        validate_package_mutation_version(self.version, "package_apply.version")?;
        validate_workflow_package_plan(&self.plan)?;
        validate_expected_package_generations(&self.plan, &self.expected_generations)
    }
}

impl WorkflowPackagePublishRequest {
    /// Validate optimistic package publication facts without performing mutation.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, malformed package/lock identities, duplicate or
    /// incomplete generation facts, or zero generations.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        validate_package_mutation_version(self.version, "package_publish.version")?;
        validate_authoring_id("package_publish.package_id", &self.package_id)?;
        self.expected_lock.validate()?;
        if self.expected_lock.package_id != self.package_id {
            return Err(authoring_error(
                "package_publish.expected_lock.package_id",
                "publication package and lock identities must match",
            ));
        }
        let expected_ids = self
            .expected_lock
            .members
            .iter()
            .map(|member| member.member_id.as_str())
            .collect::<BTreeSet<_>>();
        let actual_ids = validate_generation_facts(&self.expected_generations)?;
        if actual_ids != expected_ids {
            return Err(authoring_error(
                "package_publish.expected_generations",
                "publication requires one exact expected generation for every locked member",
            ));
        }
        Ok(())
    }
}

impl WorkflowPackageMutationResult {
    /// Validate success/conflict atomicity and result identity.
    ///
    /// # Errors
    ///
    /// Returns an error when success lacks complete ordered results/lock, or a non-success outcome
    /// carries partial mutation facts.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        validate_package_mutation_version(self.version, "package_result.version")?;
        validate_authoring_id("package_result.package_id", &self.package_id)?;
        let success = matches!(
            self.outcome,
            WorkflowPackageMutationOutcome::Applied | WorkflowPackageMutationOutcome::Published
        );
        if success {
            let lock = self.lock.as_ref().ok_or_else(|| {
                authoring_error(
                    "package_result.lock",
                    "successful package result requires a lock",
                )
            })?;
            lock.validate()?;
            if lock.package_id != self.package_id
                || self.members.len() != lock.members.len()
                || !self
                    .members
                    .windows(2)
                    .all(|pair| pair[0].member_id < pair[1].member_id)
                || self.members.iter().any(|member| member.generation == 0)
            {
                return Err(authoring_error(
                    "package_result.members",
                    "successful package result must be complete, ordered, nonzero, and lock-matched",
                ));
            }
            for (member, locked) in self.members.iter().zip(&lock.members) {
                if member.member_id != locked.member_id
                    || member.definition_identity != locked.definition_identity
                {
                    return Err(authoring_error(
                        "package_result.members",
                        "successful member facts do not match the generated lock",
                    ));
                }
            }
        } else if !self.members.is_empty() || self.lock.is_some() {
            return Err(authoring_error(
                "package_result",
                "conflict/rejected package results must not expose partial mutation facts",
            ));
        }
        Ok(())
    }
}

fn validate_package_mutation_version(version: u32, path: &str) -> Result<(), WorkflowError> {
    if version != WORKFLOW_PACKAGE_MUTATION_VERSION {
        return Err(authoring_error(
            path,
            format!("unsupported package mutation version {version}"),
        ));
    }
    Ok(())
}

fn validate_generation_facts(
    facts: &[WorkflowPackageExpectedGeneration],
) -> Result<BTreeSet<&str>, WorkflowError> {
    let mut ids = BTreeSet::new();
    for fact in facts {
        validate_authoring_id("package.expected_generations.member_id", &fact.member_id)?;
        if fact.expected_generation == 0 || !ids.insert(fact.member_id.as_str()) {
            return Err(authoring_error(
                "package.expected_generations",
                "expected generations must be nonzero and unique by member",
            ));
        }
    }
    Ok(ids)
}

fn validate_expected_package_generations(
    plan: &WorkflowPackagePlan,
    facts: &[WorkflowPackageExpectedGeneration],
) -> Result<(), WorkflowError> {
    let known = plan
        .members
        .iter()
        .map(|member| member.member_id.as_str())
        .collect::<BTreeSet<_>>();
    let actual = validate_generation_facts(facts)?;
    if !actual.is_subset(&known) {
        return Err(authoring_error(
            "package_apply.expected_generations",
            "expected generation references an unknown package member",
        ));
    }
    Ok(())
}

fn validate_workflow_package_plan(plan: &WorkflowPackagePlan) -> Result<(), WorkflowError> {
    if plan.version != WORKFLOW_PACKAGE_PLAN_VERSION
        || plan.package_id != plan.lock.package_id
        || plan.members.len() != plan.lock.members.len()
        || plan.members.is_empty()
    {
        return Err(authoring_error(
            "package_plan",
            "package plan version, identity, or member inventory is inconsistent",
        ));
    }
    plan.lock.validate()?;
    let locked = plan
        .lock
        .members
        .iter()
        .map(|member| (member.member_id.as_str(), member))
        .collect::<BTreeMap<_, _>>();
    let mut seen = BTreeSet::new();
    for member in &plan.members {
        if member.member_source_map.version != WORKFLOW_PACKAGE_MEMBER_SOURCE_MAP_VERSION
            || member.member_source_map.member_id != member.member_id
            || member.member_source_map.source_name.trim().is_empty()
            || member.member_source_map.source_map != member.lowering.source_map
            || !seen.insert(member.member_id.as_str())
            || locked.get(member.member_id.as_str()).is_none_or(|lock| {
                lock.definition_identity != member.definition_identity
                    || lock.dependency_closure != member.dependency_closure
            })
        {
            return Err(authoring_error(
                "package_plan.members",
                "planned members must be unique and match the lock candidate",
            ));
        }
    }
    Ok(())
}

/// Current deterministic workflow package lock/result version.
pub const WORKFLOW_PACKAGE_LOCK_VERSION: u32 = 4;
/// Current durable package publication receipt version.
pub const WORKFLOW_PACKAGE_PUBLICATION_RECEIPT_VERSION: u32 = 1;

/// One exact imported package/export lock fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackageLockedImport {
    pub import_id: String,
    pub package_id: String,
    pub export: String,
    pub package_lock_digest_sha256: String,
    pub target: WorkflowCallTarget,
}

impl WorkflowPackageLockedImport {
    fn validate(&self, path: &str) -> Result<(), WorkflowError> {
        WorkflowPackageImport {
            import_id: self.import_id.clone(),
            package_id: self.package_id.clone(),
            export: self.export.clone(),
            manifest: None,
            target: Some(self.target.clone()),
            package_lock_digest_sha256: Some(self.package_lock_digest_sha256.clone()),
        }
        .validate(path)
    }
}

/// One exact successfully compiled/published package member result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackageLockedMember {
    /// Stable package-local member identity.
    pub member_id: String,
    /// Digest of exact source format/name/bytes and declared dependency identities.
    pub source_digest_sha256: String,
    /// Digest of the exact canonical executable definition bytes.
    pub executable_digest_sha256: String,
    /// Digest-derived canonical executable definition identity.
    pub definition_identity: WorkflowDefinitionIdentity,
    /// Exact published authored revision, when publication was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_revision: Option<WorkflowRevisionIdentity>,
    /// Package-local dependency closure in deterministic child-before-parent order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependency_closure: Vec<String>,
}

/// Portable package/export identity resolved to exact publication facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackageExportIdentity {
    pub package_id: String,
    pub export: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_lock_digest_sha256: Option<String>,
}

impl WorkflowPackageExportIdentity {
    /// Validate portable package/export selection facts.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed package/export identities or lock digests.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        validate_authoring_id("package_export.package_id", &self.package_id)?;
        validate_authoring_id("package_export.export", &self.export)?;
        if let Some(digest) = &self.package_lock_digest_sha256 {
            validate_sha256("package_export.package_lock_digest_sha256", digest)?;
        }
        Ok(())
    }
}

/// One named exported workflow locked to an exact package member and immutable identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackageLockedExport {
    pub export: String,
    pub member_id: String,
    pub definition_identity: WorkflowDefinitionIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_revision: Option<WorkflowRevisionIdentity>,
}

/// Durable bounded publication receipt for one package and its public exports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackagePublicationReceipt {
    pub version: u32,
    pub package_id: String,
    pub package_lock_digest_sha256: String,
    pub published_at_ms: u64,
    pub exports: Vec<WorkflowPackageLockedExport>,
}

impl WorkflowPackagePublicationReceipt {
    /// Validate receipt identity, ordering, digests, and exact published export facts.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, malformed identities/digests, absent exports,
    /// duplicate ordering, or definition/revision identity mismatch.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if self.version != WORKFLOW_PACKAGE_PUBLICATION_RECEIPT_VERSION {
            return Err(authoring_error(
                "package_receipt.version",
                "unsupported package publication receipt version",
            ));
        }
        validate_authoring_id("package_receipt.package_id", &self.package_id)?;
        validate_sha256(
            "package_receipt.package_lock_digest_sha256",
            &self.package_lock_digest_sha256,
        )?;
        if self.published_at_ms == 0
            || self.exports.is_empty()
            || self.exports.len() > MAX_WORKFLOW_PACKAGE_MEMBERS
            || !self
                .exports
                .windows(2)
                .all(|pair| pair[0].export < pair[1].export)
        {
            return Err(authoring_error(
                "package_receipt.exports",
                "package receipt requires bounded identity-ordered exports and a timestamp",
            ));
        }
        for export in &self.exports {
            validate_authoring_id("package_receipt.exports.export", &export.export)?;
            validate_authoring_id("package_receipt.exports.member_id", &export.member_id)?;
            if export.definition_identity.definition_id.trim().is_empty()
                || export.definition_identity.definition_version == 0
            {
                return Err(authoring_error(
                    "package_receipt.exports.definition_identity",
                    "export definition identity is malformed",
                ));
            }
            let revision = export.published_revision.as_ref().ok_or_else(|| {
                authoring_error(
                    "package_receipt.exports.published_revision",
                    "published package exports require an exact authored revision",
                )
            })?;
            if revision.revision == 0 || revision.workflow_id != export.definition_identity.kind {
                return Err(authoring_error(
                    "package_receipt.exports.published_revision",
                    "export revision must match its definition logical identity",
                ));
            }
        }
        Ok(())
    }
}

/// Deterministic reproducibility result generated only from successful canonical outcomes.
///
/// This contract is never runtime authority and does not authorize publication or execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackageLock {
    /// Must equal [`WORKFLOW_PACKAGE_LOCK_VERSION`].
    pub version: u32,
    /// Stable package identity.
    pub package_id: String,
    /// Digest of the validated package source manifest.
    pub package_source_digest_sha256: String,
    /// Exact imported package/export bindings in deterministic local identity order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<WorkflowPackageLockedImport>,
    /// Named exports locked to exact members in deterministic export-name order.
    pub exports: Vec<WorkflowPackageLockedExport>,
    /// Members ordered deterministically by package-local identity.
    pub members: Vec<WorkflowPackageLockedMember>,
}

impl WorkflowPackageLock {
    /// Return the deterministic canonical SHA-256 identity of this exact lock.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock cannot be canonically encoded.
    pub fn digest_sha256(&self) -> Result<String, WorkflowError> {
        digest_serializable(self)
    }

    /// Validate lock identity, digests, exact definition/revision facts, and dependency closure.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, malformed digests/identities, duplicate or
    /// unsorted members/closures, missing closure members, or inconsistent published revisions.
    #[allow(clippy::too_many_lines)]
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if self.version != WORKFLOW_PACKAGE_LOCK_VERSION {
            return Err(authoring_error(
                "package_lock.version",
                format!(
                    "unsupported workflow package lock version {}; expected {WORKFLOW_PACKAGE_LOCK_VERSION}",
                    self.version
                ),
            ));
        }
        validate_authoring_id("package_lock.package_id", &self.package_id)?;
        validate_sha256(
            "package_lock.package_source_digest_sha256",
            &self.package_source_digest_sha256,
        )?;
        let import_ids = self
            .imports
            .iter()
            .map(|import| import.import_id.as_str())
            .collect::<Vec<_>>();
        if !import_ids.windows(2).all(|pair| pair[0] < pair[1]) {
            return Err(authoring_error(
                "package_lock.imports",
                "locked imports must be unique and ordered by local identity",
            ));
        }
        for (index, import) in self.imports.iter().enumerate() {
            import.validate(&format!("package_lock.imports[{index}]"))?;
        }
        if self.exports.is_empty()
            || self.exports.len() > MAX_WORKFLOW_PACKAGE_MEMBERS
            || !self
                .exports
                .windows(2)
                .all(|pair| pair[0].export < pair[1].export)
        {
            return Err(authoring_error(
                "package_lock.exports",
                "locked exports must be bounded, unique, and ordered by export identity",
            ));
        }
        if self.members.is_empty() || self.members.len() > MAX_WORKFLOW_PACKAGE_MEMBERS {
            return Err(authoring_error(
                "package_lock.members",
                "package lock must contain a bounded non-empty member list",
            ));
        }
        let ids = self
            .members
            .iter()
            .map(|member| member.member_id.as_str())
            .collect::<Vec<_>>();
        if !ids.windows(2).all(|pair| pair[0] < pair[1]) {
            return Err(authoring_error(
                "package_lock.members",
                "locked members must be unique and ordered by member identity",
            ));
        }
        let known = ids.iter().copied().collect::<BTreeSet<_>>();
        for (index, export) in self.exports.iter().enumerate() {
            validate_authoring_id(
                &format!("package_lock.exports[{index}].export"),
                &export.export,
            )?;
            validate_authoring_id(
                &format!("package_lock.exports[{index}].member_id"),
                &export.member_id,
            )?;
            if export.definition_identity.definition_id.trim().is_empty()
                || export.definition_identity.definition_version == 0
            {
                return Err(authoring_error(
                    format!("package_lock.exports[{index}].definition_identity"),
                    "locked export definition identity is malformed",
                ));
            }
            let member = self
                .members
                .iter()
                .find(|member| member.member_id == export.member_id)
                .ok_or_else(|| {
                    authoring_error(
                        format!("package_lock.exports[{index}].member_id"),
                        "locked export references an unknown member",
                    )
                })?;
            if member.definition_identity != export.definition_identity
                || member.published_revision != export.published_revision
            {
                return Err(authoring_error(
                    format!("package_lock.exports[{index}]"),
                    "locked export facts must exactly match their member",
                ));
            }
        }
        for (index, member) in self.members.iter().enumerate() {
            validate_authoring_id(
                &format!("package_lock.members[{index}].member_id"),
                &member.member_id,
            )?;
            validate_sha256(
                &format!("package_lock.members[{index}].source_digest_sha256"),
                &member.source_digest_sha256,
            )?;
            validate_sha256(
                &format!("package_lock.members[{index}].executable_digest_sha256"),
                &member.executable_digest_sha256,
            )?;
            if member.definition_identity.definition_id.trim().is_empty()
                || member.definition_identity.definition_version == 0
            {
                return Err(authoring_error(
                    format!("package_lock.members[{index}].definition_identity"),
                    "locked definition identity is malformed",
                ));
            }
            if member.published_revision.as_ref().is_some_and(|revision| {
                revision.revision == 0 || revision.workflow_id != member.definition_identity.kind
            }) {
                return Err(authoring_error(
                    format!("package_lock.members[{index}].published_revision"),
                    "published revision must be nonzero and match the definition logical identity",
                ));
            }
            if !member
                .dependency_closure
                .windows(2)
                .all(|pair| pair[0] < pair[1])
                || member.dependency_closure.iter().any(|dependency| {
                    dependency == &member.member_id || !known.contains(dependency.as_str())
                })
            {
                return Err(authoring_error(
                    format!("package_lock.members[{index}].dependency_closure"),
                    "dependency closure must be unique, ordered, known, and exclude the member",
                ));
            }
        }
        Ok(())
    }
}

fn validate_sha256(path: &str, value: &str) -> Result<(), WorkflowError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(authoring_error(
            path,
            "SHA-256 digest must be 64 lowercase hexadecimal bytes",
        ));
    }
    Ok(())
}

/// One exact imported package export made available to a source package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackageImport {
    /// Stable import-local identity referenced by member external dependency inventories.
    pub import_id: String,
    /// Stable imported package identity.
    pub package_id: String,
    /// Named imported export.
    pub export: String,
    /// Optional confined relative manifest path used by source resolvers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<String>,
    /// Exact immutable target selected for that export after closure resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<WorkflowCallTarget>,
    /// Exact imported package lock digest used to detect stale or silently relocked imports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_lock_digest_sha256: Option<String>,
}

impl WorkflowPackageImport {
    fn validate(&self, path: &str) -> Result<(), WorkflowError> {
        validate_authoring_id(&format!("{path}.import_id"), &self.import_id)?;
        validate_authoring_id(&format!("{path}.package_id"), &self.package_id)?;
        validate_authoring_id(&format!("{path}.export"), &self.export)?;
        if let Some(manifest) = &self.manifest {
            validate_package_import_source_name(path, manifest)?;
        }
        if self.target.is_some() != self.package_lock_digest_sha256.is_some() {
            return Err(authoring_error(
                path,
                "package import target and lock digest must either both be absent or both be present",
            ));
        }
        if let Some(target) = &self.target {
            target.validate()?;
        }
        if let Some(digest) = &self.package_lock_digest_sha256 {
            validate_sha256(&format!("{path}.package_lock_digest_sha256"), digest)?;
        }
        Ok(())
    }
}

/// One bounded package source member supplied through a portable boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackageMember {
    /// Stable package-local identity used by exports and dependencies.
    pub member_id: String,
    /// Client-relative diagnostic name; hosts never interpret it as a local path.
    pub source_name: String,
    /// Exact source format selected by the client.
    pub format: WorkflowSourceFormat,
    /// Bounded source payload. This is an authoring input, never canonical runtime state.
    pub source: String,
    /// Package-local members that must be compiled before this member.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<String>,
    /// Exact manifest-level external dependencies callable directly by this member.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub external_dependencies: Vec<String>,
}

/// Minimal bounded source package manifest transported across application boundaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPackageManifest {
    /// Must equal [`WORKFLOW_PACKAGE_MANIFEST_VERSION`].
    pub version: u32,
    /// Stable package identity.
    pub package_id: String,
    /// Named exports mapped to package-local member identities.
    pub exports: BTreeMap<String, String>,
    /// Optional exact immutable dependencies supplied outside this package.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub external_dependencies: BTreeMap<String, WorkflowCallTarget>,
    /// Exact imported package exports selected by bounded source resolution.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<WorkflowPackageImport>,
    /// Complete bounded member inventory.
    pub members: Vec<WorkflowPackageMember>,
}

impl WorkflowPackageManifest {
    /// Validate package identities, bounds, references, and dependency acyclicity.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, malformed/duplicate identities, excessive
    /// bytes/members/dependencies/depth, missing dependencies or exports, and dependency cycles.
    #[allow(clippy::too_many_lines)]
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if self.version != WORKFLOW_PACKAGE_MANIFEST_VERSION {
            return Err(authoring_error(
                "package.version",
                format!(
                    "unsupported workflow package version {}; expected {WORKFLOW_PACKAGE_MANIFEST_VERSION}",
                    self.version
                ),
            ));
        }
        validate_authoring_id("package.package_id", &self.package_id)?;
        if self.members.is_empty() || self.members.len() > MAX_WORKFLOW_PACKAGE_MEMBERS {
            return Err(authoring_error(
                "package.members",
                format!("packages require 1..={MAX_WORKFLOW_PACKAGE_MEMBERS} members"),
            ));
        }
        if self.exports.is_empty() || self.exports.len() > MAX_WORKFLOW_PACKAGE_MEMBERS {
            return Err(authoring_error(
                "package.exports",
                "packages require a bounded non-empty export map",
            ));
        }
        if self
            .external_dependencies
            .len()
            .saturating_add(self.imports.len())
            > MAX_WORKFLOW_PACKAGE_MEMBERS
        {
            return Err(authoring_error(
                "package.external_dependencies",
                "package external dependencies exceed the package bound",
            ));
        }
        for (name, target) in &self.external_dependencies {
            validate_authoring_id("package.external_dependencies.name", name)?;
            target.validate()?;
        }
        let mut imported_ids = BTreeSet::new();
        let mut imported_packages = BTreeSet::new();
        for (index, import) in self.imports.iter().enumerate() {
            import.validate(&format!("package.imports[{index}]"))?;
            if !imported_ids.insert(import.import_id.as_str())
                || !imported_packages.insert((import.package_id.as_str(), import.export.as_str()))
                || self.external_dependencies.contains_key(&import.import_id)
            {
                return Err(authoring_error(
                    format!("package.imports[{index}]"),
                    "package imports must have unique local identities and package/export bindings",
                ));
            }
        }
        let imported_targets = self
            .imports
            .iter()
            .map(|import| (import.import_id.as_str(), import.target.as_ref()))
            .collect::<BTreeMap<_, _>>();
        let mut by_id = BTreeMap::new();
        let mut source_names = BTreeSet::new();
        let mut total_bytes = 0_usize;
        let mut total_edges = 0_usize;
        let mut total_external_edges = 0_usize;
        for (index, member) in self.members.iter().enumerate() {
            validate_authoring_id(
                &format!("package.members[{index}].member_id"),
                &member.member_id,
            )?;
            validate_package_source_name(index, &member.source_name)?;
            total_bytes = total_bytes
                .checked_add(member.source.len())
                .ok_or_else(|| {
                    authoring_error("package.members", "package source byte count overflow")
                })?;
            total_edges = total_edges
                .checked_add(member.dependencies.len())
                .ok_or_else(|| {
                    authoring_error("package.members", "package dependency edge count overflow")
                })?;
            total_external_edges = total_external_edges
                .checked_add(member.external_dependencies.len())
                .ok_or_else(|| {
                    authoring_error(
                        "package.members",
                        "package external dependency edge count overflow",
                    )
                })?;
            if member.source.is_empty()
                || member.dependencies.len() > MAX_WORKFLOW_PACKAGE_MEMBER_DEPENDENCIES
                || by_id.insert(member.member_id.as_str(), member).is_some()
                || !source_names.insert(member.source_name.as_str())
            {
                return Err(authoring_error(
                    format!("package.members[{index}]"),
                    "member source, dependency bound, identity, or source name is invalid/duplicate",
                ));
            }
            let unique = member.dependencies.iter().collect::<BTreeSet<_>>();
            if unique.len() != member.dependencies.len() {
                return Err(authoring_error(
                    format!("package.members[{index}].dependencies"),
                    "member dependencies must be unique",
                ));
            }
            if member.external_dependencies.len() > MAX_WORKFLOW_PACKAGE_MEMBER_DEPENDENCIES {
                return Err(authoring_error(
                    format!("package.members[{index}].external_dependencies"),
                    "member external dependencies exceed the direct dependency bound",
                ));
            }
            let unique_external = member.external_dependencies.iter().collect::<BTreeSet<_>>();
            if unique_external.len() != member.external_dependencies.len() {
                return Err(authoring_error(
                    format!("package.members[{index}].external_dependencies"),
                    "member external dependencies must be unique",
                ));
            }
            for dependency in &member.external_dependencies {
                validate_authoring_id(
                    &format!("package.members[{index}].external_dependencies"),
                    dependency,
                )?;
                if !self.external_dependencies.contains_key(dependency)
                    && !imported_targets.contains_key(dependency.as_str())
                {
                    return Err(authoring_error(
                        format!("package.members[{index}].external_dependencies"),
                        format!(
                            "external dependency references missing manifest target '{dependency}'"
                        ),
                    ));
                }
            }
        }
        if total_bytes > MAX_WORKFLOW_PACKAGE_SOURCE_BYTES {
            return Err(authoring_error(
                "package.members",
                format!("package source exceeds {MAX_WORKFLOW_PACKAGE_SOURCE_BYTES} bytes"),
            ));
        }
        if total_edges.saturating_add(total_external_edges) > MAX_WORKFLOW_PACKAGE_EDGES {
            return Err(authoring_error(
                "package.members.dependencies",
                format!("package dependency graph exceeds {MAX_WORKFLOW_PACKAGE_EDGES} edges"),
            ));
        }
        for (name, member_id) in &self.exports {
            validate_authoring_id("package.exports.name", name)?;
            if !by_id.contains_key(member_id.as_str()) {
                return Err(authoring_error(
                    format!("package.exports.{name}"),
                    format!("export references missing member '{member_id}'"),
                ));
            }
        }
        validate_workflow_package_dag(&by_id)
    }
}

fn validate_package_import_source_name(path: &str, value: &str) -> Result<(), WorkflowError> {
    let candidate = std::path::Path::new(value);
    let valid_extension = candidate
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| matches!(extension, "json" | "yaml" | "yml" | "toml"));
    if value.is_empty()
        || value.len() > 512
        || candidate.is_absolute()
        || candidate
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
        || !valid_extension
    {
        return Err(authoring_error(
            format!("{path}.manifest"),
            "import manifest must be a confined relative JSON/YAML/TOML path",
        ));
    }
    Ok(())
}

fn validate_package_source_name(index: usize, value: &str) -> Result<(), WorkflowError> {
    let path = std::path::Path::new(value);
    if value.is_empty()
        || value.len() > 512
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
        || WorkflowSourceFormat::from_file_name(value).is_err()
    {
        return Err(authoring_error(
            format!("package.members[{index}].source_name"),
            "source name must be a confined relative JSON/YAML/TOML diagnostic path",
        ));
    }
    Ok(())
}

fn validate_workflow_package_dag(
    members: &BTreeMap<&str, &WorkflowPackageMember>,
) -> Result<(), WorkflowError> {
    fn visit<'a>(
        id: &'a str,
        members: &BTreeMap<&'a str, &'a WorkflowPackageMember>,
        visiting: &mut BTreeSet<&'a str>,
        complete: &mut BTreeSet<&'a str>,
        depth: usize,
    ) -> Result<(), WorkflowError> {
        if complete.contains(id) {
            return Ok(());
        }
        if depth > MAX_WORKFLOW_PACKAGE_DEPTH || !visiting.insert(id) {
            return Err(authoring_error(
                "package.members.dependencies",
                "package dependency graph is cyclic or exceeds its depth bound",
            ));
        }
        let member = members.get(id).expect("validated package member");
        for dependency in &member.dependencies {
            if !members.contains_key(dependency.as_str()) {
                return Err(authoring_error(
                    format!("package.members.{id}.dependencies"),
                    format!("dependency references missing member '{dependency}'"),
                ));
            }
            visit(dependency, members, visiting, complete, depth + 1)?;
        }
        visiting.remove(id);
        complete.insert(id);
        Ok(())
    }
    let mut visiting = BTreeSet::new();
    let mut complete = BTreeSet::new();
    for id in members.keys().copied() {
        visit(id, members, &mut visiting, &mut complete, 1)?;
    }
    Ok(())
}

/// Portable source encoding for one authored workflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowSourceFormat {
    /// JavaScript Object Notation.
    Json,
    /// YAML 1.2 syntax restricted to JSON-compatible values.
    Yaml,
    /// TOML 1.x document syntax.
    Toml,
}

impl WorkflowSourceFormat {
    /// Infer one exact supported format from a source-file name.
    ///
    /// # Errors
    ///
    /// Returns an error when the file name has no supported JSON, YAML, or TOML suffix. The
    /// function never guesses by trying multiple parsers.
    pub fn from_file_name(file_name: &str) -> Result<Self, WorkflowError> {
        let normalized = file_name.to_ascii_lowercase();
        if normalized
            .strip_suffix(".json")
            .is_some_and(|stem| !stem.is_empty())
        {
            return Ok(Self::Json);
        }
        if normalized
            .strip_suffix(".yaml")
            .or_else(|| normalized.strip_suffix(".yml"))
            .is_some_and(|stem| !stem.is_empty())
        {
            return Ok(Self::Yaml);
        }
        if normalized
            .strip_suffix(".toml")
            .is_some_and(|stem| !stem.is_empty())
        {
            return Ok(Self::Toml);
        }
        Err(authoring_error(
            "source.format",
            "workflow source format requires an explicit format or a .json/.yaml/.yml/.toml file name",
        ))
    }
}

/// Decode one bounded canonical authored-workflow source document.
///
/// JSON, YAML, and TOML are adapters into the same [`WorkflowAuthoringDocument`]. No
/// format-specific value is retained after decoding.
///
/// # Errors
///
/// Returns a source-addressed error when the input exceeds the authoring document bound, contains
/// malformed syntax, uses unknown fields or unsupported versions, or fails workflow validation.
pub fn decode_workflow_authoring_source(
    source: &str,
    format: WorkflowSourceFormat,
) -> Result<WorkflowAuthoringDocument, WorkflowError> {
    let value = decode_workflow_source_value(source, format)?;
    let document: WorkflowAuthoringDocument = serde_json::from_value(value).map_err(|error| {
        authoring_error(
            workflow_source_format_path(format),
            format!("invalid canonical workflow document: {error}"),
        )
    })?;
    document.validate()?;
    Ok(document)
}

/// Decode and structurally select a concise or canonical source profile, then lower it to the
/// canonical authoring document.
///
/// # Errors
///
/// Returns an error for malformed or ambiguous source, unsupported versions, invalid concise
/// structure, unavailable actions, or an invalid lowered canonical document.
pub fn lower_workflow_authoring_source(
    source: &str,
    format: WorkflowSourceFormat,
    catalog: &WorkflowAuthoringCatalogSnapshot,
) -> Result<WorkflowSourceLoweringResult, WorkflowError> {
    let value = decode_workflow_source_value(source, format)?;
    let object = value
        .as_object()
        .ok_or_else(|| authoring_error("source", "workflow source document must be an object"))?;
    let concise = object.contains_key("workflow_source_version");
    let canonical = object.contains_key("schema_version");
    if concise == canonical {
        return Err(authoring_error(
            "source.profile",
            "workflow source must declare exactly one of workflow_source_version or schema_version",
        ));
    }
    let (profile, document, source_map) = if concise {
        let version = object
            .get("workflow_source_version")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                authoring_error(
                    "workflow_source_version",
                    "workflow source version must be an unsigned integer",
                )
            })?;
        if version != u64::from(WORKFLOW_SOURCE_DOCUMENT_VERSION) {
            return Err(authoring_error(
                "workflow_source_version",
                format!(
                    "unsupported workflow source version {version}; expected {WORKFLOW_SOURCE_DOCUMENT_VERSION}"
                ),
            ));
        }
        let source_document: WorkflowStructuredSourceDocument =
            serde_json::from_value(value.clone()).map_err(|error| {
                authoring_error(
                    "source.structured",
                    format!("invalid structured workflow source: {error}"),
                )
            })?;
        let (document, source_map) = source_document.lower(catalog)?;
        (WorkflowSourceProfile::Structured, document, source_map)
    } else {
        let document: WorkflowAuthoringDocument =
            serde_json::from_value(value).map_err(|error| {
                authoring_error(
                    "source.canonical",
                    format!("invalid canonical workflow source: {error}"),
                )
            })?;
        document.validate()?;
        (
            WorkflowSourceProfile::Canonical,
            document,
            WorkflowSourceMap {
                version: WORKFLOW_SOURCE_MAP_VERSION,
                entries: Vec::new(),
            },
        )
    };
    let validation = document.validation_report().remap_diagnostics(&source_map);
    if !validation.is_valid() {
        return Err(authoring_error(
            "source.lowered",
            validation
                .diagnostics
                .first()
                .map_or("lowered workflow is invalid", |diagnostic| {
                    diagnostic.message.as_str()
                }),
        ));
    }
    Ok(WorkflowSourceLoweringResult {
        version: WORKFLOW_SOURCE_LOWERING_VERSION,
        profile,
        document,
        source_map,
        validation,
    })
}

fn decode_workflow_source_value(
    source: &str,
    format: WorkflowSourceFormat,
) -> Result<serde_json::Value, WorkflowError> {
    if source.len() > MAX_WORKFLOW_AUTHORING_DOCUMENT_BYTES {
        return Err(authoring_error(
            "source",
            format!("workflow source exceeds {MAX_WORKFLOW_AUTHORING_DOCUMENT_BYTES} bytes"),
        ));
    }
    let value = match format {
        WorkflowSourceFormat::Json => {
            let mut deserializer = serde_json::Deserializer::from_str(source);
            let value = DuplicateRejectingJsonValue::deserialize(&mut deserializer)
                .map_err(|error| {
                    authoring_error(
                        "source.json",
                        format!(
                            "invalid JSON workflow source at line {}, column {}: {error}",
                            error.line(),
                            error.column()
                        ),
                    )
                })?
                .0;
            deserializer.end().map_err(|error| {
                authoring_error(
                    "source.json",
                    format!("invalid trailing JSON workflow source: {error}"),
                )
            })?;
            value
        }
        WorkflowSourceFormat::Yaml => decode_workflow_yaml_value(source)?,
        WorkflowSourceFormat::Toml => {
            let mut value: serde_json::Value = toml::from_str(source).map_err(|error| {
                let location = error.span().map_or_else(String::new, |span| {
                    format!(" at byte range {}..{}", span.start, span.end)
                });
                authoring_error(
                    "source.toml",
                    format!("invalid TOML workflow source{location}: {error}"),
                )
            })?;
            decode_workflow_toml_null_markers(&mut value, 0)?;
            value
        }
    };
    validate_authoring_json_value(workflow_source_format_path(format), &value)?;
    Ok(value)
}

const fn workflow_source_format_path(format: WorkflowSourceFormat) -> &'static str {
    match format {
        WorkflowSourceFormat::Json => "source.json",
        WorkflowSourceFormat::Yaml => "source.yaml",
        WorkflowSourceFormat::Toml => "source.toml",
    }
}

fn decode_workflow_yaml_value(source: &str) -> Result<serde_json::Value, WorkflowError> {
    reject_unsupported_yaml_syntax(source)?;
    let value: yaml_serde::Value = yaml_serde::from_str(source).map_err(|error| {
        authoring_error(
            "source.yaml",
            format!("invalid YAML workflow source: {error}"),
        )
    })?;
    yaml_value_to_json(value, "source.yaml", 0)
}

fn reject_unsupported_yaml_syntax(source: &str) -> Result<(), WorkflowError> {
    for (line_index, line) in source.lines().enumerate() {
        let content = line.split('#').next().unwrap_or_default();
        if content.contains("<<:")
            || content
                .split_whitespace()
                .any(|word| word.starts_with('&') || word.starts_with('*') || word.starts_with('!'))
        {
            return Err(authoring_error(
                format!("source.yaml.line_{}", line_index + 1),
                "YAML anchors, aliases, merge keys, and custom tags are unsupported",
            ));
        }
    }
    Ok(())
}

fn yaml_value_to_json(
    value: yaml_serde::Value,
    path: &str,
    depth: usize,
) -> Result<serde_json::Value, WorkflowError> {
    if depth > MAX_WORKFLOW_AUTHORING_JSON_DEPTH {
        return Err(authoring_error(
            path,
            format!("YAML value depth exceeds {MAX_WORKFLOW_AUTHORING_JSON_DEPTH}"),
        ));
    }
    match value {
        yaml_serde::Value::Null => Ok(serde_json::Value::Null),
        yaml_serde::Value::Bool(value) => Ok(serde_json::Value::Bool(value)),
        yaml_serde::Value::Number(value) => {
            serde_json::to_value(value).map_err(|error| authoring_error(path, error.to_string()))
        }
        yaml_serde::Value::String(value) => Ok(serde_json::Value::String(value)),
        yaml_serde::Value::Sequence(values) => values
            .into_iter()
            .enumerate()
            .map(|(index, value)| yaml_value_to_json(value, &format!("{path}[{index}]"), depth + 1))
            .collect::<Result<Vec<_>, _>>()
            .map(serde_json::Value::Array),
        yaml_serde::Value::Mapping(values) => {
            let mut object = serde_json::Map::new();
            for (key, value) in values {
                let yaml_serde::Value::String(key) = key else {
                    return Err(authoring_error(path, "YAML mapping keys must be strings"));
                };
                if object.contains_key(&key) {
                    return Err(authoring_error(
                        format!("{path}.{key}"),
                        "duplicate YAML mapping key",
                    ));
                }
                let value = yaml_value_to_json(value, &format!("{path}.{key}"), depth + 1)?;
                object.insert(key, value);
            }
            Ok(serde_json::Value::Object(object))
        }
        yaml_serde::Value::Tagged(_) => {
            Err(authoring_error(path, "custom YAML tags are unsupported"))
        }
    }
}

fn empty_json_object() -> serde_json::Value {
    serde_json::json!({})
}

fn empty_workflow_source_configuration_schema() -> ValueSchema {
    ValueSchema {
        type_name: "bcode.workflow.source-configuration/v1".to_string(),
        schema: serde_json::json!({
            "$schema": WORKFLOW_AUTHORING_JSON_SCHEMA_DIALECT,
            "type": "object",
            "additionalProperties": false
        }),
    }
}

impl WorkflowStructuredSourceDocument {
    /// Deterministically lower source-v3 actions, conditions, control flow, and child calls to
    /// canonical nodes and edges. Unsupported future structured constructs are rejected by
    /// `deny_unknown_fields` rather than approximated.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, invalid identities, forward/cyclic references,
    /// unavailable actions, selector/predicate failures, schema mismatches, or invalid canonical
    /// output.
    #[allow(clippy::too_many_lines)]
    pub fn lower(
        &self,
        catalog: &WorkflowAuthoringCatalogSnapshot,
    ) -> Result<(WorkflowAuthoringDocument, WorkflowSourceMap), WorkflowError> {
        if self.workflow_source_version != WORKFLOW_SOURCE_DOCUMENT_VERSION {
            return Err(authoring_error(
                "workflow_source_version",
                format!(
                    "unsupported structured workflow source version {}; expected {}",
                    self.workflow_source_version, WORKFLOW_SOURCE_DOCUMENT_VERSION
                ),
            ));
        }
        catalog.validate()?;
        validate_authoring_id("workflow_id", &self.workflow_id)?;
        if let Some(input) = &self.input {
            validate_runtime_value_schema("input", input)?;
        }
        if let Some(output) = &self.output {
            validate_runtime_value_schema("output", output)?;
        }
        WorkflowAuthoringMetadata {
            title: self.title.clone(),
            description: self.description.clone(),
            labels: self.labels.clone(),
        }
        .validate()?;
        validate_runtime_value_schema("configuration_schema", &self.configuration_schema)?;
        self.run_limits.validate()?;
        if self.steps.is_empty() || self.steps.len() > MAX_WORKFLOW_SOURCE_STEPS {
            return Err(authoring_error(
                "steps",
                format!("structured workflows require 1..={MAX_WORKFLOW_SOURCE_STEPS} steps"),
            ));
        }
        let (mut document, mut source_map) = self.lower_mixed_steps(catalog)?;
        self.materialize_static_source_dependencies(&mut document, catalog)?;
        let order = self
            .steps
            .iter()
            .enumerate()
            .map(|(index, step)| (step.id.as_str(), index))
            .collect::<BTreeMap<_, _>>();
        for (index, step) in self.steps.iter().enumerate() {
            if let WorkflowStructuredSourceOperation::WorkflowCall(call) = &step.operation
                && let Some(mapping) = &call.input
            {
                if step.input_from.is_some() || step.input_expression.is_some() {
                    return Err(authoring_error(
                        format!("steps[{index}].workflow_call.input"),
                        "workflow_call.input cannot be combined with step input_from or input_expression",
                    ));
                }
                let target = document.definition.nodes.get(&step.id).ok_or_else(|| {
                    authoring_error(
                        format!("steps[{index}].id"),
                        "structured step did not lower to a canonical node",
                    )
                })?;
                if mapping.output != target.input {
                    return Err(authoring_error(
                        format!("steps[{index}].workflow_call.input.output"),
                        "child-call input mapping must produce the exact child input interface",
                    ));
                }
                validate_structured_source_expression_dependencies(
                    mapping,
                    &order,
                    &document.definition,
                    step,
                    index,
                )?;
                for dependency in &step.needs {
                    let edge = document
                        .definition
                        .edges
                        .iter_mut()
                        .find(|edge| edge.from == *dependency && edge.to == step.id)
                        .ok_or_else(|| {
                            authoring_error(
                                format!("steps[{index}].workflow_call.input"),
                                format!(
                                    "named dependency '{dependency}' has no canonical edge to the child call"
                                ),
                            )
                        })?;
                    edge.transform = Some(mapping.clone());
                }
            }
            if step.input_from.is_some() && step.input_expression.is_some() {
                return Err(authoring_error(
                    format!("steps[{index}].input_expression"),
                    "input_from and input_expression are mutually exclusive",
                ));
            }
            if step.input_from.is_some() || step.input_expression.is_some() {
                // Dynamic dependency payloads are canonical runtime input; action `with` values
                // remain schema-checked authoring data but must not replace the selected edge.
                document.plugin_input_defaults.remove(&step.id);
            }
            if let Some(expression) = &step.input_expression {
                expression.validate()?;
                let target = document.definition.nodes.get(&step.id).ok_or_else(|| {
                    authoring_error(
                        format!("steps[{index}].id"),
                        "structured step did not lower to a canonical node",
                    )
                })?;
                if expression.output != target.input {
                    return Err(authoring_error(
                        format!("steps[{index}].input_expression.output"),
                        "input expression output must exactly match the target input interface",
                    ));
                }
                validate_structured_source_expression_dependencies(
                    expression,
                    &order,
                    &document.definition,
                    step,
                    index,
                )?;
                for dependency in &step.needs {
                    let edge = document
                        .definition
                        .edges
                        .iter_mut()
                        .find(|edge| edge.from == *dependency && edge.to == step.id)
                        .ok_or_else(|| {
                            authoring_error(
                                format!("steps[{index}].input_expression"),
                                format!(
                                    "named dependency '{dependency}' has no canonical edge to the step"
                                ),
                            )
                        })?;
                    edge.transform = Some(expression.clone());
                }
                source_map.entries.push(WorkflowSourceMapEntry {
                    step_index: index,
                    source_path: format!("steps[{index}].input_expression"),
                    target_kind: WorkflowSourceMapTargetKind::Edge,
                    node_id: step.id.clone(),
                    edge_to: None,
                });
            }
            if let Some(reference) = &step.input_from {
                let source_schema = validate_structured_source_reference(
                    reference,
                    &order,
                    &document.definition,
                    step,
                    index,
                    "input_from",
                )?;
                let target = document.definition.nodes.get(&step.id).ok_or_else(|| {
                    authoring_error(
                        format!("steps[{index}].id"),
                        "structured step did not lower to a canonical node",
                    )
                })?;
                if target.kind == NodeKind::PluginBlock {
                    let block: WorkflowBlockDefinition =
                        serde_json::from_value(target.configuration.clone()).map_err(|error| {
                            authoring_error(
                                format!("steps[{index}].input_from"),
                                format!("plugin-block contract is invalid: {error}"),
                            )
                        })?;
                    if !workflow_allows_dynamic_complete_input(&block.input) {
                        return Err(authoring_error(
                            format!("steps[{index}].input_from"),
                            "plugin block forbids dynamic complete-input binding; bind only declared safe fields",
                        ));
                    }
                }
                let edge = document
                    .definition
                    .edges
                    .iter_mut()
                    .find(|edge| edge.from == reference.step && edge.to == step.id)
                    .ok_or_else(|| {
                        authoring_error(
                            format!("steps[{index}].input_from"),
                            "input reference must have one canonical dependency edge to the step",
                        )
                    })?;
                edge.transform = if let Some(selector) = &reference.select {
                    if source_schema.schema != target.input.schema {
                        return Err(authoring_error(
                            format!("steps[{index}].input_from.select"),
                            "selected source schema must match the target input schema exactly",
                        ));
                    }
                    Some(WorkflowTransform {
                        version: WORKFLOW_TRANSFORM_VERSION,
                        expression: WorkflowTransformExpression::SelectedInput {
                            source: WORKFLOW_TRANSFORM_SOURCE_CURRENT.to_string(),
                            selector: selector.clone(),
                        },
                        output: target.input.clone(),
                    })
                } else {
                    if source_schema != target.input {
                        return Err(authoring_error(
                            format!("steps[{index}].input_from"),
                            "whole-value source output and target input schemas must match exactly",
                        ));
                    }
                    None
                };
                source_map.entries.push(WorkflowSourceMapEntry {
                    step_index: index,
                    source_path: format!("steps[{index}].input_from"),
                    target_kind: WorkflowSourceMapTargetKind::Edge,
                    node_id: reference.step.clone(),
                    edge_to: Some(step.id.clone()),
                });
            }
            if let Some(retry) = &step.retry {
                validate_structured_source_retry(retry, &document.run_limits, index)?;
                let node = document.definition.nodes.get_mut(&step.id).ok_or_else(|| {
                    authoring_error(
                        format!("steps[{index}].id"),
                        "structured step did not lower to a canonical node",
                    )
                })?;
                if node.kind != NodeKind::PluginBlock {
                    return Err(authoring_error(
                        format!("steps[{index}].retry"),
                        "durable automatic retry currently requires a plugin-owned action",
                    ));
                }
                let mut block: WorkflowBlockDefinition =
                    serde_json::from_value(node.configuration.clone()).map_err(|error| {
                        authoring_error(
                            format!("steps[{index}].retry"),
                            format!("retryable block configuration is invalid: {error}"),
                        )
                    })?;
                block.automatic_retry = Some(WorkflowAutomaticRetryPolicy::from(retry));
                block.validate()?;
                node.configuration = serde_json::to_value(block).map_err(|error| {
                    authoring_error(
                        format!("steps[{index}].retry"),
                        format!("retry policy cannot be serialized: {error}"),
                    )
                })?;
            }
            if let Some(repeat) = &step.repeat {
                lower_structured_source_repeat(
                    &mut document,
                    &mut source_map,
                    step,
                    repeat,
                    index,
                )?;
            }
            let Some(condition) = &step.when else {
                continue;
            };
            validate_predicate_expression(&condition.predicate)?;
            let selected_schema = validate_structured_source_reference(
                &condition.source,
                &order,
                &document.definition,
                step,
                index,
                "when.source",
            )?;
            let edge = document
                .definition
                .edges
                .iter_mut()
                .find(|edge| edge.from == condition.source.step && edge.to == step.id)
                .ok_or_else(|| {
                    authoring_error(
                        format!("steps[{index}].when"),
                        "condition source must have one canonical dependency edge to the step",
                    )
                })?;
            let target = document.definition.nodes.get(&step.id).ok_or_else(|| {
                authoring_error(
                    format!("steps[{index}].id"),
                    "structured step did not lower to a canonical node",
                )
            })?;
            let uses_selected_input = condition.source.select.is_some();
            let uses_dependency_input =
                step.input_from.is_some() || step.input_expression.is_some();
            let condition_gate = matches!(
                step.operation,
                WorkflowStructuredSourceOperation::Input(_)
                    | WorkflowStructuredSourceOperation::Approval(_)
            );
            if uses_selected_input || uses_dependency_input || condition_gate {
                if selected_schema.schema != target.input.schema {
                    return Err(authoring_error(
                        format!("steps[{index}].when"),
                        "condition source output and target input schemas must match exactly",
                    ));
                }
            } else if edge.transform.is_none() {
                return Err(authoring_error(
                    format!("steps[{index}].when"),
                    "condition-only dependencies require static source input or an explicit input_from",
                ));
            }
            let predicate = if let Some(selector) = &condition.source.select {
                let selected_transform = WorkflowTransform {
                    version: WORKFLOW_TRANSFORM_VERSION,
                    expression: WorkflowTransformExpression::SelectedInput {
                        source: WORKFLOW_TRANSFORM_SOURCE_CURRENT.to_string(),
                        selector: selector.clone(),
                    },
                    output: target.input.clone(),
                };
                if edge
                    .transform
                    .as_ref()
                    .is_some_and(|existing| existing != &selected_transform)
                {
                    return Err(authoring_error(
                        format!("steps[{index}].when.source.select"),
                        "condition and input references on one edge must select the same value",
                    ));
                }
                edge.transform = Some(selected_transform);
                prefix_predicate_selector(&condition.predicate, selector)?
            } else {
                condition.predicate.clone()
            };
            edge.kind = EdgeKind::Conditional {
                predicate,
                expected: condition.expected,
            };
            source_map.entries.push(WorkflowSourceMapEntry {
                step_index: index,
                source_path: format!("steps[{index}].when"),
                target_kind: WorkflowSourceMapTargetKind::Edge,
                node_id: condition.source.step.clone(),
                edge_to: Some(step.id.clone()),
            });
        }
        document.validate()?;
        Ok((document, source_map))
    }

    fn materialize_static_source_dependencies(
        &self,
        document: &mut WorkflowAuthoringDocument,
        catalog: &WorkflowAuthoringCatalogSnapshot,
    ) -> Result<(), WorkflowError> {
        for (index, step) in self.steps.iter().enumerate() {
            if step.input_from.is_some()
                || step.input_expression.is_some()
                || matches!(
                    &step.operation,
                    WorkflowStructuredSourceOperation::Parallel(_)
                        | WorkflowStructuredSourceOperation::WorkflowCall(_)
                        | WorkflowStructuredSourceOperation::Input(_)
                        | WorkflowStructuredSourceOperation::Approval(_)
                        | WorkflowStructuredSourceOperation::Agent(_)
                )
            {
                continue;
            }
            let target_input = document
                .definition
                .node(&step.id)
                .ok_or_else(|| {
                    authoring_error(
                        format!("steps[{index}].id"),
                        "structured step did not lower to a canonical node",
                    )
                })?
                .input
                .clone();
            let static_input = match &step.operation {
                WorkflowStructuredSourceOperation::Action(action) => {
                    Some(lower_workflow_source_action(action, catalog, index)?.1)
                }
                WorkflowStructuredSourceOperation::Prompt(prompt) => prompt.input_value.clone(),
                WorkflowStructuredSourceOperation::FanOut(_)
                | WorkflowStructuredSourceOperation::Parallel(_)
                | WorkflowStructuredSourceOperation::WorkflowCall(_)
                | WorkflowStructuredSourceOperation::Input(_)
                | WorkflowStructuredSourceOperation::Approval(_)
                | WorkflowStructuredSourceOperation::Agent(_) => None,
            };
            let Some(static_input) = static_input else {
                continue;
            };
            for edge in document
                .definition
                .edges
                .iter_mut()
                .filter(|edge| edge.to == step.id)
            {
                edge.transform = Some(WorkflowTransform {
                    version: WORKFLOW_TRANSFORM_VERSION,
                    expression: WorkflowTransformExpression::Constant {
                        value: static_input.clone(),
                    },
                    output: target_input.clone(),
                });
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn lower_mixed_steps(
        &self,
        catalog: &WorkflowAuthoringCatalogSnapshot,
    ) -> Result<(WorkflowAuthoringDocument, WorkflowSourceMap), WorkflowError> {
        let mut node_ids: Vec<String> = Vec::with_capacity(self.steps.len());
        let mut nodes: BTreeMap<String, NodeDefinition> = BTreeMap::new();
        let mut edges = Vec::new();
        let mut plugin_input_defaults = BTreeMap::new();
        let mut source_entries = Vec::with_capacity(self.steps.len());
        let mut outgoing = BTreeSet::new();
        for (index, step) in self.steps.iter().enumerate() {
            validate_authoring_id(&format!("steps[{index}].id"), &step.id)?;
            if nodes.contains_key(&step.id) {
                return Err(authoring_error(
                    format!("steps[{index}].id"),
                    "structured step IDs must be unique",
                ));
            }
            let dependencies = if step.needs.is_empty() && index > 0 && !step.independent {
                vec![node_ids[index - 1].clone()]
            } else {
                step.needs.clone()
            };
            let unique = dependencies.iter().collect::<BTreeSet<_>>();
            if unique.len() != dependencies.len() {
                return Err(authoring_error(
                    format!("steps[{index}].needs"),
                    "structured dependencies must be unique",
                ));
            }
            for predecessor in dependencies {
                if !nodes.contains_key(&predecessor) {
                    return Err(authoring_error(
                        format!("steps[{index}].needs"),
                        "structured dependencies must reference an earlier step",
                    ));
                }
                outgoing.insert(predecessor.clone());
                edges.push(EdgeDefinition {
                    from: predecessor,
                    to: step.id.clone(),
                    kind: EdgeKind::Direct,
                    transform: None,
                });
            }
            let node = match &step.operation {
                WorkflowStructuredSourceOperation::FanOut(fan_out) => {
                    validate_structured_source_fan_out(fan_out, &self.run_limits, index)?;
                    let member_node = lower_structured_fan_out_member(fan_out, catalog, index)?;
                    let configuration = WorkflowFanOutConfiguration {
                        version: WORKFLOW_FAN_OUT_CONFIGURATION_VERSION,
                        member_node: Box::new(member_node),
                        max_members: fan_out.max_members,
                        max_concurrency: fan_out.max_concurrency,
                        failure_policy: fan_out.failure_policy,
                    };
                    configuration.validate()?;
                    NodeDefinition {
                        id: step.id.clone(),
                        name: step.name.clone().unwrap_or_else(|| step.id.clone()),
                        kind: NodeKind::FanOut,
                        dataflow: WorkflowNodeDataflowPolicy::Direct,
                        input: fan_out.input.clone(),
                        output: workflow_fan_out_result_schema(&fan_out.output_member)?,
                        resources: Vec::new(),
                        configuration: serde_json::to_value(configuration).map_err(|error| {
                            authoring_error(
                                format!("steps[{index}].fan_out"),
                                format!("fan-out configuration cannot be serialized: {error}"),
                            )
                        })?,
                    }
                }
                WorkflowStructuredSourceOperation::Parallel(parallel) => {
                    if step.needs.len() != 2
                        || !step.needs.contains(&parallel.left)
                        || !step.needs.contains(&parallel.right)
                        || parallel.left == parallel.right
                    {
                        return Err(authoring_error(
                            format!("steps[{index}].parallel"),
                            "parallel join must name exactly two distinct explicit dependencies",
                        ));
                    }
                    let left = nodes.get(&parallel.left).ok_or_else(|| {
                        authoring_error(
                            format!("steps[{index}].parallel.left"),
                            "parallel left branch must reference a prior step",
                        )
                    })?;
                    let right = nodes.get(&parallel.right).ok_or_else(|| {
                        authoring_error(
                            format!("steps[{index}].parallel.right"),
                            "parallel right branch must reference a prior step",
                        )
                    })?;
                    let join_schema = workflow_parallel_join_schema(&left.output, &right.output)?;
                    NodeDefinition {
                        id: step.id.clone(),
                        name: step.name.clone().unwrap_or_else(|| step.id.clone()),
                        kind: NodeKind::Parallel,
                        dataflow: WorkflowNodeDataflowPolicy::Direct,
                        input: join_schema.clone(),
                        output: join_schema,
                        resources: Vec::new(),
                        configuration: serde_json::json!({
                            "failure_policy": parallel.failure_policy,
                            "left_exits": [&parallel.left],
                            "right_exits": [&parallel.right],
                        }),
                    }
                }
                WorkflowStructuredSourceOperation::WorkflowCall(call) => {
                    call.validate()?;
                    let identity = call.target.definition_identity();
                    let child = catalog
                        .workflow_definitions
                        .get(&identity.definition_id)
                        .ok_or_else(|| {
                            authoring_error(
                                format!("steps[{index}].workflow_call.target"),
                                format!(
                                    "exact child definition '{}' is unavailable",
                                    identity.definition_id
                                ),
                            )
                        })?;
                    let actual =
                        WorkflowDefinitionIdentity::for_definition(identity.kind.clone(), child)?;
                    if &actual != identity {
                        return Err(authoring_error(
                            format!("steps[{index}].workflow_call.target"),
                            "exact child definition identity does not match catalog content",
                        ));
                    }
                    if let Some(input) = &call.input
                        && input.output != child.input
                    {
                        return Err(authoring_error(
                            format!("steps[{index}].workflow_call.input.output"),
                            "child-call input mapping must produce the exact child input interface",
                        ));
                    }
                    let output = call
                        .output
                        .as_ref()
                        .map_or_else(|| child.output.clone(), |mapping| mapping.output.clone());
                    NodeDefinition {
                        id: step.id.clone(),
                        name: step.name.clone().unwrap_or_else(|| step.id.clone()),
                        kind: NodeKind::WorkflowCall,
                        dataflow: WorkflowNodeDataflowPolicy::Direct,
                        input: child.input.clone(),
                        output,
                        resources: Vec::new(),
                        configuration: serde_json::to_value(call).map_err(|error| {
                            authoring_error(
                                format!("steps[{index}].workflow_call"),
                                format!("workflow call cannot be serialized: {error}"),
                            )
                        })?,
                    }
                }
                WorkflowStructuredSourceOperation::Input(gate)
                | WorkflowStructuredSourceOperation::Approval(gate) => {
                    validate_runtime_value_schema(
                        &format!("steps[{index}].gate.schema"),
                        &gate.schema,
                    )?;
                    NodeDefinition {
                        id: step.id.clone(),
                        name: step.name.clone().unwrap_or_else(|| step.id.clone()),
                        kind: match &step.operation {
                            WorkflowStructuredSourceOperation::Input(_) => NodeKind::Input,
                            WorkflowStructuredSourceOperation::Approval(_) => NodeKind::Approval,
                            _ => unreachable!(),
                        },
                        dataflow: WorkflowNodeDataflowPolicy::Direct,
                        input: gate.schema.clone(),
                        output: gate.schema.clone(),
                        resources: gate.resources.clone(),
                        configuration: serde_json::json!({"gate_version": 1}),
                    }
                }
                WorkflowStructuredSourceOperation::Action(action) => {
                    let (block, input) = lower_workflow_source_action(action, catalog, index)?;
                    plugin_input_defaults.insert(step.id.clone(), input);
                    NodeDefinition {
                        id: step.id.clone(),
                        name: step.name.clone().unwrap_or_else(|| step.id.clone()),
                        kind: NodeKind::PluginBlock,
                        dataflow: WorkflowNodeDataflowPolicy::Direct,
                        input: block.input.clone(),
                        output: block.output.clone(),
                        resources: block.resources.clone(),
                        configuration: serde_json::to_value(&block).map_err(|error| {
                            authoring_error(
                                format!("steps[{index}]"),
                                format!("target block cannot be serialized: {error}"),
                            )
                        })?,
                    }
                }
                WorkflowStructuredSourceOperation::Prompt(prompt) => {
                    let prompt = prompt.expand()?;
                    NodeDefinition {
                        id: step.id.clone(),
                        name: step.name.clone().unwrap_or_else(|| step.id.clone()),
                        kind: NodeKind::Agent,
                        dataflow: WorkflowNodeDataflowPolicy::Direct,
                        input: prompt.input,
                        output: prompt.output,
                        resources: prompt.resources,
                        configuration: serde_json::to_value(prompt.configuration).map_err(
                            |error| {
                                authoring_error(
                                    format!("steps[{index}].prompt"),
                                    format!("prompt configuration cannot be serialized: {error}"),
                                )
                            },
                        )?,
                    }
                }
                WorkflowStructuredSourceOperation::Agent(prompt) => {
                    prompt.configuration.validate()?;
                    validate_runtime_value_schema(
                        &format!("steps[{index}].prompt.input"),
                        &prompt.input,
                    )?;
                    validate_runtime_value_schema(
                        &format!("steps[{index}].prompt.output"),
                        &prompt.output,
                    )?;
                    if prompt.configuration.structured_output.schema != prompt.output {
                        return Err(authoring_error(
                            format!("steps[{index}].prompt.output"),
                            "agent output must match its structured-output schema exactly",
                        ));
                    }
                    NodeDefinition {
                        id: step.id.clone(),
                        name: step.name.clone().unwrap_or_else(|| step.id.clone()),
                        kind: NodeKind::Agent,
                        dataflow: WorkflowNodeDataflowPolicy::Direct,
                        input: prompt.input.clone(),
                        output: prompt.output.clone(),
                        resources: prompt.resources.clone(),
                        configuration: serde_json::to_value(&prompt.configuration).map_err(
                            |error| {
                                authoring_error(
                                    format!("steps[{index}].agent"),
                                    format!("prompt configuration cannot be serialized: {error}"),
                                )
                            },
                        )?,
                    }
                }
            };
            node_ids.push(step.id.clone());
            nodes.insert(step.id.clone(), node);
            source_entries.push(WorkflowSourceMapEntry {
                step_index: index,
                source_path: format!("steps[{index}]"),
                target_kind: WorkflowSourceMapTargetKind::Node,
                node_id: step.id.clone(),
                edge_to: None,
            });
        }
        let entries = node_ids
            .iter()
            .filter(|node_id| !edges.iter().any(|edge| &edge.to == *node_id))
            .cloned()
            .collect::<Vec<_>>();
        let exits = node_ids
            .iter()
            .filter(|node_id| !outgoing.contains(*node_id))
            .cloned()
            .collect::<Vec<_>>();
        let first_entry = entries
            .first()
            .ok_or_else(|| authoring_error("steps", "no entry"))?;
        let input = self.input.clone().unwrap_or_else(|| {
            nodes
                .get(first_entry)
                .expect("entry node exists")
                .input
                .clone()
        });
        for entry in &entries {
            let entry_schema = &nodes.get(entry).expect("entry node exists").input;
            if entry_schema != &input {
                return Err(authoring_error(
                    format!("steps.{entry}.input"),
                    format!(
                        "entry input interface '{}' does not exactly match declared workflow input interface '{}'",
                        entry_schema.type_name, input.type_name
                    ),
                ));
            }
        }
        let first_exit = exits
            .first()
            .ok_or_else(|| authoring_error("steps", "no exit"))?;
        let output = self.output.clone().unwrap_or_else(|| {
            nodes
                .get(first_exit)
                .expect("exit node exists")
                .output
                .clone()
        });
        for exit in &exits {
            let exit_schema = &nodes.get(exit).expect("exit node exists").output;
            if exit_schema != &output {
                return Err(authoring_error(
                    format!("steps.{exit}.output"),
                    format!(
                        "successful terminal output interface '{}' does not exactly match declared workflow output interface '{}'",
                        exit_schema.type_name, output.type_name
                    ),
                ));
            }
        }
        let document = WorkflowAuthoringDocument {
            schema_version: WORKFLOW_AUTHORING_DOCUMENT_VERSION,
            workflow_id: self.workflow_id.clone(),
            metadata: WorkflowAuthoringMetadata {
                title: self.title.clone(),
                description: self.description.clone(),
                labels: self.labels.clone(),
            },
            configuration_schema: self.configuration_schema.clone(),
            configuration_defaults: self.configuration_defaults.clone(),
            plugin_input_defaults,
            definition: WorkflowDefinition {
                schema_version: WORKFLOW_DEFINITION_SCHEMA_VERSION,
                name: self.title.clone(),
                input,
                output,
                nodes,
                entries,
                exits,
                edges,
            },
            bindings: Vec::new(),
            requirements: WorkflowRequirementSummary::default(),
            run_limits: self.run_limits.clone(),
            producer: WorkflowProducerProvenance {
                kind: WorkflowProducerKind::Human,
                producer_id: None,
                source_revision: None,
            },
            presentation: None,
        };
        document.validate()?;
        validate_definition_boundary_interfaces(&document.definition).map_err(
            |error| match error {
                WorkflowError::Build { path, message } => authoring_error(path, message),
                other => other,
            },
        )?;
        Ok((
            document,
            WorkflowSourceMap {
                version: WORKFLOW_SOURCE_MAP_VERSION,
                entries: source_entries,
            },
        ))
    }
}

fn validate_generated_source_node_id(path: &str, value: &str) -> Result<(), WorkflowError> {
    if value.is_empty()
        || value.len() > MAX_WORKFLOW_AUTHORING_ID_BYTES
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '/' | ':')
        })
    {
        return Err(authoring_error(
            path,
            "generated source node identity exceeds bounds or contains unsupported characters",
        ));
    }
    Ok(())
}

fn validate_structured_source_fan_out(
    fan_out: &WorkflowStructuredSourceFanOut,
    limits: &WorkflowRunLimitPolicy,
    index: usize,
) -> Result<(), WorkflowError> {
    for (suffix, schema) in [
        ("input", &fan_out.input),
        ("member", &fan_out.member),
        ("output_member", &fan_out.output_member),
    ] {
        validate_runtime_value_schema(&format!("steps[{index}].fan_out.{suffix}"), schema)?;
    }
    let members_fit_run = fan_out.max_members <= limits.node_execution_cap;
    let concurrency_fits_run = fan_out.max_concurrency <= limits.concurrency_cap;
    if fan_out.max_members == 0
        || !members_fit_run
        || fan_out.max_concurrency == 0
        || fan_out.max_concurrency > fan_out.max_members
        || !concurrency_fits_run
    {
        return Err(authoring_error(
            format!("steps[{index}].fan_out"),
            "fan-out member/concurrency bounds must be nonzero and fit run limits",
        ));
    }
    let items = fan_out.input.schema.get("items").ok_or_else(|| {
        authoring_error(
            format!("steps[{index}].fan_out.input"),
            "fan-out input must declare a homogeneous array item schema",
        )
    })?;
    if fan_out
        .input
        .schema
        .get("type")
        .and_then(serde_json::Value::as_str)
        != Some("array")
        || items != &fan_out.member.schema
    {
        return Err(authoring_error(
            format!("steps[{index}].fan_out.input"),
            "fan-out input item schema must exactly match the member schema",
        ));
    }
    let output = workflow_fan_out_result_schema(&fan_out.output_member)?;
    validate_runtime_value_schema(&format!("steps[{index}].fan_out.output"), &output)?;
    match &fan_out.operation {
        WorkflowStructuredSourceOperation::Prompt(prompt) => {
            let expanded = prompt.expand()?;
            if expanded.input != fan_out.member || expanded.output != fan_out.output_member {
                return Err(authoring_error(
                    format!("steps[{index}].fan_out.operation"),
                    "fan-out prompt schemas must match member schemas",
                ));
            }
        }
        WorkflowStructuredSourceOperation::Agent(prompt)
            if prompt.input == fan_out.member && prompt.output == fan_out.output_member => {}
        WorkflowStructuredSourceOperation::WorkflowCall(_)
        | WorkflowStructuredSourceOperation::Action(_)
        | WorkflowStructuredSourceOperation::Input(_)
        | WorkflowStructuredSourceOperation::Approval(_) => {}
        WorkflowStructuredSourceOperation::FanOut(_)
        | WorkflowStructuredSourceOperation::Parallel(_)
        | WorkflowStructuredSourceOperation::Agent(_) => {
            return Err(authoring_error(
                format!("steps[{index}].fan_out.operation"),
                "fan-out operation must be a leaf action/call or schema-matched agent",
            ));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn lower_structured_fan_out_member(
    fan_out: &WorkflowStructuredSourceFanOut,
    catalog: &WorkflowAuthoringCatalogSnapshot,
    index: usize,
) -> Result<NodeDefinition, WorkflowError> {
    let member_id = format!("__fan_out_member_{index}");
    let member_name = format!("Fan-out member for step {index}");
    let mut node = match &fan_out.operation {
        WorkflowStructuredSourceOperation::Prompt(prompt) => {
            let prompt = prompt.expand()?;
            NodeDefinition {
                id: member_id,
                name: member_name,
                kind: NodeKind::Agent,
                dataflow: WorkflowNodeDataflowPolicy::Direct,
                input: prompt.input,
                output: prompt.output,
                resources: prompt.resources,
                configuration: serde_json::to_value(prompt.configuration).map_err(|error| {
                    authoring_error(
                        format!("steps[{index}].fan_out.operation"),
                        format!("fan-out prompt configuration cannot be serialized: {error}"),
                    )
                })?,
            }
        }
        WorkflowStructuredSourceOperation::Agent(prompt) => NodeDefinition {
            id: member_id,
            name: member_name,
            kind: NodeKind::Agent,
            dataflow: WorkflowNodeDataflowPolicy::Direct,
            input: prompt.input.clone(),
            output: prompt.output.clone(),
            resources: prompt.resources.clone(),
            configuration: serde_json::to_value(&prompt.configuration).map_err(|error| {
                authoring_error(
                    format!("steps[{index}].fan_out.operation"),
                    format!("fan-out agent configuration cannot be serialized: {error}"),
                )
            })?,
        },
        WorkflowStructuredSourceOperation::WorkflowCall(call) => {
            let identity = call.target.definition_identity();
            let child = catalog
                .workflow_definitions
                .get(&identity.definition_id)
                .ok_or_else(|| {
                    authoring_error(
                        format!("steps[{index}].fan_out.operation.workflow_call"),
                        "fan-out child workflow is unavailable",
                    )
                })?;
            NodeDefinition {
                id: member_id,
                name: member_name,
                kind: NodeKind::WorkflowCall,
                dataflow: WorkflowNodeDataflowPolicy::Direct,
                input: child.input.clone(),
                output: child.output.clone(),
                resources: Vec::new(),
                configuration: serde_json::to_value(call).map_err(|error| {
                    authoring_error(
                        format!("steps[{index}].fan_out.operation.workflow_call"),
                        format!("fan-out child call cannot be serialized: {error}"),
                    )
                })?,
            }
        }
        WorkflowStructuredSourceOperation::Action(action) => {
            let (block, _input) = lower_workflow_source_action(action, catalog, index)?;
            NodeDefinition {
                id: member_id,
                name: member_name,
                kind: NodeKind::PluginBlock,
                dataflow: WorkflowNodeDataflowPolicy::Direct,
                input: block.input.clone(),
                output: block.output.clone(),
                resources: block.resources.clone(),
                configuration: serde_json::to_value(block).map_err(|error| {
                    authoring_error(
                        format!("steps[{index}].fan_out.operation"),
                        format!("fan-out block cannot be serialized: {error}"),
                    )
                })?,
            }
        }
        WorkflowStructuredSourceOperation::Input(gate)
        | WorkflowStructuredSourceOperation::Approval(gate) => NodeDefinition {
            id: member_id,
            name: member_name,
            kind: match &fan_out.operation {
                WorkflowStructuredSourceOperation::Input(_) => NodeKind::Input,
                WorkflowStructuredSourceOperation::Approval(_) => NodeKind::Approval,
                _ => unreachable!(),
            },
            dataflow: WorkflowNodeDataflowPolicy::Direct,
            input: gate.schema.clone(),
            output: gate.schema.clone(),
            resources: gate.resources.clone(),
            configuration: serde_json::json!({"gate_version": 1}),
        },
        WorkflowStructuredSourceOperation::FanOut(_)
        | WorkflowStructuredSourceOperation::Parallel(_) => unreachable!(),
    };
    if node.input != fan_out.member || node.output != fan_out.output_member {
        return Err(authoring_error(
            format!("steps[{index}].fan_out.operation"),
            "fan-out member operation schemas must exactly match member boundaries",
        ));
    }
    node.id = "member".to_string();
    Ok(node)
}

fn workflow_fan_out_result_schema(member: &ValueSchema) -> Result<ValueSchema, WorkflowError> {
    let schema = ValueSchema {
        type_name: format!("workflow.fan-out-result/v1<{}>", member.type_name),
        schema: serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["version", "members"],
            "properties": {
                "version": {"type": "integer", "const": WORKFLOW_FAN_OUT_RESULT_VERSION},
                "members": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["index", "value"],
                        "properties": {
                            "index": {"type": "integer", "minimum": 0},
                            "value": member.schema.clone()
                        }
                    }
                }
            }
        }),
    };
    validate_runtime_value_schema("fan_out.output", &schema)?;
    Ok(schema)
}

fn validate_structured_source_retry(
    retry: &WorkflowStructuredSourceRetry,
    limits: &WorkflowRunLimitPolicy,
    index: usize,
) -> Result<(), WorkflowError> {
    if retry.max_attempts == 0
        || retry.max_attempts > limits.retry_cap
        || retry.initial_backoff_ms == 0
        || retry.maximum_backoff_ms < retry.initial_backoff_ms
        || retry.maximum_backoff_ms > 86_400_000
        || retry.backoff_multiplier == 0
        || retry.backoff_multiplier > 100
        || retry.eligible_failures.is_empty()
    {
        return Err(authoring_error(
            format!("steps[{index}].retry"),
            "retry bounds, backoff, or eligible failure inventory is invalid",
        ));
    }
    let unique = retry.eligible_failures.iter().collect::<BTreeSet<_>>();
    if unique.len() != retry.eligible_failures.len()
        || retry.eligible_failures.iter().any(|failure| {
            !matches!(
                failure,
                AutomaticRetryFailureKind::OwnerUnavailableBeforeAcceptance
                    | AutomaticRetryFailureKind::OwnerReportedRetryable
            )
        })
    {
        return Err(authoring_error(
            format!("steps[{index}].retry.eligible_failures"),
            "retry failure classes must be unique and safely owner-retryable",
        ));
    }
    Ok(())
}

fn workflow_parallel_join_schema(
    left: &ValueSchema,
    right: &ValueSchema,
) -> Result<ValueSchema, WorkflowError> {
    let join = ValueSchema {
        type_name: format!(
            "workflow.parallel/v1<{},{}>",
            left.type_name, right.type_name
        ),
        schema: serde_json::json!({
            "type": "array",
            "prefixItems": [left.schema.clone(), right.schema.clone()],
            "minItems": 2,
            "maxItems": 2
        }),
    };
    validate_runtime_value_schema("parallel.output", &join)?;
    Ok(join)
}

fn workflow_repeat_outcome_schema(value: &ValueSchema) -> Result<ValueSchema, WorkflowError> {
    let schema = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "version", "outcome", "iterations_completed", "max_iterations", "cycle_cap",
            "effective_iteration_bound", "value"
        ],
        "properties": {
            "version": {"type": "integer", "const": WORKFLOW_REPEAT_OUTCOME_VERSION},
            "outcome": {
                "type": "string",
                "enum": ["condition_cleared", "iteration_limit_reached"]
            },
            "iterations_completed": {"type": "integer", "minimum": 0},
            "max_iterations": {"type": "integer", "minimum": 1},
            "cycle_cap": {"type": "integer", "minimum": 1},
            "effective_iteration_bound": {"type": "integer", "minimum": 1},
            "value": value.schema.clone()
        }
    });
    let outcome = ValueSchema {
        type_name: format!("workflow.repeat-outcome/v1<{}>", value.type_name),
        schema,
    };
    validate_runtime_value_schema("repeat.output", &outcome)?;
    Ok(outcome)
}

#[allow(clippy::too_many_lines)]
fn lower_structured_source_repeat(
    document: &mut WorkflowAuthoringDocument,
    source_map: &mut WorkflowSourceMap,
    step: &WorkflowStructuredSourceStep,
    repeat: &WorkflowStructuredSourceRepeat,
    index: usize,
) -> Result<(), WorkflowError> {
    validate_predicate_expression(&repeat.while_predicate)?;
    if repeat.max_iterations == 0 || repeat.max_iterations > document.run_limits.cycle_cap {
        return Err(authoring_error(
            format!("steps[{index}].repeat.max_iterations"),
            "repeat max_iterations must be nonzero and not exceed the workflow cycle cap",
        ));
    }
    let body = document.definition.node(&step.id).cloned().ok_or_else(|| {
        authoring_error(
            format!("steps[{index}].id"),
            "repeat body did not lower to a canonical node",
        )
    })?;
    if body.input != body.output {
        return Err(authoring_error(
            format!("steps[{index}].repeat"),
            "repeat body input and output schemas must match exactly",
        ));
    }
    let outcome_schema = if repeat.exhaustion_policy == WorkflowRepeatExhaustionPolicy::EmitOutcome
    {
        Some(workflow_repeat_outcome_schema(&body.output)?)
    } else {
        None
    };
    let controller_output = outcome_schema
        .clone()
        .unwrap_or_else(|| body.output.clone());
    let controller_id = format!("{}__repeat", step.id);
    validate_generated_source_node_id(&format!("steps[{index}].repeat"), &controller_id)?;
    if document.definition.nodes.contains_key(&controller_id) {
        return Err(authoring_error(
            format!("steps[{index}].repeat"),
            "generated repeat-controller identity conflicts with an explicit step ID",
        ));
    }
    if outcome_schema.is_some() {
        for successor in document
            .definition
            .edges
            .iter()
            .filter(|edge| edge.from == step.id)
            .map(|edge| &edge.to)
        {
            let target = document.definition.node(successor).ok_or_else(|| {
                authoring_error(
                    format!("steps[{index}].repeat"),
                    "repeat successor did not lower to a canonical node",
                )
            })?;
            if target.input != controller_output {
                return Err(authoring_error(
                    format!("steps[{index}].repeat.exhaustion_policy"),
                    "emit_outcome repeat successors must accept the exact typed repeat outcome schema",
                ));
            }
        }
    }
    for edge in document
        .definition
        .edges
        .iter_mut()
        .filter(|edge| edge.from == step.id)
    {
        edge.from.clone_from(&controller_id);
    }
    document.definition.nodes.insert(
        controller_id.clone(),
        NodeDefinition {
            id: controller_id.clone(),
            name: format!("{} repeat", body.name),
            kind: NodeKind::Repeat,
            dataflow: WorkflowNodeDataflowPolicy::Direct,
            input: body.output,
            output: controller_output,
            resources: Vec::new(),
            configuration: serde_json::json!({
                "predicate_version": repeat.while_predicate.version(),
                "predicate": repeat.while_predicate,
                "max_iterations": repeat.max_iterations,
                "iteration_state": "explicit_back_edge_transform",
                "exhaustion_policy": repeat.exhaustion_policy,
                "repeat_outcome_version": outcome_schema
                    .as_ref()
                    .map(|_| WORKFLOW_REPEAT_OUTCOME_VERSION),
            }),
        },
    );
    document.definition.edges.push(EdgeDefinition {
        from: step.id.clone(),
        to: controller_id.clone(),
        kind: EdgeKind::Direct,
        transform: None,
    });
    document.definition.edges.push(EdgeDefinition {
        from: controller_id.clone(),
        to: step.id.clone(),
        kind: EdgeKind::Back {
            predicate: repeat.while_predicate.clone(),
            max_iterations: repeat.max_iterations,
        },
        transform: None,
    });
    if let Some(exit) = document
        .definition
        .exits
        .iter_mut()
        .find(|exit| **exit == step.id)
    {
        exit.clone_from(&controller_id);
        document.definition.output = document
            .definition
            .nodes
            .get(&controller_id)
            .expect("inserted repeat controller")
            .output
            .clone();
    }
    source_map.entries.push(WorkflowSourceMapEntry {
        step_index: index,
        source_path: format!("steps[{index}].repeat"),
        target_kind: WorkflowSourceMapTargetKind::Node,
        node_id: controller_id.clone(),
        edge_to: None,
    });
    source_map.entries.push(WorkflowSourceMapEntry {
        step_index: index,
        source_path: format!("steps[{index}].repeat.while_predicate"),
        target_kind: WorkflowSourceMapTargetKind::Edge,
        node_id: controller_id,
        edge_to: Some(step.id.clone()),
    });
    Ok(())
}

fn prefix_predicate_selector(
    predicate: &PredicateExpression,
    prefix: &WorkflowValueSelector,
) -> Result<PredicateExpression, WorkflowError> {
    let prefix_one = |selector: &WorkflowValueSelector| WorkflowValueSelector {
        version: WORKFLOW_VALUE_SELECTOR_VERSION,
        segments: prefix
            .segments
            .iter()
            .chain(&selector.segments)
            .cloned()
            .collect(),
    };
    let prefixed = match predicate {
        PredicateExpression::SelectedEquals {
            selector, value, ..
        } => PredicateExpression::SelectedEquals {
            version: WORKFLOW_PREDICATE_VERSION,
            selector: prefix_one(selector),
            value: value.clone(),
        },
        PredicateExpression::SelectedValuesEqual {
            left_selector,
            right_selector,
            ..
        } => PredicateExpression::SelectedValuesEqual {
            version: WORKFLOW_PREDICATE_VERSION,
            left_selector: prefix_one(left_selector),
            right_selector: prefix_one(right_selector),
        },
        PredicateExpression::SelectedNumericCompare {
            left_selector,
            right_selector,
            comparison,
            ..
        } => PredicateExpression::SelectedNumericCompare {
            version: WORKFLOW_PREDICATE_VERSION,
            left_selector: prefix_one(left_selector),
            right_selector: prefix_one(right_selector),
            comparison: *comparison,
        },
        _ if prefix.segments.is_empty() => predicate.clone(),
        _ => {
            return Err(authoring_error(
                "condition.predicate",
                "selected source conditions require explicit selector-based predicates",
            ));
        }
    };
    validate_predicate_expression(&prefixed)?;
    Ok(prefixed)
}

fn validate_structured_source_expression_dependencies(
    transform: &WorkflowTransform,
    order: &BTreeMap<&str, usize>,
    definition: &WorkflowDefinition,
    step: &WorkflowStructuredSourceStep,
    index: usize,
) -> Result<(), WorkflowError> {
    let sources = transform.referenced_sources();
    let dependencies = sources
        .iter()
        .filter_map(|source| {
            source
                .strip_prefix(WORKFLOW_TRANSFORM_SOURCE_DEPENDENCY_PREFIX)
                .map(str::to_string)
        })
        .collect::<BTreeSet<_>>();
    let mut allowed_sources = BTreeSet::from([
        WORKFLOW_TRANSFORM_SOURCE_CURRENT.to_string(),
        WORKFLOW_TRANSFORM_SOURCE_STATE.to_string(),
        WORKFLOW_TRANSFORM_SOURCE_CONFIGURATION.to_string(),
    ]);
    for dependency in dependencies {
        allowed_sources.insert(format!(
            "{WORKFLOW_TRANSFORM_SOURCE_DEPENDENCY_PREFIX}{dependency}"
        ));
        let reference = WorkflowStructuredSourceReference {
            step: dependency,
            select: None,
        };
        validate_structured_source_reference(
            &reference,
            order,
            definition,
            step,
            index,
            "input_expression",
        )?;
    }
    if let Some(source) = sources
        .iter()
        .find(|source| !allowed_sources.contains(*source))
    {
        return Err(authoring_error(
            format!("steps[{index}].input_expression"),
            format!("input expression references unavailable named source '{source}'"),
        ));
    }
    Ok(())
}

fn validate_structured_source_reference(
    reference: &WorkflowStructuredSourceReference,
    order: &BTreeMap<&str, usize>,
    definition: &WorkflowDefinition,
    step: &WorkflowStructuredSourceStep,
    index: usize,
    path: &str,
) -> Result<ValueSchema, WorkflowError> {
    if let Some(selector) = &reference.select {
        selector.validate()?;
    }
    let source_index = order.get(reference.step.as_str()).ok_or_else(|| {
        authoring_error(
            format!("steps[{index}].{path}.step"),
            format!("reference targets unknown step '{}'", reference.step),
        )
    })?;
    if *source_index >= index {
        return Err(authoring_error(
            format!("steps[{index}].{path}.step"),
            "references may target only prior steps",
        ));
    }
    if !step.needs.is_empty() && !step.needs.contains(&reference.step) {
        return Err(authoring_error(
            format!("steps[{index}].{path}.step"),
            "reference source must be one of the step's explicit dependencies",
        ));
    }
    let source = definition.node(&reference.step).ok_or_else(|| {
        authoring_error(
            format!("steps[{index}].{path}.step"),
            "reference source did not lower to a canonical node",
        )
    })?;
    reference.select.as_ref().map_or_else(
        || Ok(source.output.clone()),
        |selector| {
            select_workflow_value_schema(
                &source.output,
                selector,
                &format!("steps[{index}].{path}.select"),
            )
        },
    )
}

fn select_workflow_value_schema(
    schema: &ValueSchema,
    selector: &WorkflowValueSelector,
    path: &str,
) -> Result<ValueSchema, WorkflowError> {
    let mut selected = &schema.schema;
    for (position, segment) in selector.segments.iter().enumerate() {
        selected = match segment {
            WorkflowValueSelectorSegment::Field { name } => {
                let object_type = selected.get("type").and_then(serde_json::Value::as_str);
                if object_type != Some("object") {
                    return Err(authoring_error(
                        format!("{path}.segments[{position}]"),
                        "field selector requires an exact object schema",
                    ));
                }
                selected
                    .get("properties")
                    .and_then(serde_json::Value::as_object)
                    .and_then(|properties| properties.get(name))
                    .ok_or_else(|| {
                        authoring_error(
                            format!("{path}.segments[{position}]"),
                            format!("object schema has no declared field '{name}'"),
                        )
                    })?
            }
            WorkflowValueSelectorSegment::Index { index } => {
                let array_type = selected.get("type").and_then(serde_json::Value::as_str);
                if array_type != Some("array") {
                    return Err(authoring_error(
                        format!("{path}.segments[{position}]"),
                        "index selector requires an exact array schema",
                    ));
                }
                if let Some(prefix_items) = selected
                    .get("prefixItems")
                    .and_then(serde_json::Value::as_array)
                {
                    prefix_items.get(*index).ok_or_else(|| {
                        authoring_error(
                            format!("{path}.segments[{position}]"),
                            format!("tuple schema has no member at index {index}"),
                        )
                    })?
                } else {
                    selected.get("items").ok_or_else(|| {
                        authoring_error(
                            format!("{path}.segments[{position}]"),
                            "array schema does not declare a homogeneous item schema",
                        )
                    })?
                }
            }
        };
        if selected.get("$ref").is_some()
            || selected.get("oneOf").is_some()
            || selected.get("anyOf").is_some()
            || selected.get("allOf").is_some()
        {
            return Err(authoring_error(
                format!("{path}.segments[{position}]"),
                "selector traversal through references or combinators is ambiguous",
            ));
        }
    }
    let selected_schema = ValueSchema {
        type_name: format!("{}#selector", schema.type_name),
        schema: selected.clone(),
    };
    validate_runtime_value_schema(path, &selected_schema)?;
    Ok(selected_schema)
}

fn lower_workflow_source_action(
    action: &WorkflowSourceAction,
    catalog: &WorkflowAuthoringCatalogSnapshot,
    index: usize,
) -> Result<(WorkflowBlockDefinition, serde_json::Value), WorkflowError> {
    match action {
        WorkflowSourceAction::Uses { uses, input } => {
            let block = catalog.blocks.get(uses).ok_or_else(|| {
                authoring_error(
                    format!("steps[{index}].uses"),
                    format!("exact workflow block '{uses}' is unavailable"),
                )
            })?;
            block
                .input
                .validate_value(&format!("steps[{index}].with"), input)?;
            Ok((block.clone(), input.clone()))
        }
        WorkflowSourceAction::Shorthand(fields) => {
            if fields.len() != 1 {
                return Err(authoring_error(
                    format!("steps[{index}]"),
                    "concise steps must declare exactly one shorthand action",
                ));
            }
            let (action_key, payload) = fields.first_key_value().expect("one action field");
            let matches = catalog
                .authoring_actions
                .values()
                .filter(|descriptor| descriptor.action_key == *action_key)
                .collect::<Vec<_>>();
            let descriptor = match matches.as_slice() {
                [descriptor] => *descriptor,
                [] => {
                    return Err(authoring_error(
                        format!("steps[{index}].{action_key}"),
                        format!("workflow authoring action '{action_key}' is unavailable"),
                    ));
                }
                _ => {
                    return Err(authoring_error(
                        format!("steps[{index}].{action_key}"),
                        format!("workflow authoring action '{action_key}' is ambiguous"),
                    ));
                }
            };
            descriptor
                .input
                .validate_value(&format!("steps[{index}].{action_key}"), payload)?;
            let input = if let Some(adapter) = &descriptor.input_adapter {
                adapter.evaluate(&[WorkflowTransformInput {
                    name: WORKFLOW_TRANSFORM_SOURCE_CURRENT,
                    value: payload,
                }])?
            } else {
                payload.clone()
            };
            let block = catalog
                .blocks
                .get(&descriptor.target_block)
                .ok_or_else(|| {
                    authoring_error(
                        format!("steps[{index}].{action_key}"),
                        "validated action target block became unavailable",
                    )
                })?;
            block
                .input
                .validate_value(&format!("steps[{index}].{action_key}"), &input)?;
            Ok((block.clone(), input))
        }
    }
}

struct DuplicateRejectingJsonValue(serde_json::Value);

impl<'de> Deserialize<'de> for DuplicateRejectingJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(DuplicateRejectingJsonVisitor)
    }
}

struct DuplicateRejectingJsonVisitor;

impl<'de> Visitor<'de> for DuplicateRejectingJsonVisitor {
    type Value = DuplicateRejectingJsonValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(DuplicateRejectingJsonValue(value.into()))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(DuplicateRejectingJsonValue(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(DuplicateRejectingJsonValue(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        serde_json::Number::from_f64(value).map_or_else(
            || Err(E::custom("non-finite JSON number")),
            |number| Ok(DuplicateRejectingJsonValue(number.into())),
        )
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(DuplicateRejectingJsonValue(value.into()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(DuplicateRejectingJsonValue(value.into()))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(DuplicateRejectingJsonValue(serde_json::Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(DuplicateRejectingJsonValue(serde_json::Value::Null))
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        DuplicateRejectingJsonValue::deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<DuplicateRejectingJsonValue>()? {
            values.push(value.0);
        }
        Ok(DuplicateRejectingJsonValue(values.into()))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = serde_json::Map::new();
        while let Some((key, value)) = map.next_entry::<String, DuplicateRejectingJsonValue>()? {
            if values.insert(key.clone(), value.0).is_some() {
                return Err(serde::de::Error::custom(format!(
                    "duplicate JSON object key '{key}'"
                )));
            }
        }
        Ok(DuplicateRejectingJsonValue(values.into()))
    }
}

fn decode_workflow_toml_null_markers(
    value: &mut serde_json::Value,
    depth: usize,
) -> Result<(), WorkflowError> {
    if depth > MAX_WORKFLOW_AUTHORING_JSON_DEPTH {
        return Err(authoring_error(
            "source.toml",
            format!("TOML value depth exceeds {MAX_WORKFLOW_AUTHORING_JSON_DEPTH}"),
        ));
    }
    match value {
        serde_json::Value::Object(fields) => {
            if fields.len() == 1
                && fields.get(WORKFLOW_TOML_NULL_MARKER) == Some(&serde_json::json!(true))
            {
                *value = serde_json::Value::Null;
                return Ok(());
            }
            if fields.contains_key(WORKFLOW_TOML_NULL_MARKER) {
                return Err(authoring_error(
                    "source.toml",
                    format!(
                        "reserved TOML null marker '{WORKFLOW_TOML_NULL_MARKER}' must be the only field and equal true"
                    ),
                ));
            }
            for value in fields.values_mut() {
                decode_workflow_toml_null_markers(value, depth + 1)?;
            }
        }
        serde_json::Value::Array(items) => {
            for value in items {
                decode_workflow_toml_null_markers(value, depth + 1)?;
            }
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {}
    }
    Ok(())
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
    /// Bounded plugin-operation input defaults keyed by plugin-block node identity.
    ///
    /// These values are authored separately from immutable plugin owner contracts and are
    /// validated against the exact catalog block input schema during compilation.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugin_input_defaults: BTreeMap<String, serde_json::Value>,
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugin_input_defaults: BTreeMap<String, serde_json::Value>,
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
        let mut plugin_input_defaults = normalized.plugin_input_defaults.clone();
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
        materialize_plugin_input_defaults(
            &mut definition,
            &plugin_input_defaults,
            &mut input_defaults,
        )?;
        for node_id in normalized.plugin_input_defaults.keys() {
            if !definition.entries.contains(node_id)
                && definition.edges.iter().any(|edge| {
                    edge.to == *node_id
                        && edge.transform.as_ref().is_some_and(|transform| {
                            matches!(
                                transform.expression,
                                WorkflowTransformExpression::Constant { .. }
                            )
                        })
                })
            {
                plugin_input_defaults.remove(node_id);
            }
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
    #[allow(clippy::too_many_lines)]
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
        if self.plugin_input_defaults.len() > MAX_WORKFLOW_AUTHORING_REQUIREMENTS {
            return Err(authoring_error(
                "plugin_input_defaults",
                format!(
                    "plugin input defaults exceed {MAX_WORKFLOW_AUTHORING_REQUIREMENTS} entries"
                ),
            ));
        }
        for (node_id, defaults) in &self.plugin_input_defaults {
            validate_authoring_id("plugin_input_defaults.node_id", node_id)?;
            let node = self.definition.node(node_id).ok_or_else(|| {
                authoring_error(
                    format!("plugin_input_defaults.{node_id}"),
                    "plugin input defaults reference an unknown node",
                )
            })?;
            if node.kind != NodeKind::PluginBlock {
                return Err(authoring_error(
                    format!("plugin_input_defaults.{node_id}"),
                    "plugin input defaults require a plugin-block node",
                ));
            }
            let block: WorkflowBlockDefinition = serde_json::from_value(node.configuration.clone())
                .map_err(|error| {
                    authoring_error(
                        format!("definition.nodes.{node_id}.configuration"),
                        format!("plugin block configuration is invalid: {error}"),
                    )
                })?;
            block
                .input
                .validate_value(&format!("plugin_input_defaults.{node_id}"), defaults)?;
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
            plugin_input_defaults: self.plugin_input_defaults.clone(),
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

fn materialize_plugin_input_defaults(
    definition: &mut WorkflowDefinition,
    plugin_input_defaults: &BTreeMap<String, serde_json::Value>,
    workflow_input: &mut serde_json::Value,
) -> Result<(), WorkflowError> {
    if plugin_input_defaults.is_empty() {
        return Ok(());
    }
    if definition.entries.len() != 1 {
        return Err(authoring_error(
            "plugin_input_defaults",
            "plugin input defaults require exactly one workflow entry",
        ));
    }
    for (node_id, defaults) in plugin_input_defaults {
        let node = definition.nodes.get_mut(node_id).ok_or_else(|| {
            authoring_error(
                format!("plugin_input_defaults.{node_id}"),
                "plugin input defaults reference an unknown node",
            )
        })?;
        if definition.entries.first() == Some(node_id) {
            *workflow_input = defaults.clone();
        }
        for edge in definition
            .edges
            .iter_mut()
            .filter(|edge| edge.to == *node_id)
        {
            edge.transform = Some(WorkflowTransform {
                version: WORKFLOW_TRANSFORM_VERSION,
                expression: WorkflowTransformExpression::Constant {
                    value: defaults.clone(),
                },
                output: node.input.clone(),
            });
        }
    }
    let entry = definition
        .entries
        .first()
        .and_then(|node_id| definition.nodes.get(node_id))
        .ok_or_else(|| authoring_error("definition.entries", "workflow entry node is missing"))?;
    definition.input = entry.input.clone();
    let exit = definition
        .exits
        .first()
        .and_then(|node_id| definition.nodes.get(node_id))
        .ok_or_else(|| authoring_error("definition.exits", "workflow exit node is missing"))?;
    definition.output = exit.output.clone();
    Ok(())
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
    let mut prompt: WorkflowPromptConfiguration =
        serde_json::from_value(node.configuration.clone()).map_err(|error| {
            authoring_error(
                format!("definition.nodes.{node_id}.configuration"),
                format!("prompt configuration is invalid: {error}"),
            )
        })?;
    match field {
        "agent_profile" => prompt.agent_profile = authoring_non_empty_string(field, value)?,
        "provider" => prompt.provider = authoring_optional_string(field, value)?,
        "model" => prompt.model = authoring_optional_string(field, value)?,
        _ => {
            return Err(authoring_error(
                "bindings.target.field",
                format!("unsupported agent selection field '{field}'"),
            ));
        }
    }
    prompt.validate()?;
    node.configuration = serde_json::to_value(prompt).map_err(|error| {
        authoring_error(
            format!("definition.nodes.{node_id}.configuration"),
            format!("prompt configuration cannot be serialized: {error}"),
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
        };
        let label = match unavailable.kind {
            WorkflowRequirementKind::Capability => "capability",
            WorkflowRequirementKind::Plugin => "plugin",
            WorkflowRequirementKind::Block => "block",
            WorkflowRequirementKind::Agent => "prompt profile",
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
                let prompt: WorkflowPromptConfiguration =
                    serde_json::from_value(node.configuration.clone()).map_err(|error| {
                        authoring_error(
                            format!("definition.nodes.{node_id}.configuration"),
                            format!("prompt configuration is invalid: {error}"),
                        )
                    })?;
                prompt.validate()?;
                if !catalog.agent_profiles.contains(&prompt.agent_profile) {
                    return Err(authoring_error(
                        format!("definition.nodes.{node_id}.configuration.agent_profile"),
                        format!("prompt profile '{}' is unavailable", prompt.agent_profile),
                    ));
                }
                requirements.agents.insert(prompt.agent_profile.clone());
                effects.maximum_capability = effects.maximum_capability.max(prompt.tool_capability);
                permissions.maximum_capability =
                    permissions.maximum_capability.max(prompt.tool_capability);
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
        block
            .input
            .validate_value(&format!("plugin_input_defaults.{node_id}"), defaults)?;
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

/// Exact normalized non-secret authorization policy/profile identity pinned by one run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowAuthorizationProfileIdentity {
    pub version: u32,
    pub provider_id: String,
    pub profile_id: String,
    pub policy_digest_sha256: String,
}

impl WorkflowAuthorizationProfileIdentity {
    /// Validate bounded normalized identity facts.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, malformed identities, or malformed digest.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if self.version != 1 {
            return Err(authoring_error(
                "workflow.authorization_profile.version",
                "unsupported workflow authorization profile identity version",
            ));
        }
        validate_authoring_id(
            "workflow.authorization_profile.provider_id",
            &self.provider_id,
        )?;
        validate_authoring_id(
            "workflow.authorization_profile.profile_id",
            &self.profile_id,
        )?;
        validate_sha256(
            "workflow.authorization_profile.policy_digest_sha256",
            &self.policy_digest_sha256,
        )
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

/// Maximum encoded canonical mutation input retained for exact approval review.
pub const MAX_WORKFLOW_MUTATION_APPROVAL_INPUT_BYTES: usize = 1_048_576;

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
    /// Exact owner-prepared canonical operation facts authorized for this activation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_facts: Option<serde_json::Value>,
    /// SHA-256 identity of the exact owner-issued preparation descriptor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preparation_descriptor_sha256: Option<String>,
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
            || serde_json::to_vec(&self.input_summary).map_or(true, |summary| {
                summary.len() > MAX_WORKFLOW_MUTATION_APPROVAL_INPUT_BYTES
            })
            || self.operation_facts.as_ref().is_some_and(|facts| {
                facts.is_null()
                    || serde_json::to_vec(facts).map_or(true, |encoded| {
                        encoded.len() > MAX_WORKFLOW_BLOCK_PREPARATION_BYTES
                    })
            })
            || self
                .preparation_descriptor_sha256
                .as_ref()
                .is_some_and(|checksum| {
                    checksum.len() != 64
                        || !checksum
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
                })
            || self.operation_facts.is_some() != self.preparation_descriptor_sha256.is_some()
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

/// Stable durable prompt-node configuration version.
pub const WORKFLOW_PROMPT_CONFIGURATION_VERSION: u32 = 2;

/// Typed structured-output policy for a durable prompt node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptStructuredOutputPolicy {
    pub schema: ValueSchema,
    pub strict: bool,
}

/// Versioned serializable durable prompt-node configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPromptConfiguration {
    pub version: u32,
    pub execution_target: PromptContextTarget,
    pub agent_profile: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub structured_output: PromptStructuredOutputPolicy,
    pub read_only: bool,
    pub tool_capability: WorkflowToolCapability,
    pub tool_allowlist: Vec<String>,
    pub timeout_ms: u64,
    pub prompt_mode: String,
    pub system_prompt: String,
}

impl WorkflowPromptConfiguration {
    /// Validate bounded identity and policy rules.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, empty identities, invalid timeout/prompt mode,
    /// duplicate tools, or read-only policy escalation.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if self.version != WORKFLOW_PROMPT_CONFIGURATION_VERSION {
            return Err(WorkflowError::Build {
                path: "prompt.configuration.version".to_string(),
                message: format!(
                    "unsupported prompt configuration version {}; expected {WORKFLOW_PROMPT_CONFIGURATION_VERSION}",
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
                path: "prompt.configuration".to_string(),
                message: "prompt profile, prompt mode, timeout, or system prompt is invalid"
                    .to_string(),
            });
        }
        if self.read_only && self.tool_capability != WorkflowToolCapability::ReadOnly {
            return Err(WorkflowError::Build {
                path: "prompt.tool_capability".to_string(),
                message: "read-only prompt must request the read-only tool capability".to_string(),
            });
        }
        if !self.read_only && self.tool_capability == WorkflowToolCapability::ReadOnly {
            return Err(WorkflowError::Build {
                path: "prompt.tool_capability".to_string(),
                message: "mutating prompt cannot claim a read-only tool capability".to_string(),
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
            || self
                .tool_allowlist
                .iter()
                .any(|tool| tool.trim().is_empty() || tool.len() > 256)
        {
            return Err(WorkflowError::Build {
                path: "prompt.configuration".to_string(),
                message: "prompt identity or tool fields exceed durable bounds".to_string(),
            });
        }
        let tools = self.tool_allowlist.iter().collect::<BTreeSet<_>>();
        if tools.len() != self.tool_allowlist.len() {
            return Err(WorkflowError::Build {
                path: "prompt.configuration".to_string(),
                message: "agent tool IDs must be non-empty and unique".to_string(),
            });
        }
        jsonschema::validator_for(&self.structured_output.schema.schema).map_err(|error| {
            WorkflowError::Build {
                path: "prompt.structured_output".to_string(),
                message: format!("invalid structured output schema: {error}"),
            }
        })?;
        Ok(())
    }
}

/// Execution target for a daemon-hosted workflow prompt node.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PromptContextTarget {
    /// Execute in a fresh isolated child session.
    #[default]
    FreshIsolated,
    /// Execute in a fresh child session copied from the run's parent at its pinned generation.
    FixedGenerationFork,
    /// Execute sequentially in the workflow run's parent session.
    SharedParentSequential,
}

/// Current exact child-workflow call contract version.
pub const WORKFLOW_CALL_VERSION: u32 = 2;
/// Maximum supported workflow-call nesting depth, including the root run.
pub const MAX_WORKFLOW_CALL_DEPTH: u32 = 8;
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
    /// Optional explicit child-input mapping. Source lowering materializes this on incoming edges;
    /// the exact compiled child input interface remains authoritative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<WorkflowTransform>,
    /// Optional explicit child-output mapping evaluated before the result is projected to the
    /// parent call activation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<WorkflowTransform>,
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
        self.target.validate()?;
        if let Some(input) = &self.input {
            input.validate()?;
        }
        if let Some(output) = &self.output {
            output.validate()?;
            let sources = output.referenced_sources();
            if sources != BTreeSet::from([WORKFLOW_TRANSFORM_SOURCE_CURRENT.to_string()]) {
                return Err(WorkflowError::Build {
                    path: "workflow_call.output".to_string(),
                    message:
                        "child-call output mapping may reference only the canonical child result"
                            .to_string(),
                });
            }
        }
        Ok(())
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
    pub agent_execution_targets: BTreeSet<PromptContextTarget>,
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
            (NodeKind::FanOut, WorkflowCapabilitySupport::Supported),
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
            automatic_retry_policy_version: Some(WORKFLOW_AUTOMATIC_RETRY_POLICY_VERSION),
            agent_configuration_version: 1,
            workflow_block_interface_version: WORKFLOW_BLOCK_INTERFACE_VERSION,
            node_kinds,
            edge_kinds,
            parallel_join_policies: BTreeSet::from([
                ParallelFailurePolicy::WaitAll,
                ParallelFailurePolicy::FailFast,
            ]),
            automatic_retry: WorkflowCapabilitySupport::Supported,
            fan_out: WorkflowCapabilitySupport::Supported,
            transforms: WorkflowCapabilitySupport::Supported,
            artifact_references: WorkflowCapabilitySupport::Supported,
            agent_execution_targets: BTreeSet::from([
                PromptContextTarget::FreshIsolated,
                PromptContextTarget::FixedGenerationFork,
                PromptContextTarget::SharedParentSequential,
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

/// One explicit segment in a bounded structured-value selector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowValueSelectorSegment {
    /// Select one exact object field.
    Field { name: String },
    /// Select one exact array member by zero-based index.
    Index { index: usize },
}

/// Versioned, bounded selector shared by generic predicates and transforms.
///
/// An empty segment list selects the complete input value. Field and index segments are explicit,
/// so numeric object keys are never confused with array indices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowValueSelector {
    /// Selector contract version.
    pub version: u32,
    /// Ordered traversal from the input root.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub segments: Vec<WorkflowValueSelectorSegment>,
}

impl WorkflowValueSelector {
    /// Validate this selector's version and durable bounds.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, excessive segments, empty/oversized fields, or
    /// indices above the durable bound.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if self.version != WORKFLOW_VALUE_SELECTOR_VERSION {
            return Err(WorkflowError::Build {
                path: "selector.version".to_string(),
                message: format!(
                    "unsupported workflow value selector version {}; expected {}",
                    self.version, WORKFLOW_VALUE_SELECTOR_VERSION
                ),
            });
        }
        if self.segments.len() > MAX_VALUE_SELECTOR_SEGMENTS {
            return Err(WorkflowError::Build {
                path: "selector.segments".to_string(),
                message: format!("value selector exceeds {MAX_VALUE_SELECTOR_SEGMENTS} segments"),
            });
        }
        for (position, segment) in self.segments.iter().enumerate() {
            match segment {
                WorkflowValueSelectorSegment::Field { name }
                    if name.is_empty() || name.len() > MAX_VALUE_SELECTOR_FIELD_BYTES =>
                {
                    return Err(WorkflowError::Build {
                        path: format!("selector.segments.{position}"),
                        message: format!(
                            "selector field must contain 1..={MAX_VALUE_SELECTOR_FIELD_BYTES} bytes"
                        ),
                    });
                }
                WorkflowValueSelectorSegment::Index { index }
                    if *index > MAX_VALUE_SELECTOR_INDEX =>
                {
                    return Err(WorkflowError::Build {
                        path: format!("selector.segments.{position}"),
                        message: format!(
                            "selector index exceeds durable maximum {MAX_VALUE_SELECTOR_INDEX}"
                        ),
                    });
                }
                WorkflowValueSelectorSegment::Field { .. }
                | WorkflowValueSelectorSegment::Index { .. } => {}
            }
        }
        Ok(())
    }

    fn select<'a>(
        &self,
        value: &'a serde_json::Value,
    ) -> Result<&'a serde_json::Value, WorkflowError> {
        self.validate()?;
        let mut selected = value;
        for (position, segment) in self.segments.iter().enumerate() {
            selected = match segment {
                WorkflowValueSelectorSegment::Field { name } => {
                    selected.get(name).ok_or_else(|| WorkflowError::Build {
                        path: format!("selector.segments.{position}"),
                        message: format!("selected object field '{name}' was not present"),
                    })?
                }
                WorkflowValueSelectorSegment::Index { index } => {
                    selected.get(*index).ok_or_else(|| WorkflowError::Build {
                        path: format!("selector.segments.{position}"),
                        message: format!("selected array index {index} was not present"),
                    })?
                }
            };
        }
        Ok(selected)
    }
}

/// Deterministic assertion over a selected typed workflow value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "assertion", rename_all = "snake_case")]
pub enum WorkflowValueAssertion {
    /// Require a complete UTF-8 string equal to the expected text.
    TextEquals { expected: String },
    /// Require an exact byte length for a UTF-8 string or typed artifact reference.
    ByteLength { expected: u64 },
    /// Require the SHA-256 identity of a UTF-8 string or typed artifact reference.
    Sha256 { expected: String },
}

impl WorkflowValueAssertion {
    /// Evaluate this assertion over one already-selected value.
    ///
    /// # Errors
    ///
    /// Returns an error when the selected value has the wrong schema, text is marked binary or
    /// truncated, the assertion is malformed, or an artifact does not carry the required identity.
    pub fn evaluate(&self, value: &serde_json::Value) -> Result<bool, WorkflowError> {
        match self {
            Self::TextEquals { expected } => {
                let (text, complete_utf8) = assertion_text(value)?;
                if !complete_utf8 {
                    return Err(assertion_error(
                        "text assertions require complete valid UTF-8 without truncation",
                    ));
                }
                Ok(text == expected)
            }
            Self::ByteLength { expected } => Ok(assertion_byte_length(value)? == *expected),
            Self::Sha256 { expected } => {
                validate_sha256("assertion.sha256", expected)?;
                Ok(assertion_sha256(value)? == *expected)
            }
        }
    }
}

fn assertion_error(message: impl Into<String>) -> WorkflowError {
    WorkflowError::Build {
        path: "assertion".to_string(),
        message: message.into(),
    }
}

fn assertion_text(value: &serde_json::Value) -> Result<(&str, bool), WorkflowError> {
    if let Some(text) = value.as_str() {
        return Ok((text, true));
    }
    let object = value
        .as_object()
        .ok_or_else(|| assertion_error("text assertion requires a string or typed text result"))?;
    let text = object
        .get("text")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| assertion_error("typed text result is missing text"))?;
    let encoding = object
        .get("encoding")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| assertion_error("typed text result is missing encoding"))?;
    let truncated = object
        .get("truncated")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| assertion_error("typed text result is missing truncation state"))?;
    Ok((text, encoding == "utf8" && !truncated))
}

fn assertion_byte_length(value: &serde_json::Value) -> Result<u64, WorkflowError> {
    if let Some(text) = value.as_str() {
        return u64::try_from(text.len()).map_err(|error| assertion_error(error.to_string()));
    }
    value
        .get("byte_length")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| assertion_error("byte-length assertion requires typed byte_length"))
}

fn assertion_sha256(value: &serde_json::Value) -> Result<String, WorkflowError> {
    if let Some(text) = value.as_str() {
        use sha2::Digest as _;
        return Ok(format!("{:x}", sha2::Sha256::digest(text.as_bytes())));
    }
    let checksum = value
        .get("checksum_sha256")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| assertion_error("SHA-256 assertion requires typed checksum_sha256"))?;
    validate_sha256("assertion.value.checksum_sha256", checksum)?;
    Ok(checksum.to_string())
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
    /// Apply one bounded typed assertion to an explicit selected value.
    SelectedAssertion {
        /// Predicate contract version. This operation requires version 3.
        version: u32,
        selector: WorkflowValueSelector,
        assertion: WorkflowValueAssertion,
    },
    /// Compare a value selected with explicit field/index segments to one constant.
    SelectedEquals {
        /// Predicate contract version. This operation requires version 3.
        version: u32,
        /// Explicit structured-value selector.
        selector: WorkflowValueSelector,
        /// Expected JSON value.
        value: serde_json::Value,
    },
    /// Compare two values selected from the same input with explicit field/index segments.
    SelectedValuesEqual {
        /// Predicate contract version. This operation requires version 3.
        version: u32,
        /// Left structured-value selector.
        left_selector: WorkflowValueSelector,
        /// Right structured-value selector.
        right_selector: WorkflowValueSelector,
    },
    /// Compare two selected numeric values using an exact ordering operation.
    SelectedNumericCompare {
        /// Predicate contract version. This operation requires version 3.
        version: u32,
        /// Left structured-value selector.
        left_selector: WorkflowValueSelector,
        /// Right structured-value selector.
        right_selector: WorkflowValueSelector,
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
            | Self::NumericCompare { version, .. }
            | Self::SelectedAssertion { version, .. }
            | Self::SelectedEquals { version, .. }
            | Self::SelectedValuesEqual { version, .. }
            | Self::SelectedNumericCompare { version, .. } => *version,
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

    #[allow(clippy::too_many_lines)]
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
            } => evaluate_numeric_predicate_values(
                predicate_value_at_path(value, left_path)?,
                predicate_value_at_path(value, right_path)?,
                *comparison,
                left_path,
                right_path,
            ),
            Self::SelectedAssertion {
                version: _,
                selector,
                assertion,
            } => assertion.evaluate(selector.select(value)?),
            Self::SelectedEquals {
                version: _,
                selector,
                value: expected,
            } => {
                let actual = selector.select(value)?;
                if !predicate_values_compatible(actual, expected) {
                    return Err(predicate_type_mismatch("selector", actual, expected));
                }
                Ok(actual == expected)
            }
            Self::SelectedValuesEqual {
                version: _,
                left_selector,
                right_selector,
            } => {
                let left = left_selector.select(value)?;
                let right = right_selector.select(value)?;
                if !predicate_values_compatible(left, right) {
                    return Err(predicate_type_mismatch("selector", left, right));
                }
                Ok(left == right)
            }
            Self::SelectedNumericCompare {
                version: _,
                left_selector,
                right_selector,
                comparison,
            } => evaluate_numeric_predicate_values(
                left_selector.select(value)?,
                right_selector.select(value)?,
                *comparison,
                "left_selector",
                "right_selector",
            ),
        }
    }
}

fn evaluate_numeric_predicate_values(
    left_value: &serde_json::Value,
    right_value: &serde_json::Value,
    comparison: PredicateNumericComparison,
    left_path: &str,
    right_path: &str,
) -> Result<bool, WorkflowError> {
    let serde_json::Value::Number(left) = left_value else {
        return Err(WorkflowError::Build {
            path: left_path.to_string(),
            message: format!(
                "numeric predicate expected number, found {}",
                predicate_value_kind(left_value)
            ),
        });
    };
    let serde_json::Value::Number(right) = right_value else {
        return Err(WorkflowError::Build {
            path: right_path.to_string(),
            message: format!(
                "numeric predicate expected number, found {}",
                predicate_value_kind(right_value)
            ),
        });
    };
    let ordering = compare_json_numbers(left, right).ok_or_else(|| WorkflowError::Build {
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
    /// [`WORKFLOW_TRANSFORM_SOURCE_STATE`],
    /// [`WORKFLOW_TRANSFORM_SOURCE_CONFIGURATION`], exact predecessor outputs under
    /// [`WORKFLOW_TRANSFORM_SOURCE_DEPENDENCY_PREFIX`], and, for a completed parallel join,
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
    /// Select a named input using explicit field/index segments.
    SelectedInput {
        /// Stable input source name.
        source: String,
        /// Explicit structured-value selector. This operation requires transform version 2.
        selector: WorkflowValueSelector,
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

    /// Return the exact named source inventory referenced by this expression.
    #[must_use]
    pub fn referenced_sources(&self) -> BTreeSet<String> {
        fn visit(expression: &WorkflowTransformExpression, sources: &mut BTreeSet<String>) {
            match expression {
                WorkflowTransformExpression::Input { source, .. }
                | WorkflowTransformExpression::SelectedInput { source, .. } => {
                    sources.insert(source.clone());
                }
                WorkflowTransformExpression::Object { fields } => {
                    for expression in fields.values() {
                        visit(expression, sources);
                    }
                }
                WorkflowTransformExpression::Array { items }
                | WorkflowTransformExpression::Merge { objects: items, .. } => {
                    for expression in items {
                        visit(expression, sources);
                    }
                }
                WorkflowTransformExpression::Increment { value, .. } => visit(value, sources),
                WorkflowTransformExpression::Default { value, default } => {
                    visit(value, sources);
                    visit(default, sources);
                }
                WorkflowTransformExpression::Constant { .. } => {}
            }
        }

        let mut sources = BTreeSet::new();
        visit(&self.expression, &mut sources);
        sources
    }

    /// Validate the transform contract without evaluating it.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, invalid output schemas, or expression bounds.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if !(WORKFLOW_TRANSFORM_MIN_VERSION..=WORKFLOW_TRANSFORM_VERSION).contains(&self.version) {
            return Err(WorkflowError::Build {
                path: "transform.version".to_string(),
                message: format!(
                    "unsupported workflow transform version {}; expected {} through {}",
                    self.version, WORKFLOW_TRANSFORM_MIN_VERSION, WORKFLOW_TRANSFORM_VERSION
                ),
            });
        }
        jsonschema::validator_for(&self.output.schema).map_err(|error| WorkflowError::Build {
            path: "transform.output".to_string(),
            message: format!("invalid transform output schema: {error}"),
        })?;
        let mut operations = 0_usize;
        validate_transform_expression(self.version, &self.expression, 0, &mut operations)
    }
}

fn validate_transform_expression(
    version: u32,
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
        WorkflowTransformExpression::SelectedInput { source, selector } => {
            if version < 2 {
                return Err(WorkflowError::Build {
                    path: "transform.selected_input".to_string(),
                    message: "selected input requires transform version 2".to_string(),
                });
            }
            if source.trim().is_empty() || source.len() > 256 {
                return Err(WorkflowError::Build {
                    path: "transform.selected_input".to_string(),
                    message: "transform input source exceeds bounds".to_string(),
                });
            }
            selector.validate()?;
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
                validate_transform_expression(version, value, depth + 1, operations)?;
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
                validate_transform_expression(version, value, depth + 1, operations)?;
            }
        }
        WorkflowTransformExpression::Increment { value, .. } => {
            validate_transform_expression(version, value, depth + 1, operations)?;
        }
        WorkflowTransformExpression::Default { value, default } => {
            validate_transform_expression(version, value, depth + 1, operations)?;
            validate_transform_expression(version, default, depth + 1, operations)?;
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
        WorkflowTransformExpression::SelectedInput { source, selector } => {
            let source = inputs
                .get(source.as_str())
                .ok_or_else(|| WorkflowError::Build {
                    path: source.clone(),
                    message: "transform references an unknown named input".to_string(),
                })?;
            selector.select(source).cloned()
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
            if node.kind == NodeKind::FanOut
                && serde_json::from_value::<WorkflowFanOutConfiguration>(node.configuration.clone())
                    .is_err()
            {
                diagnostics.push(WorkflowCapabilityDiagnostic {
                    code: "invalid_fan_out_configuration".to_string(),
                    node_id: Some(node.id.clone()),
                    message: format!(
                        "fan-out node '{}' does not use the durable member-operation contract",
                        node.id
                    ),
                });
            }
            if node.kind == NodeKind::PluginBlock
                && let Ok(block) =
                    serde_json::from_value::<WorkflowBlockDefinition>(node.configuration.clone())
                && let Some(retry) = &block.automatic_retry
            {
                if capabilities.automatic_retry != WorkflowCapabilitySupport::Supported {
                    diagnostics.push(WorkflowCapabilityDiagnostic {
                        code: "unsupported_automatic_retry".to_string(),
                        node_id: Some(node.id.clone()),
                        message: format!(
                            "node '{}' declares automatic retry without durable production support",
                            node.id
                        ),
                    });
                }
                if capabilities.automatic_retry_policy_version != Some(retry.version) {
                    diagnostics.push(WorkflowCapabilityDiagnostic {
                        code: "unsupported_automatic_retry_version".to_string(),
                        node_id: Some(node.id.clone()),
                        message: format!(
                            "node '{}' uses unsupported automatic retry version {}",
                            node.id, retry.version
                        ),
                    });
                }
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
                if capabilities.transform_version.is_none_or(|supported| {
                    !(WORKFLOW_TRANSFORM_MIN_VERSION..=supported).contains(&transform.version)
                }) {
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
                    if capabilities.edge_support(WorkflowEdgeKind::Retry)
                        != WorkflowCapabilitySupport::Supported =>
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

    /// Create a daemon-hosted agent step from its typed prompt configuration.
    ///
    /// Prefer this over [`Step::configured_task`] with [`NodeKind::Agent`]: the configuration is
    /// validated here, so an invalid prompt contract is reported at construction rather than
    /// surfacing later as a receipt-reconciliation failure.
    ///
    /// The step's own operation is an identity pass-through. The daemon executes the agent turn and
    /// materializes its validated output; the closure exists only so the node participates in typed
    /// composition.
    ///
    /// # Errors
    ///
    /// Returns an error when `configuration` is not a valid prompt contract or cannot be
    /// serialized.
    pub fn agent(
        name: impl Into<String>,
        configuration: &WorkflowPromptConfiguration,
    ) -> Result<Self, WorkflowError>
    where
        I: Into<O>,
    {
        configuration.validate()?;
        let configuration =
            serde_json::to_value(configuration).map_err(|error| WorkflowError::Build {
                path: "prompt.configuration".to_string(),
                message: format!("agent configuration cannot be serialized: {error}"),
            })?;
        Ok(Self::configured_task(
            name,
            NodeKind::Agent,
            configuration,
            |input: I, _context| async move { Ok(input.into()) },
        ))
    }

    /// Select where a daemon-hosted agent leaf executes.
    ///
    /// # Panics
    ///
    /// Panics when called on a composed flow or a non-agent leaf.
    #[must_use]
    pub fn agent_execution_target(mut self, target: PromptContextTarget) -> Self {
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
            .expect("prompt node configuration must be an object");
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
    body.entries = vec![fan_out_id.clone()];
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

fn validate_definition_boundary_interfaces(
    definition: &WorkflowDefinition,
) -> Result<(), WorkflowError> {
    for entry in &definition.entries {
        if definition.nodes[entry].kind == NodeKind::FanOut {
            continue;
        }
        let entry_schema = &definition.nodes[entry].input;
        if entry_schema != &definition.input {
            return Err(WorkflowError::Build {
                path: entry.clone(),
                message: format!(
                    "entry input '{}' does not match workflow input '{}'",
                    entry_schema.type_name, definition.input.type_name
                ),
            });
        }
    }
    for exit in &definition.exits {
        let exit_schema = &definition.nodes[exit].output;
        if exit_schema != &definition.output {
            return Err(WorkflowError::Build {
                path: exit.clone(),
                message: format!(
                    "exit output '{}' does not match workflow output '{}'",
                    exit_schema.type_name, definition.output.type_name
                ),
            });
        }
    }
    Ok(())
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
            let dependency_sources = transform
                .referenced_sources()
                .into_iter()
                .filter_map(|source| {
                    source
                        .strip_prefix(WORKFLOW_TRANSFORM_SOURCE_DEPENDENCY_PREFIX)
                        .map(str::to_string)
                })
                .collect::<BTreeSet<_>>();
            let declared_dependencies = definition
                .edges
                .iter()
                .filter(|candidate| candidate.to == edge.to)
                .map(|candidate| candidate.from.clone())
                .collect::<BTreeSet<_>>();
            if !dependency_sources.is_subset(&declared_dependencies) {
                return Err(WorkflowError::Build {
                    path: edge.to.clone(),
                    message: "edge transform references an undeclared predecessor output"
                        .to_string(),
                });
            }
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
        let contract =
            match serde_json::from_value::<WorkflowPromptConfiguration>(node.configuration.clone())
            {
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
        || Ok(PromptContextTarget::FreshIsolated),
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

#[allow(clippy::too_many_lines)]
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
        PredicateExpression::SelectedAssertion {
            selector,
            assertion,
            ..
        } => {
            if version < 3 {
                return Err(predicate_operation_requires_version_three(expression));
            }
            selector.validate()?;
            let encoded = serde_json::to_vec(assertion).map_err(|error| WorkflowError::Build {
                path: "assertion".to_string(),
                message: format!("assertion cannot be serialized: {error}"),
            })?;
            if encoded.len() > MAX_PREDICATE_VALUE_BYTES {
                return Err(WorkflowError::Build {
                    path: "assertion".to_string(),
                    message: format!("assertion exceeds {MAX_PREDICATE_VALUE_BYTES} bytes"),
                });
            }
            if let WorkflowValueAssertion::Sha256 { expected } = assertion {
                validate_sha256("assertion.sha256", expected)?;
            }
        }
        PredicateExpression::SelectedEquals {
            selector, value, ..
        } => {
            if version < 3 {
                return Err(predicate_operation_requires_version_three(expression));
            }
            selector.validate()?;
            let encoded = serde_json::to_vec(value).map_err(|error| WorkflowError::Build {
                path: "selector".to_string(),
                message: format!("predicate value cannot be serialized: {error}"),
            })?;
            if encoded.len() > MAX_PREDICATE_VALUE_BYTES {
                return Err(WorkflowError::Build {
                    path: "selector".to_string(),
                    message: format!("predicate value exceeds {MAX_PREDICATE_VALUE_BYTES} bytes"),
                });
            }
        }
        PredicateExpression::SelectedValuesEqual {
            left_selector,
            right_selector,
            ..
        }
        | PredicateExpression::SelectedNumericCompare {
            left_selector,
            right_selector,
            ..
        } => {
            if version < 3 {
                return Err(predicate_operation_requires_version_three(expression));
            }
            left_selector.validate()?;
            right_selector.validate()?;
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

fn predicate_operation_requires_version_three(expression: &PredicateExpression) -> WorkflowError {
    WorkflowError::Build {
        path: "predicate".to_string(),
        message: format!(
            "predicate operation requires version 3, found {}",
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

#[allow(clippy::too_many_lines)]
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
    if node.kind == NodeKind::FanOut {
        if node.configuration.get("member_node").is_some() {
            let configuration: WorkflowFanOutConfiguration =
                serde_json::from_value(node.configuration.clone()).map_err(|error| {
                    WorkflowError::Build {
                        path: node.id.clone(),
                        message: format!("fan-out configuration is invalid: {error}"),
                    }
                })?;
            configuration.validate()?;
            let items = node
                .input
                .schema
                .get("items")
                .ok_or_else(|| WorkflowError::Build {
                    path: node.id.clone(),
                    message: "fan-out input must declare homogeneous items".to_string(),
                })?;
            if items != &configuration.member_node.input.schema {
                return Err(WorkflowError::Build {
                    path: node.id.clone(),
                    message: "fan-out member input schema does not match array items".to_string(),
                });
            }
            let expected_output =
                workflow_fan_out_result_schema(&configuration.member_node.output)?;
            if node.output != expected_output {
                return Err(WorkflowError::Build {
                    path: node.id.clone(),
                    message: "fan-out output schema does not match the member operation"
                        .to_string(),
                });
            }
        } else if node
            .configuration
            .get("fan_out_version")
            .and_then(serde_json::Value::as_u64)
            != Some(u64::from(WORKFLOW_FAN_OUT_RESULT_VERSION))
            || node
                .configuration
                .get("ordering")
                .and_then(serde_json::Value::as_str)
                != Some("input_index_ascending")
        {
            return Err(WorkflowError::Build {
                path: node.id.clone(),
                message: format!(
                    "fan_out must declare version {WORKFLOW_FAN_OUT_RESULT_VERSION} and input-index ordering"
                ),
            });
        }
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

    fn valid_prompt_configuration() -> WorkflowPromptConfiguration {
        WorkflowPromptConfiguration {
            version: WORKFLOW_PROMPT_CONFIGURATION_VERSION,
            execution_target: PromptContextTarget::FreshIsolated,
            agent_profile: "build".to_string(),
            provider: None,
            model: None,
            structured_output: PromptStructuredOutputPolicy {
                schema: ValueSchema::of::<u32>(),
                strict: true,
            },
            read_only: true,
            tool_capability: WorkflowToolCapability::ReadOnly,
            tool_allowlist: Vec::new(),
            timeout_ms: 30_000,
            prompt_mode: "json_input".to_string(),
            system_prompt: String::new(),
        }
    }

    #[test]
    fn agent_step_builds_an_agent_node_carrying_its_prompt_contract() {
        let configuration = valid_prompt_configuration();
        let step =
            Step::<u32, u32>::agent("agent", &configuration).expect("valid prompt configuration");
        let flow = WorkflowBuilder::new("agent-flow", step)
            .build()
            .expect("workflow builds");
        let definition = flow.definition();
        let node = definition.nodes.get("agent").expect("agent node");
        assert_eq!(node.kind, NodeKind::Agent);
        // The stored configuration must round-trip as the typed contract, which is what receipt
        // reconciliation later requires.
        let stored: WorkflowPromptConfiguration =
            serde_json::from_value(node.configuration.clone()).expect("typed prompt contract");
        assert_eq!(stored, configuration);
    }

    #[test]
    fn agent_step_rejects_an_invalid_prompt_contract_at_construction() {
        // An unsupported contract version is exactly the class of error that previously escaped
        // construction and only surfaced as a receipt-reconciliation failure at runtime.
        let mut configuration = valid_prompt_configuration();
        configuration.version = WORKFLOW_PROMPT_CONFIGURATION_VERSION + 1;
        let error = Step::<u32, u32>::agent("agent", &configuration)
            .expect_err("invalid prompt configuration must be rejected");
        assert!(
            matches!(error, WorkflowError::Build { .. }),
            "expected a build error, got {error:?}"
        );
    }

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
            plugin_input_defaults: BTreeMap::new(),
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
                        configuration: serde_json::to_value(WorkflowPromptConfiguration {
                            version: WORKFLOW_PROMPT_CONFIGURATION_VERSION,
                            execution_target: PromptContextTarget::FreshIsolated,
                            agent_profile: "review".to_string(),
                            provider: None,
                            model: None,
                            structured_output: PromptStructuredOutputPolicy {
                                schema: value_schema,
                                strict: true,
                            },
                            read_only: true,
                            tool_capability: WorkflowToolCapability::ReadOnly,
                            tool_allowlist: Vec::new(),
                            timeout_ms: 30_000,
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

    #[test]
    fn shell_v2_source_example_compiles_and_routes_only_on_generic_typed_data() {
        let source =
            include_str!("../../../fixtures/workflows/shell-v2-exit-routing.workflow.json");
        let document = decode_workflow_authoring_source(source, WorkflowSourceFormat::Json)
            .expect("shell v2 source example");
        let block: WorkflowBlockDefinition =
            serde_json::from_value(document.definition.nodes["run_shell"].configuration.clone())
                .expect("exact shell block");
        assert_eq!(block.plugin_id, "bcode.shell");
        assert_eq!(block.block_id, "exec");
        assert_eq!(block.block_version, 1);
        let mut catalog = WorkflowAuthoringCatalogSnapshot {
            version: WORKFLOW_AUTHORING_CATALOG_VERSION,
            capabilities: WorkflowAuthoringCapabilitySummary::from(
                &WorkflowProductionCapabilities::current(),
            ),
            plugins: BTreeSet::from(["bcode.shell".to_string()]),
            blocks: BTreeMap::from([(workflow_block_catalog_key(&block), block.clone())]),
            node_configuration_schemas: workflow_node_configuration_schemas(),
            workflow_definitions: BTreeMap::new(),
            agent_profiles: BTreeSet::new(),
            authoring_actions: BTreeMap::new(),
        };
        catalog.validate().expect("shell example catalog");
        let preview = document.compilation_preview(&catalog, None);
        assert!(
            preview.is_compiled(),
            "{:?}",
            preview.validation.diagnostics
        );
        let compiled = preview.compiled.expect("compiled shell source");
        assert_eq!(
            compiled.input_defaults["commands"][0]["accepted_exit_codes"],
            serde_json::json!([0, 7])
        );
        assert!(compiled.requirements.blocks.contains("bcode.shell/exec@1"));
        assert_eq!(
            compiled.effects.block_effects,
            BTreeSet::from([WorkflowBlockEffect::Mutating])
        );
        let predicate = document.definition.nodes["route_exit"]
            .configuration
            .get("predicate")
            .cloned()
            .and_then(|value| serde_json::from_value::<PredicateExpression>(value).ok())
            .expect("generic route predicate");
        assert!(
            predicate
                .evaluate_value(&serde_json::json!({
                    "commands": [{"exit_accepted": true}]
                }))
                .expect("accepted route")
        );
        assert!(
            !predicate
                .evaluate_value(&serde_json::json!({
                    "commands": [{"exit_accepted": false}]
                }))
                .expect("unaccepted route")
        );
        catalog.blocks.clear();
        assert!(
            document
                .compilation_preview(&catalog, None)
                .compiled
                .is_none()
        );
    }

    fn source_interface_schema(type_name: &str) -> ValueSchema {
        ValueSchema {
            type_name: type_name.to_string(),
            schema: serde_json::json!({
                "$schema": WORKFLOW_AUTHORING_JSON_SCHEMA_DIALECT,
                "type": "object",
                "additionalProperties": false,
                "required": ["message"],
                "properties": {"message": {"type": "string"}}
            }),
        }
    }

    fn single_gate_source(
        input: Option<&ValueSchema>,
        output: Option<&ValueSchema>,
    ) -> serde_json::Value {
        let mut source = serde_json::json!({
            "workflow_source_version": WORKFLOW_SOURCE_DOCUMENT_VERSION,
            "workflow_id": "example/interfaces",
            "title": "Explicit interfaces",
            "steps": [{
                "id": "gate",
                "input": {"schema": source_interface_schema("example.interface/v1")}
            }]
        });
        if let Some(input) = input {
            source["input"] = serde_json::to_value(input).expect("input interface");
        }
        if let Some(output) = output {
            source["output"] = serde_json::to_value(output).expect("output interface");
        }
        source
    }

    #[test]
    fn structured_source_declares_or_derives_versioned_interfaces() {
        let schema = source_interface_schema("example.interface/v1");
        let source = single_gate_source(None, None);
        let lowered = lower_workflow_authoring_source(
            &serde_json::to_string(&source).expect("source"),
            WorkflowSourceFormat::Json,
            &authoring_catalog(),
        )
        .expect("legacy source interface derivation");
        assert_eq!(lowered.document.definition.input, schema);
        assert_eq!(lowered.document.definition.output, schema);

        let source = single_gate_source(Some(&schema), Some(&schema));
        let lowered = lower_workflow_authoring_source(
            &serde_json::to_string(&source).expect("source"),
            WorkflowSourceFormat::Json,
            &authoring_catalog(),
        )
        .expect("explicit source interfaces");
        assert_eq!(lowered.document.definition.input, schema);
        assert_eq!(lowered.document.definition.output, schema);
    }

    #[test]
    fn structured_source_rejects_entry_and_terminal_interface_mismatches() {
        let input = source_interface_schema("example.interface/v1");
        let other = source_interface_schema("example.other/v1");
        let mut source = single_gate_source(Some(&input), Some(&input));
        source["input"] = serde_json::to_value(&other).expect("interface");
        let error = lower_workflow_authoring_source(
            &serde_json::to_string(&source).expect("source"),
            WorkflowSourceFormat::Json,
            &authoring_catalog(),
        )
        .expect_err("entry interface mismatch");
        assert!(error.to_string().contains("entry input interface"));

        let source = single_gate_source(Some(&input), Some(&other));
        let error = lower_workflow_authoring_source(
            &serde_json::to_string(&source).expect("source"),
            WorkflowSourceFormat::Json,
            &authoring_catalog(),
        )
        .expect_err("terminal interface mismatch");
        assert!(error.to_string().contains("terminal output interface"));
    }

    #[test]
    fn compiled_definition_rejects_boundary_interface_mismatches() {
        let mut document = authored_document();
        document.definition.input = source_interface_schema("example.wrong-input/v1");
        document
            .definition
            .validate()
            .expect("canonical boundary validation remains host-neutral");
        assert!(validate_definition_boundary_interfaces(&document.definition).is_err());

        let mut document = authored_document();
        document.definition.output = source_interface_schema("example.wrong-output/v1");
        document
            .definition
            .validate()
            .expect("canonical boundary validation remains host-neutral");
        assert!(validate_definition_boundary_interfaces(&document.definition).is_err());
    }

    #[test]
    fn canonical_yaml_decodes_to_identical_semantics() {
        let document = authored_document();
        let yaml = yaml_serde::to_string(&document).expect("YAML source");
        let decoded = decode_workflow_authoring_source(&yaml, WorkflowSourceFormat::Yaml)
            .expect("canonical YAML workflow");
        assert_eq!(decoded, document);
        assert!(
            decode_workflow_authoring_source("key: !custom value", WorkflowSourceFormat::Yaml)
                .is_err()
        );
        assert!(
            decode_workflow_authoring_source(
                "base: &base value\ncopy: *base",
                WorkflowSourceFormat::Yaml
            )
            .is_err()
        );
        assert!(
            decode_workflow_authoring_source(
                "base: &base {x: 1}\nvalue:\n  <<: *base",
                WorkflowSourceFormat::Yaml
            )
            .is_err()
        );
        assert!(
            decode_workflow_authoring_source("value: .nan", WorkflowSourceFormat::Yaml).is_err()
        );
        assert!(
            decode_workflow_authoring_source("? [complex]\n: value", WorkflowSourceFormat::Yaml)
                .is_err()
        );
    }

    #[test]
    fn structured_source_v3_generic_dataflow_is_equivalent_across_formats() {
        let schema = serde_json::json!({"type_name": "example.value/v1", "schema": {}});
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/cross-format-dataflow",
            "title": "Cross format dataflow",
            "steps": [{"id": "first", "input": {"schema": schema}}, {
                "id": "second",
                "needs": ["first"],
                "input_expression": {
                    "version": WORKFLOW_TRANSFORM_VERSION,
                    "expression": {"operation": "object", "fields": {
                        "constant": {"operation": "constant", "value": true},
                        "prior": {"operation": "input", "source": "dependency.first", "path": ""}
                    }},
                    "output": schema
                },
                "input": {"schema": schema}
            }]
        });
        let json = serde_json::to_string(&source).expect("JSON");
        let yaml = yaml_serde::to_string(&source).expect("YAML");
        let toml = toml::to_string_pretty(&source).expect("TOML");
        let lowered = [
            (WorkflowSourceFormat::Json, json),
            (WorkflowSourceFormat::Yaml, yaml),
            (WorkflowSourceFormat::Toml, toml),
        ]
        .map(|(format, source)| {
            lower_workflow_authoring_source(&source, format, &authoring_catalog())
                .expect("lower source")
        });
        assert_eq!(lowered[0].document, lowered[1].document);
        assert_eq!(lowered[0].document, lowered[2].document);
        assert_eq!(lowered[0].source_map, lowered[1].source_map);
        assert_eq!(lowered[0].source_map, lowered[2].source_map);
    }

    #[test]
    fn workflow_package_cross_format_members_lower_byte_equivalently() {
        let json = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/member",
            "title": "Member",
            "input": {"type_name": "value/v1", "schema": {"type": "string"}},
            "output": {"type_name": "value/v1", "schema": {"type": "string"}},
            "steps": [{
                "id": "input",
                "input": {"schema": {"type_name": "value/v1", "schema": {"type": "string"}}}
            }]
        });
        let sources = [
            (
                WorkflowSourceFormat::Json,
                "member.json",
                serde_json::to_string(&json).expect("json"),
            ),
            (
                WorkflowSourceFormat::Yaml,
                "member.yaml",
                "workflow_source_version: 3\nworkflow_id: example/member\ntitle: Member\ninput: {type_name: value/v1, schema: {type: string}}\noutput: {type_name: value/v1, schema: {type: string}}\nsteps:\n  - id: input\n    input:\n      schema:\n        type_name: value/v1\n        schema:\n          type: string\n"
                    .to_string(),
            ),
            (
                WorkflowSourceFormat::Toml,
                "member.toml",
                "workflow_source_version = 3\nworkflow_id = \"example/member\"\ntitle = \"Member\"\n\n[input]\ntype_name = \"value/v1\"\n[input.schema]\ntype = \"string\"\n\n[output]\ntype_name = \"value/v1\"\n[output.schema]\ntype = \"string\"\n\n[[steps]]\nid = \"input\"\n[steps.input.schema]\ntype_name = \"value/v1\"\n[steps.input.schema.schema]\ntype = \"string\"\n"
                    .to_string(),
            ),
        ];
        let plans = sources.map(|(format, source_name, source)| {
            plan_workflow_package(
                &WorkflowPackageManifest {
                    version: WORKFLOW_PACKAGE_MANIFEST_VERSION,
                    package_id: "example/package".to_string(),
                    exports: BTreeMap::from([("main".to_string(), "member".to_string())]),
                    external_dependencies: BTreeMap::new(),
                    imports: Vec::new(),
                    members: vec![WorkflowPackageMember {
                        member_id: "member".to_string(),
                        source_name: source_name.to_string(),
                        format,
                        source,
                        dependencies: Vec::new(),
                        external_dependencies: Vec::new(),
                    }],
                },
                &authoring_catalog(),
            )
            .expect("package plan")
        });
        let canonical =
            serde_json::to_vec(&plans[0].members[0].lowering.document).expect("canonical document");
        for plan in &plans[1..] {
            assert_eq!(
                serde_json::to_vec(&plan.members[0].lowering.document).expect("document"),
                canonical
            );
            assert_eq!(
                plan.members[0].definition_identity,
                plans[0].members[0].definition_identity
            );
        }
    }

    #[test]
    fn workflow_package_plan_resolves_declared_external_calls_to_exact_targets() {
        let schema = ValueSchema {
            type_name: "example.external/v1".to_string(),
            schema: serde_json::json!({"type": "string"}),
        };
        let external = WorkflowDefinition {
            schema_version: WORKFLOW_DEFINITION_SCHEMA_VERSION,
            name: "external/child".to_string(),
            input: schema.clone(),
            output: schema.clone(),
            nodes: BTreeMap::from([(
                "child".to_string(),
                NodeDefinition {
                    id: "child".to_string(),
                    name: "Child".to_string(),
                    kind: NodeKind::Input,
                    dataflow: WorkflowNodeDataflowPolicy::Direct,
                    input: schema.clone(),
                    output: schema,
                    resources: Vec::new(),
                    configuration: serde_json::json!({"gate_version": 1}),
                },
            )]),
            entries: vec!["child".to_string()],
            exits: vec!["child".to_string()],
            edges: Vec::new(),
        };
        let identity = WorkflowDefinitionIdentity::for_definition("external/child", &external)
            .expect("identity");
        let mut catalog = authoring_catalog();
        catalog
            .workflow_definitions
            .insert(identity.definition_id.clone(), external);
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/parent",
            "title": "Parent",
            "steps": [{"id": "child", "package_call": {"external": "shared"}}]
        });
        let manifest = WorkflowPackageManifest {
            version: WORKFLOW_PACKAGE_MANIFEST_VERSION,
            package_id: "example/importer".to_string(),
            exports: BTreeMap::from([("main".to_string(), "parent".to_string())]),
            external_dependencies: BTreeMap::new(),
            imports: vec![WorkflowPackageImport {
                import_id: "shared".to_string(),
                package_id: "external/package".to_string(),
                export: "main".to_string(),
                manifest: None,
                target: Some(WorkflowCallTarget::Definition {
                    identity: identity.clone(),
                }),
                package_lock_digest_sha256: Some("a".repeat(64)),
            }],
            members: vec![WorkflowPackageMember {
                member_id: "parent".to_string(),
                source_name: "parent.json".to_string(),
                format: WorkflowSourceFormat::Json,
                source: serde_json::to_string(&source).expect("source"),
                dependencies: Vec::new(),
                external_dependencies: vec!["shared".to_string()],
            }],
        };
        let plan = plan_workflow_package(&manifest, &catalog).expect("package plan");
        let call: WorkflowCallConfiguration = serde_json::from_value(
            plan.members[0].lowering.document.definition.nodes["child"]
                .configuration
                .clone(),
        )
        .expect("call");
        assert_eq!(call.target.definition_identity(), &identity);

        let mut undeclared = manifest;
        undeclared.members[0].external_dependencies.clear();
        assert!(plan_workflow_package(&undeclared, &catalog).is_err());
    }

    #[test]
    fn package_plan_persists_the_exact_compiled_definition_used_by_its_identity() {
        let schema = ValueSchema {
            type_name: "example.value/v1".to_string(),
            schema: serde_json::json!({"type": "string"}),
        };
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/compiled-plan",
            "title": "Compiled plan",
            "input": schema,
            "output": schema,
            "steps": [{
                "id": "gate",
                "input": {"schema": schema}
            }]
        });
        let manifest = WorkflowPackageManifest {
            version: WORKFLOW_PACKAGE_MANIFEST_VERSION,
            package_id: "example/compiled-plan-package".to_string(),
            exports: BTreeMap::from([("main".to_string(), "main".to_string())]),
            external_dependencies: BTreeMap::new(),
            imports: Vec::new(),
            members: vec![WorkflowPackageMember {
                member_id: "main".to_string(),
                source_name: "main.json".to_string(),
                format: WorkflowSourceFormat::Json,
                source: serde_json::to_string(&source).expect("source"),
                dependencies: Vec::new(),
                external_dependencies: Vec::new(),
            }],
        };
        let plan = plan_workflow_package(&manifest, &authoring_catalog()).expect("plan");
        let member = &plan.members[0];
        assert_eq!(
            member.definition_identity,
            WorkflowDefinitionIdentity::for_definition(
                &member.definition_identity.kind,
                &member.lowering.document.definition,
            )
            .expect("exact identity")
        );
    }

    #[test]
    fn workflow_package_mutation_contracts_enforce_atomic_results() {
        let plan = workflow_package_test_plan();
        let apply = WorkflowPackageApplyRequest {
            version: WORKFLOW_PACKAGE_MUTATION_VERSION,
            plan: plan.clone(),
            expected_generations: Vec::new(),
        };
        apply.validate().expect("apply request");
        let publish = WorkflowPackagePublishRequest {
            version: WORKFLOW_PACKAGE_MUTATION_VERSION,
            package_id: plan.package_id.clone(),
            expected_lock: plan.lock.clone(),
            expected_generations: vec![WorkflowPackageExpectedGeneration {
                member_id: "member".to_string(),
                expected_generation: 1,
            }],
        };
        publish.validate().expect("publish request");
        WorkflowPackageMutationResult {
            version: WORKFLOW_PACKAGE_MUTATION_VERSION,
            package_id: plan.package_id.clone(),
            outcome: WorkflowPackageMutationOutcome::Applied,
            members: vec![WorkflowPackageMutationMemberResult {
                member_id: "member".to_string(),
                generation: 1,
                revision: None,
                definition_identity: plan.members[0].definition_identity.clone(),
            }],
            lock: Some(plan.lock),
            diagnostics: Vec::new(),
        }
        .validate()
        .expect("successful result");
    }

    #[test]
    fn workflow_package_mutation_contracts_reject_partial_conflict_and_bad_generations() {
        let plan = workflow_package_test_plan();
        let partial = WorkflowPackageMutationResult {
            version: 1,
            package_id: plan.package_id.clone(),
            outcome: WorkflowPackageMutationOutcome::Conflict,
            members: vec![WorkflowPackageMutationMemberResult {
                member_id: "member".to_string(),
                generation: 2,
                revision: None,
                definition_identity: plan.members[0].definition_identity.clone(),
            }],
            lock: None,
            diagnostics: Vec::new(),
        };
        assert!(partial.validate().is_err());
        assert!(
            WorkflowPackageApplyRequest {
                version: 1,
                plan,
                expected_generations: vec![WorkflowPackageExpectedGeneration {
                    member_id: "member".to_string(),
                    expected_generation: 0,
                }],
            }
            .validate()
            .is_err()
        );
    }

    fn workflow_package_test_plan() -> WorkflowPackagePlan {
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/member",
            "title": "Member",
            "input": {"type_name": "value/v1", "schema": {"type": "string"}},
            "output": {"type_name": "value/v1", "schema": {"type": "string"}},
            "steps": [{
                "id": "input",
                "input": {"schema": {"type_name": "value/v1", "schema": {"type": "string"}}}
            }]
        });
        let manifest = WorkflowPackageManifest {
            version: WORKFLOW_PACKAGE_MANIFEST_VERSION,
            package_id: "example/package".to_string(),
            exports: BTreeMap::from([("main".to_string(), "member".to_string())]),
            external_dependencies: BTreeMap::new(),
            imports: Vec::new(),
            members: vec![WorkflowPackageMember {
                member_id: "member".to_string(),
                source_name: "member.json".to_string(),
                format: WorkflowSourceFormat::Json,
                source: serde_json::to_string(&source).expect("source"),
                dependencies: Vec::new(),
                external_dependencies: Vec::new(),
            }],
        };
        plan_workflow_package(&manifest, &authoring_catalog()).expect("plan")
    }

    #[test]
    fn workflow_package_closure_resolves_recursive_exports_before_importers() {
        let source = |workflow_id: &str, title: &str, call: Option<&str>| {
            let step = call.map_or_else(
                || {
                    serde_json::json!({
                        "id": "value",
                        "input": {"schema": {"type_name": "value/v1", "schema": {"type": "string"}}}
                    })
                },
                |import| {
                    serde_json::json!({
                        "id": "value",
                        "package_call": {"external": import}
                    })
                },
            );
            serde_json::to_string(&serde_json::json!({
                "workflow_source_version": WORKFLOW_SOURCE_DOCUMENT_VERSION,
                "workflow_id": workflow_id,
                "title": title,
                "steps": [step]
            }))
            .expect("source")
        };
        let package = |package_id: &str,
                       workflow_id: &str,
                       title: &str,
                       import: Option<(&str, &str)>| {
            let imports = import.map_or_else(Vec::new, |(import_id, imported_package)| {
                vec![WorkflowPackageImport {
                    import_id: import_id.to_string(),
                    package_id: imported_package.to_string(),
                    export: "main".to_string(),
                    manifest: Some(format!("{imported_package}.workflow-package.json")),
                    target: None,
                    package_lock_digest_sha256: None,
                }]
            });
            WorkflowPackageClosureSource {
                package_id: package_id.to_string(),
                source_name: Some(format!("{package_id}.workflow-package.json")),
                manifest: WorkflowPackageManifest {
                    version: WORKFLOW_PACKAGE_MANIFEST_VERSION,
                    package_id: package_id.to_string(),
                    exports: BTreeMap::from([("main".to_string(), "main".to_string())]),
                    external_dependencies: BTreeMap::new(),
                    imports,
                    members: vec![WorkflowPackageMember {
                        member_id: "main".to_string(),
                        source_name: "main.workflow.json".to_string(),
                        format: WorkflowSourceFormat::Json,
                        source: source(workflow_id, title, import.map(|(import_id, _)| import_id)),
                        dependencies: Vec::new(),
                        external_dependencies: import
                            .map(|(import_id, _)| vec![import_id.to_string()])
                            .unwrap_or_default(),
                    }],
                },
            }
        };
        let closure = WorkflowPackageClosure {
            version: WORKFLOW_PACKAGE_CLOSURE_VERSION,
            entry_package_id: "root".to_string(),
            packages: vec![
                package("root", "example/root", "Root", Some(("middle", "middle"))),
                package("middle", "example/middle", "Middle", Some(("leaf", "leaf"))),
                package("leaf", "example/leaf", "Leaf", None),
            ],
        };
        let plan = plan_workflow_package_closure(&closure, &authoring_catalog())
            .expect("recursive closure plan");
        assert_eq!(
            plan.packages
                .iter()
                .map(|package| package.package_id.as_str())
                .collect::<Vec<_>>(),
            ["leaf", "middle", "root"]
        );
        for package in &plan.packages[1..] {
            assert_eq!(package.plan.lock.imports.len(), 1);
            assert!(
                !package.plan.lock.imports[0]
                    .package_lock_digest_sha256
                    .is_empty()
            );
            let call: WorkflowCallConfiguration = serde_json::from_value(
                package.plan.members[0].lowering.document.definition.nodes["value"]
                    .configuration
                    .clone(),
            )
            .expect("compiled imported call");
            assert!(matches!(call.target, WorkflowCallTarget::Definition { .. }));
        }
    }

    #[test]
    fn workflow_package_closure_rejects_cycles_missing_exports_drift_and_unreachable_packages() {
        let source = serde_json::to_string(&serde_json::json!({
            "workflow_source_version": WORKFLOW_SOURCE_DOCUMENT_VERSION,
            "workflow_id": "example/member",
            "title": "Member",
            "steps": [{
                "id": "value",
                "input": {"schema": {"type_name": "value/v1", "schema": {"type": "string"}}}
            }]
        }))
        .expect("source");
        let package = |id: &str, imported: Option<&str>| WorkflowPackageClosureSource {
            package_id: id.to_string(),
            source_name: Some(format!("{id}.workflow-package.json")),
            manifest: WorkflowPackageManifest {
                version: WORKFLOW_PACKAGE_MANIFEST_VERSION,
                package_id: id.to_string(),
                exports: BTreeMap::from([("main".to_string(), "main".to_string())]),
                external_dependencies: BTreeMap::new(),
                imports: imported.map_or_else(Vec::new, |dependency| {
                    vec![WorkflowPackageImport {
                        import_id: dependency.to_string(),
                        package_id: dependency.to_string(),
                        export: "main".to_string(),
                        manifest: None,
                        target: None,
                        package_lock_digest_sha256: None,
                    }]
                }),
                members: vec![WorkflowPackageMember {
                    member_id: "main".to_string(),
                    source_name: "main.workflow.json".to_string(),
                    format: WorkflowSourceFormat::Json,
                    source: source.clone(),
                    dependencies: Vec::new(),
                    external_dependencies: Vec::new(),
                }],
            },
        };
        let cycle = WorkflowPackageClosure {
            version: WORKFLOW_PACKAGE_CLOSURE_VERSION,
            entry_package_id: "a".to_string(),
            packages: vec![package("a", Some("b")), package("b", Some("a"))],
        };
        assert!(plan_workflow_package_closure(&cycle, &authoring_catalog()).is_err());

        let mut missing = WorkflowPackageClosure {
            version: WORKFLOW_PACKAGE_CLOSURE_VERSION,
            entry_package_id: "a".to_string(),
            packages: vec![package("a", Some("b")), package("b", None)],
        };
        missing.packages[0].manifest.imports[0].export = "missing".to_string();
        assert!(plan_workflow_package_closure(&missing, &authoring_catalog()).is_err());

        let mut drift = missing.clone();
        drift.packages[0].manifest.imports[0].export = "main".to_string();
        drift.packages[0].manifest.imports[0].target = Some(WorkflowCallTarget::Definition {
            identity: WorkflowDefinitionIdentity {
                kind: "wrong".to_string(),
                definition_id: "wrong".to_string(),
                definition_version: WORKFLOW_DEFINITION_SCHEMA_VERSION,
            },
        });
        drift.packages[0].manifest.imports[0].package_lock_digest_sha256 = Some("a".repeat(64));
        assert!(plan_workflow_package_closure(&drift, &authoring_catalog()).is_err());

        let unreachable = WorkflowPackageClosure {
            version: WORKFLOW_PACKAGE_CLOSURE_VERSION,
            entry_package_id: "a".to_string(),
            packages: vec![package("a", None), package("unused", None)],
        };
        assert!(plan_workflow_package_closure(&unreachable, &authoring_catalog()).is_err());
    }

    #[test]
    fn workflow_package_plan_compiles_children_before_exact_parent_calls() {
        let child = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/child",
            "title": "Child",
            "steps": [{
                "id": "input",
                "input": {"schema": {"type_name": "value/v1", "schema": {"type": "string"}}}
            }]
        });
        let parent = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/parent",
            "title": "Parent",
            "steps": [{"id": "child", "package_call": {"member": "child"}}]
        });
        let manifest = WorkflowPackageManifest {
            version: WORKFLOW_PACKAGE_MANIFEST_VERSION,
            package_id: "example/package".to_string(),
            exports: BTreeMap::from([("main".to_string(), "parent".to_string())]),
            external_dependencies: BTreeMap::new(),
            imports: Vec::new(),
            members: vec![
                WorkflowPackageMember {
                    member_id: "parent".to_string(),
                    source_name: "parent.json".to_string(),
                    format: WorkflowSourceFormat::Json,
                    source: serde_json::to_string(&parent).expect("parent"),
                    external_dependencies: Vec::new(),
                    dependencies: vec!["child".to_string()],
                },
                WorkflowPackageMember {
                    member_id: "child".to_string(),
                    source_name: "child.json".to_string(),
                    format: WorkflowSourceFormat::Json,
                    source: serde_json::to_string(&child).expect("child"),
                    dependencies: Vec::new(),
                    external_dependencies: Vec::new(),
                },
            ],
        };
        let plan = plan_workflow_package(&manifest, &authoring_catalog()).expect("package plan");
        assert_eq!(
            plan.members
                .iter()
                .map(|member| member.member_id.as_str())
                .collect::<Vec<_>>(),
            ["child", "parent"]
        );
        let parent = &plan.members[1];
        assert_eq!(parent.member_source_map.member_id, "parent");
        assert_eq!(parent.member_source_map.source_name, "parent.json");
        assert_eq!(
            parent.member_source_map.source_map,
            parent.lowering.source_map
        );
        let call = &parent.lowering.document.definition.nodes["child"];
        assert_eq!(call.kind, NodeKind::WorkflowCall);
        let configuration: WorkflowCallConfiguration =
            serde_json::from_value(call.configuration.clone()).expect("call");
        assert_eq!(
            configuration.target.definition_identity(),
            &plan.members[0].definition_identity
        );
        assert_eq!(plan.lock.members.len(), 2);
        assert_eq!(plan.lock.members[1].dependency_closure, ["child"]);

        let preview = preview_workflow_package(&plan, &authoring_catalog(), &BTreeMap::new())
            .expect("package preview");
        assert!(preview.is_compiled());
        assert_eq!(
            preview
                .members
                .iter()
                .map(|member| member.member_id.as_str())
                .collect::<Vec<_>>(),
            ["child", "parent"]
        );
        let parent_preview = preview.members[1]
            .compilation
            .compiled
            .as_ref()
            .expect("parent preview");
        assert_eq!(
            parent_preview.definition_identity,
            plan.members[1].definition_identity
        );
    }

    #[test]
    fn workflow_package_preview_rejects_unknown_member_configuration() {
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/member",
            "title": "Member",
            "steps": [{"id": "input", "input": {
                "schema": {"type_name": "value/v1", "schema": {"type": "string"}}
            }}]
        });
        let manifest = WorkflowPackageManifest {
            version: WORKFLOW_PACKAGE_MANIFEST_VERSION,
            package_id: "example/package".to_string(),
            exports: BTreeMap::from([("main".to_string(), "member".to_string())]),
            external_dependencies: BTreeMap::new(),
            imports: Vec::new(),
            members: vec![WorkflowPackageMember {
                member_id: "member".to_string(),
                source_name: "member.json".to_string(),
                format: WorkflowSourceFormat::Json,
                source: serde_json::to_string(&source).expect("source"),
                dependencies: Vec::new(),
                external_dependencies: Vec::new(),
            }],
        };
        let plan = plan_workflow_package(&manifest, &authoring_catalog()).expect("plan");
        let error = preview_workflow_package(
            &plan,
            &authoring_catalog(),
            &BTreeMap::from([("missing".to_string(), serde_json::json!({}))]),
        )
        .expect_err("unknown member configuration");
        assert!(matches!(
            error,
            WorkflowError::Build { path, .. }
                if path == "package_preview.configurations"
        ));
    }

    #[test]
    fn workflow_package_member_diagnostics_are_exactly_qualified() {
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/member",
            "title": "Member",
            "steps": [{"id": "input", "input": {
                "schema": {"type_name": "value/v1", "schema": {"type": "string"}}
            }}]
        });
        let manifest = WorkflowPackageManifest {
            version: WORKFLOW_PACKAGE_MANIFEST_VERSION,
            package_id: "example/package".to_string(),
            exports: BTreeMap::from([("main".to_string(), "member".to_string())]),
            external_dependencies: BTreeMap::new(),
            imports: Vec::new(),
            members: vec![WorkflowPackageMember {
                member_id: "member".to_string(),
                source_name: "member.yaml".to_string(),
                format: WorkflowSourceFormat::Json,
                source: serde_json::to_string(&source).expect("source"),
                dependencies: Vec::new(),
                external_dependencies: Vec::new(),
            }],
        };
        let plan = plan_workflow_package(&manifest, &authoring_catalog()).expect("plan");
        let diagnostics =
            plan.members[0]
                .member_source_map
                .remap_diagnostics(&[WorkflowValidationDiagnostic {
                    severity: WorkflowValidationSeverity::Error,
                    code: "node".to_string(),
                    document_path: "definition.nodes.input.configuration".to_string(),
                    message: "node".to_string(),
                    remediation: "fix node".to_string(),
                }]);
        assert_eq!(
            diagnostics[0].document_path,
            "package.members.member.source.steps[0].configuration"
        );
        let report = WorkflowValidationReport {
            authoring_version: WORKFLOW_AUTHORING_DOCUMENT_VERSION,
            valid: false,
            source_digest_sha256: None,
            executable_source_digest_sha256: None,
            diagnostics: vec![WorkflowValidationDiagnostic {
                severity: WorkflowValidationSeverity::Error,
                code: "node".to_string(),
                document_path: "definition.nodes.input.configuration".to_string(),
                message: "node".to_string(),
                remediation: "fix node".to_string(),
            }],
        }
        .remap_package_member_diagnostics(&plan.members[0].member_source_map);
        assert_eq!(
            report.diagnostics[0].document_path,
            "package.members.member.source.steps[0].configuration"
        );
        let preview = WorkflowPackagePreview {
            version: WORKFLOW_PACKAGE_PREVIEW_VERSION,
            package_id: manifest.package_id.clone(),
            members: vec![WorkflowPackageMemberPreview {
                member_id: "member".to_string(),
                source_name: "member.yaml".to_string(),
                source_map: plan.members[0].member_source_map.clone(),
                compilation: WorkflowCompilationPreview {
                    version: WORKFLOW_COMPILATION_PREVIEW_VERSION,
                    validation: WorkflowValidationReport {
                        authoring_version: WORKFLOW_AUTHORING_DOCUMENT_VERSION,
                        valid: false,
                        source_digest_sha256: None,
                        executable_source_digest_sha256: None,
                        diagnostics: vec![WorkflowValidationDiagnostic {
                            severity: WorkflowValidationSeverity::Error,
                            code: "node".to_string(),
                            document_path: "definition.nodes.input.configuration".to_string(),
                            message: "node".to_string(),
                            remediation: "fix node".to_string(),
                        }],
                    },
                    compiled: None,
                },
            }],
            lock: plan.lock.clone(),
        };
        assert_eq!(
            preview.remapped_diagnostics()[0].document_path,
            "package.members.member.source.steps[0].configuration"
        );

        let mut invalid = manifest;
        invalid.members[0].source = "{".to_string();
        let error = plan_workflow_package(&invalid, &authoring_catalog()).expect_err("invalid");
        assert!(matches!(
            error,
            WorkflowError::Build { path, .. }
                if path == "package.members.member.source.source.json"
        ));
    }

    #[test]
    fn workflow_package_plan_rejects_undeclared_local_call() {
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/parent",
            "title": "Parent",
            "steps": [{"id": "child", "package_call": {"member": "missing"}}]
        });
        let manifest = WorkflowPackageManifest {
            version: WORKFLOW_PACKAGE_MANIFEST_VERSION,
            package_id: "example/package".to_string(),
            exports: BTreeMap::from([("main".to_string(), "parent".to_string())]),
            external_dependencies: BTreeMap::new(),
            imports: Vec::new(),
            members: vec![WorkflowPackageMember {
                member_id: "parent".to_string(),
                source_name: "parent.json".to_string(),
                format: WorkflowSourceFormat::Json,
                source: serde_json::to_string(&source).expect("source"),
                dependencies: Vec::new(),
                external_dependencies: Vec::new(),
            }],
        };
        assert!(plan_workflow_package(&manifest, &authoring_catalog()).is_err());
    }

    #[test]
    fn workflow_package_lock_validates_exact_reproducibility_facts() {
        let schema = ValueSchema {
            type_name: "value/v1".to_string(),
            schema: serde_json::json!({"type": "string"}),
        };
        let definition = WorkflowDefinition {
            schema_version: WORKFLOW_DEFINITION_SCHEMA_VERSION,
            name: "example/member".to_string(),
            input: schema.clone(),
            output: schema.clone(),
            nodes: BTreeMap::from([(
                "input".to_string(),
                NodeDefinition {
                    id: "input".to_string(),
                    name: "Input".to_string(),
                    kind: NodeKind::Input,
                    dataflow: WorkflowNodeDataflowPolicy::Direct,
                    input: schema.clone(),
                    output: schema,
                    resources: Vec::new(),
                    configuration: serde_json::json!({"gate_version": 1}),
                },
            )]),
            entries: vec!["input".to_string()],
            exits: vec!["input".to_string()],
            edges: Vec::new(),
        };
        let identity = WorkflowDefinitionIdentity::for_definition("example/member", &definition)
            .expect("identity");
        let locked_member = WorkflowPackageLockedMember {
            member_id: "member".to_string(),
            source_digest_sha256: "b".repeat(64),
            executable_digest_sha256: "c".repeat(64),
            definition_identity: identity,
            published_revision: Some(WorkflowRevisionIdentity {
                workflow_id: "example/member".to_string(),
                revision: 1,
            }),
            dependency_closure: Vec::new(),
        };
        let lock = WorkflowPackageLock {
            version: WORKFLOW_PACKAGE_LOCK_VERSION,
            package_id: "example/package".to_string(),
            imports: Vec::new(),
            package_source_digest_sha256: "a".repeat(64),
            exports: vec![WorkflowPackageLockedExport {
                export: "main".to_string(),
                member_id: locked_member.member_id.clone(),
                definition_identity: locked_member.definition_identity.clone(),
                published_revision: locked_member.published_revision.clone(),
            }],
            members: vec![locked_member],
        };
        lock.validate().expect("package lock");
        let round_trip: WorkflowPackageLock =
            serde_json::from_value(serde_json::to_value(&lock).expect("serialize lock"))
                .expect("deserialize lock");
        assert_eq!(round_trip, lock);
    }

    #[test]
    fn workflow_package_lock_rejects_future_duplicate_and_malformed_state() {
        let identity = WorkflowDefinitionIdentity {
            kind: "example/member".to_string(),
            definition_id: "example/member@digest".to_string(),
            definition_version: 1,
        };
        let member = |id: &str| WorkflowPackageLockedMember {
            member_id: id.to_string(),
            source_digest_sha256: "b".repeat(64),
            executable_digest_sha256: "c".repeat(64),
            definition_identity: identity.clone(),
            published_revision: None,
            dependency_closure: Vec::new(),
        };
        let lock = |version, digest: String, members: Vec<WorkflowPackageLockedMember>| {
            let first = members.first().expect("test lock member");
            WorkflowPackageLock {
                version,
                package_id: "example/package".to_string(),
                imports: Vec::new(),
                package_source_digest_sha256: digest,
                exports: vec![WorkflowPackageLockedExport {
                    export: "main".to_string(),
                    member_id: first.member_id.clone(),
                    definition_identity: first.definition_identity.clone(),
                    published_revision: first.published_revision.clone(),
                }],
                members,
            }
        };
        assert!(
            lock(
                WORKFLOW_PACKAGE_LOCK_VERSION + 1,
                "a".repeat(64),
                vec![member("a")]
            )
            .validate()
            .is_err()
        );
        assert!(
            lock(
                WORKFLOW_PACKAGE_LOCK_VERSION,
                "not-a-digest".to_string(),
                vec![member("a")]
            )
            .validate()
            .is_err()
        );
        assert!(
            lock(
                WORKFLOW_PACKAGE_LOCK_VERSION,
                "a".repeat(64),
                vec![member("a"), member("a")]
            )
            .validate()
            .is_err()
        );
    }

    #[test]
    fn workflow_package_manifest_validates_bounded_dependency_dag() {
        let manifest = WorkflowPackageManifest {
            version: WORKFLOW_PACKAGE_MANIFEST_VERSION,
            package_id: "example/package".to_string(),
            exports: BTreeMap::from([("main".to_string(), "parent".to_string())]),
            external_dependencies: BTreeMap::new(),
            imports: Vec::new(),
            members: vec![
                WorkflowPackageMember {
                    member_id: "child".to_string(),
                    source_name: "workflows/child.workflow.yaml".to_string(),
                    format: WorkflowSourceFormat::Yaml,
                    source: "workflow_source_version: 3".to_string(),
                    dependencies: Vec::new(),
                    external_dependencies: Vec::new(),
                },
                WorkflowPackageMember {
                    member_id: "parent".to_string(),
                    source_name: "workflows/parent.workflow.yaml".to_string(),
                    format: WorkflowSourceFormat::Yaml,
                    source: "workflow_source_version: 3".to_string(),
                    external_dependencies: Vec::new(),
                    dependencies: vec!["child".to_string()],
                },
            ],
        };
        manifest.validate().expect("package manifest");
        let round_trip: WorkflowPackageManifest =
            serde_json::from_value(serde_json::to_value(&manifest).expect("serialize package"))
                .expect("deserialize package");
        assert_eq!(round_trip, manifest);
    }

    #[test]
    fn workflow_package_manifest_rejects_cycles_duplicates_escape_and_future_version() {
        let member = |id: &str, source_name: &str, dependencies: Vec<&str>| WorkflowPackageMember {
            member_id: id.to_string(),
            source_name: source_name.to_string(),
            format: WorkflowSourceFormat::Yaml,
            source: "workflow_source_version: 3".to_string(),
            dependencies: dependencies.into_iter().map(str::to_string).collect(),
            external_dependencies: Vec::new(),
        };
        let package = |version, members| WorkflowPackageManifest {
            version,
            package_id: "example/package".to_string(),
            exports: BTreeMap::from([("main".to_string(), "a".to_string())]),
            external_dependencies: BTreeMap::new(),
            imports: Vec::new(),
            members,
        };
        assert!(
            package(
                WORKFLOW_PACKAGE_MANIFEST_VERSION,
                vec![
                    member("a", "a.yaml", vec!["b"]),
                    member("b", "b.yaml", vec!["a"])
                ]
            )
            .validate()
            .is_err()
        );
        assert!(
            package(
                WORKFLOW_PACKAGE_MANIFEST_VERSION,
                vec![member("a", "a.yaml", vec![]), member("a", "b.yaml", vec![])]
            )
            .validate()
            .is_err()
        );
        assert!(
            package(
                WORKFLOW_PACKAGE_MANIFEST_VERSION,
                vec![member("a", "../a.yaml", vec![])]
            )
            .validate()
            .is_err()
        );
        assert!(
            package(
                WORKFLOW_PACKAGE_MANIFEST_VERSION + 1,
                vec![member("a", "a.yaml", vec![])]
            )
            .validate()
            .is_err()
        );
    }

    #[test]
    fn source_map_remaps_nested_nodes_and_edges() {
        let map = WorkflowSourceMap {
            version: WORKFLOW_SOURCE_MAP_VERSION,
            entries: vec![
                WorkflowSourceMapEntry {
                    step_index: 0,
                    source_path: "steps[0].repeat".to_string(),
                    target_kind: WorkflowSourceMapTargetKind::Node,
                    node_id: "body__repeat".to_string(),
                    edge_to: None,
                },
                WorkflowSourceMapEntry {
                    step_index: 0,
                    source_path: "steps[0].repeat.while_predicate".to_string(),
                    target_kind: WorkflowSourceMapTargetKind::Edge,
                    node_id: "body__repeat".to_string(),
                    edge_to: Some("body".to_string()),
                },
            ],
        };
        let remapped = map.remap_diagnostics(&[
            WorkflowValidationDiagnostic {
                severity: WorkflowValidationSeverity::Error,
                code: "node".to_string(),
                document_path: "definition.nodes.body__repeat.configuration".to_string(),
                message: "node".to_string(),
                remediation: "fix node".to_string(),
            },
            WorkflowValidationDiagnostic {
                severity: WorkflowValidationSeverity::Error,
                code: "edge".to_string(),
                document_path: "definition.edges.body__repeat->body.predicate".to_string(),
                message: "edge".to_string(),
                remediation: "fix edge".to_string(),
            },
        ]);
        assert_eq!(remapped[0].document_path, "steps[0].repeat.configuration");
        assert_eq!(
            remapped[1].document_path,
            "steps[0].repeat.while_predicate.predicate"
        );
    }

    #[test]
    fn structured_source_v3_lowers_deterministically_and_rejects_previous_versions() {
        let block = WorkflowBlockDefinition {
            block_id: "example.echo".to_string(),
            block_version: 1,
            plugin_id: "bcode.example".to_string(),
            operation: "example.echo".to_string(),
            input: ValueSchema {
                type_name: "example.value/v1".to_string(),
                schema: serde_json::json!({"type": "string"}),
            },
            output: ValueSchema {
                type_name: "example.value/v1".to_string(),
                schema: serde_json::json!({"type": "string"}),
            },
            effect: WorkflowBlockEffect::ReadOnly,
            resources: Vec::new(),
            authorization: WorkflowBlockAuthorization {
                capability: WorkflowToolCapability::ReadOnly,
                explicit_grant_required: false,
            },
            timeout_ms: 1_000,
            cancellation_supported: true,
            reconciliation: WorkflowBlockReconciliation::IdempotentReplay,
            automatic_retry: None,
            preparation_required: false,
        };
        let action = WorkflowAuthoringActionDescriptor {
            version: WORKFLOW_AUTHORING_ACTION_DESCRIPTOR_VERSION,
            action_key: "echo".to_string(),
            action_version: 1,
            plugin_id: "bcode.example".to_string(),
            input: block.input.clone(),
            target_block: workflow_block_catalog_key(&block),
            input_adapter: None,
        };
        let mut catalog = authoring_catalog();
        catalog.plugins.insert("bcode.example".to_string());
        catalog
            .blocks
            .insert(workflow_block_catalog_key(&block), block);
        catalog
            .authoring_actions
            .insert(action.catalog_key(), action);
        let v3 = r#"
workflow_source_version: 3
workflow_id: example/structured
 title: Structured
input:
  type_name: example.value/v1
  schema: {type: string}
output:
  type_name: example.value/v1
  schema: {type: string}
steps:
  - id: first
    echo: ready
  - id: second
    needs: [first]
    when:
      source:
        step: first
      predicate:
        operation: equals
        version: 3
        path: ""
        value: ready
    echo: done
"#;
        let v3 = v3.replace("\n title:", "\ntitle:");
        let first = lower_workflow_authoring_source(&v3, WorkflowSourceFormat::Yaml, &catalog)
            .expect("structured v3");
        let second = lower_workflow_authoring_source(&v3, WorkflowSourceFormat::Yaml, &catalog)
            .expect("deterministic structured v3");
        assert_eq!(first, second);
        assert_eq!(first.profile, WorkflowSourceProfile::Structured);
        assert!(matches!(
            first.document.definition.edges[0].kind,
            EdgeKind::Conditional { expected: true, .. }
        ));
        let old = v3.replace("workflow_source_version: 3", "workflow_source_version: 2");
        assert!(
            lower_workflow_authoring_source(&old, WorkflowSourceFormat::Yaml, &catalog).is_err(),
            "previous source versions must fail closed"
        );
    }

    #[test]
    fn structured_source_v3_lowers_named_generic_input_expressions() {
        let schema = ValueSchema {
            type_name: "example.composed/v1".to_string(),
            schema: serde_json::json!({"type": "object", "additionalProperties": true}),
        };
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/named-dataflow",
            "title": "Named dataflow",
            "steps": [{
                "id": "first",
                "input": {"schema": schema}
            }, {
                "id": "second",
                "needs": ["first"],
                "input_expression": {
                    "version": 2,
                    "expression": {
                        "operation": "object",
                        "fields": {
                            "constant": {"operation": "constant", "value": true},
                            "configuration": {"operation": "input", "source": "configuration", "path": "mode"},
                            "root": {"operation": "input", "source": "state", "path": "request"},
                            "prior": {"operation": "selected_input", "source": "dependency.first", "selector": {"version": 1, "segments": [{"kind": "field", "name": "value"}]}}
                        }
                    },
                    "output": schema
                },
                "input": {"schema": schema}
            }]
        });
        let lowered = lower_workflow_authoring_source(
            &serde_json::to_string(&source).expect("source"),
            WorkflowSourceFormat::Json,
            &authoring_catalog(),
        )
        .expect("named dataflow source");
        let edge = &lowered.document.definition.edges[0];
        let transform = edge.transform.as_ref().expect("input transform");
        assert!(matches!(
            transform.expression,
            WorkflowTransformExpression::Object { .. }
        ));
    }

    #[test]
    fn structured_source_v3_rejects_undeclared_named_dependency_sources() {
        let schema = ValueSchema {
            type_name: "example.value/v1".to_string(),
            schema: serde_json::json!({}),
        };
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/invalid-named-dataflow",
            "title": "Invalid named dataflow",
            "steps": [{"id": "first", "input": {"schema": schema}}, {
                "id": "second",
                "needs": ["first"],
                "input_expression": {
                    "version": 2,
                    "expression": {"operation": "input", "source": "dependency.missing", "path": ""},
                    "output": schema
                },
                "input": {"schema": schema}
            }]
        });
        let error = lower_workflow_authoring_source(
            &serde_json::to_string(&source).expect("source"),
            WorkflowSourceFormat::Json,
            &authoring_catalog(),
        )
        .expect_err("undeclared named dependency");
        assert!(error.to_string().contains("unknown step 'missing'"));
    }

    #[test]
    fn structured_source_v3_materializes_static_inputs_between_source_components() {
        let schema = ValueSchema {
            type_name: "example.static/v1".to_string(),
            schema: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["message"],
                "properties": {"message": {"type": "string"}}
            }),
        };
        let block = WorkflowBlockDefinition {
            block_id: "echo".to_string(),
            block_version: 1,
            plugin_id: "bcode.example".to_string(),
            operation: "echo".to_string(),
            input: schema.clone(),
            output: ValueSchema {
                type_name: "example.other/v1".to_string(),
                schema: serde_json::json!({"type": "boolean"}),
            },
            effect: WorkflowBlockEffect::ReadOnly,
            resources: Vec::new(),
            authorization: WorkflowBlockAuthorization {
                capability: WorkflowToolCapability::ReadOnly,
                explicit_grant_required: false,
            },
            timeout_ms: 30_000,
            cancellation_supported: true,
            reconciliation: WorkflowBlockReconciliation::IdempotentReplay,
            automatic_retry: None,
            preparation_required: false,
        };
        let action = WorkflowAuthoringActionDescriptor {
            version: WORKFLOW_AUTHORING_ACTION_DESCRIPTOR_VERSION,
            action_key: "echo".to_string(),
            action_version: 1,
            plugin_id: "bcode.example".to_string(),
            input: schema,
            target_block: workflow_block_catalog_key(&block),
            input_adapter: None,
        };
        let mut catalog = authoring_catalog();
        catalog.plugins.insert("bcode.example".to_string());
        catalog
            .blocks
            .insert(workflow_block_catalog_key(&block), block);
        catalog
            .authoring_actions
            .insert(action.catalog_key(), action);
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/static-chain",
            "title": "Static chain",
            "steps": [
                {"id": "first", "echo": {"message": "first"}},
                {"id": "second", "echo": {"message": "second"}}
            ]
        });
        let lowered = lower_workflow_authoring_source(
            &serde_json::to_string(&source).expect("source"),
            WorkflowSourceFormat::Json,
            &catalog,
        )
        .expect("static source chain");
        let edge = &lowered.document.definition.edges[0];
        assert!(matches!(
            edge.transform.as_ref().map(|transform| &transform.expression),
            Some(WorkflowTransformExpression::Constant { value })
                if value == &serde_json::json!({"message": "second"})
        ));
        assert!(
            lowered
                .document
                .plugin_input_defaults
                .contains_key("second")
        );
        let compiled = lowered
            .document
            .compilation_preview(&catalog, None)
            .compiled
            .expect("compiled static chain");
        assert!(!compiled.plugin_input_defaults.contains_key("second"));
    }

    #[test]
    fn structured_source_v3_lowers_agents_with_exact_context_policy() {
        let schema = ValueSchema {
            type_name: "example.agent-value/v1".to_string(),
            schema: serde_json::json!({"type": "object", "additionalProperties": true}),
        };
        let configuration = WorkflowPromptConfiguration {
            version: WORKFLOW_PROMPT_CONFIGURATION_VERSION,
            execution_target: PromptContextTarget::FixedGenerationFork,
            agent_profile: "review".to_string(),
            provider: None,
            model: None,
            structured_output: PromptStructuredOutputPolicy {
                schema: schema.clone(),
                strict: true,
            },
            read_only: true,
            tool_capability: WorkflowToolCapability::ReadOnly,
            tool_allowlist: vec!["filesystem.read".to_string()],
            timeout_ms: 30_000,
            prompt_mode: "json_input".to_string(),
            system_prompt: "Review the input.".to_string(),
        };
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/agent",
            "title": "Agent",
            "steps": [{
                "id": "review",
                "agent": {
                    "configuration": configuration,
                    "input": schema,
                    "output": schema,
                    "resources": [{"resource": "repository", "access": "read"}]
                }
            }]
        });
        let lowered = lower_workflow_authoring_source(
            &serde_json::to_string(&source).expect("source"),
            WorkflowSourceFormat::Json,
            &authoring_catalog(),
        )
        .expect("agent source");
        assert_eq!(lowered.profile, WorkflowSourceProfile::Structured);
        let node = &lowered.document.definition.nodes["review"];
        assert_eq!(node.kind, NodeKind::Agent);
        let configuration: WorkflowPromptConfiguration =
            serde_json::from_value(node.configuration.clone()).expect("agent configuration");
        assert_eq!(
            configuration.execution_target,
            PromptContextTarget::FixedGenerationFork
        );
    }

    #[test]
    fn structured_source_v3_rejects_agent_output_schema_mismatch() {
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/agent-invalid",
            "title": "Agent invalid",
            "steps": [{
                "id": "review",
                "agent": {
                    "configuration": {
                        "version": 1,
                        "execution_target": "fresh_isolated",
                        "profile": "review",
                        "provider": null,
                        "model": null,
                        "structured_output": {
                            "schema": {"type_name": "expected/v1", "schema": {"type": "string"}},
                            "strict": true
                        },
                        "read_only": true,
                        "tool_capability": "read_only",
                        "tool_allowlist": [],
                        "timeout_ms": 30000,
                        "prompt_mode": "json_input",
                        "system_prompt": "Review."
                    },
                    "input": {"type_name": "input/v1", "schema": {"type": "string"}},
                    "output": {"type_name": "different/v1", "schema": {"type": "string"}}
                }
            }]
        });
        assert!(
            lower_workflow_authoring_source(
                &serde_json::to_string(&source).expect("source"),
                WorkflowSourceFormat::Json,
                &authoring_catalog(),
            )
            .is_err()
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn structured_source_v3_lowers_typed_selected_input_reference() {
        let source_schema = ValueSchema {
            type_name: "example.record/v1".to_string(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {"message": {"type": "string"}},
                "required": ["message"],
                "additionalProperties": false
            }),
        };
        let target_schema = ValueSchema {
            type_name: "example.message/v1".to_string(),
            schema: serde_json::json!({"type": "string"}),
        };
        let block =
            |block_id: &str, input: ValueSchema, output: ValueSchema| WorkflowBlockDefinition {
                block_id: block_id.to_string(),
                block_version: 1,
                plugin_id: "bcode.example".to_string(),
                operation: block_id.to_string(),
                input,
                output,
                effect: WorkflowBlockEffect::ReadOnly,
                resources: Vec::new(),
                authorization: WorkflowBlockAuthorization {
                    capability: WorkflowToolCapability::ReadOnly,
                    explicit_grant_required: false,
                },
                timeout_ms: 30_000,
                cancellation_supported: true,
                reconciliation: WorkflowBlockReconciliation::IdempotentReplay,
                automatic_retry: None,
                preparation_required: false,
            };
        let produce = block("example.produce", source_schema.clone(), source_schema);
        let consume = block("example.consume", target_schema.clone(), target_schema);
        let mut catalog = authoring_catalog();
        catalog.plugins.insert("bcode.example".to_string());
        catalog
            .blocks
            .insert(workflow_block_catalog_key(&produce), produce);
        catalog
            .blocks
            .insert(workflow_block_catalog_key(&consume), consume);
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/selected-input",
            "title": "Selected input",
            "steps": [
                {
                    "id": "produce",
                    "uses": "bcode.example/example.produce@1",
                    "with": {"message": "hello"}
                },
                {
                    "id": "consume",
                    "needs": ["produce"],
                    "input_from": {
                        "step": "produce",
                        "select": {
                            "version": 1,
                            "segments": [{"kind": "field", "name": "message"}]
                        }
                    },
                    "when": {
                        "source": {
                            "step": "produce",
                            "select": {
                                "version": 1,
                                "segments": [{"kind": "field", "name": "message"}]
                            }
                        },
                        "predicate": {
                            "operation": "selected_equals",
                            "version": 3,
                            "selector": {"version": 1, "segments": []},
                            "value": "hello"
                        }
                    },
                    "uses": "bcode.example/example.consume@1",
                    "with": "ignored-by-edge-input"
                }
            ]
        });
        let lowered = lower_workflow_authoring_source(
            &serde_json::to_string(&source).expect("source"),
            WorkflowSourceFormat::Json,
            &catalog,
        )
        .expect("selected input source");
        let edge = lowered
            .document
            .definition
            .edges
            .iter()
            .find(|edge| edge.from == "produce" && edge.to == "consume")
            .expect("selected edge");
        assert!(matches!(
            edge.transform
                .as_ref()
                .map(|transform| &transform.expression),
            Some(WorkflowTransformExpression::SelectedInput { selector, .. })
                if selector.segments == vec![WorkflowValueSelectorSegment::Field {
                    name: "message".to_string()
                }]
        ));
        assert!(matches!(
            &edge.kind,
            EdgeKind::Conditional {
                predicate: PredicateExpression::SelectedEquals { selector, .. },
                ..
            } if selector.segments == vec![WorkflowValueSelectorSegment::Field {
                name: "message".to_string()
            }]
        ));
    }

    #[test]
    fn structured_source_v3_rejects_ambiguous_selector_schema() {
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/ambiguous-selector",
            "title": "Ambiguous selector",
            "steps": [{
                "id": "first",
                "agent": {
                    "configuration": {
                        "version": 1,
                        "execution_target": "fresh_isolated",
                        "profile": "review",
                        "structured_output": {
                            "schema": {"type_name": "choice/v1", "schema": {
                                "oneOf": [{"type": "object", "properties": {"value": {"type": "string"}}}]
                            }},
                            "strict": true
                        },
                        "read_only": true,
                        "tool_capability": "read_only",
                        "tool_allowlist": [],
                        "timeout_ms": 30000,
                        "prompt_mode": "json_input",
                        "system_prompt": "Review."
                    },
                    "input": {"type_name": "choice/v1", "schema": {"oneOf": [{"type": "object"}]}},
                    "output": {"type_name": "choice/v1", "schema": {
                        "oneOf": [{"type": "object", "properties": {"value": {"type": "string"}}}]
                    }}
                }
            }, {
                "id": "second",
                "needs": ["first"],
                "input_from": {
                    "step": "first",
                    "select": {"version": 1, "segments": [{"kind": "field", "name": "value"}]}
                },
                "agent": {
                    "configuration": {
                        "version": 1,
                        "execution_target": "fresh_isolated",
                        "profile": "review",
                        "structured_output": {"schema": {"type_name": "string/v1", "schema": {"type": "string"}}, "strict": true},
                        "read_only": true,
                        "tool_capability": "read_only",
                        "tool_allowlist": [],
                        "timeout_ms": 30000,
                        "prompt_mode": "json_input",
                        "system_prompt": "Review."
                    },
                    "input": {"type_name": "string/v1", "schema": {"type": "string"}},
                    "output": {"type_name": "string/v1", "schema": {"type": "string"}}
                }
            }]
        });
        assert!(
            lower_workflow_authoring_source(
                &serde_json::to_string(&source).expect("source"),
                WorkflowSourceFormat::Json,
                &authoring_catalog(),
            )
            .is_err()
        );
    }

    #[test]
    fn structured_source_v3_fan_out_lowers_durable_member_contract() {
        let member = serde_json::json!({"type_name": "value/v1", "schema": {"type": "string"}});
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/fan-out",
            "title": "Fan out",
            "steps": [{
                "id": "reviewers",
                "fan_out": {
                    "input": {"type_name": "values/v1", "schema": {"type": "array", "items": {"type": "string"}}},
                    "member": member,
                    "output_member": member,
                    "operation": {"approval": {"schema": member}},
                    "max_members": 4,
                    "max_concurrency": 2,
                    "failure_policy": "wait_all"
                }
            }]
        });
        let lowered = lower_workflow_authoring_source(
            &serde_json::to_string(&source).expect("source"),
            WorkflowSourceFormat::Json,
            &authoring_catalog(),
        )
        .expect("fan-out source");
        let node = &lowered.document.definition.nodes["reviewers"];
        assert_eq!(node.kind, NodeKind::FanOut);
        let configuration: WorkflowFanOutConfiguration =
            serde_json::from_value(node.configuration.clone()).expect("configuration");
        assert_eq!(configuration.max_members, 4);
        assert_eq!(configuration.max_concurrency, 2);
        assert_eq!(configuration.member_node.kind, NodeKind::Approval);
    }

    #[test]
    fn structured_source_v3_fan_out_contract_rejects_unbounded_or_mismatched_members() {
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/fan-out-invalid",
            "title": "Fan out invalid",
            "steps": [{
                "id": "reviewers",
                "fan_out": {
                    "input": {"type_name": "values/v1", "schema": {"type": "array", "items": {"type": "string"}}},
                    "member": {"type_name": "member/v1", "schema": {"type": "integer"}},
                    "output_member": {"type_name": "result/v1", "schema": {"type": "string"}},
                    "operation": {"input": {"schema": {"type_name": "member/v1", "schema": {"type": "integer"}}}},
                    "max_members": 1,
                    "max_concurrency": 2
                }
            }]
        });
        assert!(
            lower_workflow_authoring_source(
                &serde_json::to_string(&source).expect("source"),
                WorkflowSourceFormat::Json,
                &authoring_catalog(),
            )
            .is_err()
        );
    }

    #[test]
    fn structured_source_v3_retry_lowers_into_plugin_block_policy() {
        let schema = ValueSchema {
            type_name: "value/v1".to_string(),
            schema: serde_json::json!({"type": "string"}),
        };
        let block = WorkflowBlockDefinition {
            block_id: "example.retryable".to_string(),
            block_version: 1,
            plugin_id: "bcode.example".to_string(),
            operation: "example.retryable".to_string(),
            input: schema.clone(),
            output: schema.clone(),
            effect: WorkflowBlockEffect::ReadOnly,
            resources: Vec::new(),
            authorization: WorkflowBlockAuthorization {
                capability: WorkflowToolCapability::ReadOnly,
                explicit_grant_required: false,
            },
            timeout_ms: 1_000,
            cancellation_supported: true,
            reconciliation: WorkflowBlockReconciliation::ReceiptStatus,
            automatic_retry: None,
            preparation_required: false,
        };
        let mut catalog = authoring_catalog();
        catalog.plugins.insert("bcode.example".to_string());
        catalog
            .blocks
            .insert(workflow_block_catalog_key(&block), block.clone());
        catalog.authoring_actions.insert(
            "retryable@1".to_string(),
            WorkflowAuthoringActionDescriptor {
                version: WORKFLOW_AUTHORING_ACTION_DESCRIPTOR_VERSION,
                action_key: "retryable".to_string(),
                action_version: 1,
                plugin_id: "bcode.example".to_string(),
                input: schema,
                target_block: workflow_block_catalog_key(&block),
                input_adapter: None,
            },
        );
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/retry",
            "title": "Retry",
            "steps": [{
                "id": "work",
                "retry": {
                    "max_attempts": 2,
                    "eligible_failures": ["owner_reported_retryable"],
                    "initial_backoff_ms": 100,
                    "backoff_multiplier": 2,
                    "maximum_backoff_ms": 1000
                },
                "uses": "bcode.example/example.retryable@1",
                "with": "value"
            }]
        });
        let lowered = lower_workflow_authoring_source(
            &serde_json::to_string(&source).expect("source"),
            WorkflowSourceFormat::Json,
            &catalog,
        )
        .expect("retry source");
        let block: WorkflowBlockDefinition = serde_json::from_value(
            lowered.document.definition.nodes["work"]
                .configuration
                .clone(),
        )
        .expect("block");
        assert_eq!(
            block.automatic_retry,
            Some(WorkflowAutomaticRetryPolicy {
                version: WORKFLOW_AUTOMATIC_RETRY_POLICY_VERSION,
                max_attempts: 2,
                eligible_failures: vec![AutomaticRetryFailureKind::OwnerReportedRetryable],
                initial_backoff_ms: 100,
                backoff_multiplier: 2,
                maximum_backoff_ms: 1_000,
            })
        );
    }

    #[test]
    fn structured_source_v3_retry_contract_rejects_unsafe_failure_classes() {
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/retry-invalid",
            "title": "Retry invalid",
            "steps": [{
                "id": "input",
                "retry": {
                    "max_attempts": 2,
                    "eligible_failures": ["ambiguous_mutation"],
                    "initial_backoff_ms": 100,
                    "backoff_multiplier": 2,
                    "maximum_backoff_ms": 1000
                },
                "input": {"schema": {"type_name": "value/v1", "schema": {"type": "string"}}}
            }]
        });
        let error = lower_workflow_authoring_source(
            &serde_json::to_string(&source).expect("source"),
            WorkflowSourceFormat::Json,
            &authoring_catalog(),
        )
        .expect_err("unsafe retry");
        assert!(error.to_string().contains("safely owner-retryable"));
    }

    #[test]
    fn structured_source_v3_lowers_fixed_parallel_join() {
        let value_schema = serde_json::json!({
            "type_name": "value/v1",
            "schema": {"type": "string"}
        });
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/parallel",
            "title": "Parallel",
            "steps": [{
                "id": "root",
                "input": {"schema": value_schema}
            }, {
                "id": "left",
                "needs": ["root"],
                "input": {"schema": value_schema}
            }, {
                "id": "right",
                "needs": ["root"],
                "input": {"schema": value_schema}
            }, {
                "id": "join",
                "needs": ["left", "right"],
                "parallel": {
                    "left": "left",
                    "right": "right",
                    "failure_policy": "fail_fast"
                }
            }]
        });
        let lowered = lower_workflow_authoring_source(
            &serde_json::to_string(&source).expect("source"),
            WorkflowSourceFormat::Json,
            &authoring_catalog(),
        )
        .expect("parallel source");
        let definition = &lowered.document.definition;
        let join = &definition.nodes["join"];
        assert_eq!(join.kind, NodeKind::Parallel);
        assert_eq!(join.configuration["failure_policy"], "fail_fast");
        assert_eq!(
            join.input.schema["prefixItems"].as_array().map(Vec::len),
            Some(2)
        );
        assert!(
            definition
                .edges
                .iter()
                .any(|edge| edge.from == "left" && edge.to == "join")
        );
        assert!(
            definition
                .edges
                .iter()
                .any(|edge| edge.from == "right" && edge.to == "join")
        );
    }

    #[test]
    fn structured_source_v3_rejects_parallel_without_exact_branches() {
        let schema = serde_json::json!({"type_name": "value/v1", "schema": {"type": "string"}});
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/parallel-invalid",
            "title": "Parallel invalid",
            "steps": [{"id": "left", "input": {"schema": schema}}, {
                "id": "join",
                "needs": ["left"],
                "parallel": {"left": "left", "right": "missing"}
            }]
        });
        assert!(
            lower_workflow_authoring_source(
                &serde_json::to_string(&source).expect("source"),
                WorkflowSourceFormat::Json,
                &authoring_catalog(),
            )
            .is_err()
        );
    }

    #[test]
    fn structured_source_v3_lowers_bounded_repeat_topology() {
        let block = WorkflowBlockDefinition {
            block_id: "example.echo".to_string(),
            block_version: 1,
            plugin_id: "bcode.example".to_string(),
            operation: "example.echo".to_string(),
            input: ValueSchema {
                type_name: "example.value/v1".to_string(),
                schema: serde_json::json!({"type": "string"}),
            },
            output: ValueSchema {
                type_name: "example.value/v1".to_string(),
                schema: serde_json::json!({"type": "string"}),
            },
            effect: WorkflowBlockEffect::ReadOnly,
            resources: Vec::new(),
            authorization: WorkflowBlockAuthorization {
                capability: WorkflowToolCapability::ReadOnly,
                explicit_grant_required: false,
            },
            timeout_ms: 30_000,
            cancellation_supported: true,
            reconciliation: WorkflowBlockReconciliation::IdempotentReplay,
            automatic_retry: None,
            preparation_required: false,
        };
        let mut catalog = authoring_catalog();
        catalog.plugins.insert("bcode.example".to_string());
        catalog
            .blocks
            .insert(workflow_block_catalog_key(&block), block);
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/repeat",
            "title": "Repeat",
            "run_limits": {
                "node_execution_cap": 20,
                "concurrency_cap": 1,
                "cycle_cap": 5,
                "retry_cap": 1
            },
            "steps": [{
                "id": "echo",
                "repeat": {
                    "while_predicate": {
                        "operation": "equals",
                        "version": 1,
                        "path": "",
                        "value": "again"
                    },
                    "max_iterations": 3,
                    "exhaustion_policy": "fail"
                },
                "uses": "bcode.example/example.echo@1",
                "with": "again"
            }, {
                "id": "done",
                "needs": ["echo"],
                "input": {"schema": {
                    "type_name": "example.value/v1",
                    "schema": {"type": "string"}
                }}
            }]
        });
        let lowered = lower_workflow_authoring_source(
            &serde_json::to_string(&source).expect("source"),
            WorkflowSourceFormat::Json,
            &catalog,
        )
        .expect("repeat source");
        let definition = &lowered.document.definition;
        let controller = &definition.nodes["echo__repeat"];
        assert_eq!(controller.kind, NodeKind::Repeat);
        assert_eq!(definition.exits, ["done"]);
        assert!(definition.edges.iter().any(|edge| {
            edge.from == "echo" && edge.to == "echo__repeat" && edge.kind == EdgeKind::Direct
        }));
        assert!(definition.edges.iter().any(|edge| {
            edge.from == "echo__repeat"
                && edge.to == "echo"
                && matches!(
                    edge.kind,
                    EdgeKind::Back {
                        max_iterations: 3,
                        ..
                    }
                )
        }));
        assert!(definition.edges.iter().any(|edge| {
            edge.from == "echo__repeat" && edge.to == "done" && edge.kind == EdgeKind::Direct
        }));
        assert!(lowered.source_map.entries.iter().any(|entry| {
            entry.source_path == "steps[0].repeat" && entry.node_id == "echo__repeat"
        }));
    }

    #[test]
    fn structured_source_v3_rejects_unbounded_repeat() {
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/repeat-invalid",
            "title": "Repeat invalid",
            "steps": [{
                "id": "gate",
                "repeat": {
                    "while_predicate": {"operation": "equals", "version": 1, "path": "", "value": "again"},
                    "max_iterations": 0,
                    "exhaustion_policy": "fail"
                },
                "input": {"schema": {"type_name": "value/v1", "schema": {"type": "string"}}}
            }]
        });
        assert!(
            lower_workflow_authoring_source(
                &serde_json::to_string(&source).expect("source"),
                WorkflowSourceFormat::Json,
                &authoring_catalog(),
            )
            .is_err()
        );
    }

    #[test]
    fn structured_source_v3_lowers_typed_repeat_outcome_boundary() {
        let value_schema = ValueSchema {
            type_name: "value/v1".to_string(),
            schema: serde_json::json!({"type": "string"}),
        };
        let outcome_schema = workflow_repeat_outcome_schema(&value_schema).expect("outcome schema");
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/repeat-outcome",
            "title": "Repeat outcome",
            "steps": [{
                "id": "gate",
                "repeat": {
                    "while_predicate": {"operation": "equals", "version": 1, "path": "", "value": "again"},
                    "max_iterations": 2,
                    "exhaustion_policy": "emit_outcome"
                },
                "input": {"schema": value_schema}
            }, {
                "id": "outcome",
                "needs": ["gate"],
                "input": {"schema": outcome_schema}
            }]
        });
        let lowered = lower_workflow_authoring_source(
            &serde_json::to_string(&source).expect("source"),
            WorkflowSourceFormat::Json,
            &authoring_catalog(),
        )
        .expect("typed outcome");
        let definition = &lowered.document.definition;
        let repeat = &definition.nodes["gate__repeat"];
        assert_eq!(
            repeat.configuration["repeat_outcome_version"],
            WORKFLOW_REPEAT_OUTCOME_VERSION
        );
        assert_eq!(repeat.output, definition.nodes["outcome"].input);
        assert!(definition.edges.iter().any(|edge| {
            edge.from == "gate__repeat" && edge.to == "outcome" && edge.kind == EdgeKind::Direct
        }));
    }

    #[test]
    fn structured_source_v3_lowers_exact_child_input_and_output_mappings() {
        let child_input = ValueSchema {
            type_name: "example.child-input/v1".to_string(),
            schema: serde_json::json!({"type": "integer"}),
        };
        let child_output = ValueSchema {
            type_name: "example.child-output/v1".to_string(),
            schema: serde_json::json!({
                "type": "object",
                "required": ["value"],
                "properties": {"value": {"type": "integer"}}
            }),
        };
        let projected = ValueSchema {
            type_name: "example.projected/v1".to_string(),
            schema: serde_json::json!({"type": "integer"}),
        };
        let child = WorkflowDefinition {
            schema_version: WORKFLOW_DEFINITION_SCHEMA_VERSION,
            name: "example/child-mapped".to_string(),
            input: child_input.clone(),
            output: child_output.clone(),
            nodes: BTreeMap::from([(
                "child".to_string(),
                NodeDefinition {
                    id: "child".to_string(),
                    name: "child".to_string(),
                    kind: NodeKind::Input,
                    dataflow: WorkflowNodeDataflowPolicy::Direct,
                    input: child_input.clone(),
                    output: child_output,
                    resources: Vec::new(),
                    configuration: serde_json::json!({"gate_version": 1}),
                },
            )]),
            entries: vec!["child".to_string()],
            exits: vec!["child".to_string()],
            edges: Vec::new(),
        };
        let identity = WorkflowDefinitionIdentity::for_definition("example/child-mapped", &child)
            .expect("identity");
        let mut catalog = authoring_catalog();
        catalog
            .workflow_definitions
            .insert(identity.definition_id.clone(), child);
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/parent-mapped",
            "title": "Parent mapped",
            "steps": [{
                "id": "root",
                "input": {"schema": child_input}
            }, {
                "id": "call",
                "needs": ["root"],
                "workflow_call": {
                    "version": WORKFLOW_CALL_VERSION,
                    "target": {"kind": "definition", "identity": identity},
                    "input": {
                        "version": WORKFLOW_TRANSFORM_VERSION,
                        "expression": {"operation": "input", "source": "dependency.root", "path": ""},
                        "output": child_input
                    },
                    "output": {
                        "version": WORKFLOW_TRANSFORM_VERSION,
                        "expression": {"operation": "input", "source": "current", "path": "value"},
                        "output": projected
                    }
                }
            }]
        });
        let lowered = lower_workflow_authoring_source(
            &serde_json::to_string(&source).expect("source"),
            WorkflowSourceFormat::Json,
            &catalog,
        )
        .expect("mapped child call");
        let call = &lowered.document.definition.nodes["call"];
        assert_eq!(call.input.type_name, "example.child-input/v1");
        assert_eq!(call.output.type_name, "example.projected/v1");
        assert!(lowered.document.definition.edges[0].transform.is_some());
    }

    #[test]
    fn structured_source_v3_lowers_exact_immutable_workflow_calls() {
        let schema = ValueSchema {
            type_name: "example.call/v1".to_string(),
            schema: serde_json::json!({"type": "string"}),
        };
        let child = WorkflowDefinition {
            schema_version: WORKFLOW_DEFINITION_SCHEMA_VERSION,
            name: "example/child".to_string(),
            input: schema.clone(),
            output: schema,
            nodes: BTreeMap::from([(
                "input".to_string(),
                NodeDefinition {
                    id: "input".to_string(),
                    name: "Input".to_string(),
                    kind: NodeKind::Input,
                    dataflow: WorkflowNodeDataflowPolicy::Direct,
                    input: ValueSchema {
                        type_name: "example.call/v1".to_string(),
                        schema: serde_json::json!({"type": "string"}),
                    },
                    output: ValueSchema {
                        type_name: "example.call/v1".to_string(),
                        schema: serde_json::json!({"type": "string"}),
                    },
                    resources: Vec::new(),
                    configuration: serde_json::json!({"gate_version": 1}),
                },
            )]),
            entries: vec!["input".to_string()],
            exits: vec!["input".to_string()],
            edges: Vec::new(),
        };
        let identity =
            WorkflowDefinitionIdentity::for_definition("example/child", &child).expect("identity");
        let mut catalog = authoring_catalog();
        catalog
            .workflow_definitions
            .insert(identity.definition_id.clone(), child);
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/parent",
            "title": "Parent",
            "steps": [{
                "id": "child",
                "workflow_call": {
                    "version": WORKFLOW_CALL_VERSION,
                    "target": {"kind": "definition", "identity": identity}
                }
            }]
        });
        let lowered = lower_workflow_authoring_source(
            &serde_json::to_string(&source).expect("source"),
            WorkflowSourceFormat::Json,
            &catalog,
        )
        .expect("workflow call source");
        let node = &lowered.document.definition.nodes["child"];
        assert_eq!(node.kind, NodeKind::WorkflowCall);
        let call: WorkflowCallConfiguration =
            serde_json::from_value(node.configuration.clone()).expect("call");
        assert_eq!(call.target.definition_identity().kind, "example/child");
        let preview = lowered.document.compilation_preview(&catalog, None);
        assert!(preview.compiled.is_some());
    }

    #[test]
    fn structured_source_v3_rejects_unavailable_workflow_call() {
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/parent",
            "title": "Parent",
            "steps": [{
                "id": "child",
                "workflow_call": {
                    "version": 1,
                    "target": {
                        "kind": "definition",
                        "identity": {
                            "kind": "example/child",
                            "definition_id": "example/child:missing",
                            "definition_version": 1
                        }
                    }
                }
            }]
        });
        assert!(
            lower_workflow_authoring_source(
                &serde_json::to_string(&source).expect("source"),
                WorkflowSourceFormat::Json,
                &authoring_catalog(),
            )
            .is_err()
        );
    }

    #[test]
    fn structured_source_v3_lowers_durable_input_and_approval_gates() {
        let schema = serde_json::json!({
            "type_name": "example.request/v1",
            "schema": {"type": "string"}
        });
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/gates",
            "title": "Gates",
            "steps": [
                {
                    "id": "request",
                    "input": {
                        "schema": schema,
                        "resources": [{"resource": "operator", "access": "read"}]
                    }
                },
                {
                    "id": "approve",
                    "needs": ["request"],
                    "approval": {"schema": schema}
                }
            ]
        });
        let lowered = lower_workflow_authoring_source(
            &serde_json::to_string(&source).expect("source"),
            WorkflowSourceFormat::Json,
            &authoring_catalog(),
        )
        .expect("gate source");
        let input = &lowered.document.definition.nodes["request"];
        let approval = &lowered.document.definition.nodes["approve"];
        assert_eq!(input.kind, NodeKind::Input);
        assert_eq!(approval.kind, NodeKind::Approval);
        assert_eq!(input.input, input.output);
        assert_eq!(approval.input, approval.output);
        assert_eq!(input.resources, vec![ResourceClaim::read("operator")]);
        assert_eq!(input.configuration, serde_json::json!({"gate_version": 1}));
    }

    #[test]
    fn structured_source_v3_rejects_malformed_gate_fields() {
        let source = serde_json::json!({
            "workflow_source_version": 3,
            "workflow_id": "example/gate-invalid",
            "title": "Invalid gate",
            "steps": [{
                "id": "request",
                "input": {
                    "schema": {"type_name": "example.request/v1", "schema": {"type": "string"}},
                    "unexpected": true
                }
            }]
        });
        assert!(
            lower_workflow_authoring_source(
                &serde_json::to_string(&source).expect("source"),
                WorkflowSourceFormat::Json,
                &authoring_catalog(),
            )
            .is_err()
        );
    }

    #[test]
    fn structured_source_v3_rejects_future_references_and_unknown_fields() {
        let source = r#"{
            "workflow_source_version": 3,
            "workflow_id": "example/invalid",
            "title": "Invalid",
            "steps": [{
                "id": "first",
                "needs": ["later"],
                "uses": "missing/block@1",
                "with": {},
                "unknown": true
            }]
        }"#;
        assert!(
            lower_workflow_authoring_source(
                source,
                WorkflowSourceFormat::Json,
                &authoring_catalog(),
            )
            .is_err()
        );
    }

    #[test]
    fn structured_source_v3_rejects_future_version_and_document_size() {
        let future = serde_json::json!({
            "workflow_source_version": WORKFLOW_SOURCE_DOCUMENT_VERSION + 1,
            "workflow_id": "example/future",
            "title": "Future",
            "steps": [{
                "id": "gate",
                "input": {"schema": source_interface_schema("example.interface/v1")}
            }]
        });
        let error = lower_workflow_authoring_source(
            &serde_json::to_string(&future).expect("future source"),
            WorkflowSourceFormat::Json,
            &authoring_catalog(),
        )
        .expect_err("future source version");
        assert!(error.to_string().contains("version 4"));

        let oversized = " ".repeat(MAX_WORKFLOW_AUTHORING_DOCUMENT_BYTES + 1);
        let error = lower_workflow_authoring_source(
            &oversized,
            WorkflowSourceFormat::Json,
            &authoring_catalog(),
        )
        .expect_err("oversized source");
        assert!(error.to_string().contains("workflow source exceeds"));
    }

    #[test]
    fn structured_source_v3_rejects_selector_and_transform_depth_bounds() {
        let schema = ValueSchema {
            type_name: "example.value/v1".to_string(),
            schema: serde_json::json!({}),
        };
        let oversized_selector = WorkflowValueSelector {
            version: WORKFLOW_VALUE_SELECTOR_VERSION,
            segments: (0..=MAX_VALUE_SELECTOR_SEGMENTS)
                .map(|index| WorkflowValueSelectorSegment::Field {
                    name: format!("field-{index}"),
                })
                .collect(),
        };
        let selector_source = serde_json::json!({
            "workflow_source_version": WORKFLOW_SOURCE_DOCUMENT_VERSION,
            "workflow_id": "example/selector-bound",
            "title": "Selector bound",
            "steps": [{
                "id": "first",
                "input": {"schema": schema}
            }, {
                "id": "second",
                "needs": ["first"],
                "input_from": {"step": "first", "select": oversized_selector},
                "input": {"schema": schema}
            }]
        });
        let error = lower_workflow_authoring_source(
            &serde_json::to_string(&selector_source).expect("selector source"),
            WorkflowSourceFormat::Json,
            &authoring_catalog(),
        )
        .expect_err("selector segment bound");
        assert!(error.to_string().contains("value selector exceeds"));

        let mut expression = WorkflowTransformExpression::Constant {
            value: serde_json::Value::Null,
        };
        for _ in 0..=MAX_TRANSFORM_DEPTH {
            expression = WorkflowTransformExpression::Default {
                value: Box::new(expression),
                default: Box::new(WorkflowTransformExpression::Constant {
                    value: serde_json::Value::Null,
                }),
            };
        }
        let transform_source = serde_json::json!({
            "workflow_source_version": WORKFLOW_SOURCE_DOCUMENT_VERSION,
            "workflow_id": "example/transform-depth",
            "title": "Transform depth",
            "steps": [{
                "id": "first",
                "input": {"schema": schema}
            }, {
                "id": "second",
                "needs": ["first"],
                "input_expression": WorkflowTransform {
                    version: WORKFLOW_TRANSFORM_VERSION,
                    expression,
                    output: schema.clone(),
                },
                "input": {"schema": schema}
            }]
        });
        let error = lower_workflow_authoring_source(
            &serde_json::to_string(&transform_source).expect("transform source"),
            WorkflowSourceFormat::Json,
            &authoring_catalog(),
        )
        .expect_err("transform depth bound");
        assert!(error.to_string().contains("transform depth exceeds"));
    }

    #[test]
    fn structured_source_v3_rejects_schema_mismatch_and_ambiguous_terminal_outputs() {
        let text = ValueSchema {
            type_name: "example.text/v1".to_string(),
            schema: serde_json::json!({"type": "string"}),
        };
        let number = ValueSchema {
            type_name: "example.number/v1".to_string(),
            schema: serde_json::json!({"type": "integer"}),
        };
        let mismatched = serde_json::json!({
            "workflow_source_version": WORKFLOW_SOURCE_DOCUMENT_VERSION,
            "workflow_id": "example/schema-mismatch",
            "title": "Schema mismatch",
            "steps": [{
                "id": "first",
                "input": {"schema": text}
            }, {
                "id": "second",
                "needs": ["first"],
                "input_from": {"step": "first"},
                "input": {"schema": number}
            }]
        });
        let error = lower_workflow_authoring_source(
            &serde_json::to_string(&mismatched).expect("mismatched source"),
            WorkflowSourceFormat::Json,
            &authoring_catalog(),
        )
        .expect_err("schema mismatch");
        assert!(error.to_string().contains("must match exactly"));

        let ambiguous = serde_json::json!({
            "workflow_source_version": WORKFLOW_SOURCE_DOCUMENT_VERSION,
            "workflow_id": "example/multiple-outputs",
            "title": "Multiple outputs",
            "input": text,
            "steps": [{
                "id": "entry",
                "input": {"schema": text}
            }, {
                "id": "text",
                "needs": ["entry"],
                "input": {"schema": text}
            }, {
                "id": "number",
                "needs": ["entry"],
                "input_expression": {
                    "version": WORKFLOW_TRANSFORM_VERSION,
                    "expression": {"operation": "constant", "value": 1},
                    "output": number
                },
                "input": {"schema": number}
            }]
        });
        let error = lower_workflow_authoring_source(
            &serde_json::to_string(&ambiguous).expect("multiple-output source"),
            WorkflowSourceFormat::Json,
            &authoring_catalog(),
        )
        .expect_err("ambiguous terminal interface");
        assert!(error.to_string().contains("terminal output interface"));
    }

    #[test]
    fn source_v3_shorthand_lowers_deterministically_through_exact_actions() {
        let block = WorkflowBlockDefinition {
            block_id: "example.echo".to_string(),
            block_version: 1,
            plugin_id: "bcode.example".to_string(),
            operation: "example.echo".to_string(),
            input: ValueSchema {
                type_name: "example.input/v1".to_string(),
                schema: serde_json::json!({"type": "string"}),
            },
            output: ValueSchema {
                type_name: "example.output/v1".to_string(),
                schema: serde_json::json!({"type": "string"}),
            },
            effect: WorkflowBlockEffect::ReadOnly,
            resources: Vec::new(),
            authorization: WorkflowBlockAuthorization {
                capability: WorkflowToolCapability::ReadOnly,
                explicit_grant_required: false,
            },
            timeout_ms: 30_000,
            cancellation_supported: true,
            reconciliation: WorkflowBlockReconciliation::IdempotentReplay,
            automatic_retry: None,
            preparation_required: false,
        };
        let key = workflow_block_catalog_key(&block);
        let action = WorkflowAuthoringActionDescriptor {
            version: WORKFLOW_AUTHORING_ACTION_DESCRIPTOR_VERSION,
            action_key: "echo".to_string(),
            action_version: 1,
            plugin_id: "bcode.example".to_string(),
            input: block.input.clone(),
            target_block: key.clone(),
            input_adapter: Some(WorkflowTransform {
                version: WORKFLOW_TRANSFORM_VERSION,
                expression: WorkflowTransformExpression::Input {
                    source: WORKFLOW_TRANSFORM_SOURCE_CURRENT.to_string(),
                    path: String::new(),
                },
                output: block.input.clone(),
            }),
        };
        let mut catalog = authoring_catalog();
        catalog.plugins.insert("bcode.example".to_string());
        catalog.blocks.insert(key, block);
        catalog
            .authoring_actions
            .insert(action.catalog_key(), action);
        let source = r"
workflow_source_version: 3
workflow_id: example/concise
title: Concise
steps:
  - id: first
    echo: first
  - id: final
    echo: second
";
        let first = lower_workflow_authoring_source(source, WorkflowSourceFormat::Yaml, &catalog)
            .expect("source-v3 shorthand");
        let second = lower_workflow_authoring_source(source, WorkflowSourceFormat::Yaml, &catalog)
            .expect("repeat lowering");
        assert_eq!(first, second);
        assert_eq!(first.profile, WorkflowSourceProfile::Structured);
        assert_eq!(first.source_map.entries[0].node_id, "first");
        assert_eq!(first.source_map.entries[1].node_id, "final");
        assert_eq!(first.document.definition.edges[0].from, "first");
        assert_eq!(first.document.definition.edges[0].to, "final");
        assert_eq!(
            first.document.plugin_input_defaults["final"],
            serde_json::json!("second")
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn checked_in_source_v3_shorthand_lowers_identically() {
        let block = WorkflowBlockDefinition {
            block_id: "exec".to_string(),
            block_version: 1,
            plugin_id: "bcode.shell".to_string(),
            operation: "exec".to_string(),
            input: ValueSchema {
                type_name: "bcode.shell.exec/v1".to_string(),
                schema: serde_json::json!({
                    "oneOf": [
                        {"type": "string", "minLength": 1},
                        {"type": "object"}
                    ]
                }),
            },
            output: ValueSchema {
                type_name: "bcode.shell.exec-result/v1".to_string(),
                schema: serde_json::json!({"type": "object"}),
            },
            effect: WorkflowBlockEffect::Mutating,
            resources: vec![ResourceClaim::write("repository")],
            authorization: WorkflowBlockAuthorization {
                capability: WorkflowToolCapability::Mutating,
                explicit_grant_required: true,
            },
            timeout_ms: 300_000,
            cancellation_supported: true,
            reconciliation: WorkflowBlockReconciliation::RepairRequired,
            automatic_retry: None,
            preparation_required: false,
        };
        let action = WorkflowAuthoringActionDescriptor {
            version: WORKFLOW_AUTHORING_ACTION_DESCRIPTOR_VERSION,
            action_key: "run".to_string(),
            action_version: 1,
            plugin_id: "bcode.shell".to_string(),
            input: block.input.clone(),
            target_block: workflow_block_catalog_key(&block),
            input_adapter: None,
        };
        let mut catalog = authoring_catalog();
        catalog.plugins.insert("bcode.shell".to_string());
        catalog
            .blocks
            .insert(workflow_block_catalog_key(&block), block);
        catalog
            .authoring_actions
            .insert(action.catalog_key(), action);
        let lowered = [
            lower_workflow_authoring_source(
                include_str!("../../../fixtures/workflows/concise-run.workflow.json"),
                WorkflowSourceFormat::Json,
                &catalog,
            )
            .expect("source-v3 JSON"),
            lower_workflow_authoring_source(
                include_str!("../../../fixtures/workflows/concise-run.workflow.yaml"),
                WorkflowSourceFormat::Yaml,
                &catalog,
            )
            .expect("source-v3 YAML"),
            lower_workflow_authoring_source(
                include_str!("../../../fixtures/workflows/concise-run.workflow.toml"),
                WorkflowSourceFormat::Toml,
                &catalog,
            )
            .expect("source-v3 TOML"),
        ];
        assert_eq!(lowered[0], lowered[1]);
        assert_eq!(lowered[1], lowered[2]);
        let preview = lowered[0].document.compilation_preview(&catalog, None);
        let compiled = preview.compiled.expect("compiled source-v3 workflow");
        assert_eq!(
            compiled.input_defaults,
            serde_json::json!("printf 'first\\n'")
        );
        assert!(compiled.definition.edges[0].transform.is_some());

        let mut catalog_without_shell = catalog.clone();
        catalog_without_shell.plugins.remove("bcode.shell");
        catalog_without_shell
            .blocks
            .retain(|_, block| block.plugin_id != "bcode.shell");
        catalog_without_shell
            .authoring_actions
            .retain(|_, descriptor| descriptor.plugin_id != "bcode.shell");
        let error = lower_workflow_authoring_source(
            include_str!("../../../fixtures/workflows/concise-run.workflow.yaml"),
            WorkflowSourceFormat::Yaml,
            &catalog_without_shell,
        )
        .expect_err("disabled shell plugin must remove run");
        let message = error.to_string();
        assert!(message.contains("run") && message.contains("unavailable"));

        let echo_block = WorkflowBlockDefinition {
            block_id: "echo".to_string(),
            block_version: 1,
            plugin_id: "bcode.example".to_string(),
            operation: "echo".to_string(),
            input: ValueSchema {
                type_name: "bcode.example.echo/v1".to_string(),
                schema: serde_json::json!({"type": "string"}),
            },
            output: ValueSchema {
                type_name: "bcode.example.echo-result/v1".to_string(),
                schema: serde_json::json!({"type": "string"}),
            },
            effect: WorkflowBlockEffect::ReadOnly,
            resources: Vec::new(),
            authorization: WorkflowBlockAuthorization {
                capability: WorkflowToolCapability::ReadOnly,
                explicit_grant_required: false,
            },
            timeout_ms: 1_000,
            cancellation_supported: true,
            reconciliation: WorkflowBlockReconciliation::IdempotentReplay,
            automatic_retry: None,
            preparation_required: false,
        };
        let echo_action = WorkflowAuthoringActionDescriptor {
            version: WORKFLOW_AUTHORING_ACTION_DESCRIPTOR_VERSION,
            action_key: "echo".to_string(),
            action_version: 1,
            plugin_id: "bcode.example".to_string(),
            input: echo_block.input.clone(),
            target_block: workflow_block_catalog_key(&echo_block),
            input_adapter: None,
        };
        catalog_without_shell
            .plugins
            .insert("bcode.example".to_string());
        catalog_without_shell
            .blocks
            .insert(workflow_block_catalog_key(&echo_block), echo_block);
        catalog_without_shell
            .authoring_actions
            .insert(echo_action.catalog_key(), echo_action);
        lower_workflow_authoring_source(
            "workflow_source_version: 3\nworkflow_id: example/no-shell\ntitle: No shell\nsteps:\n  - id: echo\n    echo: still-available\n",
            WorkflowSourceFormat::Yaml,
            &catalog_without_shell,
        )
        .expect("unrelated action remains usable without shell");
    }

    #[test]
    fn checked_in_workflow_sources_have_identical_compiled_semantics() {
        let json = include_str!("../../../fixtures/workflows/source-defined-input.workflow.json");
        let toml = include_str!("../../../fixtures/workflows/source-defined-input.workflow.toml");
        let from_json = decode_workflow_authoring_source(json, WorkflowSourceFormat::Json)
            .expect("checked-in JSON workflow");
        let from_toml = decode_workflow_authoring_source(toml, WorkflowSourceFormat::Toml)
            .expect("checked-in TOML workflow");
        assert_eq!(from_json, from_toml);
        assert_eq!(
            from_json.source_digest_sha256().expect("JSON digest"),
            from_toml.source_digest_sha256().expect("TOML digest")
        );
        assert_eq!(
            from_json
                .executable_source_digest_sha256()
                .expect("JSON executable digest"),
            from_toml
                .executable_source_digest_sha256()
                .expect("TOML executable digest")
        );
        assert_eq!(
            from_json.compilation_preview(&authoring_catalog(), None),
            from_toml.compilation_preview(&authoring_catalog(), None)
        );
    }

    #[test]
    #[allow(clippy::items_after_statements)]
    fn workflow_authoring_sources_decode_json_and_toml_to_identical_semantics() {
        let document = authored_document();
        let json = serde_json::to_string_pretty(&document).expect("JSON source");
        fn encode_toml_nulls(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::Object(fields) => {
                    for value in fields.values_mut() {
                        encode_toml_nulls(value);
                    }
                }
                serde_json::Value::Array(items) => {
                    for value in items {
                        encode_toml_nulls(value);
                    }
                }
                serde_json::Value::Null => {
                    *value = serde_json::json!({WORKFLOW_TOML_NULL_MARKER: true});
                }
                serde_json::Value::Bool(_)
                | serde_json::Value::Number(_)
                | serde_json::Value::String(_) => {}
            }
        }
        let mut toml_value = serde_json::to_value(&document).expect("TOML value");
        encode_toml_nulls(&mut toml_value);
        let toml = toml::to_string_pretty(&toml_value).expect("TOML source");
        let from_json = decode_workflow_authoring_source(&json, WorkflowSourceFormat::Json)
            .expect("decode JSON source");
        let from_toml = decode_workflow_authoring_source(&toml, WorkflowSourceFormat::Toml)
            .expect("decode TOML source");
        assert_eq!(from_json, document);
        assert_eq!(from_toml, document);
        assert_eq!(
            from_json.source_digest_sha256().expect("JSON digest"),
            from_toml.source_digest_sha256().expect("TOML digest")
        );
        assert_eq!(
            from_json
                .executable_source_digest_sha256()
                .expect("JSON executable digest"),
            from_toml
                .executable_source_digest_sha256()
                .expect("TOML executable digest")
        );
        assert_eq!(
            from_json.compilation_preview(&authoring_catalog(), None),
            from_toml.compilation_preview(&authoring_catalog(), None)
        );
        assert_eq!(
            WorkflowSourceFormat::from_file_name("check.workflow.json").expect("JSON extension"),
            WorkflowSourceFormat::Json
        );
        assert_eq!(
            WorkflowSourceFormat::from_file_name("check.workflow.toml").expect("TOML extension"),
            WorkflowSourceFormat::Toml
        );
    }

    #[test]
    fn workflow_authoring_sources_fail_closed_for_syntax_versions_fields_and_bounds() {
        assert!(
            decode_workflow_authoring_source("{", WorkflowSourceFormat::Json)
                .expect_err("malformed JSON")
                .to_string()
                .contains("line 1")
        );
        assert!(
            decode_workflow_authoring_source("workflow_id = [", WorkflowSourceFormat::Toml)
                .expect_err("malformed TOML")
                .to_string()
                .contains("byte range")
        );
        let mut future = serde_json::to_value(authored_document()).expect("document value");
        future["schema_version"] = serde_json::json!(WORKFLOW_AUTHORING_DOCUMENT_VERSION + 1);
        assert!(
            decode_workflow_authoring_source(
                &serde_json::to_string(&future).expect("future source"),
                WorkflowSourceFormat::Json,
            )
            .is_err()
        );
        future["schema_version"] = serde_json::json!(WORKFLOW_AUTHORING_DOCUMENT_VERSION);
        future["unknown"] = serde_json::json!(true);
        assert!(
            decode_workflow_authoring_source(
                &serde_json::to_string(&future).expect("unknown source"),
                WorkflowSourceFormat::Json,
            )
            .is_err()
        );
        assert!(
            decode_workflow_authoring_source(
                &" ".repeat(MAX_WORKFLOW_AUTHORING_DOCUMENT_BYTES + 1),
                WorkflowSourceFormat::Json,
            )
            .is_err()
        );
        let json_duplicate = r#"{"schema_version":1,"schema_version":1}"#;
        assert!(
            decode_workflow_authoring_source(json_duplicate, WorkflowSourceFormat::Json).is_err()
        );
        let nested_json_duplicate = r#"{"outer":{"value":1,"value":2}}"#;
        assert!(
            decode_workflow_authoring_source(nested_json_duplicate, WorkflowSourceFormat::Json)
                .expect_err("nested duplicate JSON key")
                .to_string()
                .contains("duplicate JSON object key")
        );
        let toml_duplicate = "schema_version = 1\nschema_version = 1\n";
        assert!(
            decode_workflow_authoring_source(toml_duplicate, WorkflowSourceFormat::Toml).is_err()
        );
        let invalid_null_marker = r#"
            schema_version = 1
            workflow_id = "invalid"
            [metadata]
            title = "Invalid"
            [configuration_defaults]
            "$bcode_null" = false
        "#;
        assert!(
            decode_workflow_authoring_source(invalid_null_marker, WorkflowSourceFormat::Toml)
                .expect_err("invalid reserved null marker")
                .to_string()
                .contains("reserved TOML null marker")
        );
        let mixed_null_marker = r#"
            schema_version = 1
            workflow_id = "invalid"
            [metadata]
            title = "Invalid"
            [configuration_defaults]
            "$bcode_null" = true
            other = true
        "#;
        assert!(
            decode_workflow_authoring_source(mixed_null_marker, WorkflowSourceFormat::Toml)
                .expect_err("mixed reserved null marker")
                .to_string()
                .contains("reserved TOML null marker")
        );
        let mut nested = String::from("value = ");
        nested.push_str(&"[".repeat(MAX_WORKFLOW_AUTHORING_JSON_DEPTH + 2));
        nested.push('1');
        nested.push_str(&"]".repeat(MAX_WORKFLOW_AUTHORING_JSON_DEPTH + 2));
        assert!(decode_workflow_authoring_source(&nested, WorkflowSourceFormat::Toml).is_err());
        assert_eq!(
            WorkflowSourceFormat::from_file_name("workflow.yaml").expect("YAML suffix"),
            WorkflowSourceFormat::Yaml
        );
    }

    #[test]
    fn semantic_edit_batch_is_atomic_and_presentation_neutral() {
        let document = authored_document();
        let executable_digest = document
            .executable_source_digest_sha256()
            .expect("executable digest");
        let batch = WorkflowAuthoringEditBatch {
            version: WORKFLOW_AUTHORING_EDIT_VERSION,
            expected_generation: 1,
            edits: vec![
                WorkflowAuthoringEdit::UpdateMetadata {
                    metadata: WorkflowAuthoringMetadata {
                        title: "Edited workflow".to_string(),
                        description: None,
                        labels: BTreeMap::new(),
                    },
                },
                WorkflowAuthoringEdit::UpdatePresentationNamespace {
                    namespace: "bcode.graph".to_string(),
                    value: Some(serde_json::json!({"agent": {"x": 40, "y": 80}})),
                },
            ],
        };
        let updated = apply_workflow_authoring_edits(&document, &batch).expect("semantic edits");
        assert_eq!(updated.metadata.title, "Edited workflow");
        assert_eq!(
            updated.executable_source_digest_sha256().expect("digest"),
            executable_digest
        );
        assert_ne!(
            updated.source_digest_sha256().expect("source digest"),
            document.source_digest_sha256().expect("source digest")
        );
        assert_eq!(document.metadata.title, "Example workflow");
    }

    #[test]
    fn semantic_edit_batch_rejects_complete_batch_on_invalid_result() {
        let document = authored_document();
        let batch = WorkflowAuthoringEditBatch {
            version: WORKFLOW_AUTHORING_EDIT_VERSION,
            expected_generation: 1,
            edits: vec![
                WorkflowAuthoringEdit::UpdateMetadata {
                    metadata: WorkflowAuthoringMetadata {
                        title: "Would otherwise apply".to_string(),
                        description: None,
                        labels: BTreeMap::new(),
                    },
                },
                WorkflowAuthoringEdit::RemoveNode {
                    node_id: "agent".to_string(),
                },
            ],
        };
        assert!(apply_workflow_authoring_edits(&document, &batch).is_err());
        assert_eq!(document.metadata.title, "Example workflow");
        assert!(document.definition.nodes.contains_key("agent"));
    }

    #[test]
    fn semantic_edit_batch_targets_exact_duplicate_edge_occurrence() {
        let mut document = authored_document();
        let repeat_predicate = || PredicateExpression::Equals {
            version: WORKFLOW_PREDICATE_VERSION,
            path: String::new(),
            value: serde_json::json!(true),
        };
        document.definition.edges = vec![
            EdgeDefinition {
                from: "agent".to_string(),
                to: "agent".to_string(),
                kind: EdgeKind::Back {
                    predicate: repeat_predicate(),
                    max_iterations: 2,
                },
                transform: None,
            },
            EdgeDefinition {
                from: "agent".to_string(),
                to: "agent".to_string(),
                kind: EdgeKind::Back {
                    predicate: repeat_predicate(),
                    max_iterations: 3,
                },
                transform: None,
            },
        ];
        let batch = WorkflowAuthoringEditBatch {
            version: WORKFLOW_AUTHORING_EDIT_VERSION,
            expected_generation: 1,
            edits: vec![WorkflowAuthoringEdit::RemoveEdge {
                selector: WorkflowAuthoringEdgeSelector {
                    from: "agent".to_string(),
                    to: "agent".to_string(),
                    occurrence: 1,
                },
            }],
        };
        let updated = apply_workflow_authoring_edits(&document, &batch).expect("remove edge");
        assert_eq!(updated.definition.edges.len(), 1);
        assert!(matches!(
            updated.definition.edges[0].kind,
            EdgeKind::Back {
                max_iterations: 2,
                ..
            }
        ));
    }

    #[test]
    fn schema_form_description_projects_required_native_controls() {
        let document = authored_document();
        let form = WorkflowSchemaFormDescription::from_schema(&document.configuration_schema)
            .expect("schema form");
        assert_eq!(form.version, WORKFLOW_SCHEMA_FORM_VERSION);
        assert_eq!(form.fields[0].control, WorkflowSchemaFormControl::Object);
        assert!(form.fields.iter().any(|field| {
            field.path == "message"
                && field.required
                && field.control == WorkflowSchemaFormControl::Text
        }));
        assert!(form.fields.iter().any(|field| {
            field.path == "duration_ms"
                && !field.required
                && field.control == WorkflowSchemaFormControl::Integer
        }));
    }

    #[test]
    fn semantic_diff_separates_presentation_from_executable_change() {
        let before = authored_document();
        let batch = WorkflowAuthoringEditBatch {
            version: WORKFLOW_AUTHORING_EDIT_VERSION,
            expected_generation: 1,
            edits: vec![WorkflowAuthoringEdit::UpdatePresentationNamespace {
                namespace: "bcode.graph".to_string(),
                value: Some(serde_json::json!({"agent": {"x": 900, "y": 400}})),
            }],
        };
        let after = apply_workflow_authoring_edits(&before, &batch).expect("presentation edit");
        let diff = workflow_authoring_semantic_diff(&before, &after, &authoring_catalog())
            .expect("semantic diff");
        assert!(
            !diff
                .changes
                .contains(&WorkflowAuthoringChangeKind::Executable)
        );
        assert!(
            diff.changes
                .contains(&WorkflowAuthoringChangeKind::Presentation)
        );
        assert!(
            !diff
                .changes
                .contains(&WorkflowAuthoringChangeKind::Metadata)
        );
        assert!(diff.added_nodes.is_empty());
        assert_eq!(diff.before_effects, diff.after_effects);
    }

    fn authoring_catalog() -> WorkflowAuthoringCatalogSnapshot {
        WorkflowAuthoringCatalogSnapshot {
            version: WORKFLOW_AUTHORING_CATALOG_VERSION,
            capabilities: WorkflowAuthoringCapabilitySummary::from(
                &WorkflowProductionCapabilities::current(),
            ),
            plugins: BTreeSet::new(),
            blocks: BTreeMap::new(),
            node_configuration_schemas: workflow_node_configuration_schemas(),
            workflow_definitions: BTreeMap::new(),
            agent_profiles: BTreeSet::from(["build".to_string(), "review".to_string()]),
            authoring_actions: BTreeMap::new(),
        }
    }

    #[test]
    fn plugin_dynamic_bindings_are_confined_to_owner_declared_paths() {
        let schema = ValueSchema {
            type_name: "bcode.shell.exec/v1".to_string(),
            schema: serde_json::json!({
                "type": "object",
                "x-bcode-allow-dynamic-complete-input": false,
                "x-bcode-dynamic-binding-paths": ["commands.*.argv.*", "environment.set.*"]
            }),
        };
        assert!(workflow_dynamic_binding_path_allowed(
            &schema,
            "commands.0.argv.1"
        ));
        assert!(workflow_dynamic_binding_path_allowed(
            &schema,
            "environment.set.VERSION"
        ));
        assert!(!workflow_dynamic_binding_path_allowed(&schema, "script"));
        assert!(!workflow_dynamic_binding_path_allowed(
            &schema,
            "commands.0.timeout_ms"
        ));
        assert!(!workflow_allows_dynamic_complete_input(&schema));

        let unrestricted = ValueSchema::of::<serde_json::Value>();
        assert!(workflow_dynamic_binding_path_allowed(
            &unrestricted,
            "message"
        ));
        assert!(workflow_allows_dynamic_complete_input(&unrestricted));
    }

    #[test]
    fn authoring_catalog_validates_definition_identity_from_stored_logical_kind() {
        let mut catalog = authoring_catalog();
        let definition = authored_document().definition;
        let identity = WorkflowDefinitionIdentity::for_definition("authored/example", &definition)
            .expect("identity");
        catalog
            .workflow_definitions
            .insert(identity.definition_id, definition);

        catalog.validate().expect("catalog");
    }

    #[test]
    fn authoring_catalog_rejects_definition_key_with_stale_content_digest() {
        let mut catalog = authoring_catalog();
        let definition = authored_document().definition;
        let identity = WorkflowDefinitionIdentity::for_definition("authored/example", &definition)
            .expect("identity");
        let mut changed = definition;
        changed.name = "changed".to_string();
        catalog
            .workflow_definitions
            .insert(identity.definition_id, changed);

        let error = catalog.validate().expect_err("stale identity");
        assert!(
            error
                .to_string()
                .contains("does not match exact content identity")
        );
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
            automatic_retry: None,
            preparation_required: false,
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
            automatic_retry: None,
            preparation_required: false,
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
            automatic_retry: None,
            preparation_required: false,
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
        let prompt: WorkflowPromptConfiguration = serde_json::from_value(
            compiled
                .definition
                .node("agent")
                .expect("agent")
                .configuration
                .clone(),
        )
        .expect("agent configuration");
        assert_eq!(prompt.agent_profile, "build");
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
    fn authored_plugin_defaults_are_separate_from_owner_contract_and_semantically_editable() {
        let (mut document, catalog, block) = authored_mutating_block_document();
        document.bindings.clear();
        document.plugin_input_defaults.insert(
            "commit".to_string(),
            serde_json::json!({"message": "authored"}),
        );
        let before_contract = document.definition.nodes["commit"].configuration.clone();
        let batch = WorkflowAuthoringEditBatch {
            version: WORKFLOW_AUTHORING_EDIT_VERSION,
            expected_generation: 1,
            edits: vec![WorkflowAuthoringEdit::UpdatePluginInputDefaults {
                node_id: "commit".to_string(),
                defaults: Some(serde_json::json!({"message": "edited"})),
            }],
        };
        let edited = apply_workflow_authoring_edits(&document, &batch).expect("edit defaults");
        assert_eq!(
            edited.definition.nodes["commit"].configuration,
            before_contract
        );
        let compiled = edited
            .compilation_preview(&catalog, None)
            .compiled
            .expect("compiled preview");
        assert_eq!(
            compiled.plugin_input_defaults["commit"],
            serde_json::json!({"message": "edited"})
        );
        assert_eq!(
            serde_json::from_value::<WorkflowBlockDefinition>(
                edited.definition.nodes["commit"].configuration.clone()
            )
            .expect("owner contract"),
            block
        );

        let invalid = WorkflowAuthoringEditBatch {
            version: WORKFLOW_AUTHORING_EDIT_VERSION,
            expected_generation: 1,
            edits: vec![WorkflowAuthoringEdit::UpdatePluginInputDefaults {
                node_id: "commit".to_string(),
                defaults: Some(serde_json::json!({"message": 42})),
            }],
        };
        assert!(apply_workflow_authoring_edits(&document, &invalid).is_err());
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
            automatic_retry: None,
            preparation_required: false,
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
            input: None,
            output: None,
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
            input: None,
            output: None,
        };
        let node = document.definition.nodes.get_mut("agent").expect("agent");
        node.kind = NodeKind::WorkflowCall;
        node.resources.clear();
        node.configuration = serde_json::to_value(&call).expect("call");

        let catalog = authoring_catalog();
        let unavailable = document.compilation_preview(&catalog, None);
        assert!(unavailable.compiled.is_none());
        assert!(unavailable.validation.diagnostics.iter().any(|diagnostic| {
            diagnostic.document_path.ends_with(".configuration.target")
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
            input: None,
            output: None,
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
            .agent_execution_target(PromptContextTarget::SharedParentSequential);
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
            NodeKind::FanOut,
            NodeKind::PluginBlock,
            NodeKind::Input,
            NodeKind::Approval,
        ] {
            assert_eq!(
                capabilities.node_support(kind),
                WorkflowCapabilitySupport::Supported
            );
        }
        assert_eq!(
            capabilities.node_support(NodeKind::Retry),
            WorkflowCapabilitySupport::Unsupported
        );
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
            WorkflowCapabilitySupport::Supported
        );
        assert_eq!(capabilities.fan_out, WorkflowCapabilitySupport::Supported);
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
            input: None,
            output: None,
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
                "invalid_fan_out_configuration",
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
            WorkflowCapabilitySupport::Supported
        );
        assert_eq!(
            WorkflowProductionCapabilities::current().automatic_retry_policy_version,
            Some(WORKFLOW_AUTOMATIC_RETRY_POLICY_VERSION)
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
            WorkflowCapabilitySupport::Supported
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
    fn typed_value_selectors_support_objects_arrays_and_whole_values() {
        let input = serde_json::json!({
            "commands": [
                {"status": "exited", "exit_code": 7},
                {"status": "exited", "exit_code": 0}
            ],
            "expected": 7
        });
        let command_exit = WorkflowValueSelector {
            version: WORKFLOW_VALUE_SELECTOR_VERSION,
            segments: vec![
                WorkflowValueSelectorSegment::Field {
                    name: "commands".to_string(),
                },
                WorkflowValueSelectorSegment::Index { index: 0 },
                WorkflowValueSelectorSegment::Field {
                    name: "exit_code".to_string(),
                },
            ],
        };
        let expected = WorkflowValueSelector {
            version: WORKFLOW_VALUE_SELECTOR_VERSION,
            segments: vec![WorkflowValueSelectorSegment::Field {
                name: "expected".to_string(),
            }],
        };
        assert!(
            PredicateExpression::SelectedEquals {
                version: WORKFLOW_PREDICATE_VERSION,
                selector: command_exit.clone(),
                value: serde_json::json!(7),
            }
            .evaluate_value(&input)
            .expect("selected equality")
        );
        assert!(
            PredicateExpression::SelectedValuesEqual {
                version: WORKFLOW_PREDICATE_VERSION,
                left_selector: command_exit.clone(),
                right_selector: expected,
            }
            .evaluate_value(&input)
            .expect("selected values equality")
        );
        assert!(
            PredicateExpression::SelectedNumericCompare {
                version: WORKFLOW_PREDICATE_VERSION,
                left_selector: command_exit.clone(),
                right_selector: WorkflowValueSelector {
                    version: WORKFLOW_VALUE_SELECTOR_VERSION,
                    segments: vec![
                        WorkflowValueSelectorSegment::Field {
                            name: "commands".to_string(),
                        },
                        WorkflowValueSelectorSegment::Index { index: 1 },
                        WorkflowValueSelectorSegment::Field {
                            name: "exit_code".to_string(),
                        },
                    ],
                },
                comparison: PredicateNumericComparison::GreaterThan,
            }
            .evaluate_value(&input)
            .expect("selected numeric comparison")
        );

        let transform = WorkflowTransform {
            version: WORKFLOW_TRANSFORM_VERSION,
            expression: WorkflowTransformExpression::SelectedInput {
                source: WORKFLOW_TRANSFORM_SOURCE_CURRENT.to_string(),
                selector: command_exit,
            },
            output: ValueSchema {
                type_name: "exit-code/v1".to_string(),
                schema: serde_json::json!({"type": "integer"}),
            },
        };
        assert_eq!(
            transform
                .evaluate(&[WorkflowTransformInput {
                    name: WORKFLOW_TRANSFORM_SOURCE_CURRENT,
                    value: &input,
                }])
                .expect("selected transform"),
            serde_json::json!(7)
        );
        let whole = WorkflowValueSelector {
            version: WORKFLOW_VALUE_SELECTOR_VERSION,
            segments: Vec::new(),
        };
        assert_eq!(whole.select(&input).expect("whole value"), &input);
        let version_one = WorkflowTransform {
            version: WORKFLOW_TRANSFORM_MIN_VERSION,
            expression: WorkflowTransformExpression::SelectedInput {
                source: WORKFLOW_TRANSFORM_SOURCE_CURRENT.to_string(),
                selector: whole,
            },
            output: ValueSchema {
                type_name: "object/v1".to_string(),
                schema: serde_json::json!({"type": "object"}),
            },
        };
        assert!(version_one.validate().is_err());
    }

    #[test]
    fn typed_value_selectors_reject_missing_invalid_and_unbounded_segments() {
        let input = serde_json::json!({"commands": []});
        let missing = WorkflowValueSelector {
            version: WORKFLOW_VALUE_SELECTOR_VERSION,
            segments: vec![
                WorkflowValueSelectorSegment::Field {
                    name: "commands".to_string(),
                },
                WorkflowValueSelectorSegment::Index { index: 0 },
            ],
        };
        assert!(missing.select(&input).is_err());
        assert!(
            WorkflowValueSelector {
                version: WORKFLOW_VALUE_SELECTOR_VERSION + 1,
                segments: Vec::new(),
            }
            .validate()
            .is_err()
        );
        assert!(
            WorkflowValueSelector {
                version: WORKFLOW_VALUE_SELECTOR_VERSION,
                segments: vec![WorkflowValueSelectorSegment::Field {
                    name: String::new(),
                }],
            }
            .validate()
            .is_err()
        );
        assert!(
            WorkflowValueSelector {
                version: WORKFLOW_VALUE_SELECTOR_VERSION,
                segments: vec![WorkflowValueSelectorSegment::Index {
                    index: MAX_VALUE_SELECTOR_INDEX + 1,
                }],
            }
            .validate()
            .is_err()
        );
        assert!(
            WorkflowValueSelector {
                version: WORKFLOW_VALUE_SELECTOR_VERSION,
                segments: (0..=MAX_VALUE_SELECTOR_SEGMENTS)
                    .map(|index| WorkflowValueSelectorSegment::Index { index })
                    .collect(),
            }
            .validate()
            .is_err()
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
            version: WORKFLOW_TRANSFORM_VERSION + 1,
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
    fn versioned_agent_configuration_rejects_workflow_skill_selection_and_escalation() {
        let contract = WorkflowPromptConfiguration {
            version: WORKFLOW_PROMPT_CONFIGURATION_VERSION,
            execution_target: PromptContextTarget::FreshIsolated,
            agent_profile: "build".to_string(),
            provider: Some("configured".to_string()),
            model: Some("configured".to_string()),
            structured_output: PromptStructuredOutputPolicy {
                schema: ValueSchema::of::<serde_json::Value>(),
                strict: true,
            },
            read_only: true,
            tool_capability: WorkflowToolCapability::ReadOnly,
            tool_allowlist: vec!["filesystem.read".to_string()],
            timeout_ms: 30_000,
            prompt_mode: "json_input".to_string(),
            system_prompt: "Use the `commit-message` skill when available.".to_string(),
        };
        contract.validate().expect("valid contract");
        let mut encoded = serde_json::to_value(&contract).expect("serialize");
        encoded["skills"] = serde_json::json!([]);
        assert!(serde_json::from_value::<WorkflowPromptConfiguration>(encoded).is_err());
        let mut escalated = contract;
        escalated.tool_capability = WorkflowToolCapability::Mutating;
        assert!(escalated.validate().is_err());
    }

    #[test]
    fn workflow_block_preparation_contract_is_versioned_bounded_and_strict() {
        let block = WorkflowBlockDefinition {
            block_id: "exec".to_string(),
            block_version: 1,
            plugin_id: "bcode.shell".to_string(),
            operation: "exec".to_string(),
            input: ValueSchema::of::<serde_json::Value>(),
            output: ValueSchema::of::<serde_json::Value>(),
            effect: WorkflowBlockEffect::Mutating,
            resources: vec![ResourceClaim::write("repository")],
            authorization: WorkflowBlockAuthorization {
                capability: WorkflowToolCapability::Mutating,
                explicit_grant_required: true,
            },
            timeout_ms: 30_000,
            cancellation_supported: true,
            reconciliation: WorkflowBlockReconciliation::RepairRequired,
            automatic_retry: None,
            preparation_required: true,
        };
        let request = WorkflowBlockPreparationRequest {
            version: WORKFLOW_BLOCK_PREPARATION_VERSION,
            block,
            context: WorkflowBlockPreparationContext {
                run_id: "run-1".to_string(),
                node_id: "shell".to_string(),
                activation_id: "activation-1".to_string(),
                attempt: 0,
                preparation_identity: "workflow-preparation:run-1:shell:activation-1".to_string(),
                workspace_root: std::path::PathBuf::from("/workspace"),
            },
            input: serde_json::json!({"argv": ["true"]}),
        };
        let encoded = serde_json::to_value(&request).expect("request");
        assert!(serde_json::from_value::<WorkflowBlockPreparationRequest>(encoded).is_ok());
        request.validate().expect("valid request");
        let mut future_request = request.clone();
        future_request.version += 1;
        assert!(future_request.validate().is_err());
        let mut invalid_context = request.clone();
        invalid_context.context.attempt = 1;
        assert!(invalid_context.validate().is_err());
        let mut oversized_request = request;
        oversized_request.input =
            serde_json::Value::String("x".repeat(MAX_WORKFLOW_BLOCK_PREPARATION_REQUEST_BYTES));
        assert!(oversized_request.validate().is_err());
        let response = WorkflowBlockPreparationResponse {
            version: WORKFLOW_BLOCK_PREPARATION_VERSION,
            input_sha256: "a".repeat(64),
            owner_id: "bcode.shell".to_string(),
            operation_facts: serde_json::json!({"program": "true", "arguments": []}),
            descriptor: serde_json::json!({"input_sha256": "a".repeat(64)}),
            diagnostics: Vec::new(),
        };
        response.validate().expect("response");
        let mut future = response.clone();
        future.version += 1;
        assert!(future.validate().is_err());
        let mut oversized_response = response.clone();
        oversized_response.descriptor =
            serde_json::Value::String("x".repeat(MAX_WORKFLOW_BLOCK_PREPARATION_BYTES));
        assert!(oversized_response.validate().is_err());
        let mut too_many_diagnostics = response.clone();
        too_many_diagnostics.diagnostics =
            vec!["diagnostic".to_string(); MAX_WORKFLOW_BLOCK_PREPARATION_DIAGNOSTICS + 1];
        assert!(too_many_diagnostics.validate().is_err());
        let input_sha256 = response.input_sha256.clone();
        let owner_id = response.owner_id.clone();
        let mut encoded = serde_json::to_value(&response).expect("serialize");
        encoded["private_owner_state"] = serde_json::json!(true);
        assert!(serde_json::from_value::<WorkflowBlockPreparationResponse>(encoded).is_err());
        let mut null_facts = response.clone();
        null_facts.operation_facts = serde_json::Value::Null;
        assert!(null_facts.validate().is_err());
        let mut wrong_owner = response.clone();
        wrong_owner.owner_id.clear();
        assert!(wrong_owner.validate().is_err());
        let mut stale_input = response;
        stale_input.input_sha256 = "z".repeat(64);
        assert!(stale_input.validate().is_err());
        assert_eq!(input_sha256.len(), 64);
        assert_eq!(owner_id, "bcode.shell");
    }

    #[test]
    fn concise_prompt_source_expands_to_complete_safe_configuration() {
        let input = ValueSchema {
            type_name: "example.prompt-input/v1".to_string(),
            schema: serde_json::json!({"type": "object"}),
        };
        let output = ValueSchema {
            type_name: "example.prompt-output/v1".to_string(),
            schema: serde_json::json!({"type": "object"}),
        };
        let prompt = WorkflowStructuredSourceConcisePrompt {
            text: "Use the commit-message skill if available; return JSON.".to_string(),
            input_value: None,
            input: input.clone(),
            output: output.clone(),
            agent_profile: "review".to_string(),
            provider: Some("configured".to_string()),
            model: Some("configured".to_string()),
            execution_target: PromptContextTarget::FixedGenerationFork,
            read_only: true,
            tool_allowlist: vec!["filesystem.read".to_string()],
            timeout_ms: 30_000,
            resources: vec![ResourceClaim::read("repository")],
        };
        let expanded = prompt.expand().expect("prompt");
        assert_eq!(expanded.input, input);
        assert_eq!(expanded.output, output);
        assert_eq!(expanded.configuration.prompt_mode, "json_input");
        assert_eq!(
            expanded.configuration.execution_target,
            PromptContextTarget::FixedGenerationFork
        );
        assert!(expanded.configuration.read_only);
        assert_eq!(
            expanded.configuration.tool_capability,
            WorkflowToolCapability::ReadOnly
        );
        assert_eq!(
            expanded.configuration.structured_output.schema,
            expanded.output
        );
        assert!(expanded.configuration.structured_output.strict);
        assert!(
            expanded
                .configuration
                .system_prompt
                .contains("commit-message")
        );
    }

    #[test]
    fn typed_assertions_enforce_utf8_truncation_byte_length_sha_and_artifact_boundaries() {
        let selector = WorkflowValueSelector {
            version: WORKFLOW_VALUE_SELECTOR_VERSION,
            segments: vec![WorkflowValueSelectorSegment::Field {
                name: "stream".to_string(),
            }],
        };
        let text = PredicateExpression::SelectedAssertion {
            version: WORKFLOW_PREDICATE_VERSION,
            selector: selector.clone(),
            assertion: WorkflowValueAssertion::TextEquals {
                expected: "hello".to_string(),
            },
        };
        let complete = serde_json::json!({
            "stream": {
                "text": "hello",
                "encoding": "utf8",
                "truncated": false,
                "byte_length": 5,
                "checksum_sha256": "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
            }
        });
        assert!(text.evaluate_value(&complete).expect("complete text"));
        let mut binary = complete.clone();
        binary["stream"]["encoding"] = serde_json::json!("binary");
        assert!(text.evaluate_value(&binary).is_err());
        let mut truncated = complete.clone();
        truncated["stream"]["truncated"] = serde_json::json!(true);
        assert!(text.evaluate_value(&truncated).is_err());

        let length = PredicateExpression::SelectedAssertion {
            version: WORKFLOW_PREDICATE_VERSION,
            selector: selector.clone(),
            assertion: WorkflowValueAssertion::ByteLength { expected: 5 },
        };
        assert!(length.evaluate_value(&complete).expect("length"));
        let checksum = PredicateExpression::SelectedAssertion {
            version: WORKFLOW_PREDICATE_VERSION,
            selector,
            assertion: WorkflowValueAssertion::Sha256 {
                expected: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
                    .to_string(),
            },
        };
        assert!(checksum.evaluate_value(&complete).expect("checksum"));
        let artifact = serde_json::json!({
            "byte_length": 5,
            "checksum_sha256": "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            "artifact_id": "opaque"
        });
        assert!(
            WorkflowValueAssertion::ByteLength { expected: 5 }
                .evaluate(&artifact)
                .expect("artifact length")
        );
        assert!(
            WorkflowValueAssertion::Sha256 {
                expected: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
                    .to_string()
            }
            .evaluate(&artifact)
            .expect("artifact checksum")
        );
        assert!(
            WorkflowValueAssertion::TextEquals {
                expected: "secret".to_string()
            }
            .evaluate(&artifact)
            .is_err()
        );
    }

    #[test]
    fn agent_execution_target_serializes_explicitly() {
        let step = Step::configured_task(
            "agent",
            NodeKind::Agent,
            serde_json::json!({"prompt_mode": "json_input"}),
            |value: u32, _context| async move { Ok(value) },
        )
        .agent_execution_target(PromptContextTarget::SharedParentSequential);
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

    fn prompt_configuration(schema: &ValueSchema, read_only: bool) -> WorkflowPromptConfiguration {
        WorkflowPromptConfiguration {
            version: WORKFLOW_PROMPT_CONFIGURATION_VERSION,
            execution_target: PromptContextTarget::FreshIsolated,
            agent_profile: "build".to_string(),
            provider: None,
            model: None,
            structured_output: PromptStructuredOutputPolicy {
                schema: schema.clone(),
                strict: true,
            },
            read_only,
            tool_capability: if read_only {
                WorkflowToolCapability::ReadOnly
            } else {
                WorkflowToolCapability::Mutating
            },
            tool_allowlist: Vec::new(),
            timeout_ms: 30_000,
            prompt_mode: "json_input".to_string(),
            system_prompt: "Perform the requested operation.".to_string(),
        }
    }

    fn prompt_node(
        id: &str,
        schema: &ValueSchema,
        configuration: WorkflowPromptConfiguration,
    ) -> NodeDefinition {
        NodeDefinition {
            id: id.to_string(),
            name: id.to_string(),
            kind: NodeKind::Agent,
            dataflow: WorkflowNodeDataflowPolicy::Direct,
            input: schema.clone(),
            output: schema.clone(),
            resources: Vec::new(),
            configuration: serde_json::to_value(configuration).expect("prompt configuration"),
        }
    }

    #[test]
    fn production_admission_allows_mutating_prompts_without_prescribing_successors() {
        let schema = ValueSchema {
            type_name: "example.value/v1".to_string(),
            schema: serde_json::json!({"type": "object", "additionalProperties": true}),
        };
        let prompt = prompt_node("mutate", &schema, prompt_configuration(&schema, false));
        let definition = WorkflowDefinition {
            schema_version: WORKFLOW_DEFINITION_SCHEMA_VERSION,
            name: "mutating-prompt".to_string(),
            input: schema.clone(),
            output: schema,
            nodes: BTreeMap::from([("mutate".to_string(), prompt)]),
            entries: vec!["mutate".to_string()],
            exits: vec!["mutate".to_string()],
            edges: Vec::new(),
        };
        let admission = definition
            .production_admission(&WorkflowProductionCapabilities::current())
            .expect("valid definition");
        assert!(
            admission.is_supported(),
            "mutating prompt topology is the workflow author's choice: {:?}",
            admission.diagnostics
        );
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
