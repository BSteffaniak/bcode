//! Layer versions from branch reflogs.
//!
//! Every commit a stack's local or remote-tracking branches pointed at, with when they moved
//! there: the author's own stack fully, and anyone else's as far as it was fetched.
//!
//! Reflogs are read from their files (`logs/refs/...` in the repository's common directory): no
//! git process, and the time each entry records is when the branch moved, not when its commit
//! was made. A repository whose references are not files (the reftable format) is read with
//! `git reflog` per branch instead.

use crate::data::DataError;
use crate::data::git;
use bcode_code_review_models::{LayerVersion, VersionSource};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The versions the reflogs of `layers`' head branches (`(number, branch)`) record, local and
/// remote-tracking.
///
/// # Errors
///
/// * [`DataError::Git`] when git fails.
pub fn reflog_versions(
    root: &Path,
    provider: &str,
    repository: &str,
    layers: &[(u64, String)],
) -> Result<Vec<LayerVersion>, DataError> {
    let by_branch: BTreeMap<&str, u64> = layers
        .iter()
        .map(|(number, branch)| (branch.as_str(), *number))
        .collect();
    let listed = git::output(
        root,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "--",
            "refs/heads",
            "refs/remotes",
        ],
    )?;
    let listed = String::from_utf8_lossy(&listed);
    // Each reference of a layer's branch, with the layer.
    let refs: Vec<(String, u64)> = listed
        .lines()
        .filter_map(|reference| {
            let branch = reference.strip_prefix("refs/heads/").or_else(|| {
                reference
                    .strip_prefix("refs/remotes/")
                    .and_then(|rest| rest.split_once('/'))
                    .map(|(_, branch)| branch)
            })?;
            Some((reference.to_owned(), *by_branch.get(branch)?))
        })
        .collect();
    let common = common_dir(root);
    let mut versions: BTreeMap<(u64, String), LayerVersion> = BTreeMap::new();
    for (reference, number) in refs {
        let entries = match common.as_deref() {
            Some(common) => read_reflog_file(&common.join("logs").join(&reference)),
            None => read_reflog_with_git(root, &reference)?,
        };
        for (head, at_ms) in entries {
            versions
                .entry((number, head.clone()))
                .and_modify(|known| known.seen_at_ms = known.seen_at_ms.min(at_ms))
                .or_insert_with(|| LayerVersion {
                    provider: provider.to_owned(),
                    repository: repository.to_owned(),
                    number,
                    head_oid: head,
                    base_oid: None,
                    base_branch: None,
                    seen_at_ms: at_ms,
                    actor: None,
                    sources: BTreeSet::from([VersionSource::Reflog]),
                });
        }
    }
    Ok(versions.into_values().collect())
}

/// The repository's common directory when its reflogs are files: `.git`, or a linked
/// worktree's main repository's. `None` for other layouts and the reftable format.
fn common_dir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let pointer = std::fs::read_to_string(&dot_git).ok()?;
        root.join(pointer.strip_prefix("gitdir:")?.trim())
    };
    let common = std::fs::read_to_string(git_dir.join("commondir"))
        .map_or_else(|_| git_dir.clone(), |common| git_dir.join(common.trim()));
    (!common.join("reftable").exists()).then_some(common)
}

/// Each commit a reflog file records the reference moving to, with when (milliseconds). A
/// missing file is no entries. Each line is `<old> <new> <name> <<email>> <seconds> <zone>\t<message>`.
fn read_reflog_file(path: &Path) -> Vec<(String, u64)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let (identity, _) = line.split_once('\t').unwrap_or((line, ""));
            let mut fields = identity.splitn(3, ' ');
            let (_, new, rest) = (fields.next()?, fields.next()?, fields.next()?);
            // The time follows the email, which may contain spaces inside its brackets.
            let (_, after_email) = rest.rsplit_once('>')?;
            let seconds: u64 = after_email.split_whitespace().next()?.parse().ok()?;
            let is_commit = git::validate_oid(new).is_ok() && new.bytes().any(|byte| byte != b'0');
            is_commit.then(|| (new.to_owned(), seconds.saturating_mul(1000)))
        })
        .collect()
}

/// [`read_reflog_file`] for a repository whose reflogs are not files.
fn read_reflog_with_git(root: &Path, reference: &str) -> Result<Vec<(String, u64)>, DataError> {
    let Some(output) = git::optional_output(
        root,
        &[
            "reflog",
            "show",
            "--format=%H %gd",
            "--date=unix",
            reference,
            "--",
        ],
    )?
    else {
        return Ok(Vec::new());
    };
    // `%gd` with `--date=unix` reads `<reference>@{<seconds>}`.
    Ok(String::from_utf8_lossy(&output)
        .lines()
        .filter_map(|line| {
            let (head, selector) = line.split_once(' ')?;
            let seconds: u64 = selector
                .rsplit_once("@{")?
                .1
                .strip_suffix('}')?
                .parse()
                .ok()?;
            Some((head.to_owned(), seconds.saturating_mul(1000)))
        })
        .collect())
}
