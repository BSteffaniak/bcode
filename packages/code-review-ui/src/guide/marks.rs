//! The guide's marks on diff rows.
//!
//! [`GuideMarks`] indexes the shown version's citation ranges by path and side. A diff row asks
//! for the marks of the line it shows: a badge (`[3]` with the citation's verification glyph) on
//! the first row of each cited range the document shows, and whether the line is cited by the
//! current section (its gutter is tinted). Which citations get badges follows
//! `guide.markers` (`current`, `all`, `off`); the current section's lines are tinted whatever
//! it says, so stepping through the guide always shows where it is.

use super::state::{ActiveGuide, CitationLook};
use bcode_code_review::document::FileKey;
use bcode_code_review::guide::config::Markers;
use bcode_code_review_models::ReviewLineSide;
use std::collections::BTreeMap;

/// The glyph next to a badge's number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BadgeGlyph {
    /// The quoted text checked out.
    Verified,
    /// Not checked.
    Unverified,
    /// A problem, or the code changed since.
    Problem,
}

impl From<CitationLook> for BadgeGlyph {
    fn from(look: CitationLook) -> Self {
        match look {
            CitationLook::Verified => Self::Verified,
            CitationLook::Unverified => Self::Unverified,
            CitationLook::Problem => Self::Problem,
        }
    }
}

/// A citation badge on a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MarkBadge {
    /// The citation's number.
    pub number: u16,
    /// Its glyph.
    pub glyph: BadgeGlyph,
}

/// The marks of one side of one diff row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct RowMarks {
    /// The badge on this row, when a cited range starts showing here.
    pub badge: Option<MarkBadge>,
    /// Whether the current section cites this line.
    pub focus: bool,
}

impl RowMarks {
    /// A stamp of these marks for row caches.
    #[must_use]
    pub fn stamp(self) -> u64 {
        let badge = self.badge.map_or(0, |badge| {
            (u64::from(badge.number) << 2)
                | match badge.glyph {
                    BadgeGlyph::Verified => 1,
                    BadgeGlyph::Unverified => 2,
                    BadgeGlyph::Problem => 3,
                }
        });
        (badge << 1) | u64::from(self.focus)
    }
}

/// The index of `side` in a file's marks.
const fn side_index(side: ReviewLineSide) -> usize {
    match side {
        ReviewLineSide::Old => 0,
        ReviewLineSide::New => 1,
    }
}

/// One cited range.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mark {
    /// The source cited; `None` matches the path in any source.
    source: Option<String>,
    start: u32,
    end: u32,
    badge: Option<MarkBadge>,
    focus: bool,
}

/// The shown version's citation ranges, by path and side.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GuideMarks {
    by_file: BTreeMap<String, [Vec<Mark>; 2]>,
    revision: u64,
}

impl GuideMarks {
    /// The marks of `active` as `markers` shows them; none unless the reviewer is `engaged`
    /// with the guide. `revision` identifies them for row caches.
    #[must_use]
    pub fn build(active: &ActiveGuide, markers: Markers, engaged: bool, revision: u64) -> Self {
        let mut by_file: BTreeMap<String, [Vec<Mark>; 2]> = BTreeMap::new();
        if engaged {
            let current: &[String] = active
                .current()
                .map_or(&[], |entry| entry.citations.as_slice());
            for (id, citation) in &active.version.citations {
                let focus = current.contains(id);
                let badged = match markers {
                    Markers::All => true,
                    Markers::Current => focus,
                    Markers::Off => false,
                };
                if !badged && !focus {
                    continue;
                }
                let Some(target) = active.target(id) else {
                    continue;
                };
                let badge = badged
                    .then(|| {
                        active.number(id).map(|number| MarkBadge {
                            number,
                            glyph: active.citation_look(id).into(),
                        })
                    })
                    .flatten();
                by_file.entry(citation.path.clone()).or_default()[side_index(citation.side)].push(
                    Mark {
                        source: citation.source_id.clone(),
                        start: target.lines.start,
                        end: target.lines.end.max(target.lines.start),
                        badge,
                        focus,
                    },
                );
            }
        }
        for marks in by_file.values_mut().flatten() {
            marks.sort_by_key(|mark| (mark.start, mark.badge.map_or(u16::MAX, |b| b.number)));
        }
        Self { by_file, revision }
    }

    /// Whether there are no marks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_file.is_empty()
    }

    /// Identifies these marks.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// The marks of `line` on `side` of `file`. `previous` is the line the document item just
    /// before shows on the same side of the same file, when it is one: a range's badge goes on
    /// the first row of each run of it the document shows (a hidden gap splits a range).
    #[must_use]
    pub fn at(
        &self,
        file: &FileKey,
        side: ReviewLineSide,
        line: u32,
        previous: Option<u32>,
    ) -> RowMarks {
        let Some(marks) = self
            .by_file
            .get(file.path.as_str())
            .map(|sides| &sides[side_index(side)])
        else {
            return RowMarks::default();
        };
        let mut row = RowMarks::default();
        for mark in marks {
            if mark.start > line {
                break;
            }
            if line > mark.end
                || mark
                    .source
                    .as_deref()
                    .is_some_and(|source| source != file.source_id)
            {
                continue;
            }
            row.focus |= mark.focus;
            let first = previous.is_none_or(|previous| previous < mark.start || previous >= line);
            if first && row.badge.is_none() {
                row.badge = mark.badge;
            }
        }
        row
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marks(entries: Vec<(&str, u32, u32, Option<u16>, bool)>) -> GuideMarks {
        let mut by_file: BTreeMap<String, [Vec<Mark>; 2]> = BTreeMap::new();
        for (source, start, end, badge, focus) in entries {
            by_file.entry("a.rs".to_owned()).or_default()[1].push(Mark {
                source: Some(source.to_owned()),
                start,
                end,
                badge: badge.map(|number| MarkBadge {
                    number,
                    glyph: BadgeGlyph::Verified,
                }),
                focus,
            });
        }
        GuideMarks {
            by_file,
            revision: 1,
        }
    }

    #[test]
    fn badges_go_on_the_first_shown_row_of_each_run_of_a_range() {
        let marks = marks(vec![
            ("s", 10, 14, Some(1), true),
            ("s", 30, 30, Some(2), false),
        ]);
        let file = FileKey::new("s", None, "a.rs");
        let at = |line, previous| marks.at(&file, ReviewLineSide::New, line, previous);
        assert_eq!(at(10, Some(9)).badge.map(|badge| badge.number), Some(1));
        assert!(at(11, Some(10)).badge.is_none());
        assert!(at(11, Some(10)).focus);
        // A gap hid lines 11 and 12: the run after it gets its own badge.
        assert_eq!(at(13, None).badge.map(|badge| badge.number), Some(1));
        assert_eq!(at(15, Some(14)), RowMarks::default());
        assert_eq!(at(30, Some(29)).badge.map(|badge| badge.number), Some(2));
        assert!(!at(30, Some(29)).focus);
        let other = FileKey::new("other", None, "a.rs");
        assert_eq!(
            marks.at(&other, ReviewLineSide::New, 10, None),
            RowMarks::default()
        );
        assert_eq!(
            marks.at(&file, ReviewLineSide::Old, 10, None),
            RowMarks::default()
        );
    }
}
