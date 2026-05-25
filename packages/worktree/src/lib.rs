#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Git worktree orchestration for Bcode.

use bcode_config::{BcodeConfig, WorktreeBaseRefConfig};
use bcode_worktree_models::{
    WorktreeBaseRef, WorktreeCreateRequest, WorktreeCreateResponse, WorktreeInfo,
    WorktreeListResponse, WorktreeRemoveResponse,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use thiserror::Error;
use worktree_setup_git::{
    WorktreeCreateOptions, create_worktree as setup_create_worktree, discover_repo, get_repo_root,
    get_workdir, get_worktrees, remove_worktree as setup_remove_worktree,
};

/// Errors returned by Bcode worktree operations.
#[derive(Debug, Error)]
pub enum WorktreeError {
    /// Git operation failed.
    #[error("git worktree operation failed: {0}")]
    Git(#[from] worktree_setup_git::GitError),
    /// Worktree request was invalid.
    #[error("invalid worktree request: {0}")]
    InvalidRequest(String),
    /// Worktree setup failed.
    #[error("worktree setup failed with status {status}: {stderr}")]
    SetupFailed { status: String, stderr: String },
    /// I/O failed.
    #[error("worktree I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

/// List registered worktrees for the repository discovered from `cwd`.
///
/// # Errors
///
/// Returns an error when repository discovery or worktree listing fails.
pub fn list_worktrees(cwd: &Path) -> Result<WorktreeListResponse, WorktreeError> {
    let repo = discover_repo(cwd)?;
    let repo_root = get_repo_root(&repo)?;
    let current_worktree = get_workdir(&repo)?;
    let worktrees = get_worktrees(&repo)?
        .into_iter()
        .map(|worktree| WorktreeInfo {
            path: worktree.path,
            is_main: worktree.is_main,
            branch: worktree.branch,
            commit: worktree.commit,
        })
        .collect();
    Ok(WorktreeListResponse {
        repo_root,
        current_worktree,
        worktrees,
    })
}

/// Create a worktree using Bcode defaults and configuration.
///
/// # Errors
///
/// Returns an error when the request is invalid, git worktree creation fails,
/// or automatic setup fails.
pub fn create_worktree(
    config: &BcodeConfig,
    request: &WorktreeCreateRequest,
    cwd: &Path,
) -> Result<WorktreeCreateResponse, WorktreeError> {
    validate_create_request(request)?;
    let repo = discover_repo(cwd)?;
    let repo_root = get_repo_root(&repo)?;
    let slug = slugify(&request.name);
    let path = request.path.clone().map_or_else(
        || configured_worktree_root(config, &repo_root).join(&slug),
        |path| resolve_path(&repo_root, &path),
    );
    let branch = requested_branch(config, request, &slug);
    let created_branch = !request.detach && request.branch.is_none();
    let base_ref = request
        .base_ref
        .unwrap_or_else(|| base_ref_from_config(config.worktree.base_ref));
    let branch_ref = branch_ref_for_create(request, branch.as_deref(), base_ref, cwd, &repo_root)?;
    setup_create_worktree(
        &repo,
        &path,
        &WorktreeCreateOptions {
            branch: branch_ref,
            new_branch: created_branch.then(|| branch.clone()).flatten(),
            detach: request.detach,
            force: request.force,
        },
    )?;
    let setup_applied = if config.worktree.setup.enabled && !request.no_setup {
        apply_setup(&repo_root, &path)?;
        true
    } else {
        false
    };
    Ok(WorktreeCreateResponse {
        repo_root,
        path,
        branch,
        created_branch,
        setup_applied,
        session: None,
    })
}

/// Remove a registered worktree without deleting its branch.
///
/// # Errors
///
/// Returns an error when repository discovery or removal fails.
pub fn remove_worktree(
    cwd: &Path,
    path: &Path,
    force: bool,
) -> Result<WorktreeRemoveResponse, WorktreeError> {
    let repo = discover_repo(cwd)?;
    setup_remove_worktree(&repo, path, force)?;
    Ok(WorktreeRemoveResponse {
        path: path.to_path_buf(),
    })
}

fn validate_create_request(request: &WorktreeCreateRequest) -> Result<(), WorktreeError> {
    if request.name.trim().is_empty() {
        return Err(WorktreeError::InvalidRequest(
            "worktree name must not be empty".to_string(),
        ));
    }
    if request.detach && (request.branch.is_some() || request.new_branch.is_some()) {
        return Err(WorktreeError::InvalidRequest(
            "detached worktrees cannot also specify a branch".to_string(),
        ));
    }
    if request.branch.is_some() && request.new_branch.is_some() {
        return Err(WorktreeError::InvalidRequest(
            "choose either branch or new_branch, not both".to_string(),
        ));
    }
    Ok(())
}

fn configured_worktree_root(config: &BcodeConfig, repo_root: &Path) -> PathBuf {
    resolve_path(repo_root, &config.worktree.root)
}

fn resolve_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn requested_branch(
    config: &BcodeConfig,
    request: &WorktreeCreateRequest,
    slug: &str,
) -> Option<String> {
    if request.detach {
        None
    } else {
        request.branch.clone().or_else(|| {
            request
                .new_branch
                .clone()
                .or_else(|| Some(format!("{}{}", config.worktree.branch_prefix, slug)))
        })
    }
}

fn branch_ref_for_create(
    request: &WorktreeCreateRequest,
    branch: Option<&str>,
    base_ref: WorktreeBaseRef,
    cwd: &Path,
    repo_root: &Path,
) -> Result<Option<String>, WorktreeError> {
    if request.detach {
        return Ok(None);
    }
    if request.branch.is_some() {
        return Ok(branch.map(ToString::to_string));
    }
    match base_ref {
        WorktreeBaseRef::Head => Ok(current_head_ref(cwd)),
        WorktreeBaseRef::DefaultBranch => Ok(Some(default_branch_ref(repo_root)?)),
        WorktreeBaseRef::Auto => default_branch_ref(repo_root).map_or_else(
            |_| Ok(current_head_ref(cwd)),
            |default_branch| Ok(Some(default_branch)),
        ),
    }
}

fn default_branch_ref(repo_root: &Path) -> Result<String, WorktreeError> {
    discover_repo(repo_root)?;
    run_git(
        repo_root,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .map(|branch| branch.trim_start_matches("origin/").to_string())
    .or_else(|| run_git(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"]))
    .ok_or_else(|| {
        WorktreeError::InvalidRequest("default branch could not be resolved".to_string())
    })
}

fn current_head_ref(cwd: &Path) -> Option<String> {
    run_git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
        .and_then(|value| if value == "HEAD" { None } else { Some(value) })
}

fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn apply_setup(repo_root: &Path, path: &Path) -> Result<(), WorktreeError> {
    if !repo_root.join("worktree.config.toml").exists()
        && !path.join("worktree.config.toml").exists()
    {
        return Ok(());
    }
    let output = Command::new("worktree-setup")
        .arg("setup")
        .arg(path)
        .arg("--non-interactive")
        .current_dir(repo_root)
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(WorktreeError::SetupFailed {
        status: output.status.to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    })
}

const fn base_ref_from_config(config: WorktreeBaseRefConfig) -> WorktreeBaseRef {
    match config {
        WorktreeBaseRefConfig::Auto => WorktreeBaseRef::Auto,
        WorktreeBaseRefConfig::DefaultBranch => WorktreeBaseRef::DefaultBranch,
        WorktreeBaseRefConfig::Head => WorktreeBaseRef::Head,
    }
}

fn slugify(value: &str) -> String {
    let mut slug = String::new();
    for character in value.trim().chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if matches!(character, '-' | '_' | '.') {
            slug.push(character);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::slugify;

    #[test]
    fn slugify_normalizes_task_names() {
        assert_eq!(slugify("Feature Auth"), "feature-auth");
        assert_eq!(slugify(" fix:issue!! "), "fix-issue");
    }
}
