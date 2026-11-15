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

use bcode_code_review_models::ReviewWorkspace;
use std::collections::BTreeSet;

pub use bcode_code_review::workspace_merge::{ViewedSet as Viewed, merge_viewed as merge};

/// The stored state the review last read or wrote.
#[derive(Debug, Clone, Default)]
pub struct Tracking {
    /// An unsaved review's viewed files, marks and threads as last stored for its target.
    pub transient: Option<(
        BTreeSet<String>,
        Vec<bcode_code_review_models::ViewedMark>,
        Vec<bcode_code_review_models::ThreadViewMark>,
    )>,
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
