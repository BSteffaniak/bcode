//! Identity-pinned viewed marks.
//!
//! `ReviewWorkspace::viewed_files` remains the authoritative set of viewed paths so home, review
//! bundles, and reviews that recorded only paths keep working; `ReviewWorkspace::viewed_marks`
//! adds the identity each path was viewed at. A viewed path without a mark is a legacy mark of
//! unknown identity, and a mark whose path is no longer in `viewed_files` (unmarked by an older
//! client) is ignored.
//!
//! A file's identity is its two blob sides and its diff fingerprint (see
//! [`FINGERPRINT_VERSION`]). [`ViewedReset`] chooses which of them must stay the same for a
//! viewed file to stay viewed.

use bcode_code_review_models::{ReviewIndexEntry, ReviewWorkspace, ViewedMark};

/// Version of the diff fingerprint (the "patch id") new marks record.
///
/// Version 2 hashes, from a diff without context lines, any mode change and then, per hunk, a
/// separator, the hunk's section heading (git's `@@ … @@ heading`, the function or section the
/// change is in), and its changed lines with their markers. Line numbers and context are left
/// out, so changes elsewhere in the file keep the fingerprint, while a change that moves to
/// another section or splits differently does not. Version 2 fingerprints are written `2:<hex>`;
/// version 1 (changed lines only) wrote bare hex.
pub const FINGERPRINT_VERSION: u32 = 2;

const FINGERPRINT_PREFIX: &str = "2:";

/// The fingerprint version `fingerprint` was computed with.
#[must_use]
pub fn fingerprint_version(fingerprint: &str) -> u32 {
    if fingerprint.starts_with(FINGERPRINT_PREFIX) {
        FINGERPRINT_VERSION
    } else {
        1
    }
}

/// `digest` (hex) written as a current-version fingerprint.
#[must_use]
pub fn versioned_fingerprint(digest: &str) -> String {
    format!("{FINGERPRINT_PREFIX}{digest}")
}

/// When a viewed file stops counting as viewed (`viewed_reset` in the review's configuration).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewedReset {
    /// While the file's changes are the same (its diff fingerprint), it stays viewed, even when
    /// the content around them changes (for example after a rebase).
    #[default]
    Diff,
    /// Any change to either side of the file resets it. Working-tree content whose identity is
    /// not known yet is compared by its changes until it is.
    File,
}

/// The identity of one file as currently reviewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewedIdentity<'a> {
    /// File display path.
    pub path: &'a str,
    /// The path the file was renamed from, whose marks carry over.
    pub old_path: Option<&'a str>,
    /// Old-side identity (see [`ViewedMark::old_oid`]); `None` when not pinned.
    pub old_oid: Option<&'a str>,
    /// New-side identity; `None` when not pinned (working-tree content not identified yet).
    pub new_oid: Option<&'a str>,
    /// Diff fingerprint, once known.
    pub patch_id: Option<&'a str>,
}

/// Owned identity derived from an index entry (fingerprint unknown until loaded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryIdentity {
    /// File display path.
    pub path: String,
    /// The path a renamed file had.
    pub old_path: Option<String>,
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
            old_path: (entry.status == bcode_code_review_models::ReviewFileStatus::Renamed)
                .then(|| entry.old_path.clone())
                .flatten(),
            old_oid: entry.old_blob.viewed_identity(),
            new_oid: entry.new_blob.viewed_identity(),
        }
    }

    /// Identify working-tree content (a side the index leaves unpinned) by `content_oid`, the
    /// object id it would have if added.
    #[must_use]
    pub fn with_content(mut self, content_oid: Option<&str>) -> Self {
        if self.new_oid.is_none() {
            self.new_oid = content_oid.map(str::to_owned);
        }
        self
    }

    /// Borrow as a [`ViewedIdentity`] with an optional fingerprint.
    #[must_use]
    pub fn with_patch<'a>(&'a self, patch_id: Option<&'a str>) -> ViewedIdentity<'a> {
        ViewedIdentity {
            path: &self.path,
            old_path: self.old_path.as_deref(),
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
    /// Viewed at this exact identity (same blob pair and fingerprint).
    Viewed,
    /// Viewed, but the stored mark should be re-recorded with the current identity: a legacy
    /// mark of unknown identity, a mark matched by fingerprint after the blobs changed, a mark
    /// with an older fingerprint, or a mark made before the file was renamed.
    ViewedNeedsUpdate,
}

impl ViewedState {
    /// Whether the file displays as viewed.
    #[must_use]
    pub const fn is_viewed(self) -> bool {
        !matches!(self, Self::Unviewed)
    }

    const fn rank(self) -> u8 {
        match self {
            Self::Viewed => 2,
            Self::ViewedNeedsUpdate => 1,
            Self::Unviewed => 0,
        }
    }
}

/// Whether `mark` is for `file`: made at its path, or at the path it was renamed from.
fn names_file(mark: &ViewedMark, file: &ViewedIdentity<'_>) -> bool {
    mark.path == file.path || file.old_path.is_some_and(|old| mark.path == old)
}

/// Match one mark against a file identity under `reset`.
///
/// * A mark for another path (and not the path the file was renamed from): unviewed.
/// * Mark of unknown identity (legacy, or working-tree content marked before it was
///   identified): viewed until the identity is known; re-record it.
/// * Both sides pinned on mark and file and equal: viewed. Needs no fingerprint. Re-record it
///   when the file was renamed since or the mark's fingerprint is older or missing.
/// * [`ViewedReset::File`] with both sides known on mark and file: otherwise unviewed.
/// * Equal fingerprints of the current version: viewed (the changes survived a rebase or edits
///   around them); re-record it with the new blobs.
/// * Otherwise unviewed. A mark with an older fingerprint version therefore stays viewed only
///   while both blobs are unchanged.
#[must_use]
pub fn match_mark(mark: &ViewedMark, file: &ViewedIdentity<'_>, reset: ViewedReset) -> ViewedState {
    if !names_file(mark, file) {
        return ViewedState::Unviewed;
    }
    if !mark.has_identity() {
        return ViewedState::ViewedNeedsUpdate;
    }
    let renamed = mark.path != file.path;
    let pinned = |mark_side: Option<&String>, file_side: Option<&str>| matches!((mark_side, file_side), (Some(mark), Some(file)) if mark == file);
    if pinned(mark.old_oid.as_ref(), file.old_oid) && pinned(mark.new_oid.as_ref(), file.new_oid) {
        let stale = file
            .patch_id
            .is_some_and(|patch| mark.patch_id.as_deref() != Some(patch));
        return if renamed || stale {
            ViewedState::ViewedNeedsUpdate
        } else {
            ViewedState::Viewed
        };
    }
    let sides_known = mark.old_oid.is_some()
        && mark.new_oid.is_some()
        && file.old_oid.is_some()
        && file.new_oid.is_some();
    if reset == ViewedReset::File && sides_known {
        return ViewedState::Unviewed;
    }
    match (mark.patch_id.as_deref(), file.patch_id) {
        (Some(mark), Some(file))
            if mark == file && fingerprint_version(mark) == FINGERPRINT_VERSION =>
        {
            ViewedState::ViewedNeedsUpdate
        }
        _ => ViewedState::Unviewed,
    }
}

/// The best state any of `marks` gives `file`.
#[must_use]
pub fn best_state<'m>(
    marks: impl IntoIterator<Item = &'m ViewedMark>,
    file: &ViewedIdentity<'_>,
    reset: ViewedReset,
) -> ViewedState {
    marks
        .into_iter()
        .map(|mark| match_mark(mark, file, reset))
        .max_by_key(|state| state.rank())
        .unwrap_or(ViewedState::Unviewed)
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
pub fn viewed_state(
    workspace: &ReviewWorkspace,
    file: &ViewedIdentity<'_>,
    reset: ViewedReset,
) -> ViewedState {
    best_state(&effective_marks(workspace), file, reset)
}

/// Whether `existing` describes `file` closely enough to be replaced by a new mark for it.
fn replaced_by(existing: &ViewedMark, file: &ViewedIdentity<'_>) -> bool {
    names_file(existing, file)
        && (!existing.has_identity()
            || match_mark(existing, file, ViewedReset::Diff) != ViewedState::Unviewed)
}

fn new_mark(file: &ViewedIdentity<'_>) -> ViewedMark {
    ViewedMark {
        path: file.path.to_string(),
        old_oid: file.old_oid.map(str::to_string),
        new_oid: file.new_oid.map(str::to_string),
        patch_id: file.patch_id.map(str::to_string),
    }
}

/// Drop the renamed-from path of `file` from `viewed_files` once no mark is left for it.
fn forget_old_path(workspace: &mut ReviewWorkspace, file: &ViewedIdentity<'_>) {
    if let Some(old) = file.old_path.filter(|old| *old != file.path)
        && !workspace.viewed_marks.iter().any(|mark| mark.path == old)
    {
        workspace.viewed_files.remove(old);
    }
}

/// Record `file` as viewed at its current identity, replacing marks for it (including a legacy
/// mark, and marks made before a rename) and keeping `viewed_files` in sync.
pub fn mark_viewed(workspace: &mut ReviewWorkspace, file: &ViewedIdentity<'_>) {
    workspace
        .viewed_marks
        .retain(|existing| !replaced_by(existing, file));
    forget_old_path(workspace, file);
    workspace.viewed_marks.push(new_mark(file));
    workspace.viewed_marks.sort();
    workspace.viewed_files.insert(file.path.to_string());
}

/// Mark every file in `files` viewed (as [`mark_viewed`] does for one), sorting the marks once.
pub fn mark_all_viewed(workspace: &mut ReviewWorkspace, files: &[ViewedIdentity<'_>]) {
    workspace
        .viewed_marks
        .retain(|existing| !files.iter().any(|file| replaced_by(existing, file)));
    for file in files {
        forget_old_path(workspace, file);
        workspace.viewed_marks.push(new_mark(file));
        workspace.viewed_files.insert(file.path.to_string());
    }
    workspace.viewed_marks.sort();
    workspace.viewed_marks.dedup();
}

/// Remove every mark matching `file` (including marks made before a rename). Its paths leave
/// `viewed_files` once no mark for them remains.
pub fn unmark_viewed(workspace: &mut ReviewWorkspace, file: &ViewedIdentity<'_>) {
    workspace
        .viewed_marks
        .retain(|mark| match_mark(mark, file, ViewedReset::Diff) == ViewedState::Unviewed);
    for path in std::iter::once(file.path).chain(file.old_path) {
        if !workspace.viewed_marks.iter().any(|mark| mark.path == path) {
            workspace.viewed_files.remove(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const DIFF: ViewedReset = ViewedReset::Diff;
    const FILE: ViewedReset = ViewedReset::File;

    fn workspace() -> ReviewWorkspace {
        ReviewWorkspace::from_target(
            PathBuf::from("/repo"),
            bcode_code_review_models::ReviewTarget::WorkingTreeUnstaged,
        )
    }

    fn file<'a>(old: &'a str, new: &'a str, patch: Option<&'a str>) -> ViewedIdentity<'a> {
        ViewedIdentity {
            path: "src/a.rs",
            old_path: None,
            old_oid: Some(old),
            new_oid: Some(new),
            patch_id: patch,
        }
    }

    #[test]
    fn same_blob_pair_is_viewed_without_a_fingerprint() {
        let mut workspace = workspace();
        mark_viewed(&mut workspace, &file("o1", "n1", Some("2:p1")));
        for reset in [DIFF, FILE] {
            assert_eq!(
                viewed_state(&workspace, &file("o1", "n1", None), reset),
                ViewedState::Viewed
            );
        }
        assert!(workspace.viewed_files.contains("src/a.rs"));
    }

    #[test]
    fn changes_kept_around_other_edits_stay_viewed_only_when_resetting_by_diff() {
        let mut workspace = workspace();
        mark_viewed(&mut workspace, &file("o1", "n1", Some("2:p1")));
        // Both blobs changed (a rebase, or edits elsewhere in the file); the changes did not.
        let moved = file("o2", "n2", Some("2:p1"));
        assert_eq!(
            viewed_state(&workspace, &moved, DIFF),
            ViewedState::ViewedNeedsUpdate
        );
        assert_eq!(
            viewed_state(&workspace, &moved, FILE),
            ViewedState::Unviewed
        );
        // Before the fingerprint is known, a different blob pair is not known to be viewed.
        assert_eq!(
            viewed_state(&workspace, &file("o2", "n2", None), DIFF),
            ViewedState::Unviewed
        );
        mark_viewed(&mut workspace, &moved);
        assert_eq!(viewed_state(&workspace, &moved, DIFF), ViewedState::Viewed);
        assert_eq!(
            workspace.viewed_marks.len(),
            1,
            "re-recording replaces the mark"
        );
    }

    #[test]
    fn changed_changes_are_unviewed() {
        let mut workspace = workspace();
        mark_viewed(&mut workspace, &file("o1", "n1", Some("2:p1")));
        for reset in [DIFF, FILE] {
            assert_eq!(
                viewed_state(&workspace, &file("o1", "n2", Some("2:p2")), reset),
                ViewedState::Unviewed
            );
        }
    }

    #[test]
    fn older_fingerprints_keep_equal_blobs_viewed_and_are_re_recorded() {
        let mut workspace = workspace();
        // A mark from before the fingerprint gained hunk sections.
        mark_viewed(&mut workspace, &file("o1", "n1", Some("abc123")));
        assert_eq!(fingerprint_version("abc123"), 1);
        assert_eq!(fingerprint_version("2:abc123"), FINGERPRINT_VERSION);
        let current = file("o1", "n1", Some("2:def"));
        assert_eq!(
            viewed_state(&workspace, &current, DIFF),
            ViewedState::ViewedNeedsUpdate
        );
        // Different blobs cannot be compared across fingerprint versions: unviewed, even when
        // the digests happen to be equal.
        assert_eq!(
            viewed_state(&workspace, &file("o2", "n2", Some("abc123")), DIFF),
            ViewedState::Unviewed
        );
        mark_viewed(&mut workspace, &current);
        assert_eq!(
            viewed_state(&workspace, &current, DIFF),
            ViewedState::Viewed
        );
        assert_eq!(workspace.viewed_marks.len(), 1);
    }

    #[test]
    fn marks_follow_a_rename() {
        let mut workspace = workspace();
        mark_viewed(&mut workspace, &file("o1", "n1", Some("2:p1")));
        let renamed = ViewedIdentity {
            path: "src/b.rs",
            old_path: Some("src/a.rs"),
            ..file("o1", "n1", Some("2:p1"))
        };
        assert_eq!(
            viewed_state(&workspace, &renamed, DIFF),
            ViewedState::ViewedNeedsUpdate
        );
        mark_viewed(&mut workspace, &renamed);
        assert_eq!(
            viewed_state(&workspace, &renamed, DIFF),
            ViewedState::Viewed
        );
        assert!(workspace.viewed_files.contains("src/b.rs"));
        assert!(!workspace.viewed_files.contains("src/a.rs"));
        assert_eq!(workspace.viewed_marks.len(), 1);
        // A legacy path mark follows a rename too.
        let mut legacy = self::workspace();
        legacy.viewed_files.insert("src/a.rs".to_owned());
        assert!(viewed_state(&legacy, &renamed, FILE).is_viewed());
        unmark_viewed(&mut workspace, &renamed);
        assert!(workspace.viewed_files.is_empty());
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
            viewed_state(&workspace, &file("o1", "n1", None), DIFF),
            ViewedState::ViewedNeedsUpdate
        );
        mark_viewed(&mut workspace, &file("o1", "n1", Some("2:p1")));
        assert_eq!(
            viewed_state(&workspace, &file("o1", "n9", Some("2:p9")), DIFF),
            ViewedState::Unviewed,
            "once identity is known, a changed patch is unviewed"
        );
    }

    #[test]
    fn unidentified_working_tree_content_matches_by_fingerprint() {
        let mut workspace = workspace();
        let live = ViewedIdentity {
            path: "src/a.rs",
            old_path: None,
            old_oid: Some("o1"),
            new_oid: None,
            patch_id: Some("2:p1"),
        };
        mark_viewed(&mut workspace, &live);
        for reset in [DIFF, FILE] {
            assert!(viewed_state(&workspace, &live, reset).is_viewed());
        }
        let edited = ViewedIdentity {
            patch_id: Some("2:p2"),
            ..live
        };
        assert_eq!(
            viewed_state(&workspace, &edited, DIFF),
            ViewedState::Unviewed
        );
        let unloaded = ViewedIdentity {
            patch_id: None,
            ..live
        };
        assert_eq!(
            viewed_state(&workspace, &unloaded, DIFF),
            ViewedState::Unviewed
        );
        // Once the content is identified, the mark is re-recorded with it, and from then on
        // `file` compares the content itself.
        let identified = ViewedIdentity {
            new_oid: Some("w1"),
            ..live
        };
        assert_eq!(
            viewed_state(&workspace, &identified, FILE),
            ViewedState::ViewedNeedsUpdate
        );
        mark_viewed(&mut workspace, &identified);
        let touched = ViewedIdentity {
            new_oid: Some("w2"),
            ..live
        };
        assert_eq!(
            viewed_state(&workspace, &touched, FILE),
            ViewedState::Unviewed
        );
        assert!(viewed_state(&workspace, &touched, DIFF).is_viewed());
    }

    #[test]
    fn working_tree_mark_without_fingerprint_has_unknown_identity_until_re_recorded() {
        let mut workspace = workspace();
        let unloaded = ViewedIdentity {
            path: "src/a.rs",
            old_path: None,
            old_oid: Some("o1"),
            new_oid: None,
            patch_id: None,
        };
        mark_viewed(&mut workspace, &unloaded);
        assert_eq!(
            viewed_state(&workspace, &unloaded, DIFF),
            ViewedState::ViewedNeedsUpdate
        );
        let loaded = ViewedIdentity {
            patch_id: Some("2:p1"),
            ..unloaded
        };
        mark_viewed(&mut workspace, &loaded);
        assert_eq!(workspace.viewed_marks.len(), 1);
        assert_eq!(
            viewed_state(
                &workspace,
                &ViewedIdentity {
                    patch_id: Some("2:p2"),
                    ..unloaded
                },
                DIFF
            ),
            ViewedState::Unviewed
        );
    }

    #[test]
    fn removed_viewed_path_hides_identity_marks_and_unmark_clears_path() {
        let mut workspace = workspace();
        mark_viewed(&mut workspace, &file("o1", "n1", Some("2:p1")));
        workspace.viewed_files.remove("src/a.rs");
        assert_eq!(
            viewed_state(&workspace, &file("o1", "n1", None), DIFF),
            ViewedState::Unviewed
        );
        mark_viewed(&mut workspace, &file("o1", "n1", Some("2:p1")));
        unmark_viewed(&mut workspace, &file("o1", "n1", Some("2:p1")));
        assert_eq!(workspace.viewed_marks, Vec::new());
        assert!(!workspace.viewed_files.contains("src/a.rs"));
    }
}
