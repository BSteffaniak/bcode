#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

#[cfg(feature = "static-bundled")]
mod git_tui;

use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID, ToolArtifact,
    ToolDefinition, ToolInvocationRequest, ToolInvocationResponse, ToolInvocationResult, ToolList,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use thiserror::Error;

const GIT_PLUGIN_ID: &str = "bcode.git";
const GIT_CLONE_REQUEST_SCHEMA: &str = "bcode.git.clone_request";
const GIT_CLONE_RESULT_SCHEMA: &str = "bcode.git.clone_result";
const REPOSITORY_SNAPSHOT_VERSION: u32 = 1;
const VERIFICATION_RECEIPT_VERSION: u32 = 1;
const MAX_SNAPSHOT_PATHS: usize = 10_000;
const MAX_SNAPSHOT_MANIFEST_BYTES: usize = 16 * 1024 * 1024;
const MAX_SNAPSHOT_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SYMLINK_TARGET_BYTES: usize = 16 * 1024;

/// Git access plugin.
#[derive(Default)]
pub struct GitPlugin;

impl RustPlugin for GitPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match context.request.interface_id.as_str() {
            TOOL_SERVICE_INTERFACE_ID => invoke_tool_service(&context),
            bcode_workflow::WORKFLOW_BLOCK_INTERFACE_ID => invoke_workflow_block(&context),
            _ => ServiceResponse::error(
                "unsupported_interface",
                "unsupported Git plugin service interface",
            ),
        }
    }
}

fn git_policy_preparation(
    request: &bcode_tool::ToolPreparationRequest,
    _definition: &ToolDefinition,
) -> Result<bcode_plugin_sdk::ToolPolicyPreparation, String> {
    let descriptor = git_preparation_descriptor(request)?;
    let operation = bcode_plugin_sdk::ToolPolicyOperation::Write {
        paths: vec![descriptor.destination.display().to_string()],
        category: "write".to_owned(),
    };
    Ok(
        bcode_plugin_sdk::ToolPolicyPreparation::new(true, operation)
            .with_identity(bcode_plugin_sdk::ToolPolicyIdentity {
                aliases: Vec::new(),
                compatibility_aliases: Vec::new(),
                capabilities: Vec::new(),
                permission_category: Some("write".to_string()),
            })
            .with_descriptor(serde_json::to_value(descriptor).map_err(|error| error.to_string())?),
    )
}

fn invoke_tool_service(context: &NativeServiceContext) -> ServiceResponse {
    let request = &context.request;
    match request.operation.as_str() {
        OP_LIST_TOOLS => list_tools(request),
        bcode_tool::OP_PREPARE_TOOL => prepare_tool_service_response(
            request,
            [clone_tool_definition(), github_clone_alias_definition()],
            git_policy_preparation,
        ),
        OP_INVOKE_TOOL => invoke_tool(context),
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported Git tool service operation",
        ),
    }
}

fn list_tools(request: &ServiceRequest) -> ServiceResponse {
    if let Err(error) = request.payload_json::<ListToolsRequest>() {
        return invalid_request(&error);
    }
    json_response(&ToolList {
        tools: vec![clone_tool_definition(), github_clone_alias_definition()],
    })
}

fn invoke_tool(context: &NativeServiceContext) -> ServiceResponse {
    let request = &context.request;
    let invocation = match request.payload_json::<ToolInvocationRequest>() {
        Ok(invocation) => invocation,
        Err(error) => return invalid_request(&error),
    };
    if context.cancellation.is_cancelled() {
        return json_response(&tool_error("git tool cancelled".to_string()));
    }
    let response = match invocation.name.as_str() {
        "git.clone" | "github.clone" => invoke_clone(context, &invocation),
        _ => ToolInvocationResponse {
            output: format!("unsupported Git tool: {}", invocation.name),
            is_error: true,
            content: Vec::new(),
            full_output: None,
            result: None,
        },
    };
    json_response(&response)
}

fn invoke_clone(
    context: &NativeServiceContext,
    invocation: &ToolInvocationRequest,
) -> ToolInvocationResponse {
    let request = match serde_json::from_value::<CloneRequest>(invocation.arguments.clone()) {
        Ok(request) => request,
        Err(error) => return tool_error(error.to_string()),
    };
    let mut presentation = PrimaryPresentationPublisher::with_limits_and_cancellation(
        context.events,
        &invocation.tool_call_id,
        GIT_PLUGIN_ID,
        GIT_CLONE_REQUEST_SCHEMA,
        1,
        bcode_tool::ToolPresentationRetention::RetainLatest,
        context.transient_progress_limits,
        context.cancellation.clone(),
    );
    let _ = presentation.replace(&request);
    let descriptor = match serde_json::from_value::<GitPreparationDescriptor>(
        invocation.preparation_descriptor.clone(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) => return tool_error(format!("invalid Git preparation descriptor: {error}")),
    };
    match clone_repository(&request, &descriptor) {
        Ok(response) => json_tool_response_with_artifact(
            &response,
            &invocation.tool_call_id,
            "clone",
            GIT_CLONE_RESULT_SCHEMA,
            "Repository cloned",
        ),
        Err(error) => tool_error(error.to_string()),
    }
}

#[derive(Debug, Clone, Deserialize)]
struct GitWorkspaceContext {
    working_directory: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
struct GitArtifactContext {
    root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct GitPreparationDescriptor {
    destination: PathBuf,
    artifact_scope: String,
}

fn git_preparation_descriptor(
    request: &bcode_tool::ToolPreparationRequest,
) -> Result<GitPreparationDescriptor, String> {
    let clone: CloneRequest = serde_json::from_value(request.invocation.arguments.clone())
        .map_err(|error| format!("invalid Git clone request: {error}"))?;
    let workspace = decode_owner_context::<GitWorkspaceContext>(
        &request.host_context,
        bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA,
        bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA_VERSION,
        true,
    )?;
    if !workspace.working_directory.is_absolute() {
        return Err("workspace working directory must be absolute".to_owned());
    }
    let artifact = decode_optional_owner_context::<GitArtifactContext>(
        &request.host_context,
        bcode_tool::TOOL_ARTIFACT_CONTEXT_SCHEMA,
        bcode_tool::TOOL_ARTIFACT_CONTEXT_SCHEMA_VERSION,
    )?;
    if artifact
        .as_ref()
        .is_some_and(|context| !context.root.is_absolute())
    {
        return Err("artifact root must be absolute".to_owned());
    }
    let remote = parse_git_remote(&clone.url).map_err(|error| error.to_string())?;
    let (destination, artifact_scope) = clone.destination.as_ref().map_or_else(
        || {
            let root = artifact
                .as_ref()
                .map_or_else(default_global_artifact_dir, |context| context.root.clone());
            (default_destination(&root, &remote), "session".to_owned())
        },
        |destination| {
            let destination = if destination.is_absolute() {
                destination.clone()
            } else {
                workspace.working_directory.join(destination)
            };
            (destination, "explicit".to_owned())
        },
    );
    Ok(GitPreparationDescriptor {
        destination,
        artifact_scope,
    })
}

fn decode_owner_context<T: serde::de::DeserializeOwned>(
    entries: &[bcode_tool::ToolHostContextEntry],
    schema: &str,
    version: u32,
    required: bool,
) -> Result<T, String> {
    decode_optional_owner_context(entries, schema, version)?.ok_or_else(|| {
        if required {
            format!("required host context {schema}@{version} is missing")
        } else {
            format!("host context {schema}@{version} is missing")
        }
    })
}

fn decode_optional_owner_context<T: serde::de::DeserializeOwned>(
    entries: &[bcode_tool::ToolHostContextEntry],
    schema: &str,
    version: u32,
) -> Result<Option<T>, String> {
    let mut matching = entries.iter().filter(|entry| entry.schema == schema);
    let Some(entry) = matching.next() else {
        return Ok(None);
    };
    if matching.next().is_some() {
        return Err(format!("duplicate host context for {schema}"));
    }
    if entry.schema_version != version {
        return Err(format!(
            "unsupported host context version for {schema}: {}; expected {version}",
            entry.schema_version
        ));
    }
    serde_json::from_value(entry.payload.clone())
        .map(Some)
        .map_err(|error| format!("invalid host context {schema}@{version}: {error}"))
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepositorySnapshotRequest {
    pub version: u32,
    #[serde(default)]
    pub include_prefixes: Vec<PathBuf>,
    #[serde(default)]
    pub exclude_prefixes: Vec<PathBuf>,
    #[serde(default)]
    pub progress_document_path: Option<PathBuf>,
    pub max_paths: u32,
    pub project_instruction_fingerprint_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryEntryKind {
    File,
    Symlink,
    Submodule,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepositorySnapshotEntry {
    pub path: PathBuf,
    pub index_status: char,
    pub worktree_status: char,
    pub kind: RepositoryEntryKind,
    pub base_mode: String,
    pub index_mode: String,
    pub worktree_mode: String,
    pub base_object_id: Option<String>,
    pub index_object_id: Option<String>,
    pub worktree_sha256: Option<String>,
    #[serde(default)]
    pub worktree_object_id: Option<String>,
    #[serde(default)]
    pub source_path: Option<PathBuf>,
    #[serde(default)]
    pub symlink_target_sha256: Option<String>,
    #[serde(default)]
    pub submodule_state: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepositorySnapshot {
    pub version: u32,
    pub repository_root: PathBuf,
    pub repository_identity_sha256: String,
    pub head_object_id: String,
    pub include_prefixes: Vec<PathBuf>,
    pub exclude_prefixes: Vec<PathBuf>,
    pub entries: Vec<RepositorySnapshotEntry>,
    pub project_instruction_fingerprint_sha256: String,
    pub aggregate_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStage {
    PreFormat,
    PostFormat,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationReceiptRequest {
    pub version: u32,
    pub stage: VerificationStage,
    pub plan_sha256: String,
    pub instruction_fingerprint_sha256: String,
    pub pre_snapshot: RepositorySnapshot,
    pub post_snapshot: RepositorySnapshot,
    pub commands_passed: bool,
    pub required_artifacts_complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationReceipt {
    pub version: u32,
    pub stage: VerificationStage,
    pub verified: bool,
    pub plan_sha256: String,
    pub instruction_fingerprint_sha256: String,
    pub repository_snapshot_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerifiedCheckpointManifest {
    pub version: u32,
    pub repository_identity_sha256: String,
    pub repository_snapshot_sha256: String,
    pub head_object_id: String,
    pub entries: Vec<RepositorySnapshotEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommitRequest {
    pub repo_path: PathBuf,
    pub expected_head: String,
    pub expected_repository_identity_sha256: String,
    pub expected_snapshot_sha256: String,
    pub manifest: VerifiedCheckpointManifest,
    pub title: String,
    pub description: String,
    pub paths: Vec<PathBuf>,
}

const MAX_COMMIT_MESSAGE_BYTES: usize = 8_192;
const MAX_COMMIT_PATHS: usize = 10_000;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ComposeCommitRequest {
    pub preparation: PrepareResponse,
    pub message: ProposedCommitMessage,
    pub no_changes: NoChangesDecision,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProposedCommitMessage {
    pub title: String,
    pub description: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NoChangesDecision {
    Fail,
    NoOp,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ComposedCommitRequest {
    Ready { request: Box<CommitRequest> },
    NoChanges,
}

impl ComposedCommitRequest {
    #[must_use]
    pub fn request(&self) -> Option<&CommitRequest> {
        match self {
            Self::Ready { request } => Some(request),
            Self::NoChanges => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CommitResponse {
    pub previous_head: String,
    pub commit_hash: String,
    pub repository_snapshot_sha256: String,
    pub committed_tree: String,
    pub paths: Vec<PathBuf>,
    pub committed_objects: Vec<CommittedObjectIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CommittedObjectIdentity {
    pub path: PathBuf,
    pub object_id: String,
    pub mode: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CommitStatusRequest {
    repo_path: PathBuf,
    expected_head: String,
    paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CommitReconciliationOutcome {
    NotCommitted,
    CandidateCommit,
    Diverged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct CommitStatusResponse {
    expected_head: String,
    actual_head: String,
    outcome: CommitReconciliationOutcome,
    actual_commit_paths: Vec<PathBuf>,
    guidance: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareRequest {
    #[serde(default)]
    pub include_prefixes: Vec<PathBuf>,
    #[serde(default)]
    pub exclude_prefixes: Vec<PathBuf>,
    #[serde(default)]
    pub progress_document_path: Option<PathBuf>,
    #[serde(default = "zero_sha256")]
    pub project_instruction_fingerprint_sha256: String,
    pub max_paths: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PreparedChangedPath {
    pub path: PathBuf,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PrepareResponse {
    pub repository_root: PathBuf,
    pub head: String,
    pub repository_identity_sha256: String,
    pub repository_snapshot_sha256: String,
    pub manifest: VerifiedCheckpointManifest,
    pub changed_paths: Vec<PreparedChangedPath>,
}

fn zero_sha256() -> String {
    "0".repeat(64)
}

fn invoke_workflow_block(context: &NativeServiceContext) -> ServiceResponse {
    if context.request.operation == "git.repository-snapshot" {
        return repository_snapshot_workflow(context);
    }
    if context.request.operation == "git.verification-receipt" {
        return verification_receipt_workflow(context);
    }
    if context.request.operation == "git.prepare" {
        return prepare_workflow_repository(context);
    }
    if context.request.operation == "git.commit-status" {
        return commit_status_workflow(context);
    }
    if context.request.operation == "git.compose-commit" {
        return compose_workflow_commit(context);
    }
    if context.request.operation != "git.commit" {
        return ServiceResponse::error(
            "unsupported_operation",
            "unsupported Git workflow block operation",
        );
    }
    if context.cancellation.is_cancelled() {
        return ServiceResponse::error("cancelled", "Git commit cancelled");
    }
    let invocation = match context
        .request
        .payload_json::<bcode_workflow::WorkflowBlockInvocation>()
    {
        Ok(invocation) => invocation,
        Err(error) => return invalid_request(&error),
    };
    let request = match invocation.typed_input::<CommitRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error),
    };
    match commit_repository(&request) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("commit_failed", error.to_string()),
    }
}

fn commit_status_workflow(context: &NativeServiceContext) -> ServiceResponse {
    let invocation = match context
        .request
        .payload_json::<bcode_workflow::WorkflowBlockInvocation>()
    {
        Ok(invocation) => invocation,
        Err(error) => return invalid_request(&error),
    };
    let request = match invocation.typed_input::<CommitStatusRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error),
    };
    match commit_status(&request) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("commit_status_failed", error.to_string()),
    }
}

fn commit_status(request: &CommitStatusRequest) -> Result<CommitStatusResponse, GitError> {
    let repo = request.repo_path.canonicalize()?;
    if request.expected_head.trim().is_empty() || request.paths.is_empty() {
        return Err(GitError::InvalidRequest(
            "commit status requires expected HEAD and exact paths".to_string(),
        ));
    }
    let expected_paths = normalize_commit_paths(&request.paths)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let actual_head = git_stdout(&repo, ["rev-parse", "HEAD"])?;
    if actual_head == request.expected_head {
        return Ok(CommitStatusResponse {
            expected_head: request.expected_head.clone(),
            actual_head,
            outcome: CommitReconciliationOutcome::NotCommitted,
            actual_commit_paths: Vec::new(),
            guidance: "HEAD has not advanced; verify owner acceptance before retrying".to_string(),
        });
    }
    let parent = git_stdout(&repo, ["rev-parse", "HEAD^"])?;
    let actual_commit_paths = git_stdout(
        &repo,
        ["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
    )?
    .lines()
    .map(PathBuf::from)
    .collect::<Vec<_>>();
    let actual_paths = actual_commit_paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<BTreeSet<_>>();
    let (outcome, guidance) = if parent == request.expected_head && actual_paths == expected_paths {
        (
            CommitReconciliationOutcome::CandidateCommit,
            "HEAD advanced exactly once from expected HEAD with the requested paths; verify commit identity before recording success".to_string(),
        )
    } else {
        (
            CommitReconciliationOutcome::Diverged,
            "repository evidence is inconsistent with the expected commit; explicit repair is required".to_string(),
        )
    };
    Ok(CommitStatusResponse {
        expected_head: request.expected_head.clone(),
        actual_head,
        outcome,
        actual_commit_paths,
        guidance,
    })
}

fn repository_snapshot_workflow(context: &NativeServiceContext) -> ServiceResponse {
    if context.cancellation.is_cancelled() {
        return ServiceResponse::error("cancelled", "Git repository snapshot cancelled");
    }
    let invocation = match context
        .request
        .payload_json::<bcode_workflow::WorkflowBlockInvocation>()
    {
        Ok(invocation) => invocation,
        Err(error) => return invalid_request(&error),
    };
    let request = match invocation.typed_input::<RepositorySnapshotRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error),
    };
    match repository_snapshot(&invocation.workspace_root, &request) {
        Ok(snapshot) => json_response(&snapshot),
        Err(GitError::ScopeTooLarge) => {
            ServiceResponse::error("scope_too_large", "repository snapshot scope is too large")
        }
        Err(error) => ServiceResponse::error("snapshot_failed", error.to_string()),
    }
}

fn verification_receipt_workflow(context: &NativeServiceContext) -> ServiceResponse {
    let invocation = match context
        .request
        .payload_json::<bcode_workflow::WorkflowBlockInvocation>()
    {
        Ok(invocation) => invocation,
        Err(error) => return invalid_request(&error),
    };
    let request = match invocation.typed_input::<VerificationReceiptRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error),
    };
    match build_verification_receipt(&request) {
        Ok(receipt) => json_response(&receipt),
        Err(error) => ServiceResponse::error("verification_failed", error.to_string()),
    }
}

fn compose_workflow_commit(context: &NativeServiceContext) -> ServiceResponse {
    let invocation = match context
        .request
        .payload_json::<bcode_workflow::WorkflowBlockInvocation>()
    {
        Ok(invocation) => invocation,
        Err(error) => return invalid_request(&error),
    };
    let request = match invocation.typed_input::<ComposeCommitRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error),
    };
    match compose_commit_request(request) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("compose_failed", error.to_string()),
    }
}

fn compose_commit_request(
    request: ComposeCommitRequest,
) -> Result<ComposedCommitRequest, GitError> {
    let _message = normalize_commit_message(&request.message.title, &request.message.description)?;
    if request.preparation.head.trim().is_empty()
        || request.preparation.head.len() > 256
        || request.preparation.changed_paths.len() > MAX_COMMIT_PATHS
    {
        return Err(GitError::InvalidRequest(
            "commit request exceeds message, HEAD, or path bounds".to_string(),
        ));
    }
    if request.preparation.changed_paths.is_empty() {
        return match request.no_changes {
            NoChangesDecision::NoOp => Ok(ComposedCommitRequest::NoChanges),
            NoChangesDecision::Fail => Err(GitError::InvalidRequest(
                "Git preparation found no changes and policy requires failure".to_string(),
            )),
        };
    }
    let mut unique = BTreeSet::new();
    for changed in request.preparation.changed_paths {
        let path = changed.path;
        if path.is_absolute()
            || path.as_os_str().is_empty()
            || path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
            || !unique.insert(path.clone())
        {
            return Err(GitError::InvalidRequest(
                "prepared commit paths must be unique bounded repository-relative paths"
                    .to_string(),
            ));
        }
    }
    Ok(ComposedCommitRequest::Ready {
        request: Box::new(checkpoint_request(
            &RepositorySnapshot {
                version: request.preparation.manifest.version,
                repository_root: request.preparation.repository_root,
                repository_identity_sha256: request.preparation.repository_identity_sha256,
                head_object_id: request.preparation.head,
                include_prefixes: Vec::new(),
                exclude_prefixes: Vec::new(),
                entries: request.preparation.manifest.entries,
                project_instruction_fingerprint_sha256: zero_sha256(),
                aggregate_sha256: request.preparation.repository_snapshot_sha256,
            },
            request.message.title,
            request.message.description,
        )),
    })
}

fn prepare_workflow_repository(context: &NativeServiceContext) -> ServiceResponse {
    let invocation = match context
        .request
        .payload_json::<bcode_workflow::WorkflowBlockInvocation>()
    {
        Ok(invocation) => invocation,
        Err(error) => return invalid_request(&error),
    };
    let request = match invocation.typed_input::<PrepareRequest>() {
        Ok(request) => request,
        Err(error) => return ServiceResponse::error("invalid_request", error),
    };
    match prepare_repository(&invocation.workspace_root, &request) {
        Ok(response) => json_response(&response),
        Err(error) => ServiceResponse::error("prepare_failed", error.to_string()),
    }
}

fn repository_snapshot(
    root: &Path,
    request: &RepositorySnapshotRequest,
) -> Result<RepositorySnapshot, GitError> {
    if request.version != REPOSITORY_SNAPSHOT_VERSION
        || request.max_paths == 0
        || usize::try_from(request.max_paths).unwrap_or(usize::MAX) > MAX_SNAPSHOT_PATHS
    {
        return Err(GitError::InvalidRequest(
            "repository snapshot version or path bound is invalid".to_string(),
        ));
    }
    validate_sha256(&request.project_instruction_fingerprint_sha256)?;
    let repo = root.canonicalize()?;
    let repository_root = PathBuf::from(git_stdout(&repo, ["rev-parse", "--show-toplevel"])?);
    let repository_root = repository_root.canonicalize()?;
    if !repo.starts_with(&repository_root) {
        return Err(GitError::InvalidRequest(
            "workflow workspace is outside the resolved repository".to_string(),
        ));
    }
    let include_prefixes = normalize_path_prefixes(&request.include_prefixes)?;
    let mut exclude_prefixes = normalize_path_prefixes(&request.exclude_prefixes)?;
    if let Some(progress_document_path) = &request.progress_document_path {
        exclude_prefixes.extend(normalize_path_prefixes(std::slice::from_ref(
            progress_document_path,
        ))?);
        exclude_prefixes.sort();
        exclude_prefixes.dedup();
    }
    let status = Command::new("git")
        .current_dir(&repository_root)
        .args([
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ])
        .output()?;
    if !status.status.success() {
        return Err(GitError::CloneFailed {
            status: status.status.to_string(),
            stderr: String::from_utf8_lossy(&status.stderr).trim().to_string(),
        });
    }
    let mut entries = parse_porcelain_v2_entries(
        &repository_root,
        &status.stdout,
        &include_prefixes,
        &exclude_prefixes,
        usize::try_from(request.max_paths).unwrap_or(usize::MAX),
    )?;
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    let head_object_id = git_stdout(&repository_root, ["rev-parse", "HEAD"])?;
    validate_object_id(&head_object_id)?;
    let git_common_dir = PathBuf::from(git_stdout(
        &repository_root,
        ["rev-parse", "--git-common-dir"],
    )?);
    let git_common_dir = if git_common_dir.is_absolute() {
        git_common_dir
    } else {
        repository_root.join(git_common_dir)
    }
    .canonicalize()?;
    let repository_identity_sha256 = sha256_hex(git_common_dir.as_os_str().as_encoded_bytes());
    let mut snapshot = RepositorySnapshot {
        version: REPOSITORY_SNAPSHOT_VERSION,
        repository_root,
        repository_identity_sha256,
        head_object_id,
        include_prefixes,
        exclude_prefixes,
        entries,
        project_instruction_fingerprint_sha256: request
            .project_instruction_fingerprint_sha256
            .clone(),
        aggregate_sha256: String::new(),
    };
    let canonical = serde_json::to_vec(&snapshot)?;
    if canonical.len() > MAX_SNAPSHOT_MANIFEST_BYTES {
        return Err(GitError::ScopeTooLarge);
    }
    snapshot.aggregate_sha256 = sha256_hex(&canonical);
    Ok(snapshot)
}

fn parse_porcelain_v2_entries(
    repository_root: &Path,
    bytes: &[u8],
    include: &[PathBuf],
    exclude: &[PathBuf],
    max_paths: usize,
) -> Result<Vec<RepositorySnapshotEntry>, GitError> {
    let fields = bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    let mut entries = Vec::new();
    let mut index = 0_usize;
    while index < fields.len() {
        let record = std::str::from_utf8(fields[index]).map_err(|_| {
            GitError::InvalidRequest("Git status contains a non-portable path".to_string())
        })?;
        let kind = record.as_bytes().first().copied();
        let (entry, consumed) = match kind {
            Some(b'1') => (parse_ordinary_entry(repository_root, record)?, 1),
            Some(b'2') => {
                let source = fields.get(index + 1).ok_or_else(|| {
                    GitError::InvalidRequest("Git rename/copy record is incomplete".to_string())
                })?;
                let source = std::str::from_utf8(source).map_err(|_| {
                    GitError::InvalidRequest("Git status contains a non-portable path".to_string())
                })?;
                (parse_rename_entry(repository_root, record, source)?, 2)
            }
            Some(b'u') => {
                return Err(GitError::InvalidRequest(
                    "unmerged Git entries are unsupported by repository snapshot v1".to_string(),
                ));
            }
            Some(b'?') => (parse_untracked_entry(repository_root, record)?, 1),
            Some(b'!') => {
                index += 1;
                continue;
            }
            _ => {
                return Err(GitError::InvalidRequest(
                    "Git status returned an unsupported porcelain-v2 record".to_string(),
                ));
            }
        };
        index += consumed;
        if !include.is_empty() && !include.iter().any(|prefix| entry.path.starts_with(prefix)) {
            continue;
        }
        if exclude.iter().any(|prefix| entry.path.starts_with(prefix)) {
            continue;
        }
        entries.push(entry);
        if entries.len() > max_paths {
            return Err(GitError::ScopeTooLarge);
        }
    }
    Ok(entries)
}

fn parse_ordinary_entry(
    repository_root: &Path,
    record: &str,
) -> Result<RepositorySnapshotEntry, GitError> {
    let fields = record.splitn(9, ' ').collect::<Vec<_>>();
    if fields.len() != 9 {
        return Err(GitError::InvalidRequest(
            "Git ordinary status record is malformed".to_string(),
        ));
    }
    snapshot_entry(
        repository_root,
        fields[8],
        fields[1],
        fields[2],
        fields[3],
        fields[4],
        fields[5],
        fields[6],
        fields[7],
        None,
    )
}

fn parse_rename_entry(
    repository_root: &Path,
    record: &str,
    source: &str,
) -> Result<RepositorySnapshotEntry, GitError> {
    let fields = record.splitn(10, ' ').collect::<Vec<_>>();
    if fields.len() != 10 {
        return Err(GitError::InvalidRequest(
            "Git rename/copy status record is malformed".to_string(),
        ));
    }
    let source = normalize_status_path(source)?;
    snapshot_entry(
        repository_root,
        fields[9],
        fields[1],
        fields[2],
        fields[3],
        fields[4],
        fields[5],
        fields[6],
        fields[7],
        Some(source),
    )
}

#[allow(clippy::too_many_arguments)]
fn snapshot_entry(
    repository_root: &Path,
    path: &str,
    status: &str,
    submodule: &str,
    base_mode: &str,
    index_mode: &str,
    worktree_mode: &str,
    base_object_id: &str,
    index_object_id: &str,
    source_path: Option<PathBuf>,
) -> Result<RepositorySnapshotEntry, GitError> {
    let path = normalize_status_path(path)?;
    let mut statuses = status.chars();
    let index_status = statuses
        .next()
        .ok_or_else(|| GitError::InvalidRequest("Git status pair is malformed".to_string()))?;
    let worktree_status = statuses
        .next()
        .ok_or_else(|| GitError::InvalidRequest("Git status pair is malformed".to_string()))?;
    if statuses.next().is_some() {
        return Err(GitError::InvalidRequest(
            "Git status pair is malformed".to_string(),
        ));
    }
    let kind = if worktree_status == 'D' || index_status == 'D' || worktree_mode == "000000" {
        RepositoryEntryKind::Deleted
    } else if submodule.starts_with('S') {
        RepositoryEntryKind::Submodule
    } else if worktree_mode == "120000" || index_mode == "120000" {
        RepositoryEntryKind::Symlink
    } else {
        RepositoryEntryKind::File
    };
    let (worktree_sha256, symlink_target_sha256) = worktree_identity(repository_root, &path, kind)?;
    let worktree_object_id = worktree_git_object_id(repository_root, &path)?;
    Ok(RepositorySnapshotEntry {
        path,
        index_status,
        worktree_status,
        kind,
        base_mode: base_mode.to_string(),
        index_mode: index_mode.to_string(),
        worktree_mode: worktree_mode.to_string(),
        base_object_id: normalized_object_id(base_object_id)?,
        index_object_id: normalized_object_id(index_object_id)?,
        worktree_sha256,
        worktree_object_id,
        source_path,
        symlink_target_sha256,
        submodule_state: submodule.starts_with('S').then(|| submodule.to_string()),
    })
}

fn parse_untracked_entry(
    repository_root: &Path,
    record: &str,
) -> Result<RepositorySnapshotEntry, GitError> {
    let path = record.strip_prefix("? ").ok_or_else(|| {
        GitError::InvalidRequest("Git untracked status record is malformed".to_string())
    })?;
    let path = normalize_status_path(path)?;
    let metadata = fs::symlink_metadata(repository_root.join(&path))?;
    let kind = if metadata.file_type().is_symlink() {
        RepositoryEntryKind::Symlink
    } else if metadata.is_file() {
        RepositoryEntryKind::File
    } else {
        return Err(GitError::InvalidRequest(
            "unsupported untracked repository entry kind".to_string(),
        ));
    };
    let (worktree_sha256, symlink_target_sha256) = worktree_identity(repository_root, &path, kind)?;
    let worktree_object_id = worktree_git_object_id(repository_root, &path)?;
    Ok(RepositorySnapshotEntry {
        path,
        index_status: '?',
        worktree_status: '?',
        kind,
        base_mode: "000000".to_string(),
        index_mode: "000000".to_string(),
        worktree_mode: if kind == RepositoryEntryKind::Symlink {
            "120000"
        } else {
            "100644"
        }
        .to_string(),
        base_object_id: None,
        index_object_id: None,
        worktree_sha256,
        worktree_object_id,
        source_path: None,
        symlink_target_sha256,
        submodule_state: None,
    })
}

fn worktree_git_object_id(repository_root: &Path, path: &Path) -> Result<Option<String>, GitError> {
    let output = Command::new("git")
        .current_dir(repository_root)
        .arg("hash-object")
        .arg("--")
        .arg(path)
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let object_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    validate_object_id(&object_id)?;
    Ok(Some(object_id))
}

fn worktree_identity(
    repository_root: &Path,
    path: &Path,
    kind: RepositoryEntryKind,
) -> Result<(Option<String>, Option<String>), GitError> {
    let absolute = repository_root.join(path);
    match kind {
        RepositoryEntryKind::Deleted | RepositoryEntryKind::Submodule => Ok((None, None)),
        RepositoryEntryKind::Symlink => {
            let target = fs::read_link(&absolute)?;
            let bytes = target.as_os_str().as_encoded_bytes();
            if bytes.len() > MAX_SYMLINK_TARGET_BYTES {
                return Err(GitError::ScopeTooLarge);
            }
            Ok((None, Some(sha256_hex(bytes))))
        }
        RepositoryEntryKind::File => {
            let metadata = fs::metadata(&absolute)?;
            if metadata.len() > MAX_SNAPSHOT_FILE_BYTES {
                return Err(GitError::ScopeTooLarge);
            }
            Ok((Some(sha256_hex(&fs::read(absolute)?)), None))
        }
    }
}

fn normalize_status_path(path: &str) -> Result<PathBuf, GitError> {
    let path = PathBuf::from(path);
    if path.is_absolute()
        || path.as_os_str().is_empty()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(GitError::InvalidRequest(
            "Git status returned an unsafe changed path".to_string(),
        ));
    }
    Ok(path)
}

fn normalized_object_id(value: &str) -> Result<Option<String>, GitError> {
    if value.chars().all(|character| character == '0') {
        return Ok(None);
    }
    validate_object_id(value)?;
    Ok(Some(value.to_string()))
}

fn validate_object_id(value: &str) -> Result<(), GitError> {
    if !matches!(value.len(), 40 | 64) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(GitError::InvalidRequest(
            "Git object identity is invalid".to_string(),
        ));
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<(), GitError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(GitError::InvalidRequest(
            "SHA-256 identity is invalid".to_string(),
        ));
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn build_verification_receipt(
    request: &VerificationReceiptRequest,
) -> Result<VerificationReceipt, GitError> {
    if request.version != VERIFICATION_RECEIPT_VERSION {
        return Err(GitError::InvalidRequest(
            "verification receipt version is unsupported".to_string(),
        ));
    }
    validate_sha256(&request.plan_sha256)?;
    validate_sha256(&request.instruction_fingerprint_sha256)?;
    for snapshot in [&request.pre_snapshot, &request.post_snapshot] {
        validate_sha256(&snapshot.aggregate_sha256)?;
        let mut canonical = (*snapshot).clone();
        canonical.aggregate_sha256.clear();
        if sha256_hex(&serde_json::to_vec(&canonical)?) != snapshot.aggregate_sha256 {
            return Err(GitError::InvalidRequest(
                "verification receipt contains an invalid repository snapshot digest".to_string(),
            ));
        }
        if snapshot.version != REPOSITORY_SNAPSHOT_VERSION
            || snapshot.project_instruction_fingerprint_sha256
                != request.instruction_fingerprint_sha256
        {
            return Err(GitError::InvalidRequest(
                "verification receipt instruction or snapshot identity is inconsistent".to_string(),
            ));
        }
    }
    if !request.commands_passed
        || !request.required_artifacts_complete
        || request.pre_snapshot.aggregate_sha256 != request.post_snapshot.aggregate_sha256
    {
        return Err(GitError::InvalidRequest(
            "verification evidence is incomplete or repository state changed".to_string(),
        ));
    }
    Ok(VerificationReceipt {
        version: VERIFICATION_RECEIPT_VERSION,
        stage: request.stage,
        verified: true,
        plan_sha256: request.plan_sha256.clone(),
        instruction_fingerprint_sha256: request.instruction_fingerprint_sha256.clone(),
        repository_snapshot_sha256: request.pre_snapshot.aggregate_sha256.clone(),
    })
}

fn prepare_repository(root: &Path, request: &PrepareRequest) -> Result<PrepareResponse, GitError> {
    let snapshot = repository_snapshot(
        root,
        &RepositorySnapshotRequest {
            version: REPOSITORY_SNAPSHOT_VERSION,
            include_prefixes: request.include_prefixes.clone(),
            exclude_prefixes: request.exclude_prefixes.clone(),
            progress_document_path: request.progress_document_path.clone(),
            max_paths: request.max_paths,
            project_instruction_fingerprint_sha256: request
                .project_instruction_fingerprint_sha256
                .clone(),
        },
    )?;
    let changed_paths = snapshot
        .entries
        .iter()
        .map(|entry| PreparedChangedPath {
            path: entry.path.clone(),
            status: format!("{}{}", entry.index_status, entry.worktree_status),
        })
        .collect();
    Ok(PrepareResponse {
        repository_root: snapshot.repository_root.clone(),
        head: snapshot.head_object_id.clone(),
        repository_identity_sha256: snapshot.repository_identity_sha256.clone(),
        repository_snapshot_sha256: snapshot.aggregate_sha256.clone(),
        manifest: verified_checkpoint_manifest(&snapshot),
        changed_paths,
    })
}

fn normalize_path_prefixes(paths: &[PathBuf]) -> Result<Vec<PathBuf>, GitError> {
    let mut normalized = BTreeSet::new();
    for path in paths {
        if path.is_absolute()
            || path.as_os_str().is_empty()
            || path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
        {
            return Err(GitError::InvalidRequest(
                "Git path prefixes must be non-empty repository-relative paths".to_string(),
            ));
        }
        normalized.insert(path.clone());
    }
    Ok(normalized.into_iter().collect())
}

fn normalize_commit_paths(paths: &[PathBuf]) -> Result<Vec<String>, GitError> {
    if paths.is_empty() || paths.len() > MAX_COMMIT_PATHS {
        return Err(GitError::InvalidRequest(
            "commit paths must be non-empty and bounded".to_string(),
        ));
    }
    let mut normalized = Vec::with_capacity(paths.len());
    let mut seen = BTreeSet::new();
    for path in paths {
        if path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
        {
            return Err(GitError::InvalidRequest(format!(
                "commit path must be repository-relative without traversal: {}",
                path.display()
            )));
        }
        let text = path.to_string_lossy().into_owned();
        if text.is_empty() || !seen.insert(text.clone()) {
            return Err(GitError::InvalidRequest(
                "commit paths must be non-empty and unique".to_string(),
            ));
        }
        normalized.push(text);
    }
    Ok(normalized)
}

fn verified_checkpoint_manifest(snapshot: &RepositorySnapshot) -> VerifiedCheckpointManifest {
    VerifiedCheckpointManifest {
        version: snapshot.version,
        repository_identity_sha256: snapshot.repository_identity_sha256.clone(),
        repository_snapshot_sha256: snapshot.aggregate_sha256.clone(),
        head_object_id: snapshot.head_object_id.clone(),
        entries: snapshot.entries.clone(),
    }
}

fn checkpoint_request(
    snapshot: &RepositorySnapshot,
    title: impl Into<String>,
    description: impl Into<String>,
) -> CommitRequest {
    CommitRequest {
        repo_path: snapshot.repository_root.clone(),
        expected_head: snapshot.head_object_id.clone(),
        expected_repository_identity_sha256: snapshot.repository_identity_sha256.clone(),
        expected_snapshot_sha256: snapshot.aggregate_sha256.clone(),
        manifest: verified_checkpoint_manifest(snapshot),
        title: title.into(),
        description: description.into(),
        paths: snapshot
            .entries
            .iter()
            .map(|entry| entry.path.clone())
            .collect(),
    }
}

fn normalize_commit_message(title: &str, description: &str) -> Result<String, GitError> {
    let title = title.trim();
    let description = description.trim();
    if title.is_empty()
        || title.contains(['\r', '\n'])
        || title.len() > 256
        || description.len() > 7_935
    {
        return Err(GitError::InvalidRequest(
            "commit message title/description is empty, multiline, or unbounded".to_string(),
        ));
    }
    let message = if description.is_empty() {
        title.to_string()
    } else {
        format!("{title}\n\n{description}")
    };
    if message.len() > MAX_COMMIT_MESSAGE_BYTES {
        return Err(GitError::InvalidRequest(
            "commit message exceeds the bounded size".to_string(),
        ));
    }
    Ok(message)
}

fn repository_identity_sha256(repository_root: &Path) -> Result<String, GitError> {
    let git_common_dir = PathBuf::from(git_stdout(
        repository_root,
        ["rev-parse", "--git-common-dir"],
    )?);
    let git_common_dir = if git_common_dir.is_absolute() {
        git_common_dir
    } else {
        repository_root.join(git_common_dir)
    }
    .canonicalize()?;
    Ok(sha256_hex(git_common_dir.as_os_str().as_encoded_bytes()))
}

fn validate_checkpoint_request(repo: &Path, request: &CommitRequest) -> Result<(), GitError> {
    validate_sha256(&request.expected_repository_identity_sha256)?;
    validate_sha256(&request.expected_snapshot_sha256)?;
    if request.manifest.version != REPOSITORY_SNAPSHOT_VERSION
        || request.manifest.repository_identity_sha256
            != request.expected_repository_identity_sha256
        || request.manifest.repository_snapshot_sha256 != request.expected_snapshot_sha256
        || request.manifest.head_object_id != request.expected_head
        || repository_identity_sha256(repo)? != request.expected_repository_identity_sha256
    {
        return Err(GitError::InvalidRequest(
            "verified checkpoint identity does not match the repository".to_string(),
        ));
    }
    let requested = request.paths.iter().collect::<BTreeSet<_>>();
    let manifested = request
        .manifest
        .entries
        .iter()
        .map(|entry| &entry.path)
        .collect::<BTreeSet<_>>();
    if requested != manifested {
        return Err(GitError::InvalidRequest(
            "checkpoint paths do not match the verified manifest".to_string(),
        ));
    }
    for entry in &request.manifest.entries {
        let actual = worktree_git_object_id(repo, &entry.path)?;
        if entry.kind == RepositoryEntryKind::Deleted {
            if actual.is_some() {
                return Err(GitError::InvalidRequest(
                    "verified deletion no longer matches the worktree".to_string(),
                ));
            }
        } else if actual != entry.worktree_object_id {
            return Err(GitError::InvalidRequest(format!(
                "worktree object changed after verification: {}",
                entry.path.display()
            )));
        }
    }
    Ok(())
}

fn staged_object_identities(
    repo: &Path,
    paths: &[String],
) -> Result<Vec<CommittedObjectIdentity>, GitError> {
    let mut identities = Vec::with_capacity(paths.len());
    for path in paths {
        let output = Command::new("git")
            .current_dir(repo)
            .args(["ls-files", "--stage", "--"])
            .arg(path)
            .output()?;
        if !output.status.success() {
            return Err(GitError::InvalidRequest(
                "failed to inspect staged object identity".to_string(),
            ));
        }
        let row = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if row.is_empty() {
            identities.push(CommittedObjectIdentity {
                path: PathBuf::from(path),
                object_id: "deleted".to_string(),
                mode: "000000".to_string(),
            });
            continue;
        }
        let (metadata, _) = row.split_once('\t').ok_or_else(|| {
            GitError::InvalidRequest("staged object record is malformed".to_string())
        })?;
        let mut metadata = metadata.split_whitespace();
        let mode = metadata.next().unwrap_or_default().to_string();
        let object_id = metadata.next().unwrap_or_default().to_string();
        validate_object_id(&object_id)?;
        identities.push(CommittedObjectIdentity {
            path: PathBuf::from(path),
            object_id,
            mode,
        });
    }
    Ok(identities)
}

#[allow(clippy::too_many_lines)]
fn commit_repository(request: &CommitRequest) -> Result<CommitResponse, GitError> {
    let repo = request.repo_path.canonicalize()?;
    let message = normalize_commit_message(&request.title, &request.description)?;
    if !repo.is_dir() || request.paths.is_empty() {
        return Err(GitError::InvalidRequest(
            "commit requires a repository and bounded verified paths".to_string(),
        ));
    }
    validate_checkpoint_request(&repo, request)?;
    let actual_head = git_stdout(&repo, ["rev-parse", "HEAD"])?;
    if actual_head != request.expected_head {
        return Err(GitError::InvalidRequest(format!(
            "repository HEAD changed: expected {}, found {actual_head}",
            request.expected_head
        )));
    }
    let normalized = normalize_commit_paths(&request.paths)?;
    let expected = normalized
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let staged_before = git_stdout(&repo, ["diff", "--cached", "--name-only", "--"])?
        .lines()
        .map(str::to_string)
        .collect::<std::collections::BTreeSet<_>>();
    if !staged_before.is_subset(&expected) {
        return Err(GitError::InvalidRequest(format!(
            "unrelated staged paths would be included: {staged_before:?}"
        )));
    }
    let mut add = Command::new("git");
    add.current_dir(&repo).arg("add").arg("--");
    add.args(&normalized);
    run_git(&mut add, "git add")?;

    let staged = git_stdout(&repo, ["diff", "--cached", "--name-only", "--"])?;
    let staged = staged.lines().map(str::to_string).collect::<Vec<_>>();
    let actual = staged
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    if actual != expected {
        return Err(GitError::InvalidRequest(format!(
            "staged paths differ from the bounded request: expected {expected:?}, found {actual:?}"
        )));
    }
    let committed_objects = staged_object_identities(&repo, &normalized)?;
    for (entry, staged) in request.manifest.entries.iter().zip(&committed_objects) {
        let expected_object = if entry.kind == RepositoryEntryKind::Deleted {
            "deleted"
        } else {
            entry.worktree_object_id.as_deref().ok_or_else(|| {
                GitError::InvalidRequest(
                    "verified entry has no worktree object identity".to_string(),
                )
            })?
        };
        if staged.path != entry.path || staged.object_id != expected_object {
            return Err(GitError::InvalidRequest(format!(
                "staged Git object differs from verified content: {}",
                entry.path.display()
            )));
        }
    }
    let mut commit = Command::new("git");
    commit
        .current_dir(&repo)
        .arg("commit")
        .arg("--only")
        .arg("-m")
        .arg(&message)
        .arg("--")
        .args(&normalized);
    run_git(&mut commit, "git commit")?;
    let commit_hash = git_stdout(&repo, ["rev-parse", "HEAD"])?;
    if commit_hash == actual_head {
        return Err(GitError::InvalidRequest(
            "Git commit did not advance HEAD".to_string(),
        ));
    }
    let parent = git_stdout(&repo, ["rev-parse", "HEAD^"])?;
    if parent != actual_head {
        return Err(GitError::InvalidRequest(
            "committed checkpoint parent is not the verified HEAD".to_string(),
        ));
    }
    let committed_paths = git_stdout(
        &repo,
        ["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
    )?
    .lines()
    .map(str::to_string)
    .collect::<BTreeSet<_>>();
    if committed_paths != expected {
        return Err(GitError::InvalidRequest(
            "committed path set differs from the verified manifest".to_string(),
        ));
    }
    let committed_tree = git_stdout(&repo, ["rev-parse", "HEAD^{tree}"])?;
    Ok(CommitResponse {
        previous_head: actual_head,
        commit_hash,
        repository_snapshot_sha256: request.expected_snapshot_sha256.clone(),
        committed_tree,
        paths: normalized.into_iter().map(PathBuf::from).collect(),
        committed_objects,
    })
}

fn git_stdout<const N: usize>(repo: &Path, args: [&str; N]) -> Result<String, GitError> {
    let output = Command::new("git").current_dir(repo).args(args).output()?;
    if !output.status.success() {
        return Err(GitError::CloneFailed {
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn run_git(command: &mut Command, operation: &str) -> Result<(), GitError> {
    let output = command.output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(GitError::CloneFailed {
            status: format!("{operation}: {}", output.status),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CloneRequest {
    url: String,
    #[serde(default, rename = "ref", alias = "branch")]
    git_ref: Option<String>,
    #[serde(default)]
    destination: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
struct CloneResponse {
    url: String,
    clone_url: String,
    host: String,
    owner: Option<String>,
    repo: String,
    git_ref: Option<String>,
    artifact_kind: String,
    artifact_scope: String,
    path: PathBuf,
    already_exists: bool,
}

#[derive(Debug, Error)]
enum GitError {
    #[error("{0}")]
    InvalidRequest(String),
    #[error("repository snapshot scope is too large")]
    ScopeTooLarge,
    #[error("git clone failed with status {status}: {stderr}")]
    CloneFailed { status: String, stderr: String },
    #[error("failed to encode repository identity: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("failed to run git: {0}")]
    GitIo(#[from] std::io::Error),
}

fn clone_repository(
    request: &CloneRequest,
    descriptor: &GitPreparationDescriptor,
) -> Result<CloneResponse, GitError> {
    let remote = parse_git_remote(&request.url)?;
    let base = descriptor.destination.clone();
    if base.exists() {
        return Ok(clone_response(
            request,
            remote,
            base,
            true,
            &descriptor.artifact_scope,
        ));
    }
    if let Some(parent) = base.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut command = Command::new("git");
    command.arg("clone").arg("--depth").arg("1");
    if let Some(git_ref) = request
        .git_ref
        .as_deref()
        .filter(|git_ref| !git_ref.trim().is_empty())
    {
        command.arg("--branch").arg(git_ref);
    }
    command.arg(&remote.clone_url).arg(&base);
    let output = command.output()?;
    if !output.status.success() {
        return Err(GitError::CloneFailed {
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        });
    }
    Ok(clone_response(
        request,
        remote,
        base,
        false,
        &descriptor.artifact_scope,
    ))
}

fn clone_response(
    request: &CloneRequest,
    remote: GitRemote,
    path: PathBuf,
    already_exists: bool,
    artifact_scope: &str,
) -> CloneResponse {
    CloneResponse {
        url: request.url.clone(),
        clone_url: remote.clone_url,
        host: remote.host,
        owner: remote.owner,
        repo: remote.repo,
        git_ref: request.git_ref.clone(),
        artifact_kind: "git_repo_clone".to_string(),
        artifact_scope: artifact_scope.to_owned(),
        path,
        already_exists,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitRemote {
    clone_url: String,
    host: String,
    owner: Option<String>,
    repo: String,
}

fn parse_git_remote(url: &str) -> Result<GitRemote, GitError> {
    if !url.contains("://")
        && let Some(remote) = parse_scp_like_remote(url)
    {
        return Ok(remote);
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(GitError::InvalidRequest(
            "url must be an http(s), ssh, git, or scp-like Git remote URL".to_string(),
        ));
    };
    match scheme.to_ascii_lowercase().as_str() {
        "http" | "https" | "ssh" | "git" => parse_scheme_remote(scheme, url, rest),
        _ => Err(GitError::InvalidRequest(format!(
            "unsupported Git URL scheme: {scheme}"
        ))),
    }
}

fn parse_scp_like_remote(url: &str) -> Option<GitRemote> {
    let (user_host, path) = url.split_once(':')?;
    if user_host.contains('/') || path.is_empty() {
        return None;
    }
    let host = user_host.rsplit('@').next()?.to_string();
    let (owner, repo) = owner_repo_from_path(path)?;
    Some(GitRemote {
        clone_url: url.to_string(),
        host,
        owner: Some(owner),
        repo,
    })
}

fn parse_scheme_remote(scheme: &str, original: &str, rest: &str) -> Result<GitRemote, GitError> {
    let host_path = rest
        .split_once('@')
        .map_or(rest, |(_, host_path)| host_path);
    let (host_port, path) = host_path.split_once('/').ok_or_else(|| {
        GitError::InvalidRequest("Git URL must include host and repository path".to_string())
    })?;
    let host = host_port
        .split(':')
        .next()
        .filter(|host| !host.is_empty())
        .ok_or_else(|| GitError::InvalidRequest("Git URL host must not be empty".to_string()))?
        .to_string();
    let (owner, repo) = owner_repo_from_path(path).ok_or_else(|| {
        GitError::InvalidRequest("Git URL must include owner/group and repository".to_string())
    })?;
    let original_is_git_remote = has_git_suffix(original);
    if matches!(scheme, "http" | "https") && !is_known_git_host(&host) && !original_is_git_remote {
        return Err(GitError::InvalidRequest(
            "generic http(s) Git URLs must end with .git unless the host is a known Git forge"
                .to_string(),
        ));
    }
    let clone_url = if original_is_git_remote {
        original.to_string()
    } else {
        format!("https://{host}/{owner}/{repo}.git")
    };
    Ok(GitRemote {
        clone_url,
        host,
        owner: Some(owner),
        repo,
    })
}

fn has_git_suffix(value: &str) -> bool {
    std::path::Path::new(value)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("git"))
}

fn is_known_git_host(host: &str) -> bool {
    matches!(
        host,
        "github.com" | "gitlab.com" | "codeberg.org" | "bitbucket.org"
    )
}

fn owner_repo_from_path(path: &str) -> Option<(String, String)> {
    let mut segments = path
        .trim_start_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty());
    let owner = segments.next()?.to_string();
    let repo = segments.next()?.trim_end_matches(".git").to_string();
    if owner.is_empty() || repo.is_empty() || is_known_non_repo_path(&owner) {
        None
    } else {
        Some((owner, repo))
    }
}

fn is_known_non_repo_path(segment: &str) -> bool {
    matches!(
        segment,
        "features" | "topics" | "trending" | "marketplace" | "explore"
    )
}

fn default_destination(root: &Path, remote: &GitRemote) -> PathBuf {
    let mut path = root.join("git").join(sanitize_path_component(&remote.host));
    if let Some(owner) = remote.owner.as_deref() {
        path = path.join(sanitize_path_component(owner));
    }
    path.join(sanitize_path_component(&remote.repo))
}

fn sanitize_path_component(component: &str) -> String {
    component
        .chars()
        .map(|character| match character {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => character,
            _ => '_',
        })
        .collect()
}

fn default_global_artifact_dir() -> PathBuf {
    default_state_dir().join("artifacts").join("git")
}

fn default_state_dir() -> PathBuf {
    if let Ok(path) = env::var("BCODE_STATE_DIR") {
        return PathBuf::from(path);
    }
    if let Ok(state_home) = env::var("XDG_STATE_HOME") {
        return PathBuf::from(state_home).join("bcode");
    }
    if let Ok(home) = env::var("HOME") {
        return PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("bcode");
    }
    env::temp_dir().join("bcode")
}

fn clone_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "git.clone".to_string(),
        description: "Shallow-clone a Git repository into Bcode-managed artifact state so agents can inspect real files instead of rendered HTML.".to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["url"],
            "properties": {
                "url": { "type": "string" },
                "ref": { "type": "string", "description": "Optional branch or tag to clone" },
                "branch": { "type": "string", "description": "Deprecated alias for ref" },
                "destination": { "type": "string" }
            }
        }),
    }
}

fn github_clone_alias_definition() -> ToolDefinition {
    let mut definition = clone_tool_definition();
    definition.name = "github.clone".to_string();
    definition.description =
        "Compatibility alias for git.clone; prefer git.clone for all Git hosts.".to_string();
    definition
}

fn json_response<T: Serialize>(value: &T) -> ServiceResponse {
    match ServiceResponse::json(value) {
        Ok(response) => response,
        Err(error) => ServiceResponse::error("encode_failed", error.to_string()),
    }
}

fn invalid_request(error: &serde_json::Error) -> ServiceResponse {
    ServiceResponse::error("invalid_request", error.to_string())
}

fn json_tool_response_with_artifact<T: Serialize>(
    value: &T,
    tool_call_id: &str,
    artifact_suffix: &str,
    schema: &str,
    title: &str,
) -> ToolInvocationResponse {
    match serde_json::to_string_pretty(value).and_then(|output| {
        let payload = serde_json::to_value(value)?;
        Ok((output, payload))
    }) {
        Ok((output, payload)) => ToolInvocationResponse {
            output,
            is_error: false,
            content: Vec::new(),
            full_output: None,
            result: Some(ToolInvocationResult::Artifact {
                artifact: Box::new(ToolArtifact {
                    artifact_id: format!("{tool_call_id}-git-{artifact_suffix}"),
                    producer_plugin_id: GIT_PLUGIN_ID.to_string(),
                    schema: schema.to_string(),
                    schema_version: 1,
                    tool_call_id: Some(tool_call_id.to_string()),
                    title: Some(title.to_string()),
                    metadata: payload,
                    refs: Vec::new(),
                }),
            }),
        },
        Err(error) => tool_error(error.to_string()),
    }
}

const fn tool_error(output: String) -> ToolInvocationResponse {
    ToolInvocationResponse {
        output,
        is_error: true,
        content: Vec::new(),
        full_output: None,
        result: None,
    }
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(GitPlugin, include_str!("../bcode-plugin.toml"))
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn git_tui_registry() -> bcode_plugin_sdk::tui::PluginTuiRegistry {
    let mut registry = bcode_plugin_sdk::tui::PluginTuiRegistry::default();
    registry.register_visual_adapter(
        ["git-clone-request-card", "git-clone-result-card"],
        Box::new(git_tui::GitTuiVisualAdapter),
    );
    registry
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(GitPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repository() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("repo");
        for args in [
            &["init"][..],
            &["config", "user.email", "bcode@example.invalid"][..],
            &["config", "user.name", "Bcode Test"][..],
        ] {
            run_git(
                Command::new("git").current_dir(directory.path()).args(args),
                "initialize repository",
            )
            .expect("initialize repository");
        }
        fs::write(directory.path().join("tracked.txt"), "before\n").expect("tracked");
        run_git(
            Command::new("git")
                .current_dir(directory.path())
                .args(["add", "tracked.txt"]),
            "stage initial",
        )
        .expect("stage initial");
        run_git(
            Command::new("git")
                .current_dir(directory.path())
                .args(["commit", "-m", "initial"]),
            "commit initial",
        )
        .expect("commit initial");
        directory
    }

    fn snapshot_request() -> RepositorySnapshotRequest {
        RepositorySnapshotRequest {
            version: REPOSITORY_SNAPSHOT_VERSION,
            include_prefixes: Vec::new(),
            exclude_prefixes: Vec::new(),
            progress_document_path: Some(PathBuf::from("local-progress.md")),
            max_paths: 100,
            project_instruction_fingerprint_sha256: "a".repeat(64),
        }
    }

    #[test]
    fn repository_snapshot_is_ordered_exact_and_excludes_progress_document() {
        let directory = init_repository();
        fs::write(directory.path().join("tracked.txt"), "worktree\n").expect("worktree");
        run_git(
            Command::new("git")
                .current_dir(directory.path())
                .args(["add", "tracked.txt"]),
            "stage tracked",
        )
        .expect("stage tracked");
        fs::write(directory.path().join("tracked.txt"), "partial\n").expect("partial");
        fs::write(directory.path().join("untracked.txt"), "new\n").expect("untracked");
        fs::write(directory.path().join("local-progress.md"), "local\n").expect("progress");

        let first = repository_snapshot(directory.path(), &snapshot_request()).expect("snapshot");
        assert_eq!(
            first
                .entries
                .iter()
                .map(|entry| entry.path.as_path())
                .collect::<Vec<_>>(),
            [Path::new("tracked.txt"), Path::new("untracked.txt")]
        );
        assert_eq!(first.entries[0].index_status, 'M');
        assert_eq!(first.entries[0].worktree_status, 'M');
        assert_eq!(first.entries[1].index_status, '?');
        assert_eq!(first.aggregate_sha256.len(), 64);
        assert!(
            first
                .exclude_prefixes
                .contains(&PathBuf::from("local-progress.md"))
        );
        let second = repository_snapshot(directory.path(), &snapshot_request()).expect("snapshot");
        assert_eq!(first, second);
    }

    #[cfg(unix)]
    #[test]
    fn repository_snapshot_captures_rename_deletion_and_symlink_identity() {
        use std::os::unix::fs::symlink;
        let directory = init_repository();
        run_git(
            Command::new("git").current_dir(directory.path()).args([
                "mv",
                "tracked.txt",
                "renamed.txt",
            ]),
            "rename",
        )
        .expect("rename");
        symlink("renamed.txt", directory.path().join("linked.txt")).expect("symlink");
        let snapshot =
            repository_snapshot(directory.path(), &snapshot_request()).expect("snapshot");
        assert!(snapshot.entries.iter().any(|entry| {
            entry.path == Path::new("renamed.txt")
                && entry.source_path.as_deref() == Some(Path::new("tracked.txt"))
        }));
        assert!(snapshot.entries.iter().any(|entry| {
            entry.path == Path::new("linked.txt")
                && entry.kind == RepositoryEntryKind::Symlink
                && entry.symlink_target_sha256.is_some()
        }));
    }

    #[test]
    fn verification_receipt_requires_unchanged_complete_evidence() {
        let directory = init_repository();
        fs::write(directory.path().join("tracked.txt"), "changed\n").expect("change");
        let snapshot =
            repository_snapshot(directory.path(), &snapshot_request()).expect("snapshot");
        let request = VerificationReceiptRequest {
            version: VERIFICATION_RECEIPT_VERSION,
            stage: VerificationStage::PostFormat,
            plan_sha256: "b".repeat(64),
            instruction_fingerprint_sha256: "a".repeat(64),
            pre_snapshot: snapshot.clone(),
            post_snapshot: snapshot,
            commands_passed: true,
            required_artifacts_complete: true,
        };
        let receipt = build_verification_receipt(&request).expect("receipt");
        assert!(receipt.verified);
        assert_eq!(receipt.stage, VerificationStage::PostFormat);
        let mut changed = request.clone();
        changed.post_snapshot.aggregate_sha256 = "c".repeat(64);
        assert!(build_verification_receipt(&changed).is_err());

        let mut instruction_drift = request.clone();
        instruction_drift.instruction_fingerprint_sha256 = "d".repeat(64);
        assert!(build_verification_receipt(&instruction_drift).is_err());

        let mut failed_command = request.clone();
        failed_command.commands_passed = false;
        assert!(build_verification_receipt(&failed_command).is_err());

        let mut missing_artifact = request;
        missing_artifact.required_artifacts_complete = false;
        assert!(build_verification_receipt(&missing_artifact).is_err());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn typed_commit_enforces_snapshot_paths_and_returns_commit_hash() {
        let directory = tempfile::tempdir().expect("repo");
        run_git(
            Command::new("git")
                .current_dir(directory.path())
                .arg("init"),
            "git init",
        )
        .expect("init");
        run_git(
            Command::new("git").current_dir(directory.path()).args([
                "config",
                "user.email",
                "bcode@example.invalid",
            ]),
            "email",
        )
        .expect("email");
        run_git(
            Command::new("git").current_dir(directory.path()).args([
                "config",
                "user.name",
                "Bcode Test",
            ]),
            "name",
        )
        .expect("name");
        std::fs::write(directory.path().join("tracked.txt"), "before\n").expect("file");
        run_git(
            Command::new("git")
                .current_dir(directory.path())
                .args(["add", "tracked.txt"]),
            "add initial",
        )
        .expect("add");
        run_git(
            Command::new("git")
                .current_dir(directory.path())
                .args(["commit", "-m", "initial"]),
            "commit initial",
        )
        .expect("commit");
        let head = git_stdout(directory.path(), ["rev-parse", "HEAD"]).expect("head");
        let before = commit_status(&CommitStatusRequest {
            repo_path: directory.path().to_path_buf(),
            expected_head: head.clone(),
            paths: vec![PathBuf::from("tracked.txt")],
        })
        .expect("status before commit");
        assert_eq!(before.outcome, CommitReconciliationOutcome::NotCommitted);
        std::fs::write(directory.path().join("tracked.txt"), "after\n").expect("change");
        std::fs::write(directory.path().join("other.txt"), "not committed\n").expect("other");

        let snapshot =
            repository_snapshot(directory.path(), &snapshot_request()).expect("snapshot");
        let response = commit_repository(&checkpoint_request(
            &RepositorySnapshot {
                entries: snapshot
                    .entries
                    .iter()
                    .filter(|entry| entry.path == Path::new("tracked.txt"))
                    .cloned()
                    .collect(),
                ..snapshot.clone()
            },
            "bounded change",
            "",
        ))
        .expect("commit");
        assert_eq!(response.previous_head, head);
        assert_ne!(response.commit_hash, response.previous_head);
        assert_eq!(response.paths, [PathBuf::from("tracked.txt")]);
        let status = commit_status(&CommitStatusRequest {
            repo_path: directory.path().to_path_buf(),
            expected_head: response.previous_head.clone(),
            paths: response.paths.clone(),
        })
        .expect("status");
        assert_eq!(status.outcome, CommitReconciliationOutcome::CandidateCommit);
        assert_eq!(status.actual_head, response.commit_hash);
        assert_eq!(status.actual_commit_paths, [PathBuf::from("tracked.txt")]);
        assert_eq!(
            git_stdout(
                directory.path(),
                ["show", "--pretty=", "--name-only", "HEAD"]
            )
            .expect("show"),
            "tracked.txt"
        );
        assert!(directory.path().join("other.txt").exists());

        std::fs::write(directory.path().join("tracked.txt"), "second change\n").expect("change");
        run_git(
            Command::new("git")
                .current_dir(directory.path())
                .args(["add", "other.txt"]),
            "stage unrelated",
        )
        .expect("stage unrelated");
        let before_rejected_commit = git_stdout(directory.path(), ["rev-parse", "HEAD"])
            .expect("head before rejected commit");
        assert!(
            commit_repository(&CommitRequest {
                repo_path: directory.path().to_path_buf(),
                expected_head: before_rejected_commit.clone(),
                expected_repository_identity_sha256: snapshot.repository_identity_sha256.clone(),
                expected_snapshot_sha256: snapshot.aggregate_sha256.clone(),
                manifest: verified_checkpoint_manifest(&snapshot),
                title: "reject unrelated staged path".to_string(),
                description: String::new(),
                paths: vec![PathBuf::from("tracked.txt")],
            })
            .is_err()
        );
        assert_eq!(
            git_stdout(directory.path(), ["rev-parse", "HEAD"]).expect("head"),
            before_rejected_commit
        );

        assert!(
            commit_repository(&CommitRequest {
                repo_path: directory.path().to_path_buf(),
                expected_head: response.previous_head,
                expected_repository_identity_sha256: snapshot.repository_identity_sha256.clone(),
                expected_snapshot_sha256: snapshot.aggregate_sha256.clone(),
                manifest: verified_checkpoint_manifest(&snapshot),
                title: "stale".to_string(),
                description: String::new(),
                paths: vec![PathBuf::from("other.txt")],
            })
            .is_err()
        );
        assert!(
            commit_repository(&CommitRequest {
                repo_path: directory.path().to_path_buf(),
                expected_head: response.commit_hash.clone(),
                expected_repository_identity_sha256: snapshot.repository_identity_sha256.clone(),
                expected_snapshot_sha256: snapshot.aggregate_sha256.clone(),
                manifest: verified_checkpoint_manifest(&snapshot),
                title: "empty".to_string(),
                description: String::new(),
                paths: Vec::new(),
            })
            .is_err()
        );
        assert!(
            commit_repository(&CommitRequest {
                repo_path: directory.path().to_path_buf(),
                expected_head: response.commit_hash,
                expected_repository_identity_sha256: snapshot.repository_identity_sha256.clone(),
                expected_snapshot_sha256: snapshot.aggregate_sha256.clone(),
                manifest: verified_checkpoint_manifest(&snapshot),
                title: "escape".to_string(),
                description: String::new(),
                paths: vec![PathBuf::from("../outside")],
            })
            .is_err()
        );
    }

    #[test]
    fn typed_commit_rejects_failing_hook_without_advancing_head() {
        let directory = tempfile::tempdir().expect("repo");
        for args in [
            &["init"][..],
            &["config", "user.email", "bcode@example.invalid"][..],
            &["config", "user.name", "Bcode Test"][..],
        ] {
            run_git(
                Command::new("git").current_dir(directory.path()).args(args),
                "setup",
            )
            .expect("setup");
        }
        std::fs::write(directory.path().join("tracked.txt"), "before\n").expect("file");
        run_git(
            Command::new("git")
                .current_dir(directory.path())
                .args(["add", "tracked.txt"]),
            "add",
        )
        .expect("add");
        run_git(
            Command::new("git")
                .current_dir(directory.path())
                .args(["commit", "-m", "initial"]),
            "initial commit",
        )
        .expect("initial commit");
        let head = git_stdout(directory.path(), ["rev-parse", "HEAD"]).expect("head");
        std::fs::write(directory.path().join("tracked.txt"), "after\n").expect("change");
        let hooks = directory.path().join("hooks");
        std::fs::create_dir(&hooks).expect("hooks");
        let hook = hooks.join("pre-commit");
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").expect("hook");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))
                .expect("permissions");
        }
        run_git(
            Command::new("git").current_dir(directory.path()).args([
                "config",
                "core.hooksPath",
                "hooks",
            ]),
            "hooks path",
        )
        .expect("hooks path");
        let hook_snapshot =
            repository_snapshot(directory.path(), &snapshot_request()).expect("snapshot");
        assert!(
            commit_repository(&checkpoint_request(&hook_snapshot, "blocked by hook", "")).is_err()
        );
        assert_eq!(
            git_stdout(directory.path(), ["rev-parse", "HEAD"]).expect("head after failure"),
            head
        );
    }

    #[test]
    fn compose_commit_preserves_prepared_head_paths_and_no_changes_policy() {
        let dummy_entry = RepositorySnapshotEntry {
            path: PathBuf::from("src/lib.rs"),
            index_status: ' ',
            worktree_status: 'M',
            kind: RepositoryEntryKind::File,
            base_mode: "100644".to_string(),
            index_mode: "100644".to_string(),
            worktree_mode: "100644".to_string(),
            base_object_id: None,
            index_object_id: None,
            worktree_sha256: Some("a".repeat(64)),
            worktree_object_id: Some("a".repeat(40)),
            source_path: None,
            symlink_target_sha256: None,
            submodule_state: None,
        };
        let preparation = PrepareResponse {
            repository_root: PathBuf::from("/repo"),
            head: "a".repeat(40),
            repository_identity_sha256: "b".repeat(64),
            repository_snapshot_sha256: "c".repeat(64),
            manifest: VerifiedCheckpointManifest {
                version: 1,
                repository_identity_sha256: "b".repeat(64),
                repository_snapshot_sha256: "c".repeat(64),
                head_object_id: "a".repeat(40),
                entries: vec![dummy_entry],
            },
            changed_paths: vec![PreparedChangedPath {
                path: PathBuf::from("src/lib.rs"),
                status: " M".to_string(),
            }],
        };
        let composed = compose_commit_request(ComposeCommitRequest {
            preparation,
            message: ProposedCommitMessage {
                title: "Implement workflow".to_string(),
                description: "Preserve exact preparation facts.".to_string(),
            },
            no_changes: NoChangesDecision::Fail,
        })
        .expect("compose");
        let ComposedCommitRequest::Ready { request } = composed else {
            panic!("expected ready commit");
        };
        assert_eq!(request.repo_path, PathBuf::from("/repo"));
        assert_eq!(request.expected_head, "a".repeat(40));
        assert_eq!(request.paths, [PathBuf::from("src/lib.rs")]);
        assert_eq!(
            normalize_commit_message(&request.title, &request.description).expect("message"),
            "Implement workflow\n\nPreserve exact preparation facts."
        );

        let empty = PrepareResponse {
            repository_root: PathBuf::from("/repo"),
            head: "a".repeat(40),
            repository_identity_sha256: "b".repeat(64),
            repository_snapshot_sha256: "c".repeat(64),
            manifest: VerifiedCheckpointManifest {
                version: 1,
                repository_identity_sha256: "b".repeat(64),
                repository_snapshot_sha256: "c".repeat(64),
                head_object_id: "a".repeat(40),
                entries: Vec::new(),
            },
            changed_paths: Vec::new(),
        };
        assert_eq!(
            compose_commit_request(ComposeCommitRequest {
                preparation: empty.clone(),
                message: ProposedCommitMessage {
                    title: "No changes".to_string(),
                    description: String::new(),
                },
                no_changes: NoChangesDecision::NoOp,
            })
            .expect("no-op"),
            ComposedCommitRequest::NoChanges
        );
        assert!(
            compose_commit_request(ComposeCommitRequest {
                preparation: empty,
                message: ProposedCommitMessage {
                    title: "No changes".to_string(),
                    description: String::new(),
                },
                no_changes: NoChangesDecision::Fail,
            })
            .is_err()
        );
    }

    #[test]
    fn prepare_repository_is_read_only_bounded_and_filtered() {
        let directory = tempfile::tempdir().expect("repository");
        run_git(
            Command::new("git")
                .arg("init")
                .current_dir(directory.path()),
            "init",
        )
        .expect("init");
        run_git(
            Command::new("git")
                .args(["config", "user.email", "bcode@example.invalid"])
                .current_dir(directory.path()),
            "config",
        )
        .expect("config");
        run_git(
            Command::new("git")
                .args(["config", "user.name", "Bcode Test"])
                .current_dir(directory.path()),
            "config",
        )
        .expect("config");
        std::fs::create_dir_all(directory.path().join("src")).expect("src");
        std::fs::write(directory.path().join("src/lib.rs"), "before\n").expect("file");
        run_git(
            Command::new("git")
                .args(["add", "src/lib.rs"])
                .current_dir(directory.path()),
            "add",
        )
        .expect("add");
        run_git(
            Command::new("git")
                .args(["commit", "-m", "initial"])
                .current_dir(directory.path()),
            "commit",
        )
        .expect("commit");
        std::fs::write(directory.path().join("src/lib.rs"), "after\n").expect("change");
        std::fs::write(directory.path().join("ignored.txt"), "ignored\n").expect("ignored");
        let before = git_stdout(directory.path(), ["rev-parse", "HEAD"]).expect("head");
        let response = prepare_repository(
            directory.path(),
            &PrepareRequest {
                include_prefixes: vec![PathBuf::from("src")],
                exclude_prefixes: Vec::new(),
                progress_document_path: None,
                project_instruction_fingerprint_sha256: zero_sha256(),
                max_paths: 10,
            },
        )
        .expect("prepare");
        assert_eq!(response.head, before);
        assert_eq!(response.changed_paths.len(), 1);
        assert_eq!(response.changed_paths[0].path, PathBuf::from("src/lib.rs"));
        assert_eq!(
            git_stdout(directory.path(), ["rev-parse", "HEAD"]).expect("head"),
            before
        );
        assert!(
            prepare_repository(
                directory.path(),
                &PrepareRequest {
                    include_prefixes: Vec::new(),
                    exclude_prefixes: Vec::new(),
                    progress_document_path: None,
                    project_instruction_fingerprint_sha256: zero_sha256(),
                    max_paths: 1,
                },
            )
            .is_err()
        );
    }

    #[test]
    fn commit_manifest_declares_mutating_repair_and_write_resources() {
        let manifest: bcode_plugin::PluginManifest =
            toml::from_str(include_str!("../bcode-plugin.toml")).expect("manifest");
        let snapshot = &manifest.services[1].workflow_blocks[0];
        assert_eq!(snapshot.block_id, "git.repository-snapshot");
        assert_eq!(
            snapshot.effect,
            bcode_workflow::WorkflowBlockEffect::ReadOnly
        );
        assert_eq!(
            snapshot.reconciliation,
            bcode_workflow::WorkflowBlockReconciliation::IdempotentReplay
        );
        assert_eq!(
            snapshot.resources,
            [bcode_workflow::ResourceClaim::read("repository")]
        );
        let receipt = &manifest.services[1].workflow_blocks[1];
        assert_eq!(receipt.block_id, "git.verification-receipt");
        assert_eq!(
            receipt.effect,
            bcode_workflow::WorkflowBlockEffect::ReadOnly
        );
        assert_eq!(
            receipt.reconciliation,
            bcode_workflow::WorkflowBlockReconciliation::IdempotentReplay
        );
        let prepare = &manifest.services[1].workflow_blocks[2];
        assert_eq!(prepare.block_id, "git.prepare");
        assert_eq!(
            prepare.effect,
            bcode_workflow::WorkflowBlockEffect::ReadOnly
        );
        assert_eq!(
            prepare.reconciliation,
            bcode_workflow::WorkflowBlockReconciliation::IdempotentReplay
        );
        assert!(!prepare.authorization.explicit_grant_required);
        assert_eq!(
            prepare.resources,
            [bcode_workflow::ResourceClaim::read("repository")]
        );
        let compose = &manifest.services[1].workflow_blocks[3];
        assert_eq!(compose.block_id, "git.compose-commit");
        assert_eq!(
            compose.effect,
            bcode_workflow::WorkflowBlockEffect::ReadOnly
        );
        assert_eq!(
            compose.reconciliation,
            bcode_workflow::WorkflowBlockReconciliation::IdempotentReplay
        );
        assert!(!compose.authorization.explicit_grant_required);
        let status = &manifest.services[1].workflow_blocks[4];
        assert_eq!(status.block_id, "git.commit-status");
        assert_eq!(status.effect, bcode_workflow::WorkflowBlockEffect::ReadOnly);
        assert_eq!(
            status.reconciliation,
            bcode_workflow::WorkflowBlockReconciliation::IdempotentReplay
        );
        let block = &manifest.services[1].workflow_blocks[5];
        assert_eq!(block.block_id, "git.commit");
        assert_eq!(block.effect, bcode_workflow::WorkflowBlockEffect::Mutating);
        assert_eq!(
            block.reconciliation,
            bcode_workflow::WorkflowBlockReconciliation::RepairRequired
        );
        assert!(block.authorization.explicit_grant_required);
        assert_eq!(
            block.resources,
            [
                bcode_workflow::ResourceClaim::write("repository"),
                bcode_workflow::ResourceClaim::write("git-ref"),
            ]
        );
    }

    #[test]
    fn clone_request_uses_durable_generic_contribution_without_legacy_visual() {
        let request = CloneRequest {
            url: "https://github.com/bmorphism/bcode".to_owned(),
            git_ref: Some("main".to_owned()),
            destination: None,
        };
        let payload = serde_json::to_value(request).expect("clone request payload");
        assert_eq!(payload["url"], "https://github.com/bmorphism/bcode");
        assert_eq!(payload["ref"], "main");
    }

    fn host_context(
        workspace: &Path,
        artifact_root: Option<&Path>,
    ) -> Vec<bcode_tool::ToolHostContextEntry> {
        let mut entries = vec![bcode_tool::ToolHostContextEntry {
            schema: bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA.to_owned(),
            schema_version: bcode_tool::TOOL_WORKSPACE_CONTEXT_SCHEMA_VERSION,
            payload: serde_json::json!({"working_directory": workspace}),
        }];
        if let Some(root) = artifact_root {
            entries.push(bcode_tool::ToolHostContextEntry {
                schema: bcode_tool::TOOL_ARTIFACT_CONTEXT_SCHEMA.to_owned(),
                schema_version: bcode_tool::TOOL_ARTIFACT_CONTEXT_SCHEMA_VERSION,
                payload: serde_json::json!({"root": root}),
            });
        }
        entries
    }

    #[test]
    fn git_owner_prepares_permission_required_clone_policy() {
        let definition = clone_tool_definition();
        let request = bcode_tool::ToolPreparationRequest {
            invocation: bcode_tool::ToolInvocationDescriptor {
                invocation_id: "call".to_owned(),
                tool_name: definition.name.clone(),
                arguments: serde_json::json!({
                    "url": "https://github.com/bmorphism/bcode",
                    "destination": "repos/bcode"
                }),
            },
            host_context: host_context(
                Path::new("/tmp/workspace"),
                Some(Path::new("/tmp/artifacts")),
            ),
        };
        let policy = git_policy_preparation(&request, &definition).expect("Git preparation");
        assert!(policy.requires_permission);
        assert_eq!(
            policy.operation,
            bcode_plugin_sdk::ToolPolicyOperation::Write {
                paths: vec!["/tmp/workspace/repos/bcode".to_owned()],
                category: "write".to_owned(),
            }
        );
        assert_eq!(
            serde_json::from_value::<GitPreparationDescriptor>(policy.descriptor)
                .expect("Git descriptor"),
            GitPreparationDescriptor {
                destination: PathBuf::from("/tmp/workspace/repos/bcode"),
                artifact_scope: "explicit".to_owned(),
            }
        );
    }

    #[test]
    fn parses_github_web_urls() {
        let remote = parse_git_remote("https://github.com/bmorphism/bcode").expect("repo");
        assert_eq!(remote.host, "github.com");
        assert_eq!(remote.owner.as_deref(), Some("bmorphism"));
        assert_eq!(remote.repo, "bcode");
        assert_eq!(remote.clone_url, "https://github.com/bmorphism/bcode.git");
    }

    #[test]
    fn parses_gitlab_web_urls() {
        let remote = parse_git_remote("https://gitlab.com/group/project").expect("repo");
        assert_eq!(remote.host, "gitlab.com");
        assert_eq!(remote.owner.as_deref(), Some("group"));
        assert_eq!(remote.repo, "project");
        assert_eq!(remote.clone_url, "https://gitlab.com/group/project.git");
    }

    #[test]
    fn preserves_scp_like_remotes() {
        let remote = parse_git_remote("git@gitlab.com:group/project.git").expect("repo");
        assert_eq!(remote.host, "gitlab.com");
        assert_eq!(remote.clone_url, "git@gitlab.com:group/project.git");
    }

    #[test]
    fn rejects_non_repo_urls() {
        assert!(parse_git_remote("https://example.com/repo").is_err());
        assert!(parse_git_remote("https://github.com/features/actions").is_err());
    }

    #[test]
    fn default_destination_uses_owner_resolved_artifact_root() {
        let definition = clone_tool_definition();
        let request = bcode_tool::ToolPreparationRequest {
            invocation: bcode_tool::ToolInvocationDescriptor {
                invocation_id: "call".to_owned(),
                tool_name: definition.name,
                arguments: serde_json::json!({
                    "url": "https://gitlab.com/group/project"
                }),
            },
            host_context: host_context(
                Path::new("/tmp/workspace"),
                Some(Path::new("/tmp/artifacts/session-1")),
            ),
        };

        let descriptor = git_preparation_descriptor(&request).expect("Git descriptor");

        assert_eq!(
            descriptor,
            GitPreparationDescriptor {
                destination: PathBuf::from("/tmp/artifacts/session-1/git/gitlab.com/group/project"),
                artifact_scope: "session".to_owned(),
            }
        );
    }

    #[test]
    fn preparation_requires_valid_workspace_context() {
        let definition = clone_tool_definition();
        let request = bcode_tool::ToolPreparationRequest {
            invocation: bcode_tool::ToolInvocationDescriptor {
                invocation_id: "call".to_owned(),
                tool_name: definition.name,
                arguments: serde_json::json!({
                    "url": "https://gitlab.com/group/project"
                }),
            },
            host_context: Vec::new(),
        };

        let error = git_preparation_descriptor(&request).expect_err("missing workspace");

        assert!(error.contains("required host context"));
    }
}
