//! Resolve selected commits as individual first-parent deltas or one exact aggregate.

use super::resolve::{CommitInfo, commit_infos, commit_parts, first_parent_chain};
use super::{DataError, ReviewData, git};
use bcode_code_review_models::{
    CommitSeriesResolution, ResolvedSource, ReviewSource, ReviewSourceKind, RevisionRef,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

impl ReviewData {
    /// Pin the members of a `Commits` source. Only an explicit combined request may replay
    /// sparse deltas. A failed combination retains the separately reviewable members.
    ///
    /// # Errors
    ///
    /// * Invalid source kind, empty selection, or a revision that cannot be pinned.
    /// * Repository access or Git failure while resolving members.
    pub fn commit_series(
        &self,
        root: &Path,
        source: &ReviewSource,
        combined: bool,
    ) -> Result<CommitSeriesResolution, DataError> {
        let ReviewSourceKind::Commits { revs } = &source.kind else {
            return Err(DataError::InvalidRequest(
                "expected a commits source".into(),
            ));
        };
        let root = self.repo_root(root)?;
        let mut seen = BTreeSet::new();
        let mut oids = Vec::new();
        for rev in revs {
            let oid = git::resolve_commit(&root, rev)?.ok_or_else(|| {
                DataError::NotFound(format!("revision {rev} does not exist locally"))
            })?;
            if seen.insert(oid.clone()) {
                oids.push(oid);
            }
        }
        if oids.is_empty() {
            return Err(DataError::InvalidRequest("no commits selected".into()));
        }
        let infos = commit_infos(&root, &oids)?;
        for oid in &oids {
            let info = infos
                .get(oid)
                .ok_or_else(|| DataError::NotFound(oid.clone()))?;
            // `git log %P` reports no parents at shallow boundaries. Never represent an
            // unavailable first parent as an empty tree (the entire snapshot as a delta).
            if info.parent.is_none() {
                let raw = git::output(&root, &["cat-file", "commit", oid])?;
                if raw
                    .split(|&byte| byte == b'\n')
                    .take_while(|line| !line.is_empty())
                    .any(|line| line.starts_with(b"parent "))
                {
                    return Err(DataError::NotFound(format!(
                        "first parent unavailable for {oid}"
                    )));
                }
            }
        }
        let parents = infos
            .values()
            .filter_map(|info| info.parent.as_deref())
            .collect();
        let present = crate::history::present_commits(&root, &parents)?;
        if let Some(parent) = parents.iter().find(|parent| !present.contains(**parent)) {
            return Err(DataError::NotFound(format!(
                "first parent {parent} unavailable"
            )));
        }
        let chain = first_parent_chain(&oids, &infos);
        let mut resolution = CommitSeriesResolution {
            members: commit_parts(source, &oids, &infos),
            contiguous: chain.is_some(),
            combined: None,
            unavailable: None,
        };
        // Part positions belong to the original selection. Only the private replay plan
        // follows ancestry; otherwise changing views could silently renumber the members.
        if combined {
            let ordered = match chain {
                Some((_, newest)) => Ok(contiguous_order(&infos, &newest)),
                None => sparse_order(&root, &oids, &seen),
            };
            let result = ordered.map_err(|err| err.to_string()).and_then(|order| {
                aggregate(&root, self.disk(), &commit_parts(source, &order, &infos))
            });
            match result {
                Ok(source) => resolution.combined = Some(source),
                Err(reason) => resolution.unavailable = Some(reason),
            }
        }
        Ok(resolution)
    }
}

fn contiguous_order(infos: &BTreeMap<String, CommitInfo>, newest: &str) -> Vec<String> {
    let mut ordered = Vec::with_capacity(infos.len());
    let mut next = infos.get(newest);
    while let Some(info) = next {
        ordered.push(info.oid.clone());
        next = info.parent.as_ref().and_then(|oid| infos.get(oid));
    }
    ordered.reverse();
    ordered
}

/// An independent tip is necessary but not sufficient: a merge's second parent must not be
/// mistaken for part of the selected first-parent line. Walk only until the oldest selection.
fn sparse_order(
    root: &Path,
    oids: &[String],
    selected: &BTreeSet<String>,
) -> Result<Vec<String>, DataError> {
    let ambiguous =
        || DataError::InvalidRequest("selected commits are not on one first-parent line".into());
    let mut args = vec!["merge-base", "--independent"];
    args.extend(oids.iter().map(String::as_str));
    let tips = git::output(root, &args)?;
    let mut tips = std::str::from_utf8(&tips)
        .map_err(|_| ambiguous())?
        .split_whitespace();
    let mut cursor = tips.next().ok_or_else(ambiguous)?.to_owned();
    if tips.next().is_some() {
        return Err(ambiguous());
    }
    let mut ordered = Vec::with_capacity(oids.len());
    // Page bounded output and do not walk an entire unrelated history on every refresh.
    for _ in 0..200 {
        let page = git::output(
            root,
            &[
                "rev-list",
                "--first-parent",
                "--max-count=256",
                "--parents",
                &cursor,
                "--",
            ],
        )?;
        let mut next = None;
        for line in std::str::from_utf8(&page).map_err(|_| ambiguous())?.lines() {
            let mut fields = line.split_whitespace();
            let oid = fields.next().ok_or_else(ambiguous)?;
            if selected.contains(oid) {
                ordered.push(oid.to_owned());
                if ordered.len() == selected.len() {
                    ordered.reverse();
                    return Ok(ordered);
                }
            }
            next = fields.next();
        }
        next.ok_or_else(ambiguous)?.clone_into(&mut cursor);
    }
    Err(DataError::TooLarge(
        "selected first-parent history exceeds the series walk limit".into(),
    ))
}

fn aggregate(
    root: &Path,
    disk: &crate::cache::ReviewCache,
    members: &[ResolvedSource],
) -> Result<ResolvedSource, String> {
    let (first, rest) = members.split_first().ok_or("no commits selected")?;
    let mut result = first.clone();
    result.part = None;
    let mut head = first
        .head
        .commit_oid()
        .ok_or("missing selected commit")?
        .to_owned();
    let mut remaining = rest;
    while let Some((start, tail)) = remaining.split_first() {
        // Replay one delta per uninterrupted run, not one per member: intermediate selected
        // changes can cancel and need not apply individually across an omitted dependency.
        let mut end = start;
        remaining = tail;
        while let Some((next, tail)) = remaining.split_first() {
            if next.base != end.head {
                break;
            }
            end = next;
            remaining = tail;
        }
        let base = start
            .base
            .commit_oid()
            .ok_or("missing first parent for selected run")?;
        let tip = end.head.commit_oid().ok_or("missing selected commit")?;
        if base == head {
            tip.clone_into(&mut head);
        } else {
            let (oid, conflicts) = crate::history::replay_selected(root, disk, base, &head, tip)
                .map_err(|err| err.to_string())?
                .ok_or_else(|| format!("cannot replay selected run ending at {tip}"))?;
            if !conflicts.is_empty() {
                return Err(conflict_diagnostic(tip, &conflicts));
            }
            head = oid;
        }
    }
    result.head = RevisionRef::commit(head);
    Ok(result)
}

fn conflict_diagnostic(tip: &str, conflicts: &BTreeSet<String>) -> String {
    const PATHS: usize = 4;
    const CHARS: usize = 96;
    let mut paths = Vec::new();
    for path in conflicts.iter().take(PATHS) {
        let mut chars = path.chars().flat_map(char::escape_debug);
        let mut sample: String = chars.by_ref().take(CHARS).collect();
        if chars.next().is_some() {
            sample.push('…');
        }
        paths.push(sample);
    }
    if conflicts.len() > PATHS {
        paths.push(format!("… (+{} more)", conflicts.len() - PATHS));
    }
    format!(
        "selected run ending at {tip} conflicts on {} path(s): {}",
        conflicts.len(),
        paths.join(", ")
    )
}
