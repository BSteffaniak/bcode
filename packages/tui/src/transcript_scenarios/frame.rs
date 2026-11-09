//! Observations taken from one committed frame.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::ops::RangeInclusive;

use super::session::Marker;

/// Terminal cell rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellRect {
    /// Left column.
    pub x: u16,
    /// Top row.
    pub y: u16,
    /// Width in cells.
    pub width: u16,
    /// Height in cells.
    pub height: u16,
}

impl CellRect {
    /// Center cell of the rectangle.
    #[must_use]
    pub const fn center(self) -> super::driver::CellPoint {
        super::driver::CellPoint {
            x: self.x.saturating_add(self.width / 2),
            y: self.y.saturating_add(self.height / 2),
        }
    }
}

/// Which surface owns keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusObservation {
    /// The composer text input.
    Composer,
    /// The transcript; `units` is true in unit focus mode (`alt+u`).
    Transcript {
        /// Whether unit focus mode is active.
        units: bool,
    },
}

/// A marker visible in a frame, with its row relative to the transcript area top.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkerAt {
    /// The marker.
    pub marker: Marker,
    /// Row within the transcript area (0 = top row of the transcript viewport).
    pub row: u16,
}

/// One item visible in a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleItem {
    /// Canonical sequence identifying the item.
    pub sequence: u64,
    /// Transcript-area rows on which the item's markers appear.
    pub rows: RangeInclusive<u16>,
    /// Item source lines visible in the frame.
    pub lines: RangeInclusive<u32>,
}

/// What one committed frame shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScenarioFrame {
    /// Committed frame index within the scenario (assigned by the runner).
    pub index: usize,
    /// Every terminal row's symbols.
    pub rows: Vec<String>,
    /// Transcript viewport area.
    pub transcript: CellRect,
    /// Area of the latest-content bar, when shown.
    pub latest_bar: Option<CellRect>,
    /// Whether the transcript is following the latest content bottom.
    pub following_bottom: bool,
    /// Whether a scroll motion (animation toward a target) is still running.
    pub motion_running: bool,
    /// Keyboard focus owner.
    pub focus: FocusObservation,
    /// Focused transcript unit, rendered as an engine-neutral label, in unit focus mode.
    pub focused_target: Option<String>,
    /// Terminal area of the transcript scrollbar track, when one is presented (X6).
    pub scrollbar: Option<CellRect>,
}

impl ScenarioFrame {
    /// Rows of the transcript area.
    #[must_use]
    pub fn transcript_rows(&self) -> &[String] {
        let start = usize::from(self.transcript.y).min(self.rows.len());
        let end = start
            .saturating_add(usize::from(self.transcript.height))
            .min(self.rows.len());
        &self.rows[start..end]
    }

    /// Markers visible in the transcript area, top to bottom.
    #[must_use]
    pub fn markers(&self) -> Vec<MarkerAt> {
        let mut markers = Vec::new();
        for (row, text) in self.transcript_rows().iter().enumerate() {
            let row = u16::try_from(row).unwrap_or(u16::MAX);
            markers.extend(
                text.split_whitespace()
                    .filter_map(Marker::parse)
                    .map(|marker| MarkerAt { marker, row }),
            );
        }
        markers
    }

    /// Items visible in the transcript area, in display order.
    #[must_use]
    pub fn visible_items(&self) -> Vec<VisibleItem> {
        let mut items: Vec<VisibleItem> = Vec::new();
        for MarkerAt { marker, row } in self.markers() {
            match items.last_mut() {
                Some(item) if item.sequence == marker.sequence => {
                    item.rows = *item.rows.start()..=row;
                    item.lines = *item.lines.start()..=marker.line;
                }
                _ => items.push(VisibleItem {
                    sequence: marker.sequence,
                    rows: row..=row,
                    lines: marker.line..=marker.line,
                }),
            }
        }
        items
    }

    /// Top-most visible marker: the reading position's item and source line.
    #[must_use]
    pub fn top_marker(&self) -> Option<MarkerAt> {
        self.markers().into_iter().next()
    }

    /// Bottom-most visible marker.
    #[must_use]
    pub fn bottom_marker(&self) -> Option<MarkerAt> {
        self.markers().into_iter().last()
    }

    /// Transcript-area row of `marker`, when visible.
    #[must_use]
    pub fn row_of(&self, marker: Marker) -> Option<u16> {
        self.markers()
            .into_iter()
            .find(|candidate| candidate.marker == marker)
            .map(|candidate| candidate.row)
    }

    /// Whether `marker` is visible.
    #[must_use]
    pub fn shows(&self, marker: Marker) -> bool {
        self.row_of(marker).is_some()
    }

    /// Whether any row of the whole frame contains `needle`.
    #[must_use]
    pub fn contains(&self, needle: &str) -> bool {
        self.rows.iter().any(|row| row.contains(needle))
    }

    /// Row movement of content between `previous` and this frame, per common marker.
    ///
    /// Positive values mean content moved down (the view scrolled toward older content).
    #[must_use]
    pub fn shifts_from(&self, previous: &Self) -> BTreeMap<Marker, i32> {
        let before = previous
            .markers()
            .into_iter()
            .map(|at| (at.marker, at.row))
            .collect::<BTreeMap<_, _>>();
        self.markers()
            .into_iter()
            .filter_map(|at| {
                before
                    .get(&at.marker)
                    .map(|row| (at.marker, i32::from(at.row) - i32::from(*row)))
            })
            .collect()
    }

    /// Movement of the top-most marker of `previous` that is still visible.
    #[must_use]
    pub fn shift_from(&self, previous: &Self) -> Option<i32> {
        let shifts = self.shifts_from(previous);
        previous
            .markers()
            .into_iter()
            .find_map(|at| shifts.get(&at.marker).copied())
    }

    /// Rows of the scrollbar track (relative to the track top) that paint the thumb symbol
    /// `thumb`; empty when no scrollbar is presented.
    #[must_use]
    pub fn scrollbar_thumb_rows(&self, thumb: &str) -> Vec<u16> {
        let Some(track) = self.scrollbar else {
            return Vec::new();
        };
        (0..track.height)
            .filter(|offset| {
                self.rows
                    .get(usize::from(track.y.saturating_add(*offset)))
                    .and_then(|row| row.chars().nth(usize::from(track.x)))
                    .is_some_and(|cell| thumb.chars().eq(std::iter::once(cell)))
            })
            .collect()
    }

    /// Whole frame as text with a transcript-area gutter, for failure messages.
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = String::new();
        let top = usize::from(self.transcript.y);
        let bottom = top.saturating_add(usize::from(self.transcript.height));
        let _ = writeln!(
            text,
            "frame {} following={} motion={} latest_bar={} focus={:?} target={:?}",
            self.index,
            self.following_bottom,
            self.motion_running,
            self.latest_bar.is_some(),
            self.focus,
            self.focused_target
        );
        for (index, row) in self.rows.iter().enumerate() {
            let gutter = if (top..bottom).contains(&index) {
                '|'
            } else {
                ' '
            };
            let _ = writeln!(text, "{index:>3}{gutter}{}", row.trim_end());
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::{CellRect, FocusObservation, ScenarioFrame, VisibleItem};
    use crate::transcript_scenarios::session::Marker;

    fn frame(rows: &[&str]) -> ScenarioFrame {
        ScenarioFrame {
            index: 0,
            rows: rows.iter().map(|row| (*row).to_owned()).collect(),
            transcript: CellRect {
                x: 0,
                y: 1,
                width: 40,
                height: u16::try_from(rows.len() - 2).unwrap_or(0),
            },
            latest_bar: None,
            following_bottom: false,
            motion_running: false,
            focus: FocusObservation::Composer,
            focused_target: None,
            scrollbar: None,
        }
    }

    #[test]
    fn observations_read_only_the_transcript_area() {
        let before = frame(&[
            "header S00001L000",
            "Bcode",
            "  S00007L000 item 7 line 0",
            "  S00007L001 item 7 line 1",
            "  S00008L000 item 8 line 0",
            "status S00009L000",
        ]);
        assert_eq!(
            before.visible_items(),
            vec![
                VisibleItem {
                    sequence: 7,
                    rows: 1..=2,
                    lines: 0..=1,
                },
                VisibleItem {
                    sequence: 8,
                    rows: 3..=3,
                    lines: 0..=0,
                },
            ]
        );
        assert_eq!(before.top_marker().map(|at| at.row), Some(1));
        assert!(!before.shows(Marker::new(1, 0)));
        assert!(before.contains("status"));
    }

    #[test]
    fn shift_follows_the_top_most_surviving_marker() {
        let before = frame(&["", "  S00007L000", "  S00007L001", "  S00008L000", "", ""]);
        let after = frame(&["", "  S00006L000", "", "  S00007L001", "  S00008L000", ""]);
        assert_eq!(after.shift_from(&before), Some(1));
        assert_eq!(after.shifts_from(&before).len(), 2);
    }
}
