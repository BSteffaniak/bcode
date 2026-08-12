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
    artifact_root: PathBuf,
    relative_destination: PathBuf,
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
    let (artifact_root, artifact_scope) = artifact.map_or_else(
        || (default_global_artifact_dir(), "global".to_owned()),
        |context| (context.root, "session".to_owned()),
    );
    let artifact_root = resolve_artifact_root(&artifact_root)?;
    let remote = parse_git_remote(&clone.url).map_err(|error| error.to_string())?;
    let relative_destination = clone.destination.as_ref().map_or_else(
        || Ok(default_destination(Path::new(""), &remote)),
        |destination| {
            validate_clone_destination(destination).map(|path| Path::new("git").join(path))
        },
    )?;
    let destination = confined_clone_destination(&artifact_root, &relative_destination)?;
    Ok(GitPreparationDescriptor {
        artifact_root,
        relative_destination,
        destination,
        artifact_scope,
    })
}

fn validate_clone_destination(destination: &Path) -> Result<PathBuf, String> {
    if destination.as_os_str().is_empty() || destination.is_absolute() {
        return Err("Git clone destination must be a non-empty artifact-relative path".to_owned());
    }
    if destination
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(
            "Git clone destination must not contain parent traversal or root components".to_owned(),
        );
    }
    Ok(destination.to_path_buf())
}

fn resolve_artifact_root(root: &Path) -> Result<PathBuf, String> {
    if !root.is_absolute() {
        return Err("artifact root must be absolute".to_owned());
    }
    let mut existing = root;
    let mut missing = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .ok_or_else(|| "artifact root has no existing ancestor".to_owned())?;
        missing.push(name.to_os_string());
        existing = existing
            .parent()
            .ok_or_else(|| "artifact root has no existing ancestor".to_owned())?;
    }
    let mut resolved = existing
        .canonicalize()
        .map_err(|error| format!("artifact root is unavailable: {error}"))?;
    for component in missing.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn confined_clone_destination(root: &Path, relative: &Path) -> Result<PathBuf, String> {
    let relative = validate_clone_destination(relative)?;
    let destination = root.join(&relative);
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err("Git clone destination escapes artifact storage".to_owned());
        };
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("Git clone destination traverses a symbolic link".to_owned());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(format!("Git clone destination is unavailable: {error}")),
        }
    }
    Ok(destination)
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
    let artifact_root =
        resolve_artifact_root(&descriptor.artifact_root).map_err(GitError::InvalidRequest)?;
    if artifact_root != descriptor.artifact_root {
        return Err(GitError::InvalidRequest(
            "prepared Git artifact root changed before execution".to_owned(),
        ));
    }
    let base = confined_clone_destination(&artifact_root, &descriptor.relative_destination)
        .map_err(GitError::InvalidRequest)?;
    if base != descriptor.destination {
        return Err(GitError::InvalidRequest(
            "prepared Git clone destination does not match its artifact root".to_owned(),
        ));
    }
    if base.exists() {
        if !base.is_dir() {
            return Err(GitError::InvalidRequest(format!(
                "Git clone destination {} is not a directory",
                base.display()
            )));
        }
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
    default_state_dir().join("artifacts")
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
                "destination": { "type": "string", "description": "Optional path relative to Bcode-managed artifact storage; absolute paths and parent traversal are rejected" }
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
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        let artifact_root = root.path().join("artifacts");
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
            host_context: host_context(&workspace, Some(&artifact_root)),
        };
        let policy = git_policy_preparation(&request, &definition).expect("Git preparation");
        let artifact_root = resolve_artifact_root(&artifact_root).expect("artifact root");
        let destination = artifact_root.join("git/repos/bcode");
        assert!(policy.requires_permission);
        assert_eq!(
            policy.operation,
            bcode_plugin_sdk::ToolPolicyOperation::Write {
                paths: vec![destination.display().to_string()],
                category: "write".to_owned(),
            }
        );
        assert_eq!(
            serde_json::from_value::<GitPreparationDescriptor>(policy.descriptor)
                .expect("Git descriptor"),
            GitPreparationDescriptor {
                artifact_root,
                relative_destination: PathBuf::from("git/repos/bcode"),
                destination,
                artifact_scope: "session".to_owned(),
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
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        let artifact_root = root.path().join("artifacts/session-1");
        let definition = clone_tool_definition();
        let request = bcode_tool::ToolPreparationRequest {
            invocation: bcode_tool::ToolInvocationDescriptor {
                invocation_id: "call".to_owned(),
                tool_name: definition.name,
                arguments: serde_json::json!({
                    "url": "https://gitlab.com/group/project"
                }),
            },
            host_context: host_context(&workspace, Some(&artifact_root)),
        };

        let descriptor = git_preparation_descriptor(&request).expect("Git descriptor");
        let artifact_root = resolve_artifact_root(&artifact_root).expect("artifact root");

        assert_eq!(
            descriptor,
            GitPreparationDescriptor {
                destination: artifact_root.join("git/gitlab.com/group/project"),
                relative_destination: PathBuf::from("git/gitlab.com/group/project"),
                artifact_root,
                artifact_scope: "session".to_owned(),
            }
        );
    }

    #[test]
    fn explicit_destination_is_confined_to_artifact_root() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        let artifact_root = root.path().join("artifacts");
        let definition = clone_tool_definition();
        let request = bcode_tool::ToolPreparationRequest {
            invocation: bcode_tool::ToolInvocationDescriptor {
                invocation_id: "call".to_owned(),
                tool_name: definition.name,
                arguments: serde_json::json!({
                    "url": "https://github.com/rust-lang/rustfmt",
                    "destination": "rustfmt-reference"
                }),
            },
            host_context: host_context(&workspace, Some(&artifact_root)),
        };

        let descriptor = git_preparation_descriptor(&request).expect("Git descriptor");
        let artifact_root = resolve_artifact_root(&artifact_root).expect("artifact root");
        assert_eq!(
            descriptor.destination,
            artifact_root.join("git/rustfmt-reference")
        );
        assert!(!descriptor.destination.starts_with(&workspace));
        assert_eq!(descriptor.artifact_scope, "session");
    }

    #[test]
    fn preparation_rejects_destinations_outside_artifact_storage() {
        for destination in ["/tmp/repository", "../repository", "repos/../../repository"] {
            let definition = clone_tool_definition();
            let request = bcode_tool::ToolPreparationRequest {
                invocation: bcode_tool::ToolInvocationDescriptor {
                    invocation_id: "call".to_owned(),
                    tool_name: definition.name,
                    arguments: serde_json::json!({
                        "url": "https://github.com/bmorphism/bcode",
                        "destination": destination
                    }),
                },
                host_context: host_context(
                    Path::new("/tmp/workspace"),
                    Some(Path::new("/tmp/artifacts")),
                ),
            };

            assert!(
                git_preparation_descriptor(&request).is_err(),
                "{destination}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn preparation_rejects_symlink_escape_from_artifact_root() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let artifact_root = root.path().join("artifacts");
        std::fs::create_dir(&artifact_root).expect("artifact root");
        symlink(root.path(), artifact_root.join("git")).expect("symlink");
        let definition = clone_tool_definition();
        let request = bcode_tool::ToolPreparationRequest {
            invocation: bcode_tool::ToolInvocationDescriptor {
                invocation_id: "call".to_owned(),
                tool_name: definition.name,
                arguments: serde_json::json!({
                    "url": "https://github.com/bmorphism/bcode",
                    "destination": "repository"
                }),
            },
            host_context: host_context(root.path(), Some(&artifact_root)),
        };

        let error = git_preparation_descriptor(&request).expect_err("symlink escape");
        assert!(error.contains("symbolic link"));
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
