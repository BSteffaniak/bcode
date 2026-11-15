//! Three-way merges of a saved review's state.
//!
//! Several writers change one saved review at the same time: the open review in the app saves
//! its presentation, viewed files and review pass; the assistant's tools mark files viewed,
//! rename and archive; home renames and restores. Each writer knows the review as it last read
//! or wrote it (the **base**), its own version (**local**) and, at write time, the version in the
//! store (**stored**). A merge keeps what the writer changed from its base and takes everything
//! else from the store, so no writer undoes another's change:
//!
//! * Whole fields (title, sources, archived, review pass, presentation, stack snapshots): the
//!   local value when it differs from the base, else the stored one.
//! * Viewed state, per file and per thread: the local state where it differs from the base, else
//!   the stored state.

use bcode_code_review_models::{ReviewWorkspace, ThreadViewMark, ViewedMark};
use std::collections::{BTreeMap, BTreeSet};

/// A workspace's viewed state: viewed paths, viewed marks and viewed threads.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ViewedSet {
    /// Viewed display paths.
    pub files: BTreeSet<String>,
    /// Viewed marks.
    pub marks: Vec<ViewedMark>,
    /// Viewed threads.
    pub threads: Vec<ThreadViewMark>,
}

impl ViewedSet {
    /// The viewed state of `workspace`.
    #[must_use]
    pub fn of(workspace: &ReviewWorkspace) -> Self {
        Self {
            files: workspace.viewed_files.clone(),
            marks: workspace.viewed_marks.clone(),
            threads: workspace.viewed_threads.clone(),
        }
    }

    /// Write this viewed state into `workspace`.
    pub fn apply(&self, workspace: &mut ReviewWorkspace) {
        workspace.viewed_files.clone_from(&self.files);
        workspace.viewed_marks.clone_from(&self.marks);
        workspace.viewed_threads.clone_from(&self.threads);
    }

    /// One path's state: whether it is listed viewed, and its marks.
    fn path(&self, path: &str) -> (bool, Vec<&ViewedMark>) {
        let mut marks: Vec<&ViewedMark> =
            self.marks.iter().filter(|mark| mark.path == path).collect();
        marks.sort();
        (self.files.contains(path), marks)
    }

    fn thread(&self, id: &str) -> Option<&ThreadViewMark> {
        self.threads.iter().find(|mark| mark.thread_id == id)
    }

    fn paths(&self) -> impl Iterator<Item = &str> {
        self.files
            .iter()
            .map(String::as_str)
            .chain(self.marks.iter().map(|mark| mark.path.as_str()))
    }
}

/// Merge `local` and `stored` from their common `base`: per file and per thread, the local
/// state where it differs from the base, else the stored state.
#[must_use]
pub fn merge_viewed(base: &ViewedSet, local: &ViewedSet, stored: &ViewedSet) -> ViewedSet {
    let paths: BTreeSet<&str> = base
        .paths()
        .chain(local.paths())
        .chain(stored.paths())
        .collect();
    let mut merged = ViewedSet::default();
    for path in paths {
        let mine = local.path(path);
        let (listed, marks) = if mine == base.path(path) {
            stored.path(path)
        } else {
            mine
        };
        if listed {
            merged.files.insert(path.to_owned());
        }
        merged.marks.extend(marks.into_iter().cloned());
    }
    merged.marks.sort();
    merged.marks.dedup();
    let threads: BTreeSet<&str> = base
        .threads
        .iter()
        .chain(&local.threads)
        .chain(&stored.threads)
        .map(|mark| mark.thread_id.as_str())
        .collect();
    let mut kept: BTreeMap<&str, ThreadViewMark> = BTreeMap::new();
    for id in threads {
        let mine = local.thread(id);
        let chosen = if mine == base.thread(id) {
            stored.thread(id)
        } else {
            mine
        };
        if let Some(mark) = chosen {
            kept.insert(id, mark.clone());
        }
    }
    // Keep the local order of threads, then the ones only stored.
    for mark in local.threads.iter().chain(&stored.threads) {
        if let Some(mark) = kept.remove(mark.thread_id.as_str()) {
            merged.threads.push(mark);
        }
    }
    merged
}

/// Merge the writer's `local` workspace into `stored` from the writer's `base` (see the module
/// documentation). Identity, repository and timestamps come from `stored`.
#[must_use]
pub fn merge_workspace(
    base: &ReviewWorkspace,
    local: &ReviewWorkspace,
    stored: &ReviewWorkspace,
) -> ReviewWorkspace {
    fn pick<T: PartialEq + Clone>(base: &T, local: &T, stored: &T) -> T {
        if local == base {
            stored.clone()
        } else {
            local.clone()
        }
    }
    let mut merged = ReviewWorkspace {
        id: stored.id.clone(),
        title: pick(&base.title, &local.title, &stored.title),
        repo_root: stored.repo_root.clone(),
        sources: pick(&base.sources, &local.sources, &stored.sources),
        created_at_ms: stored.created_at_ms,
        updated_at_ms: stored.updated_at_ms,
        viewed_files: BTreeSet::new(),
        archived_at_ms: pick(
            &base.archived_at_ms,
            &local.archived_at_ms,
            &stored.archived_at_ms,
        ),
        viewed_marks: Vec::new(),
        viewed_threads: Vec::new(),
        reviewed_revisions: pick(
            &base.reviewed_revisions,
            &local.reviewed_revisions,
            &stored.reviewed_revisions,
        ),
        presentation: pick(
            &base.presentation,
            &local.presentation,
            &stored.presentation,
        ),
        stack_snapshots: pick(
            &base.stack_snapshots,
            &local.stack_snapshots,
            &stored.stack_snapshots,
        ),
    };
    merge_viewed(
        &ViewedSet::of(base),
        &ViewedSet::of(local),
        &ViewedSet::of(stored),
    )
    .apply(&mut merged);
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn viewed(paths: &[&str]) -> ViewedSet {
        ViewedSet {
            files: paths.iter().map(|path| (*path).to_owned()).collect(),
            marks: paths
                .iter()
                .map(|path| ViewedMark {
                    path: (*path).to_owned(),
                    old_oid: Some("a".to_owned()),
                    new_oid: Some("b".to_owned()),
                    patch_id: None,
                })
                .collect(),
            threads: Vec::new(),
        }
    }

    fn workspace(title: &str, paths: &[&str]) -> ReviewWorkspace {
        let mut workspace = ReviewWorkspace {
            id: "rv".to_owned(),
            title: title.to_owned(),
            repo_root: PathBuf::from("/repo"),
            sources: Vec::new(),
            created_at_ms: Some(1),
            updated_at_ms: Some(1),
            viewed_files: BTreeSet::new(),
            archived_at_ms: None,
            viewed_marks: Vec::new(),
            viewed_threads: Vec::new(),
            reviewed_revisions: BTreeMap::new(),
            presentation: None,
            stack_snapshots: Vec::new(),
        };
        viewed(paths).apply(&mut workspace);
        workspace
    }

    #[test]
    fn a_mark_written_elsewhere_survives_a_save_of_unrelated_changes() {
        let base = viewed(&["a.rs"]);
        let local = viewed(&["a.rs"]);
        let stored = viewed(&["a.rs", "b.rs", "c.rs"]);
        assert_eq!(merge_viewed(&base, &local, &stored), stored);
    }

    #[test]
    fn the_writers_own_changes_win_for_the_files_they_touched() {
        let base = viewed(&["a.rs", "b.rs"]);
        // The writer unmarked b and marked d; elsewhere c was marked and a unmarked.
        let local = viewed(&["a.rs", "d.rs"]);
        let stored = viewed(&["b.rs", "c.rs"]);
        assert_eq!(
            merge_viewed(&base, &local, &stored),
            viewed(&["c.rs", "d.rs"]),
            "a unmarked elsewhere, b unmarked here, c marked elsewhere, d marked here"
        );
    }

    #[test]
    fn threads_merge_by_id() {
        let mark = |id: &str, at: u64| ThreadViewMark {
            thread_id: id.to_owned(),
            comment_ids: vec!["c1".to_owned()],
            viewed_at_ms: at,
        };
        let base = ViewedSet {
            threads: vec![mark("t1", 1)],
            ..ViewedSet::default()
        };
        let local = ViewedSet {
            threads: vec![mark("t1", 1), mark("t2", 2)],
            ..ViewedSet::default()
        };
        let stored = ViewedSet {
            threads: vec![mark("t3", 3)],
            ..ViewedSet::default()
        };
        let merged = merge_viewed(&base, &local, &stored);
        let ids: Vec<&str> = merged
            .threads
            .iter()
            .map(|mark| mark.thread_id.as_str())
            .collect();
        assert_eq!(ids, vec!["t2", "t3"], "t1 was unviewed elsewhere");
    }

    #[test]
    fn a_save_keeps_a_rename_and_an_archive_made_elsewhere() {
        let base = workspace("Old", &["a.rs"]);
        let mut local = base.clone();
        local.viewed_files.insert("z.rs".to_owned());
        let mut stored = workspace("Renamed", &["a.rs", "b.rs"]);
        stored.archived_at_ms = Some(7);
        let merged = merge_workspace(&base, &local, &stored);
        assert_eq!(merged.title, "Renamed");
        assert_eq!(merged.archived_at_ms, Some(7));
        assert_eq!(
            merged
                .viewed_files
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["a.rs", "b.rs", "z.rs"]
        );
    }

    #[test]
    fn the_writers_rename_wins_over_the_stored_title() {
        let base = workspace("Old", &[]);
        let local = workspace("Mine", &[]);
        let stored = workspace("Theirs", &[]);
        assert_eq!(merge_workspace(&base, &local, &stored).title, "Mine");
    }
}
