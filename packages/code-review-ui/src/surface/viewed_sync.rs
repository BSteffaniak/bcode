//! Keeping changes written elsewhere while the review is open.
//!
//! The review saves its workspace (presentation, review pass, viewed state). The same saved
//! review can change while it is open: the assistant's `code_review.mark_viewed`,
//! `update_review` (rename, archive) and `delete_review`, an AI card, home, or another app on the
//! same review. So the review never writes its whole copy over the store:
//!
//! * **Saves** send the record as the review last read or wrote it (**base**) with the review's
//!   own copy (**local**); the store writes only what local changed from base over what it holds
//!   now (see [`bcode_code_review::workspace_merge`]) and answers with the result. A saved review
//!   deleted meanwhile is not written again.
//! * **Answers and reloads** rebase the review's copy onto what the store holds: fields and
//!   files the reviewer changed since the base keep the reviewer's state; every other one takes
//!   the stored state. So a save never undoes a change made elsewhere, and a reload never drops
//!   a change the reviewer made that is still being saved.

use bcode_code_review::document::{FileRef, ItemKey, ReviewDocument};
use bcode_code_review::viewed::{self, EntryIdentity, ViewedIdentity};
use bcode_code_review_models::ReviewGroupKey;
use bcode_code_review_models::ReviewWorkspace;

pub use bcode_code_review::workspace_merge::{ViewedSet as Viewed, merge_viewed as merge};

/// The stored state the review last read or wrote.
#[derive(Debug, Clone, Default)]
pub struct Tracking {
    /// An unsaved review's viewed files, marks and threads as last stored for its target.
    pub transient: Option<Viewed>,
    /// The viewed state as last read from or written to the store.
    pub base: Option<Viewed>,
    /// A saved review's whole record as last read from or written to the store: the base of
    /// its saves.
    pub stored: Option<ReviewWorkspace>,
    /// The record being saved, while a save runs.
    pub saving: Option<ReviewWorkspace>,
    /// The saved review was deleted elsewhere: nothing is saved any more.
    pub deleted: bool,
    /// How many of the review's own writes were taken as the stored record: a read started
    /// before the last of them may predate it and is read again instead of applied.
    pub writes: u64,
}

/// Why a save failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveFailure {
    /// The saved review was deleted meanwhile; it is not written again.
    Deleted,
    /// Any other failure, with its message.
    Failed(String),
}

/// Capture the group before taking the mutable workspace borrow. A file can occur at the
/// exact same path and blob sides on both representations of a series.
pub(super) fn identity(
    doc: &ReviewDocument,
    file: &FileRef,
) -> Option<(ReviewGroupKey, EntryIdentity, Option<String>)> {
    let group = doc.group_of_key(&ItemKey::FileHeader(file.clone()))?;
    let (identity, patch) = doc.viewed_identity(file)?;
    Some((group.clone(), identity, patch))
}

pub(super) fn set(
    workspace: &mut ReviewWorkspace,
    group: &ReviewGroupKey,
    file: &ViewedIdentity<'_>,
    value: bool,
) {
    match (group, value) {
        (ReviewGroupKey::Standalone { .. }, true) => viewed::mark_viewed(workspace, file),
        (ReviewGroupKey::Standalone { .. }, false) => viewed::unmark_viewed(workspace, file),
        (_, true) => viewed::mark_group_viewed(workspace, group, std::slice::from_ref(file)),
        (_, false) => viewed::unmark_group_viewed(workspace, group, file),
    }
}

/// Update a batch together so marking a whole review sorts each group's marks just once.
pub(super) fn set_batch(
    workspace: &mut ReviewWorkspace,
    batch: &[(ReviewGroupKey, EntryIdentity, Option<String>)],
    value: bool,
) {
    let mut groups = std::collections::BTreeMap::<_, Vec<_>>::new();
    for (key, identity, patch) in batch {
        groups
            .entry(key)
            .or_default()
            .push(identity.with_patch(patch.as_deref()));
    }
    for (key, files) in groups {
        if !value {
            for file in files {
                set(workspace, key, &file, false);
            }
        } else if matches!(key, ReviewGroupKey::Standalone { .. }) {
            viewed::mark_all_viewed(workspace, &files);
        } else {
            viewed::mark_group_viewed(workspace, key, &files);
        }
    }
}
