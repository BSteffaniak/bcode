//! Shared pieces of the review's modal flows (publish and import).
//!
//! A keyboard-driven BMUX `menu` with an active item, and painting and pointer routing of a
//! BMUX `picker_frame` through exactly the layout it was painted with.

use super::theme::ReviewTheme;
use bmux_keyboard::KeyCode;
use bmux_tui::component::{Component, Constraints, EventCx, LayoutCx, LayoutId, LayoutNode};
use bmux_tui::event::{Event, EventOutcome};
use bmux_tui::geometry::{Rect, Size};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::text::{Line, Span};
use bmux_tui_components::menu::{MenuComponent, MenuItem, MenuOutcome, MenuPolicy, MenuState};
use bmux_tui_components::picker_frame::{PickerFrame, PickerFramePolicy};
use std::cell::Cell;

/// A menu whose active item the keyboard moves.
pub struct MenuPanel {
    id: &'static str,
    items: Vec<MenuItem>,
    menu: Cell<MenuState>,
    outcome: Cell<MenuOutcome>,
    active: Option<usize>,
    /// Rows the menu had when last painted (for paging and scrolling into view).
    rows: Cell<u16>,
}

impl std::fmt::Debug for MenuPanel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MenuPanel")
            .field("id", &self.id)
            .field("items", &self.items.len())
            .field("active", &self.active)
            .finish_non_exhaustive()
    }
}

impl MenuPanel {
    /// An empty menu with layout id `id`.
    #[must_use]
    pub const fn new(id: &'static str) -> Self {
        Self {
            id,
            items: Vec::new(),
            menu: Cell::new(MenuState::new(None)),
            outcome: Cell::new(MenuOutcome::Ignored),
            active: None,
            rows: Cell::new(10),
        }
    }

    /// Replace the items, keeping the active item by id when it still exists (else the first
    /// selectable item).
    pub fn set_items(&mut self, items: Vec<MenuItem>) {
        let keep = self.active_id().map(str::to_owned);
        self.items = items;
        self.menu.set(MenuState::new(None));
        let index = keep
            .and_then(|id| {
                self.items
                    .iter()
                    .position(|item| item.id == id && !item.section)
            })
            .or_else(|| self.items.iter().position(|item| !item.section));
        self.activate(index);
    }

    /// The items.
    #[must_use]
    pub fn items(&self) -> &[MenuItem] {
        &self.items
    }

    /// Id of the active item.
    #[must_use]
    pub fn active_id(&self) -> Option<&str> {
        self.items
            .get(self.active?)
            .filter(|item| !item.section)
            .map(|item| item.id.as_str())
    }

    /// Make the item with `id` active.
    pub fn activate_id(&mut self, id: &str) {
        let index = self.items.iter().position(|item| item.id == id);
        if index.is_some() {
            self.activate(index);
        }
    }

    fn activate(&mut self, index: Option<usize>) {
        self.active = index;
        let rows = u64::from(self.rows.get().max(1));
        let mut state = self.menu.get();
        state.list.set_focused(index);
        state.list.set_selected(index);
        if let Some(index) = index {
            let index = index as u64;
            let scroll = state.list.vertical_scroll();
            if index < scroll {
                state.list.set_vertical_scroll(index);
            } else if index >= scroll + rows {
                state.list.set_vertical_scroll(index + 1 - rows);
            }
        }
        self.menu.set(state);
    }

    /// Move the active item with an arrow, page, home, or end key (`j`/`k` too). Returns
    /// whether the key was a navigation key.
    pub fn navigate(&mut self, key: KeyCode) -> bool {
        let selectable: Vec<usize> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| !item.section)
            .map(|(index, _)| index)
            .collect();
        let page = usize::from(self.rows.get().max(1));
        let current = self
            .active
            .and_then(|active| selectable.iter().position(|index| *index == active))
            .unwrap_or(0);
        let last = selectable.len().saturating_sub(1);
        let next = match key {
            KeyCode::Up | KeyCode::Char('k') => current.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => (current + 1).min(last),
            KeyCode::PageUp => current.saturating_sub(page),
            KeyCode::PageDown => (current + page).min(last),
            KeyCode::Home => 0,
            KeyCode::End => last,
            _ => return false,
        };
        if let Some(index) = selectable.get(next) {
            self.activate(Some(*index));
        }
        true
    }

    /// The BMUX menu component.
    #[must_use]
    pub fn component(&self, theme: &ReviewTheme) -> MenuComponent<'_, '_> {
        MenuComponent::new(self.id, &self.items, &self.menu)
            .policy(MenuPolicy::interactive())
            .styles(theme.component.selectable_list_styles())
            .outcome(&self.outcome)
    }

    /// Remember the painted menu height from `layout`.
    pub fn note_layout(&self, layout: &LayoutNode) {
        if let Some(rect) = layout.find_rect(&LayoutId::new(self.id)) {
            self.rows.set(rect.height);
        }
    }

    /// The item a pointer event activated, if any (cleared on read).
    pub fn take_activated(&mut self) -> Option<String> {
        match self.outcome.replace(MenuOutcome::Ignored) {
            MenuOutcome::Activated { index, id } => {
                self.active = Some(index);
                Some(id)
            }
            MenuOutcome::Focused(index) => {
                self.active = Some(index);
                None
            }
            _ => None,
        }
    }
}

/// A picker frame titled `title` with a footer of key hints, sized for `area`.
#[must_use]
pub fn frame<'a>(
    theme: &ReviewTheme,
    title: &'a str,
    footer: &'a str,
    area: Size,
) -> PickerFrame<'a> {
    let max = Size::new(
        area.width.saturating_sub(4).min(96),
        area.height.saturating_sub(2).min(32),
    );
    let mut policy = PickerFramePolicy::palette().max_size(max);
    policy.input = false;
    policy.header = false;
    policy.margin = bmux_tui::geometry::Insets::all(1);
    policy.min_size = Size::new(area.width.min(24), area.height.min(8));
    PickerFrame::new()
        .title(title)
        .footer(Line::from_spans(vec![Span::styled(footer, theme.muted)]))
        .policy(policy)
        .styles(theme.component.picker_frame_styles())
        .chrome(theme.component.chrome)
}

/// Paint `component` over `area`; returns the layout it was painted with.
pub fn paint(component: &dyn Component, area: Rect, cx: &mut PaintCx<'_, '_>) -> LayoutNode {
    let layout = component.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
    cx.with_child(
        i32::from(area.x),
        i64::from(area.y),
        LocalRect::new(0, 0, area.width, area.height),
        |cx| component.paint(&layout, cx),
    );
    layout
}

/// Route `event` through `component` laid out as `layout` at `area`.
pub fn route(
    component: &dyn Component,
    area: Rect,
    layout: &LayoutNode,
    event: &Event,
) -> EventOutcome {
    let mut cx = EventCx::new(layout);
    cx.with_transform(0, 0, i32::from(area.x), i64::from(area.y), area, |cx| {
        component.event(event, layout, cx)
    })
}
