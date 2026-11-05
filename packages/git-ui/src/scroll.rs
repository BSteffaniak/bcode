//! Scroll positions of the lists, kept apart from their cursors.
//!
//! Lists scroll like pages: the wheel and the scrollbars move what is shown, never the cursor,
//! and a list scrolls to its cursor only when the cursor moves (see `view::list_policy`). Each
//! paint records every list's scroll geometry from the laid-out frame; pointer and keyboard
//! scrolling use that record instead of deriving geometry again.

use std::cell::Cell;
use std::collections::BTreeMap;

use bmux_tui::component::{EventCx, LayoutId, LayoutNode};
use bmux_tui::event::Event;
use bmux_tui::geometry::Rect;
use bmux_tui_components::pane::Pane;
use bmux_tui_components::scroll_view::{
    ScrollExtent, ScrollView, ScrollViewOutcome, TrackScrollbar,
};
use bmux_tui_components::selectable_list::{SelectableListState, list_viewport_id};
use bmux_tui_runtime::Update;

use crate::app::{App, Msg, Window};
use crate::popup::Popup;

/// The command log's list.
pub const LOG_LIST: &str = "command-log";
/// A menu popup's list.
pub const MENU_LIST: &str = "popup-list";
/// A text popup's lines.
pub const TEXT_LIST: &str = "popup-text";

/// The list of a side window.
#[must_use]
pub fn side_list(window: Window) -> String {
    format!("side-{}", window.number())
}

/// The list of a main view section.
#[must_use]
pub fn main_list(section: usize) -> String {
    format!("main-{section}")
}

/// Scroll geometry of one list as last painted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Painted {
    /// Content and visible heights.
    pub extent: ScrollExtent,
    /// Scrollbar track, in terminal coordinates.
    pub track: Option<Rect>,
    /// First row of each item (main view sections only, for hunk navigation).
    pub tops: Vec<u64>,
}

/// Record the scroll geometry of every list laid out in `layout`. Pane lists have their
/// scrollbar on the pane's border; popup lists overlay theirs on the list's last column.
#[must_use]
pub fn capture(app: &App, layout: &LayoutNode, placed: &EventCx<'_>) -> BTreeMap<String, Painted> {
    let popup_scroll = ScrollView::new()
        .policy(crate::view::list_base_policy(crate::view::POPUP_SCROLLBAR).scroll_view_policy());
    let panes = app
        .side_windows()
        .map(|window| (side_list(window), Some(format!("pane-{}", window.number()))))
        .chain((0..2).map(|section| (main_list(section), Some(format!("pane-main-{section}")))))
        .chain([(LOG_LIST.to_owned(), Some("pane-log".to_owned()))]);
    let popups = [MENU_LIST, TEXT_LIST].map(|list| (list.to_owned(), None));
    panes
        .chain(popups)
        .filter_map(|(list, pane)| {
            let id = LayoutId::new(list.clone());
            let viewport = layout.find(&list_viewport_id(&id))?;
            let track = pane.map_or_else(
                || {
                    placed
                        .find_visible_rect(&id)
                        .and_then(|area| popup_scroll.scrollbar_area(area))
                },
                |pane| {
                    placed
                        .find_visible_rect(&LayoutId::new(format!("{pane}.surface")))
                        .and_then(|area| Pane::new().border_scrollbar_track(area))
                },
            );
            let tops = if list.starts_with("main-") {
                viewport.children.first().map_or_else(Vec::new, |content| {
                    content.node.children.iter().map(|item| item.y).collect()
                })
            } else {
                Vec::new()
            };
            Some((
                list,
                Painted {
                    extent: ScrollExtent::of(viewport),
                    track,
                    tops,
                },
            ))
        })
        .collect()
}

impl App {
    /// The state holding a list's scroll position.
    pub(crate) fn list_state(&self, id: &str) -> Option<&Cell<SelectableListState>> {
        if let Some(number) = id.strip_prefix("side-") {
            let index = number.parse::<usize>().ok()?.checked_sub(1)?;
            let view = self.view(Window::Side(index));
            return self.lists.get(&view);
        }
        if let Some(section) = id.strip_prefix("main-") {
            return self.main_lists.get(section.parse::<usize>().ok()?);
        }
        match (id, self.popups.last()) {
            (LOG_LIST, _) => Some(&self.log_list),
            (MENU_LIST, Some(Popup::Menu(menu))) => Some(&menu.list),
            (TEXT_LIST, Some(Popup::Text(view))) => Some(&view.list),
            _ => None,
        }
    }

    fn painted(&self, id: &str) -> Option<Painted> {
        self.painted.borrow().get(id).cloned()
    }

    /// Scroll a list by `delta` rows within its painted extent. Returns whether it moved.
    pub(crate) fn scroll_list(&self, id: &str, delta: i64) -> bool {
        let (Some(state), Some(painted)) = (self.list_state(id), self.painted(id)) else {
            return false;
        };
        let mut list = state.get();
        let moved = painted.extent.scroll_by(&mut list.scroll, delta) != ScrollViewOutcome::Ignored;
        state.set(list);
        moved
    }

    /// Scroll a list to `offset`, clamped to its painted extent.
    pub(crate) fn scroll_list_to(&self, id: &str, offset: u64) {
        let Some(state) = self.list_state(id) else {
            return;
        };
        let maximum = self
            .painted(id)
            .map_or(offset, |painted| painted.extent.max_offset());
        let mut list = state.get();
        list.scroll.set_vertical_offset(offset.min(maximum));
        state.set(list);
    }

    /// Current scroll offset of a list (for tests and automation).
    #[must_use]
    pub fn list_scroll(&self, id: &str) -> u64 {
        self.list_state(id)
            .map_or(0, |state| state.get().scroll.vertical_offset())
    }

    /// Whether a list is scrolled to its end.
    pub(crate) fn list_at_end(&self, id: &str) -> bool {
        self.painted(id)
            .is_some_and(|painted| self.list_scroll(id) >= painted.extent.max_offset())
    }

    /// Visible rows of a list as last painted (at least one).
    pub(crate) fn list_page(&self, id: &str) -> i64 {
        self.painted(id).map_or(1, |painted| {
            i64::try_from(painted.extent.viewport)
                .unwrap_or(i64::MAX)
                .max(1)
        })
    }

    /// Scroll a main section so the next (or previous) hunk starts the page.
    pub(crate) fn scroll_to_hunk(&self, section: usize, direction: isize) {
        let id = main_list(section);
        let Some(painted) = self.painted(&id) else {
            return;
        };
        let Some(rows) = self
            .main
            .as_ref()
            .and_then(|main| main.sections.get(section))
            .map(|section| &section.rows)
        else {
            return;
        };
        let offset = self.list_scroll(&id).min(painted.extent.max_offset());
        let header = |index: &usize| rows[*index].kind == crate::diffview::RowKind::HunkHeader;
        let top = |index: usize| painted.tops.get(index).copied();
        let count = rows.len().min(painted.tops.len());
        let target = if direction > 0 {
            (0..count)
                .filter(header)
                .find(|index| top(*index).is_some_and(|row| row > offset))
        } else {
            (0..count)
                .rev()
                .filter(header)
                .find(|index| top(*index).is_some_and(|row| row < offset))
        };
        if let Some(row) = target.and_then(top) {
            self.scroll_list_to(&id, row);
        }
    }

    /// Scrollbar dragging and track clicks. A drag in progress keeps the pointer; otherwise
    /// only the open popup's list, or with no popup the panes' lists, can take it.
    pub(crate) fn route_scrollbars(&self, event: &Event) -> Option<Update<Msg>> {
        let painted = self.painted.borrow().clone();
        let popup = !self.popups.is_empty();
        let dragging = painted.keys().find(|id| {
            self.list_state(id)
                .is_some_and(|state| state.get().scroll.dragging_scrollbar())
        });
        let candidates: Vec<&String> = dragging.map_or_else(
            || {
                painted
                    .keys()
                    .filter(|id| (id.as_str() == MENU_LIST || id.as_str() == TEXT_LIST) == popup)
                    .collect()
            },
            |id| vec![id],
        );
        for id in candidates {
            let (Some(state), Some(geometry)) = (self.list_state(id), painted.get(id)) else {
                continue;
            };
            let Some(track) = geometry.track else {
                continue;
            };
            let mut list = state.get();
            let held = list.scroll.dragging_scrollbar();
            let outcome =
                TrackScrollbar::new().handle_event(track, geometry.extent, &mut list.scroll, event);
            state.set(list);
            if outcome != ScrollViewOutcome::Ignored {
                return Some(Update::redraw());
            }
            if held || list.scroll.dragging_scrollbar() {
                return Some(Update::none());
            }
        }
        None
    }
}

/// Rows scrolled per wheel notch.
pub const WHEEL_ROWS: i64 = 3;

/// The list a hit region belongs to: a list row, the list itself, or its pane's chrome.
#[must_use]
pub fn list_under(hit: &str) -> Option<String> {
    let head = hit.split('.').next()?;
    if head.starts_with("side-") || head.starts_with("main-") || head == LOG_LIST {
        return Some(head.to_owned());
    }
    if head == "pane-log" {
        return Some(LOG_LIST.to_owned());
    }
    if let Some(section) = head.strip_prefix("pane-main-") {
        return Some(format!("main-{section}"));
    }
    let number = head
        .strip_prefix("pane-")
        .or_else(|| head.strip_prefix("tabs-"))?;
    number.parse::<usize>().ok()?;
    Some(format!("side-{number}"))
}
