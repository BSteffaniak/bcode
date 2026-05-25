#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Bundled GitHub repository access tool plugin for Bcode.

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

/// Bundled GitHub access plugin.
#[derive(Default)]
pub struct GithubPlugin;

impl RustPlugin for GithubPlugin {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        match context.request.interface_id.as_str() {
            TOOL_SERVICE_INTERFACE_ID => invoke_tool_service(&context.request),
            _ => ServiceResponse::error(
                "unsupported_interface",
                "unsupported GitHub plugin service interface",
            ),
        }
    }
}

fn invoke_tool_service(request: &ServiceRequest) -> ServiceResponse {
    match request.operation.as_str() {
        OP_LIST_TOOLS => list_tools(request),
        OP_INVOKE_TOOL => invoke_tool(request),
        _ => ServiceResponse::error(
            "unsupported_operation",
            "unsupported GitHub tool service operation",
        ),
    }
}

fn list_tools(request: &ServiceRequest) -> ServiceResponse {
    if let Err(error) = request.payload_json::<ListToolsRequest>() {
        return invalid_request(&error);
    }
    json_response(&ToolList {
        tools: vec![clone_tool_definition()],
    })
}

fn invoke_tool(request: &ServiceRequest) -> ServiceResponse {
    let invocation = match request.payload_json::<ToolInvocationRequest>() {
        Ok(invocation) => invocation,
        Err(error) => return invalid_request(&error),
    };
    let response = match invocation.name.as_str() {
        "github.clone" => invoke_clone(&invocation),
        _ => ToolInvocationResponse {
            output: format!("unsupported GitHub tool: {}", invocation.name),
            is_error: true,
            content: Vec::new(),
            full_output: None,
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
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    destination: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
struct CloneResponse {
    url: String,
    clone_url: String,
    owner: String,
    repo: String,
    branch: Option<String>,
    artifact_kind: String,
    artifact_scope: String,
    path: PathBuf,
    already_exists: bool,
}

#[derive(Debug, Error)]
enum GithubError {
    #[error("{0}")]
    InvalidRequest(String),
    #[error("git clone failed with status {status}: {stderr}")]
    GitCloneFailed { status: String, stderr: String },
    #[error("failed to run git: {0}")]
    GitIo(#[from] std::io::Error),
}

fn clone_repository(
    request: &CloneRequest,
    artifact_dir: Option<&Path>,
) -> Result<CloneResponse, GithubError> {
    let repo = parse_github_repo(&request.url)?;
    let base = request
        .destination
        .clone()
        .unwrap_or_else(|| default_destination(artifact_dir, &repo.owner, &repo.repo));
    if base.exists() {
        return Ok(CloneResponse {
            url: request.url.clone(),
            clone_url: repo.clone_url(),
            owner: repo.owner,
            repo: repo.repo,
            branch: request.branch.clone(),
            artifact_kind: "github_repo_clone".to_string(),
            artifact_scope: artifact_scope(request.destination.as_ref()),
            path: base,
            already_exists: true,
        });
    }
    if let Some(parent) = base.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut command = Command::new("git");
    command.arg("clone").arg("--depth").arg("1");
    if let Some(branch) = request
        .branch
        .as_deref()
        .filter(|branch| !branch.trim().is_empty())
    {
        command.arg("--branch").arg(branch);
    }
    command.arg(repo.clone_url()).arg(&base);
    let output = command.output()?;
    if !output.status.success() {
        return Err(GithubError::GitCloneFailed {
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        });
    }
    Ok(CloneResponse {
        url: request.url.clone(),
        clone_url: repo.clone_url(),
        owner: repo.owner,
        repo: repo.repo,
        branch: request.branch.clone(),
        artifact_kind: "github_repo_clone".to_string(),
        artifact_scope: artifact_scope(request.destination.as_ref()),
        path: base,
        already_exists: false,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GithubRepo {
    owner: String,
    repo: String,
}

impl GithubRepo {
    fn clone_url(&self) -> String {
        format!("https://github.com/{}/{}.git", self.owner, self.repo)
    }
}

fn parse_github_repo(url: &str) -> Result<GithubRepo, GithubError> {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("https://github.com/") || lower.starts_with("http://github.com/")) {
        return Err(GithubError::InvalidRequest(
            "url must be an http(s) GitHub repository URL".to_string(),
        ));
    }
    let path = url
        .trim_start_matches("https://github.com/")
        .trim_start_matches("http://github.com/");
    let mut segments = path.split('/').filter(|segment| !segment.is_empty());
    let Some(owner) = segments.next() else {
        return Err(GithubError::InvalidRequest(
            "missing GitHub owner".to_string(),
        ));
    };
    let Some(repo) = segments.next() else {
        return Err(GithubError::InvalidRequest(
            "missing GitHub repo".to_string(),
        ));
    };
    let repo = repo.trim_end_matches(".git");
    if owner == "features" || owner == "topics" || owner == "trending" || owner == "marketplace" {
        return Err(GithubError::InvalidRequest(
            "url is not a GitHub repository".to_string(),
        ));
    }
    Ok(GithubRepo {
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

fn default_destination(artifact_dir: Option<&Path>, owner: &str, repo: &str) -> PathBuf {
    artifact_dir
        .map_or_else(default_global_artifact_dir, Path::to_path_buf)
        .join("github")
        .join(owner)
        .join(repo)
}

fn default_global_artifact_dir() -> PathBuf {
    default_state_dir().join("artifacts").join("github")
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
        name: "github.clone".to_string(),
        description: "Shallow-clone a GitHub repository into Bcode-managed artifact state so agents can inspect real files instead of rendered HTML.".to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["url"],
            "properties": {
                "url": { "type": "string" },
                "branch": { "type": "string" },
                "destination": { "type": "string" }
            }
        }),
        side_effect: ToolSideEffect::WriteFiles,
        requires_permission: true,
    }
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
    }
}

#[cfg(feature = "static-bundled")]
#[must_use]
pub fn static_plugin() -> bcode_plugin_sdk::StaticPluginVtable {
    bcode_plugin_sdk::static_plugin_vtable!(GithubPlugin, include_str!("../bcode-plugin.toml"))
}

bcode_plugin_sdk::export_plugin!(GithubPlugin, include_str!("../bcode-plugin.toml"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_repo_urls() {
        let repo = parse_github_repo("https://github.com/bmorphism/bcode").expect("repo");
        assert_eq!(repo.owner, "bmorphism");
        assert_eq!(repo.repo, "bcode");
        assert_eq!(repo.clone_url(), "https://github.com/bmorphism/bcode.git");
    }

    #[test]
    fn rejects_non_repo_urls() {
        assert!(parse_github_repo("https://example.com/repo").is_err());
        assert!(parse_github_repo("https://github.com/features/actions").is_err());
    }

    #[test]
    fn default_destination_uses_artifact_dir_not_workspace() {
        let path =
            default_destination(Some(Path::new("/tmp/artifacts/session-1")), "owner", "repo");
        assert_eq!(
            path,
            PathBuf::from("/tmp/artifacts/session-1/github/owner/repo")
        );
    }
}
