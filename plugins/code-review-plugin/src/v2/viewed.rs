//! Identity-pinned viewed marks (spec section 5.4).
//!
//! `ReviewWorkspace::viewed_files` remains the authoritative set of viewed paths so V1 and the
//! home screen keep working; `ReviewWorkspace::viewed_marks` adds the identity each path was
//! viewed at. A viewed path without a mark (a V1 or pre-V2 mark) is a legacy mark of unknown
//! identity, and a mark whose path is no longer in `viewed_files` (unmarked by V1) is ignored.

use bcode_code_review_models::{ReviewIndexEntry, ReviewWorkspace, ViewedMark};

/// The identity of one file as currently reviewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewedIdentity<'a> {
    /// File display path.
    pub path: &'a str,
    /// Old-side identity (see [`ViewedMark::old_oid`]); `None` when not pinned.
    pub old_oid: Option<&'a str>,
    /// New-side identity; `None` when not pinned (working-tree content).
    pub new_oid: Option<&'a str>,
    /// Patch identity, once hunks are loaded.
    pub patch_id: Option<&'a str>,
}

/// Owned identity derived from an index entry (patch id unknown until hunks load).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryIdentity {
    /// File display path.
    pub path: String,
    /// Old-side identity.
    pub old_oid: Option<String>,
    /// New-side identity.
    pub new_oid: Option<String>,
}

impl EntryIdentity {
    /// Derive the identity of an index entry.
    #[must_use]
    pub fn from_entry(entry: &ReviewIndexEntry) -> Self {
        Self {
            path: entry.path.clone(),
            old_oid: entry.old_blob.viewed_identity(),
            new_oid: entry.new_blob.viewed_identity(),
        }
    }

    /// Borrow as a [`ViewedIdentity`] with an optional patch id.
    #[must_use]
    pub fn with_patch<'a>(&'a self, patch_id: Option<&'a str>) -> ViewedIdentity<'a> {
        ViewedIdentity {
            path: &self.path,
            old_oid: self.old_oid.as_deref(),
            new_oid: self.new_oid.as_deref(),
            patch_id,
        }
    }
}

/// Result of matching a file against viewed marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewedState {
    /// Not viewed.
    Unviewed,
    /// Viewed at this exact identity (same blob pair).
    Viewed,
    /// Viewed, but the stored mark should be re-recorded with the current identity: a legacy
    /// mark of unknown identity, or a mark matched by patch id after the blobs changed.
    ViewedNeedsUpdate,
}

impl ViewedState {
    /// Whether the file displays as viewed.
    #[must_use]
    pub const fn is_viewed(self) -> bool {
        !matches!(self, Self::Unviewed)
    }
}

/// Match one mark against a file identity.
///
/// * Different path: unviewed.
/// * Mark of unknown identity (legacy, or working-tree content marked without a patch id):
///   viewed until the identity is known; re-record it.
/// * Both sides pinned on mark and file and equal: viewed. Needs no hunks.
/// * Equal patch ids: viewed (the patch survived a rebase); re-record it with the new blobs.
/// * Otherwise: unviewed.
#[must_use]
pub fn match_mark(mark: &ViewedMark, file: &ViewedIdentity<'_>) -> ViewedState {
    if mark.path != file.path {
        return ViewedState::Unviewed;
    }
    if !mark.has_identity() {
        return ViewedState::ViewedNeedsUpdate;
    }
    let pinned = |mark_side: Option<&String>, file_side: Option<&str>| matches!((mark_side, file_side), (Some(mark), Some(file)) if mark == file);
    if pinned(mark.old_oid.as_ref(), file.old_oid) && pinned(mark.new_oid.as_ref(), file.new_oid) {
        return if mark.patch_id.is_none() && file.patch_id.is_some() {
            ViewedState::ViewedNeedsUpdate
        } else {
            ViewedState::Viewed
        };
    }
    match (&mark.patch_id, file.patch_id) {
        (Some(mark), Some(file)) if mark == file => ViewedState::ViewedNeedsUpdate,
        _ => ViewedState::Unviewed,
    }
}

/// Effective marks of a workspace: identity marks whose path is still viewed, plus a legacy
/// mark for every viewed path that has none.
#[must_use]
pub fn effective_marks(workspace: &ReviewWorkspace) -> Vec<ViewedMark> {
    let mut marks: Vec<ViewedMark> = workspace
        .viewed_marks
        .iter()
        .filter(|mark| workspace.viewed_files.contains(&mark.path))
        .cloned()
        .collect();
    for path in &workspace.viewed_files {
        if !marks.iter().any(|mark| &mark.path == path) {
            marks.push(ViewedMark::legacy(path.clone()));
        }
    }
    marks.sort();
    marks.dedup();
    marks
}

/// Viewed state of one file against a workspace.
#[must_use]
pub fn viewed_state(workspace: &ReviewWorkspace, file: &ViewedIdentity<'_>) -> ViewedState {
    effective_marks(workspace)
        .iter()
        .map(|mark| match_mark(mark, file))
        .max_by_key(|state| match state {
            ViewedState::Viewed => 2,
            ViewedState::ViewedNeedsUpdate => 1,
            ViewedState::Unviewed => 0,
        })
        .unwrap_or(ViewedState::Unviewed)
}

/// Record `file` as viewed at its current identity, replacing marks for the same path that no
/// longer match (including a legacy mark) and keeping `viewed_files` in sync.
pub fn mark_viewed(workspace: &mut ReviewWorkspace, file: &ViewedIdentity<'_>) {
    let mark = ViewedMark {
        path: file.path.to_string(),
        old_oid: file.old_oid.map(str::to_string),
        new_oid: file.new_oid.map(str::to_string),
        patch_id: file.patch_id.map(str::to_string),
    };
    workspace.viewed_marks.retain(|existing| {
        existing.path != file.path
            || (existing.has_identity() && match_mark(existing, file) == ViewedState::Unviewed)
    });
    workspace.viewed_marks.push(mark);
    workspace.viewed_marks.sort();
    workspace.viewed_files.insert(file.path.to_string());
}

/// Mark every file in `files` viewed (as [`mark_viewed`] does for one), sorting the marks once.
pub fn mark_all_viewed(workspace: &mut ReviewWorkspace, files: &[ViewedIdentity<'_>]) {
    let paths: std::collections::BTreeMap<&str, &ViewedIdentity<'_>> =
        files.iter().map(|file| (file.path, file)).collect();
    workspace.viewed_marks.retain(|existing| {
        paths.get(existing.path.as_str()).is_none_or(|file| {
            existing.has_identity() && match_mark(existing, file) == ViewedState::Unviewed
        })
    });
    for file in files {
        workspace.viewed_marks.push(ViewedMark {
            path: file.path.to_string(),
            old_oid: file.old_oid.map(str::to_string),
            new_oid: file.new_oid.map(str::to_string),
            patch_id: file.patch_id.map(str::to_string),
        });
        workspace.viewed_files.insert(file.path.to_string());
    }
    workspace.viewed_marks.sort();
    workspace.viewed_marks.dedup();
}

/// Remove every mark matching `file`. The path leaves `viewed_files` once no mark for it
/// remains.
pub fn unmark_viewed(workspace: &mut ReviewWorkspace, file: &ViewedIdentity<'_>) {
    let legacy_only = !workspace
        .viewed_marks
        .iter()
        .any(|mark| mark.path == file.path);
    workspace
        .viewed_marks
        .retain(|mark| match_mark(mark, file) == ViewedState::Unviewed);
    let remaining = workspace
        .viewed_marks
        .iter()
        .any(|mark| mark.path == file.path);
    if legacy_only || !remaining {
        workspace.viewed_files.remove(file.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn workspace() -> ReviewWorkspace {
        ReviewWorkspace::from_target(
            PathBuf::from("/repo"),
            bcode_code_review_models::ReviewTarget::WorkingTreeUnstaged,
        )
    }

    fn file<'a>(old: &'a str, new: &'a str, patch: Option<&'a str>) -> ViewedIdentity<'a> {
        ViewedIdentity {
            path: "src/a.rs",
            old_oid: Some(old),
            new_oid: Some(new),
            patch_id: patch,
        }
    }

    #[test]
    fn same_blob_pair_is_viewed_without_hunks() {
        let mut workspace = workspace();
        mark_viewed(&mut workspace, &file("o1", "n1", Some("p1")));
        assert_eq!(
            viewed_state(&workspace, &file("o1", "n1", None)),
            ViewedState::Viewed
        );
        assert!(workspace.viewed_files.contains("src/a.rs"));
    }

    #[test]
    fn rebased_patch_with_equal_patch_id_stays_viewed() {
        let mut workspace = workspace();
        mark_viewed(&mut workspace, &file("o1", "n1", Some("p1")));
        let rebased = file("o2", "n2", Some("p1"));
        assert_eq!(
            viewed_state(&workspace, &rebased),
            ViewedState::ViewedNeedsUpdate
        );
        // Before hunks load, a different blob pair is not known to be viewed.
        assert_eq!(
            viewed_state(&workspace, &file("o2", "n2", None)),
            ViewedState::Unviewed
        );
        mark_viewed(&mut workspace, &rebased);
        assert_eq!(viewed_state(&workspace, &rebased), ViewedState::Viewed);
        assert_eq!(
            workspace.viewed_marks.len(),
            1,
            "re-recording replaces the mark"
        );
    }

    #[test]
    fn changed_patch_is_unviewed() {
        let mut workspace = workspace();
        mark_viewed(&mut workspace, &file("o1", "n1", Some("p1")));
        assert_eq!(
            viewed_state(&workspace, &file("o1", "n2", Some("p2"))),
            ViewedState::Unviewed
        );
    }

    #[test]
    fn legacy_path_marks_migrate_as_unknown_identity() {
        let mut workspace = workspace();
        workspace.viewed_files.insert("src/a.rs".to_string());
        assert_eq!(
            effective_marks(&workspace),
            vec![ViewedMark::legacy("src/a.rs")]
        );
        assert_eq!(
            viewed_state(&workspace, &file("o1", "n1", None)),
            ViewedState::ViewedNeedsUpdate
        );
        mark_viewed(&mut workspace, &file("o1", "n1", Some("p1")));
        assert_eq!(
            viewed_state(&workspace, &file("o1", "n9", Some("p9"))),
            ViewedState::Unviewed,
            "once identity is known, a changed patch is unviewed"
        );
    }

    #[test]
    fn working_tree_side_matches_only_by_patch_id() {
        let mut workspace = workspace();
        let live = ViewedIdentity {
            path: "src/a.rs",
            old_oid: Some("o1"),
            new_oid: None,
            patch_id: Some("p1"),
        };
        mark_viewed(&mut workspace, &live);
        assert!(viewed_state(&workspace, &live).is_viewed());
        let edited = ViewedIdentity {
            patch_id: Some("p2"),
            ..live
        };
        assert_eq!(viewed_state(&workspace, &edited), ViewedState::Unviewed);
        let unloaded = ViewedIdentity {
            patch_id: None,
            ..live
        };
        assert_eq!(viewed_state(&workspace, &unloaded), ViewedState::Unviewed);
    }

    #[test]
    fn working_tree_mark_without_patch_id_has_unknown_identity_until_re_recorded() {
        let mut workspace = workspace();
        let unloaded = ViewedIdentity {
            path: "src/a.rs",
            old_oid: Some("o1"),
            new_oid: None,
            patch_id: None,
        };
        mark_viewed(&mut workspace, &unloaded);
        assert_eq!(
            viewed_state(&workspace, &unloaded),
            ViewedState::ViewedNeedsUpdate
        );
        let loaded = ViewedIdentity {
            patch_id: Some("p1"),
            ..unloaded
        };
        mark_viewed(&mut workspace, &loaded);
        assert_eq!(workspace.viewed_marks.len(), 1);
        assert_eq!(
            viewed_state(
                &workspace,
                &ViewedIdentity {
                    patch_id: Some("p2"),
                    ..unloaded
                }
            ),
            ViewedState::Unviewed
        );
    }

    #[test]
    fn v1_unmark_hides_stale_identity_marks_and_unmark_clears_path() {
        let mut workspace = workspace();
        mark_viewed(&mut workspace, &file("o1", "n1", Some("p1")));
        workspace.viewed_files.remove("src/a.rs");
        assert_eq!(
            viewed_state(&workspace, &file("o1", "n1", None)),
            ViewedState::Unviewed
        );
        mark_viewed(&mut workspace, &file("o1", "n1", Some("p1")));
        unmark_viewed(&mut workspace, &file("o1", "n1", Some("p1")));
        assert_eq!(workspace.viewed_marks, Vec::new());
        assert!(!workspace.viewed_files.contains("src/a.rs"));
    }
}
