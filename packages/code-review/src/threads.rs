//! A thread's three states, kept apart (see `docs/review-thread-cards.md`):
//!
//! * **Resolved** is everyone's: the discussion is settled (the forge's state, or a draft's).
//! * **Viewed** is yours: you read it as it is. A mark pins the comments it had, so a comment
//!   from someone else afterwards makes it unviewed again, with that many new.
//! * **Folded** is how it shows: by itself from viewed (and resolved, by setting), or by hand. A
//!   fold by hand records how many comments the thread had, so a new comment can open it again
//!   (or leave it folded with a *new* chip, by setting).

use bcode_code_review_models::{ThreadFoldMark, ThreadViewMark};

/// Whether you have read a thread as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadViewed {
    /// Never marked.
    Unviewed,
    /// Marked, and nothing came from anyone else since.
    Viewed,
    /// Marked, and `new` comments from others came since.
    Stale {
        /// Comments from others since the mark.
        new: usize,
    },
}

impl ThreadViewed {
    /// Whether it shows as viewed.
    #[must_use]
    pub const fn is_viewed(self) -> bool {
        matches!(self, Self::Viewed)
    }

    /// Comments from others since it was marked.
    #[must_use]
    pub const fn new_count(self) -> usize {
        match self {
            Self::Stale { new } => new,
            Self::Unviewed | Self::Viewed => 0,
        }
    }
}

/// One comment of a thread, as viewed state needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommentRef<'a> {
    /// Comment id.
    pub id: &'a str,
    /// Written by you: your own comments never make a thread unviewed.
    pub mine: bool,
}

/// The viewed state of a thread with `comments`, given its `mark`.
#[must_use]
pub fn viewed(mark: Option<&ThreadViewMark>, comments: &[CommentRef]) -> ThreadViewed {
    let Some(mark) = mark else {
        return ThreadViewed::Unviewed;
    };
    let new = comments
        .iter()
        .filter(|comment| is_new(Some(mark), **comment))
        .count();
    if new == 0 {
        ThreadViewed::Viewed
    } else {
        ThreadViewed::Stale { new }
    }
}

/// Whether `comment` came after `mark` (from someone else). Nothing is new on a thread never
/// marked: all of it is unread.
#[must_use]
pub fn is_new(mark: Option<&ThreadViewMark>, comment: CommentRef) -> bool {
    mark.is_some_and(|mark| !comment.mine && !mark.comment_ids.iter().any(|id| id == comment.id))
}

/// A mark for a thread with `comments`, made at `now_ms`.
#[must_use]
pub fn mark(thread_id: &str, comments: &[CommentRef], now_ms: u64) -> ThreadViewMark {
    ThreadViewMark {
        thread_id: thread_id.to_owned(),
        comment_ids: comments
            .iter()
            .map(|comment| comment.id.to_owned())
            .collect(),
        viewed_at_ms: now_ms,
    }
}

/// What a comment arriving does to a folded thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NewComments {
    /// It opens again.
    #[default]
    Reopen,
    /// It stays folded, with a *new* chip.
    KeepFolded,
}

/// What folds a thread by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldRules {
    /// Viewed threads fold.
    pub fold_viewed: bool,
    /// Resolved threads fold.
    pub fold_resolved: bool,
    /// What a new comment does to a folded thread.
    pub new_comments: NewComments,
}

impl Default for FoldRules {
    fn default() -> Self {
        Self {
            fold_viewed: true,
            fold_resolved: true,
            new_comments: NewComments::Reopen,
        }
    }
}

/// Whether a thread shows folded: its fold by `hand` (made when it had that many comments),
/// else by itself from `viewed` and `resolved`. The thread has `comments` comments now.
#[must_use]
pub fn folded(
    hand: Option<ThreadFoldMark>,
    viewed: ThreadViewed,
    resolved: bool,
    comments: usize,
    rules: FoldRules,
) -> bool {
    let keep = rules.new_comments == NewComments::KeepFolded;
    let by_itself = match viewed {
        ThreadViewed::Viewed => rules.fold_viewed || (rules.fold_resolved && resolved),
        // Something new: open, unless folds are kept (then as it was when viewed).
        ThreadViewed::Stale { .. } => {
            keep && (rules.fold_viewed || (rules.fold_resolved && resolved))
        }
        ThreadViewed::Unviewed => rules.fold_resolved && resolved,
    };
    // A fold by hand holds, unless comments came since it and they reopen threads.
    hand.filter(|mark| keep || mark.comments.is_none_or(|count| comments <= count))
        .map_or(by_itself, |mark| mark.folded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comments<'a>(ids: &'a [(&'a str, bool)]) -> Vec<CommentRef<'a>> {
        ids.iter()
            .map(|(id, mine)| CommentRef { id, mine: *mine })
            .collect()
    }

    #[test]
    fn a_comment_from_someone_else_makes_a_viewed_thread_stale_and_yours_does_not() {
        let before = comments(&[("a", false), ("b", true)]);
        let mark = mark("t", &before, 1);
        assert_eq!(viewed(Some(&mark), &before), ThreadViewed::Viewed);
        let mine = comments(&[("a", false), ("b", true), ("c", true)]);
        assert_eq!(viewed(Some(&mark), &mine), ThreadViewed::Viewed);
        let theirs = comments(&[("a", false), ("b", true), ("c", true), ("d", false)]);
        assert_eq!(viewed(Some(&mark), &theirs), ThreadViewed::Stale { new: 1 });
        assert!(is_new(Some(&mark), theirs[3]));
        assert!(!is_new(None, theirs[3]));
        assert_eq!(viewed(None, &theirs), ThreadViewed::Unviewed);
    }

    #[test]
    fn folds_follow_viewed_and_resolved_and_hand_folds_follow_the_setting() {
        let rules = FoldRules::default();
        let keep = FoldRules {
            new_comments: NewComments::KeepFolded,
            ..rules
        };
        let stale = ThreadViewed::Stale { new: 1 };
        // By itself.
        assert!(folded(None, ThreadViewed::Viewed, false, 2, rules));
        assert!(!folded(None, ThreadViewed::Unviewed, false, 2, rules));
        assert!(folded(None, ThreadViewed::Unviewed, true, 2, rules));
        assert!(!folded(None, stale, false, 3, rules));
        assert!(folded(None, stale, false, 3, keep));
        let no_resolved = FoldRules {
            fold_resolved: false,
            ..rules
        };
        assert!(!folded(None, ThreadViewed::Unviewed, true, 2, no_resolved));
        // By hand, at two comments.
        let hand = |folded: bool| {
            Some(ThreadFoldMark {
                folded,
                comments: Some(2),
            })
        };
        assert!(folded(hand(true), ThreadViewed::Unviewed, false, 2, rules));
        assert!(!folded(hand(false), ThreadViewed::Viewed, false, 2, rules));
        // A third comment: reopened, or kept folded.
        assert!(!folded(hand(true), ThreadViewed::Unviewed, false, 3, rules));
        assert!(folded(hand(true), ThreadViewed::Unviewed, false, 3, keep));
        // A fold from before counts were kept never lapses.
        let legacy = Some(ThreadFoldMark {
            folded: true,
            comments: None,
        });
        assert!(folded(legacy, ThreadViewed::Unviewed, false, 9, rules));
    }
}
