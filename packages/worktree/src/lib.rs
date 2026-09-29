#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Git worktree orchestration for Bcode.

use bcode_config::{BcodeConfig, WorktreeBaseRefConfig};
use bcode_plugin_sdk::path::display_from_current_dir;
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
    let current_worktree = get_workdir(&repo)?;
    let worktrees = get_worktrees(&repo)?
        .into_iter()
        .map(|worktree| WorktreeInfo {
            path: worktree.path,
            is_main: worktree.is_main,
            branch: worktree.branch,
            commit: worktree.commit,
        })
        .collect::<Vec<_>>();
    let repo_root = worktrees
        .iter()
        .find(|worktree| worktree.is_main)
        .map_or_else(
            || current_worktree.clone(),
            |worktree| worktree.path.clone(),
        );
    Ok(WorktreeListResponse {
        repo_root,
        current_worktree,
        worktrees,
    })
}

/// An owning-domain-validated registered Git worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredWorktree {
    path: PathBuf,
    repo_root: PathBuf,
    commit: Option<String>,
}

impl RegisteredWorktree {
    /// Return the canonical worktree directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return the canonical main repository root.
    #[must_use]
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Return the worktree commit reported by Git, when available.
    #[must_use]
    pub fn commit(&self) -> Option<&str> {
        self.commit.as_deref()
    }
}

/// Validate that `path` is a registered worktree of the repository discovered from `cwd`.
///
/// The returned capability carries canonical paths and can be passed across host boundaries
/// without allowing an arbitrary existing directory to masquerade as a declared worktree.
///
/// # Errors
///
/// Returns an error when repository discovery/listing fails, `path` cannot be canonicalized, or
/// it is not registered with the discovered repository.
pub fn validate_registered_worktree(
    cwd: &Path,
    path: &Path,
) -> Result<RegisteredWorktree, WorktreeError> {
    let listed = list_worktrees(cwd)?;
    let requested = path.canonicalize()?;
    let Some(worktree) = listed.worktrees.into_iter().find(|worktree| {
        worktree
            .path
            .canonicalize()
            .is_ok_and(|registered| registered == requested)
    }) else {
        return Err(WorktreeError::InvalidRequest(format!(
            "{} is not a registered worktree of {}",
            display_from_current_dir(path),
            display_from_current_dir(&listed.repo_root)
        )));
    };
    Ok(RegisteredWorktree {
        path: requested,
        repo_root: listed.repo_root.canonicalize()?,
        commit: worktree.commit,
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
    let current_repo_root = get_repo_root(&repo)?;
    let repo_root = get_worktrees(&repo)?
        .into_iter()
        .find(|worktree| worktree.is_main)
        .map_or_else(|| current_repo_root.clone(), |worktree| worktree.path);
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
    let branch_ref = branch_ref_for_create(
        request,
        branch.as_deref(),
        base_ref,
        cwd,
        &current_repo_root,
    )?;
    let source_directory = current_repo_root.canonicalize().map_err(|error| {
        WorktreeError::InvalidRequest(format!("cannot resolve source checkout: {error}"))
    })?;
    let source_has_local_changes = worktree_is_dirty(&source_directory);
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
    let provenance = bcode_worktree_models::WorktreeCreationProvenance {
        source_directory,
        base_commit: current_head_ref(&path)?,
        source_has_local_changes,
    };
    let setup_applied = if config.worktree.setup.enabled && !request.no_setup {
        apply_setup(config, &repo_root, &path)?;
        true
    } else {
        false
    };
    if config.worktree.setup.direnv_allow && !request.no_setup {
        allow_direnv_for_worktree(&path)?;
    }
    Ok(WorktreeCreateResponse {
        repo_root,
        path,
        branch,
        created_branch,
        setup_applied,
        provenance: Some(provenance),
        session: None,
    })
}

/// Remove a registered worktree without deleting its branch.
///
/// Without `force`, refuses dirty (including ignored), unverifiable, or unreferenced
/// work. A detached HEAD must be retained by a branch, tag, or remote-tracking ref.
/// Retention protects the contribution; it does not assert integration or verification.
///
/// # Errors
///
/// Returns an error when repository discovery or removal fails, the target is the
/// main worktree, or safe removal cannot be established without explicit force.
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
            display_from_current_dir(path)
        )));
    };
    if worktree.is_main {
        return Err(WorktreeError::RemoveRefused(
            "refusing to remove the main worktree".to_string(),
        ));
    }
    if !force {
        if worktree_has_pending_operation(path) {
            return Err(WorktreeError::RemoveRefused(
                "unfinished or unverifiable Git operation; resolve, continue or abort it in the worktree before removal".to_string(),
            ));
        }
        if worktree_is_dirty(path) {
            return Err(WorktreeError::RemoveRefused(format!(
                "{} has uncommitted, ignored or unverifiable work; use force to remove it",
                display_from_current_dir(path)
            )));
        }
        // Detached worker commits have no branch retained by removal. Require a
        // durable ref containing HEAD before discarding the checkout; do not count
        // another worktree's HEAD or reflog as retained contribution ownership.
        if !worktree_head_is_retained(path) {
            return Err(WorktreeError::RemoveRefused(
                "HEAD is not verifiably retained by a branch or tag; retain the contribution before removal, or explicitly use force".to_string(),
            ));
        }
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
    if request.branch.is_some() {
        return Ok(branch.map(ToString::to_string));
    }
    // Detachment controls branch ownership, not the selected base commit.
    match base_ref {
        WorktreeBaseRef::Head => current_head_ref(cwd).map(Some),
        WorktreeBaseRef::DefaultBranch => Ok(Some(default_branch_ref(repo_root)?)),
        WorktreeBaseRef::Auto => default_branch_ref(repo_root).map_or_else(
            |_| current_head_ref(cwd).map(Some),
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

fn current_head_ref(cwd: &Path) -> Result<String, WorktreeError> {
    // Resolve the invoking checkout, including detached linked worktrees. Passing
    // no ref delegates base selection to another repository handle's HEAD; passing
    // a branch name allows that ref to move between selection and creation.
    run_git(cwd, &["rev-parse", "--verify", "HEAD^{commit}"]).ok_or_else(|| {
        WorktreeError::InvalidRequest("current HEAD commit could not be resolved".to_string())
    })
}

fn worktree_has_pending_operation(cwd: &Path) -> bool {
    // A resolved conflict can leave a clean index while sequencer/rebase state
    // still owns recovery instructions. Resolve paths through Git: linked
    // worktrees have private operation state, not a local .git directory.
    [
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "MERGE_HEAD",
        "rebase-merge",
        "rebase-apply",
        "sequencer",
        "BISECT_START",
    ]
    .iter()
    .any(|marker| {
        run_git(cwd, &["rev-parse", "--git-path", marker])
            .is_none_or(|path| cwd.join(path).try_exists().unwrap_or(true))
    })
}

fn worktree_is_dirty(cwd: &Path) -> bool {
    // Cleanup must fail closed: an unavailable status is not evidence that user
    // work is absent. Include ignored files and override status configuration so
    // generated artifacts and untracked contributions are not silently deleted.
    run_git(
        cwd,
        &[
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignored",
            "--ignore-submodules=none",
        ],
    )
    .is_none_or(|status| !status.trim().is_empty())
}

fn worktree_head_is_retained(cwd: &Path) -> bool {
    run_git(
        cwd,
        &[
            "for-each-ref",
            "--contains=HEAD",
            "--format=%(refname)",
            "--count=1",
            "refs/heads/",
            "refs/tags/",
            "refs/remotes/",
        ],
    )
    .is_some_and(|refs| !refs.is_empty())
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

fn direnv_file_for(cwd: &Path) -> Option<PathBuf> {
    let mut current = cwd.to_path_buf();
    loop {
        let envrc = current.join(".envrc");
        if envrc.exists() {
            return Some(envrc);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn allow_direnv_for_worktree(path: &Path) -> Result<(), WorktreeError> {
    let Some(envrc) = direnv_file_for(path) else {
        return Ok(());
    };
    let output = Command::new("direnv")
        .arg("allow")
        .arg(&envrc)
        .current_dir(path)
        .output()
        .map_err(|error| {
            WorktreeError::Setup(format!(
                "direnv_allow is enabled, but `direnv allow {}` could not run: {error}",
                display_from_current_dir(&envrc)
            ))
        })?;
    if output.status.success() {
        return Ok(());
    }
    Err(WorktreeError::Setup(format!(
        "direnv_allow is enabled, but `direnv allow {}` failed with status {}: {}",
        display_from_current_dir(&envrc),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )))
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
    use super::{
        create_worktree, direnv_file_for, list_worktrees, remove_worktree, slugify,
        validate_registered_worktree,
    };
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
    fn cleanup_preserves_clean_but_unfinished_conflict_recovery() {
        let repo = TempRepo::init();
        let workspaces = tempfile::tempdir().unwrap();
        let worker = workspaces.path().join("worker");
        run(
            &repo.root,
            &["worktree", "add", "-b", "worker", worker.to_str().unwrap()],
        );
        std::fs::write(worker.join("README.md"), "worker contribution\n").unwrap();
        run(&worker, &["commit", "-am", "worker contribution"]);
        let contribution = super::current_head_ref(&worker).unwrap();
        std::fs::write(repo.root.join("README.md"), "integration change\n").unwrap();
        run(&repo.root, &["commit", "-am", "integration change"]);
        // Existing user work stays outside the isolated integration checkout.
        std::fs::write(repo.root.join("user.txt"), "keep me\n").unwrap();
        let integration = workspaces.path().join("integration");
        run(
            &repo.root,
            &[
                "worktree",
                "add",
                "-b",
                "integration",
                integration.to_str().unwrap(),
            ],
        );
        let pick = Command::new("git")
            .args(["cherry-pick", &contribution])
            .current_dir(&integration)
            .output()
            .unwrap();
        assert!(!pick.status.success());
        assert!(remove_worktree(&repo.root, &integration, false).is_err());
        // Resolving to the current contents yields an empty pick: status is
        // clean, but removing now would silently discard the pending decision.
        run(&integration, &["checkout", "--ours", "README.md"]);
        run(&integration, &["add", "README.md"]);
        assert!(!super::worktree_is_dirty(&integration));
        let error = remove_worktree(&repo.root, &integration, false).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unfinished or unverifiable Git operation")
        );
        // Recovery is explicit; cleanup never decides to discard a contribution.
        run(&integration, &["cherry-pick", "--abort"]);
        remove_worktree(&repo.root, &integration, false).unwrap();
        assert_eq!(super::current_head_ref(&worker).unwrap(), contribution);
        assert_eq!(
            std::fs::read_to_string(worker.join("README.md")).unwrap(),
            "worker contribution\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.root.join("user.txt")).unwrap(),
            "keep me\n"
        );
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
    fn head_base_preserves_detached_linked_checkout_provenance() {
        let repo = TempRepo::init();
        let workspaces = tempfile::tempdir().expect("workspace root");
        let source = workspaces.path().join("source");
        run(
            &repo.root,
            &["worktree", "add", "--detach", source.to_str().unwrap()],
        );
        std::fs::write(source.join("contribution.txt"), "worker contribution\n").unwrap();
        run(&source, &["add", "contribution.txt"]);
        run(&source, &["commit", "-m", "detached contribution"]);
        let expected = super::current_head_ref(&source).unwrap();
        assert_ne!(expected, super::current_head_ref(&repo.root).unwrap());
        // Uncommitted user work must neither be lost nor implicitly copied.
        std::fs::write(source.join("user.txt"), "uncommitted\n").unwrap();
        for detached in [false, true] {
            let mut request = create_request(if detached { "detached" } else { "branched" });
            request.path = Some(workspaces.path().join(&request.name));
            request.base_ref = Some(bcode_worktree_models::WorktreeBaseRef::Head);
            request.detach = detached;
            request.no_setup = true;
            let response =
                create_worktree(&bcode_config::BcodeConfig::default(), &request, &source)
                    .expect("create from exact detached source HEAD");
            assert_eq!(super::current_head_ref(&response.path).unwrap(), expected);
            assert_eq!(
                std::fs::read_to_string(response.path.join("contribution.txt")).unwrap(),
                "worker contribution\n"
            );
            assert!(!response.path.join("user.txt").exists());
        }
        assert_eq!(
            std::fs::read_to_string(source.join("user.txt")).unwrap(),
            "uncommitted\n"
        );
    }

    #[test]
    fn head_base_is_pinned_before_source_branch_moves() {
        let repo = TempRepo::init();
        let mut request = create_request("pinned");
        request.base_ref = Some(bcode_worktree_models::WorktreeBaseRef::Head);
        let pinned = super::branch_ref_for_create(
            &request,
            Some("pinned"),
            bcode_worktree_models::WorktreeBaseRef::Head,
            &repo.root,
            &repo.root,
        )
        .unwrap()
        .unwrap();
        run(&repo.root, &["commit", "--allow-empty", "-m", "advance"]);
        assert_ne!(pinned, super::current_head_ref(&repo.root).unwrap());
        let destination = tempfile::tempdir().unwrap();
        let checkout = destination.path().join("pinned");
        run(
            &repo.root,
            &[
                "worktree",
                "add",
                "--detach",
                checkout.to_str().unwrap(),
                &pinned,
            ],
        );
        assert_eq!(super::current_head_ref(&checkout).unwrap(), pinned);
    }

    #[test]
    fn dirty_source_and_conflicting_contributions_remain_recoverable() {
        let repo = TempRepo::init();
        let workspaces = tempfile::tempdir().unwrap();
        std::fs::write(repo.root.join("README.md"), "user draft\n").unwrap();
        std::fs::write(repo.root.join("notes.txt"), "user notes\n").unwrap();
        let base = super::current_head_ref(&repo.root).unwrap();
        let create = |name: &str| {
            let mut request = create_request(name);
            request.path = Some(workspaces.path().join(name));
            request.base_ref = Some(bcode_worktree_models::WorktreeBaseRef::Head);
            create_worktree(&bcode_config::BcodeConfig::default(), &request, &repo.root).unwrap()
        };
        let first = create("first");
        let second = create("second");
        let target = create("integration");
        let mut historical = serde_json::to_value(&target).unwrap();
        historical.as_object_mut().unwrap().remove("provenance");
        let historical: bcode_worktree_models::WorktreeCreateResponse =
            serde_json::from_value(historical).unwrap();
        assert!(historical.provenance.is_none());
        for workspace in [&first, &second, &target] {
            let provenance = workspace.provenance.as_ref().unwrap();
            assert_eq!(provenance.base_commit, base);
            assert_eq!(
                provenance.source_directory,
                repo.root.canonicalize().unwrap()
            );
            assert!(provenance.source_has_local_changes);
            assert!(!workspace.path.join("notes.txt").exists());
        }
        for (workspace, contents) in [(&first, "first\n"), (&second, "second\n")] {
            std::fs::write(workspace.path.join("README.md"), contents).unwrap();
            run(&workspace.path, &["add", "README.md"]);
            run(&workspace.path, &["commit", "-m", "contribution"]);
        }
        let first_commit = super::current_head_ref(&first.path).unwrap();
        let second_commit = super::current_head_ref(&second.path).unwrap();
        run(&target.path, &["cherry-pick", &first_commit]);
        let conflict = Command::new("git")
            .args(["cherry-pick", &second_commit])
            .current_dir(&target.path)
            .output()
            .unwrap();
        assert!(!conflict.status.success());
        assert!(remove_worktree(&repo.root, &target.path, false).is_err());
        assert_eq!(
            super::current_head_ref(&second.path).unwrap(),
            second_commit
        );
        assert_eq!(
            std::fs::read_to_string(repo.root.join("README.md")).unwrap(),
            "user draft\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo.root.join("notes.txt")).unwrap(),
            "user notes\n"
        );
        // Explicit resolution, not a successful worker summary, establishes the
        // combined artifact. All Git mutations here are confined to temporary repos.
        std::fs::write(target.path.join("README.md"), "first\nsecond\n").unwrap();
        run(&target.path, &["add", "README.md"]);
        run(
            &target.path,
            &["-c", "core.editor=true", "cherry-pick", "--continue"],
        );
        assert_eq!(
            std::fs::read_to_string(target.path.join("README.md")).unwrap(),
            "first\nsecond\n"
        );
        for workspace in [&first, &second, &target] {
            remove_worktree(&repo.root, &workspace.path, false).unwrap();
            assert!(!workspace.path.exists());
            assert!(
                super::run_git(
                    &repo.root,
                    &["rev-parse", "--verify", workspace.branch.as_ref().unwrap()]
                )
                .is_some()
            );
        }
    }

    #[test]
    fn head_base_rejects_unborn_checkout() {
        let repo = tempfile::tempdir().unwrap();
        run(repo.path(), &["init", "--initial-branch", "main"]);
        assert!(super::current_head_ref(repo.path()).is_err());
    }

    #[test]
    fn registered_worktree_validation_rejects_arbitrary_directory() {
        let repo = TempRepo::init();
        let arbitrary = tempfile::tempdir().expect("arbitrary directory");

        let error = validate_registered_worktree(&repo.root, arbitrary.path())
            .expect_err("unregistered directory rejected");

        assert!(error.to_string().contains("not a registered worktree"));
    }

    #[test]
    fn registered_worktree_validation_returns_canonical_identity() {
        let repo = TempRepo::init();
        let registered = validate_registered_worktree(&repo.root, &repo.root)
            .expect("main worktree is registered");

        assert_eq!(
            registered.path(),
            repo.root.canonicalize().expect("canonical root")
        );
        assert_eq!(registered.repo_root(), registered.path());
        assert!(registered.commit().is_some());
    }

    #[test]
    fn create_worktree_from_linked_worktree_resolves_paths_from_main_worktree() {
        let repo = TempRepo::init();
        let sibling_root = repo
            .root
            .parent()
            .expect("repo should have parent")
            .join(format!(
                "wt-{}",
                repo.root
                    .file_name()
                    .expect("repo should have file name")
                    .to_string_lossy()
            ));
        let leaf = sibling_root.join("leaf");
        run(
            &repo.root,
            &[
                "worktree",
                "add",
                leaf.to_str().expect("leaf path should be utf-8"),
                "-b",
                "leaf",
            ],
        );
        let mut config = bcode_config::BcodeConfig::default();
        config.worktree.root = std::path::PathBuf::from(format!(
            "../{}",
            sibling_root
                .file_name()
                .expect("sibling root should have file name")
                .to_string_lossy()
        ));
        let request = create_request("From Leaf");

        let response =
            create_worktree(&config, &request, &leaf).expect("worktree should be created");

        assert_eq!(
            response
                .path
                .canonicalize()
                .expect("response path canonical"),
            sibling_root
                .join("from-leaf")
                .canonicalize()
                .expect("expected path canonical")
        );
        assert_eq!(
            response
                .repo_root
                .canonicalize()
                .expect("repo root canonical"),
            repo.root.canonicalize().expect("temp root canonical")
        );
    }

    #[test]
    fn list_worktrees_from_linked_worktree_reports_main_repo_root() {
        let repo = TempRepo::init();
        let sibling_root = repo
            .root
            .parent()
            .expect("repo should have parent")
            .join(format!(
                "wt-{}",
                repo.root
                    .file_name()
                    .expect("repo should have file name")
                    .to_string_lossy()
            ));
        let leaf = sibling_root.join("list-leaf");
        run(
            &repo.root,
            &[
                "worktree",
                "add",
                leaf.to_str().expect("leaf path should be utf-8"),
                "-b",
                "list-leaf",
            ],
        );

        let response = list_worktrees(&leaf).expect("worktrees should list");

        assert_eq!(
            response
                .repo_root
                .canonicalize()
                .expect("repo root canonical"),
            repo.root.canonicalize().expect("temp root canonical")
        );
        assert_eq!(
            response
                .current_worktree
                .canonicalize()
                .expect("current worktree canonical"),
            leaf.canonicalize().expect("leaf canonical")
        );
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
    fn direnv_file_for_finds_parent_envrc() {
        let temp = TempDir::new().expect("tempdir");
        let nested = temp.path().join("a/b");
        std::fs::create_dir_all(&nested).expect("nested dir");
        let envrc = temp.path().join(".envrc");
        std::fs::write(&envrc, "use flake\n").expect("envrc");

        assert_eq!(direnv_file_for(&nested), Some(envrc));
    }

    #[test]
    fn direnv_file_for_returns_none_without_envrc() {
        let temp = TempDir::new().expect("tempdir");

        assert_eq!(direnv_file_for(temp.path()), None);
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

        assert!(error.to_string().contains("uncommitted"));
        remove_worktree(&repo.root, &response.path, true)
            .expect("forced dirty worktree removal should succeed");
    }

    #[test]
    fn detached_contribution_requires_retained_ref_before_cleanup() {
        let repo = TempRepo::init();
        let mut request = create_request("Detached Contribution");
        request.detach = true;
        let response = create_worktree(&bcode_config::BcodeConfig::default(), &request, &repo.root)
            .expect("detached worktree");
        std::fs::write(response.path.join("result.txt"), "worker result\n").expect("result");
        run(&response.path, &["add", "result.txt"]);
        run(&response.path, &["commit", "-m", "worker contribution"]);

        let error = remove_worktree(&repo.root, &response.path, false)
            .expect_err("unreferenced contribution must survive");
        assert!(error.to_string().contains("retain the contribution"));
        assert_eq!(
            std::fs::read_to_string(response.path.join("result.txt")).expect("preserved result"),
            "worker result\n"
        );

        run(&response.path, &["branch", "retained-contribution"]);
        remove_worktree(&repo.root, &response.path, false).expect("retained contribution cleanup");
        let output = Command::new("git")
            .args(["show", "retained-contribution:result.txt"])
            .current_dir(&repo.root)
            .output()
            .expect("read retained result");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"worker result\n");
    }

    #[test]
    fn remove_worktree_preserves_ignored_and_hidden_untracked_contributions() {
        for ignored in [false, true] {
            let repo = TempRepo::init();
            run(&repo.root, &["config", "status.showUntrackedFiles", "no"]);
            if ignored {
                std::fs::write(repo.root.join(".gitignore"), "contribution.txt\n")
                    .expect("ignore rule");
                run(&repo.root, &["add", ".gitignore"]);
                run(
                    &repo.root,
                    &["commit", "-m", "ignore generated contribution"],
                );
            }
            let response = create_worktree(
                &bcode_config::BcodeConfig::default(),
                &create_request("Retain Contribution"),
                &repo.root,
            )
            .expect("worktree");
            let contribution = response.path.join("contribution.txt");
            std::fs::write(&contribution, "unintegrated worker result\n").expect("contribution");

            remove_worktree(&repo.root, &response.path, false)
                .expect_err("cleanup must preserve contributions");

            assert_eq!(
                std::fs::read_to_string(contribution).expect("retained contribution"),
                "unintegrated worker result\n"
            );
            assert!(
                list_worktrees(&repo.root)
                    .expect("registered worktrees")
                    .worktrees
                    .iter()
                    .any(|worktree| worktree.path == response.path)
            );
        }
    }

    #[test]
    fn unavailable_worktree_status_is_not_clean() {
        let temp = TempDir::new().expect("directory without repository");
        assert!(super::worktree_is_dirty(temp.path()));
        assert!(super::worktree_is_dirty(&temp.path().join("missing")));
    }

    #[test]
    fn remove_worktree_refuses_main_worktree() {
        let repo = TempRepo::init();

        let error = remove_worktree(&repo.root, &repo.root, true)
            .expect_err("main worktree removal should be refused");

        assert!(error.to_string().contains("main worktree"));
    }
}
