//! Thread listings and full threads fitted to a size, as the tools encode them (pretty JSON).
//!
//! What goes first: a listing shortens its page of pull request threads (down to one; the page's
//! `next_offset` continues it), then leaves out local threads and pending suggestions from the
//! end (counted). A full thread leaves out its oldest replies (keeping the first comment and the
//! latest ones, counted), then shortens its diff hunk (keeping the end, where the comment is),
//! then its longest comment bodies.

use super::{PullThreadView, ThreadView, ThreadsView, bounded};
use serde::Serialize;

/// Bytes of a thread listing, or of one thread, encoded as pretty JSON at most.
pub const MAX_THREADS_OUTPUT_BYTES: usize = 96 * 1024;
/// Bytes of the diff hunk a full pull request thread shows at most (its end is kept).
pub const MAX_HUNK_BYTES: usize = 16 * 1024;
/// Marks the start of a diff hunk that was shortened.
pub const HUNK_CUT: &str = "[… earlier diff lines left out]\n";
/// Bytes below which a hunk or a comment body is not shortened further.
const MIN_TEXT_BYTES: usize = 256;

fn pretty_len<T: Serialize>(value: &T) -> usize {
    serde_json::to_string_pretty(value).map_or(0, |text| text.len())
}

fn compact_len<T: Serialize>(value: &T) -> usize {
    serde_json::to_string(value).map_or(0, |text| text.len())
}

/// Remove items of `items` at `index(len)` until their encoded size covers `excess` (at least
/// one) while more than `keep` remain; how many were removed.
fn remove<T: Serialize>(
    items: &mut Vec<T>,
    index: impl Fn(usize) -> usize,
    keep: usize,
    excess: usize,
) -> usize {
    let (mut removed, mut freed) = (0, 0);
    while items.len() > keep && (removed == 0 || freed < excess) {
        let at = index(items.len());
        freed += compact_len(&items[at]);
        items.remove(at);
        removed += 1;
    }
    removed
}

const fn last(len: usize) -> usize {
    len - 1
}

const fn oldest_reply(_len: usize) -> usize {
    1
}

/// The end of `hunk` within `max` bytes, from a line start, marked with [`HUNK_CUT`] when cut.
#[must_use]
pub fn hunk_tail(hunk: &str, max: usize) -> String {
    if hunk.len() <= max {
        return hunk.to_owned();
    }
    let mut start = hunk.len() - max;
    while !hunk.is_char_boundary(start) {
        start += 1;
    }
    let start = hunk[start..]
        .find('\n')
        .map_or(start, |newline| start + newline + 1);
    format!("{HUNK_CUT}{}", &hunk[start..])
}

/// Halve the longest of `bodies` when it is long enough; whether one was.
fn halve_longest<'a>(bodies: impl Iterator<Item = &'a mut String>) -> bool {
    let Some(body) = bodies
        .filter(|body| body.len() > MIN_TEXT_BYTES)
        .max_by_key(|body| body.len())
    else {
        return false;
    };
    *body = bounded(body, body.len() / 2);
    true
}

/// `view` within `max` bytes of pretty JSON.
pub fn listing(view: &mut ThreadsView, max: usize) {
    loop {
        let size = pretty_len(view);
        if size <= max {
            return;
        }
        let excess = size - max;
        let page = &mut view.pull_request_threads;
        if page.threads.len() > 1 {
            remove(&mut page.threads, last, 1, excess);
            let end = page.offset + page.threads.len();
            page.next_offset = (end < page.total).then_some(end);
        } else if !view.threads.is_empty() {
            view.omitted += remove(&mut view.threads, last, 0, excess);
        } else if !view.suggestions.is_empty() {
            view.suggestions_omitted += remove(&mut view.suggestions, last, 0, excess);
        } else {
            return;
        }
    }
}

/// `view` within `max` bytes of pretty JSON.
pub fn pull_thread(view: &mut PullThreadView, max: usize) {
    loop {
        let size = pretty_len(view);
        if size <= max {
            return;
        }
        let excess = size - max;
        if view.comments.len() > 2 {
            view.comments_omitted += remove(&mut view.comments, oldest_reply, 2, excess);
        } else if let Some(hunk) = view
            .diff_hunk
            .as_mut()
            .filter(|hunk| hunk.len() > MIN_TEXT_BYTES)
        {
            let shorter = hunk_tail(hunk.strip_prefix(HUNK_CUT).unwrap_or(hunk), hunk.len() / 2);
            *hunk = shorter;
        } else if !halve_longest(view.comments.iter_mut().map(|comment| &mut comment.body)) {
            return;
        }
    }
}

/// `view` within `max` bytes of pretty JSON.
pub fn local_thread(view: &mut ThreadView, max: usize) {
    loop {
        let size = pretty_len(view);
        if size <= max {
            return;
        }
        let excess = size - max;
        if view.comments.len() > 2 {
            view.comments_omitted += remove(&mut view.comments, oldest_reply, 2, excess);
        } else if !halve_longest(view.comments.iter_mut()) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant::{SuggestionView, ThreadOrigin};
    use bcode_code_review_models::{ReviewThreadKind, ReviewThreadSeverity};

    fn local(index: usize, comments: usize, bytes: usize) -> ThreadView {
        ThreadView {
            thread_id: format!("t{index}"),
            origin: ThreadOrigin::Local,
            location: "src/lib.rs:1".to_owned(),
            kind: ReviewThreadKind::default(),
            severity: ReviewThreadSeverity::default(),
            resolved: false,
            comments: (0..comments)
                .map(|comment| format!("{comment} {}", "z".repeat(bytes)))
                .collect(),
            comments_omitted: 0,
        }
    }

    #[test]
    fn a_long_local_thread_keeps_its_first_and_latest_comments() {
        let mut view = local(0, 40, 8 * 1024);
        local_thread(&mut view, MAX_THREADS_OUTPUT_BYTES);
        assert!(pretty_len(&view) <= MAX_THREADS_OUTPUT_BYTES);
        assert!(view.comments[0].starts_with("0 "));
        assert!(
            view.comments
                .last()
                .is_some_and(|body| body.starts_with("39 "))
        );
        assert_eq!(view.comments.len() + view.comments_omitted, 40);
    }

    #[test]
    fn a_listing_leaves_out_local_threads_from_the_end_counted() {
        let mut view = ThreadsView {
            threads: (0..100).map(|index| local(index, 3, 1024)).collect(),
            suggestions: (0..20)
                .map(|index| SuggestionView {
                    suggestion_id: format!("s{index}"),
                    location: "src/lib.rs:1".to_owned(),
                    body: "s".repeat(1024),
                })
                .collect(),
            omitted: 2,
            suggestions_omitted: 0,
            pull_request_threads: super::super::PullThreadsView::default(),
        };
        listing(&mut view, MAX_THREADS_OUTPUT_BYTES);
        assert!(pretty_len(&view) <= MAX_THREADS_OUTPUT_BYTES);
        assert_eq!(view.threads.len() + view.omitted, 102);
        assert_eq!(view.suggestions.len() + view.suggestions_omitted, 20);
        assert_eq!(view.suggestions_omitted, 0, "local threads go first");
        assert_eq!(view.threads[0].thread_id, "t0", "the first ones stay");
    }

    #[test]
    fn a_hunk_keeps_its_end_from_a_line_start() {
        let hunk = (0..100)
            .map(|line| format!("+line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = hunk_tail(&hunk, 40);
        assert!(tail.len() <= 40 + HUNK_CUT.len());
        let kept = tail.strip_prefix(HUNK_CUT).expect("marked");
        assert!(kept.starts_with("+line "), "{kept}");
        assert!(kept.ends_with("+line 99"));
        assert_eq!(hunk_tail("short", 40), "short");
    }
}
