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
//! 4. The guide panel shows when the user opened it: to the right when the body is at least
//!    [`GUIDE_RIGHT_BODY`] columns (at least [`GUIDE_MIN_WIDTH`] wide; the file panel gives way
//!    first, and is left out when both would leave the document too little), under the
//!    document when the body has at least [`GUIDE_BOTTOM_BODY_HEIGHT`] rows (at least
//!    [`GUIDE_MIN_HEIGHT`] tall), otherwise not at all. When the threads panel is to the right
//!    too, the two share the right column, the guide on top.
//!
//! **Window controls.** Every panel has the controls of [`crate::panel_controls`]. A minimized
//! panel collapses along its group's axis (BMUX [`PanelGroupState::collapse`]): to its title
//! row when stacked, to a strip when side by side; the review document takes the cells it
//! frees (or, when the document itself is minimized, the expanded panels share them). The
//! document minimizes only while an expanded panel shares its group: under it when panels sit
//! under it, else beside it. When the guide and the threads share the right column and both are
//! minimized, the column becomes a strip holding both. Dividers next to a minimized panel do not
//! resize. A maximized panel is the only one the body lays out ([`Panels::maximized`]); the
//! group states stay as they were, so restoring brings back the same sizes. Minimized panels
//! are kept with the presentation state; the maximized panel is not (a review reopens with
//! every panel at its size).
//!
//! The review assistant is not a review panel: the app docks it beside or under the whole
//! screen ([`crate::assistant::panel`]), with the same controls.

use crate::panel_controls::Shape;
use bcode_code_review_models::{PANEL_SHARE_SCALE, ReviewPanelState};
use bmux_tui::geometry::Rect;
use bmux_tui_components::panel_group::{
    PanelDividerMode, PanelGroup, PanelGroupAxis, PanelGroupConstraints, PanelGroupOutcome,
    PanelGroupPolicy, PanelGroupResizePolicy, PanelGroupState, PanelGroupStyles, PanelSize,
};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

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
/// Body width from which the guide panel sits to the right.
pub const GUIDE_RIGHT_BODY: u16 = 100;
/// Body height from which the guide panel can sit under the document.
pub const GUIDE_BOTTOM_BODY_HEIGHT: u16 = 16;
/// Narrowest guide panel beside the document.
pub const GUIDE_MIN_WIDTH: u16 = 36;
/// Shortest guide panel under the document.
pub const GUIDE_MIN_HEIGHT: u16 = 8;
/// Default guide panel width (right) or height (bottom).
const GUIDE_DEFAULT_WIDTH: u16 = 48;
const GUIDE_DEFAULT_HEIGHT: u16 = 10;
/// Share of the right column the guide takes over the threads panel, in percent.
const GUIDE_SIDE_PERCENT: u16 = 60;

/// A panel of the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PanelId {
    /// File tree.
    Files,
    /// Review document.
    Review,
    /// Threads list.
    Threads,
    /// The guide.
    Guide,
    /// The right column holding the guide over the threads panel.
    Side,
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
    /// Where the guide panel shows.
    pub guide: SidePlacement,
}

/// Panel visibility, sizes, and the BMUX group states.
#[derive(Debug)]
pub struct Panels {
    /// Horizontal group: files, the review column, threads on the right.
    pub root: RefCell<PanelGroupState>,
    /// Vertical group of the review column: document, then bottom panels.
    pub column: RefCell<PanelGroupState>,
    /// Vertical group of the right column when the guide and the threads share it.
    pub side: RefCell<PanelGroupState>,
    /// Panels in `root`, in order.
    root_members: Vec<PanelId>,
    /// Panels in `column`, in order (the document first).
    column_members: Vec<PanelId>,
    /// The user's file panel choice; `None` follows the width.
    pub files_choice: Option<bool>,
    /// Whether the user opened the threads panel.
    pub threads_open: bool,
    /// Whether the user opened the guide panel.
    pub guide_open: bool,
    /// Sizes chosen by resizing, as shares of the body.
    shares: Shares,
    /// Cell sizes from state saved before sizes were proportional, until converted.
    legacy: Legacy,
    /// Whether the last [`Self::sync`] converted a saved cell size into a share.
    converted: bool,
    plan: Option<PanelPlan>,
    body: (u16, u16),
    /// Panels the user minimized.
    minimized: Minimized,
    /// The panel the user maximized, while it is.
    maximized: Option<PanelId>,
    /// Width of a minimized strip ([`crate::panel_controls::strip_width`]).
    strip: u16,
    /// How each minimized panel is shown in the last [`Self::sync`].
    shapes: BTreeMap<PanelId, Shape>,
}

/// Panels the user minimized.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Minimized(BTreeSet<PanelId>);

impl Minimized {
    fn get(&self, id: PanelId) -> bool {
        self.0.contains(&id)
    }

    fn set(&mut self, id: PanelId, minimized: bool) {
        if id == PanelId::Side {
            return;
        }
        if minimized {
            self.0.insert(id);
        } else {
            self.0.remove(&id);
        }
    }
}

/// Panels a sync collapses, as `(index, cells)` in each group.
#[derive(Debug, Default)]
struct Collapses {
    root: Vec<(usize, u16)>,
    column: Vec<(usize, u16)>,
    side: Vec<(usize, u16)>,
}

/// Default strip width before the surface sets one from its theme.
const DEFAULT_STRIP: u16 = 5;

/// Sizes chosen by resizing, in parts of [`PANEL_SHARE_SCALE`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Shares {
    /// File panel width, of the body width.
    files: Option<u16>,
    /// Threads panel width beside the document, of the body width.
    threads_right: Option<u16>,
    /// Threads panel height under the document, of the body height.
    threads_bottom: Option<u16>,
    /// Guide panel width beside the document, of the body width.
    guide_right: Option<u16>,
    /// Guide panel height under the document, of the body height.
    guide_bottom: Option<u16>,
    /// The guide's outline height, of the rows it shares with the current section.
    guide_outline: Option<u16>,
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
            side: RefCell::new(PanelGroupState::new(Vec::new())),
            root_members: Vec::new(),
            column_members: Vec::new(),
            files_choice: None,
            threads_open: false,
            guide_open: false,
            shares: Shares::default(),
            legacy: Legacy::default(),
            converted: false,
            plan: None,
            body: (0, 0),
            minimized: Minimized::default(),
            maximized: None,
            strip: DEFAULT_STRIP,
            shapes: BTreeMap::new(),
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

/// `Some(true)` for a set flag, else `None` (left out of the saved state).
const fn flag(set: bool) -> Option<bool> {
    if set { Some(true) } else { None }
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
            guide_right: state.guide_right_share,
            guide_bottom: state.guide_bottom_share,
            guide_outline: state.guide_outline_share,
        };
        Self {
            files_choice: state.file_panel_visible,
            threads_open: state.threads_panel_visible.unwrap_or(false),
            guide_open: state.guide_panel_visible.unwrap_or(false),
            legacy: Legacy {
                files: state.file_panel_width.filter(|_| shares.files.is_none()),
                threads: state
                    .threads_panel_size
                    .filter(|_| shares.threads_right.is_none() && shares.threads_bottom.is_none()),
            },
            shares,
            minimized: Minimized(
                [
                    (PanelId::Files, state.file_panel_minimized),
                    (PanelId::Review, state.review_panel_minimized),
                    (PanelId::Threads, state.threads_panel_minimized),
                    (PanelId::Guide, state.guide_panel_minimized),
                ]
                .into_iter()
                .filter_map(|(id, minimized)| minimized.unwrap_or(false).then_some(id))
                .collect(),
            ),
            ..Self::default()
        }
    }

    /// Presentation state of the panels.
    #[must_use]
    pub fn state(&self) -> ReviewPanelState {
        ReviewPanelState {
            file_panel_visible: self.files_choice,
            file_panel_width: self.legacy.files,
            threads_panel_visible: if self.threads_open { Some(true) } else { None },
            threads_panel_size: self.legacy.threads,
            file_panel_share: self.shares.files,
            threads_right_share: self.shares.threads_right,
            threads_bottom_share: self.shares.threads_bottom,
            guide_panel_visible: if self.guide_open { Some(true) } else { None },
            guide_right_share: self.shares.guide_right,
            guide_bottom_share: self.shares.guide_bottom,
            guide_outline_share: self.shares.guide_outline,
            file_panel_minimized: flag(self.minimized.get(PanelId::Files)),
            review_panel_minimized: flag(self.minimized.get(PanelId::Review)),
            threads_panel_minimized: flag(self.minimized.get(PanelId::Threads)),
            guide_panel_minimized: flag(self.minimized.get(PanelId::Guide)),
        }
    }

    /// The guide's outline height as dragged, of the rows it shares with the current section
    /// (parts of [`PANEL_SHARE_SCALE`]).
    #[must_use]
    pub const fn guide_outline_share(&self) -> Option<u16> {
        self.shares.guide_outline
    }

    /// Keep the guide's outline height as dragged. Returns whether it changed.
    pub fn set_guide_outline_share(&mut self, share: u16) -> bool {
        let share = Some(share.min(PANEL_SHARE_SCALE));
        let changed = self.shares.guide_outline != share;
        self.shares.guide_outline = share;
        changed
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
        let threads = if !bordered || !self.threads_open {
            SidePlacement::Hidden
        } else if width >= THREADS_RIGHT_BODY {
            SidePlacement::Right
        } else if height >= THREADS_BOTTOM_BODY_HEIGHT {
            SidePlacement::Bottom
        } else {
            SidePlacement::Hidden
        };
        let guide = if !bordered || !self.guide_open {
            SidePlacement::Hidden
        } else if width >= GUIDE_RIGHT_BODY {
            SidePlacement::Right
        } else if height >= GUIDE_BOTTOM_BODY_HEIGHT {
            SidePlacement::Bottom
        } else {
            SidePlacement::Hidden
        };
        let mut files = bordered
            && width >= FILES_MIN_BODY
            && match self.files_choice {
                Some(open) => open,
                None => width >= FILES_AUTO_BODY,
            };
        // Beside the guide, the file panel is left out when both at their narrowest would leave
        // the document too little.
        if files
            && matches!(guide, SidePlacement::Right)
            && width.saturating_sub(REVIEW_MIN_WIDTH) < FILES_MIN_WIDTH + GUIDE_MIN_WIDTH
        {
            files = false;
        }
        PanelPlan {
            bordered,
            files,
            threads,
            guide,
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

    /// Panels of the right column when the guide and the threads share it.
    #[must_use]
    pub const fn side_members(&self) -> &'static [PanelId] {
        &[PanelId::Guide, PanelId::Threads]
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
    /// minimum and to what leaves the document [`REVIEW_MIN_WIDTH`] columns. When the side
    /// panels together leave the document less, the threads panel gives way first (down to its
    /// minimum), then the file panel; beside the guide, the file panel gives way first.
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
        let guide_right = plan.guide == SidePlacement::Right;
        let right_member = match (guide_right, plan.threads == SidePlacement::Right) {
            (true, true) => Some(PanelId::Side),
            (true, false) => Some(PanelId::Guide),
            (false, true) => Some(PanelId::Threads),
            (false, false) => None,
        };
        let right_min = if guide_right {
            GUIDE_MIN_WIDTH
        } else {
            FILES_MIN_WIDTH
        };
        let right_max = side_room.max(right_min);
        let mut right = right_member.map(|_| {
            let (share, default) = if guide_right {
                (self.shares.guide_right, GUIDE_DEFAULT_WIDTH)
            } else {
                (self.shares.threads_right, THREADS_DEFAULT_WIDTH)
            };
            share
                .map_or(default, |share| cells_of(share, width))
                .clamp(right_min, right_max)
        });
        let sides = |files: Option<u16>, right: Option<u16>| {
            files.unwrap_or(0).saturating_add(right.unwrap_or(0))
        };
        let shrink_right = |files: Option<u16>, right: &mut Option<u16>| {
            if let Some(cells) = right.as_mut() {
                let excess = sides(files, Some(*cells)).saturating_sub(side_room);
                *cells = cells.saturating_sub(excess).max(right_min);
            }
        };
        let shrink_files = |files: &mut Option<u16>, right: Option<u16>| {
            if let Some(cells) = files.as_mut() {
                let excess = sides(Some(*cells), right).saturating_sub(side_room);
                *cells = cells.saturating_sub(excess).max(FILES_MIN_WIDTH);
            }
        };
        if guide_right {
            shrink_files(&mut files, right);
            shrink_right(files, &mut right);
        } else {
            shrink_right(files, &mut right);
            shrink_files(&mut files, right);
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
        lengths.push(width.saturating_sub(sides(files, right)));
        constraints.push(PanelGroupConstraints::new(
            REVIEW_MIN_WIDTH.min(width),
            None,
        ));
        if let (Some(member), Some(cells)) = (right_member, right) {
            members.push(member);
            lengths.push(cells);
            constraints.push(PanelGroupConstraints::new(right_min, Some(right_max)));
        }
        self.root_members = members;
        let (column_lengths, column_constraints) = self.column_layout(plan, height);
        let collapses = self.collapses();
        let review = self
            .root_members
            .iter()
            .position(|member| *member == PanelId::Review);
        *self.root.borrow_mut() = build_group(lengths, constraints, &collapses.root, review);
        *self.column.borrow_mut() = build_group(
            column_lengths,
            column_constraints,
            &collapses.column,
            Some(0),
        );
        let (side_lengths, side_constraints) = side_layout(height);
        let side_absorb = collapses.side.first().map(|(index, _)| 1 - index.min(&1));
        *self.side.borrow_mut() =
            build_group(side_lengths, side_constraints, &collapses.side, side_absorb);
        plan
    }

    /// The review column: the document, then the guide and threads panels under it. Sets its
    /// members and returns their lengths and constraints.
    fn column_layout(
        &mut self,
        plan: PanelPlan,
        height: u16,
    ) -> (Vec<u16>, Vec<PanelGroupConstraints>) {
        let mut members = vec![PanelId::Review];
        let mut lengths = Vec::new();
        let mut constraints = Vec::new();
        let room = height.saturating_sub(MIN_PANE_ROWS);
        let mut guide = (plan.guide == SidePlacement::Bottom).then(|| {
            self.shares
                .guide_bottom
                .map_or(GUIDE_DEFAULT_HEIGHT, |share| cells_of(share, height))
                .clamp(GUIDE_MIN_HEIGHT, room.max(GUIDE_MIN_HEIGHT))
                .min(height)
        });
        let mut threads = (plan.threads == SidePlacement::Bottom).then(|| {
            self.shares
                .threads_bottom
                .map_or(THREADS_DEFAULT_HEIGHT, |share| cells_of(share, height))
                .clamp(THREADS_MIN_HEIGHT, room.max(THREADS_MIN_HEIGHT))
                .min(height)
        });
        // Both under the document: the threads give way first, then the guide.
        let total = |guide: Option<u16>, threads: Option<u16>| {
            guide.unwrap_or(0).saturating_add(threads.unwrap_or(0))
        };
        if let Some(cells) = threads.as_mut() {
            let excess = total(guide, Some(*cells)).saturating_sub(room);
            *cells = cells.saturating_sub(excess).max(THREADS_MIN_HEIGHT);
        }
        if let Some(cells) = guide.as_mut() {
            let excess = total(Some(*cells), threads).saturating_sub(room);
            *cells = cells.saturating_sub(excess).max(GUIDE_MIN_HEIGHT);
        }
        if total(guide, threads) > room {
            // No room for both at their minimum: the threads panel is left out.
            threads = None;
        }
        let document = height.saturating_sub(total(guide, threads));
        lengths.push(document);
        constraints.push(PanelGroupConstraints::new(
            if guide.is_some() || threads.is_some() {
                MIN_PANE_ROWS
            } else {
                0
            },
            None,
        ));
        if let Some(cells) = guide {
            members.push(PanelId::Guide);
            lengths.push(cells);
            constraints.push(PanelGroupConstraints::new(
                GUIDE_MIN_HEIGHT,
                Some(room.max(GUIDE_MIN_HEIGHT)),
            ));
        }
        if let Some(cells) = threads {
            members.push(PanelId::Threads);
            lengths.push(cells);
            constraints.push(PanelGroupConstraints::new(
                THREADS_MIN_HEIGHT,
                Some(room.max(THREADS_MIN_HEIGHT)),
            ));
        }
        self.column_members = members;
        (lengths, constraints)
    }

    /// Decide which shown panels collapse, and how each is shown (see the module docs).
    fn collapses(&mut self) -> Collapses {
        let strip = self.strip.max(1);
        let minimized = self.minimized.clone();
        let mut collapses = Collapses::default();
        let mut shapes = BTreeMap::new();
        let mut side_strip = false;
        if self.root_members.contains(&PanelId::Side) {
            match (
                minimized.get(PanelId::Guide),
                minimized.get(PanelId::Threads),
            ) {
                (true, true) => {
                    side_strip = true;
                    shapes.insert(PanelId::Guide, Shape::Strip);
                    shapes.insert(PanelId::Threads, Shape::Strip);
                }
                (true, false) => {
                    collapses.side.push((0, 1));
                    shapes.insert(PanelId::Guide, Shape::Row);
                }
                (false, true) => {
                    collapses.side.push((1, 1));
                    shapes.insert(PanelId::Threads, Shape::Row);
                }
                (false, false) => {}
            }
        }
        for (index, member) in self.column_members.iter().enumerate().skip(1) {
            if minimized.get(*member) {
                collapses.column.push((index, 1));
                shapes.insert(*member, Shape::Row);
            }
        }
        for (index, member) in self.root_members.iter().enumerate() {
            let collapse = match member {
                PanelId::Files | PanelId::Guide | PanelId::Threads => minimized.get(*member),
                PanelId::Side => side_strip,
                PanelId::Review => false,
            };
            if collapse {
                collapses.root.push((index, strip));
                if *member != PanelId::Side {
                    shapes.insert(*member, Shape::Strip);
                }
            }
        }
        if minimized.get(PanelId::Review) {
            let expanded = |members: &[PanelId], collapsed: &[(usize, u16)]| {
                (0..members.len()).any(|index| {
                    members[index] != PanelId::Review
                        && !collapsed.iter().any(|(at, _)| *at == index)
                })
            };
            if self.column_members.len() > 1 {
                if expanded(&self.column_members, &collapses.column) {
                    collapses.column.push((0, 1));
                    shapes.insert(PanelId::Review, Shape::Row);
                }
            } else if expanded(&self.root_members, &collapses.root)
                && let Some(index) = self
                    .root_members
                    .iter()
                    .position(|member| *member == PanelId::Review)
            {
                collapses.root.push((index, strip));
                shapes.insert(PanelId::Review, Shape::Strip);
            }
        }
        self.shapes = shapes;
        collapses
    }

    /// Record the sizes a divider drag left, as shares of the body. Returns whether a
    /// persisted size changed. Minimized panels keep their sizes, and a group whose document
    /// is minimized records nothing (its panels hold the document's cells meanwhile).
    pub fn note_resize(&mut self) -> bool {
        let before = self.shares;
        let (width, height) = self.body;
        let review_collapsed = |members: &[PanelId], state: &PanelGroupState| {
            members
                .iter()
                .position(|member| *member == PanelId::Review)
                .is_some_and(|index| state.is_collapsed(index))
        };
        let root_state = self.root.borrow().clone();
        if !review_collapsed(&self.root_members, &root_state) {
            let root = Self::lengths(PanelGroupAxis::Horizontal, &root_state, width, height);
            for (index, (member, cells)) in self.root_members.iter().zip(root).enumerate() {
                if root_state.is_collapsed(index) {
                    continue;
                }
                match member {
                    PanelId::Files => self.shares.files = Some(share_of(cells, width)),
                    PanelId::Threads => self.shares.threads_right = Some(share_of(cells, width)),
                    PanelId::Guide | PanelId::Side => {
                        self.shares.guide_right = Some(share_of(cells, width));
                    }
                    PanelId::Review => {}
                }
            }
        }
        let column_state = self.column.borrow().clone();
        if !review_collapsed(&self.column_members, &column_state) {
            let column = Self::lengths(PanelGroupAxis::Vertical, &column_state, width, height);
            for (index, (member, cells)) in
                self.column_members.iter().zip(column).enumerate().skip(1)
            {
                if column_state.is_collapsed(index) {
                    continue;
                }
                match member {
                    PanelId::Threads => {
                        self.shares.threads_bottom = Some(share_of(cells, height));
                    }
                    PanelId::Guide => self.shares.guide_bottom = Some(share_of(cells, height)),
                    PanelId::Files | PanelId::Review | PanelId::Side => {}
                }
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
    pub fn toggle_files(&mut self) -> bool {
        let (width, height) = self.body;
        let shown = self.plan(width, height).files;
        self.files_choice = Some(!shown);
        self.opened_or_closed(PanelId::Files);
        self.plan(width, height).files
    }

    /// Show the file panel (when the body is wide enough for it at all).
    pub fn show_files(&mut self) {
        self.files_choice = Some(true);
        self.opened_or_closed(PanelId::Files);
    }

    /// Toggle the threads panel; returns whether the user wants it open.
    pub fn toggle_threads(&mut self) -> bool {
        self.threads_open = !self.threads_open;
        self.opened_or_closed(PanelId::Threads);
        self.threads_open
    }

    /// Toggle the guide panel; returns whether the user wants it open.
    pub fn toggle_guide(&mut self) -> bool {
        self.guide_open = !self.guide_open;
        self.opened_or_closed(PanelId::Guide);
        self.guide_open
    }

    /// Whether the guide panel has room in the body as last laid out (beside or under the
    /// document), opened or not.
    #[must_use]
    pub const fn guide_fits(&self) -> bool {
        let (width, height) = self.body;
        width >= MIN_BORDERED_WIDTH
            && height >= MIN_BORDERED_HEIGHT
            && (width >= GUIDE_RIGHT_BODY || height >= GUIDE_BOTTOM_BODY_HEIGHT)
    }

    /// Open the guide panel (restored, when it was minimized).
    pub fn show_guide(&mut self) {
        self.guide_open = true;
        self.opened_or_closed(PanelId::Guide);
    }

    /// A panel was opened (it shows at its size, not minimized) or closed (it is no longer
    /// maximized, and opens at its size next time).
    fn opened_or_closed(&mut self, id: PanelId) {
        self.minimized.set(id, false);
        if self.maximized == Some(id) {
            self.maximized = None;
        }
        self.plan = None;
    }

    /// Close panel `id`; returns whether it closed. The review document never closes.
    pub fn close(&mut self, id: PanelId) -> bool {
        match id {
            PanelId::Files => self.files_choice = Some(false),
            PanelId::Threads => self.threads_open = false,
            PanelId::Guide => self.guide_open = false,
            PanelId::Review | PanelId::Side => return false,
        }
        self.opened_or_closed(id);
        true
    }

    /// Set the width of a minimized strip.
    pub const fn set_strip_width(&mut self, cells: u16) {
        if self.strip != cells {
            self.strip = cells;
            self.plan = None;
        }
    }

    /// Whether panel `id` shows in the body as last laid out.
    #[must_use]
    pub fn shown(&self, id: PanelId) -> bool {
        self.plan.is_some_and(|plan| plan.bordered)
            && (self.root_members.contains(&id)
                || self.column_members.contains(&id)
                || (matches!(id, PanelId::Guide | PanelId::Threads)
                    && self.root_members.contains(&PanelId::Side)))
    }

    /// The maximized panel, while it shows: the only panel the body lays out.
    #[must_use]
    pub fn maximized(&self) -> Option<PanelId> {
        self.maximized.filter(|id| self.shown(*id))
    }

    /// How panel `id` is shown in the body as last laid out.
    #[must_use]
    pub fn shape(&self, id: PanelId) -> Shape {
        if self.maximized() == Some(id) {
            return Shape::Maximized;
        }
        self.shapes.get(&id).copied().unwrap_or_default()
    }

    /// Restore a maximized panel; returns whether one was.
    pub const fn restore_maximized(&mut self) -> bool {
        self.maximized.take().is_some()
    }

    /// Maximize panel `id`, or restore it when it is maximized. Returns whether it is now
    /// maximized; `None` when it does not show.
    pub fn toggle_maximize(&mut self, id: PanelId) -> Option<bool> {
        if self.maximized() == Some(id) {
            self.maximized = None;
            return Some(false);
        }
        if !self.shown(id) {
            return None;
        }
        if self.minimized.get(id) {
            self.minimized.set(id, false);
            self.plan = None;
        }
        self.maximized = Some(id);
        Some(true)
    }

    /// Minimize panel `id`, or restore it when it is minimized. Returns whether it is now
    /// minimized; `None` when it cannot minimize here (it does not show, or it is the
    /// document and no expanded panel shares its group).
    pub fn toggle_minimize(&mut self, id: PanelId) -> Option<bool> {
        if !self.shown(id) {
            return None;
        }
        if self.maximized == Some(id) {
            self.maximized = None;
        }
        let (width, height) = self.body;
        if self.shape(id).minimized() {
            self.minimized.set(id, false);
            self.plan = None;
            self.sync(width, height);
            return Some(false);
        }
        self.minimized.set(id, true);
        self.plan = None;
        self.sync(width, height);
        if self.shape(id).minimized() {
            return Some(true);
        }
        self.minimized.set(id, false);
        self.plan = None;
        self.sync(width, height);
        None
    }

    /// Grow (positive) or shrink panel `id` by `delta` cells, moving its edge toward the
    /// document (the document's own edge toward a panel beside or under it), within the same
    /// limits as dragging it. Returns whether a size changed. Minimized and maximized panels
    /// keep their size.
    pub fn resize(&mut self, id: PanelId, delta: i32) -> bool {
        if self.maximized().is_some() || !self.shown(id) || self.shape(id).minimized() {
            return false;
        }
        let in_side = matches!(id, PanelId::Guide | PanelId::Threads)
            && self.root_members.contains(&PanelId::Side);
        let member = if in_side { PanelId::Side } else { id };
        // The document grows against panels under it when there are any, else beside it.
        let column = self.column_members.len() > 1
            && (member == PanelId::Review || self.column_members[1..].contains(&member));
        let (axis, state, members) = if column {
            (
                PanelGroupAxis::Vertical,
                &self.column,
                self.column_members.clone(),
            )
        } else {
            (
                PanelGroupAxis::Horizontal,
                &self.root,
                self.root_members.clone(),
            )
        };
        let Some(index) = members.iter().position(|at| *at == member) else {
            return false;
        };
        let review = members
            .iter()
            .position(|at| *at == PanelId::Review)
            .unwrap_or(0);
        // A panel after the document moves the divider before it; one before the document (and
        // the document itself, toward a panel after it) the divider after it.
        let (divider, sign) = match index.cmp(&review) {
            std::cmp::Ordering::Equal if index + 1 < members.len() => (index, 1),
            std::cmp::Ordering::Equal if index > 0 => (index - 1, -1),
            std::cmp::Ordering::Equal => return false,
            std::cmp::Ordering::Greater => (index - 1, -1),
            std::cmp::Ordering::Less => (index, 1),
        };
        let (width, height) = self.body;
        let outcome = group(axis, PanelGroupStyles::default()).resize_divider(
            Rect::new(0, 0, width, height),
            &mut state.borrow_mut(),
            divider,
            delta.saturating_mul(sign),
        );
        matches!(outcome, PanelGroupOutcome::Resized { .. }) && self.note_resize()
    }

    /// Grow (positive) or shrink the file panel by `delta` columns, within the same limits as
    /// dragging its divider. Returns whether its size changed.
    pub fn resize_files(&mut self, delta: i32) -> bool {
        self.resize(PanelId::Files, delta)
    }

    /// Forget the sizes chosen for panel `id` by resizing (for the document, every panel's),
    /// so it takes its default size again. Returns whether a saved size changed.
    pub fn reset_size(&mut self, id: PanelId) -> bool {
        let before = (self.shares, self.legacy);
        let files = |panels: &mut Self| {
            panels.shares.files = None;
            panels.legacy.files = None;
        };
        let threads = |panels: &mut Self| {
            panels.shares.threads_right = None;
            panels.shares.threads_bottom = None;
            panels.legacy.threads = None;
        };
        let guide = |panels: &mut Self| {
            panels.shares.guide_right = None;
            panels.shares.guide_bottom = None;
        };
        match id {
            PanelId::Files => files(self),
            PanelId::Threads => threads(self),
            PanelId::Guide => guide(self),
            PanelId::Side => {
                threads(self);
                guide(self);
            }
            PanelId::Review => {
                files(self);
                threads(self);
                guide(self);
            }
        }
        self.plan = None;
        before != (self.shares, self.legacy)
    }
}

/// The right column shared by the guide (on top) and the threads, at `height` rows: lengths
/// and constraints.
fn side_layout(height: u16) -> (Vec<u16>, Vec<PanelGroupConstraints>) {
    let guide =
        u16::try_from(u32::from(height) * u32::from(GUIDE_SIDE_PERCENT) / 100).unwrap_or(height);
    let guide = guide
        .max(GUIDE_MIN_HEIGHT)
        .min(height.saturating_sub(THREADS_MIN_HEIGHT))
        .max(MIN_PANE_ROWS.min(height));
    (
        vec![guide, height.saturating_sub(guide)],
        vec![
            PanelGroupConstraints::new(MIN_PANE_ROWS.min(height), None),
            PanelGroupConstraints::new(MIN_PANE_ROWS.min(height), None),
        ],
    )
}

/// A group's state: panels at `lengths` within `constraints`, with each of `collapsed` at its
/// collapsed cells. The cells a collapse frees go to `absorb` (unless it is collapsed too),
/// else the expanded panels share them; either way the expanded panels lose their upper
/// limits, so they fill the group.
fn build_group(
    mut lengths: Vec<u16>,
    mut constraints: Vec<PanelGroupConstraints>,
    collapsed: &[(usize, u16)],
    absorb: Option<usize>,
) -> PanelGroupState {
    let is_collapsed = |index: usize| collapsed.iter().any(|(at, _)| *at == index);
    if !collapsed.is_empty() {
        let mut freed: u16 = 0;
        for (index, cells) in collapsed {
            if let Some(length) = lengths.get(*index) {
                freed = freed.saturating_add(length.saturating_sub(*cells));
            }
        }
        for (index, constraint) in constraints.iter_mut().enumerate() {
            if !is_collapsed(index) {
                constraint.max = None;
            }
        }
        if let Some(length) = absorb
            .filter(|index| !is_collapsed(*index))
            .and_then(|index| lengths.get_mut(index))
        {
            *length = length.saturating_add(freed);
        }
    }
    let mut state = PanelGroupState::new(flex_sizes(&lengths));
    state.set_constraints(constraints);
    for (index, cells) in collapsed {
        state.collapse(*index, *cells);
    }
    state
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
    fn guide_panel_sits_beside_the_document_or_under_it() {
        let mut panels = Panels::default();
        assert_eq!(panels.plan(200, 40).guide, SidePlacement::Hidden);
        assert!(panels.toggle_guide());
        assert_eq!(
            panels.plan(GUIDE_RIGHT_BODY, 40).guide,
            SidePlacement::Right
        );
        assert_eq!(
            panels
                .plan(GUIDE_RIGHT_BODY - 1, GUIDE_BOTTOM_BODY_HEIGHT)
                .guide,
            SidePlacement::Bottom
        );
        assert_eq!(
            panels
                .plan(GUIDE_RIGHT_BODY - 1, GUIDE_BOTTOM_BODY_HEIGHT - 1)
                .guide,
            SidePlacement::Hidden
        );
        assert_eq!(
            panels.plan(MIN_BORDERED_WIDTH - 1, 40).guide,
            SidePlacement::Hidden
        );
        let state = panels.state();
        assert_eq!(state.guide_panel_visible, Some(true));
        assert_eq!(Panels::restore(&state).state(), state);
    }

    #[test]
    fn beside_the_guide_the_file_panel_gives_way_first() {
        let mut panels = Panels::restore(&ReviewPanelState {
            file_panel_visible: Some(true),
            guide_panel_visible: Some(true),
            ..ReviewPanelState::default()
        });
        for width in [GUIDE_RIGHT_BODY, 120, 150, 200, 333] {
            let widths = root_widths(&mut panels, width, 40);
            assert_eq!(widths.iter().sum::<u16>(), width, "{width}: {widths:?}");
            let members = panels.root_members().to_vec();
            let review = members.iter().position(|member| *member == PanelId::Review);
            let guide = members.iter().position(|member| *member == PanelId::Guide);
            let (Some(review), Some(guide)) = (review, guide) else {
                panic!("{width}: {members:?}");
            };
            assert!(widths[review] >= REVIEW_MIN_WIDTH, "{width}: {widths:?}");
            assert!(widths[guide] >= GUIDE_MIN_WIDTH, "{width}: {widths:?}");
        }
        // From the narrowest body with the guide beside it, both fit at their narrowest.
        assert!(panels.plan(GUIDE_RIGHT_BODY, 40).files);
    }

    #[test]
    fn guide_and_threads_share_the_right_column_or_stack_under_the_document() {
        let mut panels = Panels::restore(&ReviewPanelState {
            threads_panel_visible: Some(true),
            guide_panel_visible: Some(true),
            ..ReviewPanelState::default()
        });
        panels.sync(200, 40);
        assert_eq!(panels.root_members().last(), Some(&PanelId::Side));
        panels.sync(90, 40);
        assert_eq!(
            panels.column_members(),
            [PanelId::Review, PanelId::Guide, PanelId::Threads]
        );
        let heights = Panels::lengths(PanelGroupAxis::Vertical, &panels.column.borrow(), 90, 40);
        assert_eq!(heights.iter().sum::<u16>(), 40);
        assert!(
            heights[0] >= MIN_PANE_ROWS && heights[1] >= GUIDE_MIN_HEIGHT,
            "{heights:?}"
        );
        // Short: no room for both under the document; the threads panel is left out.
        panels.sync(90, GUIDE_BOTTOM_BODY_HEIGHT);
        assert_eq!(panels.column_members(), [PanelId::Review, PanelId::Guide]);
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
