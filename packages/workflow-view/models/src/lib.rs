#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Portable, renderer-neutral workflow projection contracts.

use serde::{Deserialize, Serialize};

/// Current workflow projection schema version.
pub const WORKFLOW_VIEW_VERSION: u32 = 1;

/// Bounded catalog snapshot of durable workflow runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCatalogView {
    /// Projection schema version.
    pub version: u32,
    /// Runs ordered by the application catalog query.
    pub runs: Vec<WorkflowRunListItem>,
}

/// Portable workflow run catalog item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRunListItem {
    pub run_id: String,
    pub definition_id: String,
    pub definition_version: u32,
    pub status: WorkflowRunStatus,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Portable workflow run state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    Running,
    Paused,
    Completed,
    Failed,
    Cancelled,
    RepairRequired,
}

/// Bounded semantic projection of one workflow run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRunView {
    pub version: u32,
    pub run: WorkflowRunListItem,
    pub nodes: Vec<WorkflowNodeView>,
    pub edges: Vec<WorkflowEdgeView>,
    pub waits: Vec<WorkflowWaitView>,
    pub mutation_approvals: Vec<WorkflowMutationApprovalView>,
    pub attempts: Vec<WorkflowAttemptView>,
    pub outputs: Vec<WorkflowOutputView>,
    pub descendant_runs: Vec<WorkflowDescendantRunView>,
    pub child_sessions: Vec<WorkflowChildSessionView>,
    pub terminal: Option<WorkflowTerminalView>,
    pub health: WorkflowProjectionHealth,
}

/// Semantic node kind, independent of any renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowNodeKind {
    Task,
    Agent,
    Branch,
    Repeat,
    Retry,
    Parallel,
    FanOut,
    PluginBlock,
    Input,
    Approval,
    WorkflowCall,
}

/// Projected node and latest bounded activation state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowNodeView {
    pub node_id: String,
    pub name: String,
    pub kind: WorkflowNodeKind,
    pub activation_id: Option<String>,
    pub status: WorkflowNodeStatus,
}

/// Semantic node state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowNodeStatus {
    NotStarted,
    Pending,
    Running,
    WaitingInput,
    WaitingApproval,
    WaitingMutationApproval,
    Completed,
    Failed,
    Cancelled,
    Skipped,
    RepairRequired,
    Unknown(String),
}

/// Portable graph edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowEdgeView {
    pub from: String,
    pub to: String,
    pub kind: String,
}

/// Durable wait kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowWaitKind {
    Input,
    Approval,
}

/// Pending durable wait.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowWaitView {
    pub node_id: String,
    pub activation_id: String,
    pub kind: WorkflowWaitKind,
    pub input: Option<serde_json::Value>,
    pub requested_at_ms: u64,
}

/// Pending plugin mutation approval. Presentation fields cannot authorize execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowMutationApprovalView {
    pub approval_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub requested_at_ms: u64,
    pub expires_at_ms: Option<u64>,
}

/// Bounded dispatch attempt state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowAttemptView {
    pub node_id: String,
    pub activation_id: String,
    pub attempt: u32,
    pub dispatch_identity: String,
    pub status: String,
    pub has_receipt: bool,
    pub prepared_at_ms: u64,
    pub terminal_at_ms: Option<u64>,
}

/// Availability of a canonical validated output value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "availability", rename_all = "snake_case")]
pub enum WorkflowOutputValue {
    Resolved { value: serde_json::Value },
    Unresolved,
}

/// Portable typed node output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowOutputView {
    pub output_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub schema_id: String,
    pub schema_version: u32,
    pub checksum_sha256: String,
    pub value: WorkflowOutputValue,
    pub artifact_reference: Option<String>,
    pub created_at_ms: u64,
}

/// Bounded descendant workflow run and its parent linkage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowDescendantRunView {
    pub run: WorkflowRunListItem,
    pub parent_run_id: String,
    pub parent_node_id: String,
    pub depth: u32,
}

/// Link from workflow execution to a child session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowChildSessionView {
    pub node_id: String,
    pub activation_id: String,
    pub attempt: u32,
    pub session_id: String,
}

/// Stable terminal outcome of a workflow run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum WorkflowTerminalView {
    Completed { output_id: String },
    Failed,
    Cancelled,
    RepairRequired,
}

/// Projection health when canonical or derived data cannot be represented completely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WorkflowProjectionHealth {
    Current,
    Degraded { reason: String },
    RepairRequired { reason: String },
    UnsupportedVersion { version: u32 },
}
