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
use worktree_setup_config::{discover_configs, load_config, resolve_profiles};
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
    /// Worktree removal was refused.
    #[error("worktree removal refused: {0}")]
    RemoveRefused(String),
    /// Worktree setup failed.
    #[error("worktree setup failed: {0}")]
    Setup(String),
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
        apply_setup(config, &repo_root, &path)?;
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
    let list = list_worktrees(cwd)?;
    let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let Some(worktree) = list.worktrees.iter().find(|worktree| {
        worktree
            .path
            .canonicalize()
            .unwrap_or_else(|_| worktree.path.clone())
            == target
    }) else {
        return Err(WorktreeError::RemoveRefused(format!(
            "{} is not a registered worktree",
            path.display()
        )));
    };
    if worktree.is_main {
        return Err(WorktreeError::RemoveRefused(
            "refusing to remove the main worktree".to_string(),
        ));
    }
    if !force && worktree_is_dirty(path) {
        return Err(WorktreeError::RemoveRefused(format!(
            "{} has uncommitted changes; use force to remove it",
            path.display()
        )));
    }
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

fn worktree_is_dirty(cwd: &Path) -> bool {
    run_git(cwd, &["status", "--porcelain"]).is_some_and(|status| !status.trim().is_empty())
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

fn apply_setup(config: &BcodeConfig, repo_root: &Path, path: &Path) -> Result<(), WorktreeError> {
    let config_paths =
        discover_configs(repo_root).map_err(|error| WorktreeError::Setup(error.to_string()))?;
    if config_paths.is_empty() {
        return Ok(());
    }
    let loaded = config_paths
        .iter()
        .filter_map(|config_path| load_config(config_path, repo_root).ok())
        .collect::<Vec<_>>();
    if loaded.is_empty() {
        return Ok(());
    }
    let selected = if let Some(profile) = config.worktree.setup.profile.as_deref() {
        let profile_names = vec![profile.to_string()];
        let resolved = resolve_profiles(&profile_names, &loaded, repo_root)
            .map_err(|error| WorktreeError::Setup(error.to_string()))?;
        resolved
            .config_indices
            .into_iter()
            .filter_map(|index| loaded.get(index))
            .collect::<Vec<_>>()
    } else {
        loaded.iter().collect::<Vec<_>>()
    };
    for loaded_config in selected {
        let options = worktree_setup_operations::ApplyConfigOptions {
            copy_unstaged: None,
            overwrite_existing: false,
            allow_path_escape: loaded_config.config.allow_path_escape.unwrap_or(false),
        };
        worktree_setup_operations::apply_config(loaded_config, repo_root, path, &options)
            .map_err(|error| WorktreeError::Setup(error.to_string()))?;
        for command in &loaded_config.config.post_setup {
            run_setup_command(path, command)?;
        }
    }
    Ok(())
}

fn run_setup_command(cwd: &Path, command: &str) -> Result<(), WorktreeError> {
    let output = if cfg!(windows) {
        Command::new("cmd")
            .args(["/C", command])
            .current_dir(cwd)
            .output()?
    } else {
        Command::new("sh")
            .args(["-c", command])
            .current_dir(cwd)
            .output()?
    };
    if output.status.success() {
        return Ok(());
    }
    Err(WorktreeError::Setup(format!(
        "setup command failed with status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    )))
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
    use super::{create_worktree, list_worktrees, remove_worktree, slugify};
    use bcode_worktree_models::WorktreeCreateRequest;
    use std::path::Path;
    use std::process::Command;
    use tempfile::TempDir;

    struct TempRepo {
        _temp: TempDir,
        root: std::path::PathBuf,
    }

    impl TempRepo {
        fn init() -> Self {
            let temp = tempfile::tempdir().expect("temp dir should be created");
            run(temp.path(), &["init", "--initial-branch", "main"]);
            run(temp.path(), &["config", "user.email", "bcode@example.test"]);
            run(temp.path(), &["config", "user.name", "Bcode Test"]);
            std::fs::write(temp.path().join("README.md"), "test\n")
                .expect("readme should be written");
            run(temp.path(), &["add", "README.md"]);
            run(temp.path(), &["commit", "-m", "initial"]);
            Self {
                root: temp.path().to_path_buf(),
                _temp: temp,
            }
        }
    }

    fn run(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git should run");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn create_request(name: &str) -> WorktreeCreateRequest {
        WorktreeCreateRequest {
            name: name.to_string(),
            cwd: None,
            path: None,
            branch: None,
            new_branch: None,
            base_ref: None,
            detach: false,
            force: false,
            attach_session_id: None,
            new_session: false,
            no_setup: true,
        }
    }

    #[test]
    fn slugify_normalizes_task_names() {
        assert_eq!(slugify("Feature Auth"), "feature-auth");
        assert_eq!(slugify(" fix:issue!! "), "fix-issue");
    }

    #[test]
    fn list_worktrees_includes_main_worktree() {
        let repo = TempRepo::init();

        let response = list_worktrees(&repo.root).expect("worktrees should list");

        assert_eq!(
            response
                .repo_root
                .canonicalize()
                .expect("repo root canonical"),
            repo.root.canonicalize().expect("temp root canonical")
        );
        assert!(response.worktrees.iter().any(|worktree| worktree.is_main));
    }

    #[test]
    fn create_worktree_uses_default_path_and_branch() {
        let repo = TempRepo::init();
        let request = create_request("Feature Auth");

        let response = create_worktree(&bcode_config::BcodeConfig::default(), &request, &repo.root)
            .expect("worktree should be created");

        assert_eq!(response.branch.as_deref(), Some("bcode/feature-auth"));
        assert!(response.path.ends_with(".bcode/worktrees/feature-auth"));
        assert!(response.path.join("README.md").exists());
        let listed = list_worktrees(&repo.root).expect("worktrees should list");
        assert!(
            listed
                .worktrees
                .iter()
                .any(|worktree| worktree.path == response.path)
        );
    }

    #[test]
    fn create_worktree_applies_native_setup_config() {
        let repo = TempRepo::init();
        std::fs::write(repo.root.join(".env"), "TOKEN=test\n").expect("env should be written");
        std::fs::write(
            repo.root.join("worktree.config.toml"),
            "copy = [\".env\"]\n",
        )
        .expect("setup config should be written");
        let mut request = create_request("Setup Copy");
        request.no_setup = false;

        let response = create_worktree(&bcode_config::BcodeConfig::default(), &request, &repo.root)
            .expect("worktree should be created with setup");

        assert!(response.setup_applied);
        assert_eq!(
            std::fs::read_to_string(response.path.join(".env")).expect("env should be copied"),
            "TOKEN=test\n"
        );
    }

    #[test]
    fn remove_worktree_removes_registered_worktree() {
        let repo = TempRepo::init();
        let request = create_request("Remove Me");
        let response = create_worktree(&bcode_config::BcodeConfig::default(), &request, &repo.root)
            .expect("worktree should be created");

        let removed =
            remove_worktree(&repo.root, &response.path, false).expect("worktree should be removed");

        assert_eq!(removed.path, response.path);
        assert!(!removed.path.exists());
    }

    #[test]
    fn remove_worktree_refuses_dirty_worktree_without_force() {
        let repo = TempRepo::init();
        let request = create_request("Dirty Remove");
        let response = create_worktree(&bcode_config::BcodeConfig::default(), &request, &repo.root)
            .expect("worktree should be created");
        std::fs::write(response.path.join("dirty.txt"), "dirty\n")
            .expect("dirty file should be written");

        let error = remove_worktree(&repo.root, &response.path, false)
            .expect_err("dirty worktree removal should be refused");

        assert!(error.to_string().contains("uncommitted changes"));
        remove_worktree(&repo.root, &response.path, true)
            .expect("forced dirty worktree removal should succeed");
    }

    #[test]
    fn remove_worktree_refuses_main_worktree() {
        let repo = TempRepo::init();

        let error = remove_worktree(&repo.root, &repo.root, true)
            .expect_err("main worktree removal should be refused");

        assert!(error.to_string().contains("main worktree"));
    }
}
