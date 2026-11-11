//! Inclusive `u32` interval sets (spec section 5.2).
//!
//! Intervals are kept disjoint and non-adjacent in a `BTreeMap` keyed by start line: inserting
//! merges every overlapping or touching interval, so the representation of a set is unique and
//! does not depend on insertion order.

use bcode_code_review_models::ReviewLineInterval;
use std::collections::BTreeMap;

/// A set of 1-based line numbers stored as merged inclusive intervals.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntervalSet {
    /// Start line → end line (inclusive). Intervals never overlap or touch.
    ranges: BTreeMap<u32, u32>,
}

impl IntervalSet {
    /// An empty set.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            ranges: BTreeMap::new(),
        }
    }

    /// Whether the set contains no lines.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Number of disjoint intervals.
    #[must_use]
    pub fn interval_count(&self) -> usize {
        self.ranges.len()
    }

    /// Insert `[start, end]`, merging overlapping and adjacent intervals. Returns whether the set
    /// changed. An empty range (`start > end`) is ignored.
    pub fn insert(&mut self, start: u32, end: u32) -> bool {
        if start > end || self.contains_range(start, end) {
            return false;
        }
        let mut new_start = start;
        let mut new_end = end;
        // The interval starting at or before `start` may overlap or touch it.
        if let Some((&prev_start, &prev_end)) = self.ranges.range(..=start).next_back()
            && prev_end.saturating_add(1) >= start
        {
            new_start = prev_start;
            new_end = new_end.max(prev_end);
        }
        let absorbed: Vec<u32> = self
            .ranges
            .range(new_start..)
            .take_while(|(s, _)| **s <= new_end.saturating_add(1))
            .map(|(s, _)| *s)
            .collect();
        for key in absorbed {
            if let Some(end) = self.ranges.remove(&key) {
                new_end = new_end.max(end);
            }
        }
        self.ranges.insert(new_start, new_end);
        true
    }

    /// Remove `[start, end]` from the set. Returns whether the set changed.
    pub fn remove(&mut self, start: u32, end: u32) -> bool {
        if start > end {
            return false;
        }
        let overlapping: Vec<(u32, u32)> = self
            .ranges
            .range(..=end)
            .rev()
            .take_while(|(_, e)| **e >= start)
            .map(|(s, e)| (*s, *e))
            .collect();
        if overlapping.is_empty() {
            return false;
        }
        for (s, e) in overlapping {
            self.ranges.remove(&s);
            if s < start {
                self.ranges.insert(s, start - 1);
            }
            if e > end {
                self.ranges.insert(end + 1, e);
            }
        }
        true
    }

    /// Keep only lines inside `[start, end]`. Returns whether the set changed.
    pub fn retain_within(&mut self, start: u32, end: u32) -> bool {
        let mut changed = false;
        if start > 1 {
            changed |= self.remove(1, start - 1);
        }
        if end < u32::MAX {
            changed |= self.remove(end + 1, u32::MAX);
        }
        changed
    }

    /// Whether `line` is in the set.
    #[must_use]
    pub fn contains(&self, line: u32) -> bool {
        self.ranges
            .range(..=line)
            .next_back()
            .is_some_and(|(_, end)| *end >= line)
    }

    /// Whether every line of `[start, end]` is in the set.
    #[must_use]
    pub fn contains_range(&self, start: u32, end: u32) -> bool {
        start <= end
            && self
                .ranges
                .range(..=start)
                .next_back()
                .is_some_and(|(_, e)| *e >= end)
    }

    /// Iterate intervals in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.ranges.iter().map(|(s, e)| (*s, *e))
    }

    /// The intervals of the set intersected with `[start, end]`, in ascending order.
    #[must_use]
    pub fn intersect(&self, start: u32, end: u32) -> Vec<(u32, u32)> {
        if start > end {
            return Vec::new();
        }
        let first = self
            .ranges
            .range(..=start)
            .next_back()
            .map_or(start, |(s, _)| *s);
        self.ranges
            .range(first..=end)
            .filter(|(_, e)| **e >= start)
            .map(|(s, e)| ((*s).max(start), (*e).min(end)))
            .collect()
    }

    /// The parts of `[start, end]` that are not in the set, in ascending order.
    #[must_use]
    pub fn complement_within(&self, start: u32, end: u32) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        let mut cursor = start;
        for (s, e) in self.intersect(start, end) {
            if s > cursor {
                out.push((cursor, s - 1));
            }
            if e == u32::MAX {
                return out;
            }
            cursor = e + 1;
        }
        if cursor <= end {
            out.push((cursor, end));
        }
        out
    }

    /// Total lines in the set (saturating).
    #[must_use]
    pub fn len(&self) -> u64 {
        self.iter()
            .map(|(s, e)| u64::from(e - s) + 1)
            .fold(0_u64, u64::saturating_add)
    }

    /// Convert to persisted intervals.
    #[must_use]
    pub fn to_intervals(&self) -> Vec<ReviewLineInterval> {
        self.iter()
            .map(|(start, end)| ReviewLineInterval { start, end })
            .collect()
    }

    /// Build from persisted intervals (invalid ones are skipped).
    #[must_use]
    pub fn from_intervals(intervals: &[ReviewLineInterval]) -> Self {
        let mut set = Self::new();
        for interval in intervals {
            set.insert(interval.start.max(1), interval.end);
        }
        set
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(ranges: &[(u32, u32)]) -> IntervalSet {
        let mut set = IntervalSet::new();
        for (s, e) in ranges {
            set.insert(*s, *e);
        }
        set
    }

    #[test]
    fn insert_merges_overlapping_and_adjacent() {
        let s = set(&[(10, 20), (22, 30), (21, 21)]);
        assert_eq!(s.iter().collect::<Vec<_>>(), vec![(10, 30)]);
        let s = set(&[(1, 2), (5, 6), (3, 4)]);
        assert_eq!(s.iter().collect::<Vec<_>>(), vec![(1, 6)]);
        let s = set(&[(1, 2), (10, 12), (4, 20)]);
        assert_eq!(s.iter().collect::<Vec<_>>(), vec![(1, 2), (4, 20)]);
        assert!(!set(&[(1, 10)]).insert(3, 4));
    }

    #[test]
    fn remove_splits_intervals() {
        let mut s = set(&[(1, 10)]);
        assert!(s.remove(4, 6));
        assert_eq!(s.iter().collect::<Vec<_>>(), vec![(1, 3), (7, 10)]);
        assert!(!s.remove(4, 6));
        assert!(s.remove(1, 100));
        assert!(s.is_empty());
    }

    #[test]
    fn complement_and_intersect() {
        let s = set(&[(3, 4), (8, 9)]);
        assert_eq!(s.complement_within(1, 10), vec![(1, 2), (5, 7), (10, 10)]);
        assert_eq!(s.intersect(4, 8), vec![(4, 4), (8, 8)]);
        let full = set(&[(1, u32::MAX)]);
        assert_eq!(full.complement_within(5, u32::MAX), Vec::new());
        assert!(full.contains_range(7, u32::MAX));
    }

    #[test]
    fn representation_is_order_independent() {
        let ranges = [(5, 9), (1, 3), (10, 12), (40, 41), (2, 6), (30, 39)];
        let forward = set(&ranges);
        let mut reversed = ranges;
        reversed.reverse();
        assert_eq!(forward, set(&reversed));
        assert_eq!(forward.iter().collect::<Vec<_>>(), vec![(1, 12), (30, 41)]);
    }
}
