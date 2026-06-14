#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled Git repository access tool plugin for Bcode.

use bcode_plugin_sdk::prelude::*;
use bcode_tool::{
    ListToolsRequest, OP_INVOKE_TOOL, OP_LIST_TOOLS, TOOL_SERVICE_INTERFACE_ID, ToolDefinition,
    ToolInvocationRequest, ToolInvocationResponse, ToolList, ToolSideEffect,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use thiserror::Error;

/// Bundled Git access plugin.
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

fn invoke_tool_service(context: &NativeServiceContext) -> ServiceResponse {
    let request = &context.request;
    match request.operation.as_str() {
        OP_LIST_TOOLS => list_tools(request),
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
        "git.clone" | "github.clone" => invoke_clone(&invocation),
        _ => ToolInvocationResponse {
            output: format!("unsupported Git tool: {}", invocation.name),
            is_error: true,
            content: Vec::new(),
            full_output: None,
            presentation: None,
        },
    };
    json_response(&response)
}

fn invoke_clone(invocation: &ToolInvocationRequest) -> ToolInvocationResponse {
    let request = match serde_json::from_value::<CloneRequest>(invocation.arguments.clone()) {
        Ok(request) => request,
        Err(error) => return tool_error(error.to_string()),
    };
    match clone_repository(&request, invocation.artifact_dir.as_deref()) {
        Ok(response) => json_tool_response(&response),
        Err(error) => tool_error(error.to_string()),
    }
}

#[derive(Debug, Clone, Deserialize)]
struct CloneRequest {
    url: String,
    #[serde(default, alias = "branch")]
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
    artifact_dir: Option<&Path>,
) -> Result<CloneResponse, GitError> {
    let remote = parse_git_remote(&request.url)?;
    let base = request
        .destination
        .clone()
        .unwrap_or_else(|| default_destination(artifact_dir, &remote));
    if base.exists() {
        return Ok(clone_response(request, remote, base, true));
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
    Ok(clone_response(request, remote, base, false))
}

fn clone_response(
    request: &CloneRequest,
    remote: GitRemote,
    path: PathBuf,
    already_exists: bool,
) -> CloneResponse {
    CloneResponse {
        url: request.url.clone(),
        clone_url: remote.clone_url,
        host: remote.host,
        owner: remote.owner,
        repo: remote.repo,
        git_ref: request.git_ref.clone(),
        artifact_kind: "git_repo_clone".to_string(),
        artifact_scope: artifact_scope(request.destination.as_ref()),
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

fn default_destination(artifact_dir: Option<&Path>, remote: &GitRemote) -> PathBuf {
    let mut path = artifact_dir
        .map_or_else(default_global_artifact_dir, Path::to_path_buf)
        .join("git")
        .join(sanitize_path_component(&remote.host));
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

fn artifact_scope(explicit_destination: Option<&PathBuf>) -> String {
    if explicit_destination.is_some() {
        "explicit".to_string()
    } else {
        "session".to_string()
    }
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
        side_effect: ToolSideEffect::WriteFiles,
        requires_permission: true,
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

fn json_tool_response<T: Serialize>(value: &T) -> ToolInvocationResponse {
    match serde_json::to_string_pretty(value) {
        Ok(output) => ToolInvocationResponse {
            output,
            is_error: false,
            content: Vec::new(),
            full_output: None,
            presentation: None,
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
        presentation: None,
    }
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(GitPlugin, include_str!("../bcode-plugin.toml"))
}

bcode_plugin_sdk::export_plugin!(GitPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

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
    fn default_destination_uses_artifact_dir_not_workspace() {
        let remote = parse_git_remote("https://gitlab.com/group/project").expect("repo");
        let path = default_destination(Some(Path::new("/tmp/artifacts/session-1")), &remote);
        assert_eq!(
            path,
            PathBuf::from("/tmp/artifacts/session-1/git/gitlab.com/group/project")
        );
    }
}
