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
//! **Window controls.** Every panel has the controls of [`crate::panel_controls`], run by BMUX
//! [`PanelWindows`] over the body's groups ([`Panels::tree`]): a minimized panel collapses
//! along its group's axis (to its title row when stacked, to a strip when side by side); the
//! review document fills each group it is in, taking the cells minimized panels free, and is
//! the panel each group keeps expanded, so it minimizes only while an expanded panel shares its
//! group (under it when panels sit under it, else beside it). When the guide and the threads
//! share the right column and both are minimized, the column becomes a strip holding both. A
//! maximized panel is the only one the body lays out ([`Panels::maximized`]); the sizes stay as
//! they were, so restoring brings them back. Minimized panels are kept with the presentation
//! state; the maximized panel is not (a review reopens with every panel at its size).
//!
//! **The assistant.** While the app's review assistant is open on a review, it is a member of
//! the body too ([`Panels::set_assistant`]): its own column at the far right while the body is
//! at least [`ASSISTANT_RIGHT_BODY`] columns, else under the document while the body is at least
//! [`ASSISTANT_BOTTOM_BODY_HEIGHT`] rows, else not in the body (the app zooms it while it has the
//! keyboard). Beside it, the other panels are planned for the columns it leaves at its narrowest,
//! so the file panel and then the threads and guide panels give way before it moves under the
//! document. The body only reserves its place: the app paints the assistant there, and the
//! assistant keeps its own window state (minimized, maximized, focus), which the body mirrors.
//! Its size is the app's (handed back after a resize, [`Panels::take_assistant_size`]) unless it
//! is kept per review ([`AssistantMember::per_review`]), saved with the review's panels.

use crate::assistant::profile::DockLength;
use crate::panel_controls::{Command, Shape};
use bcode_code_review_models::{PANEL_SHARE_SCALE, ReviewPanelState};
use bmux_tui::geometry::Rect;
use bmux_tui_components::panel_group::{
    PanelDividerMode, PanelGroup, PanelGroupAxis, PanelGroupConstraints, PanelGroupOutcome,
    PanelGroupPolicy, PanelGroupResizePolicy, PanelGroupState, PanelGroupStyles,
};
use bmux_tui_components::panel_window::{
    PanelMember, PanelResize, PanelTree, PanelTreeGroup, PanelWindows, WindowCommandOutcome,
    WindowRefusal,
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
/// Body width from which the assistant has its own column at the right.
pub const ASSISTANT_RIGHT_BODY: u16 = crate::assistant::panel::RIGHT_DOCK_WIDTH;
/// Body height from which the assistant can sit under the document.
pub const ASSISTANT_BOTTOM_BODY_HEIGHT: u16 = crate::assistant::panel::BOTTOM_DOCK_HEIGHT;
/// Narrowest assistant column.
pub const ASSISTANT_MIN_WIDTH: u16 = crate::assistant::panel::MIN_WIDTH;
/// Shortest assistant under the document.
pub const ASSISTANT_MIN_HEIGHT: u16 = crate::assistant::panel::MIN_HEIGHT;

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
    /// The app's review assistant (its place only; the app paints it).
    Assistant,
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
    /// Where the assistant shows.
    pub assistant: SidePlacement,
}

/// The app's review assistant as the body lays it out: open on the review, with its window
/// state and the size it should take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssistantMember {
    /// Minimized: a strip at the right, or its title row under the document.
    pub minimized: bool,
    /// Maximized: the only panel the body lays out.
    pub maximized: bool,
    /// The app's width share for it beside the document, of the body width, when dragged.
    pub right_share: Option<u16>,
    /// The app's height share for it under the document, of the body height, when dragged.
    pub bottom_share: Option<u16>,
    /// Its configured width and height until dragged.
    pub width: DockLength,
    /// Its configured height until dragged.
    pub height: DockLength,
    /// Whether its size is kept with this review (else it is the app's).
    pub per_review: bool,
}

/// A size the assistant took in the body by resizing, for the app to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssistantSize {
    /// Its width beside the document, of the body width.
    Right(u16),
    /// Its height under the document, of the body height.
    Bottom(u16),
    /// Its size was reset to the configured default.
    Reset,
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
    /// Minimized and maximized panels.
    windows: PanelWindows<PanelId>,
    /// The app's assistant as the body lays it out.
    assistant: AssistantState,
}

/// The app's assistant as the body lays it out.
#[derive(Debug, Clone, Copy, Default)]
struct AssistantState {
    /// The assistant, while the app shows it on this review.
    member: Option<AssistantMember>,
    /// The size it took by resizing, for the app (its size is the app's).
    size: Option<AssistantSize>,
    /// Whether it has the keyboard (no other panel stays maximized meanwhile).
    focused: bool,
    /// The keyboard moved on to it (`tab`): the app gives it the keyboard.
    focus_requested: bool,
}

/// Index of the root group in [`Panels::tree`].
const ROOT_GROUP: usize = 0;
/// Index of the review column in [`Panels::tree`].
const COLUMN_GROUP: usize = 1;
/// Index of the right column in [`Panels::tree`].
const SIDE_GROUP: usize = 2;

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
    /// Assistant width beside the document, of the body width (kept per review).
    assistant_right: Option<u16>,
    /// Assistant height under the document, of the body height (kept per review).
    assistant_bottom: Option<u16>,
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
            windows: PanelWindows::new().with_closable(PanelId::Review, false),
            assistant: AssistantState::default(),
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
            assistant_right: state.assistant_right_share,
            assistant_bottom: state.assistant_bottom_share,
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
            windows: Self::default().windows.with_minimized(
                [
                    (PanelId::Files, state.file_panel_minimized),
                    (PanelId::Review, state.review_panel_minimized),
                    (PanelId::Threads, state.threads_panel_minimized),
                    (PanelId::Guide, state.guide_panel_minimized),
                ]
                .into_iter()
                .filter_map(|(id, minimized)| minimized.unwrap_or(false).then_some(id)),
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
            file_panel_minimized: flag(self.windows.is_minimized(PanelId::Files)),
            review_panel_minimized: flag(self.windows.is_minimized(PanelId::Review)),
            threads_panel_minimized: flag(self.windows.is_minimized(PanelId::Threads)),
            guide_panel_minimized: flag(self.windows.is_minimized(PanelId::Guide)),
            assistant_right_share: self.shares.assistant_right,
            assistant_bottom_share: self.shares.assistant_bottom,
        }
    }

    /// Lay the assistant out as `member` from the next layout on (`None`: it is not shown on
    /// this review). Its minimized and maximized state follows the assistant's.
    pub fn set_assistant(&mut self, member: Option<AssistantMember>) {
        if self.assistant.member == member {
            return;
        }
        // The size a drag hands back to the app comes back while the drag goes on: the groups
        // already have it, and laying them out again would end the drag.
        let size_only = |a: AssistantMember, b: AssistantMember| {
            AssistantMember {
                right_share: None,
                bottom_share: None,
                ..a
            } == AssistantMember {
                right_share: None,
                bottom_share: None,
                ..b
            }
        };
        if let (Some(old), Some(new)) = (self.assistant.member, member)
            && size_only(old, new)
            && self.dragging()
        {
            self.assistant.member = member;
            return;
        }
        self.assistant.member = member;
        let minimized = member.is_some_and(|member| member.minimized);
        if self.windows.is_minimized(PanelId::Assistant) != minimized {
            self.windows.set_minimized(PanelId::Assistant, minimized);
        }
        let maximized = member.is_some_and(|member| member.maximized);
        let is_maximized = self.windows.maximized() == Some(PanelId::Assistant);
        if maximized && !is_maximized {
            self.windows.restore_maximized();
            let _ = self.windows.toggle_maximize(PanelId::Assistant);
        } else if !maximized && is_maximized {
            self.windows.restore_maximized();
        }
        self.plan = None;
    }

    /// Whether a divider of any group is being dragged.
    #[must_use]
    pub fn dragging(&self) -> bool {
        [&self.root, &self.column, &self.side]
            .iter()
            .any(|state| state.borrow().active_divider().is_some())
    }

    /// Whether the assistant has the keyboard, as the app last said.
    #[must_use]
    pub const fn assistant_focused(&self) -> bool {
        self.assistant.focused
    }

    /// Note whether the assistant has the keyboard (only while it is laid out).
    pub const fn set_assistant_focused(&mut self, focused: bool) {
        self.assistant.focused = focused && self.assistant.member.is_some();
    }

    /// Ask the app to give the assistant the keyboard.
    pub const fn request_assistant_focus(&mut self) {
        self.assistant.focus_requested = true;
    }

    /// Whether the keyboard was asked to move to the assistant since the last call.
    pub const fn take_assistant_focus(&mut self) -> bool {
        std::mem::replace(&mut self.assistant.focus_requested, false)
    }

    /// The assistant as laid out, while the app shows it on this review.
    #[must_use]
    pub const fn assistant(&self) -> Option<AssistantMember> {
        self.assistant.member
    }

    /// The size the assistant took by resizing since the last call, for the app to keep (its
    /// size is the app's). Reading clears it.
    pub const fn take_assistant_size(&mut self) -> Option<AssistantSize> {
        self.assistant.size.take()
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
        self.plan_for(width, height, self.guide_open)
    }

    /// Panels for a body of `width` x `height` cells, with the guide panel open or not.
    const fn plan_for(&self, width: u16, height: u16, guide_open: bool) -> PanelPlan {
        let bordered = width >= MIN_BORDERED_WIDTH && height >= MIN_BORDERED_HEIGHT;
        let assistant = if !bordered || self.assistant.member.is_none() {
            SidePlacement::Hidden
        } else if width >= ASSISTANT_RIGHT_BODY {
            SidePlacement::Right
        } else if height >= ASSISTANT_BOTTOM_BODY_HEIGHT {
            SidePlacement::Bottom
        } else {
            SidePlacement::Hidden
        };
        // The other panels are planned for what the assistant leaves at its narrowest.
        let (width, height) = match assistant {
            SidePlacement::Right => (width - ASSISTANT_MIN_WIDTH, height),
            SidePlacement::Bottom => (width, height - ASSISTANT_MIN_HEIGHT),
            SidePlacement::Hidden => (width, height),
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
        let guide = if !bordered || !guide_open {
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
            assistant,
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
        let guide_right = plan.guide == SidePlacement::Right;
        // The assistant's column first; the other panels share what it leaves.
        let assistant = self.assistant_column(plan, width);
        let assistant_max = width
            .saturating_sub(REVIEW_MIN_WIDTH)
            .max(ASSISTANT_MIN_WIDTH);
        let width_all = width;
        let width = width.saturating_sub(assistant.unwrap_or(0));
        let side_room = width.saturating_sub(REVIEW_MIN_WIDTH);
        let side_max = side_room.max(FILES_MIN_WIDTH);
        let mut files = plan.files.then(|| {
            let default = (width_all / 4).clamp(FILES_MIN_WIDTH, FILES_DEFAULT_MAX);
            self.shares
                .files
                .map_or(default, |share| cells_of(share, width_all))
                .clamp(FILES_MIN_WIDTH, side_max)
        });
        let right_column = self.right_column(plan, width_all, side_room);
        let right_min = right_column.map_or(FILES_MIN_WIDTH, |(_, _, min, _)| min);
        let mut right = right_column.map(|(_, cells, _, _)| cells);
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
        if let (Some((member, _, min, max)), Some(cells)) = (right_column, right) {
            members.push(member);
            lengths.push(cells);
            constraints.push(PanelGroupConstraints::new(min, Some(max)));
        }
        if let Some(cells) = assistant {
            members.push(PanelId::Assistant);
            lengths.push(cells);
            constraints.push(PanelGroupConstraints::new(
                ASSISTANT_MIN_WIDTH.min(width_all),
                Some(assistant_max),
            ));
        }
        self.root_members = members;
        let (column_lengths, column_constraints) = self.column_layout(plan, height);
        self.windows.set_tree(self.tree(plan));
        let windows = &self.windows;
        *self.root.borrow_mut() = windows.group_state(ROOT_GROUP, lengths, constraints);
        *self.column.borrow_mut() =
            windows.group_state(COLUMN_GROUP, column_lengths, column_constraints);
        let (side_lengths, side_constraints) = side_layout(height);
        *self.side.borrow_mut() = windows.group_state(SIDE_GROUP, side_lengths, side_constraints);
        plan
    }

    /// The panel to the right of the document (the guide, the threads, or both sharing a
    /// column) when `plan` puts one there: its member, its length in a body `width` wide, and
    /// its limits with `side_room` columns beside the document's minimum.
    fn right_column(
        &self,
        plan: PanelPlan,
        width: u16,
        side_room: u16,
    ) -> Option<(PanelId, u16, u16, u16)> {
        let guide_right = plan.guide == SidePlacement::Right;
        let member = match (guide_right, plan.threads == SidePlacement::Right) {
            (true, true) => PanelId::Side,
            (true, false) => PanelId::Guide,
            (false, true) => PanelId::Threads,
            (false, false) => return None,
        };
        let (share, default, min) = if guide_right {
            (
                self.shares.guide_right,
                GUIDE_DEFAULT_WIDTH,
                GUIDE_MIN_WIDTH,
            )
        } else {
            (
                self.shares.threads_right,
                THREADS_DEFAULT_WIDTH,
                FILES_MIN_WIDTH,
            )
        };
        let max = side_room.max(min);
        let cells = share
            .map_or(default, |share| cells_of(share, width))
            .clamp(min, max);
        Some((member, cells, min, max))
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
        let all_room = height.saturating_sub(MIN_PANE_ROWS);
        // The assistant first, then the guide and threads in what it leaves.
        let assistant = (plan.assistant == SidePlacement::Bottom).then(|| {
            self.assistant_length(height, false)
                .clamp(ASSISTANT_MIN_HEIGHT, all_room.max(ASSISTANT_MIN_HEIGHT))
                .min(height)
        });
        let room = all_room.saturating_sub(assistant.unwrap_or(0));
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
        if total(guide, None) > room {
            // Beside the assistant, not even the guide fits.
            guide = None;
        }
        let document = height.saturating_sub(total(guide, threads) + assistant.unwrap_or(0));
        lengths.push(document);
        constraints.push(PanelGroupConstraints::new(
            if guide.is_some() || threads.is_some() || assistant.is_some() {
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
        if let Some(cells) = assistant {
            members.push(PanelId::Assistant);
            lengths.push(cells);
            constraints.push(PanelGroupConstraints::new(
                ASSISTANT_MIN_HEIGHT.min(height),
                Some(all_room.max(ASSISTANT_MIN_HEIGHT)),
            ));
        }
        self.column_members = members;
        (lengths, constraints)
    }

    /// The assistant's column in a body `width` columns wide, when `plan` puts it beside the
    /// document: its size, leaving the document and a right panel their narrowest.
    fn assistant_column(&self, plan: PanelPlan, width: u16) -> Option<u16> {
        if plan.assistant != SidePlacement::Right {
            return None;
        }
        let right = if plan.guide == SidePlacement::Right {
            GUIDE_MIN_WIDTH
        } else if plan.threads == SidePlacement::Right {
            FILES_MIN_WIDTH
        } else {
            0
        };
        let max = width
            .saturating_sub(REVIEW_MIN_WIDTH + right)
            .max(ASSISTANT_MIN_WIDTH);
        Some(
            self.assistant_length(width, true)
                .clamp(ASSISTANT_MIN_WIDTH, max)
                .min(width),
        )
    }

    /// The assistant's preferred length along a body of `length` cells, beside the document
    /// (`right`) or under it: the size kept with this review when its size is per review, else
    /// (or before one was) the app's, else the configured one.
    fn assistant_length(&self, length: u16, right: bool) -> u16 {
        let Some(member) = self.assistant.member else {
            return 0;
        };
        let (own, app, configured) = if right {
            (
                self.shares.assistant_right,
                member.right_share,
                member.width,
            )
        } else {
            (
                self.shares.assistant_bottom,
                member.bottom_share,
                member.height,
            )
        };
        own.filter(|_| member.per_review).or(app).map_or_else(
            || configured.cells_of(length),
            |share| cells_of(share, length),
        )
    }

    /// The body's groups as laid out for `plan` (nothing without pane chrome): the root group,
    /// the review column (when panels sit under the document) and the right column (when the
    /// guide and the threads share it). The document fills, and stays expanded in, each group
    /// it is in.
    #[must_use]
    pub fn tree(&self, plan: PanelPlan) -> PanelTree<PanelId> {
        if !plan.bordered {
            return PanelTree::default();
        }
        let column = self.column_members.len() > 1;
        let root = self
            .root_members
            .iter()
            .map(|member| match member {
                PanelId::Review if column => PanelMember::Group(COLUMN_GROUP),
                PanelId::Side => PanelMember::Group(SIDE_GROUP),
                id => PanelMember::Panel(*id),
            })
            .collect();
        let review = self
            .root_members
            .iter()
            .position(|member| *member == PanelId::Review)
            .unwrap_or(0);
        let panels = |members: &[PanelId]| {
            members
                .iter()
                .map(|member| PanelMember::Panel(*member))
                .collect()
        };
        PanelTree::new(vec![
            PanelTreeGroup::new(PanelGroupAxis::Horizontal, root)
                .fill(review)
                .keep_one_expanded(true),
            PanelTreeGroup::new(PanelGroupAxis::Vertical, panels(&self.column_members))
                .fill(0)
                .keep_one_expanded(true),
            PanelTreeGroup::new(PanelGroupAxis::Vertical, panels(self.side_members())),
        ])
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
            let members = self.root_members.clone();
            for (index, (member, cells)) in members.iter().zip(root).enumerate() {
                if root_state.is_collapsed(index) {
                    continue;
                }
                match member {
                    PanelId::Files => self.shares.files = Some(share_of(cells, width)),
                    PanelId::Threads => self.shares.threads_right = Some(share_of(cells, width)),
                    PanelId::Guide | PanelId::Side => {
                        self.shares.guide_right = Some(share_of(cells, width));
                    }
                    PanelId::Assistant => {
                        self.note_assistant(AssistantSize::Right(share_of(cells, width)));
                    }
                    PanelId::Review => {}
                }
            }
        }
        let column_state = self.column.borrow().clone();
        if !review_collapsed(&self.column_members, &column_state) {
            let column = Self::lengths(PanelGroupAxis::Vertical, &column_state, width, height);
            let members = self.column_members.clone();
            for (index, (member, cells)) in members.iter().zip(column).enumerate().skip(1) {
                if column_state.is_collapsed(index) {
                    continue;
                }
                match member {
                    PanelId::Threads => {
                        self.shares.threads_bottom = Some(share_of(cells, height));
                    }
                    PanelId::Guide => self.shares.guide_bottom = Some(share_of(cells, height)),
                    PanelId::Assistant => {
                        self.note_assistant(AssistantSize::Bottom(share_of(cells, height)));
                    }
                    PanelId::Files | PanelId::Review | PanelId::Side => {}
                }
            }
        }
        before != self.shares
    }

    /// Keep a size the assistant took: with this review when its size is per review (then the
    /// review's state changes), else for the app, when it differs from the app's.
    fn note_assistant(&mut self, size: AssistantSize) {
        let Some(member) = self.assistant.member else {
            return;
        };
        match (size, member.per_review) {
            (AssistantSize::Right(share), true) => self.shares.assistant_right = Some(share),
            (AssistantSize::Bottom(share), true) => self.shares.assistant_bottom = Some(share),
            (AssistantSize::Right(share), false) if member.right_share != Some(share) => {
                self.assistant.size = Some(size);
            }
            (AssistantSize::Bottom(share), false) if member.bottom_share != Some(share) => {
                self.assistant.size = Some(size);
            }
            _ => {}
        }
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
        !matches!(
            self.plan_for(width, height, true).guide,
            SidePlacement::Hidden
        )
    }

    /// Open the guide panel (restored, when it was minimized).
    pub fn show_guide(&mut self) {
        self.guide_open = true;
        self.opened_or_closed(PanelId::Guide);
    }

    /// A panel was opened (it shows at its size, not minimized) or closed (it is no longer
    /// maximized, and opens at its size next time).
    fn opened_or_closed(&mut self, id: PanelId) {
        self.windows.forget(id);
        self.plan = None;
    }

    /// Lay the body out again at its size now (the window state changed).
    fn relayout(&mut self) {
        self.plan = None;
        let (width, height) = self.body;
        self.sync(width, height);
    }

    /// Close panel `id`; returns whether it closed. The review document never closes.
    pub fn close(&mut self, id: PanelId) -> bool {
        match id {
            PanelId::Files => self.files_choice = Some(false),
            PanelId::Threads => self.threads_open = false,
            PanelId::Guide => self.guide_open = false,
            // The assistant closes itself (it is the app's).
            PanelId::Review | PanelId::Side | PanelId::Assistant => return false,
        }
        self.opened_or_closed(id);
        true
    }

    /// Set the width of a minimized strip.
    pub fn set_strip_width(&mut self, cells: u16) {
        if self.windows.set_strip_width(cells) {
            self.plan = None;
        }
    }

    /// Whether panel `id` shows in the body as last laid out (nothing until the next layout
    /// once a panel opened or closed).
    #[must_use]
    pub fn shown(&self, id: PanelId) -> bool {
        self.plan.is_some_and(|plan| plan.bordered)
            && match id {
                PanelId::Side => self.root_members.contains(&PanelId::Side),
                id => self.windows.shown(id),
            }
    }

    /// The maximized panel, while it shows: the only panel the body lays out.
    #[must_use]
    pub fn maximized(&self) -> Option<PanelId> {
        self.windows.maximized().filter(|id| self.shown(*id))
    }

    /// How panel `id` is shown in the body as last laid out.
    #[must_use]
    pub fn shape(&self, id: PanelId) -> Shape {
        match self.windows.shape(id) {
            Shape::Maximized if self.maximized() != Some(id) => Shape::Normal,
            shape => shape,
        }
    }

    /// Restore a maximized panel; returns whether one was.
    pub fn restore_maximized(&mut self) -> bool {
        let restored = self.windows.restore_maximized();
        if restored {
            self.relayout();
        }
        restored
    }

    /// Run window command `command` on panel `id` (`count` times for a grow or shrink).
    /// Returns what it did, and whether a persisted part of the state changed.
    pub fn command(
        &mut self,
        id: PanelId,
        command: Command,
        count: Option<u32>,
    ) -> (WindowCommandOutcome, bool) {
        // Until the next layout after a panel opened or closed, no panel shows.
        if self.plan.is_none() {
            match command {
                Command::Maximize | Command::Minimize => {
                    return (
                        WindowCommandOutcome::Refused(WindowRefusal::NotShown),
                        false,
                    );
                }
                Command::Grow | Command::Shrink => {
                    return (
                        WindowCommandOutcome::Refused(WindowRefusal::NotResizable),
                        false,
                    );
                }
                Command::Close | Command::ResetSize => {}
            }
        }
        let delta = command.delta(count, crate::panel_controls::RESIZE_STEP);
        let outcome = self.windows.command(id, command, delta);
        let persist = match outcome {
            WindowCommandOutcome::Maximized(_) => {
                self.relayout();
                false
            }
            WindowCommandOutcome::Minimized(_) => {
                self.relayout();
                true
            }
            WindowCommandOutcome::Refused(_) => false,
            WindowCommandOutcome::CloseRequested => self.close(id),
            WindowCommandOutcome::Resize(resize) => self.apply_resize(resize),
            WindowCommandOutcome::ResetSizeRequested => self.reset_size(id),
        };
        (outcome, persist)
    }

    /// Grow (positive) or shrink panel `id` by `delta` cells, moving its edge toward the
    /// document (the document's own edge toward a panel beside or under it), within the same
    /// limits as dragging it. Returns whether a size changed. Minimized and maximized panels
    /// keep their size.
    pub fn resize(&mut self, id: PanelId, delta: i32) -> bool {
        self.shown(id)
            && self
                .windows
                .resize(id, delta)
                .is_some_and(|resize| self.apply_resize(resize))
    }

    /// Move a divider as `resize` asks. Returns whether a persisted size changed.
    fn apply_resize(&mut self, resize: PanelResize) -> bool {
        let (axis, state) = match resize.group {
            ROOT_GROUP => (PanelGroupAxis::Horizontal, &self.root),
            COLUMN_GROUP => (PanelGroupAxis::Vertical, &self.column),
            _ => (PanelGroupAxis::Vertical, &self.side),
        };
        let (width, height) = self.body;
        let outcome = group(axis, PanelGroupStyles::default()).resize_divider(
            Rect::new(0, 0, width, height),
            &mut state.borrow_mut(),
            resize.divider,
            resize.delta,
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
        let panels_column = |panels: &mut Self| panels.shares.guide_right = None;
        match id {
            PanelId::Files => files(self),
            // Sharing the right column with the guide, its width is the column's.
            PanelId::Threads if self.root_members.contains(&PanelId::Side) => {
                threads(self);
                panels_column(self);
            }
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
            PanelId::Assistant => {
                self.shares.assistant_right = None;
                self.shares.assistant_bottom = None;
                if !self
                    .assistant
                    .member
                    .is_some_and(|member| member.per_review)
                {
                    self.assistant.size = Some(AssistantSize::Reset);
                }
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

    fn assistant(per_review: bool) -> AssistantMember {
        AssistantMember {
            minimized: false,
            maximized: false,
            right_share: None,
            bottom_share: None,
            width: DockLength::Percent(40),
            height: DockLength::Percent(40),
            per_review,
        }
    }

    #[test]
    fn the_assistant_has_the_far_right_column_and_the_others_plan_for_what_it_leaves() {
        let mut panels = Panels::restore(&ReviewPanelState {
            threads_panel_visible: Some(true),
            guide_panel_visible: Some(true),
            ..ReviewPanelState::default()
        });
        panels.set_assistant(Some(assistant(false)));
        for width in [ASSISTANT_RIGHT_BODY, 120, 170, 200, 260] {
            let widths = root_widths(&mut panels, width, 40);
            assert_eq!(widths.iter().sum::<u16>(), width, "{width}: {widths:?}");
            let members = panels.root_members().to_vec();
            assert_eq!(
                members.last(),
                Some(&PanelId::Assistant),
                "{width}: {members:?}"
            );
            let review = members.iter().position(|member| *member == PanelId::Review);
            assert!(
                widths[review.unwrap()] >= REVIEW_MIN_WIDTH,
                "{width}: {widths:?}"
            );
            assert!(
                *widths.last().unwrap() >= ASSISTANT_MIN_WIDTH,
                "{width}: {widths:?}"
            );
        }
        // The threads and guide panels move under the document before the assistant does.
        let plan = panels.plan(ASSISTANT_RIGHT_BODY, 40);
        assert_eq!(plan.assistant, SidePlacement::Right);
        assert_eq!(plan.threads, SidePlacement::Bottom);
        assert_eq!(plan.guide, SidePlacement::Bottom);
        // Narrower, the assistant moves under the document too; then out of the body.
        let plan = panels.plan(ASSISTANT_RIGHT_BODY - 1, 40);
        assert_eq!(plan.assistant, SidePlacement::Bottom);
        panels.sync(ASSISTANT_RIGHT_BODY - 1, 40);
        assert_eq!(panels.column_members().last(), Some(&PanelId::Assistant));
        let heights = Panels::lengths(
            PanelGroupAxis::Vertical,
            &panels.column.borrow(),
            ASSISTANT_RIGHT_BODY - 1,
            40,
        );
        assert_eq!(heights.iter().sum::<u16>(), 40, "{heights:?}");
        assert!(
            *heights.last().unwrap() >= ASSISTANT_MIN_HEIGHT,
            "{heights:?}"
        );
        assert_eq!(
            panels
                .plan(ASSISTANT_RIGHT_BODY - 1, ASSISTANT_BOTTOM_BODY_HEIGHT - 1)
                .assistant,
            SidePlacement::Hidden
        );
        assert_eq!(
            panels.plan(MIN_BORDERED_WIDTH - 1, 40).assistant,
            SidePlacement::Hidden
        );
        // Not shown on the review: nothing of it is laid out.
        panels.set_assistant(None);
        panels.sync(200, 40);
        assert!(!panels.root_members().contains(&PanelId::Assistant));
        assert_eq!(panels.plan(200, 40).assistant, SidePlacement::Hidden);
    }

    #[test]
    fn the_guide_fits_where_the_plan_beside_the_assistant_has_room_for_it() {
        let mut panels = Panels::default();
        panels.set_assistant(Some(assistant(false)));
        // Under the document with the assistant: the guide needs its rows on top of its own.
        panels.sync(80, ASSISTANT_MIN_HEIGHT + GUIDE_BOTTOM_BODY_HEIGHT);
        assert!(panels.guide_fits());
        panels.sync(80, ASSISTANT_MIN_HEIGHT + GUIDE_BOTTOM_BODY_HEIGHT - 1);
        assert!(!panels.guide_fits());
        panels.set_assistant(None);
        panels.sync(80, ASSISTANT_MIN_HEIGHT + GUIDE_BOTTOM_BODY_HEIGHT - 1);
        assert!(panels.guide_fits());
    }

    #[test]
    fn the_assistant_takes_the_apps_size_or_keeps_its_own_with_the_review() {
        // The app's size: a resize is handed back, and nothing is saved with the review.
        let mut panels = Panels::default();
        let member = AssistantMember {
            right_share: Some(share_of(50, 200)),
            ..assistant(false)
        };
        panels.set_assistant(Some(member));
        assert_eq!(*root_widths(&mut panels, 200, 40).last().unwrap(), 50);
        assert!(panels.resize(PanelId::Assistant, 10));
        assert_eq!(
            panels.take_assistant_size(),
            Some(AssistantSize::Right(share_of(60, 200)))
        );
        assert_eq!(panels.take_assistant_size(), None, "taken once");
        assert_eq!(panels.state().assistant_right_share, None);
        assert!(!panels.reset_size(PanelId::Assistant));
        assert_eq!(panels.take_assistant_size(), Some(AssistantSize::Reset));

        // Kept with the review: saved, restored, and preferred over the app's size.
        let mut panels = Panels::default();
        panels.set_assistant(Some(AssistantMember {
            per_review: true,
            ..member
        }));
        root_widths(&mut panels, 200, 40);
        assert!(panels.resize(PanelId::Assistant, 10));
        assert_eq!(panels.take_assistant_size(), None);
        let state = panels.state();
        assert_eq!(state.assistant_right_share, Some(share_of(60, 200)));
        let mut restored = Panels::restore(&state);
        restored.set_assistant(Some(AssistantMember {
            per_review: true,
            ..member
        }));
        assert_eq!(*root_widths(&mut restored, 200, 40).last().unwrap(), 60);
        assert!(restored.reset_size(PanelId::Assistant));
        assert_eq!(restored.take_assistant_size(), None);
        // Without a size of its own, the review takes the app's.
        assert_eq!(*root_widths(&mut restored, 200, 40).last().unwrap(), 50);
    }

    #[test]
    fn the_assistant_window_state_follows_the_app() {
        let mut panels = Panels::default();
        panels.set_assistant(Some(AssistantMember {
            minimized: true,
            ..assistant(false)
        }));
        panels.sync(200, 40);
        assert_eq!(panels.shape(PanelId::Assistant), Shape::Strip);
        panels.set_assistant(Some(AssistantMember {
            maximized: true,
            ..assistant(false)
        }));
        panels.sync(200, 40);
        assert_eq!(panels.maximized(), Some(PanelId::Assistant));
        assert_eq!(panels.shape(PanelId::Assistant), Shape::Maximized);
        panels.set_assistant(Some(assistant(false)));
        panels.sync(200, 40);
        assert_eq!(panels.maximized(), None);
        assert_eq!(panels.shape(PanelId::Assistant), Shape::Normal);
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
