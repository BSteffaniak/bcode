//! Stack history: the versions each layer of a stack went through, how each changed from the one
//! before, and comparisons across them. See `docs/review-stack-history.md`.
//!
//! * Versions come from the version log (the review store), which gathers what reviews observed
//!   ([`observed_versions`]), what branch reflogs show ([`reflog::reflog_versions`]), and what the
//!   forge reports ([`forge::forge_versions`]).
//! * [`timeline`] orders them per layer, finds where each version's own change starts (its *fork
//!   point*), and classifies each change: content, rebase only, commits added, retargeted.
//! * [`compare`] turns two points in that history into review sources. A layer's change is
//!   compared with the rebase taken out: the old change is replayed on the new base
//!   (`git merge-tree`), and the review shows what differs from there.
//!
//! The git work is batched: one existence check, one history walk, one `merge-tree --stdin`, and
//! one tree lookup per timeline, however many layers and versions.

pub mod compare;
pub mod forge;
mod graph;
pub mod reflog;

use crate::data::DataError;
use crate::data::git;
use bcode_code_review_models::{
    ChangeKind, LayerTimeline, LayerVersion, ReviewStack, StackPush, StackTimeline,
    TimelineVersion, VersionSource,
};
use graph::CommitGraph;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Versions pushed within this many milliseconds of the one before count as one push.
pub const DEFAULT_PUSH_WINDOW_MS: u64 = 2 * 60 * 1000;

/// How [`timeline`] groups and marks versions.
#[derive(Debug, Clone, Default)]
pub struct TimelineOptions {
    /// Push window (default [`DEFAULT_PUSH_WINDOW_MS`] when zero).
    pub push_window_ms: u64,
    /// Head commits this reviewer reviewed.
    pub reviewed_heads: BTreeSet<String>,
}

/// The versions `stack` shows now, as observed at `at_ms`.
#[must_use]
pub fn observed_versions(
    provider: &str,
    repository: &str,
    stack: &ReviewStack,
    at_ms: u64,
) -> Vec<LayerVersion> {
    stack
        .layers
        .iter()
        .filter(|layer| !layer.pull.head_oid.is_empty())
        .map(|layer| LayerVersion {
            provider: provider.to_owned(),
            repository: repository.to_owned(),
            number: layer.pull.number,
            head_oid: layer.pull.head_oid.clone(),
            base_oid: layer.pull.base_oid.clone(),
            base_branch: Some(layer.pull.base_branch.clone()),
            seen_at_ms: at_ms,
            actor: None,
            sources: BTreeSet::from([VersionSource::Observed]),
        })
        .collect()
}

/// The history of `stack`'s layers from `versions` (any order, any sources): each layer's versions
/// oldest first with how each changed, and the pushes across layers.
///
/// # Errors
///
/// * [`DataError::Git`] / [`DataError::Io`] when git cannot run at all; commits that are missing
///   make their versions [`ChangeKind::Unavailable`] instead.
pub fn timeline(
    root: &Path,
    stack: &ReviewStack,
    versions: &[LayerVersion],
    options: &TimelineOptions,
) -> Result<StackTimeline, DataError> {
    let mut per_layer = versions_per_layer(stack, versions);
    let heads: BTreeSet<&str> = per_layer
        .values()
        .flatten()
        .map(|version| version.head_oid.as_str())
        .chain(
            per_layer
                .values()
                .flatten()
                .filter_map(|version| version.base_oid.as_deref()),
        )
        .collect();
    let present = present_commits(root, &heads)?;
    let trunk = trunk_tip(root, &stack.trunk)?;
    let tips: Vec<String> = per_layer
        .values()
        .flatten()
        .map(|version| version.head_oid.clone())
        .filter(|head| present.contains(head))
        .collect();
    let graph = CommitGraph::walk(root, &tips, trunk.as_slice());
    // Where each version's own change starts, bottom layer first: a version with no change of its
    // own (a branch just created on the one below, or still at the trunk) is not a version of
    // its layer, and is dropped before the layer above looks for its fork points.
    let mut forks: BTreeMap<(u64, String), String> = BTreeMap::new();
    for layer in &stack.layers {
        let mut found = Vec::new();
        {
            let parent = layer.parent.and_then(|parent| per_layer.get(&parent));
            for version in per_layer.get(&layer.pull.number).into_iter().flatten() {
                if let Some(fork) = fork_point(
                    root,
                    version,
                    parent,
                    &present,
                    graph.as_ref(),
                    trunk.as_deref(),
                )? {
                    found.push(((version.number, version.head_oid.clone()), fork));
                }
            }
        }
        if let Some(versions) = per_layer.get_mut(&layer.pull.number) {
            versions.retain(|version| {
                !found
                    .iter()
                    .any(|((_, head), fork)| head == &version.head_oid && fork == head)
            });
        }
        forks.extend(found);
    }
    let kinds = classify(root, &per_layer, &forks, &present, graph.as_ref())?;
    let layers: Vec<LayerTimeline> = stack
        .layers
        .iter()
        .map(|layer| LayerTimeline {
            number: layer.pull.number,
            title: layer.pull.title.clone(),
            head_branch: layer.pull.head_branch.clone(),
            versions: per_layer
                .get(&layer.pull.number)
                .into_iter()
                .flatten()
                .map(|version| {
                    let key = (version.number, version.head_oid.clone());
                    TimelineVersion {
                        kind: kinds.get(&key).copied().unwrap_or(ChangeKind::Unavailable),
                        fork_oid: forks.get(&key).cloned(),
                        reviewed: options.reviewed_heads.contains(&version.head_oid),
                        version: version.clone(),
                    }
                })
                .collect(),
        })
        .collect();
    let window = if options.push_window_ms == 0 {
        DEFAULT_PUSH_WINDOW_MS
    } else {
        options.push_window_ms
    };
    Ok(StackTimeline {
        pushes: pushes(&layers, window),
        layers,
    })
}

/// `versions` of `stack`'s layers by pull request number, one per head (the earliest time and
/// every source kept), oldest first.
fn versions_per_layer(
    stack: &ReviewStack,
    versions: &[LayerVersion],
) -> BTreeMap<u64, Vec<LayerVersion>> {
    let numbers: BTreeSet<u64> = stack.layers.iter().map(|layer| layer.pull.number).collect();
    let mut by_head: BTreeMap<(u64, &str), LayerVersion> = BTreeMap::new();
    for version in versions
        .iter()
        .filter(|version| numbers.contains(&version.number))
    {
        by_head
            .entry((version.number, version.head_oid.as_str()))
            .and_modify(|known| {
                known.sources.extend(version.sources.iter().copied());
                known.seen_at_ms = known.seen_at_ms.min(version.seen_at_ms);
                if known.base_oid.is_none() {
                    known.base_oid.clone_from(&version.base_oid);
                }
                if known.base_branch.is_none() {
                    known.base_branch.clone_from(&version.base_branch);
                }
                if known.actor.is_none() {
                    known.actor.clone_from(&version.actor);
                }
            })
            .or_insert_with(|| version.clone());
    }
    let mut per_layer: BTreeMap<u64, Vec<LayerVersion>> = BTreeMap::new();
    for ((number, _), version) in by_head {
        per_layer.entry(number).or_default().push(version);
    }
    for versions in per_layer.values_mut() {
        versions.sort_by(|a, b| (a.seen_at_ms, &a.head_oid).cmp(&(b.seen_at_ms, &b.head_oid)));
    }
    per_layer
}

/// The commits among `oids` that exist locally.
fn present_commits(root: &Path, oids: &BTreeSet<&str>) -> Result<BTreeSet<String>, DataError> {
    let oids: Vec<&str> = oids
        .iter()
        .copied()
        .filter(|oid| git::validate_oid(oid).is_ok())
        .collect();
    if oids.is_empty() {
        return Ok(BTreeSet::new());
    }
    let output = git::output_with_input(
        root,
        &["cat-file", "--batch-check=%(objectname) %(objecttype)"],
        &(oids.join("\n") + "\n"),
    )?;
    Ok(String::from_utf8_lossy(&output)
        .lines()
        .filter_map(|line| line.strip_suffix(" commit"))
        .map(str::to_owned)
        .collect())
}

/// The trunk's commit: its remote-tracking branch (where pushes land) else the local branch.
fn trunk_tip(root: &Path, trunk: &str) -> Result<Option<String>, DataError> {
    if trunk.is_empty() || trunk.starts_with('-') {
        return Ok(None);
    }
    for candidate in [
        format!("refs/remotes/origin/{trunk}"),
        format!("refs/heads/{trunk}"),
    ] {
        if let Some(oid) = git::resolve_commit(root, &candidate)? {
            return Ok(Some(oid));
        }
    }
    Ok(None)
}

/// Where `version`'s own change starts: its recorded base when that is in its history; else the
/// newest version of the layer below in its history; else, for the bottom layer, where it left
/// the trunk.
fn fork_point(
    root: &Path,
    version: &LayerVersion,
    parent: Option<&Vec<LayerVersion>>,
    present: &BTreeSet<String>,
    graph: Option<&CommitGraph>,
    trunk: Option<&str>,
) -> Result<Option<String>, DataError> {
    if !present.contains(&version.head_oid) {
        return Ok(None);
    }
    let reaches = |from: &str, to: &str| -> Result<bool, DataError> {
        match graph {
            Some(graph) => Ok(graph.reaches(from, to)),
            None => Ok(
                git::optional_output(root, &["merge-base", "--is-ancestor", to, from])?.is_some(),
            ),
        }
    };
    if let Some(base) = version.base_oid.as_deref()
        && present.contains(base)
        && reaches(&version.head_oid, base)?
    {
        return Ok(Some(base.to_owned()));
    }
    if let Some(parent) = parent {
        // The closest version of the layer below in this version's history: the one every other
        // such version is an ancestor of.
        let mut below: Vec<&str> = Vec::new();
        for candidate in parent {
            if present.contains(&candidate.head_oid)
                && reaches(&version.head_oid, &candidate.head_oid)?
            {
                below.push(&candidate.head_oid);
            }
        }
        for candidate in &below {
            let mut closest = true;
            for other in &below {
                if !reaches(candidate, other)? {
                    closest = false;
                    break;
                }
            }
            if closest {
                return Ok(Some((*candidate).to_owned()));
            }
        }
    }
    if let Some(graph) = graph {
        let left = graph.boundary_reached(&version.head_oid);
        if left.len() == 1 {
            return Ok(left.into_iter().next());
        }
    }
    let Some(trunk) = trunk else {
        return Ok(None);
    };
    git::merge_base(root, trunk, &version.head_oid)
}

/// How each version changed from the version of its layer before it.
fn classify(
    root: &Path,
    per_layer: &BTreeMap<u64, Vec<LayerVersion>>,
    forks: &BTreeMap<(u64, String), String>,
    present: &BTreeSet<String>,
    graph: Option<&CommitGraph>,
) -> Result<BTreeMap<(u64, String), ChangeKind>, DataError> {
    let mut kinds = BTreeMap::new();
    // Pairs only a replay can tell apart: (key, the old fork, the new fork, the old head, the new
    // head).
    let mut replays = Vec::new();
    for versions in per_layer.values() {
        for (index, version) in versions.iter().enumerate() {
            let key = (version.number, version.head_oid.clone());
            let Some(before) = index.checked_sub(1).map(|before| &versions[before]) else {
                let kind = if present.contains(&version.head_oid) {
                    ChangeKind::First
                } else {
                    ChangeKind::Unavailable
                };
                kinds.insert(key, kind);
                continue;
            };
            if !present.contains(&version.head_oid) || !present.contains(&before.head_oid) {
                kinds.insert(key, ChangeKind::Unavailable);
                continue;
            }
            if let (Some(old), Some(new)) = (&before.base_branch, &version.base_branch)
                && old != new
            {
                kinds.insert(key, ChangeKind::Retargeted);
                continue;
            }
            let fast_forward = match graph {
                Some(graph) => graph.reaches(&version.head_oid, &before.head_oid),
                None => git::optional_output(
                    root,
                    &[
                        "merge-base",
                        "--is-ancestor",
                        &before.head_oid,
                        &version.head_oid,
                    ],
                )?
                .is_some(),
            };
            if fast_forward {
                kinds.insert(key, ChangeKind::CommitsAdded);
                continue;
            }
            let old_fork = forks.get(&(before.number, before.head_oid.clone()));
            let new_fork = forks.get(&key);
            match (old_fork, new_fork) {
                (Some(old_fork), Some(new_fork)) => replays.push((
                    key,
                    old_fork.clone(),
                    new_fork.clone(),
                    before.head_oid.clone(),
                    version.head_oid.clone(),
                )),
                // Without both starting points the change cannot be told from a rebase.
                _ => {
                    kinds.insert(key, ChangeKind::Content);
                }
            }
        }
    }
    if replays.is_empty() {
        return Ok(kinds);
    }
    let lines: Vec<(String, String, String)> = replays
        .iter()
        .map(|(_, old_fork, new_fork, old_head, _)| {
            (old_fork.clone(), new_fork.clone(), old_head.clone())
        })
        .collect();
    let replayed = compare::replay_trees(root, &lines)?;
    let new_heads: Vec<&str> = replays
        .iter()
        .map(|(_, _, _, _, head)| head.as_str())
        .collect();
    let trees = compare::trees_of(root, &new_heads)?;
    for (((key, ..), replayed), tree) in replays.into_iter().zip(replayed).zip(trees) {
        let kind = match (replayed, tree) {
            (Some(replayed), Some(tree)) if replayed == tree => ChangeKind::RebaseOnly,
            _ => ChangeKind::Content,
        };
        kinds.insert(key, kind);
    }
    Ok(kinds)
}

/// The pushes across `layers`: versions (after each layer's first) within `window` of the one
/// before, oldest first.
fn pushes(layers: &[LayerTimeline], window: u64) -> Vec<StackPush> {
    let mut changes: Vec<(u64, u64, ChangeKind)> = layers
        .iter()
        .flat_map(|layer| {
            layer
                .versions
                .iter()
                .filter(|version| version.kind != ChangeKind::First)
                .map(|version| (version.version.seen_at_ms, layer.number, version.kind))
        })
        .collect();
    changes.sort_by_key(|(at, number, _)| (*at, *number));
    let mut pushes: Vec<StackPush> = Vec::new();
    let mut last_at = None;
    for (at, number, kind) in changes {
        match (pushes.last_mut(), last_at) {
            (Some(push), Some(last)) if at.saturating_sub(last) <= window => {
                push.layers.push((number, kind));
            }
            _ => pushes.push(StackPush {
                at_ms: at,
                layers: vec![(number, kind)],
            }),
        }
        last_at = Some(at);
    }
    pushes
}

/// The index of `layer`'s version current at `at_ms`: the newest seen by then.
#[must_use]
pub fn version_at(layer: &LayerTimeline, at_ms: u64) -> Option<usize> {
    layer
        .versions
        .iter()
        .rposition(|version| version.version.seen_at_ms <= at_ms)
}

/// A short summary of `push`: "#12 and #15 changed, 28 rebased".
#[must_use]
pub fn push_summary(push: &StackPush) -> String {
    let (changed, rebased) = push.split();
    let changed = match changed.as_slice() {
        [] => String::new(),
        [one] => format!("#{one} changed"),
        [first, second] => format!("#{first} and #{second} changed"),
        many => format!("{} layers changed", many.len()),
    };
    let rebased = (rebased > 0).then(|| format!("{rebased} rebased"));
    match (changed.is_empty(), rebased) {
        (true, None) => "nothing changed".to_owned(),
        (true, Some(rebased)) => format!("only rebased: {rebased}"),
        (false, None) => changed,
        (false, Some(rebased)) => format!("{changed}, {rebased}"),
    }
}

#[cfg(test)]
mod tests;
