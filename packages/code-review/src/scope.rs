//! Transient workspaces and the annotation scope of a review workspace.
//!
//! Drafts, threads, AI exchanges, and suggestions are stored under the review key
//! `review-workspace-{id}`, where `id` is [`annotation_workspace_id`]:
//!
//! * A durable workspace uses its own id, so its key never changes.
//! * A transient (unsaved) workspace uses [`TRANSIENT_WORKSPACE_ID`] plus a fingerprint of its
//!   repository and source kinds, so reopening the same target shows its annotations again
//!   while transient reviews of other targets (or other repositories) do not see them.

use bcode_code_review_models::{
    ReviewScope, ReviewSource, ReviewSourceKind, ReviewTarget, ReviewWorkspace,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Id of workspaces that are not saved (yet).
pub const TRANSIENT_WORKSPACE_ID: &str = "transient-review-workspace";

/// Hex digits of the fingerprint in a transient scope id.
const FINGERPRINT_HEX_LEN: usize = 32;

/// A transient workspace reviewing `sources` (ids `source-1`, `source-2`, …).
#[must_use]
pub fn transient_workspace(
    repo_root: PathBuf,
    title: Option<String>,
    sources: Vec<ReviewSourceKind>,
) -> ReviewWorkspace {
    let sources: Vec<ReviewSource> = sources
        .into_iter()
        .enumerate()
        .map(|(index, kind)| ReviewSource {
            id: format!("source-{}", index + 1),
            label: kind.label(),
            kind,
            included: true,
        })
        .collect();
    let title = title.unwrap_or_else(|| default_title(&sources));
    ReviewWorkspace {
        id: TRANSIENT_WORKSPACE_ID.to_owned(),
        title,
        repo_root,
        sources,
        created_at_ms: None,
        updated_at_ms: None,
        viewed_files: BTreeSet::new(),
        archived_at_ms: None,
        viewed_marks: Vec::new(),
        viewed_threads: Vec::new(),
        reviewed_revisions: BTreeMap::new(),
        presentation: None,
        stack_snapshots: Vec::new(),
    }
}

/// Title for a review of `sources`: the only source's label, else the first label and a count.
#[must_use]
pub fn default_title(sources: &[ReviewSource]) -> String {
    match sources {
        [] => "Untitled review".to_owned(),
        [only] => only.label.clone(),
        [first, rest @ ..] => format!("{} +{}", first.label, rest.len()),
    }
}

/// Whether `workspace` is transient (not saved).
#[must_use]
pub fn is_transient(workspace: &ReviewWorkspace) -> bool {
    workspace.id == TRANSIENT_WORKSPACE_ID
}

/// Whether a scope's workspace id names a transient review.
///
/// That is [`TRANSIENT_WORKSPACE_ID`] itself (annotations written before transient scopes were
/// fingerprinted) or a fingerprinted id from [`annotation_workspace_id`].
#[must_use]
pub fn is_transient_scope_id(workspace_id: &str) -> bool {
    workspace_id
        .strip_prefix(TRANSIENT_WORKSPACE_ID)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('-'))
}

/// The workspace id annotation requests of `workspace` are scoped by.
///
/// * Durable workspaces: their id.
/// * Transient workspaces: `transient-review-workspace-{fingerprint}`, where the fingerprint
///   hashes the repository root and each source's kind in order. Source ids, labels, and the
///   included flag are not part of it, so the same target opened from any entry point (CLI,
///   home suggestion, wizard) shares one scope.
#[must_use]
pub fn annotation_workspace_id(workspace: &ReviewWorkspace) -> String {
    if !is_transient(workspace) {
        return workspace.id.clone();
    }
    let mut hasher = Sha256::new();
    hasher.update(workspace.repo_root.display().to_string().as_bytes());
    for source in &workspace.sources {
        hasher.update(b"\0");
        // Source kinds always encode; the label is a stable fallback all the same.
        let kind =
            serde_json::to_vec(&source.kind).unwrap_or_else(|_| source.kind.label().into_bytes());
        hasher.update(&kind);
    }
    let digest = hex::encode(hasher.finalize());
    format!(
        "{TRANSIENT_WORKSPACE_ID}-{}",
        &digest[..FINGERPRINT_HEX_LEN]
    )
}

/// The fallback target and scope annotation requests of `workspace` use.
///
/// The scope is its annotation scope id (see [`annotation_workspace_id`]); the fallback target is
/// the first included source's target (the repository when that source has none expressible as a
/// target).
#[must_use]
pub fn annotation_scope(workspace: &ReviewWorkspace) -> (ReviewTarget, ReviewScope) {
    let target = workspace
        .sources
        .iter()
        .find(|source| source.included)
        .map_or(ReviewTarget::Repository, |source| {
            target_of_kind(&source.kind)
        });
    let scope = ReviewScope::Workspace {
        workspace_id: annotation_workspace_id(workspace),
        target: target.clone(),
    };
    (target, scope)
}

/// The target of a source kind (the repository for kinds without one).
#[must_use]
pub fn target_of_kind(kind: &ReviewSourceKind) -> ReviewTarget {
    match kind {
        ReviewSourceKind::WorkingTreeUnstaged => ReviewTarget::WorkingTreeUnstaged,
        ReviewSourceKind::IndexStaged => ReviewTarget::IndexStaged,
        ReviewSourceKind::WorkingTreeAndIndex => ReviewTarget::WorkingTreeAndIndex,
        ReviewSourceKind::LastCommit => ReviewTarget::LastCommit,
        ReviewSourceKind::Commit { rev } => ReviewTarget::CommitRange {
            base: format!("{rev}^"),
            head: rev.clone(),
            merge_base: false,
        },
        ReviewSourceKind::CommitRange {
            base,
            head,
            merge_base,
        } => ReviewTarget::CommitRange {
            base: base.clone(),
            head: head.clone(),
            merge_base: *merge_base,
        },
        ReviewSourceKind::BranchCompare {
            base_branch,
            head_branch,
            merge_base,
        } => ReviewTarget::BranchCompare {
            base_branch: base_branch.clone(),
            head_branch: head_branch.clone(),
            merge_base: *merge_base,
        },
        _ => ReviewTarget::Repository,
    }
}

/// The working tree root containing `path`: the nearest of `path` and its ancestors with a
/// `.git` entry (a repository directory or a linked worktree's file), else `path` itself.
///
/// The app opens with this as its repository, so a transient review's scope (which hashes the
/// repository) is the same whether the review was started from the root or a subdirectory.
#[must_use]
pub fn working_tree_root(path: &Path) -> PathBuf {
    path.ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(path)
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_code_review_models::ReviewTarget;

    fn transient(repo: &str, sources: Vec<ReviewSourceKind>) -> ReviewWorkspace {
        transient_workspace(PathBuf::from(repo), None, sources)
    }

    #[test]
    fn transient_workspaces_number_sources_and_derive_titles() {
        let workspace = transient_workspace(
            PathBuf::from("/repo"),
            None,
            vec![
                ReviewSourceKind::CurrentBranch { base: None },
                ReviewSourceKind::WorkingTreeUnstaged,
            ],
        );
        assert_eq!(workspace.id, TRANSIENT_WORKSPACE_ID);
        assert_eq!(
            workspace
                .sources
                .iter()
                .map(|source| source.id.as_str())
                .collect::<Vec<_>>(),
            ["source-1", "source-2"]
        );
        assert_eq!(workspace.title, "Current branch +1");
    }

    #[test]
    fn durable_workspaces_keep_their_own_id() {
        let mut workspace = transient("/repo", vec![ReviewSourceKind::LastCommit]);
        workspace.id = "workspace-abc".to_owned();
        assert_eq!(annotation_workspace_id(&workspace), "workspace-abc");
        assert!(!is_transient_scope_id("workspace-abc"));
    }

    #[test]
    fn transient_scopes_differ_by_target_and_repository_only() {
        let last = transient("/repo", vec![ReviewSourceKind::LastCommit]);
        let staged = transient("/repo", vec![ReviewSourceKind::IndexStaged]);
        let elsewhere = transient("/other", vec![ReviewSourceKind::LastCommit]);
        let both = transient(
            "/repo",
            vec![ReviewSourceKind::LastCommit, ReviewSourceKind::IndexStaged],
        );
        let ids = [&last, &staged, &elsewhere, &both].map(annotation_workspace_id);
        for (index, id) in ids.iter().enumerate() {
            assert!(is_transient_scope_id(id), "{id}");
            assert!(!ids[index + 1..].contains(id), "{id} is shared");
        }
        // The same target from another entry point (other title and labels) shares the scope.
        let mut renamed =
            ReviewWorkspace::from_target(PathBuf::from("/repo"), ReviewTarget::LastCommit);
        renamed.title = "Something else".to_owned();
        renamed.sources[0].label = "Renamed".to_owned();
        renamed.sources[0].id = "source-9".to_owned();
        assert_eq!(annotation_workspace_id(&renamed), ids[0]);
        assert!(is_transient_scope_id(TRANSIENT_WORKSPACE_ID));
        assert!(!is_transient_scope_id("transient-review-workspaces"));
    }

    #[test]
    fn subdirectories_resolve_to_their_working_tree_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let nested = root.join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        // A linked worktree has a `.git` file.
        let worktree = dir.path().join("worktree");
        std::fs::create_dir_all(worktree.join("docs")).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: elsewhere\n").unwrap();
        let outside = dir.path().join("plain");
        std::fs::create_dir(&outside).unwrap();

        assert_eq!(working_tree_root(&nested), root);
        assert_eq!(working_tree_root(&root), root);
        assert_eq!(working_tree_root(&worktree.join("docs")), worktree);
        assert_eq!(working_tree_root(&outside), outside);
    }
}
