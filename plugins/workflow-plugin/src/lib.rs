#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Workflow product integration plugin for Bcode.

mod authoring_tui;
#[cfg(feature = "static-bundled")]
mod cli;
pub mod tui;

use bcode_command::{
    COMMAND_INTERFACE_ID, CommandAction, CommandContribution, CommandEffect, CommandOwner,
    CommandSurface, InvokeCommandRequest, InvokeCommandResponse, OP_INVOKE_COMMAND,
};
use bcode_plugin_sdk::prelude::*;
use bcode_plugin_sdk::{OP_SESSION_STATUS, SESSION_STATUS_INTERFACE_ID};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const PLUGIN_ID: &str = "bcode.workflow";
const STATUS_SURFACE_KIND: &str = "workflow.status";
const AUTHOR_SURFACE_KIND: &str = "workflow.author";
const QUERY_LIMIT: usize = 100;

/// Current workflow-plugin coding-state contract version.
pub const CODING_WORKFLOW_STATE_VERSION: u32 = 1;
/// Runtime-owned maximum implementation iterations in one batch.
pub const IMPLEMENTATION_BATCH_ITERATION_LIMIT: u32 = 20;
/// Runtime-owned maximum implementation batches in one delivery tranche.
pub const DELIVERY_TRANCHE_BATCH_LIMIT: u32 = 5;
/// Runtime-owned maximum delivery tranches in one parent run.
pub const PROGRESS_DRIVEN_TRANCHE_LIMIT: u32 = 10;
/// Derived maximum implementation batches in one parent run.
pub const PROGRESS_DRIVEN_BATCH_LIMIT: u32 = 50;
/// Derived maximum implementation turns in one parent run.
pub const PROGRESS_DRIVEN_IMPLEMENTATION_TURN_LIMIT: u32 = 1_000;
/// Exact descendant budget reserved by the three-level flagship hierarchy.
pub const PROGRESS_DRIVEN_DESCENDANT_LIMIT: u32 = 60;
const _: () =
    assert!(PROGRESS_DRIVEN_DESCENDANT_LIMIT <= bcode_workflow::MAX_WORKFLOW_CALL_DESCENDANTS);

/// Exact progress-document reference retained by coding workflows.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CodingProgressDocumentReference {
    pub path: String,
    #[serde(default)]
    pub digest_sha256: Option<String>,
}

/// Explicit include/exclude policy retained by coding workflows.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CodingPathPolicy {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

/// Workflow-plugin-owned product phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodingWorkflowPhase {
    Implementing,
    Verifying,
    Formatting,
    PreparingCheckpoint,
    Evaluating,
    Completed,
    Exhausted,
}

/// Bounded latest product receipts; runtime counters deliberately do not appear here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CodingWorkflowLatest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implementation_summary: Option<String>,
    #[serde(default)]
    pub repository_snapshots: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_receipt: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formatting_receipt: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepared_change_set: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_receipt: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_assessment: Option<serde_json::Value>,
}

/// Versioned workflow-plugin-owned coding product state.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CodingWorkflowState {
    pub version: u32,
    pub objective: String,
    pub implementation_prompt: String,
    pub completion_condition: String,
    pub progress_document: CodingProgressDocumentReference,
    pub validation_plan: serde_json::Value,
    pub formatting_plan: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction_fingerprint_sha256: Option<String>,
    pub path_policy: CodingPathPolicy,
    pub phase: CodingWorkflowPhase,
    pub latest: CodingWorkflowLatest,
    #[serde(default)]
    pub artifacts: Vec<bcode_workflow::ArtifactReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_context: Option<CodingWorkflowOperationContext>,
}

/// Bounded transient values used only to construct exact owner requests within one batch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CodingWorkflowOperationContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_snapshot: Option<bcode_git_plugin::RepositorySnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_snapshot: Option<bcode_git_plugin::RepositorySnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_result: Option<bcode_shell_plugin::ShellWorkflowCommandPlanResult>,
}

/// Stable batch product outcome, separate from runtime-owned iteration facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImplementationBatchOutcome {
    Completed,
    Exhausted,
}

/// Stable tranche product outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryTrancheOutcome {
    Completed,
    Exhausted,
}

/// Stable parent product outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressDrivenOutcome {
    Completed,
    OperatorStopped,
    HardLimitReached,
}

#[derive(Default)]
pub struct WorkflowPlugin;

impl RustPlugin for WorkflowPlugin {
    fn register_commands(&mut self, registrar: CommandRegistrar) -> Result<(), PluginError> {
        for command in command_contributions() {
            registrar
                .register(&command)
                .map_err(|error| PluginError::failed(error.to_string()))?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match (
            context.request.interface_id.as_str(),
            context.request.operation.as_str(),
        ) {
            (COMMAND_INTERFACE_ID, OP_INVOKE_COMMAND) => invoke_command(&context.request),
            (SESSION_STATUS_INTERFACE_ID, OP_SESSION_STATUS) => session_status(&context.request),
            (bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID, "workflow.instruction-drift") => {
                let invocation = match context
                    .request
                    .payload_json::<bcode_workflow::WorkflowBlockInvocation>()
                {
                    Ok(invocation) => invocation,
                    Err(error) => {
                        return ServiceResponse::error("invalid_request", error.to_string());
                    }
                };
                let request = match invocation.typed_input::<InstructionDriftReviewRequest>() {
                    Ok(request) => request,
                    Err(error) => return ServiceResponse::error("invalid_request", error),
                };
                match instruction_drift_receipt(&request) {
                    Ok(receipt) => json_response(&receipt),
                    Err(error) => ServiceResponse::error("invalid_drift_review", error),
                }
            }
            (bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID, "workflow.batch-outcome") => {
                let invocation = match context
                    .request
                    .payload_json::<bcode_workflow::WorkflowBlockInvocation>()
                {
                    Ok(invocation) => invocation,
                    Err(error) => {
                        return ServiceResponse::error("invalid_request", error.to_string());
                    }
                };
                let state = match invocation.typed_input::<CodingWorkflowState>() {
                    Ok(state) => state,
                    Err(error) => return ServiceResponse::error("invalid_request", error),
                };
                let request = BatchOutcomeRequest { version: 1, state };
                match batch_outcome(&request) {
                    Ok(outcome) => json_response(&outcome),
                    Err(error) => ServiceResponse::error("invalid_batch_outcome", error),
                }
            }
            (bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID, operation)
                if matches!(
                    operation,
                    "workflow.snapshot-input"
                        | "workflow.validation-input"
                        | "workflow.formatting-input"
                        | "workflow.pre-format-verification-input"
                        | "workflow.post-format-verification-input"
                        | "workflow.prepare-checkpoint-input"
                        | "workflow.commit-message-input"
                        | "workflow.compose-commit-input"
                ) =>
            {
                let invocation = match context
                    .request
                    .payload_json::<bcode_workflow::WorkflowBlockInvocation>()
                {
                    Ok(invocation) => invocation,
                    Err(error) => {
                        return ServiceResponse::error("invalid_request", error.to_string());
                    }
                };
                let (mut state, artifacts, current_value) = match invocation
                    .typed_input::<serde_json::Value>()
                {
                    Ok(value) => match bcode_workflow::validate_workflow_state_envelope(&value) {
                        Ok(parts) => {
                            let state = match serde_json::from_value(parts.state) {
                                Ok(state) => state,
                                Err(error) => {
                                    return ServiceResponse::error(
                                        "invalid_request",
                                        error.to_string(),
                                    );
                                }
                            };
                            (state, parts.artifacts, Some(parts.value))
                        }
                        Err(_) => match serde_json::from_value(value) {
                            Ok(state) => (state, Vec::new(), None),
                            Err(error) => {
                                return ServiceResponse::error(
                                    "invalid_request",
                                    error.to_string(),
                                );
                            }
                        },
                    },
                    Err(error) => {
                        return ServiceResponse::error("invalid_request", error);
                    }
                };
                let input_operation = match operation {
                    "workflow.snapshot-input" => BatchInputOperation::Snapshot,
                    "workflow.validation-input" => BatchInputOperation::ValidationPlan,
                    "workflow.formatting-input" => BatchInputOperation::FormattingPlan,
                    "workflow.pre-format-verification-input" => {
                        BatchInputOperation::PreFormatVerification
                    }
                    "workflow.post-format-verification-input" => {
                        BatchInputOperation::PostFormatVerification
                    }
                    "workflow.prepare-checkpoint-input" => BatchInputOperation::PrepareCheckpoint,
                    "workflow.commit-message-input" => BatchInputOperation::CommitMessage,
                    "workflow.compose-commit-input" => BatchInputOperation::ComposeCommit,
                    _ => unreachable!("guarded workflow batch input operation"),
                };
                if let Err(error) =
                    retain_batch_operation_result(&mut state, input_operation, current_value)
                {
                    return ServiceResponse::error("invalid_batch_input", error);
                }
                match batch_input(&state, &artifacts, input_operation) {
                    Ok(input) => json_response(&input),
                    Err(error) => ServiceResponse::error("invalid_batch_input", error),
                }
            }
            _ => ServiceResponse::error(
                "unsupported_operation",
                "unsupported workflow plugin operation",
            ),
        }
    }
}

#[allow(clippy::too_many_lines)]
fn command_contributions() -> Vec<CommandContribution> {
    [
        ("workflow", "Workflow", "Open workflow status"),
        (
            "workflow.list",
            "Workflow: List",
            "List registered workflows",
        ),
        (
            "workflow.templates",
            "Workflow: Templates",
            "List available workflow templates",
        ),
        (
            "workflow.template-describe",
            "Workflow: Configure Template",
            "Describe and configure an exact workflow template",
        ),
        (
            "workflow.template-instantiate",
            "Workflow: Edit Template",
            "Instantiate an exact maintainable template and open the graph editor",
        ),
        (
            "workflow.template-start",
            "Workflow: Start Template",
            "Start a validated workflow template",
        ),
        (
            "workflow.author-apply",
            "Workflow: Apply Source",
            "Create or optimistically update the canonical source draft",
        ),
        (
            "workflow.author-check",
            "Workflow: Check Source",
            "Validate and compile-preview one authored workflow source",
        ),
        (
            "workflow.package-check",
            "Workflow: Check Package",
            "Validate and compile-preview one bounded workflow package",
        ),
        (
            "workflow.author-create",
            "Workflow: Create From Source",
            "Create one durable workflow draft from authored source",
        ),
        (
            "workflow.author-update",
            "Workflow: Update From Source",
            "Replace one exact durable draft generation from authored source",
        ),
        (
            "workflow.author-publish",
            "Workflow: Publish Authored Draft",
            "Publish one exact authored draft generation",
        ),
        (
            "workflow.author-export",
            "Workflow: Export Authored Revision",
            "Export one exact authored revision and dependency manifest",
        ),
        (
            "workflow.author-import",
            "Workflow: Import Authored Workflow",
            "Import one exact portable authored workflow bundle",
        ),
        (
            "workflow.register",
            "Workflow: Register",
            "Register a compiled workflow definition",
        ),
        (
            "workflow.run",
            "Workflow: Run",
            "Start a registered workflow",
        ),
        (
            "workflow.status",
            "Workflow: Status",
            "Inspect workflow status",
        ),
        ("workflow.pause", "Workflow: Pause", "Pause a workflow run"),
        (
            "workflow.resume",
            "Workflow: Resume",
            "Resume a workflow run",
        ),
        (
            "workflow.cancel",
            "Workflow: Cancel",
            "Cancel a workflow run",
        ),
        (
            "workflow.inspect",
            "Workflow: Inspect",
            "Inspect workflow graph and history",
        ),
        (
            "workflow.doctor",
            "Workflow: Doctor",
            "Diagnose one workflow run without mutation",
        ),
        (
            "workflow.repair",
            "Workflow: Repair",
            "Apply one explicit typed attempt repair",
        ),
        (
            "workflow.retry-node",
            "Workflow: Retry Node",
            "Retry one exact latest failed node attempt",
        ),
        (
            "workflow.provide-input",
            "Workflow: Provide Input",
            "Resolve a waiting input",
        ),
        (
            "workflow.approve-mutation",
            "Workflow: Approve Mutation",
            "Approve one exact pending mutation dispatch",
        ),
        (
            "workflow.deny-mutation",
            "Workflow: Deny Mutation",
            "Deny one exact pending mutation dispatch",
        ),
    ]
    .into_iter()
    .map(|(id, title, description)| CommandContribution {
        id: id.to_string(),
        title: title.to_string(),
        description: Some(description.to_string()),
        category: Some("workflow".to_string()),
        surfaces: BTreeSet::from([CommandSurface::Palette, CommandSurface::Slash]),
        execution: bcode_command::CommandExecution::Immediate,
        owner: CommandOwner::Plugin {
            plugin_id: PLUGIN_ID.to_string(),
        },
        action: CommandAction::Plugin {
            plugin_id: PLUGIN_ID.to_string(),
            command_id: id.to_string(),
        },
    })
    .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct InstructionDriftReviewRequest {
    version: u32,
    accepted_instruction_fingerprint_sha256: String,
    current_instruction_fingerprint_sha256: String,
    accepted_validation_plan_sha256: String,
    proposed_validation_plan_sha256: String,
    accepted_formatting_plan_sha256: String,
    proposed_formatting_plan_sha256: String,
    reviewed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum InstructionDriftReceipt {
    Unchanged {
        version: u32,
        instruction_fingerprint_sha256: String,
    },
    Blocked {
        version: u32,
        accepted_instruction_fingerprint_sha256: String,
        current_instruction_fingerprint_sha256: String,
        accepted_validation_plan_sha256: String,
        proposed_validation_plan_sha256: String,
        accepted_formatting_plan_sha256: String,
        proposed_formatting_plan_sha256: String,
    },
    ReviewedReplacement {
        version: u32,
        instruction_fingerprint_sha256: String,
        validation_plan_sha256: String,
        formatting_plan_sha256: String,
    },
}

fn instruction_drift_receipt(
    request: &InstructionDriftReviewRequest,
) -> Result<InstructionDriftReceipt, String> {
    if request.version != 1
        || [
            &request.accepted_instruction_fingerprint_sha256,
            &request.current_instruction_fingerprint_sha256,
            &request.accepted_validation_plan_sha256,
            &request.proposed_validation_plan_sha256,
            &request.accepted_formatting_plan_sha256,
            &request.proposed_formatting_plan_sha256,
        ]
        .into_iter()
        .any(|digest| {
            digest.len() != 64
                || !digest
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
    {
        return Err("instruction drift review contains invalid version or digest".to_string());
    }
    if request.accepted_instruction_fingerprint_sha256
        == request.current_instruction_fingerprint_sha256
    {
        return Ok(InstructionDriftReceipt::Unchanged {
            version: 1,
            instruction_fingerprint_sha256: request.current_instruction_fingerprint_sha256.clone(),
        });
    }
    if !request.reviewed {
        return Ok(InstructionDriftReceipt::Blocked {
            version: 1,
            accepted_instruction_fingerprint_sha256: request
                .accepted_instruction_fingerprint_sha256
                .clone(),
            current_instruction_fingerprint_sha256: request
                .current_instruction_fingerprint_sha256
                .clone(),
            accepted_validation_plan_sha256: request.accepted_validation_plan_sha256.clone(),
            proposed_validation_plan_sha256: request.proposed_validation_plan_sha256.clone(),
            accepted_formatting_plan_sha256: request.accepted_formatting_plan_sha256.clone(),
            proposed_formatting_plan_sha256: request.proposed_formatting_plan_sha256.clone(),
        });
    }
    Ok(InstructionDriftReceipt::ReviewedReplacement {
        version: 1,
        instruction_fingerprint_sha256: request.current_instruction_fingerprint_sha256.clone(),
        validation_plan_sha256: request.proposed_validation_plan_sha256.clone(),
        formatting_plan_sha256: request.proposed_formatting_plan_sha256.clone(),
    })
}

fn retain_batch_operation_result(
    state: &mut CodingWorkflowState,
    operation: BatchInputOperation,
    value: Option<serde_json::Value>,
) -> Result<(), String> {
    let Some(value) = value else {
        return Ok(());
    };
    if let Ok(receipt) =
        serde_json::from_value::<bcode_git_plugin::VerificationReceipt>(value.clone())
    {
        state.latest.verification_receipt = Some(
            serde_json::to_value(&receipt)
                .map_err(|error| format!("verification receipt cannot be retained: {error}"))?,
        );
        if operation == BatchInputOperation::FormattingPlan
            && receipt.stage == bcode_git_plugin::VerificationStage::PreFormat
        {
            state.operation_context = Some(CodingWorkflowOperationContext::default());
        }
        return Ok(());
    }
    let context = state.operation_context.get_or_insert_with(Default::default);
    if let Ok(snapshot) =
        serde_json::from_value::<bcode_git_plugin::RepositorySnapshot>(value.clone())
    {
        state.latest.repository_snapshots.push(value);
        if state.latest.repository_snapshots.len() > 8 {
            state.latest.repository_snapshots.remove(0);
        }
        if context.pre_snapshot.is_none() {
            context.pre_snapshot = Some(snapshot);
        } else {
            context.post_snapshot = Some(snapshot);
        }
        return Ok(());
    }
    if let Ok(result) =
        serde_json::from_value::<bcode_shell_plugin::ShellWorkflowCommandPlanResult>(value.clone())
    {
        if operation == BatchInputOperation::Snapshot && context.pre_snapshot.is_none() {
            state.latest.formatting_receipt = Some(value);
        }
        context.command_result = Some(result);
        return Ok(());
    }
    if let Ok(preparation) =
        serde_json::from_value::<bcode_git_plugin::PrepareResponse>(value.clone())
    {
        state.latest.prepared_change_set = Some(
            serde_json::to_value(preparation)
                .map_err(|error| format!("prepared change set cannot be retained: {error}"))?,
        );
        return Ok(());
    }
    if let Ok(commit) = serde_json::from_value::<bcode_git_plugin::CommitResponse>(value.clone()) {
        state.latest.commit_receipt = Some(
            serde_json::to_value(commit)
                .map_err(|error| format!("commit receipt cannot be retained: {error}"))?,
        );
        return Ok(());
    }
    if operation == BatchInputOperation::CommitMessage {
        state.latest.prepared_change_set = Some(value);
        return Ok(());
    }
    if operation == BatchInputOperation::ComposeCommit {
        let message = serde_json::from_value::<CodingWorkflowState>(value.clone())
            .ok()
            .and_then(|state| state.latest.completion_assessment)
            .unwrap_or(value);
        state.latest.completion_assessment = Some(message);
        return Ok(());
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn batch_input(
    state: &CodingWorkflowState,
    artifacts: &[bcode_workflow::ArtifactReference],
    operation: BatchInputOperation,
) -> Result<serde_json::Value, String> {
    if state.version != CODING_WORKFLOW_STATE_VERSION {
        return Err("unsupported coding batch input version".to_string());
    }
    let fingerprint = state
        .instruction_fingerprint_sha256
        .clone()
        .ok_or_else(|| "coding state has no accepted instruction fingerprint".to_string())?;
    let value = match operation {
        BatchInputOperation::Snapshot => serde_json::json!({
            "version": 1,
            "include_prefixes": state.path_policy.include.clone(),
            "exclude_prefixes": state.path_policy.exclude.clone(),
            "progress_document_path": state.progress_document.path.clone(),
            "max_paths": 10_000,
            "project_instruction_fingerprint_sha256": fingerprint,
        }),
        BatchInputOperation::ValidationPlan => state.validation_plan.clone(),
        BatchInputOperation::FormattingPlan => state.formatting_plan.clone(),
        BatchInputOperation::PreFormatVerification
        | BatchInputOperation::PostFormatVerification => {
            let pre_snapshot = state
                .operation_context
                .as_ref()
                .and_then(|context| context.pre_snapshot.as_ref())
                .cloned()
                .ok_or_else(|| "verification input has no pre-snapshot".to_string())?;
            let post_snapshot = state
                .operation_context
                .as_ref()
                .and_then(|context| context.post_snapshot.as_ref())
                .cloned()
                .ok_or_else(|| "verification input has no post-snapshot".to_string())?;
            let command_result = state
                .operation_context
                .as_ref()
                .and_then(|context| context.command_result.as_ref())
                .ok_or_else(|| "verification input has no shell result".to_string())?;
            serde_json::to_value(bcode_git_plugin::VerificationReceiptRequest {
                version: 1,
                stage: if operation == BatchInputOperation::PreFormatVerification {
                    bcode_git_plugin::VerificationStage::PreFormat
                } else {
                    bcode_git_plugin::VerificationStage::PostFormat
                },
                plan_sha256: command_result.plan_sha256.clone(),
                instruction_fingerprint_sha256: fingerprint,
                pre_snapshot,
                post_snapshot,
                commands_passed: command_result.passed,
                required_artifacts_complete: command_result.commands.iter().all(|command| {
                    !command.stdout_truncated && !command.stderr_truncated
                        || !command_result.artifacts.is_empty()
                }),
            })
            .map_err(|error| error.to_string())?
        }
        BatchInputOperation::PrepareCheckpoint => {
            serde_json::to_value(bcode_git_plugin::PrepareRequest {
                include_prefixes: state
                    .path_policy
                    .include
                    .clone()
                    .into_iter()
                    .map(Into::into)
                    .collect(),
                exclude_prefixes: state
                    .path_policy
                    .exclude
                    .clone()
                    .into_iter()
                    .map(Into::into)
                    .collect(),
                progress_document_path: Some(state.progress_document.path.clone().into()),
                project_instruction_fingerprint_sha256: fingerprint,
                max_paths: 10_000,
            })
            .map_err(|error| error.to_string())?
        }
        BatchInputOperation::ComposeCommit => {
            let preparation: bcode_git_plugin::PrepareResponse = serde_json::from_value(
                state
                    .latest
                    .prepared_change_set
                    .clone()
                    .ok_or_else(|| "coding state has no prepared change set".to_string())?,
            )
            .map_err(|error| format!("prepared change set is invalid: {error}"))?;
            let message = state
                .latest
                .completion_assessment
                .as_ref()
                .ok_or_else(|| "coding state has no structured commit message".to_string())?;
            serde_json::to_value(bcode_git_plugin::ComposeCommitRequest {
                preparation,
                message: bcode_git_plugin::ProposedCommitMessage {
                    title: message
                        .get("title")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| "structured commit message has no title".to_string())?
                        .to_string(),
                    description: message
                        .get("description")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                },
                no_changes: bcode_git_plugin::NoChangesDecision::NoOp,
            })
            .map_err(|error| error.to_string())?
        }
        BatchInputOperation::CommitMessage => state
            .latest
            .prepared_change_set
            .clone()
            .ok_or_else(|| "commit-message input has no exact Git preparation".to_string())?,
    };
    Ok(serde_json::json!({
        "schema_version": bcode_workflow::WORKFLOW_STATE_ENVELOPE_VERSION,
        "state": state,
        "value": value,
        "artifacts": artifacts,
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum BatchInputOperation {
    Snapshot,
    ValidationPlan,
    FormattingPlan,
    PreFormatVerification,
    PostFormatVerification,
    PrepareCheckpoint,
    CommitMessage,
    ComposeCommit,
}

fn batch_outcome(request: &BatchOutcomeRequest) -> Result<BatchOutcomeReceipt, String> {
    if request.version != 1 || request.state.version != CODING_WORKFLOW_STATE_VERSION {
        return Err("unsupported coding batch outcome version".to_string());
    }
    let deterministic_complete = request
        .state
        .latest
        .verification_receipt
        .as_ref()
        .and_then(|receipt| receipt.get("verified"))
        .and_then(serde_json::Value::as_bool)
        == Some(true)
        && request
            .state
            .latest
            .completion_assessment
            .as_ref()
            .and_then(|assessment| assessment.get("condition_met"))
            .and_then(serde_json::Value::as_bool)
            == Some(true);
    let outcome = if deterministic_complete {
        ImplementationBatchOutcome::Completed
    } else {
        ImplementationBatchOutcome::Exhausted
    };
    let mut state = request.state.clone();
    state.phase = if deterministic_complete {
        CodingWorkflowPhase::Completed
    } else {
        CodingWorkflowPhase::Exhausted
    };
    Ok(BatchOutcomeReceipt {
        version: 1,
        outcome,
        state,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BatchOutcomeRequest {
    version: u32,
    state: CodingWorkflowState,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BatchOutcomeReceipt {
    version: u32,
    outcome: ImplementationBatchOutcome,
    state: CodingWorkflowState,
}

fn invoke_command(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<InvokeCommandRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    match run_async(async move { Box::pin(execute_command(request)).await }) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("workflow_command_failed", error),
    }
}

#[allow(clippy::too_many_lines, clippy::large_stack_frames)]
pub(crate) async fn execute_command(
    mut request: InvokeCommandRequest,
) -> Result<InvokeCommandResponse, String> {
    if let Some(arguments) = request.args.get("arguments").cloned() {
        for (key, value) in parse_arguments(&arguments) {
            request.args.entry(key).or_insert(value);
        }
    }
    let client = bcode_client::BcodeClient::default_endpoint();
    let mut options = serde_json::Map::from_iter([
        (
            "command_id".to_string(),
            serde_json::Value::String(request.command_id.clone()),
        ),
        (
            "arguments".to_string(),
            serde_json::to_value(&request.args).map_err(|error| error.to_string())?,
        ),
    ]);
    let message = match request.command_id.as_str() {
        "workflow" | "workflow.status" => {
            let runs = client
                .list_workflow_runs(QUERY_LIMIT)
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "runs".to_string(),
                serde_json::to_value(&runs).map_err(|error| error.to_string())?,
            );
            format!("{} durable workflow runs", runs.len())
        }
        "workflow.list" => {
            let definitions = client
                .list_workflow_definitions(QUERY_LIMIT)
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "definitions".to_string(),
                serde_json::to_value(&definitions).map_err(|error| error.to_string())?,
            );
            let services = client
                .plugin_services()
                .await
                .map_err(|error| error.to_string())?;
            let blocks = services
                .into_iter()
                .flat_map(|service| service.workflow_blocks)
                .collect::<Vec<_>>();
            options.insert(
                "blocks".to_string(),
                serde_json::to_value(&blocks).map_err(|error| error.to_string())?,
            );
            format!(
                "{} registered workflow definitions · {} available plugin blocks",
                definitions.len(),
                blocks.len()
            )
        }
        "workflow.templates" => {
            let templates = client
                .list_workflow_templates(QUERY_LIMIT)
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "templates".to_string(),
                serde_json::to_value(&templates).map_err(|error| error.to_string())?,
            );
            format!("{} available workflow templates", templates.len())
        }
        "workflow.template-describe" | "workflow.template-instantiate" => {
            describe_or_instantiate_template(&request, &client, &mut options).await?
        }
        "workflow.template-start" => {
            let owner_plugin_id = required_arg(&request, "owner_plugin_id")?;
            let template_id = required_arg(&request, "template_id")?;
            let template_version = parse_arg::<u32>(&request, "template_version")?;
            let described = client
                .describe_workflow_template(
                    owner_plugin_id.clone(),
                    template_id.clone(),
                    template_version,
                )
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "workflow template not found or disabled".to_string())?;
            if !described.diagnostics.is_empty() {
                return Err(described
                    .diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; "));
            }
            let configuration_schema = described
                .authoring_document
                .as_ref()
                .map_or_else(
                    || described.template.configuration_schema(),
                    |document| &document.configuration_schema,
                )
                .schema
                .clone();
            let configuration = validate_template_configuration(
                &configuration_schema,
                &required_arg(&request, "configuration")?,
            )?;
            let limits = reference_template_run_limits(&template_id, &configuration)?;
            let parent_session_id = parse_arg(&request, "session_id")?;
            let started = client
                .start_workflow_template(bcode_ipc::WorkflowTemplateStartRequest {
                    owner_plugin_id,
                    template_id,
                    template_version,
                    run_id: request.args.get("run_id").cloned(),
                    workspace_snapshot: request.args.get("workspace_snapshot").cloned(),
                    parent_session_id,
                    configuration,
                    limits,
                })
                .await
                .map_err(|error| error.to_string())?;
            options.insert("runs".to_string(), serde_json::json!([started.run]));
            "workflow template started".to_string()
        }
        "workflow.author-apply" => {
            let source_format: bcode_workflow::WorkflowSourceFormat =
                serde_json::from_str(&required_arg(&request, "source_format")?)
                    .map_err(|error| error.to_string())?;
            let source = required_arg(&request, "source")?;
            let draft_id =
                request.args.get("draft_id").cloned().unwrap_or_else(|| {
                    bcode_workflow::DEFAULT_WORKFLOW_SOURCE_DRAFT_ID.to_string()
                });
            let applied = client
                .apply_workflow_source(source_format, source, draft_id)
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "source_apply".to_string(),
                serde_json::to_value(applied).map_err(|error| error.to_string())?,
            );
            "workflow source apply resolved".to_string()
        }
        "workflow.author-check" => {
            let source_format: bcode_workflow::WorkflowSourceFormat =
                serde_json::from_str(&required_arg(&request, "source_format")?)
                    .map_err(|error| error.to_string())?;
            let source = required_arg(&request, "source")?;
            let validation = client
                .validate_workflow_source(bcode_ipc::WorkflowSourceComputationRequest {
                    source_format,
                    source: source.clone(),
                    control: bcode_ipc::WorkflowComputationControl::default(),
                })
                .await
                .map_err(|error| error.to_string())?;
            let preview = client
                .preview_workflow_source(bcode_ipc::WorkflowSourcePreviewRequest {
                    source_format,
                    source,
                    configuration: None,
                    control: bcode_ipc::WorkflowComputationControl::default(),
                })
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "source_validation".to_string(),
                serde_json::to_value(validation).map_err(|error| error.to_string())?,
            );
            options.insert(
                "source_preview".to_string(),
                serde_json::to_value(preview).map_err(|error| error.to_string())?,
            );
            "workflow authoring source validated".to_string()
        }
        "workflow.package-check" => {
            let manifest: bcode_workflow::WorkflowPackageManifest =
                serde_json::from_str(&required_arg(&request, "package_manifest")?)
                    .map_err(|error| error.to_string())?;
            let validation = client
                .validate_workflow_package(bcode_ipc::WorkflowPackageComputationRequest {
                    manifest,
                    control: bcode_ipc::WorkflowComputationControl::default(),
                })
                .await
                .map_err(|error| error.to_string())?;
            let preview = client
                .preview_workflow_package(bcode_ipc::WorkflowPackagePreviewRequest {
                    plan: validation.plan.clone(),
                    configurations: BTreeMap::new(),
                    control: bcode_ipc::WorkflowComputationControl::default(),
                })
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "package_validation".to_string(),
                serde_json::to_value(validation).map_err(|error| error.to_string())?,
            );
            options.insert(
                "package_preview".to_string(),
                serde_json::to_value(preview).map_err(|error| error.to_string())?,
            );
            "workflow package validated".to_string()
        }
        "workflow.author-create" => {
            let source_format: bcode_workflow::WorkflowSourceFormat =
                serde_json::from_str(&required_arg(&request, "source_format")?)
                    .map_err(|error| error.to_string())?;
            let document: bcode_workflow::WorkflowAuthoringDocument =
                serde_json::from_str(&required_arg(&request, "source_document")?)
                    .map_err(|error| error.to_string())?;
            let draft_id = required_arg(&request, "draft_id")?;
            let created = client
                .create_authored_workflow(bcode_ipc::CreateAuthoredWorkflowRequest {
                    document,
                    draft_id,
                })
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "source_format".to_string(),
                serde_json::to_value(source_format).map_err(|error| error.to_string())?,
            );
            options.insert(
                "workflow".to_string(),
                serde_json::to_value(&created.0).map_err(|error| error.to_string())?,
            );
            options.insert(
                "draft".to_string(),
                serde_json::to_value(&created.1).map_err(|error| error.to_string())?,
            );
            "workflow draft created from source".to_string()
        }
        "workflow.author-update" => {
            let source_format: bcode_workflow::WorkflowSourceFormat =
                serde_json::from_str(&required_arg(&request, "source_format")?)
                    .map_err(|error| error.to_string())?;
            let document: bcode_workflow::WorkflowAuthoringDocument =
                serde_json::from_str(&required_arg(&request, "source_document")?)
                    .map_err(|error| error.to_string())?;
            let workflow_id = required_arg(&request, "workflow_id")?;
            let draft_id = required_arg(&request, "draft_id")?;
            let expected_generation = parse_arg::<u64>(&request, "expected_generation")?;
            let producer = document.producer.clone();
            let updated = client
                .update_workflow_draft(bcode_ipc::UpdateWorkflowDraftRequest {
                    workflow_id,
                    draft_id,
                    expected_generation,
                    document,
                    producer,
                })
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "source_format".to_string(),
                serde_json::to_value(source_format).map_err(|error| error.to_string())?,
            );
            options.insert(
                "draft_update".to_string(),
                serde_json::to_value(&updated).map_err(|error| error.to_string())?,
            );
            "workflow draft update resolved".to_string()
        }
        "workflow.author-publish" => {
            let workflow_id = required_arg(&request, "workflow_id")?;
            let draft_id = required_arg(&request, "draft_id")?;
            let expected_generation = parse_arg::<u64>(&request, "expected_generation")?;
            let result = client
                .publish_workflow_draft(bcode_ipc::PublishWorkflowDraftRequest {
                    workflow_id,
                    draft_id,
                    expected_generation,
                    configuration: None,
                    activate: false,
                    expected_active_revision: None,
                    control: bcode_ipc::WorkflowComputationControl::default(),
                })
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "publication".to_string(),
                serde_json::to_value(result).map_err(|error| error.to_string())?,
            );
            "workflow draft published".to_string()
        }
        "workflow.author-export" => {
            let workflow_id = required_arg(&request, "workflow_id")?;
            let revision = parse_arg::<u64>(&request, "revision")?;
            let bundle = client
                .export_workflow_revision(bcode_ipc::ExportWorkflowRevisionRequest {
                    workflow_id,
                    revision,
                })
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "export_bundle".to_string(),
                serde_json::to_value(bundle).map_err(|error| error.to_string())?,
            );
            "workflow revision exported".to_string()
        }
        "workflow.author-import" => {
            let bundle = serde_json::from_str(&required_arg(&request, "bundle")?)
                .map_err(|error| error.to_string())?;
            let target_workflow_id = required_arg(&request, "target_workflow_id")?;
            let draft_id = required_arg(&request, "draft_id")?;
            let imported = client
                .import_workflow(bcode_ipc::ImportWorkflowRequest {
                    bundle,
                    target_workflow_id,
                    draft_id,
                    collision_policy: bcode_ipc::WorkflowImportCollisionPolicy::RequireNewWorkflow,
                    control: bcode_ipc::WorkflowComputationControl::default(),
                })
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "import_result".to_string(),
                serde_json::to_value(imported).map_err(|error| error.to_string())?,
            );
            "workflow bundle imported".to_string()
        }
        "workflow.register" => {
            let definition_id = required_arg(&request, "definition_id")?;
            let version = parse_arg::<u32>(&request, "version")?;
            let definition = serde_json::from_str(&required_arg(&request, "definition")?)
                .map_err(|error| error.to_string())?;
            let registered = client
                .register_workflow_definition(bcode_ipc::WorkflowDefinitionRegistrationRequest {
                    definition_id,
                    version,
                    definition,
                })
                .await
                .map_err(|error| error.to_string())?;
            options.insert("definitions".to_string(), serde_json::json!([registered]));
            "workflow definition registered".to_string()
        }
        "workflow.run" => {
            let definition_id = required_arg(&request, "definition_id")?;
            let definition_version = parse_arg::<u32>(&request, "definition_version")?;
            let parent_session_id = parse_arg(&request, "session_id")?;
            let workspace_snapshot = required_arg(&request, "workspace_snapshot")?;
            let input = request
                .args
                .get("input")
                .map(|value| serde_json::from_str(value).map_err(|error| error.to_string()))
                .transpose()?;
            let started = client
                .start_workflow_run(bcode_ipc::WorkflowRunStartRequest {
                    definition_id,
                    definition_version,
                    run_id: None,
                    workspace_snapshot,
                    parent_session_id,
                    parent_session_generation: None,
                    binding: None,
                    input,
                    limits: bcode_workflow_store::WorkflowRunLimits::default(),
                })
                .await
                .map_err(|error| error.to_string())?;
            options.insert("run".to_string(), serde_json::json!(started.run));
            format!("started workflow run {}", started.run.run_id)
        }
        "workflow.pause" | "workflow.resume" | "workflow.cancel" => {
            let run_id = required_arg(&request, "run_id")?;
            let changed = match request.command_id.as_str() {
                "workflow.pause" => client.pause_workflow_run(run_id.clone()).await,
                "workflow.resume" => client.resume_workflow_run(run_id.clone()).await,
                _ => client.cancel_workflow_run(run_id.clone()).await,
            }
            .map_err(|error| error.to_string())?;
            options.insert("run_id".to_string(), serde_json::json!(run_id));
            format!("{} changed={changed}", request.command_id)
        }
        "workflow.inspect" => {
            let run_id = required_arg(&request, "run_id")?;
            let inspection = client
                .inspect_workflow_run(run_id.clone(), QUERY_LIMIT)
                .await
                .map_err(|error| error.to_string())?;
            options.extend([
                ("run".to_string(), serde_json::json!(inspection.run)),
                (
                    "definition".to_string(),
                    serde_json::json!(inspection.definition),
                ),
                (
                    "terminal_output".to_string(),
                    serde_json::json!(inspection.terminal_output),
                ),
                (
                    "activations".to_string(),
                    serde_json::json!(inspection.activations),
                ),
                ("waits".to_string(), serde_json::json!(inspection.waits)),
                (
                    "mutation_approvals".to_string(),
                    serde_json::json!(inspection.mutation_approvals),
                ),
                (
                    "attempts".to_string(),
                    serde_json::json!(inspection.attempts),
                ),
                ("events".to_string(), serde_json::json!(inspection.events)),
                (
                    "decisions".to_string(),
                    serde_json::json!(inspection.decisions),
                ),
                ("grants".to_string(), serde_json::json!(inspection.grants)),
                (
                    "resource_leases".to_string(),
                    serde_json::json!(inspection.resource_leases),
                ),
                ("outputs".to_string(), serde_json::json!(inspection.outputs)),
                (
                    "child_run_links".to_string(),
                    serde_json::json!(inspection.child_run_links),
                ),
                (
                    "descendant_runs".to_string(),
                    serde_json::json!(inspection.descendant_runs),
                ),
                (
                    "repeat_outcomes".to_string(),
                    serde_json::json!(inspection.repeat_outcomes),
                ),
                (
                    "child_sessions".to_string(),
                    serde_json::json!(inspection.child_sessions),
                ),
            ]);
            format!("workflow run {run_id}")
        }
        "workflow.doctor" => {
            let run_id = required_arg(&request, "run_id")?;
            let report = client
                .doctor_workflow_run(run_id, QUERY_LIMIT)
                .await
                .map_err(|error| error.to_string())?;
            options.insert("doctor".to_string(), serde_json::json!(report));
            "workflow doctor completed without mutation".to_string()
        }
        "workflow.repair" => {
            let dispatch_identity = required_arg(&request, "dispatch_identity")?;
            let resolution = serde_json::from_str(&required_arg(&request, "resolution")?)
                .map_err(|error| format!("invalid typed repair resolution: {error}"))?;
            let result = client
                .repair_workflow_attempt(dispatch_identity, resolution)
                .await
                .map_err(|error| error.to_string())?;
            options.insert("repair".to_string(), serde_json::json!(result));
            "workflow attempt repaired explicitly".to_string()
        }
        "workflow.retry-node" => {
            let run_id = required_arg(&request, "run_id")?;
            let node_id = required_arg(&request, "node_id")?;
            let activation_id = required_arg(&request, "activation_id")?;
            let failed_attempt = parse_arg::<u32>(&request, "failed_attempt")?;
            let result = client
                .retry_workflow_node(run_id, node_id, activation_id, failed_attempt)
                .await
                .map_err(|error| error.to_string())?;
            options.insert("retry".to_string(), serde_json::json!(result));
            "workflow node retry admitted".to_string()
        }
        "workflow.provide-input" => {
            let run_id = required_arg(&request, "run_id")?;
            let node_id = required_arg(&request, "node_id")?;
            let activation_id = required_arg(&request, "activation_id")?;
            let value = serde_json::from_str(&required_arg(&request, "value")?)
                .map_err(|error| error.to_string())?;
            let result = client
                .provide_workflow_input(run_id, node_id, activation_id, value)
                .await
                .map_err(|error| error.to_string())?;
            options.insert("resolution".to_string(), serde_json::json!(result));
            "workflow input recorded".to_string()
        }
        "workflow.approve-mutation" | "workflow.deny-mutation" => {
            let approval_id = required_arg(&request, "approval_id")?;
            let decision = if request.command_id == "workflow.approve-mutation" {
                bcode_workflow_store::WorkflowMutationApprovalDecision::Approve
            } else {
                bcode_workflow_store::WorkflowMutationApprovalDecision::Deny
            };
            let result = client
                .resolve_workflow_mutation_approval(approval_id, decision)
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "mutation_approval_resolution".to_string(),
                serde_json::json!(result),
            );
            format!("workflow mutation approval {decision:?}")
        }
        command => return Err(format!("unsupported workflow command '{command}'")),
    };
    Ok(InvokeCommandResponse {
        success: true,
        message: Some(message),
        updated_model: None,
        updated_provider: None,
        updated_thinking: None,
        effects: vec![CommandEffect::OpenPluginSurface {
            surface_kind: if matches!(
                request.command_id.as_str(),
                "workflow.template-describe" | "workflow.template-instantiate"
            ) {
                AUTHOR_SURFACE_KIND.to_string()
            } else {
                STATUS_SURFACE_KIND.to_string()
            },
            instance_id: if matches!(
                request.command_id.as_str(),
                "workflow.template-describe" | "workflow.template-instantiate"
            ) {
                "workflow-author".to_string()
            } else {
                "workflow-status".to_string()
            },
            options: serde_json::Value::Object(options),
        }],
    })
}

#[allow(clippy::too_many_lines)]
async fn describe_or_instantiate_template(
    request: &InvokeCommandRequest,
    client: &bcode_client::BcodeClient,
    options: &mut serde_json::Map<String, serde_json::Value>,
) -> Result<String, String> {
    let owner_plugin_id = required_arg(request, "owner_plugin_id")?;
    let template_id = required_arg(request, "template_id")?;
    let template_version = parse_arg::<u32>(request, "template_version")?;
    let template = client
        .describe_workflow_template(
            owner_plugin_id.clone(),
            template_id.clone(),
            template_version,
        )
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "workflow template not found or disabled".to_string())?;
    options.insert(
        "template".to_string(),
        serde_json::to_value(&template).map_err(|error| error.to_string())?,
    );
    match (
        request.args.get("workflow_id"),
        request.args.get("draft_id"),
    ) {
        (Some(workflow_id), Some(draft_id))
            if request.command_id == "workflow.template-instantiate" =>
        {
            let (_, draft) = client
                .instantiate_workflow_template(bcode_ipc::WorkflowTemplateInstantiationRequest {
                    owner_plugin_id,
                    template_id,
                    template_version,
                    workflow_id: workflow_id.clone(),
                    draft_id: draft_id.clone(),
                })
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "draft".to_string(),
                serde_json::to_value(draft).map_err(|error| error.to_string())?,
            );
        }
        (Some(workflow_id), Some(draft_id)) => {
            let draft = client
                .workflow_draft(workflow_id.clone(), draft_id.clone())
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("workflow draft not found: {workflow_id}/{draft_id}"))?;
            options.insert(
                "draft".to_string(),
                serde_json::to_value(draft).map_err(|error| error.to_string())?,
            );
        }
        (None, None) if request.command_id == "workflow.template-instantiate" => {
            let workflow_id = request
                .args
                .get("new_workflow_id")
                .cloned()
                .unwrap_or_else(|| generated_authored_workflow_id(&template_id));
            let draft_id = request
                .args
                .get("new_draft_id")
                .cloned()
                .unwrap_or_else(|| "draft-1".to_string());
            let (_, draft) = client
                .instantiate_workflow_template(bcode_ipc::WorkflowTemplateInstantiationRequest {
                    owner_plugin_id,
                    template_id,
                    template_version,
                    workflow_id,
                    draft_id,
                })
                .await
                .map_err(|error| error.to_string())?;
            options.insert(
                "draft".to_string(),
                serde_json::to_value(draft).map_err(|error| error.to_string())?,
            );
        }
        (None, None) => {}
        _ => {
            return Err(
                "workflow_id and draft_id must be provided together for mutable editing"
                    .to_string(),
            );
        }
    }
    if let Some(session_id) = request.args.get("session_id") {
        options.insert("session_id".to_string(), serde_json::json!(session_id));
    }
    if let Some(configuration) = request.args.get("configuration") {
        let configuration_schema = template.authoring_document.as_ref().map_or_else(
            || template.template.configuration_schema(),
            |document| &document.configuration_schema,
        );
        let configuration =
            validate_template_configuration(&configuration_schema.schema, configuration)?;
        options.insert("configuration".to_string(), configuration);
    }
    if request.command_id == "workflow.template-instantiate" {
        Ok("workflow template instantiated as a mutable graph draft".to_string())
    } else {
        Ok(format!(
            "workflow template {} v{}",
            template.template.template_id, template.template.template_version
        ))
    }
}

fn generated_authored_workflow_id(template_id: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{template_id}-{timestamp:x}-{sequence:x}")
}

fn session_status(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<bcode_plugin_sdk::SessionStatusRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error.to_string()),
    };
    let contribution = match run_async(active_session_status(request.session_id)) {
        Ok(contribution) => contribution,
        Err(error) => return ServiceResponse::error("workflow_status_failed", error),
    };
    json_response(&bcode_plugin_sdk::SessionStatusResponse { contribution })
}

async fn active_session_status(
    session_id: bcode_session_models::SessionId,
) -> Result<Option<bcode_plugin_sdk::SessionStatusContribution>, String> {
    let runs = bcode_client::BcodeClient::default_endpoint()
        .list_workflow_runs(QUERY_LIMIT)
        .await
        .map_err(|error| error.to_string())?;
    let session_id = session_id.to_string();
    let active = runs
        .into_iter()
        .filter(|run| {
            run.parent_session_id.as_deref() == Some(session_id.as_str())
                && matches!(
                    run.status,
                    bcode_workflow_store::RunStatus::Running
                        | bcode_workflow_store::RunStatus::Paused
                )
        })
        .collect::<Vec<_>>();
    if active.is_empty() {
        return Ok(None);
    }
    let paused = active
        .iter()
        .filter(|run| run.status == bcode_workflow_store::RunStatus::Paused)
        .count();
    let paused_suffix = if paused > 0 {
        format!(" · {paused} paused")
    } else {
        String::new()
    };
    Ok(Some(bcode_plugin_sdk::SessionStatusContribution {
        contribution_id: "workflow".to_string(),
        text: format!("Workflows · {} active{paused_suffix}", active.len()),
        priority: 40,
        metadata: BTreeMap::from([("runs".to_string(), serde_json::json!(active))]),
    }))
}

fn reference_template_run_limits(
    template_id: &str,
    configuration: &serde_json::Value,
) -> Result<bcode_workflow_store::WorkflowRunLimits, String> {
    let mut limits = bcode_workflow_store::WorkflowRunLimits::default();
    if template_id != "implementation-verification-commit" {
        return Ok(limits);
    }
    let iteration_limit = configuration
        .get("iteration_limit")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| "reference template iteration_limit is required".to_string())?;
    limits.cycle_cap = u32::try_from(iteration_limit)
        .map_err(|_| "reference template iteration_limit exceeds u32".to_string())?;
    limits.node_execution_cap = limits
        .cycle_cap
        .checked_mul(16)
        .ok_or_else(|| "reference template node execution limit overflow".to_string())?;
    Ok(limits)
}

fn parse_arguments(arguments: &str) -> BTreeMap<String, String> {
    let mut parsed = BTreeMap::new();
    let bytes = arguments.as_bytes();
    let mut start = 0;
    let mut depth = 0_u32;
    let mut quoted = false;
    let mut escaped = false;
    for (index, byte) in bytes.iter().copied().enumerate() {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
            continue;
        }
        match byte {
            b'"' => quoted = true,
            b'{' | b'[' => depth = depth.saturating_add(1),
            b'}' | b']' => depth = depth.saturating_sub(1),
            byte if byte.is_ascii_whitespace() && depth == 0 => {
                insert_argument(&mut parsed, &arguments[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    insert_argument(&mut parsed, &arguments[start..]);
    parsed
}

fn insert_argument(parsed: &mut BTreeMap<String, String>, argument: &str) {
    let argument = argument.trim();
    if let Some((key, value)) = argument.split_once('=')
        && !key.is_empty()
        && !value.is_empty()
    {
        parsed.insert(key.to_string(), value.to_string());
    }
}

fn validate_template_configuration(
    schema: &serde_json::Value,
    configuration: &str,
) -> Result<serde_json::Value, String> {
    if configuration.len() > 1_048_576 {
        return Err("template configuration exceeds 1048576 bytes".to_string());
    }
    let configuration = serde_json::from_str(configuration)
        .map_err(|error| format!("invalid template configuration JSON: {error}"))?;
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| format!("invalid template configuration schema: {error}"))?;
    if let Err(error) = validator.validate(&configuration) {
        return Err(format!("template configuration is invalid: {error}"));
    }
    Ok(configuration)
}

fn required_arg(request: &InvokeCommandRequest, name: &str) -> Result<String, String> {
    request
        .args
        .get(name)
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| format!("{} requires '{name}'", request.command_id))
}

fn parse_arg<T>(request: &InvokeCommandRequest, name: &str) -> Result<T, String>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    required_arg(request, name)?
        .parse()
        .map_err(|error| format!("invalid '{name}': {error}"))
}

fn run_async<F, T>(future: F) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, String>> + Send + 'static,
    T: Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?
            .block_on(future)
    })
    .join()
    .map_err(|_| "workflow plugin async worker panicked".to_string())?
}

fn json_response<T: serde::Serialize>(value: &T) -> ServiceResponse {
    ServiceResponse::json(value)
        .unwrap_or_else(|error| ServiceResponse::error("encode_failed", error.to_string()))
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    let mut vtable = bcode_plugin_sdk::static_plugin_vtable!(
        WorkflowPlugin,
        include_str!("../bcode-plugin.toml")
    );
    vtable.cli_registration = Some(cli::registration);
    vtable
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(WorkflowPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_driven_parent_pins_tranche_and_enforces_all_product_budgets() {
        let mut manifest: bcode_plugin::PluginManifest =
            toml::from_str(include_str!("../bcode-plugin.toml")).expect("manifest");
        bcode_plugin::resolve_external_workflow_templates(
            &mut manifest,
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
        )
        .expect("external flagship templates");
        let tranche = manifest
            .workflow_templates
            .iter()
            .find(|template| template.template_id == "delivery-tranche")
            .expect("tranche");
        let parent = manifest
            .workflow_templates
            .iter()
            .find(|template| template.template_id == "progress-driven-delivery")
            .expect("parent");
        let definition = parent.definition();
        let call: bcode_workflow::WorkflowCallConfiguration =
            serde_json::from_value(definition.nodes["delivery_tranche"].configuration.clone())
                .expect("exact tranche call");
        assert_eq!(
            call.target.definition_identity(),
            &tranche
                .definition_identity("bcode.workflow")
                .expect("tranche identity")
        );
        assert_eq!(
            definition.nodes["tranche_repeat"].configuration["max_iterations"],
            10
        );
        assert_eq!(
            definition.nodes["operator_continuation"].kind,
            bcode_workflow::NodeKind::Input
        );
        let presentation = parent
            .authoring_document()
            .and_then(|document| document.presentation.as_ref())
            .and_then(|presentation| presentation.namespaces.get("bcode.workflow"))
            .expect("product budgets");
        assert_eq!(presentation["derived_batch_limit"], 50);
        assert_eq!(presentation["derived_implementation_turn_limit"], 1_000);
        assert_eq!(presentation["descendant_limit"], 60);
        assert_eq!(presentation["platform_descendant_ceiling"], 64);
        assert_eq!(
            presentation["default_progress_path_pattern"],
            "local-<workflow-slug>-progress.md"
        );
        let setup: bcode_workflow::WorkflowAgentConfiguration =
            serde_json::from_value(definition.nodes["progress_setup"].configuration.clone())
                .expect("progress setup");
        assert_eq!(setup.skills[0].skill_id, "local-progress-doc");
    }

    #[test]
    fn delivery_tranche_pins_exact_batch_and_runtime_owns_five_batches() {
        let mut manifest: bcode_plugin::PluginManifest =
            toml::from_str(include_str!("../bcode-plugin.toml")).expect("manifest");
        bcode_plugin::resolve_external_workflow_templates(
            &mut manifest,
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
        )
        .expect("external flagship templates");
        let batch = manifest
            .workflow_templates
            .iter()
            .find(|template| template.template_id == "implementation-batch")
            .expect("batch");
        let tranche = manifest
            .workflow_templates
            .iter()
            .find(|template| template.template_id == "delivery-tranche")
            .expect("tranche");
        let definition = tranche.definition();
        let call: bcode_workflow::WorkflowCallConfiguration = serde_json::from_value(
            definition.nodes["implementation_batch"]
                .configuration
                .clone(),
        )
        .expect("exact call");
        let batch_identity = batch
            .definition_identity("bcode.workflow")
            .expect("batch identity");
        assert_eq!(call.target.definition_identity(), &batch_identity);
        let repeat = &definition.nodes["batch_repeat"];
        assert_eq!(repeat.configuration["max_iterations"], 5);
        assert_eq!(repeat.configuration["exhaustion_policy"], "emit_outcome");
        let refocus: bcode_workflow::WorkflowAgentConfiguration =
            serde_json::from_value(definition.nodes["refocus"].configuration.clone())
                .expect("refocus agent");
        assert_eq!(
            refocus.skills,
            [bcode_workflow::AgentSkillSelection {
                skill_id: "refocus-progress-doc".to_string(),
                mode: bcode_workflow::AgentSkillActivationMode::Required,
            }]
        );
        assert_eq!(
            tranche
                .authoring_document()
                .expect("tranche document")
                .run_limits
                .cycle_cap,
            5
        );
    }

    #[test]
    fn flagship_batch_template_has_runtime_owned_limit_and_product_outcomes() {
        let mut manifest: bcode_plugin::PluginManifest =
            toml::from_str(include_str!("../bcode-plugin.toml")).expect("manifest");
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        bcode_plugin::resolve_external_workflow_templates(&mut manifest, root)
            .expect("external flagship template");
        let template = manifest
            .workflow_templates
            .iter()
            .find(|template| template.template_id == "implementation-batch")
            .expect("implementation batch");
        let definition = template.definition();
        assert_eq!(definition.entries, ["implementation"]);
        assert_eq!(definition.exits, ["batch_repeat"]);
        assert!(definition.nodes.contains_key("implementation"));
        assert!(definition.nodes.contains_key("validation"));
        assert!(definition.nodes.contains_key("post_format_validation"));
        assert!(definition.nodes.contains_key("completion"));
        for node_id in [
            "snapshot_before",
            "snapshot_after",
            "format",
            "formatted_snapshot",
            "final_snapshot",
            "prepare_checkpoint",
        ] {
            assert!(definition.nodes.contains_key(node_id), "{node_id}");
        }
        assert_eq!(
            definition.nodes["snapshot_before"].configuration["block_id"],
            "git.repository-snapshot"
        );
        assert_eq!(
            definition.nodes["validation"].configuration["block_id"],
            "shell.command-plan"
        );
        assert_eq!(
            definition.nodes["pre_receipt"].configuration["block_id"],
            "git.verification-receipt"
        );
        assert_eq!(
            definition.nodes["post_receipt"].configuration["block_id"],
            "git.verification-receipt"
        );
        assert_eq!(
            definition.nodes["compose_commit"].configuration["block_id"],
            "git.compose-commit"
        );
        assert_eq!(
            definition.nodes["git_commit"].configuration["block_id"],
            "git.commit"
        );
        assert!(definition.nodes["git_commit"].configuration["authorization"]
            ["explicit_grant_required"]
            .as_bool()
            .unwrap_or(false));
        assert!(definition.nodes.contains_key("classify"));
        let repeat = &definition.nodes["batch_repeat"];
        assert_eq!(repeat.kind, bcode_workflow::NodeKind::Repeat);
        assert_eq!(
            repeat.configuration["max_iterations"],
            IMPLEMENTATION_BATCH_ITERATION_LIMIT
        );
        assert_eq!(repeat.configuration["exhaustion_policy"], "emit_outcome");
        assert!(definition.edges.iter().any(|edge| {
            edge.from == "validation"
                && edge.to == "classify"
                && matches!(
                    edge.kind,
                    bcode_workflow::EdgeKind::Conditional {
                        expected: false,
                        ..
                    }
                )
        }));
        assert!(definition.edges.iter().any(|edge| {
            edge.from == "post_format_validation"
                && edge.to == "classify"
                && matches!(
                    edge.kind,
                    bcode_workflow::EdgeKind::Conditional {
                        expected: false,
                        ..
                    }
                )
        }));
        assert!(definition.edges.iter().any(|edge| {
            edge.from == "batch_repeat"
                && edge.to == "implementation"
                && matches!(
                    edge.kind,
                    bcode_workflow::EdgeKind::Back {
                        max_iterations: IMPLEMENTATION_BATCH_ITERATION_LIMIT,
                        ..
                    }
                )
        }));
        template.validate().expect("valid flagship template");
    }

    #[test]
    fn composed_exhaustion_refocus_and_operator_continuation_are_exactly_bounded() {
        let mut manifest: bcode_plugin::PluginManifest =
            toml::from_str(include_str!("../bcode-plugin.toml")).expect("manifest");
        bcode_plugin::resolve_external_workflow_templates(
            &mut manifest,
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
        )
        .expect("external flagship templates");
        let tranche = manifest
            .workflow_templates
            .iter()
            .find(|template| template.template_id == "delivery-tranche")
            .expect("delivery tranche")
            .definition();
        let tranche_repeat = tranche
            .edges
            .iter()
            .find(|edge| edge.from == "batch_repeat" && edge.to == "implementation_batch")
            .expect("batch repeat edge");
        assert!(matches!(
            tranche_repeat.kind,
            bcode_workflow::EdgeKind::Back {
                max_iterations: DELIVERY_TRANCHE_BATCH_LIMIT,
                ..
            }
        ));
        let refocus: bcode_workflow::WorkflowAgentConfiguration =
            serde_json::from_value(tranche.nodes["refocus"].configuration.clone())
                .expect("refocus agent");
        assert_eq!(
            refocus.skills,
            [bcode_workflow::AgentSkillSelection {
                skill_id: "refocus-progress-doc".to_string(),
                mode: bcode_workflow::AgentSkillActivationMode::Required,
            }]
        );
        assert!(refocus.system_prompt.contains("Apply/Revise/Cancel"));

        let parent = manifest
            .workflow_templates
            .iter()
            .find(|template| template.template_id == "progress-driven-delivery")
            .expect("progress parent")
            .definition();
        let gate = &parent.nodes["operator_continuation"];
        assert_eq!(gate.kind, bcode_workflow::NodeKind::Input);
        assert_eq!(
            gate.configuration["choices"],
            serde_json::json!(["continue", "operator_stopped"])
        );
        let second_tranche = parent
            .edges
            .iter()
            .find(|edge| edge.from == "tranche_repeat" && edge.to == "grant_review")
            .expect("second tranche continuation");
        assert!(matches!(
            second_tranche.kind,
            bcode_workflow::EdgeKind::Back {
                max_iterations: PROGRESS_DRIVEN_TRANCHE_LIMIT,
                ..
            }
        ));
        let continued = serde_json::json!({
            "version": 1,
            "outcome": "continue",
            "state": {
                "version": 1,
                "objective": "ship",
                "implementation_prompt": "implement",
                "completion_condition": "done",
                "progress_document": {"path": "local-progress.md", "digest_sha256": "a".repeat(64)},
                "validation_plan": {},
                "formatting_plan": {},
                "instruction_fingerprint_sha256": "b".repeat(64),
                "path_policy": {"include": [], "exclude": ["local-progress.md"]},
                "phase": "exhausted",
                "latest": {
                    "implementation_summary": "later batch",
                    "repository_snapshots": [],
                    "verification_receipt": {"verified": true},
                    "formatting_receipt": {},
                    "prepared_change_set": {},
                    "commit_receipt": {},
                    "completion_assessment": {"condition_met": false}
                },
                "artifacts": []
            }
        });
        assert_eq!(
            second_tranche
                .transform
                .as_ref()
                .expect("state projection")
                .evaluate(&[bcode_workflow::WorkflowTransformInput {
                    name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT,
                    value: &continued,
                }])
                .expect("second tranche state"),
            continued["state"]
        );
    }

    #[test]
    fn formatter_result_starts_a_fresh_post_format_verification_context() {
        let state = serde_json::json!({
            "version": 1,
            "objective": "ship",
            "implementation_prompt": "implement",
            "completion_condition": "done",
            "progress_document": {"path": "local-progress.md", "digest_sha256": null},
            "validation_plan": {},
            "formatting_plan": {},
            "instruction_fingerprint_sha256": "a".repeat(64),
            "path_policy": {"include": [], "exclude": ["local-progress.md"]},
            "phase": "formatting",
            "latest": {
                "implementation_summary": null,
                "repository_snapshots": [],
                "verification_receipt": null,
                "formatting_receipt": null,
                "prepared_change_set": null,
                "commit_receipt": null,
                "completion_assessment": null
            },
            "artifacts": [],
            "operation_context": {
                "pre_snapshot": null,
                "post_snapshot": null,
                "command_result": null
            }
        });
        let mut state: CodingWorkflowState = serde_json::from_value(state).expect("state");
        retain_batch_operation_result(
            &mut state,
            BatchInputOperation::FormattingPlan,
            Some(serde_json::json!({
                "version": 1,
                "stage": "pre_format",
                "verified": true,
                "plan_sha256": "b".repeat(64),
                "instruction_fingerprint_sha256": "a".repeat(64),
                "repository_snapshot_sha256": "c".repeat(64)
            })),
        )
        .expect("retain receipt");
        assert_eq!(
            state.latest.verification_receipt.as_ref().unwrap()["stage"],
            "pre_format"
        );
        assert_eq!(
            state.operation_context,
            Some(CodingWorkflowOperationContext::default())
        );
    }

    #[test]
    fn batch_outcome_requires_deterministic_and_semantic_completion() {
        let state: CodingWorkflowState = serde_json::from_value(serde_json::json!({
            "version": 1,
            "objective": "ship",
            "implementation_prompt": "implement",
            "completion_condition": "done",
            "progress_document": {"path": "local-progress.md", "digest_sha256": null},
            "validation_plan": {},
            "formatting_plan": {},
            "instruction_fingerprint_sha256": "a".repeat(64),
            "path_policy": {"include": [], "exclude": ["local-progress.md"]},
            "phase": "evaluating",
            "latest": {
                "implementation_summary": "implemented",
                "repository_snapshots": [],
                "verification_receipt": {"verified": true},
                "formatting_receipt": {},
                "prepared_change_set": {},
                "commit_receipt": {},
                "completion_assessment": {"condition_met": true}
            },
            "artifacts": []
        }))
        .expect("coding state");
        let completed = batch_outcome(&BatchOutcomeRequest {
            version: 1,
            state: state.clone(),
        })
        .expect("completed");
        assert_eq!(completed.outcome, ImplementationBatchOutcome::Completed);
        let mut incomplete = state;
        incomplete.latest.completion_assessment = Some(serde_json::json!({
            "condition_met": false
        }));
        let exhausted = batch_outcome(&BatchOutcomeRequest {
            version: 1,
            state: incomplete,
        })
        .expect("exhausted");
        assert_eq!(exhausted.outcome, ImplementationBatchOutcome::Exhausted);
        let mut manifest: bcode_plugin::PluginManifest =
            toml::from_str(include_str!("../bcode-plugin.toml")).expect("manifest");
        bcode_plugin::resolve_external_workflow_templates(
            &mut manifest,
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
        )
        .expect("external flagship template");
        let definition = manifest
            .workflow_templates
            .iter()
            .find(|template| template.template_id == "implementation-batch")
            .expect("implementation batch")
            .definition();
        let repeat = definition
            .edges
            .iter()
            .find(|edge| edge.from == "batch_repeat" && edge.to == "implementation")
            .expect("semantic continuation edge");
        assert!(matches!(
            repeat.kind,
            bcode_workflow::EdgeKind::Back {
                max_iterations: IMPLEMENTATION_BATCH_ITERATION_LIMIT,
                ..
            }
        ));
        assert_eq!(
            repeat
                .transform
                .as_ref()
                .expect("state projection")
                .evaluate(&[bcode_workflow::WorkflowTransformInput {
                    name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT,
                    value: &serde_json::to_value(&exhausted).expect("exhausted value"),
                }])
                .expect("continued state"),
            serde_json::to_value(&exhausted.state).expect("state value")
        );
    }

    #[test]
    fn instruction_drift_blocks_until_exact_replacement_is_reviewed() {
        let request = InstructionDriftReviewRequest {
            version: 1,
            accepted_instruction_fingerprint_sha256: "a".repeat(64),
            current_instruction_fingerprint_sha256: "b".repeat(64),
            accepted_validation_plan_sha256: "c".repeat(64),
            proposed_validation_plan_sha256: "d".repeat(64),
            accepted_formatting_plan_sha256: "e".repeat(64),
            proposed_formatting_plan_sha256: "f".repeat(64),
            reviewed: false,
        };
        assert!(matches!(
            instruction_drift_receipt(&request).expect("blocked"),
            InstructionDriftReceipt::Blocked { .. }
        ));
        assert!(matches!(
            instruction_drift_receipt(&InstructionDriftReviewRequest {
                reviewed: true,
                ..request
            })
            .expect("reviewed"),
            InstructionDriftReceipt::ReviewedReplacement { .. }
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn manifest_contributes_bounded_reference_template_configuration() {
        let manifest: bcode_plugin::PluginManifest =
            toml::from_str(include_str!("../bcode-plugin.toml")).expect("manifest");
        let template = manifest
            .workflow_templates
            .iter()
            .find(|template| template.template_id == "implementation-verification-commit")
            .expect("reference template");
        template.validate().expect("template validates");
        assert_eq!(template.template_version, 1);
        assert_eq!(template.definition().entries, vec!["implementation"]);
        let implementation = &template.definition().nodes["implementation"];
        assert_eq!(implementation.kind, bcode_workflow::NodeKind::Agent);
        let configuration: bcode_workflow::WorkflowAgentConfiguration =
            serde_json::from_value(implementation.configuration.clone())
                .expect("typed implementation agent configuration");
        configuration
            .validate()
            .expect("implementation agent validates");
        assert_eq!(
            configuration.execution_target,
            bcode_workflow::AgentExecutionTarget::SharedParentSequential
        );
        assert_eq!(
            configuration.tool_capability,
            bcode_workflow::WorkflowToolCapability::Mutating
        );
        let evaluation = &template.definition().nodes["evaluation"];
        assert_eq!(evaluation.kind, bcode_workflow::NodeKind::Agent);
        let evaluation_configuration: bcode_workflow::WorkflowAgentConfiguration =
            serde_json::from_value(evaluation.configuration.clone())
                .expect("typed evaluation agent configuration");
        evaluation_configuration
            .validate()
            .expect("evaluation agent validates");
        assert!(evaluation_configuration.read_only);
        assert_eq!(
            evaluation_configuration.tool_capability,
            bcode_workflow::WorkflowToolCapability::ReadOnly
        );
        assert!(template.definition().edges.iter().any(|edge| {
            edge.from == "implementation"
                && edge.to == "evaluation"
                && edge.kind == bcode_workflow::EdgeKind::Direct
        }));
        let verification = &template.definition().nodes["verification"];
        assert_eq!(verification.kind, bcode_workflow::NodeKind::PluginBlock);
        let verification_block: bcode_workflow::WorkflowBlockDefinition =
            serde_json::from_value(verification.configuration.clone())
                .expect("typed shell block configuration");
        verification_block
            .validate()
            .expect("shell block validates");
        let shell_manifest: bcode_plugin::PluginManifest =
            toml::from_str(include_str!("../../shell-plugin/bcode-plugin.toml"))
                .expect("shell manifest");
        let declared_shell = shell_manifest
            .services
            .iter()
            .flat_map(|service| &service.workflow_blocks)
            .find(|block| block.block_id == "shell.command-plan")
            .expect("declared shell block");
        assert_eq!(verification_block.plugin_id, declared_shell.plugin_id);
        assert_eq!(verification_block.block_id, declared_shell.block_id);
        assert_eq!(verification_block.effect, declared_shell.effect);
        assert_eq!(
            verification_block.authorization,
            declared_shell.authorization
        );
        assert_eq!(verification_block.resources, declared_shell.resources);
        assert_eq!(verification_block.plugin_id, "bcode.shell");
        assert_eq!(verification_block.block_id, "shell.command-plan");
        assert!(verification_block.authorization.explicit_grant_required);
        let verification_edge = template
            .definition()
            .edges
            .iter()
            .find(|edge| edge.from == "evaluation" && edge.to == "verification")
            .expect("verification edge");
        let verification_transform = verification_edge
            .transform
            .as_ref()
            .expect("command-plan projection");
        let state = serde_json::json!({
            "command_plan": {
                "version": 1,
                "cwd": ".",
                "commands": [{
                    "argv": ["cargo", "test"],
                    "timeout_ms": 300_000,
                    "continue_on_nonzero": false
                }],
                "environment": {"inherit": true, "set": {}},
                "output": {"preview_bytes": 4096, "artifact_spill": true}
            }
        });
        assert_eq!(
            verification_transform
                .evaluate(&[bcode_workflow::WorkflowTransformInput {
                    name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_STATE,
                    value: &state,
                }])
                .expect("command plan projection"),
            state["command_plan"]
        );
        let branch = &template.definition().nodes["verification_decision"];
        assert_eq!(branch.kind, bcode_workflow::NodeKind::Branch);
        assert_eq!(branch.configuration["predicate"]["path"], "passed");
        let passed = serde_json::json!({"passed": true});
        let failed = serde_json::json!({"passed": false});
        let predicate: bcode_workflow::PredicateExpression =
            serde_json::from_value(branch.configuration["predicate"].clone())
                .expect("verification predicate");
        assert!(predicate.evaluate_value(&passed).expect("passed branch"));
        assert!(!predicate.evaluate_value(&failed).expect("failed branch"));
        let git_prepare = &template.definition().nodes["git_prepare"];
        assert_eq!(git_prepare.kind, bcode_workflow::NodeKind::PluginBlock);
        let git_prepare_block: bcode_workflow::WorkflowBlockDefinition =
            serde_json::from_value(git_prepare.configuration.clone())
                .expect("typed Git prepare block");
        git_prepare_block.validate().expect("Git prepare validates");
        let git_manifest: bcode_plugin::PluginManifest =
            toml::from_str(include_str!("../../git-plugin/bcode-plugin.toml"))
                .expect("Git manifest");
        let declared_prepare = git_manifest
            .services
            .iter()
            .flat_map(|service| &service.workflow_blocks)
            .find(|block| block.block_id == "git.prepare")
            .expect("declared Git prepare block");
        assert_eq!(git_prepare_block.block_id, declared_prepare.block_id);
        assert_eq!(
            git_prepare_block.block_version,
            declared_prepare.block_version
        );
        assert_eq!(git_prepare_block.plugin_id, declared_prepare.plugin_id);
        assert_eq!(git_prepare_block.operation, declared_prepare.operation);
        assert_eq!(git_prepare_block.block_id, "git.prepare");
        assert_eq!(
            git_prepare_block.effect,
            bcode_workflow::WorkflowBlockEffect::ReadOnly
        );
        assert!(!git_prepare_block.authorization.explicit_grant_required);
        let prepare_transform = template
            .definition()
            .edges
            .iter()
            .find(|edge| edge.from == "commit_policy" && edge.to == "git_prepare")
            .and_then(|edge| edge.transform.as_ref())
            .expect("Git prepare request transform");
        assert_eq!(
            prepare_transform.evaluate(&[]).expect("prepare request"),
            serde_json::json!({
                "include_prefixes": [],
                "exclude_prefixes": [],
                "project_instruction_fingerprint_sha256": "0".repeat(64),
                "max_paths": 10_000
            })
        );
        let git_compose = &template.definition().nodes["git_compose"];
        assert_eq!(git_compose.kind, bcode_workflow::NodeKind::PluginBlock);
        let git_compose_block: bcode_workflow::WorkflowBlockDefinition =
            serde_json::from_value(git_compose.configuration.clone())
                .expect("typed Git compose block");
        let declared_compose = git_manifest
            .services
            .iter()
            .flat_map(|service| &service.workflow_blocks)
            .find(|block| block.block_id == "git.compose-commit")
            .expect("declared Git compose block");
        assert_eq!(git_compose_block.plugin_id, declared_compose.plugin_id);
        assert_eq!(git_compose_block.block_id, declared_compose.block_id);
        assert_eq!(git_compose_block.effect, declared_compose.effect);
        assert_eq!(git_compose_block.resources, declared_compose.resources);
        let commit_message = &template.definition().nodes["commit_message"];
        assert_eq!(commit_message.kind, bcode_workflow::NodeKind::Agent);
        let commit_message_agent: bcode_workflow::WorkflowAgentConfiguration =
            serde_json::from_value(commit_message.configuration.clone())
                .expect("typed commit-message agent");
        assert!(commit_message_agent.read_only);
        assert_eq!(
            commit_message_agent.tool_capability,
            bcode_workflow::WorkflowToolCapability::ReadOnly
        );
        assert!(commit_message_agent.skills.is_empty());
        assert_eq!(template.compilation_bindings.len(), 1);
        let skill_binding = &template.compilation_bindings[0];
        assert_eq!(skill_binding.configuration_path, "commit_message_skill");
        assert_eq!(skill_binding.node_id, "commit_message");
        assert_eq!(
            skill_binding.skill_mode,
            bcode_workflow::AgentSkillActivationMode::Required
        );
        let compose_transform = template
            .definition()
            .edges
            .iter()
            .find(|edge| edge.from == "commit_message" && edge.to == "git_compose")
            .and_then(|edge| edge.transform.as_ref())
            .expect("skill-backed Git compose request transform");
        let preparation = serde_json::json!({
            "version": 1,
            "repository_root": "/repo",
            "expected_head": "abc",
            "paths": [{"path": "src/lib.rs", "status": "modified"}],
            "title": "Implement workflows",
            "description": "Add durable workflow support"
        });
        let composed = compose_transform
            .evaluate(&[bcode_workflow::WorkflowTransformInput {
                name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT,
                value: &preparation,
            }])
            .expect("compose request");
        assert_eq!(composed["preparation"]["repository_root"], "/repo");
        assert_eq!(composed["preparation"]["head"], "abc");
        assert_eq!(composed["message"]["title"], "Implement workflows");
        assert_eq!(
            composed["message"]["description"],
            "Add durable workflow support"
        );
        assert_eq!(composed["no_changes"], "no_op");
        let git_commit = &template.definition().nodes["git_commit"];
        assert_eq!(git_commit.kind, bcode_workflow::NodeKind::PluginBlock);
        let git_commit_block: bcode_workflow::WorkflowBlockDefinition =
            serde_json::from_value(git_commit.configuration.clone())
                .expect("typed Git commit block");
        let declared_commit = git_manifest
            .services
            .iter()
            .flat_map(|service| &service.workflow_blocks)
            .find(|block| block.block_id == "git.commit")
            .expect("declared Git commit block");
        assert_eq!(git_commit_block.plugin_id, declared_commit.plugin_id);
        assert_eq!(git_commit_block.block_id, declared_commit.block_id);
        assert_eq!(git_commit_block.effect, declared_commit.effect);
        assert_eq!(git_commit_block.resources, declared_commit.resources);
        assert!(git_commit_block.authorization.explicit_grant_required);
        assert_eq!(
            git_commit_block.reconciliation,
            bcode_workflow::WorkflowBlockReconciliation::RepairRequired
        );
        let commit_transform = template
            .definition()
            .edges
            .iter()
            .find(|edge| edge.from == "commit_decision" && edge.to == "git_commit")
            .and_then(|edge| edge.transform.as_ref())
            .expect("exact commit request projection");
        let ready = serde_json::json!({
            "status": "ready",
            "request": {
                "repo_path": "/repo",
                "expected_head": "abc",
                "expected_repository_identity_sha256": "1".repeat(64),
                "expected_snapshot_sha256": "2".repeat(64),
                "manifest": {},
                "title": "Implement workflows",
                "description": "",
                "paths": ["src/lib.rs"]
            }
        });
        assert_eq!(
            commit_transform
                .evaluate(&[bcode_workflow::WorkflowTransformInput {
                    name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT,
                    value: &ready,
                }])
                .expect("commit projection"),
            ready["request"]
        );
        let commit_result = &template.definition().nodes["commit_result"];
        assert_eq!(commit_result.kind, bcode_workflow::NodeKind::Repeat);
        assert_eq!(
            commit_result.input.type_name,
            template.configuration_schema().type_name
        );
        assert_eq!(
            commit_result.output.type_name,
            template.configuration_schema().type_name
        );
        let commit_result_edge = template
            .definition()
            .edges
            .iter()
            .find(|edge| edge.from == "git_commit" && edge.to == "commit_result")
            .expect("commit result merge edge");
        assert_eq!(commit_result_edge.kind, bcode_workflow::EdgeKind::Direct);
        let commit_result_transform = commit_result_edge
            .transform
            .as_ref()
            .expect("commit result state merge");
        let state = serde_json::json!({
            "version": 1,
            "implementation_prompt": "implement",
            "stop_condition": {"path": "condition_met", "equals": true},
            "iteration_limit": 2,
            "command_plan": {
                "version": 1,
                "cwd": ".",
                "commands": [{
                    "argv": ["true"],
                    "timeout_ms": 1000,
                    "continue_on_nonzero": false
                }],
                "environment": {"inherit": true, "set": {}},
                "output": {"preview_bytes": 1024, "artifact_spill": true}
            },
            "command_timeout_ms": 1000,
            "verification_policy": "require_pass",
            "commit_behavior": "required",
            "commit_message_skill": null,
            "commit_completed": false,
            "commit_result": null
        });
        let committed = serde_json::json!({
            "previous_head": "abc",
            "commit_hash": "def",
            "paths": ["src/lib.rs"]
        });
        let merged = commit_result_transform
            .evaluate(&[
                bcode_workflow::WorkflowTransformInput {
                    name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT,
                    value: &committed,
                },
                bcode_workflow::WorkflowTransformInput {
                    name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_STATE,
                    value: &state,
                },
            ])
            .expect("commit result merge");
        assert_eq!(merged["commit_completed"], true);
        assert_eq!(merged["commit_result"], committed);
        assert!(template.definition().edges.iter().any(|edge| {
            edge.from == "commit_result"
                && edge.to == "implementation"
                && matches!(edge.kind, bcode_workflow::EdgeKind::Back { .. })
        }));
        let repeat_edge = template
            .definition()
            .edges
            .iter()
            .find(|edge| edge.from == "commit_result" && edge.to == "implementation")
            .expect("commit repeat edge");
        let repeated = repeat_edge
            .transform
            .as_ref()
            .expect("commit repeat state projection")
            .evaluate(&[
                bcode_workflow::WorkflowTransformInput {
                    name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT,
                    value: &merged,
                },
                bcode_workflow::WorkflowTransformInput {
                    name: bcode_workflow::WORKFLOW_TRANSFORM_SOURCE_STATE,
                    value: &state,
                },
            ])
            .expect("next iteration state");
        assert_eq!(repeated["commit_completed"], true);
        assert_eq!(repeated["commit_result"], committed);
        let repeat = &template.definition().nodes["verification_repeat"];
        assert_eq!(repeat.kind, bcode_workflow::NodeKind::Repeat);
        assert_eq!(repeat.configuration["max_iterations"], 100);
        let back = template
            .definition()
            .edges
            .iter()
            .find(|edge| edge.from == "verification_repeat" && edge.to == "implementation")
            .expect("verification repeat back edge");
        assert!(matches!(
            &back.kind,
            bcode_workflow::EdgeKind::Back {
                max_iterations: 100,
                ..
            }
        ));
        assert!(back.transform.is_some());
        for node_id in [
            "implementation",
            "evaluation",
            "verification",
            "verification_decision",
            "verified",
            "commit_policy",
            "commit_disabled",
            "commit_disabled_repeat",
            "verification_failed",
            "verification_repeat",
            "git_prepare",
            "commit_message",
            "git_compose",
            "commit_decision",
            "git_commit",
            "commit_result",
            "no_changes",
        ] {
            assert!(
                template.definition().nodes.contains_key(node_id),
                "{node_id}"
            );
        }
        assert_eq!(
            template
                .definition()
                .exits
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "commit_disabled_repeat".to_string(),
                "no_changes".to_string(),
                "verification_repeat".to_string(),
            ])
        );
        let required = template.configuration_schema().schema["required"]
            .as_array()
            .expect("required fields");
        for field in [
            "implementation_prompt",
            "stop_condition",
            "iteration_limit",
            "command_plan",
            "command_timeout_ms",
            "verification_policy",
            "commit_behavior",
            "commit_message_skill",
        ] {
            assert!(required.iter().any(|value| value == field));
        }
        assert_eq!(template.required_plugins, ["bcode.shell", "bcode.git"]);
    }

    #[test]
    fn commands_cover_supported_lifecycle_surface() {
        let ids = command_contributions()
            .into_iter()
            .map(|command| command.id)
            .collect::<BTreeSet<_>>();
        for expected in [
            "workflow.list",
            "workflow.templates",
            "workflow.template-describe",
            "workflow.template-start",
            "workflow.register",
            "workflow.run",
            "workflow.status",
            "workflow.pause",
            "workflow.resume",
            "workflow.cancel",
            "workflow.inspect",
            "workflow.doctor",
            "workflow.repair",
            "workflow.retry-node",
            "workflow.provide-input",
            "workflow.approve-mutation",
            "workflow.deny-mutation",
        ] {
            assert!(ids.contains(expected));
        }
    }

    #[test]
    fn slash_arguments_parse_named_values() {
        assert_eq!(
            parse_arguments("run_id=run-1 definition_version=2"),
            BTreeMap::from([
                ("definition_version".to_string(), "2".to_string()),
                ("run_id".to_string(), "run-1".to_string()),
            ])
        );
    }

    #[test]
    fn slash_arguments_preserve_structured_json_with_spaces() {
        assert_eq!(
            parse_arguments(
                r#"template_id=reference configuration={"prompt":"implement safely", "commands":[["cargo","test"]]} session_id=session-1"#
            ),
            BTreeMap::from([
                (
                    "configuration".to_string(),
                    r#"{"prompt":"implement safely", "commands":[["cargo","test"]]}"#.to_string(),
                ),
                ("session_id".to_string(), "session-1".to_string()),
                ("template_id".to_string(), "reference".to_string()),
            ])
        );
    }

    #[test]
    fn reference_template_maps_iteration_limit_into_persisted_run_limits() {
        let limits = reference_template_run_limits(
            "implementation-verification-commit",
            &serde_json::json!({"iteration_limit": 7}),
        )
        .expect("limits");
        assert_eq!(limits.cycle_cap, 7);
        assert_eq!(limits.node_execution_cap, 112);
        assert!(
            reference_template_run_limits(
                "implementation-verification-commit",
                &serde_json::json!({})
            )
            .is_err()
        );
        assert_eq!(
            reference_template_run_limits("another-template", &serde_json::json!({}))
                .expect("default"),
            bcode_workflow_store::WorkflowRunLimits::default()
        );
    }

    #[test]
    fn template_configuration_is_locally_schema_validated_and_bounded() {
        let schema = serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["prompt", "max_iterations"],
            "properties": {
                "prompt": {"type": "string", "minLength": 1},
                "max_iterations": {"type": "integer", "minimum": 1, "maximum": 100}
            }
        });
        assert_eq!(
            validate_template_configuration(
                &schema,
                r#"{"prompt":"implement","max_iterations":3}"#
            )
            .expect("valid")["max_iterations"],
            3
        );
        assert!(
            validate_template_configuration(&schema, r#"{"prompt":"","max_iterations":0}"#)
                .is_err()
        );
        assert!(validate_template_configuration(&schema, "not-json").is_err());
        assert!(validate_template_configuration(&schema, &"x".repeat(1_048_577)).is_err());
    }

    #[test]
    fn required_arguments_fail_closed() {
        let request = InvokeCommandRequest {
            command_id: "workflow.run".to_string(),
            args: BTreeMap::new(),
        };
        assert_eq!(
            required_arg(&request, "definition_id").expect_err("missing argument"),
            "workflow.run requires 'definition_id'"
        );
    }
}
