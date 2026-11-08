//! Keyboard focus over transcript disclosure targets.
//!
//! Focus mode walks the disclosure anchors the transcript composer records for
//! each unit header, group header, and collapsible section label, in
//! transcript order. Anchors live in the same retained layout rows that are
//! painted, so the focus highlight, hit regions, and traversal agree on
//! geometry without measuring text.

use bcode_presentation_policy::models::Disclosure;
use bmux_tui::geometry::Rect;

use crate::structure::DisclosureTarget;
use crate::transcript_layout::{TranscriptLayoutCache, VisibleTranscriptSource};

/// Keymap scope active while focus mode owns the keyboard.
pub const FOCUS_SCOPE: &str = "transcript_focus";
/// Chat action that enters (or leaves) focus mode.
pub const FOCUS_UNITS_ACTION: &str = "tui.transcript.focusUnits";
/// Default binding of [`FOCUS_UNITS_ACTION`].
pub const FOCUS_UNITS_DEFAULT_KEY: &str = "alt+u";
/// Action that cycles disclosure collapsed → summary → expanded: the focused
/// target in focus mode, every unit and group otherwise.
pub const CYCLE_DETAIL_ACTION: &str = "tui.transcript.cycleDetail";
/// Chat-scope default binding of [`CYCLE_DETAIL_ACTION`] (outside focus mode).
pub const CYCLE_DETAIL_DEFAULT_KEY: &str = "alt+d";
/// Action that makes the focused tool unit's disclosure the default for its
/// tool.
pub const MAKE_DEFAULT_ACTION: &str = "tui.transcript.makeDefault";
/// Action that toggles the structure inspector for the focused target.
pub const INSPECT_ACTION: &str = "tui.transcript.inspect";
/// Maximum transcript entries visited by one traversal step.
const MAX_SCAN_ENTRIES: usize = 4096;
/// Maximum anchors of one entry inspected when enumerating its targets.
const MAX_SCAN_ROWS_PER_ENTRY: usize = 16_384;

/// Action available in the [`FOCUS_SCOPE`] keymap scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FocusAction {
    /// Focus the next target in transcript order.
    Next,
    /// Focus the previous target in transcript order.
    Previous,
    /// Toggle the focused target between expanded and collapsed.
    Toggle,
    /// Expand the focused target one level.
    Expand,
    /// Collapse the focused target one level, or focus its parent.
    Collapse,
    /// Expand everything under the focused target.
    ExpandAll,
    /// Cycle the focused target collapsed → summary → expanded.
    CycleDetail,
    /// Toggle the structure inspector for the focused target.
    Inspect,
    /// Scroll the open inspector down.
    InspectorScrollDown,
    /// Scroll the open inspector up.
    InspectorScrollUp,
    /// Make the focused tool unit's disclosure the default for its tool.
    MakeDefault,
    /// Leave focus mode (closing an open inspector first).
    Exit,
}

impl FocusAction {
    /// Every focus action, in help order.
    pub const ALL: [Self; 12] = [
        Self::Next,
        Self::Previous,
        Self::Toggle,
        Self::Expand,
        Self::Collapse,
        Self::ExpandAll,
        Self::CycleDetail,
        Self::Inspect,
        Self::InspectorScrollDown,
        Self::InspectorScrollUp,
        Self::MakeDefault,
        Self::Exit,
    ];

    /// Stable registry identity.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Next => "tui.transcriptFocus.next",
            Self::Previous => "tui.transcriptFocus.previous",
            Self::Toggle => "tui.transcriptFocus.toggle",
            Self::Expand => "tui.transcriptFocus.expand",
            Self::Collapse => "tui.transcriptFocus.collapse",
            Self::ExpandAll => "tui.transcriptFocus.expandAll",
            Self::CycleDetail => CYCLE_DETAIL_ACTION,
            Self::Inspect => INSPECT_ACTION,
            Self::InspectorScrollDown => "tui.transcriptFocus.inspectorScrollDown",
            Self::InspectorScrollUp => "tui.transcriptFocus.inspectorScrollUp",
            Self::MakeDefault => MAKE_DEFAULT_ACTION,
            Self::Exit => "tui.transcriptFocus.exit",
        }
    }

    /// User-facing description, independent of the binding.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Next => "Focus next disclosure target",
            Self::Previous => "Focus previous disclosure target",
            Self::Toggle => "Toggle focused target",
            Self::Expand => "Expand focused target one level",
            Self::Collapse => "Collapse focused target one level or focus its parent",
            Self::ExpandAll => "Expand everything under the focused target",
            Self::CycleDetail => "Cycle focused target collapsed, summary, expanded",
            Self::Inspect => "Inspect how the focused target's presentation resolved",
            Self::InspectorScrollDown => "Scroll the inspector down",
            Self::InspectorScrollUp => "Scroll the inspector up",
            Self::MakeDefault => "Make the focused tool's disclosure its default",
            Self::Exit => "Close the inspector, or leave transcript focus",
        }
    }

    /// Default bindings.
    #[must_use]
    pub const fn default_keys(self) -> &'static [&'static str] {
        match self {
            Self::Next => &["j", "down"],
            Self::Previous => &["k", "up"],
            Self::Toggle => &["enter", "space"],
            Self::Expand => &["l", "right"],
            Self::Collapse => &["h", "left"],
            Self::ExpandAll => &["e"],
            Self::CycleDetail => &["d"],
            Self::Inspect => &["i"],
            Self::InspectorScrollDown => &["shift+down", "ctrl+e"],
            Self::InspectorScrollUp => &["shift+up", "ctrl+y"],
            Self::MakeDefault => &["m"],
            Self::Exit => &["escape"],
        }
    }

    /// Parse a registry identity.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.id() == id)
    }
}

/// Focus-mode state: whether it is active and which target holds focus.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TranscriptFocus {
    active: bool,
    focused: Option<DisclosureTarget>,
    /// Resident index the focused target's item had when last located; a
    /// hint verified by item id before use (see [`Self::entry_hint`]).
    entry_hint: Option<usize>,
}

impl TranscriptFocus {
    /// Whether focus mode owns the keyboard.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.active
    }

    /// Focused target while focus mode is active.
    #[must_use]
    pub const fn focused(&self) -> Option<&DisclosureTarget> {
        if self.active {
            self.focused.as_ref()
        } else {
            None
        }
    }

    /// Enter focus mode on `target`.
    pub fn enter(&mut self, target: DisclosureTarget) {
        self.active = true;
        self.focused = Some(target);
    }

    /// Move focus to `target`; returns whether it changed.
    pub fn focus(&mut self, target: DisclosureTarget) -> bool {
        if self.focused.as_ref() == Some(&target) {
            return false;
        }
        self.focused = Some(target);
        true
    }

    /// Leave focus mode; returns whether it was active.
    pub fn exit(&mut self) -> bool {
        let was_active = self.active;
        self.active = false;
        self.focused = None;
        self.entry_hint = None;
        was_active
    }

    /// Resident index last recorded for the focused target's item.
    ///
    /// Only a hint: callers verify the item at that index by id, so a stale
    /// hint costs a fallback lookup, never a wrong answer.
    #[must_use]
    pub const fn entry_hint(&self) -> Option<usize> {
        self.entry_hint
    }

    /// Record the resident index of the focused target's item.
    pub const fn set_entry_hint(&mut self, index: Option<usize>) {
        self.entry_hint = index;
    }

    /// Forget the focused target when its item is no longer resident.
    ///
    /// Focus mode stays active; the next movement refocuses from the viewport.
    pub fn retain(&mut self, resident: impl Fn(u64) -> bool) {
        if self
            .focused
            .as_ref()
            .is_some_and(|target| !resident(target.item_id()))
        {
            self.focused = None;
        }
    }
}

/// Read-only view of the disclosure anchors in resident transcript entries.
pub trait DisclosureAnchors {
    /// Number of transcript entries.
    fn entry_count(&self) -> usize;

    /// Global row where `entry` starts.
    fn entry_start_row(&self, entry: usize) -> Option<usize>;

    /// Last transcript entry containing a row at or before global `row`.
    fn entry_at_or_before_row(&self, row: usize) -> Option<usize>;

    /// Disclosure targets of `entry` with their row inside it, in row order.
    fn entry_targets(&self, entry: usize) -> Vec<(usize, DisclosureTarget)>;

    /// Transcript entry holding item `item_id`.
    fn entry_of_item(&self, item_id: u64) -> Option<usize>;

    /// Row of `target` inside `entry`, when its anchor is present.
    fn target_row_in_entry(&self, entry: usize, target: &DisclosureTarget) -> Option<usize>;
}

/// Global row of `target`'s anchor, when it is laid out.
#[must_use]
pub fn target_row(anchors: &impl DisclosureAnchors, target: &DisclosureTarget) -> Option<usize> {
    let entry = anchors.entry_of_item(target.item_id())?;
    let start = anchors.entry_start_row(entry)?;
    anchors
        .target_row_in_entry(entry, target)
        .map(|row| start.saturating_add(row))
}

/// Last target whose anchor row is in the viewport, else the nearest one
/// above it.
#[must_use]
pub fn initial_target(
    anchors: &impl DisclosureAnchors,
    top_row: usize,
    height: u16,
) -> Option<DisclosureTarget> {
    let bottom = top_row.saturating_add(usize::from(height));
    let last = anchors.entry_at_or_before_row(bottom.checked_sub(1)?)?;
    (0..=last).rev().take(MAX_SCAN_ENTRIES).find_map(|entry| {
        let start = anchors.entry_start_row(entry)?;
        anchors
            .entry_targets(entry)
            .into_iter()
            .rev()
            .find(|(row, _)| start.saturating_add(*row) < bottom)
            .map(|(_, target)| target)
    })
}

/// Target after (`forward`) or before `current` in transcript order.
///
/// Returns `None` when `current`'s item is not resident or no target lies in
/// that direction. A focused target that is no longer laid out (for example a
/// section of a unit that was just collapsed) steps to the first target of its
/// own entry.
#[must_use]
pub fn step(
    anchors: &impl DisclosureAnchors,
    current: &DisclosureTarget,
    forward: bool,
) -> Option<DisclosureTarget> {
    let entry = anchors.entry_of_item(current.item_id())?;
    let targets = anchors.entry_targets(entry);
    match targets.iter().position(|(_, target)| target == current) {
        Some(position) => {
            let neighbor = if forward {
                targets.get(position.saturating_add(1))
            } else {
                position
                    .checked_sub(1)
                    .and_then(|position| targets.get(position))
            };
            if let Some((_, target)) = neighbor {
                return Some(target.clone());
            }
        }
        None => {
            if let Some((_, target)) = targets.into_iter().next() {
                return Some(target);
            }
        }
    }
    if forward {
        (entry.saturating_add(1)..anchors.entry_count())
            .take(MAX_SCAN_ENTRIES)
            .find_map(|entry| anchors.entry_targets(entry).into_iter().next())
    } else {
        (0..entry)
            .rev()
            .take(MAX_SCAN_ENTRIES)
            .find_map(|entry| anchors.entry_targets(entry).into_iter().next_back())
    }
    .map(|(_, target)| target)
}

/// Disclosure one level more open than `current`, or `None` when already
/// fully expanded.
///
/// Groups open collapsed → summary → expanded; units and sections expand
/// directly.
#[must_use]
pub const fn expand_level(target: &DisclosureTarget, current: Disclosure) -> Option<Disclosure> {
    match (target, current) {
        (_, Disclosure::Expanded) => None,
        (DisclosureTarget::Group(_), Disclosure::Collapsed | Disclosure::Hidden) => {
            Some(Disclosure::Summary)
        }
        _ => Some(Disclosure::Expanded),
    }
}

/// Disclosure one level more closed than `current`, or `None` when already
/// collapsed (focus then moves to the parent).
///
/// Groups close expanded → summary → collapsed; units and sections collapse
/// directly.
#[must_use]
pub const fn collapse_level(target: &DisclosureTarget, current: Disclosure) -> Option<Disclosure> {
    match (target, current) {
        (_, Disclosure::Collapsed | Disclosure::Hidden) => None,
        (DisclosureTarget::Group(_), Disclosure::Expanded) => Some(Disclosure::Summary),
        _ => Some(Disclosure::Collapsed),
    }
}

/// Next disclosure in the detail cycle collapsed → summary → expanded →
/// collapsed. Hidden targets enter the cycle at summary, so cycling always
/// reveals something first.
#[must_use]
pub const fn next_detail(current: Disclosure) -> Disclosure {
    match current {
        Disclosure::Collapsed | Disclosure::Hidden => Disclosure::Summary,
        Disclosure::Summary => Disclosure::Expanded,
        Disclosure::Expanded => Disclosure::Collapsed,
    }
}

/// Parent of `target`: a section's unit, or a unit's group as reported by
/// `group_of` (first member item id). Groups have no parent.
#[must_use]
pub fn parent(
    target: &DisclosureTarget,
    group_of: impl FnOnce(u64) -> Option<u64>,
) -> Option<DisclosureTarget> {
    match target {
        DisclosureTarget::Section(id, _) => Some(DisclosureTarget::Unit(*id)),
        DisclosureTarget::Unit(id) => group_of(*id).map(DisclosureTarget::Group),
        DisclosureTarget::Group(_) => None,
    }
}

/// New viewport top row that brings global `row` into a viewport of `height`
/// rows starting at `top_row`, or `None` when it is already visible.
#[must_use]
pub fn reveal_top_row(row: usize, top_row: usize, height: u16) -> Option<usize> {
    let height = usize::from(height).max(1);
    if row < top_row {
        Some(row)
    } else if row >= top_row.saturating_add(height) {
        Some(row.saturating_add(1).saturating_sub(height))
    } else {
        None
    }
}

/// Terminal rectangle of global `row` inside transcript `area` scrolled to
/// `top_row`, or `None` when the row is not visible.
#[must_use]
pub fn row_rect(area: Rect, top_row: usize, row: usize) -> Option<Rect> {
    let offset = u16::try_from(row.checked_sub(top_row)?).ok()?;
    if offset >= area.height || area.width == 0 {
        return None;
    }
    let rect = Rect::new(area.x, area.y.saturating_add(offset), area.width, 1).intersection(area);
    (!rect.is_empty()).then_some(rect)
}

/// [`DisclosureAnchors`] over the retained transcript layout.
pub struct LayoutAnchors<'a, F> {
    layout: &'a TranscriptLayoutCache,
    entries: usize,
    index_of: F,
}

impl<'a, F: Fn(u64) -> Option<usize>> LayoutAnchors<'a, F> {
    /// View `layout`, whose `entries` transcript entries hold the items that
    /// `index_of` locates by id.
    #[must_use]
    pub const fn new(layout: &'a TranscriptLayoutCache, entries: usize, index_of: F) -> Self {
        Self {
            layout,
            entries,
            index_of,
        }
    }
}

impl<F: Fn(u64) -> Option<usize>> DisclosureAnchors for LayoutAnchors<'_, F> {
    fn entry_count(&self) -> usize {
        self.entries
    }

    fn entry_start_row(&self, entry: usize) -> Option<usize> {
        self.layout
            .entry_start_row(VisibleTranscriptSource::Transcript, entry)
    }

    fn entry_at_or_before_row(&self, row: usize) -> Option<usize> {
        let last = self.entries.checked_sub(1)?;
        self.layout.line_at_row(row).map_or_else(
            || (row >= self.layout.total_rows()).then_some(last),
            |line| match line.source {
                VisibleTranscriptSource::Transcript => Some(line.entry_index.min(last)),
                VisibleTranscriptSource::Pending => Some(last),
                VisibleTranscriptSource::HistoryBanner => None,
            },
        )
    }

    fn entry_targets(&self, entry: usize) -> Vec<(usize, DisclosureTarget)> {
        let Some(rows) = self
            .layout
            .entry_row_count(VisibleTranscriptSource::Transcript, entry)
        else {
            return Vec::new();
        };
        // Disclosure anchors are read directly, so another anchor recorded
        // on the same row (for example a plugin visual's own first-row
        // anchor) cannot hide a target.
        let mut targets: Vec<(usize, DisclosureTarget)> = self
            .layout
            .content_anchors(entry)
            .take(MAX_SCAN_ROWS_PER_ENTRY)
            .filter(|(row, _)| *row < rows)
            .filter_map(|(row, key)| DisclosureTarget::from_hit_id(key).map(|target| (row, target)))
            .collect();
        // Stable: targets sharing a row keep their recorded order.
        targets.sort_by_key(|(row, _)| *row);
        targets
    }

    fn entry_of_item(&self, item_id: u64) -> Option<usize> {
        (self.index_of)(item_id).filter(|entry| *entry < self.entries)
    }

    fn target_row_in_entry(&self, entry: usize, target: &DisclosureTarget) -> Option<usize> {
        self.layout.content_anchor_row(entry, &target.to_hit_id())
    }
}

#[cfg(test)]
pub mod tests {
    use bcode_plugin_sdk::tui_visual::TuiVisualAnchor;
    use bmux_tui::prelude::Line;

    use super::*;
    use crate::transcript_layout::{
        TranscriptLayoutFingerprint, TranscriptLayoutRows, TranscriptLayoutSignature,
        TranscriptLayoutSpec,
    };

    /// Build a layout whose entries have `rows[i]` rows and the given
    /// `(row, anchor key)` pairs.
    pub fn layout(entries: &[(usize, Vec<(usize, String)>)]) -> TranscriptLayoutCache {
        let mut layout = TranscriptLayoutCache::default();
        let entries = entries.to_vec();
        layout.sync(TranscriptLayoutSpec {
            width: 40,
            fingerprint: TranscriptLayoutFingerprint::new("focus".to_owned()),
            structural_fingerprint: TranscriptLayoutFingerprint::new("focus".to_owned()),
            transcript_len: entries.len(),
            pending_len: 0,
            transcript_signature: |index: usize| TranscriptLayoutSignature::new(format!("{index}")),
            transcript_rows: |index: usize| {
                let (rows, anchors) = &entries[index];
                TranscriptLayoutRows::Anchored {
                    rows: (0..*rows).map(|_| Line::raw("row")).collect(),
                    anchors: anchors
                        .iter()
                        .map(|(row, key)| TuiVisualAnchor {
                            key: key.clone(),
                            row: *row,
                            source: None,
                        })
                        .collect(),
                }
            },
            transcript_invocation_id: |_| None,
            pending_signature: |_| TranscriptLayoutSignature::new(String::new()),
            pending_rows: |_| TranscriptLayoutRows::default(),
            history_banner_signature: || None,
            history_banner_rows: TranscriptLayoutRows::default,
            reset: || false,
        });
        layout
    }

    fn unit(id: u64) -> DisclosureTarget {
        DisclosureTarget::Unit(id)
    }

    fn section(id: u64, name: &str) -> DisclosureTarget {
        DisclosureTarget::Section(id, name.to_owned())
    }

    fn key(target: &DisclosureTarget) -> String {
        target.to_hit_id()
    }

    /// Items 10, 11, 12, 13: a group header plus unit on 10, a unit with two
    /// sections on 11, a plain message (no anchors) on 12, a unit on 13.
    fn sample() -> TranscriptLayoutCache {
        layout(&[
            (
                3,
                vec![
                    (0, key(&DisclosureTarget::Group(10))),
                    (1, key(&unit(10))),
                    // Non-disclosure anchors are ignored.
                    (2, "markdown:body".to_owned()),
                ],
            ),
            (
                6,
                vec![
                    (0, key(&unit(11))),
                    (1, key(&section(11, "arguments"))),
                    (3, key(&section(11, "output"))),
                ],
            ),
            (2, Vec::new()),
            (2, vec![(0, key(&unit(13)))]),
        ])
    }

    const ITEMS: [u64; 4] = [10, 11, 12, 13];

    fn anchors<'a>(
        layout: &'a TranscriptLayoutCache,
        items: &'static [u64],
    ) -> LayoutAnchors<'a, impl Fn(u64) -> Option<usize>> {
        LayoutAnchors::new(layout, items.len(), move |id| {
            items.iter().position(|item| *item == id)
        })
    }

    #[test]
    fn traversal_follows_transcript_order_across_entries_and_skips_unanchored_items() {
        let layout = sample();
        let anchors = anchors(&layout, &ITEMS);
        let order = [
            DisclosureTarget::Group(10),
            unit(10),
            unit(11),
            section(11, "arguments"),
            section(11, "output"),
            unit(13),
        ];
        for pair in order.windows(2) {
            assert_eq!(step(&anchors, &pair[0], true).as_ref(), Some(&pair[1]));
            assert_eq!(step(&anchors, &pair[1], false).as_ref(), Some(&pair[0]));
        }
        assert_eq!(step(&anchors, &unit(13), true), None);
        assert_eq!(step(&anchors, &DisclosureTarget::Group(10), false), None);
        // A target that is no longer laid out steps to its entry's first target.
        assert_eq!(step(&anchors, &section(11, "diff"), true), Some(unit(11)));
        // Targets of items outside the resident window cannot step.
        assert_eq!(step(&anchors, &unit(99), true), None);
    }

    #[test]
    fn anchors_sharing_a_row_do_not_hide_disclosure_targets() {
        // A self-drawn visual records its own anchor on the unit's first row,
        // after the unit anchor, and another anchor on a later section row.
        let layout = layout(&[
            (
                4,
                vec![
                    (0, key(&unit(10))),
                    (0, "plugin:first-line".to_owned()),
                    (2, "plugin:second".to_owned()),
                    (2, key(&section(10, "bcode.git.files"))),
                ],
            ),
            (1, vec![(0, key(&unit(11)))]),
        ]);
        let anchors = anchors(&layout, &[10, 11]);
        assert_eq!(
            anchors.entry_targets(0),
            [(0, unit(10)), (2, section(10, "bcode.git.files"))]
        );
        assert_eq!(initial_target(&anchors, 0, 1), Some(unit(10)));
        assert_eq!(
            step(&anchors, &unit(11), false),
            Some(section(10, "bcode.git.files"))
        );
        assert_eq!(target_row(&anchors, &unit(10)), Some(0));
    }

    #[test]
    fn target_rows_come_from_the_retained_anchor_rows() {
        let layout = sample();
        let anchors = anchors(&layout, &ITEMS);
        assert_eq!(target_row(&anchors, &unit(10)), Some(1));
        assert_eq!(target_row(&anchors, &section(11, "output")), Some(3 + 3));
        assert_eq!(target_row(&anchors, &unit(13)), Some(3 + 6 + 2));
        assert_eq!(target_row(&anchors, &section(11, "diff")), None);
    }

    #[test]
    fn initial_focus_is_the_last_target_in_view_or_the_nearest_above() {
        let layout = sample();
        let anchors = anchors(&layout, &ITEMS);
        // Whole transcript visible.
        assert_eq!(initial_target(&anchors, 0, 40), Some(unit(13)));
        // Rows 0..=6: the output label at row 6 is the last visible target.
        assert_eq!(initial_target(&anchors, 0, 7), Some(section(11, "output")));
        // Rows 9..=10 hold only the plain message; the nearest target above wins.
        assert_eq!(initial_target(&anchors, 9, 2), Some(section(11, "output")));
        assert_eq!(initial_target(&anchors, 0, 0), None);
        let empty = self::layout(&[]);
        assert_eq!(initial_target(&self::anchors(&empty, &[]), 0, 10), None);
    }

    #[test]
    fn expand_and_collapse_move_one_level_at_a_time() {
        let group = DisclosureTarget::Group(1);
        assert_eq!(
            expand_level(&group, Disclosure::Collapsed),
            Some(Disclosure::Summary)
        );
        assert_eq!(
            expand_level(&group, Disclosure::Summary),
            Some(Disclosure::Expanded)
        );
        assert_eq!(expand_level(&group, Disclosure::Expanded), None);
        assert_eq!(
            collapse_level(&group, Disclosure::Expanded),
            Some(Disclosure::Summary)
        );
        assert_eq!(
            collapse_level(&group, Disclosure::Summary),
            Some(Disclosure::Collapsed)
        );
        assert_eq!(collapse_level(&group, Disclosure::Collapsed), None);
        for target in [unit(1), section(1, "output")] {
            for current in [
                Disclosure::Collapsed,
                Disclosure::Summary,
                Disclosure::Hidden,
            ] {
                assert_eq!(expand_level(&target, current), Some(Disclosure::Expanded));
            }
            assert_eq!(expand_level(&target, Disclosure::Expanded), None);
            for current in [Disclosure::Expanded, Disclosure::Summary] {
                assert_eq!(
                    collapse_level(&target, current),
                    Some(Disclosure::Collapsed)
                );
            }
            assert_eq!(collapse_level(&target, Disclosure::Collapsed), None);
            assert_eq!(collapse_level(&target, Disclosure::Hidden), None);
        }
    }

    #[test]
    fn detail_cycles_collapsed_summary_expanded_and_reveals_hidden_first() {
        assert_eq!(next_detail(Disclosure::Collapsed), Disclosure::Summary);
        assert_eq!(next_detail(Disclosure::Summary), Disclosure::Expanded);
        assert_eq!(next_detail(Disclosure::Expanded), Disclosure::Collapsed);
        assert_eq!(next_detail(Disclosure::Hidden), Disclosure::Summary);
    }

    #[test]
    fn parents_climb_from_sections_to_units_to_groups() {
        assert_eq!(parent(&section(4, "output"), |_| Some(2)), Some(unit(4)));
        assert_eq!(
            parent(&unit(4), |id| (id == 4).then_some(2)),
            Some(DisclosureTarget::Group(2))
        );
        assert_eq!(parent(&unit(4), |_| None), None);
        assert_eq!(parent(&DisclosureTarget::Group(2), |_| Some(1)), None);
    }

    #[test]
    fn reveal_scrolls_only_when_the_row_is_outside_the_viewport() {
        assert_eq!(reveal_top_row(5, 0, 10), None);
        assert_eq!(reveal_top_row(9, 0, 10), None);
        assert_eq!(reveal_top_row(10, 0, 10), Some(1));
        assert_eq!(reveal_top_row(3, 4, 10), Some(3));
        // A zero-height viewport still reveals the row as the top row.
        assert_eq!(reveal_top_row(7, 0, 0), Some(7));
    }

    #[test]
    fn focus_rects_are_clipped_to_the_transcript_area() {
        let area = Rect::new(2, 3, 20, 5);
        assert_eq!(row_rect(area, 10, 12), Some(Rect::new(2, 5, 20, 1)));
        assert_eq!(row_rect(area, 10, 9), None);
        assert_eq!(row_rect(area, 10, 15), None);
        assert_eq!(row_rect(area, 10, 14), Some(Rect::new(2, 7, 20, 1)));
        assert_eq!(row_rect(Rect::new(2, 3, 0, 5), 0, 0), None);
        assert_eq!(row_rect(Rect::new(2, 3, 20, 0), 0, 0), None);
        assert_eq!(row_rect(area, 0, usize::MAX), None);
    }

    #[test]
    fn focus_state_reports_targets_only_while_active_and_forgets_departed_items() {
        let mut focus = TranscriptFocus::default();
        assert!(!focus.is_active());
        assert_eq!(focus.focused(), None);
        focus.enter(unit(3));
        assert_eq!(focus.focused(), Some(&unit(3)));
        assert!(!focus.focus(unit(3)));
        assert!(focus.focus(section(3, "output")));
        focus.retain(|id| id == 3);
        assert_eq!(focus.focused(), Some(&section(3, "output")));
        focus.retain(|_| false);
        assert!(focus.is_active());
        assert_eq!(focus.focused(), None);
        assert!(focus.exit());
        assert!(!focus.exit());
    }

    #[test]
    fn action_ids_round_trip() {
        for action in FocusAction::ALL {
            assert_eq!(FocusAction::from_id(action.id()), Some(action));
            assert_ne!(action.default_keys(), [] as [&str; 0]);
        }
        assert_eq!(FocusAction::from_id("tui.control.down"), None);
    }
}
