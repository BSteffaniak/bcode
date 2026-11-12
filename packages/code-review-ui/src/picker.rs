//! A picker for the new-review wizard: a BMUX `picker_frame` with an optional query input over a
//! BMUX `menu` of one-row items (or a BMUX `empty_state` when there is nothing to pick).
//!
//! The owner supplies the items (filtered locally or answered by a paged service search) and
//! reacts to [`PickerOutcome`]s. The picker keeps the layout it was painted with; pointer events
//! route through exactly that layout, and keyboard navigation scrolls the focused row into the
//! menu rows it was painted with.

use super::theme::ReviewTheme;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::component::{Component, Constraints, EventCx, LayoutCx, LayoutId, LayoutNode};
use bmux_tui::event::{Event, EventOutcome};
use bmux_tui::geometry::{Insets, Rect, Size};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::text::{Line, Span};
use bmux_tui_components::empty_state::{EmptyStateComponent, EmptyStatePolicy};
use bmux_tui_components::menu::{
    MenuComponent, MenuItem, MenuNavigation, MenuOutcome, MenuPolicy, MenuState,
};
use bmux_tui_components::picker_frame::{
    PickerFrame, PickerFrameComponent, PickerFramePlacement, PickerFramePolicy,
};
use bmux_tui_components::text_input::{
    EdgePolicy, TextInputComponent, TextInputControl, TextInputOutcome, TextInputPolicy,
    TextInputState,
};
use std::cell::{Cell, RefCell};

/// Rows from the end of the loaded items at which [`PickerOutcome::NearEnd`] is reported.
const NEAR_END_ROWS: usize = 8;

/// One pickable row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerItem {
    /// Caller id.
    pub id: String,
    /// Row content.
    pub line: Line,
    /// A section heading (not pickable).
    pub section: bool,
}

impl PickerItem {
    /// A pickable row.
    #[must_use]
    pub fn new(id: impl Into<String>, line: Line) -> Self {
        Self {
            id: id.into(),
            line,
            section: false,
        }
    }

    /// A section heading.
    #[must_use]
    pub fn section(id: impl Into<String>, line: Line) -> Self {
        Self {
            id: id.into(),
            line,
            section: true,
        }
    }
}

/// What an input did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerOutcome {
    /// Not handled.
    Ignored,
    /// Visual state changed.
    Redraw,
    /// The query text changed.
    Edited,
    /// A row was picked (`enter` or a click).
    Picked(String),
    /// `esc`.
    Cancel,
    /// The focus came within a few rows of the last item (load the next page).
    NearEnd,
}

/// The picker's empty or error state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerMessage {
    /// Heading.
    pub title: String,
    /// Body text.
    pub body: String,
    /// Whether it reports a failure.
    pub error: bool,
}

/// A query input over a menu of items.
pub struct Picker {
    id: &'static str,
    title: String,
    footer: String,
    placeholder: String,
    items: Vec<PickerItem>,
    menu_items: Vec<MenuItem>,
    input: Option<RefCell<TextInputState>>,
    policy: TextInputPolicy,
    menu: Cell<MenuState>,
    outcome: Cell<MenuOutcome>,
    painted: Option<(Rect, LayoutNode)>,
    active: Option<usize>,
    message: Option<PickerMessage>,
    body: Vec<Line>,
}

impl std::fmt::Debug for Picker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Picker")
            .field("id", &self.id)
            .field("items", &self.items.len())
            .field("active", &self.active)
            .finish_non_exhaustive()
    }
}

impl Picker {
    /// A picker titled `title`; `input` adds a query input with `placeholder`.
    #[must_use]
    pub fn new(id: &'static str, title: impl Into<String>, input: Option<&str>) -> Self {
        let mut policy = TextInputPolicy::chat_composer();
        policy.keyboard.shift_enter = None;
        policy.viewport.max_rows = Some(1);
        policy.edge = EdgePolicy::disabled();
        Self {
            id,
            title: title.into(),
            footer: String::new(),
            placeholder: input.unwrap_or_default().to_owned(),
            items: Vec::new(),
            menu_items: Vec::new(),
            input: input
                .map(|_| RefCell::new(TextInputState::new(bmux_text_edit::TextEditBuffer::new()))),
            policy,
            menu: Cell::new(MenuState::new(None)),
            outcome: Cell::new(MenuOutcome::Ignored),
            painted: None,
            active: None,
            message: None,
            body: Vec::new(),
        }
    }

    /// Set the footer hint.
    pub fn set_footer(&mut self, footer: impl Into<String>) {
        self.footer = footer.into();
    }

    /// Set the query text (moving the caret to its end).
    pub fn set_query(&mut self, text: &str) {
        if let Some(input) = &self.input {
            *input.borrow_mut() =
                TextInputState::new(bmux_text_edit::TextEditBuffer::from_text(text));
        }
    }

    /// The query text.
    #[must_use]
    pub fn query(&self) -> String {
        self.input
            .as_ref()
            .map(|input| input.borrow().buffer().text().to_owned())
            .unwrap_or_default()
    }

    /// The items shown.
    #[must_use]
    pub fn items(&self) -> &[PickerItem] {
        &self.items
    }

    /// Id of the focused item.
    #[must_use]
    pub fn focused(&self) -> Option<&str> {
        self.items
            .get(self.active?)
            .filter(|item| !item.section)
            .map(|item| item.id.as_str())
    }

    /// Show `message` instead of the items (or clear it).
    pub fn set_message(&mut self, message: Option<PickerMessage>, theme: &ReviewTheme) {
        self.body = message
            .as_ref()
            .map(|message| {
                vec![Line::from_spans(vec![Span::styled(
                    message.body.clone(),
                    if message.error {
                        theme.error
                    } else {
                        theme.muted
                    },
                )])]
            })
            .unwrap_or_default();
        self.message = message;
    }

    /// The message shown instead of the items.
    #[must_use]
    pub const fn message(&self) -> Option<&PickerMessage> {
        self.message.as_ref()
    }

    /// Replace the items, keeping the focus on the same id when it is still present (else the
    /// first pickable item).
    pub fn set_items(&mut self, items: Vec<PickerItem>) {
        let focused = self.focused().map(str::to_owned);
        self.items = items;
        self.menu_items = self
            .items
            .iter()
            .map(|item| {
                let mut menu = MenuItem::rich(item.id.clone(), item.line.clone());
                menu.section = item.section;
                menu
            })
            .collect();
        let keep = focused.and_then(|id| {
            self.items
                .iter()
                .position(|item| !item.section && item.id == id)
        });
        let first = self.items.iter().position(|item| !item.section);
        if keep.is_none() {
            let mut state = self.menu.get();
            state.list.set_vertical_scroll(0);
            self.menu.set(state);
        }
        self.activate(keep.or(first));
    }

    /// Put the focus on the item with `id`.
    pub fn focus_id(&mut self, id: &str) {
        if let Some(index) = self
            .items
            .iter()
            .position(|item| !item.section && item.id == id)
        {
            self.activate(Some(index));
        }
    }

    fn activate(&mut self, index: Option<usize>) {
        self.active = index;
        let rows = self
            .menu_area()
            .map_or(10, |area| u64::from(area.height.max(1)));
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

    fn menu_area(&self) -> Option<Rect> {
        let (area, layout) = self.painted.as_ref()?;
        let rect = layout.find_rect(&LayoutId::new(self.menu_id()))?;
        Some(Rect::new(
            area.x + rect.x,
            area.y + rect.y,
            rect.width,
            rect.height,
        ))
    }

    fn menu_id(&self) -> String {
        format!("{}.menu", self.id)
    }

    fn input_id(&self) -> String {
        format!("{}.input", self.id)
    }

    fn frame<'a>(&'a self, theme: &ReviewTheme, area: Size) -> PickerFrameComponent<'a> {
        // The panel fits its rows (border and padding, the input and its gap, the rows or the
        // message, a spacer, and the footer), within the area and at most 120 columns wide.
        let rows = if self.message.is_some() {
            6
        } else {
            u16::try_from(self.items.len()).unwrap_or(u16::MAX).max(3)
        };
        let input_rows = if self.input.is_some() { 2 } else { 0 };
        let height = rows.saturating_add(4 + input_rows + 2).min(area.height);
        let width = area.width.min(120);
        let mut policy = PickerFramePolicy::palette()
            .placement(PickerFramePlacement::UpperThird)
            .max_size(Size::new(width, height));
        policy.input = self.input.is_some();
        policy.header = false;
        policy.margin = Insets::all(0);
        policy.min_size = Size::new(width, height);
        let frame = PickerFrame::new()
            .title(&self.title)
            .footer(Line::from_spans(vec![Span::styled(
                self.footer.clone(),
                theme.muted,
            )]))
            .policy(policy)
            .styles(theme.component.picker_frame_styles())
            .chrome(theme.component.chrome);
        let component = if let Some(message) = self.message.as_ref() {
            let empty =
                EmptyStateComponent::new(format!("{}.empty", self.id), message.title.as_str())
                    .icon(if message.error {
                        "!"
                    } else {
                        theme.icons.ellipsis
                    })
                    .body(&self.body)
                    .policy(EmptyStatePolicy::centered())
                    .styles(theme.component.empty_state_styles());
            PickerFrameComponent::new(self.id, frame, empty)
        } else {
            // The picker keeps one active row for keys and pointer alike, so a click on the
            // active row picks it like a click on any other.
            let mut policy = MenuPolicy::interactive();
            policy.navigation = MenuNavigation::ActiveItem;
            let menu = MenuComponent::new(self.menu_id(), &self.menu_items, &self.menu)
                .policy(policy)
                .styles(theme.component.selectable_list_styles())
                .outcome(&self.outcome);
            PickerFrameComponent::new(self.id, frame, menu)
        };
        match &self.input {
            Some(input) => component.input(
                TextInputComponent::new(self.input_id(), input, &self.policy)
                    .style(theme.input)
                    .placeholder(&self.placeholder, theme.placeholder)
                    .focused(true),
            ),
            None => component,
        }
    }

    /// Paint over `area` and keep the layout for pointer routing.
    pub fn paint(&mut self, theme: &ReviewTheme, area: Rect, cx: &mut PaintCx<'_, '_>) {
        if area.width == 0 || area.height == 0 {
            self.painted = None;
            return;
        }
        let layout = {
            let frame = self.frame(theme, area.size());
            let layout = frame.layout(Constraints::tight(area.size()), &mut LayoutCx::new());
            cx.with_child(
                i32::from(area.x),
                i64::from(area.y),
                LocalRect::new(0, 0, area.width, area.height),
                |cx| frame.paint(&layout, cx),
            );
            layout
        };
        self.painted = Some((area, layout));
        // The menu rows are known now: keep the focused row inside them.
        self.activate(self.active);
    }

    /// Handle a key.
    pub fn key(&mut self, stroke: KeyStroke) -> PickerOutcome {
        let plain = stroke.modifiers.is_empty();
        match stroke.key {
            KeyCode::Escape if plain => return PickerOutcome::Cancel,
            KeyCode::Enter if plain => {
                return self.focused().map_or(PickerOutcome::Ignored, |id| {
                    PickerOutcome::Picked(id.to_owned())
                });
            }
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End
                if plain || stroke.modifiers.shift =>
            {
                return self.navigate(stroke.key);
            }
            _ => {}
        }
        let Some(input) = &self.input else {
            return match stroke.key {
                KeyCode::Char('j') if plain => self.navigate(KeyCode::Down),
                KeyCode::Char('k') if plain => self.navigate(KeyCode::Up),
                _ => PickerOutcome::Ignored,
            };
        };
        let before = input.borrow().buffer().text().to_owned();
        let outcome =
            TextInputControl::new(&self.policy).handle_key(&mut input.borrow_mut(), stroke);
        match outcome {
            TextInputOutcome::Ignored => PickerOutcome::Ignored,
            _ if input.borrow().buffer().text() != before.as_str() => PickerOutcome::Edited,
            _ => PickerOutcome::Redraw,
        }
    }

    /// Insert pasted text into the query.
    pub fn paste(&mut self, text: &str) -> PickerOutcome {
        let Some(input) = &self.input else {
            return PickerOutcome::Ignored;
        };
        TextInputControl::new(&self.policy).handle_paste(&mut input.borrow_mut(), text);
        PickerOutcome::Edited
    }

    fn navigate(&mut self, key: KeyCode) -> PickerOutcome {
        let selectable: Vec<usize> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| !item.section)
            .map(|(index, _)| index)
            .collect();
        if selectable.is_empty() {
            return PickerOutcome::Ignored;
        }
        let page = self
            .menu_area()
            .map_or(10, |area| usize::from(area.height.max(1)));
        let current = self
            .active
            .and_then(|active| selectable.iter().position(|index| *index == active))
            .unwrap_or(0);
        let last = selectable.len() - 1;
        let next = match key {
            KeyCode::Up => current.saturating_sub(1),
            KeyCode::Down => (current + 1).min(last),
            KeyCode::PageUp => current.saturating_sub(page),
            KeyCode::PageDown => (current + page).min(last),
            KeyCode::Home => 0,
            KeyCode::End => last,
            _ => return PickerOutcome::Ignored,
        };
        self.activate(Some(selectable[next]));
        if next + NEAR_END_ROWS >= selectable.len() {
            PickerOutcome::NearEnd
        } else {
            PickerOutcome::Redraw
        }
    }

    /// Route a pointer event through the painted layout.
    pub fn mouse(&mut self, event: &Event, theme: &ReviewTheme) -> PickerOutcome {
        let Some((area, layout)) = self.painted.clone() else {
            return PickerOutcome::Ignored;
        };
        if !matches!(event, Event::Mouse(_)) {
            return PickerOutcome::Ignored;
        }
        self.outcome.set(MenuOutcome::Ignored);
        let outcome = {
            let frame = self.frame(theme, area.size());
            let mut cx = EventCx::new(&layout);
            cx.with_transform(0, 0, i32::from(area.x), i64::from(area.y), area, |cx| {
                frame.event(event, &layout, cx)
            })
        };
        let focused = self.menu.get().focused();
        if focused != self.active && focused.is_some_and(|index| !self.items[index].section) {
            self.active = focused;
        }
        match self.outcome.replace(MenuOutcome::Ignored) {
            MenuOutcome::Activated { id, .. } => PickerOutcome::Picked(id),
            _ if outcome == EventOutcome::Ignored => PickerOutcome::Ignored,
            _ => {
                let near_end = self
                    .active
                    .is_some_and(|active| active + NEAR_END_ROWS >= self.items.len());
                if near_end {
                    PickerOutcome::NearEnd
                } else {
                    PickerOutcome::Redraw
                }
            }
        }
    }
}
