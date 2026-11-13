//! Comparisons across a stack's history, as review sources.
//!
//! A layer's change between two versions is shown with the rebase taken out. The old version's
//! own change (from its fork point to its head) is replayed on the new version's fork point with
//! `git merge-tree`, and wrapped in a local commit. The review then compares that commit with the
//! new head: exactly what the author changed, without what moved underneath. When the replay
//! conflicts, the comparison falls back to head against head, and says so.
//!
//! Every comparison is an ordinary [`ReviewSourceKind::CommitRange`], so the review screen, viewed
//! state, and comments work on it unchanged. The replay commits are not on any branch; git keeps
//! them for its usual grace period, and a comparison that names a lost one is rebuilt from the
//! history.

use super::version_at;
use crate::data::DataError;
use crate::data::git;
use bcode_code_review_models::{LayerTimeline, ReviewSourceKind, StackTimeline, TimelineVersion};
use std::fmt::Write as _;
use std::path::Path;

/// How replaying an old version's change on a new fork point went.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Replay {
    /// Not tried: a fork point is unknown.
    NotTried,
    /// It conflicted.
    Conflicted,
    /// It made this tree.
    Clean(String),
}

/// One comparison: the source to review, its label, and whether the rebase was taken out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    /// The pull request it is about (the top layer for a whole-stack comparison).
    pub number: u64,
    /// The source to review.
    pub source: ReviewSourceKind,
    /// What it shows.
    pub label: String,
    /// Whether what moved underneath is left out (the replay was clean).
    pub rebase_removed: bool,
}

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

/// A commit with `tree` on top of `parent` (not on any branch), to compare from.
fn commit_of(root: &Path, tree: &str, parent: &str, message: &str) -> Result<String, DataError> {
    git::validate_oid(tree)?;
    git::validate_oid(parent)?;
    let output = git::output(root, &["commit-tree", tree, "-p", parent, "-m", message])?;
    Ok(String::from_utf8_lossy(&output).trim().to_owned())
}

/// The comparison from `from` to `to`, two versions of layer `number`: the old change replayed
/// on the new fork point against the new head; head against head when either fork point is
/// unknown or the replay conflicts. `None` when they are the same commit.
fn compare_versions(
    root: &Path,
    number: u64,
    from: &TimelineVersion,
    to: &TimelineVersion,
    replayed: Replay,
    label: String,
) -> Result<Option<Comparison>, DataError> {
    if from.version.head_oid == to.version.head_oid {
        return Ok(None);
    }
    let head = to.version.head_oid.clone();
    if let (Replay::Clean(tree), Some(new_fork)) = (replayed, to.fork_oid.as_deref()) {
        let base = commit_of(
            root,
            &tree,
            new_fork,
            &format!(
                "#{number} at {} replayed on {new_fork}",
                from.version.head_oid
            ),
        )?;
        return Ok(Some(Comparison {
            number,
            source: ReviewSourceKind::CommitRange {
                base,
                head,
                merge_base: false,
            },
            label,
            rebase_removed: true,
        }));
    }
    Ok(Some(Comparison {
        number,
        source: ReviewSourceKind::CommitRange {
            base: from.version.head_oid.clone(),
            head,
            merge_base: false,
        },
        label: format!("{label} (with the rebase)"),
        rebase_removed: false,
    }))
}

/// The replays `pairs` (layer, old version, new version) need, in one batch.
fn replays_for(
    root: &Path,
    pairs: &[(&LayerTimeline, usize, usize)],
) -> Result<Vec<Replay>, DataError> {
    let wanted: Vec<Option<(String, String, String)>> = pairs
        .iter()
        .map(|(layer, from, to)| {
            let (from, to) = (&layer.versions[*from], &layer.versions[*to]);
            Some((
                from.fork_oid.clone()?,
                to.fork_oid.clone()?,
                from.version.head_oid.clone(),
            ))
        })
        .collect();
    let lines: Vec<(String, String, String)> = wanted.iter().flatten().cloned().collect();
    let mut trees = replay_trees(root, &lines)?.into_iter();
    Ok(wanted
        .into_iter()
        .map(|wanted| match wanted {
            None => Replay::NotTried,
            Some(_) => trees
                .next()
                .flatten()
                .map_or(Replay::Conflicted, Replay::Clean),
        })
        .collect())
}

/// Layer `layer`'s change from version `from` to version `to` (indices into its versions).
/// `None` when they are the same commit.
///
/// # Errors
///
/// * [`DataError::Git`] when git fails.
pub fn layer_changes(
    root: &Path,
    layer: &LayerTimeline,
    from: usize,
    to: usize,
) -> Result<Option<Comparison>, DataError> {
    let (Some(old), Some(new)) = (layer.versions.get(from), layer.versions.get(to)) else {
        return Ok(None);
    };
    let replayed = replays_for(root, &[(layer, from, to)])?
        .pop()
        .unwrap_or(Replay::NotTried);
    compare_versions(
        root,
        layer.number,
        old,
        new,
        replayed,
        format!(
            "#{} {}: changes between versions",
            layer.number, layer.title
        ),
    )
}

/// Every layer as it was at `at_ms`: each layer's version then, from its fork point. Layers that
/// did not exist yet are left out.
#[must_use]
pub fn stack_at(timeline: &StackTimeline, at_ms: u64) -> Vec<Comparison> {
    timeline
        .layers
        .iter()
        .filter_map(|layer| {
            let version = &layer.versions[version_at(layer, at_ms)?];
            Some(Comparison {
                number: layer.number,
                source: ReviewSourceKind::CommitRange {
                    base: version.fork_oid.clone()?,
                    head: version.version.head_oid.clone(),
                    merge_base: false,
                },
                label: format!("#{} {} (as it was)", layer.number, layer.title),
                rebase_removed: true,
            })
        })
        .collect()
}

/// Every layer's change since `at_ms`, rebases taken out.
///
/// From each layer's version then to its newest. A layer that did not exist yet shows its whole
/// change. With `include_rebased` false, layers whose change is the same as then (they were only
/// rebased) are left out.
///
/// # Errors
///
/// * [`DataError::Git`] when git fails.
pub fn changes_since(
    root: &Path,
    timeline: &StackTimeline,
    at_ms: u64,
    include_rebased: bool,
) -> Result<Vec<Comparison>, DataError> {
    changes_between(root, timeline, at_ms, None, include_rebased)
}

/// [`changes_since`] up to `to_ms` instead of the newest versions (`None`: the newest): what the
/// pushes between the two times changed.
///
/// # Errors
///
/// * [`DataError::Git`] when git fails.
pub fn changes_between(
    root: &Path,
    timeline: &StackTimeline,
    from_ms: u64,
    to_ms: Option<u64>,
    include_rebased: bool,
) -> Result<Vec<Comparison>, DataError> {
    changes(
        root,
        timeline,
        |layer| {
            let to = match to_ms {
                Some(to_ms) => version_at(layer, to_ms)?,
                None => layer.versions.len().checked_sub(1)?,
            };
            Some((version_at(layer, from_ms), to))
        },
        include_rebased,
    )
}

/// Every layer's change since the version of it this reviewer last reviewed (the whole change
/// of a layer never reviewed), rebases taken out.
///
/// # Errors
///
/// * [`DataError::Git`] when git fails.
pub fn changes_since_review(
    root: &Path,
    timeline: &StackTimeline,
    include_rebased: bool,
) -> Result<Vec<Comparison>, DataError> {
    changes(
        root,
        timeline,
        |layer| {
            let newest = layer.versions.len().checked_sub(1)?;
            let reviewed = layer.versions.iter().rposition(|version| version.reviewed);
            Some((reviewed, newest))
        },
        include_rebased,
    )
}

/// Each layer's change between the versions `pick` chooses (`(from, to)`; no `from` shows the
/// whole change of `to`), rebases taken out.
fn changes(
    root: &Path,
    timeline: &StackTimeline,
    pick: impl Fn(&LayerTimeline) -> Option<(Option<usize>, usize)>,
    include_rebased: bool,
) -> Result<Vec<Comparison>, DataError> {
    let mut pairs = Vec::new();
    let mut added = Vec::new();
    for layer in &timeline.layers {
        match pick(layer) {
            Some((Some(from), to)) if from == to => {}
            Some((Some(from), to)) => pairs.push((layer, from, to)),
            Some((None, to)) => added.push((layer, to)),
            None => {}
        }
    }
    let replays = replays_for(root, &pairs)?;
    let heads: Vec<&str> = pairs
        .iter()
        .map(|(layer, _, to)| layer.versions[*to].version.head_oid.as_str())
        .collect();
    let trees = trees_of(root, &heads)?;
    let mut comparisons = Vec::new();
    for (((layer, from, to), replayed), tree) in pairs.iter().zip(replays).zip(trees) {
        let only_rebased =
            matches!((&replayed, &tree), (Replay::Clean(replayed), Some(tree)) if replayed == tree);
        if only_rebased && !include_rebased {
            continue;
        }
        comparisons.extend(compare_versions(
            root,
            layer.number,
            &layer.versions[*from],
            &layer.versions[*to],
            replayed,
            format!("#{} {}: since then", layer.number, layer.title),
        )?);
    }
    for (layer, to) in added {
        let newest = &layer.versions[to];
        let Some(fork) = newest.fork_oid.clone() else {
            continue;
        };
        comparisons.push(Comparison {
            number: layer.number,
            source: ReviewSourceKind::CommitRange {
                base: fork,
                head: newest.version.head_oid.clone(),
                merge_base: false,
            },
            label: format!("#{} {}: new since then", layer.number, layer.title),
            rebase_removed: true,
        });
    }
    let order: Vec<u64> = timeline.layers.iter().map(|layer| layer.number).collect();
    comparisons
        .sort_by_key(|comparison| order.iter().position(|number| *number == comparison.number));
    Ok(comparisons)
}

/// The whole stack's change since `at_ms`, trunk movement taken out.
///
/// The top layer then, replayed on where the bottom layer starts now, against the top layer now.
/// `None` when the stack has no top version then and now.
///
/// # Errors
///
/// * [`DataError::Git`] when git fails.
pub fn whole_stack_since(
    root: &Path,
    timeline: &StackTimeline,
    at_ms: u64,
) -> Result<Option<Comparison>, DataError> {
    let (Some(bottom), Some(top)) = (timeline.layers.first(), timeline.layers.last()) else {
        return Ok(None);
    };
    let (Some(bottom_then), Some(top_then)) = (version_at(bottom, at_ms), version_at(top, at_ms))
    else {
        return Ok(None);
    };
    let (Some(bottom_now), Some(top_now)) = (bottom.versions.last(), top.versions.last()) else {
        return Ok(None);
    };
    let old = &top.versions[top_then];
    // The whole stack starts where its bottom layer forks from the trunk.
    let from = TimelineVersion {
        fork_oid: bottom.versions[bottom_then].fork_oid.clone(),
        ..old.clone()
    };
    let to = TimelineVersion {
        fork_oid: bottom_now.fork_oid.clone(),
        ..top_now.clone()
    };
    let replayed = match (&from.fork_oid, &to.fork_oid) {
        (Some(old_fork), Some(new_fork)) => replay_trees(
            root,
            &[(
                old_fork.clone(),
                new_fork.clone(),
                from.version.head_oid.clone(),
            )],
        )?
        .pop()
        .flatten()
        .map_or(Replay::Conflicted, Replay::Clean),
        _ => Replay::NotTried,
    };
    compare_versions(
        root,
        top.number,
        &from,
        &to,
        replayed,
        format!("Whole stack #{}–#{}: since then", bottom.number, top.number),
    )
}
