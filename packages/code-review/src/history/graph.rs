//! The commits between some tips and the trunk, read once with `git rev-list`, so ancestry
//! questions about a stack's versions need no more git.

use crate::data::git;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Most commits read; past it, ancestry is asked of git one question at a time.
const MAX_COMMITS: usize = 100_000;

/// Every commit reachable from the tips and not from the excluded commits, with its parents, and
/// the boundary commits where that history meets the excluded one.
#[derive(Debug, Default)]
pub struct CommitGraph {
    parents: BTreeMap<String, Vec<String>>,
    boundary: BTreeSet<String>,
}

impl CommitGraph {
    /// The graph of `tips` excluding the history of `excluded`; `None` when git cannot read it
    /// or it is larger than [`MAX_COMMITS`].
    pub fn walk(root: &Path, tips: &[String], excluded: &[String]) -> Option<Self> {
        if tips.is_empty() {
            return Some(Self::default());
        }
        let limit = (MAX_COMMITS + 1).to_string();
        let tips: BTreeSet<&String> = tips.iter().collect();
        let excluded: Vec<String> = excluded.iter().map(|oid| format!("^{oid}")).collect();
        let mut args = vec!["rev-list", "--parents", "--boundary", "--max-count", &limit];
        args.extend(tips.iter().map(|tip| tip.as_str()));
        args.extend(excluded.iter().map(String::as_str));
        args.push("--");
        let output = git::output(root, &args).ok()?;
        let mut graph = Self::default();
        for line in String::from_utf8_lossy(&output).lines() {
            if let Some(commit) = line.strip_prefix('-') {
                graph
                    .boundary
                    .insert(commit.split(' ').next().unwrap_or_default().to_owned());
                continue;
            }
            let mut ids = line.split(' ');
            if let Some(commit) = ids.next() {
                graph
                    .parents
                    .insert(commit.to_owned(), ids.map(str::to_owned).collect());
            }
        }
        (graph.parents.len() <= MAX_COMMITS).then_some(graph)
    }

    /// Whether `to` is `from` or one of its ancestors (as far as the graph reaches: through the
    /// boundary, not beyond).
    pub fn reaches(&self, from: &str, to: &str) -> bool {
        if from == to {
            return true;
        }
        let mut seen = BTreeSet::new();
        let mut queue = vec![from];
        while let Some(commit) = queue.pop() {
            for parent in self.parents.get(commit).into_iter().flatten() {
                if parent == to {
                    return true;
                }
                if seen.insert(parent.as_str()) {
                    queue.push(parent);
                }
            }
        }
        false
    }

    /// The boundary commits `from`'s history reaches: where it left the excluded history.
    pub fn boundary_reached(&self, from: &str) -> BTreeSet<String> {
        let mut seen = BTreeSet::new();
        let mut reached = BTreeSet::new();
        let mut queue = vec![from];
        while let Some(commit) = queue.pop() {
            if self.boundary.contains(commit) {
                reached.insert(commit.to_owned());
                continue;
            }
            for parent in self.parents.get(commit).into_iter().flatten() {
                if seen.insert(parent.as_str()) {
                    queue.push(parent);
                }
            }
        }
        reached
    }
}
