#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Durable workflow persistence owned independently from session transcript storage.

use bcode_workflow::WorkflowDefinition;
use rusqlite::{Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use thiserror::Error;

const DATABASE_FILE: &str = "workflow.db";
const SCHEMA_VERSION: u32 = 3;
const MAX_ID_BYTES: usize = 512;
const MAX_INLINE_JSON_BYTES: usize = 1_048_576;

/// Durable workflow run status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Run may produce new activations.
    Running,
    /// Run is paused for external input or explicit operator action.
    Paused,
    /// Run is terminal and successful.
    Completed,
    /// Run is terminal and failed.
    Failed,
    /// Run is terminal and cancelled.
    Cancelled,
    /// Run cannot continue automatically without explicit repair.
    RepairRequired,
}

impl RunStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::RepairRequired => "repair_required",
        }
    }
}

/// Side-effect classification persisted before external dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchSideEffect {
    /// Operation cannot mutate external state.
    ReadOnly,
    /// Operation may mutate external state and must never be blindly duplicated.
    Mutating,
}

impl DispatchSideEffect {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Mutating => "mutating",
        }
    }
}

/// Bounded run summary used by normal list/status paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunSummary {
    pub run_id: String,
    pub definition_id: String,
    pub definition_version: u32,
    pub workspace_snapshot: String,
    pub parent_session_id: Option<String>,
    pub status: RunStatus,
    pub cancellation_requested_at_ms: Option<u64>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Keyset cursor for bounded attempt history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptCursor {
    pub prepared_at_ms: u64,
    pub dispatch_identity: String,
}

/// Bounded durable attempt summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptSummary {
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub attempt: u32,
    pub dispatch_identity: String,
    pub side_effect: DispatchSideEffect,
    pub status: String,
    pub has_receipt: bool,
    pub prepared_at_ms: u64,
    pub admitted_at_ms: Option<u64>,
    pub terminal_at_ms: Option<u64>,
}

/// One bounded workflow event row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowEventRow {
    pub event_seq: u64,
    pub run_id: String,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub created_at_ms: u64,
}

/// Persisted workflow execution limits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRunLimits {
    /// Absolute wall-clock deadline, when configured.
    pub deadline_at_ms: Option<u64>,
    /// Maximum total node attempts in the run.
    pub node_execution_cap: u32,
    /// Maximum concurrently running nodes.
    pub concurrency_cap: u32,
    /// Maximum cycle/repeat activations.
    pub cycle_cap: u32,
    /// Maximum attempts per activation.
    pub retry_cap: u32,
}

impl Default for WorkflowRunLimits {
    fn default() -> Self {
        Self {
            deadline_at_ms: None,
            node_execution_cap: 1_000,
            concurrency_cap: 8,
            cycle_cap: 100,
            retry_cap: 3,
        }
    }
}

/// Durable workflow run creation request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewWorkflowRun {
    /// Stable run identity.
    pub run_id: String,
    /// Definition identity and version.
    pub definition_id: String,
    pub definition_version: u32,
    /// Immutable workspace snapshot identity.
    pub workspace_snapshot: String,
    /// Optional parent session identity serialized without coupling this store to session logic.
    pub parent_session_id: Option<String>,
    /// Creation timestamp supplied by the host clock.
    pub created_at_ms: u64,
    /// Persisted execution limits enforced by durable admission.
    pub limits: WorkflowRunLimits,
}

/// One durable node activation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewActivation {
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub dependency_generation: u64,
    pub created_at_ms: u64,
}

/// Prepared external-operation intent written before dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedAttempt {
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub attempt: u32,
    pub side_effect: DispatchSideEffect,
    /// Owner-specific bounded dispatch intent.
    pub intent: serde_json::Value,
    pub prepared_at_ms: u64,
}

impl PreparedAttempt {
    /// Return the stable dispatch identity derived solely from durable attempt identity.
    #[must_use]
    pub fn dispatch_identity(&self) -> String {
        dispatch_identity(
            &self.run_id,
            &self.node_id,
            &self.activation_id,
            self.attempt,
        )
    }
}

/// Durable dispatch receipt returned by an external operation owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchReceipt {
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub attempt: u32,
    pub dispatch_identity: String,
    pub receipt: serde_json::Value,
    pub admitted_at_ms: u64,
}

/// One active durable workflow attempt to signal after cancellation intent commits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveAttemptCancellation {
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub attempt: u32,
    pub dispatch_identity: String,
    pub receipt: Option<serde_json::Value>,
}

/// Result of signaling active attempt owners after durable cancellation intent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CancellationPropagationSummary {
    pub signalled: Vec<String>,
    pub already_terminal: Vec<String>,
}

/// Owner boundary for active workflow attempt cancellation.
pub trait AttemptCancellationOwner: Sync {
    /// Signal one active owner using its stable dispatch identity and optional receipt.
    ///
    /// Implementations must be idempotent for repeated calls with the same request.
    ///
    /// # Errors
    ///
    /// Returns an error when the owner cannot durably accept or safely repeat cancellation.
    fn cancel_attempt<'a>(
        &'a self,
        request: &'a ActiveAttemptCancellation,
    ) -> Pin<Box<dyn Future<Output = Result<(), WorkflowStoreError>> + Send + 'a>>;
}

/// One explicit durable workflow decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowDecision {
    pub decision_id: String,
    pub run_id: String,
    pub node_id: Option<String>,
    pub decision_type: String,
    pub value: serde_json::Value,
    pub created_at_ms: u64,
}

/// One bounded durable workflow grant record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowGrant {
    pub grant_id: String,
    pub run_id: String,
    pub node_id: String,
    pub scope: serde_json::Value,
    pub granted_at_ms: u64,
    pub expires_at_ms: Option<u64>,
}

/// Access mode for one durable workflow resource lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceLeaseMode {
    Read,
    Write,
}

impl ResourceLeaseMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }
}

/// One durable workflow resource lease.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowResourceLease {
    pub lease_id: String,
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub resource_key: String,
    pub mode: ResourceLeaseMode,
    pub acquired_at_ms: u64,
    pub expires_at_ms: Option<u64>,
}

/// One durable workflow projection checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowProjectionCheckpoint {
    pub projection_name: String,
    pub projection_version: u32,
    pub last_event_seq: u64,
}

/// Result of bounded restart reconciliation for prepared attempts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconciliationSummary {
    /// Mutating attempts moved to repair-required because no receipt proves their outcome.
    pub repair_required: Vec<String>,
    /// Read-only prepared attempts left eligible for owner-specific redispatch/reconciliation.
    pub safe_prepared: Vec<String>,
}

/// Receipt-backed attempt requiring bounded owner observation after restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptReconciliationRequest {
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub attempt: u32,
    pub dispatch_identity: String,
    pub side_effect: DispatchSideEffect,
    pub receipt: serde_json::Value,
}

/// Owner-reported durable external attempt state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AttemptObservation {
    /// Owner still recognizes admission but work has not started.
    Admitted,
    /// External work is active.
    Running,
    /// External work completed with schema-validated output.
    Succeeded { output: ValidatedOutput },
    /// External work failed terminally.
    Failed { message: String },
    /// External work was cancelled terminally.
    Cancelled,
    /// Owner cannot prove the current or terminal state.
    Unknown,
}

/// Owner boundary used for bounded restart reconciliation.
pub trait AttemptStatusObserver {
    /// Observe one receipt-backed attempt without mutating workflow storage.
    ///
    /// # Errors
    ///
    /// Returns an error when the owner status API cannot be queried safely.
    fn observe(
        &self,
        request: &AttemptReconciliationRequest,
    ) -> Result<AttemptObservation, WorkflowStoreError>;
}

/// Summary of receipt-backed restart reconciliation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReceiptReconciliationSummary {
    pub admitted: Vec<String>,
    pub running: Vec<String>,
    pub succeeded: Vec<String>,
    pub failed: Vec<String>,
    pub cancelled: Vec<String>,
    pub repair_required: Vec<String>,
    pub unresolved_read_only: Vec<String>,
}

/// One bounded inconsistency found by an explicit workflow doctor operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "issue", rename_all = "snake_case")]
pub enum WorkflowDoctorIssue {
    /// A run and its repair-required attempts disagree about whether repair is needed.
    RepairStatusMismatch {
        run_status: RunStatus,
        repair_required_attempts: u64,
    },
    /// A completed activation has no matching validated output, or a non-completed activation
    /// references one.
    ActivationOutputMismatch {
        node_id: String,
        activation_id: String,
        activation_status: String,
        output_id: Option<String>,
    },
    /// Persisted attempt identity does not match its stable identity components.
    AttemptIdentityMismatch {
        dispatch_identity: String,
        expected_dispatch_identity: String,
    },
}

/// Bounded, non-mutating result of an explicit workflow doctor operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowDoctorReport {
    pub run_id: String,
    pub issues: Vec<WorkflowDoctorIssue>,
    /// The requested bound prevented a complete inspection, so additional issues may exist.
    pub truncated: bool,
}

/// Explicit operator resolution for one ambiguous mutating attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "resolution", rename_all = "snake_case")]
pub enum RepairResolution {
    /// The owner or operator proved the operation succeeded and supplies its validated output.
    ConfirmSucceeded { output: ValidatedOutput },
    /// The owner or operator proved the operation failed terminally.
    ConfirmFailed { message: String },
    /// The owner or operator proved the operation was cancelled.
    ConfirmCancelled { message: String },
    /// Explicitly abandon the ambiguous operation and permit a later, higher-numbered attempt.
    ///
    /// This is the only repair resolution that allows retry after an ambiguous mutation. It does
    /// not dispatch work itself.
    AbandonForExplicitRetry { reason: String },
}

/// Result of an explicit repair operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairResult {
    pub dispatch_identity: String,
    pub attempt_status: String,
    pub run_status: RunStatus,
}

/// Durable validated output persisted before downstream activation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatedOutput {
    pub output_id: String,
    pub run_id: String,
    pub node_id: String,
    pub activation_id: String,
    pub schema_id: String,
    pub schema_version: u32,
    pub value: serde_json::Value,
    pub artifact_reference: Option<String>,
    pub created_at_ms: u64,
}

/// Errors returned by durable workflow persistence.
#[derive(Debug, Error)]
pub enum WorkflowStoreError {
    /// Database operation failed.
    #[error("workflow database error: {0}")]
    Database(#[from] rusqlite::Error),
    /// Filesystem operation failed.
    #[error("workflow store I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Definition serialization failed.
    #[error("workflow definition serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    /// Persisted data violated the storage contract.
    #[error("invalid workflow store data: {0}")]
    InvalidData(String),
}

/// Canonical persisted definition identity and content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredWorkflowDefinition {
    /// Stable definition identity.
    pub definition_id: String,
    /// Positive definition version.
    pub version: u32,
    /// SHA-256 of canonical serialized definition JSON.
    pub checksum_sha256: String,
    /// Canonical serialized definition.
    pub definition_json: String,
}

/// Durable workflow database.
#[derive(Debug)]
pub struct WorkflowStore {
    path: PathBuf,
    connection: Connection,
}

impl WorkflowStore {
    /// Open or create the canonical workflow database below an explicit Bcode state directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory/database cannot be opened or migrations fail.
    pub fn open_in_state_dir(state_dir: &Path) -> Result<Self, WorkflowStoreError> {
        Self::open_at(&workflow_database_path(state_dir))
    }

    /// Open the production-default workflow database.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory/database cannot be opened or migrations fail.
    pub fn open_default() -> Result<Self, WorkflowStoreError> {
        Self::open_in_state_dir(&bcode_config::default_state_dir())
    }

    fn open_at(path: &Path) -> Result<Self, WorkflowStoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut connection = Connection::open(path)?;
        connection.pragma_update(None, "foreign_keys", true)?;
        migrate(&mut connection)?;
        Ok(Self {
            path: path.to_path_buf(),
            connection,
        })
    }

    /// Return the canonical database path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Persist an immutable normalized definition and checksum.
    ///
    /// Re-persisting byte-identical content is idempotent. Reusing one definition/version for
    /// different content fails closed.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identity/version, serialization failure, checksum conflict, or
    /// database failure.
    pub fn persist_definition(
        &mut self,
        definition_id: &str,
        version: u32,
        definition: &WorkflowDefinition,
    ) -> Result<StoredWorkflowDefinition, WorkflowStoreError> {
        validate_id("definition_id", definition_id)?;
        if version == 0 {
            return Err(WorkflowStoreError::InvalidData(
                "definition version must be positive".to_string(),
            ));
        }
        let definition_json = serde_json::to_string(definition)?;
        let checksum_sha256 = sha256_hex(definition_json.as_bytes());
        let stored = StoredWorkflowDefinition {
            definition_id: definition_id.to_string(),
            version,
            checksum_sha256,
            definition_json,
        };
        let transaction = self.connection.transaction()?;
        persist_definition_transaction(&transaction, &stored)?;
        transaction.commit()?;
        Ok(stored)
    }

    /// Load one exact definition version with checksum verification.
    ///
    /// # Errors
    ///
    /// Returns an error when the query fails or persisted content does not match its checksum.
    pub fn definition(
        &self,
        definition_id: &str,
        version: u32,
    ) -> Result<Option<StoredWorkflowDefinition>, WorkflowStoreError> {
        let stored = self
            .connection
            .query_row(
                "SELECT definition_id, version, checksum_sha256, definition_json \
                 FROM workflow_definitions WHERE definition_id = ?1 AND version = ?2",
                (definition_id, version),
                |row| {
                    Ok(StoredWorkflowDefinition {
                        definition_id: row.get(0)?,
                        version: row.get(1)?,
                        checksum_sha256: row.get(2)?,
                        definition_json: row.get(3)?,
                    })
                },
            )
            .optional()?;
        if let Some(stored) = &stored
            && sha256_hex(stored.definition_json.as_bytes()) != stored.checksum_sha256
        {
            return Err(WorkflowStoreError::InvalidData(format!(
                "definition checksum mismatch: {} v{}",
                stored.definition_id, stored.version
            )));
        }
        Ok(stored)
    }

    /// Create one durable workflow run bound to an existing exact definition version.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, missing definition, identity conflict, or database
    /// failure.
    pub fn create_run(&mut self, run: &NewWorkflowRun) -> Result<(), WorkflowStoreError> {
        validate_run(run)?;
        let transaction = self.connection.transaction()?;
        let definition_exists = transaction
            .query_row(
                "SELECT 1 FROM workflow_definitions WHERE definition_id = ?1 AND version = ?2",
                (&run.definition_id, run.definition_version),
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !definition_exists {
            return Err(WorkflowStoreError::InvalidData(format!(
                "workflow definition not found: {} v{}",
                run.definition_id, run.definition_version
            )));
        }
        transaction.execute(
            "INSERT INTO workflow_runs \
             (run_id, definition_id, definition_version, workspace_snapshot, parent_session_id, \
              status, deadline_at_ms, node_execution_cap, concurrency_cap, cycle_cap, retry_cap, \
              created_at_ms, updated_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
            (
                &run.run_id,
                &run.definition_id,
                run.definition_version,
                &run.workspace_snapshot,
                &run.parent_session_id,
                RunStatus::Running.as_str(),
                run.limits.deadline_at_ms,
                run.limits.node_execution_cap,
                run.limits.concurrency_cap,
                run.limits.cycle_cap,
                run.limits.retry_cap,
                run.created_at_ms,
            ),
        )?;
        append_event(
            &transaction,
            &run.run_id,
            "run_created",
            &serde_json::to_string(run)?,
            run.created_at_ms,
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Persist one pending activation.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, missing run, duplicate activation, or database
    /// failure.
    pub fn create_activation(
        &mut self,
        activation: &NewActivation,
    ) -> Result<(), WorkflowStoreError> {
        validate_activation(activation)?;
        enforce_activation_limits(&self.connection, activation)?;
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO workflow_activations \
             (run_id, node_id, activation_id, dependency_generation, status, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5)",
            (
                &activation.run_id,
                &activation.node_id,
                &activation.activation_id,
                activation.dependency_generation,
                activation.created_at_ms,
            ),
        )?;
        append_event(
            &transaction,
            &activation.run_id,
            "activation_created",
            &serde_json::to_string(activation)?,
            activation.created_at_ms,
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Persist an immutable workflow decision.
    ///
    /// Re-persisting byte-equivalent content is idempotent. Conflicting content at one decision
    /// identity fails closed.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, oversized JSON, missing run/node, identity
    /// conflict, or database failure.
    pub fn persist_decision(
        &mut self,
        decision: &WorkflowDecision,
    ) -> Result<(), WorkflowStoreError> {
        validate_decision(decision)?;
        let value_json = bounded_json("workflow decision", &decision.value)?;
        let transaction = self.connection.transaction()?;
        let changed = transaction.execute(
            "INSERT OR IGNORE INTO workflow_decisions \
             (decision_id, run_id, node_id, decision_type, value_json, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (
                &decision.decision_id,
                &decision.run_id,
                &decision.node_id,
                &decision.decision_type,
                &value_json,
                decision.created_at_ms,
            ),
        )?;
        if changed == 0 {
            let existing = transaction.query_row(
                "SELECT run_id, node_id, decision_type, value_json, created_at_ms \
                 FROM workflow_decisions WHERE decision_id = ?1",
                [&decision.decision_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, u64>(4)?,
                    ))
                },
            )?;
            if existing
                != (
                    decision.run_id.clone(),
                    decision.node_id.clone(),
                    decision.decision_type.clone(),
                    value_json,
                    decision.created_at_ms,
                )
            {
                return Err(WorkflowStoreError::InvalidData(format!(
                    "workflow decision identity conflict: {}",
                    decision.decision_id
                )));
            }
        } else {
            append_event(
                &transaction,
                &decision.run_id,
                "decision_recorded",
                &serde_json::to_string(decision)?,
                decision.created_at_ms,
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Load one exact durable workflow decision.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, invalid persisted JSON, or database failure.
    pub fn decision(
        &self,
        decision_id: &str,
    ) -> Result<Option<WorkflowDecision>, WorkflowStoreError> {
        validate_id("decision_id", decision_id)?;
        self.connection
            .query_row(
                "SELECT run_id, node_id, decision_type, value_json, created_at_ms \
                 FROM workflow_decisions WHERE decision_id = ?1",
                [decision_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, u64>(4)?,
                    ))
                },
            )
            .optional()?
            .map(
                |(run_id, node_id, decision_type, value_json, created_at_ms)| {
                    Ok(WorkflowDecision {
                        decision_id: decision_id.to_string(),
                        run_id,
                        node_id,
                        decision_type,
                        value: serde_json::from_str(&value_json)?,
                        created_at_ms,
                    })
                },
            )
            .transpose()
    }

    /// Persist an immutable bounded workflow permission grant.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, oversized scope, missing run/node, identity
    /// conflict, or database failure.
    pub fn persist_grant(&mut self, grant: &WorkflowGrant) -> Result<(), WorkflowStoreError> {
        validate_grant(grant)?;
        let scope_json = bounded_json("workflow grant scope", &grant.scope)?;
        let transaction = self.connection.transaction()?;
        let changed = transaction.execute(
            "INSERT OR IGNORE INTO workflow_grants \
             (grant_id, run_id, node_id, scope_json, granted_at_ms, expires_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (
                &grant.grant_id,
                &grant.run_id,
                &grant.node_id,
                &scope_json,
                grant.granted_at_ms,
                grant.expires_at_ms,
            ),
        )?;
        if changed == 0 {
            let existing = transaction.query_row(
                "SELECT run_id, node_id, scope_json, granted_at_ms, expires_at_ms \
                 FROM workflow_grants WHERE grant_id = ?1",
                [&grant.grant_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, u64>(3)?,
                        row.get::<_, Option<u64>>(4)?,
                    ))
                },
            )?;
            if existing
                != (
                    grant.run_id.clone(),
                    grant.node_id.clone(),
                    scope_json,
                    grant.granted_at_ms,
                    grant.expires_at_ms,
                )
            {
                return Err(WorkflowStoreError::InvalidData(format!(
                    "workflow grant identity conflict: {}",
                    grant.grant_id
                )));
            }
        } else {
            append_event(
                &transaction,
                &grant.run_id,
                "grant_recorded",
                &serde_json::to_string(grant)?,
                grant.granted_at_ms,
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Load one exact durable workflow grant.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, invalid persisted JSON, or database failure.
    pub fn grant(&self, grant_id: &str) -> Result<Option<WorkflowGrant>, WorkflowStoreError> {
        validate_id("grant_id", grant_id)?;
        self.connection
            .query_row(
                "SELECT run_id, node_id, scope_json, granted_at_ms, expires_at_ms \
                 FROM workflow_grants WHERE grant_id = ?1",
                [grant_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, u64>(3)?,
                        row.get::<_, Option<u64>>(4)?,
                    ))
                },
            )
            .optional()?
            .map(
                |(run_id, node_id, scope_json, granted_at_ms, expires_at_ms)| {
                    Ok(WorkflowGrant {
                        grant_id: grant_id.to_string(),
                        run_id,
                        node_id,
                        scope: serde_json::from_str(&scope_json)?,
                        granted_at_ms,
                        expires_at_ms,
                    })
                },
            )
            .transpose()
    }

    /// Acquire a durable workflow resource lease while enforcing reader/writer exclusion.
    ///
    /// Reacquiring the exact lease is idempotent. Conflicting active leases fail closed.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, missing activation, lease conflict, identity
    /// conflict, or database failure.
    pub fn acquire_resource_lease(
        &mut self,
        lease: &WorkflowResourceLease,
    ) -> Result<(), WorkflowStoreError> {
        validate_resource_lease(lease)?;
        let transaction = self.connection.transaction()?;
        let existing = resource_lease(&transaction, &lease.lease_id)?;
        if let Some((existing, released_at_ms)) = existing {
            if existing == *lease && released_at_ms.is_none() {
                return Ok(());
            }
            return Err(WorkflowStoreError::InvalidData(format!(
                "workflow resource lease identity conflict: {}",
                lease.lease_id
            )));
        }
        let conflict: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM workflow_resource_leases \
             WHERE run_id = ?1 AND resource_key = ?2 AND released_at_ms IS NULL \
             AND (expires_at_ms IS NULL OR expires_at_ms > ?3) \
             AND (?4 = 'write' OR mode = 'write'))",
            (
                &lease.run_id,
                &lease.resource_key,
                lease.acquired_at_ms,
                lease.mode.as_str(),
            ),
            |row| row.get(0),
        )?;
        if conflict {
            return Err(WorkflowStoreError::InvalidData(format!(
                "workflow resource is already leased incompatibly: {}",
                lease.resource_key
            )));
        }
        transaction.execute(
            "INSERT INTO workflow_resource_leases \
             (lease_id, run_id, node_id, activation_id, resource_key, mode, acquired_at_ms, expires_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            (
                &lease.lease_id,
                &lease.run_id,
                &lease.node_id,
                &lease.activation_id,
                &lease.resource_key,
                lease.mode.as_str(),
                lease.acquired_at_ms,
                lease.expires_at_ms,
            ),
        )?;
        append_event(
            &transaction,
            &lease.run_id,
            "resource_lease_acquired",
            &serde_json::to_string(lease)?,
            lease.acquired_at_ms,
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Release one exact durable resource lease.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed/missing/already released lease or database failure.
    pub fn release_resource_lease(
        &mut self,
        lease_id: &str,
        released_at_ms: u64,
    ) -> Result<(), WorkflowStoreError> {
        validate_id("lease_id", lease_id)?;
        let transaction = self.connection.transaction()?;
        let run_id = transaction
            .query_row(
                "SELECT run_id FROM workflow_resource_leases \
                 WHERE lease_id = ?1 AND released_at_ms IS NULL",
                [lease_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!(
                    "active workflow resource lease not found: {lease_id}"
                ))
            })?;
        transaction.execute(
            "UPDATE workflow_resource_leases SET released_at_ms = ?2 WHERE lease_id = ?1",
            (lease_id, released_at_ms),
        )?;
        append_event(
            &transaction,
            &run_id,
            "resource_lease_released",
            &serde_json::json!({"lease_id": lease_id}).to_string(),
            released_at_ms,
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Advance one workflow projection checkpoint monotonically.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity/version, checkpoint regression, an event sequence
    /// beyond the durable tail, or database failure.
    pub fn advance_projection_checkpoint(
        &mut self,
        checkpoint: &WorkflowProjectionCheckpoint,
    ) -> Result<(), WorkflowStoreError> {
        validate_projection_checkpoint(checkpoint)?;
        let transaction = self.connection.transaction()?;
        let event_tail: u64 = transaction.query_row(
            "SELECT COALESCE(MAX(event_seq), 0) FROM workflow_events",
            [],
            |row| row.get(0),
        )?;
        if checkpoint.last_event_seq > event_tail {
            return Err(WorkflowStoreError::InvalidData(format!(
                "projection checkpoint {} is beyond event tail {event_tail}",
                checkpoint.last_event_seq
            )));
        }
        let existing = transaction
            .query_row(
                "SELECT projection_version, last_event_seq FROM workflow_projection_checkpoints \
                 WHERE projection_name = ?1",
                [&checkpoint.projection_name],
                |row| Ok((row.get::<_, u32>(0)?, row.get::<_, u64>(1)?)),
            )
            .optional()?;
        if let Some((version, sequence)) = existing
            && (version != checkpoint.projection_version || sequence > checkpoint.last_event_seq)
        {
            return Err(WorkflowStoreError::InvalidData(
                "workflow projection checkpoint cannot regress or change version".to_string(),
            ));
        }
        transaction.execute(
            "INSERT INTO workflow_projection_checkpoints \
             (projection_name, projection_version, last_event_seq) VALUES (?1, ?2, ?3) \
             ON CONFLICT(projection_name) DO UPDATE SET last_event_seq = excluded.last_event_seq",
            (
                &checkpoint.projection_name,
                checkpoint.projection_version,
                checkpoint.last_event_seq,
            ),
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Load one exact durable workflow projection checkpoint.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity or database failure.
    pub fn projection_checkpoint(
        &self,
        projection_name: &str,
    ) -> Result<Option<WorkflowProjectionCheckpoint>, WorkflowStoreError> {
        validate_id("projection_name", projection_name)?;
        self.connection
            .query_row(
                "SELECT projection_version, last_event_seq FROM workflow_projection_checkpoints \
                 WHERE projection_name = ?1",
                [projection_name],
                |row| {
                    Ok(WorkflowProjectionCheckpoint {
                        projection_name: projection_name.to_string(),
                        projection_version: row.get(0)?,
                        last_event_seq: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(WorkflowStoreError::from)
    }

    /// Persist prepared intent before an external operation is dispatched.
    ///
    /// This operation is idempotent only for byte-equivalent intent at the same durable attempt
    /// identity. Conflicting intent fails closed.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, unbounded intent, missing activation, conflicting
    /// prepared intent, or database failure.
    pub fn prepare_attempt(
        &mut self,
        attempt: &PreparedAttempt,
    ) -> Result<String, WorkflowStoreError> {
        validate_prepared_attempt(attempt)?;
        enforce_attempt_limits(&self.connection, attempt)?;
        let identity = attempt.dispatch_identity();
        let intent_json = serde_json::to_string(&attempt.intent)?;
        if intent_json.len() > MAX_INLINE_JSON_BYTES {
            return Err(WorkflowStoreError::InvalidData(format!(
                "dispatch intent exceeds {MAX_INLINE_JSON_BYTES} bytes"
            )));
        }
        let checksum = sha256_hex(intent_json.as_bytes());
        let transaction = self.connection.transaction()?;
        let existing = transaction
            .query_row(
                "SELECT dispatch_identity, intent_checksum FROM workflow_attempts \
                 WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3 AND attempt = ?4",
                (
                    &attempt.run_id,
                    &attempt.node_id,
                    &attempt.activation_id,
                    attempt.attempt,
                ),
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        if let Some((existing_identity, existing_checksum)) = existing {
            if existing_identity == identity && existing_checksum == checksum {
                return Ok(identity);
            }
            return Err(WorkflowStoreError::InvalidData(format!(
                "prepared attempt identity conflict: {identity}"
            )));
        }
        transaction.execute(
            "INSERT INTO workflow_attempts \
             (run_id, node_id, activation_id, attempt, dispatch_identity, side_effect, status, \
              intent_json, intent_checksum, prepared_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'prepared', ?7, ?8, ?9)",
            (
                &attempt.run_id,
                &attempt.node_id,
                &attempt.activation_id,
                attempt.attempt,
                &identity,
                attempt.side_effect.as_str(),
                &intent_json,
                &checksum,
                attempt.prepared_at_ms,
            ),
        )?;
        append_event(
            &transaction,
            &attempt.run_id,
            "attempt_prepared",
            &serde_json::to_string(attempt)?,
            attempt.prepared_at_ms,
        )?;
        transaction.commit()?;
        Ok(identity)
    }

    /// Persist validated node output before marking its activation complete.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed/bounded data, missing activation, conflicting output, or
    /// database failure.
    pub fn persist_validated_output(
        &mut self,
        output: &ValidatedOutput,
    ) -> Result<(), WorkflowStoreError> {
        validate_output(output)?;
        let transaction = self.connection.transaction()?;
        persist_validated_output_transaction(&transaction, output)?;
        transaction.commit()?;
        Ok(())
    }

    /// Persist an external admission/service receipt after dispatch.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, unbounded receipt, identity mismatch, a non-
    /// prepared attempt, or database failure.
    pub fn persist_dispatch_receipt(
        &mut self,
        receipt: &DispatchReceipt,
    ) -> Result<(), WorkflowStoreError> {
        validate_dispatch_receipt(receipt)?;
        let receipt_json = serde_json::to_string(&receipt.receipt)?;
        if receipt_json.len() > MAX_INLINE_JSON_BYTES {
            return Err(WorkflowStoreError::InvalidData(format!(
                "dispatch receipt exceeds {MAX_INLINE_JSON_BYTES} bytes"
            )));
        }
        let expected = dispatch_identity(
            &receipt.run_id,
            &receipt.node_id,
            &receipt.activation_id,
            receipt.attempt,
        );
        if receipt.dispatch_identity != expected {
            return Err(WorkflowStoreError::InvalidData(
                "dispatch receipt identity does not match durable attempt identity".to_string(),
            ));
        }
        let transaction = self.connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE workflow_attempts SET status = 'admitted', receipt_json = ?6, admitted_at_ms = ?7 \
             WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3 AND attempt = ?4 \
               AND dispatch_identity = ?5 AND status = 'prepared'",
            (
                &receipt.run_id,
                &receipt.node_id,
                &receipt.activation_id,
                receipt.attempt,
                &receipt.dispatch_identity,
                &receipt_json,
                receipt.admitted_at_ms,
            ),
        )?;
        if changed != 1 {
            return Err(WorkflowStoreError::InvalidData(format!(
                "attempt is not prepared: {}",
                receipt.dispatch_identity
            )));
        }
        append_event(
            &transaction,
            &receipt.run_id,
            "attempt_admitted",
            &serde_json::to_string(receipt)?,
            receipt.admitted_at_ms,
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Reconcile receipt-less prepared attempts after restart without external dispatch.
    ///
    /// Mutating attempts are atomically marked repair-required. Read-only attempts remain prepared
    /// for an owner-specific reconciler or safe redispatch policy.
    ///
    /// # Errors
    ///
    /// Returns an error when the bounded query or transition fails.
    pub fn reconcile_prepared_attempts(
        &mut self,
        limit: usize,
        reconciled_at_ms: u64,
    ) -> Result<ReconciliationSummary, WorkflowStoreError> {
        if limit == 0 {
            return Err(WorkflowStoreError::InvalidData(
                "reconciliation limit must be positive".to_string(),
            ));
        }
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let transaction = self.connection.transaction()?;
        let rows = {
            let mut statement = transaction.prepare(
                "SELECT run_id, dispatch_identity, side_effect FROM workflow_attempts \
                 WHERE status = 'prepared' ORDER BY prepared_at_ms, dispatch_identity LIMIT ?1",
            )?;
            statement
                .query_map([limit], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut summary = ReconciliationSummary::default();
        for (run_id, identity, side_effect) in rows {
            if side_effect == DispatchSideEffect::Mutating.as_str() {
                transaction.execute(
                    "UPDATE workflow_attempts SET status = 'repair_required', terminal_at_ms = ?2 \
                     WHERE dispatch_identity = ?1 AND status = 'prepared'",
                    (&identity, reconciled_at_ms),
                )?;
                transaction.execute(
                    "UPDATE workflow_runs SET status = ?2, updated_at_ms = ?3 WHERE run_id = ?1",
                    (
                        &run_id,
                        RunStatus::RepairRequired.as_str(),
                        reconciled_at_ms,
                    ),
                )?;
                append_event(
                    &transaction,
                    &run_id,
                    "attempt_repair_required",
                    &serde_json::json!({"dispatch_identity": identity}).to_string(),
                    reconciled_at_ms,
                )?;
                summary.repair_required.push(identity);
            } else {
                summary.safe_prepared.push(identity);
            }
        }
        transaction.commit()?;
        Ok(summary)
    }

    /// Reconcile receipt-backed admitted/running attempts through a bounded owner status API.
    ///
    /// Observation is read-only. Each returned state is then persisted atomically. Unknown
    /// mutating outcomes become repair-required and are never redispatched.
    ///
    /// # Errors
    ///
    /// Returns an error when `limit` is invalid, receipt data is malformed, owner observation
    /// fails, or a durable transition cannot be committed.
    pub fn reconcile_receipt_backed_attempts<O>(
        &mut self,
        observer: &O,
        limit: usize,
        reconciled_at_ms: u64,
    ) -> Result<ReceiptReconciliationSummary, WorkflowStoreError>
    where
        O: AttemptStatusObserver + ?Sized,
    {
        let limit = bounded_limit(limit)?;
        let requests = receipt_backed_attempts(&self.connection, limit)?;
        let observations = requests
            .iter()
            .map(|request| observer.observe(request).map(|status| (request, status)))
            .collect::<Result<Vec<_>, _>>()?;
        let transaction = self.connection.transaction()?;
        let mut summary = ReceiptReconciliationSummary::default();
        for (request, observation) in observations {
            apply_attempt_observation(
                &transaction,
                request,
                observation,
                reconciled_at_ms,
                &mut summary,
            )?;
        }
        transaction.commit()?;
        Ok(summary)
    }

    /// Inspect one run for bounded structural inconsistencies without mutating durable state.
    ///
    /// This explicit maintenance API validates stable attempt identities, run/attempt repair
    /// status agreement, and activation/output relationships. It never replays events, dispatches
    /// external work, or changes projections.
    ///
    /// # Errors
    ///
    /// Returns an error when the run identity/limit is invalid, the run does not exist, persisted
    /// status is malformed, or the bounded queries fail.
    #[allow(clippy::too_many_lines)]
    pub fn doctor_run(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<WorkflowDoctorReport, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        bounded_limit(limit)?;
        let run_status = self
            .connection
            .query_row(
                "SELECT status FROM workflow_runs WHERE run_id = ?1",
                [run_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| {
                WorkflowStoreError::InvalidData(format!("workflow run not found: {run_id}"))
            })?;
        let run_status = parse_run_status(&run_status)?;
        let repair_required_attempts = self.connection.query_row(
            "SELECT COUNT(*) FROM workflow_attempts WHERE run_id = ?1 \
             AND status = 'repair_required'",
            [run_id],
            |row| row.get::<_, u64>(0),
        )?;
        let mut issues = Vec::new();
        if (run_status == RunStatus::RepairRequired) != (repair_required_attempts > 0) {
            issues.push(WorkflowDoctorIssue::RepairStatusMismatch {
                run_status,
                repair_required_attempts,
            });
        }

        let report_limit = limit;
        let fetch_limit = report_limit.saturating_add(1);
        let mut scan_truncated = false;
        if issues.len() < fetch_limit {
            let remaining = fetch_limit - issues.len();
            let mut statement = self.connection.prepare(
                "SELECT node_id, activation_id, status, output_id FROM workflow_activations \
                 WHERE run_id = ?1 AND ((status = 'completed' AND (output_id IS NULL OR NOT EXISTS (\
                     SELECT 1 FROM workflow_outputs output WHERE output.run_id = workflow_activations.run_id \
                     AND output.node_id = workflow_activations.node_id \
                     AND output.activation_id = workflow_activations.activation_id \
                     AND output.output_id = workflow_activations.output_id))) \
                 OR (status != 'completed' AND output_id IS NOT NULL)) \
                 ORDER BY node_id, activation_id LIMIT ?2",
            )?;
            let mismatches = statement
                .query_map(
                    (run_id, i64::try_from(remaining).unwrap_or(i64::MAX)),
                    |row| {
                        Ok(WorkflowDoctorIssue::ActivationOutputMismatch {
                            node_id: row.get(0)?,
                            activation_id: row.get(1)?,
                            activation_status: row.get(2)?,
                            output_id: row.get(3)?,
                        })
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?;
            issues.extend(mismatches);
        }

        if issues.len() < fetch_limit {
            let remaining = fetch_limit - issues.len();
            let mut statement = self.connection.prepare(
                "SELECT node_id, activation_id, attempt, dispatch_identity FROM workflow_attempts \
                 WHERE run_id = ?1 ORDER BY prepared_at_ms, dispatch_identity",
            )?;
            let rows = statement.query_map([run_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u32>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;
            let scan_limit = report_limit.saturating_mul(16).max(report_limit);
            let mut mismatch_count = 0;
            for (inspected, row) in rows.enumerate() {
                if inspected >= scan_limit {
                    scan_truncated = true;
                    break;
                }
                let (node_id, activation_id, attempt, identity) = row?;
                let expected = dispatch_identity(run_id, &node_id, &activation_id, attempt);
                if identity != expected {
                    issues.push(WorkflowDoctorIssue::AttemptIdentityMismatch {
                        dispatch_identity: identity,
                        expected_dispatch_identity: expected,
                    });
                    mismatch_count += 1;
                    if issues.len() >= fetch_limit || mismatch_count >= remaining {
                        break;
                    }
                }
            }
        }

        let truncated = scan_truncated || issues.len() > report_limit;
        issues.truncate(report_limit);
        Ok(WorkflowDoctorReport {
            run_id: run_id.to_string(),
            issues,
            truncated,
        })
    }

    /// Explicitly resolve one repair-required ambiguous attempt.
    ///
    /// Repair never dispatches external work. Success requires a validated output; failure and
    /// cancellation are terminal; abandonment records an explicit operator decision before a
    /// later higher-numbered attempt may be prepared. The run leaves repair-required only after
    /// all ambiguous attempts have been resolved.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, missing/non-repairable attempts, mismatched output,
    /// unbounded messages, conflicting output, or database failure.
    pub fn repair_attempt(
        &mut self,
        dispatch_identity: &str,
        resolution: &RepairResolution,
        repaired_at_ms: u64,
    ) -> Result<RepairResult, WorkflowStoreError> {
        validate_id("dispatch_identity", dispatch_identity)?;
        validate_repair_resolution(resolution)?;
        let transaction = self.connection.transaction()?;
        let attempt = repair_required_attempt(&transaction, dispatch_identity)?;
        let (run_id, node_id, activation_id, attempt_number, side_effect) = attempt;
        if parse_side_effect(&side_effect)? != DispatchSideEffect::Mutating {
            return Err(WorkflowStoreError::InvalidData(
                "only ambiguous mutating attempts require explicit repair".to_string(),
            ));
        }
        let expected = crate::dispatch_identity(&run_id, &node_id, &activation_id, attempt_number);
        if expected != dispatch_identity {
            return Err(WorkflowStoreError::InvalidData(
                "repair attempt identity does not match durable attempt identity".to_string(),
            ));
        }

        let (attempt_status, event_type, payload) = match resolution {
            RepairResolution::ConfirmSucceeded { output } => {
                if output.run_id != run_id
                    || output.node_id != node_id
                    || output.activation_id != activation_id
                {
                    return Err(WorkflowStoreError::InvalidData(
                        "repair output identity does not match durable attempt".to_string(),
                    ));
                }
                persist_validated_output_transaction(&transaction, output)?;
                (
                    "succeeded",
                    "attempt_repair_succeeded",
                    serde_json::to_value(resolution)?,
                )
            }
            RepairResolution::ConfirmFailed { .. } => (
                "failed",
                "attempt_repair_failed",
                serde_json::to_value(resolution)?,
            ),
            RepairResolution::ConfirmCancelled { .. } => (
                "cancelled",
                "attempt_repair_cancelled",
                serde_json::to_value(resolution)?,
            ),
            RepairResolution::AbandonForExplicitRetry { .. } => (
                "abandoned",
                "attempt_repair_abandoned",
                serde_json::to_value(resolution)?,
            ),
        };
        let changed = transaction.execute(
            "UPDATE workflow_attempts SET status = ?2, terminal_at_ms = ?3 \
             WHERE dispatch_identity = ?1 AND status = 'repair_required'",
            (dispatch_identity, attempt_status, repaired_at_ms),
        )?;
        if changed != 1 {
            return Err(WorkflowStoreError::InvalidData(format!(
                "attempt cannot be repaired: {dispatch_identity}"
            )));
        }
        append_event(
            &transaction,
            &run_id,
            event_type,
            &serde_json::json!({
                "dispatch_identity": dispatch_identity,
                "resolution": payload,
            })
            .to_string(),
            repaired_at_ms,
        )?;
        let remaining: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM workflow_attempts WHERE run_id = ?1 \
             AND status = 'repair_required')",
            [&run_id],
            |row| row.get(0),
        )?;
        let run_status = if remaining {
            RunStatus::RepairRequired
        } else if matches!(resolution, RepairResolution::AbandonForExplicitRetry { .. }) {
            RunStatus::Running
        } else {
            RunStatus::Paused
        };
        transaction.execute(
            "UPDATE workflow_runs SET status = ?2, updated_at_ms = ?3 WHERE run_id = ?1",
            (&run_id, run_status.as_str(), repaired_at_ms),
        )?;
        transaction.commit()?;
        Ok(RepairResult {
            dispatch_identity: dispatch_identity.to_string(),
            attempt_status: attempt_status.to_string(),
            run_status,
        })
    }

    /// Persist cancellation intent before an executor signals active children.
    ///
    /// The returned value indicates whether this call recorded the first cancellation request.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed identity, missing/terminal run, or database failure.
    pub fn request_cancellation(
        &mut self,
        run_id: &str,
        requested_at_ms: u64,
    ) -> Result<bool, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let transaction = self.connection.transaction()?;
        let changed = transaction.execute(
            "UPDATE workflow_runs SET cancellation_requested_at_ms = ?2, updated_at_ms = ?2 \
             WHERE run_id = ?1 AND cancellation_requested_at_ms IS NULL \
               AND status IN ('running', 'paused')",
            (run_id, requested_at_ms),
        )?;
        if changed == 1 {
            append_event(
                &transaction,
                run_id,
                "cancellation_requested",
                &serde_json::json!({"requested_at_ms": requested_at_ms}).to_string(),
                requested_at_ms,
            )?;
        } else if transaction
            .query_row(
                "SELECT cancellation_requested_at_ms IS NOT NULL FROM workflow_runs \
                 WHERE run_id = ?1",
                [run_id],
                |row| row.get::<_, bool>(0),
            )
            .optional()?
            .is_none()
        {
            return Err(WorkflowStoreError::InvalidData(format!(
                "workflow run not found: {run_id}"
            )));
        }
        transaction.commit()?;
        Ok(changed == 1)
    }

    /// Return bounded active attempts only after cancellation intent is durable.
    ///
    /// Receipts are included when present so the owner can route cancellation to the exact turn or
    /// plugin operation. Receipt-less prepared attempts remain discoverable by stable identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the run identity/limit is invalid, cancellation was not persisted,
    /// receipt JSON is malformed, or the bounded query fails.
    pub fn active_attempt_cancellations(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<ActiveAttemptCancellation>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let limit = bounded_limit(limit)?;
        require_cancellation_requested(&self.connection, run_id)?;
        let mut statement = self.connection.prepare(
            "SELECT run_id, node_id, activation_id, attempt, dispatch_identity, receipt_json \
             FROM workflow_attempts WHERE run_id = ?1 \
             AND status IN ('prepared', 'admitted', 'running') \
             ORDER BY prepared_at_ms, dispatch_identity LIMIT ?2",
        )?;
        statement
            .query_map((run_id, limit), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u32>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })?
            .map(|row| {
                let (run_id, node_id, activation_id, attempt, dispatch_identity, receipt_json) =
                    row?;
                Ok(ActiveAttemptCancellation {
                    run_id,
                    node_id,
                    activation_id,
                    attempt,
                    dispatch_identity,
                    receipt: receipt_json
                        .map(|receipt| serde_json::from_str(&receipt))
                        .transpose()?,
                })
            })
            .collect()
    }

    /// Signal active attempt owners after durable cancellation intent is confirmed.
    ///
    /// Signaling occurs outside a database transaction. Successful signals are marked
    /// `cancelling`; a crash before that marker is safe because owner signaling must be idempotent
    /// and the attempt remains discoverable.
    ///
    /// # Errors
    ///
    /// Returns an error when discovery, owner signaling, or durable marking fails. Unsignalled
    /// attempts remain discoverable for a later retry.
    pub async fn propagate_cancellation<O>(
        &mut self,
        run_id: &str,
        owner: &O,
        limit: usize,
        signalled_at_ms: u64,
    ) -> Result<CancellationPropagationSummary, WorkflowStoreError>
    where
        O: AttemptCancellationOwner + ?Sized,
    {
        let attempts = self.active_attempt_cancellations(run_id, limit)?;
        let mut summary = CancellationPropagationSummary::default();
        for attempt in attempts {
            owner.cancel_attempt(&attempt).await?;
            let transaction = self.connection.transaction()?;
            let changed = transaction.execute(
                "UPDATE workflow_attempts SET status = 'cancelling' \
                 WHERE dispatch_identity = ?1 \
                 AND status IN ('prepared', 'admitted', 'running')",
                [&attempt.dispatch_identity],
            )?;
            if changed == 1 {
                append_event(
                    &transaction,
                    run_id,
                    "attempt_cancellation_signalled",
                    &serde_json::json!({"dispatch_identity": attempt.dispatch_identity})
                        .to_string(),
                    signalled_at_ms,
                )?;
                summary.signalled.push(attempt.dispatch_identity);
            } else {
                summary.already_terminal.push(attempt.dispatch_identity);
            }
            transaction.commit()?;
        }
        Ok(summary)
    }

    /// Return dispatch identities for active children after cancellation intent is durable.
    ///
    /// New owner routers should use [`Self::active_attempt_cancellations`] to retain receipts.
    ///
    /// # Errors
    ///
    /// Returns an error when the run identity/limit is invalid, cancellation was not persisted,
    /// receipt JSON is malformed, or the bounded query fails.
    pub fn active_attempts_for_cancellation(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<String>, WorkflowStoreError> {
        self.active_attempt_cancellations(run_id, limit)
            .map(|attempts| {
                attempts
                    .into_iter()
                    .map(|attempt| attempt.dispatch_identity)
                    .collect()
            })
    }

    /// Return one run summary without replaying workflow events.
    ///
    /// # Errors
    ///
    /// Returns an error when the bounded row query fails or contains invalid status data.
    pub fn run_summary(
        &self,
        run_id: &str,
    ) -> Result<Option<WorkflowRunSummary>, WorkflowStoreError> {
        self.connection
            .query_row(
                "SELECT run_id, definition_id, definition_version, workspace_snapshot, \
                 parent_session_id, status, cancellation_requested_at_ms, created_at_ms, \
                 updated_at_ms FROM workflow_runs WHERE run_id = ?1",
                [run_id],
                run_summary_from_row,
            )
            .optional()?
            .map(parse_run_summary)
            .transpose()
    }

    /// Return a bounded newest-first run list without replaying workflow events.
    ///
    /// # Errors
    ///
    /// Returns an error when `limit` is invalid or the bounded query fails.
    pub fn list_runs(&self, limit: usize) -> Result<Vec<WorkflowRunSummary>, WorkflowStoreError> {
        let limit = bounded_limit(limit)?;
        let mut statement = self.connection.prepare(
            "SELECT run_id, definition_id, definition_version, workspace_snapshot, \
             parent_session_id, status, cancellation_requested_at_ms, created_at_ms, updated_at_ms \
             FROM workflow_runs ORDER BY updated_at_ms DESC, run_id LIMIT ?1",
        )?;
        statement
            .query_map([limit], run_summary_from_row)?
            .map(|row| {
                row.map_err(WorkflowStoreError::from)
                    .and_then(parse_run_summary)
            })
            .collect()
    }

    /// Return a keyset-paged bounded attempt history for one run.
    ///
    /// # Errors
    ///
    /// Returns an error when `limit` is invalid, the cursor is malformed, or the query fails.
    pub fn attempt_history(
        &self,
        run_id: &str,
        cursor: Option<&AttemptCursor>,
        limit: usize,
    ) -> Result<Vec<AttemptSummary>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let limit = bounded_limit(limit)?;
        if let Some(cursor) = cursor {
            validate_id("cursor.dispatch_identity", &cursor.dispatch_identity)?;
        }
        let mut statement = self.connection.prepare(
            "SELECT run_id, node_id, activation_id, attempt, dispatch_identity, side_effect, \
             status, receipt_json IS NOT NULL, prepared_at_ms, admitted_at_ms, terminal_at_ms \
             FROM workflow_attempts WHERE run_id = ?1 \
               AND (?2 IS NULL OR prepared_at_ms < ?2 \
                    OR (prepared_at_ms = ?2 AND dispatch_identity > ?3)) \
             ORDER BY prepared_at_ms DESC, dispatch_identity LIMIT ?4",
        )?;
        let prepared_at = cursor.map(|cursor| cursor.prepared_at_ms);
        let identity = cursor.map(|cursor| cursor.dispatch_identity.as_str());
        statement
            .query_map(
                (run_id, prepared_at, identity, limit),
                attempt_summary_from_row,
            )?
            .map(|row| {
                row.map_err(WorkflowStoreError::from)
                    .and_then(parse_attempt_summary)
            })
            .collect()
    }

    /// Return keyset-paged workflow events after `after_sequence`.
    ///
    /// # Errors
    ///
    /// Returns an error when `limit` is invalid, JSON is malformed, or the query fails.
    pub fn event_history(
        &self,
        run_id: &str,
        after_sequence: Option<u64>,
        limit: usize,
    ) -> Result<Vec<WorkflowEventRow>, WorkflowStoreError> {
        validate_id("run_id", run_id)?;
        let limit = bounded_limit(limit)?;
        let after = after_sequence.unwrap_or(0);
        let mut statement = self.connection.prepare(
            "SELECT event_seq, run_id, event_type, payload_json, created_at_ms \
             FROM workflow_events WHERE run_id = ?1 AND event_seq > ?2 \
             ORDER BY event_seq LIMIT ?3",
        )?;
        statement
            .query_map((run_id, after, limit), |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, u64>(4)?,
                ))
            })?
            .map(|row| {
                let (event_seq, run_id, event_type, payload_json, created_at_ms) = row?;
                Ok(WorkflowEventRow {
                    event_seq,
                    run_id,
                    event_type,
                    payload: serde_json::from_str(&payload_json)?,
                    created_at_ms,
                })
            })
            .collect()
    }
}

fn persist_validated_output_transaction(
    transaction: &Transaction<'_>,
    output: &ValidatedOutput,
) -> Result<(), WorkflowStoreError> {
    validate_output(output)?;
    let value_json = serde_json::to_string(&output.value)?;
    if value_json.len() > MAX_INLINE_JSON_BYTES {
        return Err(WorkflowStoreError::InvalidData(format!(
            "validated output exceeds {MAX_INLINE_JSON_BYTES} bytes"
        )));
    }
    let checksum = sha256_hex(value_json.as_bytes());
    transaction.execute(
        "INSERT INTO workflow_outputs \
         (output_id, run_id, node_id, activation_id, schema_id, schema_version, value_json, \
          artifact_reference, checksum_sha256, created_at_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        (
            &output.output_id,
            &output.run_id,
            &output.node_id,
            &output.activation_id,
            &output.schema_id,
            output.schema_version,
            &value_json,
            &output.artifact_reference,
            &checksum,
            output.created_at_ms,
        ),
    )?;
    let changed = transaction.execute(
        "UPDATE workflow_activations SET status = 'completed', output_id = ?4 \
         WHERE run_id = ?1 AND node_id = ?2 AND activation_id = ?3 AND status = 'pending'",
        (
            &output.run_id,
            &output.node_id,
            &output.activation_id,
            &output.output_id,
        ),
    )?;
    if changed != 1 {
        return Err(WorkflowStoreError::InvalidData(format!(
            "activation is not pending: {}/{}/{}",
            output.run_id, output.node_id, output.activation_id
        )));
    }
    append_event(
        transaction,
        &output.run_id,
        "output_validated",
        &serde_json::to_string(output)?,
        output.created_at_ms,
    )?;
    Ok(())
}

fn receipt_backed_attempts(
    connection: &Connection,
    limit: i64,
) -> Result<Vec<AttemptReconciliationRequest>, WorkflowStoreError> {
    let mut statement = connection.prepare(
        "SELECT run_id, node_id, activation_id, attempt, dispatch_identity, side_effect, \
         receipt_json FROM workflow_attempts WHERE status IN ('admitted', 'running', 'cancelling') \
         AND receipt_json IS NOT NULL ORDER BY prepared_at_ms, dispatch_identity LIMIT ?1",
    )?;
    statement
        .query_map([limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, u32>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?
        .map(|row| {
            let (
                run_id,
                node_id,
                activation_id,
                attempt,
                dispatch_identity,
                side_effect,
                receipt_json,
            ) = row?;
            Ok(AttemptReconciliationRequest {
                run_id,
                node_id,
                activation_id,
                attempt,
                dispatch_identity,
                side_effect: parse_side_effect(&side_effect)?,
                receipt: serde_json::from_str(&receipt_json)?,
            })
        })
        .collect()
}

fn apply_attempt_observation(
    transaction: &Transaction<'_>,
    request: &AttemptReconciliationRequest,
    observation: AttemptObservation,
    reconciled_at_ms: u64,
    summary: &mut ReceiptReconciliationSummary,
) -> Result<(), WorkflowStoreError> {
    match observation {
        AttemptObservation::Admitted => {
            transition_attempt(transaction, request, "admitted", None)?;
            summary.admitted.push(request.dispatch_identity.clone());
        }
        AttemptObservation::Running => {
            transition_attempt(transaction, request, "running", None)?;
            summary.running.push(request.dispatch_identity.clone());
        }
        AttemptObservation::Succeeded { output } => {
            validate_observed_output(request, &output)?;
            persist_validated_output_transaction(transaction, &output)?;
            transition_attempt(transaction, request, "succeeded", Some(reconciled_at_ms))?;
            summary.succeeded.push(request.dispatch_identity.clone());
        }
        AttemptObservation::Failed { message } => {
            transition_attempt(transaction, request, "failed", Some(reconciled_at_ms))?;
            append_event(
                transaction,
                &request.run_id,
                "attempt_failed",
                &serde_json::json!({
                    "dispatch_identity": request.dispatch_identity,
                    "message": message,
                })
                .to_string(),
                reconciled_at_ms,
            )?;
            summary.failed.push(request.dispatch_identity.clone());
        }
        AttemptObservation::Cancelled => {
            transition_attempt(transaction, request, "cancelled", Some(reconciled_at_ms))?;
            summary.cancelled.push(request.dispatch_identity.clone());
        }
        AttemptObservation::Unknown => {
            if request.side_effect == DispatchSideEffect::Mutating {
                transition_attempt(
                    transaction,
                    request,
                    "repair_required",
                    Some(reconciled_at_ms),
                )?;
                transaction.execute(
                    "UPDATE workflow_runs SET status = ?2, updated_at_ms = ?3 WHERE run_id = ?1",
                    (
                        &request.run_id,
                        RunStatus::RepairRequired.as_str(),
                        reconciled_at_ms,
                    ),
                )?;
                summary
                    .repair_required
                    .push(request.dispatch_identity.clone());
            } else {
                summary
                    .unresolved_read_only
                    .push(request.dispatch_identity.clone());
            }
        }
    }
    Ok(())
}

fn transition_attempt(
    transaction: &Transaction<'_>,
    request: &AttemptReconciliationRequest,
    status: &str,
    terminal_at_ms: Option<u64>,
) -> Result<(), WorkflowStoreError> {
    let changed = transaction.execute(
        "UPDATE workflow_attempts SET status = ?2, terminal_at_ms = COALESCE(?3, terminal_at_ms) \
         WHERE dispatch_identity = ?1 AND status IN ('admitted', 'running', 'cancelling')",
        (&request.dispatch_identity, status, terminal_at_ms),
    )?;
    if changed != 1 {
        return Err(WorkflowStoreError::InvalidData(format!(
            "attempt cannot transition during reconciliation: {}",
            request.dispatch_identity
        )));
    }
    Ok(())
}

fn validate_observed_output(
    request: &AttemptReconciliationRequest,
    output: &ValidatedOutput,
) -> Result<(), WorkflowStoreError> {
    if output.run_id != request.run_id
        || output.node_id != request.node_id
        || output.activation_id != request.activation_id
    {
        return Err(WorkflowStoreError::InvalidData(
            "observed output identity does not match reconciled attempt".to_string(),
        ));
    }
    Ok(())
}

fn enforce_activation_limits(
    connection: &Connection,
    activation: &NewActivation,
) -> Result<(), WorkflowStoreError> {
    let cycle_cap: u64 = connection.query_row(
        "SELECT cycle_cap FROM workflow_runs WHERE run_id = ?1",
        [&activation.run_id],
        |row| row.get(0),
    )?;
    if activation.dependency_generation >= cycle_cap {
        return Err(WorkflowStoreError::InvalidData(
            "workflow cycle cap exceeded".to_string(),
        ));
    }
    Ok(())
}

fn enforce_attempt_limits(
    connection: &Connection,
    attempt: &PreparedAttempt,
) -> Result<(), WorkflowStoreError> {
    let (
        deadline_at_ms,
        node_execution_cap,
        concurrency_cap,
        retry_cap,
        cancellation_requested,
        run_status,
    ): (Option<u64>, u32, u32, u32, bool, String) = connection.query_row(
        "SELECT deadline_at_ms, node_execution_cap, concurrency_cap, retry_cap, \
         cancellation_requested_at_ms IS NOT NULL, status FROM workflow_runs WHERE run_id = ?1",
        [&attempt.run_id],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        },
    )?;
    if run_status != RunStatus::Running.as_str() {
        return Err(WorkflowStoreError::InvalidData(format!(
            "workflow run does not accept attempts while {run_status}"
        )));
    }
    if cancellation_requested {
        return Err(WorkflowStoreError::InvalidData(
            "workflow cancellation has been requested".to_string(),
        ));
    }
    if deadline_at_ms.is_some_and(|deadline| attempt.prepared_at_ms >= deadline) {
        return Err(WorkflowStoreError::InvalidData(
            "workflow wall-clock deadline has elapsed".to_string(),
        ));
    }
    if attempt.attempt > retry_cap.saturating_add(1) {
        return Err(WorkflowStoreError::InvalidData(
            "workflow retry cap exceeded".to_string(),
        ));
    }
    let execution_count: u32 = connection.query_row(
        "SELECT COUNT(*) FROM workflow_attempts WHERE run_id = ?1",
        [&attempt.run_id],
        |row| row.get(0),
    )?;
    if execution_count >= node_execution_cap {
        return Err(WorkflowStoreError::InvalidData(
            "workflow node-execution cap exceeded".to_string(),
        ));
    }
    let active_count: u32 = connection.query_row(
        "SELECT COUNT(*) FROM workflow_attempts WHERE run_id = ?1 \
         AND status IN ('prepared', 'admitted', 'running', 'cancelling')",
        [&attempt.run_id],
        |row| row.get(0),
    )?;
    if active_count >= concurrency_cap {
        return Err(WorkflowStoreError::InvalidData(
            "workflow concurrency cap reached".to_string(),
        ));
    }
    Ok(())
}

fn bounded_limit(limit: usize) -> Result<i64, WorkflowStoreError> {
    if limit == 0 || limit > 1_000 {
        return Err(WorkflowStoreError::InvalidData(
            "history limit must be in 1..=1000".to_string(),
        ));
    }
    Ok(i64::try_from(limit).unwrap_or(1_000))
}

type RawRunSummary = (
    String,
    String,
    u32,
    String,
    Option<String>,
    String,
    Option<u64>,
    u64,
    u64,
);

fn run_summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRunSummary> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
    ))
}

fn parse_run_summary(raw: RawRunSummary) -> Result<WorkflowRunSummary, WorkflowStoreError> {
    let (
        run_id,
        definition_id,
        definition_version,
        workspace_snapshot,
        parent_session_id,
        status,
        cancellation_requested_at_ms,
        created_at_ms,
        updated_at_ms,
    ) = raw;
    Ok(WorkflowRunSummary {
        run_id,
        definition_id,
        definition_version,
        workspace_snapshot,
        parent_session_id,
        status: parse_run_status(&status)?,
        cancellation_requested_at_ms,
        created_at_ms,
        updated_at_ms,
    })
}

fn parse_run_status(status: &str) -> Result<RunStatus, WorkflowStoreError> {
    match status {
        "running" => Ok(RunStatus::Running),
        "paused" => Ok(RunStatus::Paused),
        "completed" => Ok(RunStatus::Completed),
        "failed" => Ok(RunStatus::Failed),
        "cancelled" => Ok(RunStatus::Cancelled),
        "repair_required" => Ok(RunStatus::RepairRequired),
        _ => Err(WorkflowStoreError::InvalidData(format!(
            "unknown workflow run status: {status}"
        ))),
    }
}

type RawAttemptSummary = (
    String,
    String,
    String,
    u32,
    String,
    String,
    String,
    bool,
    u64,
    Option<u64>,
    Option<u64>,
);

fn attempt_summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawAttemptSummary> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
    ))
}

fn parse_side_effect(side_effect: &str) -> Result<DispatchSideEffect, WorkflowStoreError> {
    match side_effect {
        "read_only" => Ok(DispatchSideEffect::ReadOnly),
        "mutating" => Ok(DispatchSideEffect::Mutating),
        _ => Err(WorkflowStoreError::InvalidData(format!(
            "unknown workflow side effect: {side_effect}"
        ))),
    }
}

fn parse_attempt_summary(raw: RawAttemptSummary) -> Result<AttemptSummary, WorkflowStoreError> {
    let (
        run_id,
        node_id,
        activation_id,
        attempt,
        dispatch_identity,
        side_effect,
        status,
        has_receipt,
        prepared_at_ms,
        admitted_at_ms,
        terminal_at_ms,
    ) = raw;
    let side_effect = parse_side_effect(&side_effect)?;
    Ok(AttemptSummary {
        run_id,
        node_id,
        activation_id,
        attempt,
        dispatch_identity,
        side_effect,
        status,
        has_receipt,
        prepared_at_ms,
        admitted_at_ms,
        terminal_at_ms,
    })
}

/// Return the stable idempotency identity for one durable attempt.
#[must_use]
pub fn dispatch_identity(run_id: &str, node_id: &str, activation_id: &str, attempt: u32) -> String {
    sha256_hex(format!("{run_id}\0{node_id}\0{activation_id}\0{attempt}").as_bytes())
}

/// Return the canonical workflow database path under one Bcode state directory.
#[must_use]
pub fn workflow_database_path(state_dir: &Path) -> PathBuf {
    state_dir.join("workflows").join(DATABASE_FILE)
}

fn append_event(
    transaction: &Transaction<'_>,
    run_id: &str,
    event_type: &str,
    payload_json: &str,
    created_at_ms: u64,
) -> Result<(), WorkflowStoreError> {
    transaction.execute(
        "INSERT INTO workflow_events (run_id, event_type, payload_json, created_at_ms) \
         VALUES (?1, ?2, ?3, ?4)",
        (run_id, event_type, payload_json, created_at_ms),
    )?;
    Ok(())
}

type RepairRequiredAttempt = (String, String, String, u32, String);

fn repair_required_attempt(
    transaction: &Transaction<'_>,
    dispatch_identity: &str,
) -> Result<RepairRequiredAttempt, WorkflowStoreError> {
    transaction
        .query_row(
            "SELECT run_id, node_id, activation_id, attempt, side_effect \
             FROM workflow_attempts WHERE dispatch_identity = ?1 AND status = 'repair_required'",
            [dispatch_identity],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!(
                "repair-required workflow attempt not found: {dispatch_identity}"
            ))
        })
}

fn require_cancellation_requested(
    connection: &Connection,
    run_id: &str,
) -> Result<(), WorkflowStoreError> {
    let cancellation_requested = connection
        .query_row(
            "SELECT cancellation_requested_at_ms IS NOT NULL FROM workflow_runs WHERE run_id = ?1",
            [run_id],
            |row| row.get::<_, bool>(0),
        )
        .optional()?
        .ok_or_else(|| {
            WorkflowStoreError::InvalidData(format!("workflow run not found: {run_id}"))
        })?;
    if !cancellation_requested {
        return Err(WorkflowStoreError::InvalidData(
            "workflow cancellation intent must be persisted before signaling children".to_string(),
        ));
    }
    Ok(())
}

fn validate_repair_resolution(resolution: &RepairResolution) -> Result<(), WorkflowStoreError> {
    match resolution {
        RepairResolution::ConfirmSucceeded { output } => validate_output(output),
        RepairResolution::ConfirmFailed { message }
        | RepairResolution::ConfirmCancelled { message } => {
            validate_bounded_message("repair message", message)
        }
        RepairResolution::AbandonForExplicitRetry { reason } => {
            validate_bounded_message("repair reason", reason)
        }
    }
}

fn bounded_json<T: Serialize>(label: &str, value: &T) -> Result<String, WorkflowStoreError> {
    let json = serde_json::to_string(value)?;
    if json.len() > MAX_INLINE_JSON_BYTES {
        return Err(WorkflowStoreError::InvalidData(format!(
            "{label} exceeds {MAX_INLINE_JSON_BYTES} bytes"
        )));
    }
    Ok(json)
}

fn resource_lease(
    transaction: &Transaction<'_>,
    lease_id: &str,
) -> Result<Option<(WorkflowResourceLease, Option<u64>)>, WorkflowStoreError> {
    transaction
        .query_row(
            "SELECT run_id, node_id, activation_id, resource_key, mode, acquired_at_ms, expires_at_ms, \
             released_at_ms FROM workflow_resource_leases WHERE lease_id = ?1",
            [lease_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, u64>(5)?,
                    row.get::<_, Option<u64>>(6)?,
                    row.get::<_, Option<u64>>(7)?,
                ))
            },
        )
        .optional()?
        .map(
            |(
                run_id,
                node_id,
                activation_id,
                resource_key,
                mode,
                acquired_at_ms,
                expires_at_ms,
                released_at_ms,
            )| {
                Ok((
                    WorkflowResourceLease {
                        lease_id: lease_id.to_string(),
                        run_id,
                        node_id,
                        activation_id,
                        resource_key,
                        mode: parse_resource_lease_mode(&mode)?,
                        acquired_at_ms,
                        expires_at_ms,
                    },
                    released_at_ms,
                ))
            },
        )
        .transpose()
}

fn parse_resource_lease_mode(mode: &str) -> Result<ResourceLeaseMode, WorkflowStoreError> {
    match mode {
        "read" => Ok(ResourceLeaseMode::Read),
        "write" => Ok(ResourceLeaseMode::Write),
        _ => Err(WorkflowStoreError::InvalidData(format!(
            "unknown workflow resource lease mode: {mode}"
        ))),
    }
}

fn validate_decision(decision: &WorkflowDecision) -> Result<(), WorkflowStoreError> {
    validate_id("decision_id", &decision.decision_id)?;
    validate_id("run_id", &decision.run_id)?;
    validate_id("decision_type", &decision.decision_type)?;
    if let Some(node_id) = &decision.node_id {
        validate_id("node_id", node_id)?;
    }
    Ok(())
}

fn validate_grant(grant: &WorkflowGrant) -> Result<(), WorkflowStoreError> {
    validate_id("grant_id", &grant.grant_id)?;
    validate_id("run_id", &grant.run_id)?;
    validate_id("node_id", &grant.node_id)?;
    if grant
        .expires_at_ms
        .is_some_and(|expires| expires <= grant.granted_at_ms)
    {
        return Err(WorkflowStoreError::InvalidData(
            "workflow grant expiry must follow grant time".to_string(),
        ));
    }
    Ok(())
}

fn validate_resource_lease(lease: &WorkflowResourceLease) -> Result<(), WorkflowStoreError> {
    for (label, value) in [
        ("lease_id", lease.lease_id.as_str()),
        ("run_id", lease.run_id.as_str()),
        ("node_id", lease.node_id.as_str()),
        ("activation_id", lease.activation_id.as_str()),
        ("resource_key", lease.resource_key.as_str()),
    ] {
        validate_id(label, value)?;
    }
    if lease
        .expires_at_ms
        .is_some_and(|expires| expires <= lease.acquired_at_ms)
    {
        return Err(WorkflowStoreError::InvalidData(
            "workflow resource lease expiry must follow acquisition".to_string(),
        ));
    }
    Ok(())
}

fn validate_projection_checkpoint(
    checkpoint: &WorkflowProjectionCheckpoint,
) -> Result<(), WorkflowStoreError> {
    validate_id("projection_name", &checkpoint.projection_name)?;
    if checkpoint.projection_version == 0 {
        return Err(WorkflowStoreError::InvalidData(
            "workflow projection version must be positive".to_string(),
        ));
    }
    Ok(())
}

fn validate_bounded_message(label: &str, value: &str) -> Result<(), WorkflowStoreError> {
    if value.trim().is_empty() || value.len() > MAX_INLINE_JSON_BYTES {
        return Err(WorkflowStoreError::InvalidData(format!(
            "{label} must contain 1..={MAX_INLINE_JSON_BYTES} bytes"
        )));
    }
    Ok(())
}

fn validate_run(run: &NewWorkflowRun) -> Result<(), WorkflowStoreError> {
    validate_id("run_id", &run.run_id)?;
    validate_id("definition_id", &run.definition_id)?;
    validate_id("workspace_snapshot", &run.workspace_snapshot)?;
    validate_run_limits(&run.limits)?;
    if run.definition_version == 0 {
        return Err(WorkflowStoreError::InvalidData(
            "definition version must be positive".to_string(),
        ));
    }
    if let Some(parent_session_id) = &run.parent_session_id {
        validate_id("parent_session_id", parent_session_id)?;
    }
    Ok(())
}

fn validate_run_limits(limits: &WorkflowRunLimits) -> Result<(), WorkflowStoreError> {
    for (label, value) in [
        ("node_execution_cap", limits.node_execution_cap),
        ("concurrency_cap", limits.concurrency_cap),
        ("cycle_cap", limits.cycle_cap),
    ] {
        if value == 0 {
            return Err(WorkflowStoreError::InvalidData(format!(
                "{label} must be positive"
            )));
        }
    }
    Ok(())
}

fn validate_activation(activation: &NewActivation) -> Result<(), WorkflowStoreError> {
    validate_id("run_id", &activation.run_id)?;
    validate_id("node_id", &activation.node_id)?;
    validate_id("activation_id", &activation.activation_id)
}

fn validate_prepared_attempt(attempt: &PreparedAttempt) -> Result<(), WorkflowStoreError> {
    validate_id("run_id", &attempt.run_id)?;
    validate_id("node_id", &attempt.node_id)?;
    validate_id("activation_id", &attempt.activation_id)?;
    if attempt.attempt == 0 {
        return Err(WorkflowStoreError::InvalidData(
            "attempt must be positive".to_string(),
        ));
    }
    Ok(())
}

fn validate_dispatch_receipt(receipt: &DispatchReceipt) -> Result<(), WorkflowStoreError> {
    for (label, value) in [
        ("run_id", receipt.run_id.as_str()),
        ("node_id", receipt.node_id.as_str()),
        ("activation_id", receipt.activation_id.as_str()),
        ("dispatch_identity", receipt.dispatch_identity.as_str()),
    ] {
        validate_id(label, value)?;
    }
    if receipt.attempt == 0 {
        return Err(WorkflowStoreError::InvalidData(
            "attempt must be positive".to_string(),
        ));
    }
    Ok(())
}

fn validate_output(output: &ValidatedOutput) -> Result<(), WorkflowStoreError> {
    for (label, value) in [
        ("output_id", output.output_id.as_str()),
        ("run_id", output.run_id.as_str()),
        ("node_id", output.node_id.as_str()),
        ("activation_id", output.activation_id.as_str()),
        ("schema_id", output.schema_id.as_str()),
    ] {
        validate_id(label, value)?;
    }
    if output.schema_version == 0 {
        return Err(WorkflowStoreError::InvalidData(
            "output schema version must be positive".to_string(),
        ));
    }
    if let Some(reference) = &output.artifact_reference {
        validate_id("artifact_reference", reference)?;
    }
    Ok(())
}

fn persist_definition_transaction(
    transaction: &Transaction<'_>,
    stored: &StoredWorkflowDefinition,
) -> Result<(), WorkflowStoreError> {
    let existing = transaction
        .query_row(
            "SELECT checksum_sha256 FROM workflow_definitions \
             WHERE definition_id = ?1 AND version = ?2",
            (&stored.definition_id, stored.version),
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        if existing == stored.checksum_sha256 {
            return Ok(());
        }
        return Err(WorkflowStoreError::InvalidData(format!(
            "definition identity conflict: {} v{}",
            stored.definition_id, stored.version
        )));
    }
    transaction.execute(
        "INSERT INTO workflow_definitions \
         (definition_id, version, checksum_sha256, definition_json) VALUES (?1, ?2, ?3, ?4)",
        (
            &stored.definition_id,
            stored.version,
            &stored.checksum_sha256,
            &stored.definition_json,
        ),
    )?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn migrate(connection: &mut Connection) -> Result<(), WorkflowStoreError> {
    let transaction = connection.transaction()?;
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS workflow_store_contract (\
             contract_id INTEGER PRIMARY KEY CHECK (contract_id = 1),\
             schema_version INTEGER NOT NULL\
         );\
         INSERT OR IGNORE INTO workflow_store_contract (contract_id, schema_version) VALUES (1, 3);\
         UPDATE workflow_store_contract SET schema_version = 2 WHERE contract_id = 1 AND schema_version = 1;\
         UPDATE workflow_store_contract SET schema_version = 3 WHERE contract_id = 1 AND schema_version = 2;\
         CREATE TABLE IF NOT EXISTS workflow_definitions (\
             definition_id TEXT NOT NULL,\
             version INTEGER NOT NULL CHECK (version > 0),\
             checksum_sha256 TEXT NOT NULL,\
             definition_json TEXT NOT NULL,\
             PRIMARY KEY (definition_id, version)\
         );\
         CREATE TABLE IF NOT EXISTS workflow_runs (\
             run_id TEXT PRIMARY KEY NOT NULL,\
             definition_id TEXT NOT NULL,\
             definition_version INTEGER NOT NULL,\
             workspace_snapshot TEXT NOT NULL,\
             parent_session_id TEXT,\
             status TEXT NOT NULL,\
             cancellation_requested_at_ms INTEGER,\
             deadline_at_ms INTEGER,\
             node_execution_cap INTEGER NOT NULL,\
             concurrency_cap INTEGER NOT NULL,\
             cycle_cap INTEGER NOT NULL,\
             retry_cap INTEGER NOT NULL,\
             created_at_ms INTEGER NOT NULL,\
             updated_at_ms INTEGER NOT NULL,\
             FOREIGN KEY (definition_id, definition_version)\
                 REFERENCES workflow_definitions(definition_id, version)\
         );\
         CREATE TABLE IF NOT EXISTS workflow_activations (\
             run_id TEXT NOT NULL,\
             node_id TEXT NOT NULL,\
             activation_id TEXT NOT NULL,\
             dependency_generation INTEGER NOT NULL,\
             status TEXT NOT NULL,\
             output_id TEXT,\
             created_at_ms INTEGER NOT NULL,\
             PRIMARY KEY (run_id, node_id, activation_id),\
             FOREIGN KEY (run_id) REFERENCES workflow_runs(run_id)\
         );\
         CREATE TABLE IF NOT EXISTS workflow_attempts (\
             run_id TEXT NOT NULL,\
             node_id TEXT NOT NULL,\
             activation_id TEXT NOT NULL,\
             attempt INTEGER NOT NULL CHECK (attempt > 0),\
             dispatch_identity TEXT NOT NULL UNIQUE,\
             side_effect TEXT NOT NULL,\
             status TEXT NOT NULL,\
             intent_json TEXT NOT NULL,\
             intent_checksum TEXT NOT NULL,\
             receipt_json TEXT,\
             prepared_at_ms INTEGER NOT NULL,\
             admitted_at_ms INTEGER,\
             terminal_at_ms INTEGER,\
             PRIMARY KEY (run_id, node_id, activation_id, attempt),\
             FOREIGN KEY (run_id, node_id, activation_id)\
                 REFERENCES workflow_activations(run_id, node_id, activation_id)\
         );\
         CREATE TABLE IF NOT EXISTS workflow_outputs (\
             output_id TEXT PRIMARY KEY NOT NULL,\
             run_id TEXT NOT NULL,\
             node_id TEXT NOT NULL,\
             activation_id TEXT NOT NULL,\
             schema_id TEXT NOT NULL,\
             schema_version INTEGER NOT NULL CHECK (schema_version > 0),\
             value_json TEXT NOT NULL,\
             artifact_reference TEXT,\
             checksum_sha256 TEXT NOT NULL,\
             created_at_ms INTEGER NOT NULL,\
             FOREIGN KEY (run_id, node_id, activation_id)\
                 REFERENCES workflow_activations(run_id, node_id, activation_id)\
         );\
         CREATE TABLE IF NOT EXISTS workflow_decisions (\
             decision_id TEXT PRIMARY KEY NOT NULL,\
             run_id TEXT NOT NULL,\
             node_id TEXT,\
             decision_type TEXT NOT NULL,\
             value_json TEXT NOT NULL,\
             created_at_ms INTEGER NOT NULL,\
             FOREIGN KEY (run_id) REFERENCES workflow_runs(run_id)\
         );\
         CREATE TABLE IF NOT EXISTS workflow_grants (\
             grant_id TEXT PRIMARY KEY NOT NULL,\
             run_id TEXT NOT NULL,\
             node_id TEXT NOT NULL,\
             scope_json TEXT NOT NULL,\
             granted_at_ms INTEGER NOT NULL,\
             expires_at_ms INTEGER,\
             FOREIGN KEY (run_id) REFERENCES workflow_runs(run_id)\
         );\
         CREATE TABLE IF NOT EXISTS workflow_resource_leases (\
             lease_id TEXT PRIMARY KEY NOT NULL,\
             run_id TEXT NOT NULL,\
             node_id TEXT NOT NULL,\
             activation_id TEXT NOT NULL,\
             resource_key TEXT NOT NULL,\
             mode TEXT NOT NULL CHECK (mode IN ('read', 'write')),\
             acquired_at_ms INTEGER NOT NULL,\
             expires_at_ms INTEGER,\
             released_at_ms INTEGER,\
             FOREIGN KEY (run_id, node_id, activation_id)\
                 REFERENCES workflow_activations(run_id, node_id, activation_id)\
         );\
         CREATE INDEX IF NOT EXISTS idx_workflow_resource_leases_active \
             ON workflow_resource_leases(run_id, resource_key, released_at_ms);\
         CREATE TABLE IF NOT EXISTS workflow_events (\
             event_seq INTEGER PRIMARY KEY AUTOINCREMENT,\
             run_id TEXT NOT NULL,\
             event_type TEXT NOT NULL,\
             payload_json TEXT NOT NULL,\
             created_at_ms INTEGER NOT NULL,\
             FOREIGN KEY (run_id) REFERENCES workflow_runs(run_id)\
         );\
         CREATE INDEX IF NOT EXISTS idx_workflow_events_run_seq \
             ON workflow_events(run_id, event_seq);\
         CREATE TABLE IF NOT EXISTS workflow_projection_checkpoints (\
             projection_name TEXT PRIMARY KEY NOT NULL,\
             projection_version INTEGER NOT NULL,\
             last_event_seq INTEGER NOT NULL\
         );",
    )?;
    let actual: u32 = transaction.query_row(
        "SELECT schema_version FROM workflow_store_contract WHERE contract_id = 1",
        [],
        |row| row.get(0),
    )?;
    if actual != SCHEMA_VERSION {
        return Err(WorkflowStoreError::InvalidData(format!(
            "unsupported workflow schema version {actual}; expected {SCHEMA_VERSION}"
        )));
    }
    transaction.commit()?;
    Ok(())
}

fn validate_id(label: &str, value: &str) -> Result<(), WorkflowStoreError> {
    if value.trim().is_empty() || value.len() > MAX_ID_BYTES {
        return Err(WorkflowStoreError::InvalidData(format!(
            "{label} must contain 1..={MAX_ID_BYTES} bytes"
        )));
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().fold(
        String::with_capacity(digest.len() * 2),
        |mut encoded, byte| {
            write!(encoded, "{byte:02x}").expect("writing to a string cannot fail");
            encoded
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_workflow::{Step, WorkflowBuilder};

    fn definition(name: &str) -> WorkflowDefinition {
        WorkflowBuilder::new(
            name,
            Step::task(
                "increment",
                |value: u32, _context| async move { Ok(value + 1) },
            ),
        )
        .build()
        .expect("workflow")
        .definition()
        .clone()
    }

    fn new_run() -> NewWorkflowRun {
        NewWorkflowRun {
            run_id: "run-1".to_string(),
            definition_id: "example".to_string(),
            definition_version: 1,
            workspace_snapshot: "snapshot-1".to_string(),
            parent_session_id: Some("session-1".to_string()),
            created_at_ms: 10,
            limits: WorkflowRunLimits::default(),
        }
    }

    fn new_activation() -> NewActivation {
        NewActivation {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: "activation-1".to_string(),
            dependency_generation: 0,
            created_at_ms: 11,
        }
    }

    fn initialized_store() -> (tempfile::TempDir, WorkflowStore) {
        let temp = tempfile::tempdir().expect("temp");
        let store = initialized_store_at(temp.path());
        (temp, store)
    }

    fn initialized_store_at(state_dir: &Path) -> WorkflowStore {
        let mut store = WorkflowStore::open_in_state_dir(state_dir).expect("store");
        store
            .persist_definition("example", 1, &definition("example"))
            .expect("definition");
        store.create_run(&new_run()).expect("run");
        store
            .create_activation(&new_activation())
            .expect("activation");
        store
    }

    fn prepare_receipt_backed_attempt(
        store: &mut WorkflowStore,
        side_effect: DispatchSideEffect,
    ) -> String {
        let attempt = PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: "activation-1".to_string(),
            attempt: 1,
            side_effect,
            intent: serde_json::json!({"operation": "review"}),
            prepared_at_ms: 12,
        };
        let identity = store.prepare_attempt(&attempt).expect("prepare");
        store
            .persist_dispatch_receipt(&DispatchReceipt {
                run_id: attempt.run_id,
                node_id: attempt.node_id,
                activation_id: attempt.activation_id,
                attempt: 1,
                dispatch_identity: identity.clone(),
                receipt: serde_json::json!({"turn_id": "turn-1"}),
                admitted_at_ms: 13,
            })
            .expect("receipt");
        identity
    }

    #[test]
    fn prepared_attempt_identity_is_stable_and_conflicting_intent_fails_closed() {
        let (_temp, mut store) = initialized_store();
        let attempt = PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: "activation-1".to_string(),
            attempt: 1,
            side_effect: DispatchSideEffect::Mutating,
            intent: serde_json::json!({"operation": "apply"}),
            prepared_at_ms: 12,
        };
        let first = store.prepare_attempt(&attempt).expect("prepare");
        let second = store.prepare_attempt(&attempt).expect("idempotent");
        assert_eq!(first, second);
        assert_eq!(
            first,
            dispatch_identity("run-1", "review", "activation-1", 1)
        );
        let error = store
            .prepare_attempt(&PreparedAttempt {
                intent: serde_json::json!({"operation": "different"}),
                ..attempt
            })
            .expect_err("conflicting intent");
        assert!(error.to_string().contains("identity conflict"));
    }

    #[test]
    fn dispatch_receipt_requires_exact_prepared_identity() {
        let (_temp, mut store) = initialized_store();
        let attempt = PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: "activation-1".to_string(),
            attempt: 1,
            side_effect: DispatchSideEffect::ReadOnly,
            intent: serde_json::json!({"operation": "review"}),
            prepared_at_ms: 12,
        };
        let identity = store.prepare_attempt(&attempt).expect("prepare");
        store
            .persist_dispatch_receipt(&DispatchReceipt {
                run_id: attempt.run_id,
                node_id: attempt.node_id,
                activation_id: attempt.activation_id,
                attempt: 1,
                dispatch_identity: identity,
                receipt: serde_json::json!({"turn_id": "turn-1"}),
                admitted_at_ms: 13,
            })
            .expect("receipt");
        let status: String = store
            .connection
            .query_row(
                "SELECT status FROM workflow_attempts WHERE run_id = 'run-1'",
                [],
                |row| row.get(0),
            )
            .expect("status");
        assert_eq!(status, "admitted");
    }

    #[test]
    fn restart_reconciliation_never_retries_ambiguous_mutation() {
        let (_temp, mut store) = initialized_store();
        let mutating = PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: "activation-1".to_string(),
            attempt: 1,
            side_effect: DispatchSideEffect::Mutating,
            intent: serde_json::json!({"operation": "apply"}),
            prepared_at_ms: 12,
        };
        let identity = store.prepare_attempt(&mutating).expect("prepare");

        let summary = store
            .reconcile_prepared_attempts(10, 20)
            .expect("reconcile");

        assert_eq!(summary.repair_required, [identity]);
        assert!(summary.safe_prepared.is_empty());
        let run_status: String = store
            .connection
            .query_row(
                "SELECT status FROM workflow_runs WHERE run_id = 'run-1'",
                [],
                |row| row.get(0),
            )
            .expect("run status");
        assert_eq!(run_status, "repair_required");
    }

    #[test]
    fn normal_status_and_history_reads_are_bounded_and_projection_backed() {
        let (_temp, mut store) = initialized_store();
        let attempt = PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: "activation-1".to_string(),
            attempt: 1,
            side_effect: DispatchSideEffect::ReadOnly,
            intent: serde_json::json!({"operation": "review"}),
            prepared_at_ms: 12,
        };
        let identity = store.prepare_attempt(&attempt).expect("prepare");

        let summary = store.run_summary("run-1").expect("summary").expect("run");
        assert_eq!(summary.status, RunStatus::Running);
        assert_eq!(store.list_runs(10).expect("runs"), [summary]);
        let attempts = store.attempt_history("run-1", None, 10).expect("attempts");
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].dispatch_identity, identity);
        assert!(!attempts[0].has_receipt);
        let events = store.event_history("run-1", None, 10).expect("events");
        assert_eq!(
            events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            ["run_created", "activation_created", "attempt_prepared"]
        );
        let page = store
            .event_history("run-1", Some(events[0].event_seq), 1)
            .expect("paged events");
        assert_eq!(page[0].event_type, "activation_created");
        assert!(store.list_runs(0).is_err());
        assert!(store.event_history("run-1", None, 1_001).is_err());
    }

    #[test]
    fn receipt_backed_restart_reconciliation_persists_terminal_success() {
        struct Observer;
        impl AttemptStatusObserver for Observer {
            fn observe(
                &self,
                request: &AttemptReconciliationRequest,
            ) -> Result<AttemptObservation, WorkflowStoreError> {
                Ok(AttemptObservation::Succeeded {
                    output: ValidatedOutput {
                        output_id: "output-1".to_string(),
                        run_id: request.run_id.clone(),
                        node_id: request.node_id.clone(),
                        activation_id: request.activation_id.clone(),
                        schema_id: "review/v1".to_string(),
                        schema_version: 1,
                        value: serde_json::json!({"approved": true}),
                        artifact_reference: None,
                        created_at_ms: 20,
                    },
                })
            }
        }

        let (_temp, mut store) = initialized_store();
        let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::ReadOnly);
        let summary = store
            .reconcile_receipt_backed_attempts(&Observer, 10, 20)
            .expect("reconcile");
        assert_eq!(summary.succeeded, [identity]);
        let attempt = store
            .attempt_history("run-1", None, 10)
            .expect("attempt")
            .pop()
            .expect("row");
        assert_eq!(attempt.status, "succeeded");
        let activation_status: String = store
            .connection
            .query_row(
                "SELECT status FROM workflow_activations WHERE run_id = 'run-1'",
                [],
                |row| row.get(0),
            )
            .expect("activation");
        assert_eq!(activation_status, "completed");
    }

    #[test]
    fn unknown_receipt_backed_mutation_becomes_repair_required() {
        struct Observer;
        impl AttemptStatusObserver for Observer {
            fn observe(
                &self,
                _request: &AttemptReconciliationRequest,
            ) -> Result<AttemptObservation, WorkflowStoreError> {
                Ok(AttemptObservation::Unknown)
            }
        }

        let (_temp, mut store) = initialized_store();
        let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::Mutating);
        let summary = store
            .reconcile_receipt_backed_attempts(&Observer, 10, 20)
            .expect("reconcile");
        assert_eq!(summary.repair_required, [identity]);
        assert_eq!(
            store
                .run_summary("run-1")
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::RepairRequired
        );
    }

    #[test]
    fn cancellation_intent_is_persisted_before_further_admission() {
        let (_temp, mut store) = initialized_store();
        assert!(store.request_cancellation("run-1", 20).expect("first"));
        assert!(!store.request_cancellation("run-1", 21).expect("idempotent"));
        let summary = store.run_summary("run-1").expect("summary").expect("run");
        assert_eq!(summary.cancellation_requested_at_ms, Some(20));
        let error = store
            .prepare_attempt(&PreparedAttempt {
                run_id: "run-1".to_string(),
                node_id: "review".to_string(),
                activation_id: "activation-1".to_string(),
                attempt: 1,
                side_effect: DispatchSideEffect::ReadOnly,
                intent: serde_json::json!({}),
                prepared_at_ms: 22,
            })
            .expect_err("cancelled run rejects admission");
        assert!(error.to_string().contains("cancellation"));
        assert_eq!(
            store
                .active_attempts_for_cancellation("run-1", 10)
                .expect("active children"),
            Vec::<String>::new()
        );
    }

    #[tokio::test]
    async fn cancellation_propagates_to_receipt_backed_owner_after_durable_intent() {
        #[derive(Default)]
        struct Owner {
            requests: std::sync::Mutex<Vec<ActiveAttemptCancellation>>,
        }

        impl AttemptCancellationOwner for Owner {
            fn cancel_attempt<'a>(
                &'a self,
                request: &'a ActiveAttemptCancellation,
            ) -> Pin<Box<dyn Future<Output = Result<(), WorkflowStoreError>> + Send + 'a>>
            {
                Box::pin(async move {
                    self.requests
                        .lock()
                        .expect("owner requests")
                        .push(request.clone());
                    Ok(())
                })
            }
        }

        let (_temp, mut store) = initialized_store();
        let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::ReadOnly);
        let owner = Owner::default();
        assert!(
            store
                .propagate_cancellation("run-1", &owner, 10, 20)
                .await
                .is_err(),
            "owner signaling must be impossible before durable cancellation intent"
        );
        assert!(owner.requests.lock().expect("requests").is_empty());

        store.request_cancellation("run-1", 20).expect("intent");
        let summary = store
            .propagate_cancellation("run-1", &owner, 10, 21)
            .await
            .expect("propagate");
        assert_eq!(
            summary.signalled.as_slice(),
            std::slice::from_ref(&identity)
        );
        let requests = owner.requests.lock().expect("requests").clone();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].dispatch_identity, identity);
        assert_eq!(
            requests[0].receipt,
            Some(serde_json::json!({"turn_id": "turn-1"}))
        );
        let repeated = store
            .propagate_cancellation("run-1", &owner, 10, 22)
            .await
            .expect("already signalled");
        assert!(repeated.signalled.is_empty());
        assert_eq!(owner.requests.lock().expect("requests").len(), 1);
        assert_eq!(
            store.attempt_history("run-1", None, 10).expect("attempts")[0].status,
            "cancelling"
        );
    }

    #[tokio::test]
    async fn cancellation_propagation_is_retryable_after_owner_failure() {
        struct Owner;
        impl AttemptCancellationOwner for Owner {
            fn cancel_attempt<'a>(
                &'a self,
                _request: &'a ActiveAttemptCancellation,
            ) -> Pin<Box<dyn Future<Output = Result<(), WorkflowStoreError>> + Send + 'a>>
            {
                Box::pin(async {
                    Err(WorkflowStoreError::InvalidData(
                        "owner unavailable".to_string(),
                    ))
                })
            }
        }

        let (_temp, mut store) = initialized_store();
        let identity = prepare_receipt_backed_attempt(&mut store, DispatchSideEffect::ReadOnly);
        store.request_cancellation("run-1", 20).expect("intent");
        assert!(
            store
                .propagate_cancellation("run-1", &Owner, 10, 21)
                .await
                .is_err()
        );
        assert_eq!(
            store
                .active_attempts_for_cancellation("run-1", 10)
                .expect("retryable"),
            [identity]
        );
    }

    #[test]
    fn active_children_are_exposed_only_after_cancellation_is_durable() {
        let (_temp, mut store) = initialized_store();
        let identity = store
            .prepare_attempt(&PreparedAttempt {
                run_id: "run-1".to_string(),
                node_id: "review".to_string(),
                activation_id: "activation-1".to_string(),
                attempt: 1,
                side_effect: DispatchSideEffect::ReadOnly,
                intent: serde_json::json!({}),
                prepared_at_ms: 12,
            })
            .expect("attempt");
        assert!(store.active_attempts_for_cancellation("run-1", 10).is_err());
        store.request_cancellation("run-1", 20).expect("intent");
        assert_eq!(
            store
                .active_attempts_for_cancellation("run-1", 10)
                .expect("children"),
            [identity]
        );
    }

    #[test]
    fn persisted_limits_fail_closed_at_activation_and_attempt_admission() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("example", 1, &definition("example"))
            .expect("definition");
        let mut run = new_run();
        run.limits = WorkflowRunLimits {
            deadline_at_ms: Some(20),
            node_execution_cap: 1,
            concurrency_cap: 1,
            cycle_cap: 1,
            retry_cap: 0,
        };
        store.create_run(&run).expect("run");
        store
            .create_activation(&new_activation())
            .expect("activation");
        let attempt = PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: "activation-1".to_string(),
            attempt: 1,
            side_effect: DispatchSideEffect::ReadOnly,
            intent: serde_json::json!({}),
            prepared_at_ms: 12,
        };
        store.prepare_attempt(&attempt).expect("first attempt");
        assert!(
            store
                .prepare_attempt(&PreparedAttempt {
                    activation_id: "activation-2".to_string(),
                    ..attempt.clone()
                })
                .is_err()
        );
        assert!(
            store
                .create_activation(&NewActivation {
                    activation_id: "cycle-overflow".to_string(),
                    dependency_generation: 1,
                    ..new_activation()
                })
                .is_err()
        );
        let (_temp, mut deadline_store) = initialized_store();
        deadline_store
            .connection
            .execute(
                "UPDATE workflow_runs SET deadline_at_ms = 12 WHERE run_id = 'run-1'",
                [],
            )
            .expect("deadline");
        assert!(deadline_store.prepare_attempt(&attempt).is_err());
    }

    #[test]
    fn doctor_is_bounded_non_mutating_and_reports_corruption() {
        let (_temp, mut store) = initialized_store();
        let identity = store
            .prepare_attempt(&PreparedAttempt {
                run_id: "run-1".to_string(),
                node_id: "review".to_string(),
                activation_id: "activation-1".to_string(),
                attempt: 1,
                side_effect: DispatchSideEffect::Mutating,
                intent: serde_json::json!({"operation": "apply"}),
                prepared_at_ms: 12,
            })
            .expect("prepare");
        store
            .connection
            .execute(
                "UPDATE workflow_attempts SET dispatch_identity = 'corrupt', status = 'repair_required' \
                 WHERE dispatch_identity = ?1",
                [identity],
            )
            .expect("corrupt identity");
        store
            .connection
            .execute(
                "UPDATE workflow_activations SET status = 'completed' WHERE run_id = 'run-1'",
                [],
            )
            .expect("corrupt activation");
        let event_count_before: u64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM workflow_events", [], |row| row.get(0))
            .expect("events");

        let first = store.doctor_run("run-1", 1).expect("doctor");
        assert!(first.truncated);
        assert_eq!(first.issues.len(), 1);
        let full = store.doctor_run("run-1", 10).expect("doctor");
        assert_eq!(full.issues.len(), 3);
        assert!(
            full.issues
                .iter()
                .any(|issue| matches!(issue, WorkflowDoctorIssue::RepairStatusMismatch { .. }))
        );
        assert!(
            full.issues
                .iter()
                .any(|issue| matches!(issue, WorkflowDoctorIssue::ActivationOutputMismatch { .. }))
        );
        assert!(
            full.issues
                .iter()
                .any(|issue| matches!(issue, WorkflowDoctorIssue::AttemptIdentityMismatch { .. }))
        );
        let event_count_after: u64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM workflow_events", [], |row| row.get(0))
            .expect("events");
        assert_eq!(event_count_before, event_count_after);
    }

    #[test]
    fn explicit_repair_requires_proof_and_never_dispatches() {
        let (_temp, mut store) = initialized_store();
        let identity = store
            .prepare_attempt(&PreparedAttempt {
                run_id: "run-1".to_string(),
                node_id: "review".to_string(),
                activation_id: "activation-1".to_string(),
                attempt: 1,
                side_effect: DispatchSideEffect::Mutating,
                intent: serde_json::json!({"operation": "apply"}),
                prepared_at_ms: 12,
            })
            .expect("prepare");
        store
            .reconcile_prepared_attempts(10, 20)
            .expect("repair required");
        let error = store
            .repair_attempt(
                &identity,
                &RepairResolution::ConfirmSucceeded {
                    output: ValidatedOutput {
                        output_id: "output-1".to_string(),
                        run_id: "wrong-run".to_string(),
                        node_id: "review".to_string(),
                        activation_id: "activation-1".to_string(),
                        schema_id: "review/v1".to_string(),
                        schema_version: 1,
                        value: serde_json::json!({"approved": true}),
                        artifact_reference: None,
                        created_at_ms: 21,
                    },
                },
                21,
            )
            .expect_err("mismatched proof");
        assert!(error.to_string().contains("does not match"));

        let result = store
            .repair_attempt(
                &identity,
                &RepairResolution::ConfirmSucceeded {
                    output: ValidatedOutput {
                        output_id: "output-1".to_string(),
                        run_id: "run-1".to_string(),
                        node_id: "review".to_string(),
                        activation_id: "activation-1".to_string(),
                        schema_id: "review/v1".to_string(),
                        schema_version: 1,
                        value: serde_json::json!({"approved": true}),
                        artifact_reference: None,
                        created_at_ms: 21,
                    },
                },
                21,
            )
            .expect("repair");
        assert_eq!(result.attempt_status, "succeeded");
        assert_eq!(result.run_status, RunStatus::Paused);
        assert_eq!(
            store
                .run_summary("run-1")
                .expect("summary")
                .expect("run")
                .status,
            RunStatus::Paused
        );
        assert!(
            store
                .doctor_run("run-1", 10)
                .expect("doctor")
                .issues
                .is_empty()
        );
    }

    #[test]
    fn explicit_abandonment_is_required_before_retrying_ambiguous_mutation() {
        let (_temp, mut store) = initialized_store();
        let attempt = PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: "activation-1".to_string(),
            attempt: 1,
            side_effect: DispatchSideEffect::Mutating,
            intent: serde_json::json!({"operation": "apply"}),
            prepared_at_ms: 12,
        };
        let identity = store.prepare_attempt(&attempt).expect("prepare");
        store
            .reconcile_prepared_attempts(10, 20)
            .expect("repair required");
        assert!(
            store
                .prepare_attempt(&PreparedAttempt {
                    attempt: 2,
                    prepared_at_ms: 21,
                    ..attempt.clone()
                })
                .is_err()
        );
        let result = store
            .repair_attempt(
                &identity,
                &RepairResolution::AbandonForExplicitRetry {
                    reason: "operator verified that no external mutation occurred".to_string(),
                },
                22,
            )
            .expect("abandon");
        assert_eq!(result.attempt_status, "abandoned");
        store
            .prepare_attempt(&PreparedAttempt {
                attempt: 2,
                prepared_at_ms: 23,
                ..attempt
            })
            .expect("explicit retry");
    }

    #[test]
    fn crash_boundaries_reopen_without_blind_mutation_redispatch() {
        struct Observer;
        impl AttemptStatusObserver for Observer {
            fn observe(
                &self,
                request: &AttemptReconciliationRequest,
            ) -> Result<AttemptObservation, WorkflowStoreError> {
                Ok(AttemptObservation::Succeeded {
                    output: ValidatedOutput {
                        output_id: "output-after-restart".to_string(),
                        run_id: request.run_id.clone(),
                        node_id: request.node_id.clone(),
                        activation_id: request.activation_id.clone(),
                        schema_id: "review/v1".to_string(),
                        schema_version: 1,
                        value: serde_json::json!({"approved": true}),
                        artifact_reference: None,
                        created_at_ms: 30,
                    },
                })
            }
        }

        let temp = tempfile::tempdir().expect("temp");
        let attempt = PreparedAttempt {
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: "activation-1".to_string(),
            attempt: 1,
            side_effect: DispatchSideEffect::Mutating,
            intent: serde_json::json!({"operation": "apply"}),
            prepared_at_ms: 12,
        };
        let identity = {
            let mut store = initialized_store_at(temp.path());
            store.prepare_attempt(&attempt).expect("prepare")
        };
        {
            let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
            let summary = reopened
                .reconcile_prepared_attempts(10, 20)
                .expect("reconcile missing receipt");
            assert_eq!(summary.repair_required, [identity]);
        }

        let temp = tempfile::tempdir().expect("temp");
        let identity = {
            let mut store = initialized_store_at(temp.path());
            let identity = store.prepare_attempt(&attempt).expect("prepare");
            store
                .persist_dispatch_receipt(&DispatchReceipt {
                    run_id: attempt.run_id.clone(),
                    node_id: attempt.node_id.clone(),
                    activation_id: attempt.activation_id.clone(),
                    attempt: attempt.attempt,
                    dispatch_identity: identity.clone(),
                    receipt: serde_json::json!({"turn_id": "turn-1"}),
                    admitted_at_ms: 13,
                })
                .expect("receipt");
            identity
        };
        let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
        let summary = reopened
            .reconcile_receipt_backed_attempts(&Observer, 10, 30)
            .expect("receipt reconciliation");
        assert_eq!(summary.succeeded, [identity]);
        assert_eq!(
            reopened
                .attempt_history("run-1", None, 10)
                .expect("attempts")[0]
                .status,
            "succeeded"
        );
    }

    #[test]
    fn parallel_and_output_crash_boundaries_reconcile_independently() {
        struct Observer;
        impl AttemptStatusObserver for Observer {
            fn observe(
                &self,
                request: &AttemptReconciliationRequest,
            ) -> Result<AttemptObservation, WorkflowStoreError> {
                Ok(AttemptObservation::Succeeded {
                    output: ValidatedOutput {
                        output_id: format!("output-{}", request.activation_id),
                        run_id: request.run_id.clone(),
                        node_id: request.node_id.clone(),
                        activation_id: request.activation_id.clone(),
                        schema_id: "review/v1".to_string(),
                        schema_version: 1,
                        value: serde_json::json!({"approved": true}),
                        artifact_reference: None,
                        created_at_ms: 30,
                    },
                })
            }
        }

        let temp = tempfile::tempdir().expect("temp");
        {
            let mut store = initialized_store_at(temp.path());
            store
                .create_activation(&NewActivation {
                    node_id: "security-review".to_string(),
                    activation_id: "activation-2".to_string(),
                    created_at_ms: 12,
                    ..new_activation()
                })
                .expect("parallel activation");
            let first = PreparedAttempt {
                run_id: "run-1".to_string(),
                node_id: "review".to_string(),
                activation_id: "activation-1".to_string(),
                attempt: 1,
                side_effect: DispatchSideEffect::ReadOnly,
                intent: serde_json::json!({"operation": "review"}),
                prepared_at_ms: 13,
            };
            let first_identity = store.prepare_attempt(&first).expect("first prepare");
            store
                .persist_dispatch_receipt(&DispatchReceipt {
                    run_id: first.run_id.clone(),
                    node_id: first.node_id.clone(),
                    activation_id: first.activation_id.clone(),
                    attempt: first.attempt,
                    dispatch_identity: first_identity,
                    receipt: serde_json::json!({"turn_id": "turn-1"}),
                    admitted_at_ms: 14,
                })
                .expect("first receipt");
            store
                .prepare_attempt(&PreparedAttempt {
                    node_id: "security-review".to_string(),
                    activation_id: "activation-2".to_string(),
                    prepared_at_ms: 15,
                    ..first
                })
                .expect("second prepare");
        }
        let mut reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("reopen");
        let receipt_summary = reopened
            .reconcile_receipt_backed_attempts(&Observer, 10, 30)
            .expect("first branch");
        assert_eq!(receipt_summary.succeeded.len(), 1);
        let prepared_summary = reopened
            .reconcile_prepared_attempts(10, 31)
            .expect("second branch");
        assert_eq!(prepared_summary.safe_prepared.len(), 1);
        let attempts = reopened
            .attempt_history("run-1", None, 10)
            .expect("attempts");
        assert_eq!(attempts.len(), 2);
        assert!(attempts.iter().any(|attempt| attempt.status == "succeeded"));
        assert!(attempts.iter().any(|attempt| attempt.status == "prepared"));

        drop(reopened);
        let reopened = WorkflowStore::open_in_state_dir(temp.path()).expect("second reopen");
        let outputs: u64 = reopened
            .connection
            .query_row("SELECT COUNT(*) FROM workflow_outputs", [], |row| {
                row.get(0)
            })
            .expect("outputs");
        let completed: u64 = reopened
            .connection
            .query_row(
                "SELECT COUNT(*) FROM workflow_activations WHERE status = 'completed'",
                [],
                |row| row.get(0),
            )
            .expect("completed activations");
        assert_eq!(outputs, 1);
        assert_eq!(completed, 1);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn decisions_grants_leases_and_checkpoints_are_durable_and_fail_closed() {
        let (_temp, mut store) = initialized_store();
        let decision = WorkflowDecision {
            decision_id: "decision-1".to_string(),
            run_id: "run-1".to_string(),
            node_id: Some("review".to_string()),
            decision_type: "branch".to_string(),
            value: serde_json::json!({"selected": "approve"}),
            created_at_ms: 12,
        };
        store.persist_decision(&decision).expect("decision");
        store
            .persist_decision(&decision)
            .expect("idempotent decision");
        assert_eq!(
            store.decision("decision-1").expect("load decision"),
            Some(decision.clone())
        );
        assert!(
            store
                .persist_decision(&WorkflowDecision {
                    value: serde_json::json!({"selected": "reject"}),
                    ..decision.clone()
                })
                .is_err()
        );

        let grant = WorkflowGrant {
            grant_id: "grant-1".to_string(),
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            scope: serde_json::json!({"workspace": "snapshot-1", "tools": ["git_commit"]}),
            granted_at_ms: 13,
            expires_at_ms: Some(100),
        };
        store.persist_grant(&grant).expect("grant");
        store.persist_grant(&grant).expect("idempotent grant");
        assert_eq!(
            store.grant("grant-1").expect("load grant"),
            Some(grant.clone())
        );

        let read = WorkflowResourceLease {
            lease_id: "lease-read-1".to_string(),
            run_id: "run-1".to_string(),
            node_id: "review".to_string(),
            activation_id: "activation-1".to_string(),
            resource_key: "repository".to_string(),
            mode: ResourceLeaseMode::Read,
            acquired_at_ms: 14,
            expires_at_ms: None,
        };
        store.acquire_resource_lease(&read).expect("read lease");
        store
            .acquire_resource_lease(&read)
            .expect("idempotent lease");
        let second_read = WorkflowResourceLease {
            lease_id: "lease-read-2".to_string(),
            ..read.clone()
        };
        store
            .acquire_resource_lease(&second_read)
            .expect("compatible read lease");
        assert!(
            store
                .acquire_resource_lease(&WorkflowResourceLease {
                    lease_id: "lease-write".to_string(),
                    mode: ResourceLeaseMode::Write,
                    ..read.clone()
                })
                .is_err()
        );
        store
            .release_resource_lease("lease-read-1", 15)
            .expect("release first");
        store
            .release_resource_lease("lease-read-2", 16)
            .expect("release second");
        assert!(store.acquire_resource_lease(&second_read).is_err());
        store
            .acquire_resource_lease(&WorkflowResourceLease {
                lease_id: "lease-write".to_string(),
                mode: ResourceLeaseMode::Write,
                acquired_at_ms: 17,
                ..read
            })
            .expect("write after readers");

        let event_tail: u64 = store
            .connection
            .query_row("SELECT MAX(event_seq) FROM workflow_events", [], |row| {
                row.get(0)
            })
            .expect("tail");
        let checkpoint = WorkflowProjectionCheckpoint {
            projection_name: "run_summary".to_string(),
            projection_version: 1,
            last_event_seq: event_tail,
        };
        store
            .advance_projection_checkpoint(&checkpoint)
            .expect("checkpoint");
        assert_eq!(
            store
                .projection_checkpoint("run_summary")
                .expect("load checkpoint"),
            Some(checkpoint.clone())
        );
        assert!(
            store
                .advance_projection_checkpoint(&WorkflowProjectionCheckpoint {
                    last_event_seq: event_tail.saturating_sub(1),
                    ..checkpoint
                })
                .is_err()
        );
    }

    #[test]
    fn validated_output_and_activation_complete_atomically() {
        let (_temp, mut store) = initialized_store();
        store
            .persist_validated_output(&ValidatedOutput {
                output_id: "output-1".to_string(),
                run_id: "run-1".to_string(),
                node_id: "review".to_string(),
                activation_id: "activation-1".to_string(),
                schema_id: "review/v1".to_string(),
                schema_version: 1,
                value: serde_json::json!({"approved": true}),
                artifact_reference: None,
                created_at_ms: 13,
            })
            .expect("output");
        let status: String = store
            .connection
            .query_row(
                "SELECT status FROM workflow_activations WHERE run_id = 'run-1'",
                [],
                |row| row.get(0),
            )
            .expect("status");
        assert_eq!(status, "completed");
    }

    #[test]
    fn canonical_path_is_below_workflows_directory() {
        assert_eq!(
            workflow_database_path(Path::new("/state")),
            Path::new("/state/workflows/workflow.db")
        );
    }

    #[test]
    fn definitions_persist_idempotently_and_verify_checksum() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        let definition = definition("example");
        let first = store
            .persist_definition("example", 1, &definition)
            .expect("persist");
        let second = store
            .persist_definition("example", 1, &definition)
            .expect("idempotent");
        assert_eq!(first, second);
        assert_eq!(store.definition("example", 1).expect("load"), Some(first));
    }

    #[test]
    fn definition_identity_conflicts_fail_closed() {
        let temp = tempfile::tempdir().expect("temp");
        let mut store = WorkflowStore::open_in_state_dir(temp.path()).expect("store");
        store
            .persist_definition("example", 1, &definition("first"))
            .expect("first");
        let error = store
            .persist_definition("example", 1, &definition("second"))
            .expect_err("conflict");
        assert!(error.to_string().contains("identity conflict"));
    }
}
