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
use std::collections::BTreeSet;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use thiserror::Error;

const GIT_PLUGIN_ID: &str = "bcode.git";
const GIT_CLONE_REQUEST_SCHEMA: &str = "bcode.git.clone_request";
const GIT_CLONE_RESULT_SCHEMA: &str = "bcode.git.clone_result";

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
struct CommitRequest {
    repo_path: PathBuf,
    expected_head: String,
    message: String,
    paths: Vec<PathBuf>,
}

const MAX_COMMIT_MESSAGE_BYTES: usize = 8_192;
const MAX_COMMIT_PATHS: usize = 10_000;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ComposeCommitRequest {
    preparation: PrepareResponse,
    message: ProposedCommitMessage,
    no_changes: NoChangesDecision,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProposedCommitMessage {
    title: String,
    description: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum NoChangesDecision {
    Fail,
    NoOp,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ComposedCommitRequest {
    Ready { request: CommitRequest },
    NoChanges,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct CommitResponse {
    previous_head: String,
    commit_hash: String,
    paths: Vec<PathBuf>,
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
struct PrepareRequest {
    #[serde(default)]
    include_prefixes: Vec<PathBuf>,
    #[serde(default)]
    exclude_prefixes: Vec<PathBuf>,
    max_paths: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct PreparedChangedPath {
    path: PathBuf,
    status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct PrepareResponse {
    repository_root: PathBuf,
    head: String,
    changed_paths: Vec<PreparedChangedPath>,
}

fn invoke_workflow_block(context: &NativeServiceContext) -> ServiceResponse {
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
    let title = request.message.title.trim();
    let description = request.message.description.trim();
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
    if message.len() > MAX_COMMIT_MESSAGE_BYTES
        || request.preparation.head.trim().is_empty()
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
    let mut paths = Vec::with_capacity(request.preparation.changed_paths.len());
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
        paths.push(path);
    }
    Ok(ComposedCommitRequest::Ready {
        request: CommitRequest {
            repo_path: request.preparation.repository_root,
            expected_head: request.preparation.head,
            message,
            paths,
        },
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

fn prepare_repository(root: &Path, request: &PrepareRequest) -> Result<PrepareResponse, GitError> {
    if request.max_paths == 0 || request.max_paths > 10_000 {
        return Err(GitError::InvalidRequest(
            "git preparation max_paths must be between 1 and 10000".to_string(),
        ));
    }
    let repo = root.canonicalize()?;
    let repository_root = PathBuf::from(git_stdout(&repo, ["rev-parse", "--show-toplevel"])?);
    let repository_root = repository_root.canonicalize()?;
    if !repo.starts_with(&repository_root) {
        return Err(GitError::InvalidRequest(
            "workflow workspace is outside the resolved repository".to_string(),
        ));
    }
    let include = normalize_path_prefixes(&request.include_prefixes)?;
    let exclude = normalize_path_prefixes(&request.exclude_prefixes)?;
    let status = Command::new("git")
        .current_dir(&repository_root)
        .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"])
        .output()?;
    if !status.status.success() {
        return Err(GitError::CloneFailed {
            status: status.status.to_string(),
            stderr: String::from_utf8_lossy(&status.stderr).trim().to_string(),
        });
    }
    let mut changed_paths = Vec::new();
    for record in status
        .stdout
        .split(|byte| *byte == 0)
        .filter(|row| !row.is_empty())
    {
        let record = String::from_utf8_lossy(record);
        if record.len() < 4 {
            return Err(GitError::InvalidRequest(
                "Git status returned a malformed path record".to_string(),
            ));
        }
        let status = record[..2].to_string();
        let path = PathBuf::from(&record[3..]);
        if path.is_absolute()
            || path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir | std::path::Component::Prefix(_)
                )
            })
        {
            return Err(GitError::InvalidRequest(
                "Git status returned an unsafe changed path".to_string(),
            ));
        }
        if !include.is_empty() && !include.iter().any(|prefix| path.starts_with(prefix)) {
            continue;
        }
        if exclude.iter().any(|prefix| path.starts_with(prefix)) {
            continue;
        }
        changed_paths.push(PreparedChangedPath { path, status });
        if changed_paths.len() > usize::try_from(request.max_paths).unwrap_or(usize::MAX) {
            return Err(GitError::InvalidRequest(
                "changed path set exceeds configured max_paths".to_string(),
            ));
        }
    }
    changed_paths.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(PrepareResponse {
        repository_root,
        head: git_stdout(&repo, ["rev-parse", "HEAD"])?,
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

fn commit_repository(request: &CommitRequest) -> Result<CommitResponse, GitError> {
    let repo = request.repo_path.canonicalize()?;
    if !repo.is_dir() || request.message.trim().is_empty() || request.paths.is_empty() {
        return Err(GitError::InvalidRequest(
            "commit requires a repository, non-empty message, and bounded paths".to_string(),
        ));
    }
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
    let mut commit = Command::new("git");
    commit
        .current_dir(&repo)
        .arg("commit")
        .arg("--only")
        .arg("-m")
        .arg(&request.message)
        .arg("--")
        .args(&normalized);
    run_git(&mut commit, "git commit")?;
    let commit_hash = git_stdout(&repo, ["rev-parse", "HEAD"])?;
    if commit_hash == actual_head {
        return Err(GitError::InvalidRequest(
            "Git commit did not advance HEAD".to_string(),
        ));
    }
    Ok(CommitResponse {
        previous_head: actual_head,
        commit_hash,
        paths: normalized.into_iter().map(PathBuf::from).collect(),
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
    #[error("git clone failed with status {status}: {stderr}")]
    CloneFailed { status: String, stderr: String },
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
    registry.register_visual_adapter(Box::new(git_tui::GitTuiVisualAdapter));
    registry
}

#[cfg(not(feature = "static-bundled"))]
bcode_plugin_sdk::export_plugin!(GitPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

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

        let response = commit_repository(&CommitRequest {
            repo_path: directory.path().to_path_buf(),
            expected_head: head.clone(),
            message: "bounded change".to_string(),
            paths: vec![PathBuf::from("tracked.txt")],
        })
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
                message: "reject unrelated staged path".to_string(),
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
                message: "stale".to_string(),
                paths: vec![PathBuf::from("other.txt")],
            })
            .is_err()
        );
        assert!(
            commit_repository(&CommitRequest {
                repo_path: directory.path().to_path_buf(),
                expected_head: response.commit_hash.clone(),
                message: "empty".to_string(),
                paths: Vec::new(),
            })
            .is_err()
        );
        assert!(
            commit_repository(&CommitRequest {
                repo_path: directory.path().to_path_buf(),
                expected_head: response.commit_hash,
                message: "escape".to_string(),
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
        assert!(
            commit_repository(&CommitRequest {
                repo_path: directory.path().to_path_buf(),
                expected_head: head.clone(),
                message: "blocked by hook".to_string(),
                paths: vec![PathBuf::from("tracked.txt")],
            })
            .is_err()
        );
        assert_eq!(
            git_stdout(directory.path(), ["rev-parse", "HEAD"]).expect("head after failure"),
            head
        );
    }

    #[test]
    fn compose_commit_preserves_prepared_head_paths_and_no_changes_policy() {
        let preparation = PrepareResponse {
            repository_root: PathBuf::from("/repo"),
            head: "abc123".to_string(),
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
        assert_eq!(request.expected_head, "abc123");
        assert_eq!(request.paths, [PathBuf::from("src/lib.rs")]);
        assert_eq!(
            request.message,
            "Implement workflow\n\nPreserve exact preparation facts."
        );

        let empty = PrepareResponse {
            repository_root: PathBuf::from("/repo"),
            head: "abc123".to_string(),
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
        let prepare = &manifest.services[1].workflow_blocks[0];
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
        let compose = &manifest.services[1].workflow_blocks[1];
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
        let status = &manifest.services[1].workflow_blocks[2];
        assert_eq!(status.block_id, "git.commit-status");
        assert_eq!(status.effect, bcode_workflow::WorkflowBlockEffect::ReadOnly);
        assert_eq!(
            status.reconciliation,
            bcode_workflow::WorkflowBlockReconciliation::IdempotentReplay
        );
        let block = &manifest.services[1].workflow_blocks[3];
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
