//! Replaying a version's own change on another fork point, the git work every comparison
//! across a stack's history starts from.
//!
//! A layer's change between two versions is shown with the rebase taken out: the old version's
//! own change (from its fork point to its head) is replayed on the new version's fork point with
//! one batched `git merge-tree`, and the layer's delta ([`super::delta`]) compares the result with
//! the new head. Every comparison, automatic or picked, goes through it.

use crate::data::DataError;
use crate::data::git;
use std::fmt::Write as _;
use std::path::Path;

/// For each `(old fork, new fork, old head)`: the tree of the old head's own change replayed on
/// the new fork, or `None` when the replay conflicts. One `git merge-tree --stdin` for all.
///
/// # Errors
///
/// * [`DataError::InvalidRequest`] for a malformed commit id; [`DataError::Git`] when git fails.
pub fn replay_trees(
    root: &Path,
    replays: &[(String, String, String)],
) -> Result<Vec<Option<String>>, DataError> {
    if replays.is_empty() {
        return Ok(Vec::new());
    }
    let mut input = String::new();
    for (old_fork, new_fork, old_head) in replays {
        for oid in [old_fork, new_fork, old_head] {
            git::validate_oid(oid)?;
        }
        let _ = writeln!(input, "{old_fork} -- {new_fork} {old_head}");
    }
    let output = git::output_with_input(
        root,
        &[
            "merge-tree",
            "--stdin",
            "--write-tree",
            "--name-only",
            "--no-messages",
        ],
        &input,
    )?;
    // Per merge: its status (1 clean, 0 conflicted), its tree, any conflicted paths, then an
    // empty field.
    let text = String::from_utf8_lossy(&output);
    let mut fields = text.split('\0');
    let mut trees = Vec::with_capacity(replays.len());
    while trees.len() < replays.len() {
        let Some(status) = fields.next() else {
            break;
        };
        let tree = fields.next().unwrap_or_default();
        for field in fields.by_ref() {
            if field.is_empty() {
                break;
            }
        }
        trees.push((status == "1").then(|| tree.to_owned()));
    }
    trees.resize(replays.len(), None);
    Ok(trees)
}

/// The trees of `commits` (`None` for any that is missing), in one `git cat-file`.
///
/// # Errors
///
/// * [`DataError::InvalidRequest`] for a malformed commit id; [`DataError::Git`] when git fails.
pub fn trees_of(root: &Path, commits: &[&str]) -> Result<Vec<Option<String>>, DataError> {
    if commits.is_empty() {
        return Ok(Vec::new());
    }
    let mut input = String::new();
    for commit in commits {
        git::validate_oid(commit)?;
        let _ = writeln!(input, "{commit}^{{tree}}");
    }
    let output = git::output_with_input(
        root,
        &["cat-file", "--batch-check=%(objectname) %(objecttype)"],
        &input,
    )?;
    Ok(String::from_utf8_lossy(&output)
        .lines()
        .map(|line| line.strip_suffix(" tree").map(str::to_owned))
        .collect())
}
