//! Gap math (spec section 5.2).
//!
//! Context is counted on one *context side*: the new side, or the old side for files whose new
//! side does not exist (deleted files). A hunk covers lines `[first, next)` on each side, where
//! git reports an empty side as `start,0` meaning "after line `start`". The regions between,
//! before, and after hunks are the complement of hunk coverage within `[1, line count]`; each
//! region has a constant offset to the other side, so both line numbers of every gap and
//! revealed line are derived from hunk offsets rather than read from the file.

use bcode_code_review_models::{ReviewHunk, ReviewLineKind, ReviewLineSide};

/// Line ranges of one hunk as reported by git, plus its trailing context length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HunkRange {
    /// Old-side start line.
    pub old_start: u32,
    /// Old-side line count.
    pub old_count: u32,
    /// New-side start line.
    pub new_start: u32,
    /// New-side line count.
    pub new_count: u32,
    /// Context lines at the end of the hunk.
    pub trailing_context: u32,
}

impl HunkRange {
    /// Ranges of a parsed hunk.
    #[must_use]
    pub fn of(hunk: &ReviewHunk) -> Self {
        let trailing = hunk
            .lines
            .iter()
            .rev()
            .take_while(|line| line.kind == ReviewLineKind::Context)
            .count();
        Self {
            old_start: hunk.old_start,
            old_count: hunk.old_count,
            new_start: hunk.new_start,
            new_count: hunk.new_count,
            trailing_context: u32::try_from(trailing).unwrap_or(u32::MAX),
        }
    }
}

/// Lines `[first, next)` a hunk covers on one side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// First covered line (equals `next` when the side is empty).
    pub first: u32,
    /// First line after the hunk.
    pub next: u32,
}

/// Coverage of one side of a hunk from its git header.
#[must_use]
pub const fn span(start: u32, count: u32) -> Span {
    if count == 0 {
        Span {
            first: start + 1,
            next: start + 1,
        }
    } else {
        Span {
            first: start,
            next: start + count,
        }
    }
}

/// Coverage of `hunk` on `side`.
#[must_use]
pub const fn hunk_span(hunk: &HunkRange, side: ReviewLineSide) -> Span {
    match side {
        ReviewLineSide::Old => span(hunk.old_start, hunk.old_count),
        ReviewLineSide::New => span(hunk.new_start, hunk.new_count),
    }
}

/// One stretch of unchanged lines outside every hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    /// First line on the context side.
    pub start: u32,
    /// Last line on the context side; `None` when the region runs to an unknown end of file.
    pub end: Option<u32>,
    /// Offset from a context-side line to the other side's line, when the other side exists.
    pub other_offset: Option<i64>,
    /// Index of the hunk this region precedes; `None` for the region after the last hunk.
    pub before_hunk: Option<usize>,
}

impl Region {
    /// Other-side line of context-side `line`.
    #[must_use]
    pub fn other_line(&self, line: u32) -> Option<u32> {
        self.other_offset
            .and_then(|offset| u32::try_from(i64::from(line) + offset).ok())
    }

    /// Whether `line` lies in this region.
    #[must_use]
    pub fn contains(&self, line: u32) -> bool {
        line >= self.start && self.end.is_none_or(|end| line <= end)
    }
}

/// Inputs for region computation.
#[derive(Debug, Clone, Copy)]
pub struct RegionInput<'a> {
    /// Hunks in file order.
    pub hunks: &'a [HunkRange],
    /// Context side.
    pub side: ReviewLineSide,
    /// Whether the other side exists.
    pub other_exists: bool,
    /// Context-side line count, when known.
    pub line_count: Option<u32>,
    /// Whether the hunks were truncated (no region follows the last returned hunk).
    pub truncated: bool,
}

/// Compute the unchanged regions of a file in ascending order. Empty regions are omitted.
#[must_use]
pub fn regions(input: RegionInput<'_>) -> Vec<Region> {
    let other = match input.side {
        ReviewLineSide::Old => ReviewLineSide::New,
        ReviewLineSide::New => ReviewLineSide::Old,
    };
    let mut out = Vec::with_capacity(input.hunks.len() + 1);
    let mut cursor = 1_u32;
    let mut offset = 0_i64;
    for (index, hunk) in input.hunks.iter().enumerate() {
        let own = hunk_span(hunk, input.side);
        if own.first > cursor {
            let end = own.first - 1;
            let end = input.line_count.map_or(end, |count| end.min(count));
            if end >= cursor {
                out.push(Region {
                    start: cursor,
                    end: Some(end),
                    other_offset: input.other_exists.then_some(offset),
                    before_hunk: Some(index),
                });
            }
        }
        let theirs = hunk_span(hunk, other);
        cursor = cursor.max(own.next);
        offset = i64::from(theirs.next) - i64::from(own.next);
    }
    if input.truncated {
        return out;
    }
    let region = Region {
        start: cursor,
        end: input.line_count,
        other_offset: input.other_exists.then_some(offset),
        before_hunk: None,
    };
    if region.end.is_none_or(|end| end >= cursor) {
        out.push(region);
    }
    out
}

/// The context-side line count when the last hunk provably reaches the end of the file: it ends
/// with fewer trailing context lines than were requested, so git ran out of lines.
#[must_use]
pub fn line_count_from_trailing_context(
    hunks: &[HunkRange],
    side: ReviewLineSide,
    context_lines: u32,
) -> Option<u32> {
    let last = hunks.last()?;
    (last.trailing_context < context_lines).then(|| hunk_span(last, side).next - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(old_start: u32, old_count: u32, new_start: u32, new_count: u32) -> HunkRange {
        HunkRange {
            old_start,
            old_count,
            new_start,
            new_count,
            trailing_context: 0,
        }
    }

    fn input(hunks: &[HunkRange], line_count: Option<u32>) -> RegionInput<'_> {
        RegionInput {
            hunks,
            side: ReviewLineSide::New,
            other_exists: true,
            line_count,
            truncated: false,
        }
    }

    #[test]
    fn regions_carry_both_line_numbers() {
        // Old 1..=100, new 1..=102: hunk 1 at old 10..=12 → new 10..=14 (+2 lines),
        // hunk 2 at old 50..=52 → new 52..=54.
        let hunks = [hunk(10, 3, 10, 5), hunk(50, 3, 52, 3)];
        let regions = regions(input(&hunks, Some(102)));
        assert_eq!(regions.len(), 3);
        assert_eq!((regions[0].start, regions[0].end), (1, Some(9)));
        assert_eq!(regions[0].other_line(9), Some(9));
        assert_eq!((regions[1].start, regions[1].end), (15, Some(51)));
        assert_eq!(regions[1].other_line(15), Some(13));
        assert_eq!(regions[1].other_line(51), Some(49));
        assert_eq!((regions[2].start, regions[2].end), (55, Some(102)));
        assert_eq!(regions[2].other_line(102), Some(100));
    }

    #[test]
    fn empty_sides_cover_nothing() {
        // Pure deletion of old 5..=6 after new line 4, then pure addition after old line 20.
        let hunks = [hunk(5, 2, 4, 0), hunk(20, 0, 19, 3)];
        let regions = regions(input(&hunks, Some(30)));
        assert_eq!((regions[0].start, regions[0].end), (1, Some(4)));
        assert_eq!((regions[1].start, regions[1].end), (5, Some(18)));
        assert_eq!(regions[1].other_line(5), Some(7));
        assert_eq!(regions[1].other_line(18), Some(20));
        assert_eq!((regions[2].start, regions[2].end), (22, Some(30)));
        assert_eq!(regions[2].other_line(22), Some(21));
    }

    #[test]
    fn unknown_line_count_leaves_trailing_region_open() {
        let hunks = [hunk(1, 2, 1, 2)];
        let regions = regions(input(&hunks, None));
        assert_eq!(regions.len(), 1);
        assert_eq!((regions[0].start, regions[0].end), (3, None));
        let truncated = RegionInput {
            truncated: true,
            ..input(&hunks, None)
        };
        assert_eq!(super::regions(truncated), Vec::new());
    }

    #[test]
    fn whole_file_hunks_leave_no_gaps() {
        let added = [hunk(0, 0, 1, 7)];
        assert_eq!(regions(input(&added, Some(7))), Vec::new());
        let deleted = [hunk(1, 9, 0, 0)];
        let old_side = RegionInput {
            hunks: &deleted,
            side: ReviewLineSide::Old,
            other_exists: false,
            line_count: Some(9),
            truncated: false,
        };
        assert_eq!(regions(old_side), Vec::new());
    }
}
