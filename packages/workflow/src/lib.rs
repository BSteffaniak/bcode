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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinSet;

/// Boxed asynchronous workflow operation.
pub type StepFuture<T> = Pin<Box<dyn Future<Output = Result<T, WorkflowError>> + Send>>;

type StepFn<I, O> = dyn Fn(I, StepContext) -> StepFuture<O> + Send + Sync;

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

/// Context supplied to an executing workflow step.
#[derive(Debug, Clone)]
pub struct StepContext {
    cancellation: WorkflowCancellation,
    events: Option<WorkflowEventSender>,
}

impl StepContext {
    /// Return the workflow cancellation token.
    #[must_use]
    pub fn cancellation(&self) -> WorkflowCancellation {
        self.cancellation.clone()
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
    _types: PhantomData<fn(I) -> O>,
}

impl<I, O> Clone for Step<I, O> {
    fn clone(&self) -> Self {
        Self {
            run: Arc::clone(&self.run),
            fragment: self.fragment.clone(),
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
    O: JsonSchema + Send + 'static,
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
            configuration,
        };
        let id = node.id.clone();
        let operation = Arc::new(operation);
        let step_id = id.clone();
        let run = Arc::new(move |input, context: StepContext| {
            let operation = Arc::clone(&operation);
            let step_id = step_id.clone();
            Box::pin(async move {
                context.ensure_active(step_id.clone())?;
                context.emit(WorkflowEvent::StepStarted {
                    step: step_id.clone(),
                });
                let output = operation(input, context.clone()).await?;
                context.emit(WorkflowEvent::StepCompleted { step: step_id });
                Ok(output)
            }) as StepFuture<O>
        });
        Self {
            run,
            fragment: DefinitionFragment {
                nodes: vec![node],
                edges: Vec::new(),
                entries: vec![id.clone()],
                exits: vec![id],
            },
            _types: PhantomData,
        }
    }

    /// Run `next` after this step and carry its typed output into `next`.
    #[must_use]
    pub fn then<N>(self, next: Step<O, N>) -> Step<I, N>
    where
        N: JsonSchema + Send + 'static,
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
                    if max_iterations == 0 {
                        return Err(WorkflowError::Build {
                            path: repeat_id,
                            message: "repeat max_iterations must be greater than zero".to_string(),
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
                            step: repeat_id,
                            message: format!(
                                "repeat condition remained true after {max_iterations} iterations"
                            ),
                        });
                    }
                    Ok(output)
                })
            }),
            fragment,
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
        N: JsonSchema + Send + 'static,
    {
        let name = name.into();
        let branch_id = name.clone();
        let branch_node = NodeDefinition {
            id: branch_id.clone(),
            name,
            kind: NodeKind::Branch,
            input: ValueSchema::of::<O>(),
            output: ValueSchema::of::<O>(),
            configuration: serde_json::to_value(predicate.expression())
                .expect("workflow predicate should serialize to JSON"),
        };
        let prior_run = Arc::clone(&self.run);
        let true_run = Arc::clone(&when_true.run);
        let false_run = Arc::clone(&when_false.run);
        let expression = predicate.expression;
        let run_expression = expression.clone();
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
                let expression = run_expression.clone();
                Box::pin(async move {
                    let branch_input = prior_run(input, context.clone()).await?;
                    if expression.evaluate(&branch_input)? {
                        true_run(branch_input, context).await
                    } else {
                        false_run(branch_input, context).await
                    }
                })
            }),
            fragment,
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
        I: Clone,
    {
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
            configuration: serde_json::json!({"max_attempts": max_attempts}),
        });
        fragment.exits = vec![retry_id.clone()];
        Self {
            run: Arc::new(move |input, context| {
                let run = Arc::clone(&run);
                let retry_id = retry_id.clone();
                Box::pin(async move {
                    if max_attempts == 0 {
                        return Err(WorkflowError::Build {
                            path: retry_id,
                            message: "retry max_attempts must be greater than zero".to_string(),
                        });
                    }
                    let mut last_error = None;
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
                                last_error = Some(error);
                                if attempt < max_attempts {
                                    tokio::task::yield_now().await;
                                }
                            }
                        }
                    }
                    Err(last_error.unwrap_or_else(|| WorkflowError::Build {
                        path: retry_id,
                        message: "retry completed without executing an attempt".to_string(),
                    }))
                })
            }),
            fragment,
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
            _types: PhantomData,
        }
    }
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
    O: JsonSchema + Send + 'static,
{
    let name = name.into();
    let fan_out_id = name.clone();
    let Step {
        run,
        fragment: mut body,
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
        configuration: serde_json::json!({"max_concurrency": max_concurrency}),
    });
    body.entries = body_entries;
    body.exits = vec![fan_out_id.clone()];
    Step {
        run: Arc::new(move |inputs, context| {
            let run = Arc::clone(&run);
            let fan_out_id = fan_out_id.clone();
            Box::pin(async move {
                if max_concurrency == 0 {
                    return Err(WorkflowError::Build {
                        path: fan_out_id,
                        message: "fan_out max_concurrency must be greater than zero".to_string(),
                    });
                }
                context.ensure_active(fan_out_id.clone())?;
                let mut inputs = inputs.into_iter().enumerate();
                let mut tasks = JoinSet::new();
                for _ in 0..max_concurrency {
                    let Some((index, input)) = inputs.next() else {
                        break;
                    };
                    spawn_fan_out_task(&mut tasks, Arc::clone(&run), context.clone(), index, input);
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
            })
        }),
        fragment: body,
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
    A: JsonSchema + Send + 'static,
    B: JsonSchema + Send + 'static,
{
    let name = generated_parallel_name(&left, &right);
    parallel_named(name, left, right)
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
    A: JsonSchema + Send + 'static,
    B: JsonSchema + Send + 'static,
{
    let join_id = name.into();
    let Step {
        run: left_run,
        fragment: left_fragment,
        _types: _,
    } = left;
    let Step {
        run: right_run,
        fragment: right_fragment,
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
        configuration: serde_json::Value::Null,
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
    Step {
        run: Arc::new(move |input, context| {
            let left_run = Arc::clone(&left_run);
            let right_run = Arc::clone(&right_run);
            let right_input = input.clone();
            let right_context = context.clone();
            Box::pin(async move {
                let (left, right) = tokio::join!(
                    left_run(input, context),
                    right_run(right_input, right_context)
                );
                Ok((left?, right?))
            })
        }),
        fragment: DefinitionFragment {
            nodes,
            edges,
            entries,
            exits: vec![join_id],
        },
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

/// Builder for one typed workflow.
#[derive(Debug)]
pub struct WorkflowBuilder<I, O> {
    name: String,
    step: Step<I, O>,
}

impl<I, O> WorkflowBuilder<I, O>
where
    I: JsonSchema + Send + 'static,
    O: JsonSchema + Send + 'static,
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
        Ok(Workflow {
            definition,
            run: self.step.run,
            _types: PhantomData,
        })
    }
}

/// A validated typed workflow ready for execution.
pub struct Workflow<I, O> {
    definition: WorkflowDefinition,
    run: Arc<StepFn<I, O>>,
    _types: PhantomData<fn(I) -> O>,
}

impl<I, O> fmt::Debug for Workflow<I, O> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Workflow")
            .field("definition", &self.definition)
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
        self.run_observed(input, cancellation, None).await
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
        self.run_observed(input, cancellation, Some(events)).await
    }

    async fn run_observed(
        &self,
        input: I,
        cancellation: WorkflowCancellation,
        events: Option<WorkflowEventSender>,
    ) -> Result<O, WorkflowError> {
        let first = self
            .definition
            .nodes
            .keys()
            .next()
            .cloned()
            .unwrap_or_else(|| self.definition.name.clone());
        let context = StepContext {
            cancellation,
            events,
        };
        let result = async {
            context.ensure_active(first)?;
            let output = (self.run)(input, context.clone()).await?;
            context.ensure_active(self.definition.name.clone())?;
            validate_output(&self.definition.name, &output)?;
            Ok(output)
        }
        .await;
        context.emit(WorkflowEvent::WorkflowFinished {
            outcome: match &result {
                Ok(_) => WorkflowOutcome::Succeeded,
                Err(WorkflowError::Cancelled { .. }) => WorkflowOutcome::Cancelled,
                Err(WorkflowError::TimedOut { .. }) => WorkflowOutcome::TimedOut,
                Err(_) => WorkflowOutcome::Failed,
            },
        });
        result
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

        let output = workflow
            .run(ReviewState {
                needs_fixes: true,
                attempts: 0,
            })
            .await
            .expect("run");
        assert_eq!(output.attempts, 1);
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
