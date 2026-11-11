//! The command palette and the help overlay (spec 6.2), both generated from the keymap.
//!
//! * The palette lists every declared V2 review action with its effective keys in a BMUX
//!   `picker_frame` with a filter input over a BMUX `menu`; typing filters (every word must occur
//!   in the description or the action id), `enter` or a click runs the action.
//! * Help is the same menu without the input: every action of the review, composer, and tree
//!   contexts grouped under section rows, scrollable, and runnable.
//! * A menu is the same list without the input for a chosen set of actions (the actions of a
//!   thread or an AI answer, or the structured AI commands).
//! * A palette query that is a line number offers "Go to line N" first (the jump-to-line of
//!   inventory INV-NAV-13, by source line number).
//!
//! The overlay keeps the layout it was painted with; pointer events are routed through exactly
//! that layout.

use super::theme::ReviewTheme;
use bcode_plugin_sdk::tui::keymap::KeymapAction;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::component::{Component, Constraints, EventCx, LayoutCx, LayoutId, LayoutNode};
use bmux_tui::event::Event;
use bmux_tui::geometry::{Rect, Size};
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::text::{Line, Span};
use bmux_tui_components::menu::{MenuComponent, MenuItem, MenuOutcome, MenuPolicy, MenuState};
use bmux_tui_components::picker_frame::{PickerFrame, PickerFrameComponent, PickerFramePolicy};
use bmux_tui_components::text_input::{
    TextInputComponent, TextInputControl, TextInputOutcome, TextInputPolicy, TextInputState,
};
use std::cell::{Cell, RefCell};

const FRAME_ID: &str = "review.overlay";
const MENU_ID: &str = "review.overlay.menu";
const INPUT_ID: &str = "review.overlay.input";

/// One runnable entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayEntry {
    /// Action id without the `bcode.code_review.` prefix; `None` for a section row.
    pub action: Option<String>,
    /// Description.
    pub label: String,
    /// Effective keys, joined.
    pub keys: String,
}

/// Entries of `actions` for one scope: description and effective keys.
#[must_use]
pub fn entries(
    actions: &[KeymapAction],
    keys: impl Fn(&str) -> Vec<String>,
    section: Option<&str>,
) -> Vec<OverlayEntry> {
    let mut out = Vec::new();
    if let Some(section) = section {
        out.push(OverlayEntry {
            action: None,
            label: section.to_owned(),
            keys: String::new(),
        });
    }
    out.extend(actions.iter().map(|action| {
        OverlayEntry {
            action: Some(
                action
                    .id
                    .strip_prefix("bcode.code_review.")
                    .unwrap_or(&action.id)
                    .to_owned(),
            ),
            label: action.description.clone(),
            keys: keys(&action.id).join(" "),
        }
    }));
    out
}

/// Kind of overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    /// The command palette.
    Palette,
    /// Keyboard help.
    Help,
    /// A menu of chosen actions.
    Menu,
}

/// Action id prefix of the palette's go-to-line entry (followed by the line number).
pub const GOTO_LINE_PREFIX: &str = "goto_line:";

/// What an input did to the overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayOutcome {
    /// Close the overlay.
    Close,
    /// Close the overlay and run an action.
    Run(String),
    /// The overlay changed.
    Redraw,
    /// Not handled (the overlay still captures input).
    Ignored,
}

/// The open palette or help overlay.
pub struct Overlay {
    /// Kind.
    pub kind: OverlayKind,
    entries: Vec<OverlayEntry>,
    shown: Vec<usize>,
    items: Vec<MenuItem>,
    input: RefCell<TextInputState>,
    policy: TextInputPolicy,
    menu: Cell<MenuState>,
    outcome: Cell<MenuOutcome>,
    painted: Option<(Rect, LayoutNode)>,
    /// Item the keyboard is on.
    active: Option<usize>,
}

impl std::fmt::Debug for Overlay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Overlay")
            .field("kind", &self.kind)
            .field("shown", &self.shown.len())
            .finish_non_exhaustive()
    }
}

impl Overlay {
    /// An overlay over `entries`.
    #[must_use]
    pub fn new(kind: OverlayKind, entries: Vec<OverlayEntry>, theme: &ReviewTheme) -> Self {
        let mut policy = TextInputPolicy::chat_composer();
        policy.keyboard.shift_enter = None;
        policy.viewport.max_rows = Some(1);
        policy.edge = bmux_tui_components::text_input::EdgePolicy::disabled();
        let mut overlay = Self {
            kind,
            entries,
            shown: Vec::new(),
            items: Vec::new(),
            input: RefCell::new(TextInputState::new(bmux_text_edit::TextEditBuffer::new())),
            policy,
            menu: Cell::new(MenuState::new(None)),
            outcome: Cell::new(MenuOutcome::Ignored),
            painted: None,
            active: None,
        };
        overlay.filter(theme);
        overlay
    }

    /// Labels of the entries shown, in order (section rows included).
    #[must_use]
    pub fn shown_labels(&self) -> Vec<&str> {
        self.shown
            .iter()
            .map(|index| self.entries[*index].label.as_str())
            .collect()
    }

    /// Action ids of the runnable rows shown, in order (the go-to-line row included).
    #[must_use]
    pub fn shown_actions(&self) -> Vec<&str> {
        self.items
            .iter()
            .filter(|item| !item.section)
            .map(|item| item.id.as_str())
            .collect()
    }

    fn filter(&mut self, theme: &ReviewTheme) {
        let query = self.input.borrow().buffer().text().to_lowercase();
        let line = query
            .trim()
            .trim_start_matches(':')
            .parse::<u32>()
            .ok()
            .filter(|line| *line > 0);
        let query = if line.is_some() { String::new() } else { query };
        let words: Vec<&str> = query.split_whitespace().collect();
        self.shown = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                line.is_none()
                    && (words.is_empty()
                        || entry.action.as_ref().is_some_and(|action| {
                            let haystack = format!("{} {action}", entry.label.to_lowercase());
                            words.iter().all(|word| haystack.contains(word))
                        }))
            })
            .map(|(index, _)| index)
            .collect();
        self.items = self
            .shown
            .iter()
            .map(|index| {
                let entry = &self.entries[*index];
                entry.action.as_ref().map_or_else(
                    || {
                        let mut item = MenuItem::rich(
                            format!("section.{index}"),
                            Line::from_spans(vec![Span::styled(entry.label.clone(), theme.title)]),
                        );
                        item.section = true;
                        item
                    },
                    |action| {
                        MenuItem::rich(
                            action.clone(),
                            Line::from_spans(vec![
                                Span::styled(entry.label.clone(), theme.text),
                                Span::raw("  "),
                                Span::styled(entry.keys.clone(), theme.muted),
                            ]),
                        )
                    },
                )
            })
            .collect();
        if let Some(line) = line {
            self.items.insert(
                0,
                MenuItem::rich(
                    format!("{GOTO_LINE_PREFIX}{line}"),
                    Line::from_spans(vec![
                        Span::styled(format!("Go to line {line}"), theme.accent),
                        Span::raw("  "),
                        Span::styled("in this file", theme.muted),
                    ]),
                ),
            );
        }
        self.menu.set(MenuState::new(None));
        self.active = None;
        let first = self.items.iter().position(|item| !item.section);
        self.activate(first);
    }

    /// Put the keyboard on item `index`, scrolling it into the menu's rows.
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

    fn focused_action(&self) -> Option<String> {
        self.items
            .get(self.active?)
            .filter(|item| !item.section)
            .map(|item| item.id.clone())
    }

    fn menu_component(&self, theme: &ReviewTheme) -> MenuComponent<'_, '_> {
        MenuComponent::new(MENU_ID, &self.items, &self.menu)
            .policy(MenuPolicy::interactive())
            .styles(theme.component.selectable_list_styles())
            .outcome(&self.outcome)
    }

    fn frame<'a>(&'a self, theme: &ReviewTheme, area: Size) -> PickerFrameComponent<'a> {
        let (title, footer) = match self.kind {
            OverlayKind::Palette => ("Commands", "enter run · esc close"),
            OverlayKind::Help => ("Keys", "enter run · esc close"),
            OverlayKind::Menu => ("Actions", "enter run · esc close"),
        };
        let max = Size::new(
            area.width.saturating_sub(4).min(84),
            area.height.saturating_sub(2),
        );
        let mut policy = PickerFramePolicy::palette().max_size(max);
        policy.input = self.kind == OverlayKind::Palette;
        policy.header = false;
        policy.margin = bmux_tui::geometry::Insets::all(1);
        policy.min_size = Size::new(area.width.min(20), area.height.min(5));
        let frame = PickerFrame::new()
            .title(title)
            .footer(Line::from_spans(vec![Span::styled(footer, theme.muted)]))
            .policy(policy)
            .styles(theme.component.picker_frame_styles())
            .chrome(theme.component.chrome);
        let component = PickerFrameComponent::new(FRAME_ID, frame, self.menu_component(theme));
        if self.kind == OverlayKind::Palette {
            component.input(
                TextInputComponent::new(INPUT_ID, &self.input, &self.policy)
                    .style(theme.input)
                    .placeholder("type to filter commands", theme.placeholder)
                    .focused(true),
            )
        } else {
            component
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
    }

    /// Handle a key.
    pub fn key(&mut self, stroke: KeyStroke, theme: &ReviewTheme) -> OverlayOutcome {
        let plain = stroke.modifiers.is_empty();
        match stroke.key {
            KeyCode::Escape if plain => return OverlayOutcome::Close,
            KeyCode::Char('?') if self.kind == OverlayKind::Help => return OverlayOutcome::Close,
            KeyCode::Enter if plain => {
                return self
                    .focused_action()
                    .map_or(OverlayOutcome::Ignored, OverlayOutcome::Run);
            }
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End => return self.navigate(stroke),
            KeyCode::Char('j' | 'k') if self.kind != OverlayKind::Palette && plain => {
                let key = if stroke.key == KeyCode::Char('j') {
                    KeyCode::Down
                } else {
                    KeyCode::Up
                };
                return self.navigate(KeyStroke::simple(key));
            }
            _ => {}
        }
        if self.kind != OverlayKind::Palette {
            return OverlayOutcome::Ignored;
        }
        let outcome =
            TextInputControl::new(&self.policy).handle_key(&mut self.input.borrow_mut(), stroke);
        match outcome {
            TextInputOutcome::Edited => {
                self.filter(theme);
                OverlayOutcome::Redraw
            }
            TextInputOutcome::Ignored => OverlayOutcome::Ignored,
            _ => OverlayOutcome::Redraw,
        }
    }

    /// Insert pasted text into the palette filter.
    pub fn paste(&mut self, text: &str, theme: &ReviewTheme) -> OverlayOutcome {
        if self.kind != OverlayKind::Palette {
            return OverlayOutcome::Ignored;
        }
        TextInputControl::new(&self.policy).handle_paste(&mut self.input.borrow_mut(), text);
        self.filter(theme);
        OverlayOutcome::Redraw
    }

    fn menu_area(&self) -> Option<Rect> {
        let (area, layout) = self.painted.as_ref()?;
        let rect = layout.find_rect(&LayoutId::new(MENU_ID))?;
        Some(Rect::new(
            area.x + rect.x,
            area.y + rect.y,
            rect.width,
            rect.height,
        ))
    }

    fn navigate(&mut self, stroke: KeyStroke) -> OverlayOutcome {
        let selectable: Vec<usize> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| !item.section)
            .map(|(index, _)| index)
            .collect();
        if selectable.is_empty() {
            return OverlayOutcome::Ignored;
        }
        let page = self
            .menu_area()
            .map_or(10, |area| usize::from(area.height.max(1)));
        let current = self
            .active
            .and_then(|active| selectable.iter().position(|index| *index == active))
            .unwrap_or(0);
        let last = selectable.len() - 1;
        let next = match stroke.key {
            KeyCode::Up => current.saturating_sub(1),
            KeyCode::Down => (current + 1).min(last),
            KeyCode::PageUp => current.saturating_sub(page),
            KeyCode::PageDown => (current + page).min(last),
            KeyCode::Home => 0,
            KeyCode::End => last,
            _ => return OverlayOutcome::Ignored,
        };
        self.activate(Some(selectable[next]));
        OverlayOutcome::Redraw
    }

    /// Route a pointer event through the painted layout. A press outside the panel closes it.
    pub fn mouse(&mut self, event: &Event, theme: &ReviewTheme) -> OverlayOutcome {
        let Some((area, layout)) = self.painted.clone() else {
            return OverlayOutcome::Ignored;
        };
        let Event::Mouse(mouse) = event else {
            return OverlayOutcome::Ignored;
        };
        // The panel: the menu and input rows with the frame's padding and border around them
        // (and its footer row below), all from the painted layout.
        let rect_of = |id: &'static str| {
            layout
                .find_rect(&LayoutId::new(id))
                .map(|rect| Rect::new(area.x + rect.x, area.y + rect.y, rect.width, rect.height))
        };
        let panel = [rect_of(MENU_ID), rect_of(INPUT_ID)]
            .into_iter()
            .flatten()
            .reduce(|a, b| {
                let x = a.x.min(b.x);
                let y = a.y.min(b.y);
                Rect::new(
                    x,
                    y,
                    a.right().max(b.right()) - x,
                    a.bottom().max(b.bottom()) - y,
                )
            })
            .map(|inner| {
                let x = inner.x.saturating_sub(2).max(area.x);
                let y = inner.y.saturating_sub(2).max(area.y);
                Rect::new(
                    x,
                    y,
                    (inner.right() + 2).min(area.right()) - x,
                    (inner.bottom() + 3).min(area.bottom()) - y,
                )
            });
        if matches!(
            mouse.kind,
            bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left)
        ) && panel.is_some_and(|panel| !panel.contains(mouse.position))
        {
            return OverlayOutcome::Close;
        }
        self.outcome.set(MenuOutcome::Ignored);
        let outcome = {
            let frame = self.frame(theme, area.size());
            let mut cx = EventCx::new(&layout);
            cx.with_transform(0, 0, i32::from(area.x), i64::from(area.y), area, |cx| {
                frame.event(event, &layout, cx)
            })
        };
        match self.outcome.replace(MenuOutcome::Ignored) {
            MenuOutcome::Activated { id, .. } => OverlayOutcome::Run(id),
            _ if outcome == bmux_tui::event::EventOutcome::Ignored => OverlayOutcome::Ignored,
            _ => OverlayOutcome::Redraw,
        }
    }
}
