//! Panel layout of the review body and its narrow-width degradation.
//!
//! The body is a BMUX `panel_group`: the file panel, the review document, and the optional
//! threads panel (to the right when wide, under the document otherwise), each a bordered BMUX
//! `pane` whose border shows focus. Dividers sit on the panes' shared borders and can be
//! dragged; sizes and visibility are kept in [`ReviewPanelState`] (the workspace's presentation state).
//!
//! **Degradation.** Decided per frame from the body size, in this order:
//!
//! 1. Below [`MIN_BORDERED_WIDTH`] columns or [`MIN_BORDERED_HEIGHT`] rows the review document
//!    takes the whole body without pane chrome, and no side panel is shown.
//! 2. The file panel shows when the user opened it (at least [`FILES_MIN_BODY`] columns), or
//!    when nobody chose and the body is at least [`FILES_AUTO_BODY`] columns. Its width is the
//!    user's width (default a quarter of the body, [`FILES_MIN_WIDTH`]..=[`FILES_DEFAULT_MAX`]
//!    columns), clamped so the document keeps [`REVIEW_MIN_WIDTH`] columns.
//! 3. The threads panel shows when the user opened it: to the right when the body is at least
//!    [`THREADS_RIGHT_BODY`] columns, under the document when the body has at least
//!    [`THREADS_BOTTOM_BODY_HEIGHT`] rows, otherwise not at all.
//!
//! The review assistant is not a review panel: the app docks it beside or under the whole
//! screen ([`crate::assistant::panel`]).

use bcode_code_review_models::{PANEL_SHARE_SCALE, ReviewPanelState};
use bmux_tui::geometry::Rect;
use bmux_tui_components::panel_group::{
    PanelDividerMode, PanelGroup, PanelGroupAxis, PanelGroupConstraints, PanelGroupOutcome,
    PanelGroupPolicy, PanelGroupResizePolicy, PanelGroupState, PanelGroupStyles, PanelSize,
};
use std::cell::RefCell;

/// Narrowest body that gets pane chrome and side panels.
pub const MIN_BORDERED_WIDTH: u16 = 24;
/// Shortest body that gets pane chrome and side panels.
pub const MIN_BORDERED_HEIGHT: u16 = 5;
/// Body width from which the file panel shows unless the user hid it.
pub const FILES_AUTO_BODY: u16 = 100;
/// Narrowest body that shows the file panel at all.
pub const FILES_MIN_BODY: u16 = 60;
/// Narrowest file panel.
pub const FILES_MIN_WIDTH: u16 = 18;
/// Widest default file panel.
pub const FILES_DEFAULT_MAX: u16 = 44;
/// Columns the document keeps beside side panels.
pub const REVIEW_MIN_WIDTH: u16 = 40;
/// Body width from which the threads panel sits to the right.
pub const THREADS_RIGHT_BODY: u16 = 150;
/// Body height from which the threads panel can sit under the document.
pub const THREADS_BOTTOM_BODY_HEIGHT: u16 = 20;
/// Default threads panel width (right) or height (bottom).
const THREADS_DEFAULT_WIDTH: u16 = 44;
const THREADS_DEFAULT_HEIGHT: u16 = 10;
/// Smallest threads panel height under the document.
const THREADS_MIN_HEIGHT: u16 = 4;
/// Smallest bordered panel height.
const MIN_PANE_ROWS: u16 = 3;

/// A panel of the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelId {
    /// File tree.
    Files,
    /// Review document.
    Review,
    /// Threads list.
    Threads,
}

/// Where a side panel (threads) is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidePlacement {
    /// Not shown.
    Hidden,
    /// To the right of the document.
    Right,
    /// Under the document.
    Bottom,
}

/// Panels shown for one body size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelPlan {
    /// Whether panes have borders (and side panels can show).
    pub bordered: bool,
    /// Whether the file panel shows.
    pub files: bool,
    /// Where the threads panel shows.
    pub threads: SidePlacement,
}

/// Panel visibility, sizes, and the BMUX group states.
#[derive(Debug)]
pub struct Panels {
    /// Horizontal group: files, the review column, threads on the right.
    pub root: RefCell<PanelGroupState>,
    /// Vertical group of the review column: document, then bottom panels.
    pub column: RefCell<PanelGroupState>,
    /// Panels in `root`, in order.
    root_members: Vec<PanelId>,
    /// Panels in `column`, in order (the document first).
    column_members: Vec<PanelId>,
    /// The user's file panel choice; `None` follows the width.
    pub files_choice: Option<bool>,
    /// Whether the user opened the threads panel.
    pub threads_open: bool,
    /// Sizes chosen by resizing, as shares of the body.
    shares: Shares,
    /// Cell sizes from state saved before sizes were proportional, until converted.
    legacy: Legacy,
    /// Whether the last [`Self::sync`] converted a saved cell size into a share.
    converted: bool,
    plan: Option<PanelPlan>,
    body: (u16, u16),
}

/// Sizes chosen by resizing, in parts of [`PANEL_SHARE_SCALE`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Shares {
    /// File panel width, of the body width.
    files: Option<u16>,
    /// Threads panel width beside the document, of the body width.
    threads_right: Option<u16>,
    /// Threads panel height under the document, of the body height.
    threads_bottom: Option<u16>,
}

/// Cell sizes saved before sizes were proportional.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Legacy {
    files: Option<u16>,
    threads: Option<u16>,
}

impl Default for Panels {
    fn default() -> Self {
        Self {
            root: RefCell::new(PanelGroupState::new(Vec::new())),
            column: RefCell::new(PanelGroupState::new(Vec::new())),
            root_members: Vec::new(),
            column_members: Vec::new(),
            files_choice: None,
            threads_open: false,
            shares: Shares::default(),
            legacy: Legacy::default(),
            converted: false,
            plan: None,
            body: (0, 0),
        }
    }
}

/// `cells` of `total` as a share (parts of [`PANEL_SHARE_SCALE`]), rounded to the nearest.
#[must_use]
pub fn share_of(cells: u16, total: u16) -> u16 {
    if total == 0 {
        return 0;
    }
    let cells = u32::from(cells.min(total));
    let total = u32::from(total);
    let share = (cells * u32::from(PANEL_SHARE_SCALE) + total / 2) / total;
    u16::try_from(share).unwrap_or(PANEL_SHARE_SCALE)
}

/// The cells `share` (parts of [`PANEL_SHARE_SCALE`]) of `total` is, rounded to the nearest.
#[must_use]
pub fn cells_of(share: u16, total: u16) -> u16 {
    let share = u32::from(share.min(PANEL_SHARE_SCALE));
    let scale = u32::from(PANEL_SHARE_SCALE);
    let cells = (share * u32::from(total) + scale / 2) / scale;
    u16::try_from(cells).unwrap_or(total)
}

/// Requested sizes for panels that fill `total` cells exactly: each panel's flex weight is its
/// length, so the group lays them out at these lengths, and a divider drag (which rebases
/// weights to lengths) keeps every panel flexible.
fn flex_sizes(lengths: &[u16]) -> Vec<PanelSize> {
    lengths
        .iter()
        .map(|length| PanelSize::flex((*length).max(1)))
        .collect()
}

impl Panels {
    /// Panels restored from presentation state.
    #[must_use]
    pub fn restore(state: &ReviewPanelState) -> Self {
        let shares = Shares {
            files: state.file_panel_share,
            threads_right: state.threads_right_share,
            threads_bottom: state.threads_bottom_share,
        };
        Self {
            files_choice: state.file_panel_visible,
            threads_open: state.threads_panel_visible.unwrap_or(false),
            legacy: Legacy {
                files: state.file_panel_width.filter(|_| shares.files.is_none()),
                threads: state
                    .threads_panel_size
                    .filter(|_| shares.threads_right.is_none() && shares.threads_bottom.is_none()),
            },
            shares,
            ..Self::default()
        }
    }

    /// Presentation state of the panels.
    #[must_use]
    pub const fn state(&self) -> ReviewPanelState {
        ReviewPanelState {
            file_panel_visible: self.files_choice,
            file_panel_width: self.legacy.files,
            threads_panel_visible: if self.threads_open { Some(true) } else { None },
            threads_panel_size: self.legacy.threads,
            file_panel_share: self.shares.files,
            threads_right_share: self.shares.threads_right,
            threads_bottom_share: self.shares.threads_bottom,
        }
    }

    /// Whether the last layout converted sizes saved in cells into shares (the state to save
    /// changed). Reading clears it.
    pub const fn take_converted(&mut self) -> bool {
        std::mem::replace(&mut self.converted, false)
    }

    /// Panels for a body of `width` x `height` cells.
    #[must_use]
    pub const fn plan(&self, width: u16, height: u16) -> PanelPlan {
        let bordered = width >= MIN_BORDERED_WIDTH && height >= MIN_BORDERED_HEIGHT;
        let files = bordered
            && width >= FILES_MIN_BODY
            && match self.files_choice {
                Some(open) => open,
                None => width >= FILES_AUTO_BODY,
            };
        let threads = if !bordered || !self.threads_open {
            SidePlacement::Hidden
        } else if width >= THREADS_RIGHT_BODY {
            SidePlacement::Right
        } else if height >= THREADS_BOTTOM_BODY_HEIGHT {
            SidePlacement::Bottom
        } else {
            SidePlacement::Hidden
        };
        PanelPlan {
            bordered,
            files,
            threads,
        }
    }

    /// The plan of the last [`Self::sync`].
    #[must_use]
    pub const fn current(&self) -> Option<PanelPlan> {
        self.plan
    }

    /// Panels of the root group, in order.
    #[must_use]
    pub fn root_members(&self) -> &[PanelId] {
        &self.root_members
    }

    /// Panels of the column group, in order (the document first).
    #[must_use]
    pub fn column_members(&self) -> &[PanelId] {
        &self.column_members
    }

    /// Convert cell sizes saved before sizes were proportional into shares of this body, as
    /// clamped to the panels' limits here.
    fn convert_legacy(&mut self, plan: PanelPlan, width: u16, height: u16) {
        let side_max = width.saturating_sub(REVIEW_MIN_WIDTH).max(FILES_MIN_WIDTH);
        if plan.files
            && let Some(cells) = self.legacy.files.take()
        {
            let cells = cells.clamp(FILES_MIN_WIDTH, side_max);
            self.shares.files = Some(share_of(cells, width));
            self.converted = true;
        }
        let bottom_max = height.saturating_sub(MIN_PANE_ROWS).max(THREADS_MIN_HEIGHT);
        let threads = match plan.threads {
            SidePlacement::Right => Some((
                &mut self.shares.threads_right,
                width,
                (FILES_MIN_WIDTH, side_max),
            )),
            SidePlacement::Bottom => Some((
                &mut self.shares.threads_bottom,
                height,
                (THREADS_MIN_HEIGHT, bottom_max),
            )),
            SidePlacement::Hidden => None,
        };
        if let Some((share, total, (min, max))) = threads
            && let Some(cells) = self.legacy.threads.take()
        {
            *share = Some(share_of(cells.clamp(min, max), total));
            self.converted = true;
        }
    }

    /// Bring the group states in line with the plan for a `width` x `height` body.
    ///
    /// Side panels take their share of the body (or their default size), clamped to their
    /// minimum and to what leaves the document [`REVIEW_MIN_WIDTH`] columns. When the file and
    /// threads panels together leave the document less, the threads panel gives way first,
    /// down to its minimum, then the file panel.
    pub fn sync(&mut self, width: u16, height: u16) -> PanelPlan {
        let plan = self.plan(width, height);
        if self.plan == Some(plan) && self.body == (width, height) {
            return plan;
        }
        self.convert_legacy(plan, width, height);
        self.plan = Some(plan);
        self.body = (width, height);
        let side_room = width.saturating_sub(REVIEW_MIN_WIDTH);
        let side_max = side_room.max(FILES_MIN_WIDTH);
        let mut files = plan.files.then(|| {
            let default = (width / 4).clamp(FILES_MIN_WIDTH, FILES_DEFAULT_MAX);
            self.shares
                .files
                .map_or(default, |share| cells_of(share, width))
                .clamp(FILES_MIN_WIDTH, side_max)
        });
        let mut threads = (plan.threads == SidePlacement::Right).then(|| {
            self.shares
                .threads_right
                .map_or(THREADS_DEFAULT_WIDTH, |share| cells_of(share, width))
                .clamp(FILES_MIN_WIDTH, side_max)
        });
        let sides = |files: Option<u16>, threads: Option<u16>| {
            files.unwrap_or(0).saturating_add(threads.unwrap_or(0))
        };
        if let Some(cells) = threads.as_mut() {
            let excess = sides(files, Some(*cells)).saturating_sub(side_room);
            *cells = cells.saturating_sub(excess).max(FILES_MIN_WIDTH);
        }
        if let Some(cells) = files.as_mut() {
            let excess = sides(Some(*cells), threads).saturating_sub(side_room);
            *cells = cells.saturating_sub(excess).max(FILES_MIN_WIDTH);
        }
        let mut members = Vec::new();
        let mut lengths = Vec::new();
        let mut constraints = Vec::new();
        if let Some(cells) = files {
            members.push(PanelId::Files);
            lengths.push(cells);
            constraints.push(PanelGroupConstraints::new(FILES_MIN_WIDTH, Some(side_max)));
        }
        members.push(PanelId::Review);
        lengths.push(width.saturating_sub(sides(files, threads)));
        constraints.push(PanelGroupConstraints::new(
            REVIEW_MIN_WIDTH.min(width),
            None,
        ));
        if let Some(cells) = threads {
            members.push(PanelId::Threads);
            lengths.push(cells);
            constraints.push(PanelGroupConstraints::new(FILES_MIN_WIDTH, Some(side_max)));
        }
        let mut root = PanelGroupState::new(flex_sizes(&lengths));
        root.set_constraints(constraints);
        *self.root.borrow_mut() = root;
        self.root_members = members;
        // The column: the document, then the bottom panels.
        let mut column_members = vec![PanelId::Review];
        let mut column_constraints = vec![PanelGroupConstraints::new(MIN_PANE_ROWS, None)];
        let mut column_lengths = vec![height];
        if plan.threads == SidePlacement::Bottom {
            let max = height.saturating_sub(MIN_PANE_ROWS).max(THREADS_MIN_HEIGHT);
            let cells = self
                .shares
                .threads_bottom
                .map_or(THREADS_DEFAULT_HEIGHT, |share| cells_of(share, height))
                .clamp(THREADS_MIN_HEIGHT, max)
                .min(height);
            column_members.push(PanelId::Threads);
            column_lengths = vec![height - cells, cells];
            column_constraints.push(PanelGroupConstraints::new(THREADS_MIN_HEIGHT, Some(max)));
        } else {
            column_constraints = vec![PanelGroupConstraints::new(0, None)];
        }
        let mut column = PanelGroupState::new(flex_sizes(&column_lengths));
        column.set_constraints(column_constraints);
        *self.column.borrow_mut() = column;
        self.column_members = column_members;
        plan
    }

    /// Record the sizes a divider drag left, as shares of the body. Returns whether a
    /// persisted size changed.
    pub fn note_resize(&mut self) -> bool {
        let before = self.shares;
        let (width, height) = self.body;
        let root = Self::lengths(
            PanelGroupAxis::Horizontal,
            &self.root.borrow(),
            width,
            height,
        );
        for (member, cells) in self.root_members.iter().zip(root) {
            match member {
                PanelId::Files => self.shares.files = Some(share_of(cells, width)),
                PanelId::Threads => self.shares.threads_right = Some(share_of(cells, width)),
                PanelId::Review => {}
            }
        }
        let column = Self::lengths(
            PanelGroupAxis::Vertical,
            &self.column.borrow(),
            width,
            height,
        );
        for (member, cells) in self.column_members.iter().zip(column).skip(1) {
            if *member == PanelId::Threads {
                self.shares.threads_bottom = Some(share_of(cells, height));
            }
        }
        before != self.shares
    }

    /// The lengths `state` lays its panels out at in the body.
    fn lengths(axis: PanelGroupAxis, state: &PanelGroupState, width: u16, height: u16) -> Vec<u16> {
        group(axis, PanelGroupStyles::default())
            .layout(Rect::new(0, 0, width, height), state)
            .panels
            .iter()
            .map(|panel| match axis {
                PanelGroupAxis::Horizontal => panel.width,
                PanelGroupAxis::Vertical => panel.height,
            })
            .collect()
    }

    /// Toggle the file panel at the current body width; returns whether it is now shown.
    pub const fn toggle_files(&mut self) -> bool {
        let (width, height) = self.body;
        let shown = self.plan(width, height).files;
        self.files_choice = Some(!shown);
        self.plan = None;
        self.plan(width, height).files
    }

    /// Show the file panel (when the body is wide enough for it at all).
    pub const fn show_files(&mut self) {
        self.files_choice = Some(true);
        self.plan = None;
    }

    /// Toggle the threads panel; returns whether the user wants it open.
    pub const fn toggle_threads(&mut self) -> bool {
        self.threads_open = !self.threads_open;
        self.plan = None;
        self.threads_open
    }

    /// Grow (positive) or shrink the file panel by `delta` columns, within the same limits as
    /// dragging its divider. Returns whether its size changed.
    pub fn resize_files(&mut self, delta: i32) -> bool {
        if self.root_members.first() != Some(&PanelId::Files) {
            return false;
        }
        let (width, height) = self.body;
        let outcome = group(PanelGroupAxis::Horizontal, PanelGroupStyles::default())
            .resize_divider(
                Rect::new(0, 0, width, height),
                &mut self.root.borrow_mut(),
                0,
                delta,
            );
        matches!(outcome, PanelGroupOutcome::Resized { .. }) && self.note_resize()
    }
}

/// The policy of every resizable group: live resizing that keeps panels flexible, so dragged
/// sizes are kept as proportions when the area changes.
const fn resizable(
    axis: PanelGroupAxis,
    divider: PanelDividerMode,
    styles: PanelGroupStyles,
) -> PanelGroup {
    PanelGroup::new(axis)
        .policy(PanelGroupPolicy {
            resize: PanelGroupResizePolicy::live().preserving_flex(true),
            ..PanelGroupPolicy::resize_only()
        })
        .divider_mode(divider)
        .styles(styles)
}

/// A resizable group whose dividers sit on the panes' shared borders.
///
/// Side by side the handle covers the right pane's left border; stacked it covers the upper
/// pane's bottom border (the lower pane's top border holds its title).
#[must_use]
pub const fn group(axis: PanelGroupAxis, styles: PanelGroupStyles) -> PanelGroup {
    let (before, after) = match axis {
        PanelGroupAxis::Horizontal => (0, 1),
        PanelGroupAxis::Vertical => (1, 0),
    };
    resizable(axis, PanelDividerMode::Overlay { before, after }, styles)
}

/// The group of the app's screen and a docked panel after it (to the right or below): the
/// handle covers the panel's own edge border, the screen having no border of its own there.
#[must_use]
pub const fn dock_group(axis: PanelGroupAxis, styles: PanelGroupStyles) -> PanelGroup {
    resizable(
        axis,
        PanelDividerMode::Overlay {
            before: 0,
            after: 1,
        },
        styles,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_panel_follows_width_until_the_user_chooses() {
        let mut panels = Panels::default();
        assert!(panels.plan(FILES_AUTO_BODY, 30).files);
        assert!(!panels.plan(FILES_AUTO_BODY - 1, 30).files);
        panels.sync(FILES_AUTO_BODY - 1, 30);
        assert!(panels.toggle_files());
        assert!(panels.plan(FILES_MIN_BODY, 30).files);
        // Below the minimum body width it never shows, whatever the choice.
        assert!(!panels.plan(FILES_MIN_BODY - 1, 30).files);
        panels.sync(200, 30);
        assert!(!panels.toggle_files());
        assert!(!panels.plan(200, 30).files);
        // Tiny bodies drop all chrome.
        let plan = Panels::default().plan(MIN_BORDERED_WIDTH - 1, 30);
        assert!(!plan.bordered && !plan.files);
        let plan = Panels::default().plan(200, MIN_BORDERED_HEIGHT - 1);
        assert!(!plan.bordered && !plan.files);
    }

    #[test]
    fn threads_panel_moves_below_the_document_when_narrow() {
        let mut panels = Panels::default();
        assert_eq!(panels.plan(200, 40).threads, SidePlacement::Hidden);
        panels.toggle_threads();
        assert_eq!(panels.plan(200, 40).threads, SidePlacement::Right);
        assert_eq!(panels.plan(120, 40).threads, SidePlacement::Bottom);
        assert_eq!(panels.plan(120, 10).threads, SidePlacement::Hidden);
    }

    /// Lengths of the root group's panels at `width` x `height`.
    fn root_widths(panels: &mut Panels, width: u16, height: u16) -> Vec<u16> {
        panels.sync(width, height);
        Panels::lengths(
            PanelGroupAxis::Horizontal,
            &panels.root.borrow(),
            width,
            height,
        )
    }

    #[test]
    fn saved_cell_widths_convert_to_shares_at_the_first_layout() {
        let mut panels = Panels::restore(&ReviewPanelState {
            file_panel_visible: Some(true),
            file_panel_width: Some(500),
            ..ReviewPanelState::default()
        });
        // Clamped so the document keeps its minimum, then kept as that share of the body.
        assert_eq!(root_widths(&mut panels, 120, 30)[0], 120 - REVIEW_MIN_WIDTH);
        assert!(panels.take_converted());
        assert!(!panels.take_converted(), "reported once");
        let state = panels.state();
        assert_eq!(state.file_panel_width, None);
        assert_eq!(state.file_panel_share, Some(share_of(80, 120)));
        assert_eq!(root_widths(&mut panels, 240, 30)[0], 160);
        assert_eq!(Panels::restore(&state).state(), state);

        // A cell size that fits converts as is; the threads size converts for where it shows.
        let mut panels = Panels::restore(&ReviewPanelState {
            file_panel_visible: Some(true),
            file_panel_width: Some(30),
            threads_panel_visible: Some(true),
            threads_panel_size: Some(12),
            ..ReviewPanelState::default()
        });
        panels.sync(120, 40);
        assert!(panels.take_converted());
        let state = panels.state();
        assert_eq!(state.file_panel_share, Some(share_of(30, 120)));
        assert_eq!(state.threads_bottom_share, Some(share_of(12, 40)));
        assert_eq!(state.threads_panel_size, None);
        assert_eq!(state.threads_right_share, None);
    }

    #[test]
    fn keyboard_resizing_keeps_the_document_minimum_and_scales_with_the_body() {
        let mut panels = Panels::restore(&ReviewPanelState {
            file_panel_visible: Some(true),
            ..ReviewPanelState::default()
        });
        assert_eq!(root_widths(&mut panels, 120, 30), [30, 90]);
        assert!(panels.resize_files(10));
        assert_eq!(root_widths(&mut panels, 120, 30), [40, 80]);
        assert!(panels.resize_files(100));
        assert_eq!(root_widths(&mut panels, 120, 30), [80, 40]);
        assert!(!panels.resize_files(1), "the document keeps its minimum");
        assert!(panels.resize_files(-40));
        // A third of the body, at any width.
        assert_eq!(root_widths(&mut panels, 240, 30), [80, 160]);
        assert_eq!(root_widths(&mut panels, 90, 30), [30, 60]);
        assert!(panels.resize_files(-100));
        assert_eq!(root_widths(&mut panels, 90, 30), [FILES_MIN_WIDTH, 72]);
        assert!(!panels.resize_files(-1), "the file panel keeps its minimum");
    }

    #[test]
    fn side_panels_give_way_to_the_document_threads_first() {
        let mut panels = Panels::restore(&ReviewPanelState {
            file_panel_visible: Some(true),
            threads_panel_visible: Some(true),
            file_panel_share: Some(5_000),
            threads_right_share: Some(5_000),
            ..ReviewPanelState::default()
        });
        // Half and half would leave the document nothing: the threads panel gives way.
        assert_eq!(
            root_widths(&mut panels, 200, 40),
            [100, REVIEW_MIN_WIDTH, 60]
        );
        // Past its minimum, the file panel gives way too.
        panels.shares.files = Some(8_000);
        panels.plan = None;
        assert_eq!(
            root_widths(&mut panels, 200, 40),
            [
                200 - REVIEW_MIN_WIDTH - FILES_MIN_WIDTH,
                REVIEW_MIN_WIDTH,
                FILES_MIN_WIDTH
            ]
        );
        // At the narrowest body that puts threads on the right, everything still fits.
        let widths = root_widths(&mut panels, THREADS_RIGHT_BODY, 40);
        assert_eq!(widths.iter().sum::<u16>(), THREADS_RIGHT_BODY);
        assert!(widths[1] >= REVIEW_MIN_WIDTH, "{widths:?}");
    }

    #[test]
    fn shares_round_trip_cells_at_every_terminal_width() {
        for total in [1_u16, 7, 80, 333, 1_000, 4_999] {
            for cells in 0..=total.min(400) {
                assert_eq!(
                    cells_of(share_of(cells, total), total),
                    cells,
                    "{cells}/{total}"
                );
            }
        }
        assert_eq!(share_of(5, 0), 0);
        assert_eq!(cells_of(PANEL_SHARE_SCALE, 0), 0);
    }
}
