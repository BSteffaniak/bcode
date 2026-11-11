//! Panel layout of the review body (spec section 6.1) and its narrow-width degradation.
//!
//! The body is a BMUX `panel_group`: the file panel, the review document, and the optional
//! threads panel (to the right when wide, under the document otherwise), each a bordered BMUX
//! `pane` whose border shows focus. Dividers sit on the panes' shared borders and can be
//! dragged; sizes and visibility are kept in [`ReviewPanelState`] (presentation state V2).
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

use bcode_code_review_models::ReviewPanelState;
use bmux_tui_components::panel_group::{
    PanelDividerMode, PanelGroup, PanelGroupAxis, PanelGroupConstraints, PanelGroupPolicy,
    PanelGroupResizePolicy, PanelGroupState, PanelSize,
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

/// Where the threads panel is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadsPlacement {
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
    pub threads: ThreadsPlacement,
}

/// Panel visibility, sizes, and the BMUX group states.
#[derive(Debug)]
pub struct Panels {
    /// Horizontal group: files, the review column, threads on the right.
    pub root: RefCell<PanelGroupState>,
    /// Vertical group of the review column: document, threads at the bottom.
    pub column: RefCell<PanelGroupState>,
    /// Panels in `root`, in order.
    root_members: Vec<PanelId>,
    /// Whether `column` holds the threads panel.
    column_threads: bool,
    /// The user's file panel choice; `None` follows the width.
    pub files_choice: Option<bool>,
    /// File panel width chosen by resizing.
    pub files_width: Option<u16>,
    /// Whether the user opened the threads panel.
    pub threads_open: bool,
    /// Threads panel size chosen by resizing (columns to the right, rows at the bottom).
    pub threads_size: Option<u16>,
    plan: Option<PanelPlan>,
    body: (u16, u16),
}

impl Default for Panels {
    fn default() -> Self {
        Self {
            root: RefCell::new(PanelGroupState::new(Vec::new())),
            column: RefCell::new(PanelGroupState::new(Vec::new())),
            root_members: Vec::new(),
            column_threads: false,
            files_choice: None,
            files_width: None,
            threads_open: false,
            threads_size: None,
            plan: None,
            body: (0, 0),
        }
    }
}

impl Panels {
    /// Panels restored from presentation state.
    #[must_use]
    pub fn restore(state: &ReviewPanelState) -> Self {
        Self {
            files_choice: state.file_panel_visible,
            files_width: state.file_panel_width,
            threads_open: state.threads_panel_visible.unwrap_or(false),
            threads_size: state.threads_panel_size,
            ..Self::default()
        }
    }

    /// Presentation state of the panels.
    #[must_use]
    pub const fn state(&self) -> ReviewPanelState {
        ReviewPanelState {
            file_panel_visible: self.files_choice,
            file_panel_width: self.files_width,
            threads_panel_visible: if self.threads_open { Some(true) } else { None },
            threads_panel_size: self.threads_size,
        }
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
            ThreadsPlacement::Hidden
        } else if width >= THREADS_RIGHT_BODY {
            ThreadsPlacement::Right
        } else if height >= THREADS_BOTTOM_BODY_HEIGHT {
            ThreadsPlacement::Bottom
        } else {
            ThreadsPlacement::Hidden
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

    /// Whether the column group holds the threads panel.
    #[must_use]
    pub const fn column_threads(&self) -> bool {
        self.column_threads
    }

    /// Bring the group states in line with the plan for a `width` x `height` body.
    pub fn sync(&mut self, width: u16, height: u16) -> PanelPlan {
        let plan = self.plan(width, height);
        if self.plan == Some(plan) && self.body == (width, height) {
            return plan;
        }
        self.plan = Some(plan);
        self.body = (width, height);
        let mut members = Vec::new();
        let mut sizes = Vec::new();
        let mut constraints = Vec::new();
        let side_room = width.saturating_sub(REVIEW_MIN_WIDTH);
        if plan.files {
            let default = (width / 4).clamp(FILES_MIN_WIDTH, FILES_DEFAULT_MAX);
            let max = side_room.max(FILES_MIN_WIDTH);
            members.push(PanelId::Files);
            sizes.push(PanelSize::fixed(
                self.files_width
                    .unwrap_or(default)
                    .clamp(FILES_MIN_WIDTH, max),
            ));
            constraints.push(PanelGroupConstraints::new(FILES_MIN_WIDTH, Some(max)));
        }
        members.push(PanelId::Review);
        sizes.push(PanelSize::flex(1));
        constraints.push(PanelGroupConstraints::new(
            REVIEW_MIN_WIDTH.min(width),
            None,
        ));
        if plan.threads == ThreadsPlacement::Right {
            let max = side_room.max(FILES_MIN_WIDTH);
            members.push(PanelId::Threads);
            sizes.push(PanelSize::fixed(
                self.threads_size
                    .unwrap_or(THREADS_DEFAULT_WIDTH)
                    .clamp(FILES_MIN_WIDTH, max),
            ));
            constraints.push(PanelGroupConstraints::new(FILES_MIN_WIDTH, Some(max)));
        }
        let mut root = PanelGroupState::new(sizes);
        root.set_constraints(constraints);
        *self.root.borrow_mut() = root;
        self.root_members = members;
        self.column_threads = plan.threads == ThreadsPlacement::Bottom;
        let mut column = if self.column_threads {
            let max = height.saturating_sub(MIN_PANE_ROWS).max(THREADS_MIN_HEIGHT);
            PanelGroupState::new([
                PanelSize::flex(1),
                PanelSize::fixed(
                    self.threads_size
                        .unwrap_or(THREADS_DEFAULT_HEIGHT)
                        .clamp(THREADS_MIN_HEIGHT, max),
                ),
            ])
        } else {
            PanelGroupState::new([PanelSize::flex(1)])
        };
        column.set_constraints(column_constraints(self.column_threads, height));
        *self.column.borrow_mut() = column;
        plan
    }

    /// Record sizes after a divider drag. Returns whether a persisted size changed.
    pub fn note_resize(&mut self) -> bool {
        let before = (self.files_width, self.threads_size);
        {
            let root = self.root.borrow();
            for (member, size) in self.root_members.iter().zip(root.sizes()) {
                let PanelSize::Fixed(cells) = *size else {
                    continue;
                };
                match member {
                    PanelId::Files => self.files_width = Some(cells),
                    PanelId::Threads => self.threads_size = Some(cells),
                    PanelId::Review => {}
                }
            }
        }
        if self.column_threads
            && let Some(PanelSize::Fixed(rows)) = self.column.borrow().sizes().get(1).copied()
        {
            self.threads_size = Some(rows);
        }
        before != (self.files_width, self.threads_size)
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

    /// Grow (positive) or shrink the file panel by `delta` columns.
    pub fn resize_files(&mut self, delta: i32) -> bool {
        let (width, _) = self.body;
        let current = self
            .root_members
            .iter()
            .position(|member| *member == PanelId::Files)
            .and_then(|index| self.root.borrow().sizes().get(index).copied());
        let Some(PanelSize::Fixed(cells)) = current else {
            return false;
        };
        let max = width.saturating_sub(REVIEW_MIN_WIDTH).max(FILES_MIN_WIDTH);
        let next = i32::from(cells)
            .saturating_add(delta)
            .clamp(i32::from(FILES_MIN_WIDTH), i32::from(max));
        let next = u16::try_from(next).unwrap_or(cells);
        if next == cells {
            return false;
        }
        self.files_width = Some(next);
        self.plan = None;
        true
    }
}

fn column_constraints(threads: bool, height: u16) -> Vec<PanelGroupConstraints> {
    if threads {
        let max = height.saturating_sub(MIN_PANE_ROWS).max(THREADS_MIN_HEIGHT);
        vec![
            PanelGroupConstraints::new(MIN_PANE_ROWS, None),
            PanelGroupConstraints::new(THREADS_MIN_HEIGHT, Some(max)),
        ]
    } else {
        vec![PanelGroupConstraints::new(0, None)]
    }
}

/// A resizable group whose dividers sit on the panes' shared borders.
///
/// Side by side the handle covers the right pane's left border; stacked it covers the upper
/// pane's bottom border (the lower pane's top border holds its title).
#[must_use]
pub const fn group(
    axis: PanelGroupAxis,
    styles: bmux_tui_components::panel_group::PanelGroupStyles,
) -> PanelGroup {
    let (before, after) = match axis {
        PanelGroupAxis::Horizontal => (0, 1),
        PanelGroupAxis::Vertical => (1, 0),
    };
    PanelGroup::new(axis)
        .policy(PanelGroupPolicy {
            resize: PanelGroupResizePolicy::live(),
            ..PanelGroupPolicy::resize_only()
        })
        .divider_mode(PanelDividerMode::Overlay { before, after })
        .styles(styles)
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
        assert_eq!(panels.plan(200, 40).threads, ThreadsPlacement::Hidden);
        panels.toggle_threads();
        assert_eq!(panels.plan(200, 40).threads, ThreadsPlacement::Right);
        assert_eq!(panels.plan(120, 40).threads, ThreadsPlacement::Bottom);
        assert_eq!(panels.plan(120, 10).threads, ThreadsPlacement::Hidden);
    }

    #[test]
    fn file_panel_width_is_clamped_and_round_trips_through_presentation_state() {
        let mut panels = Panels::restore(&ReviewPanelState {
            file_panel_visible: Some(true),
            file_panel_width: Some(500),
            threads_panel_visible: None,
            threads_panel_size: None,
        });
        panels.sync(120, 30);
        let sizes = panels.root.borrow().sizes().to_vec();
        assert_eq!(sizes[0], PanelSize::fixed(120 - REVIEW_MIN_WIDTH));
        assert!(panels.resize_files(-10));
        panels.sync(120, 30);
        assert_eq!(
            panels.root.borrow().sizes()[0],
            PanelSize::fixed(120 - REVIEW_MIN_WIDTH - 10)
        );
        let state = panels.state();
        assert_eq!(state.file_panel_visible, Some(true));
        assert_eq!(state.file_panel_width, Some(120 - REVIEW_MIN_WIDTH - 10));
        assert_eq!(Panels::restore(&state).state(), state);
    }
}
