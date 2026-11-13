//! The model switcher: one modal for choosing a model, wherever Bcode asks for one.
//!
//! Rows are grouped by source (a native tool, a credential connection, a provider) beneath
//! the current model, favorites, and recent choices. Typing searches across every source.
//! The selected row's details, reasoning effort, and any fix it needs appear beside the list.
//!
//! The switcher owns presentation and interaction only. Hosts feed it discovery reports and
//! act on its [`Outcome`]s: applying a choice, approving a connection, opening setup, or
//! saving recent and favorite choices.

pub mod discovery;
pub mod inventory;
mod view;
pub use view::SwitcherTheme;

#[cfg(test)]
mod tests;

use bcode_config::model_choices::ModelChoices;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::component::LayoutNode;
use bmux_tui::event::{Event, MouseButton, MouseEventKind};
use bmux_tui::geometry::Rect;
use bmux_tui_components::selectable_list::SelectableListState;
use bmux_tui_components::text_input::{
    EdgePolicy, TextInputControl, TextInputOutcome, TextInputPolicy, TextInputState,
};
use inventory::{Access, CurrentChoice, Entry, Fix, Inventory, Row, SourceUpdate};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

/// What the user decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing changed.
    Ignored,
    /// Presentation changed.
    Redraw,
    /// Leave without choosing.
    Close,
    /// Use this model.
    Choose(Box<Choice>),
    /// Approve these credentials, then rediscover.
    Connect(Box<bcode_provider_auth_models::external::ExternalAuthCandidate>),
    /// Open native tool setup.
    Setup,
    /// Persist changed recent or favorite choices.
    SaveChoices(ModelChoices),
}

/// A chosen model and the reasoning effort picked for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// The chosen row.
    pub entry: Entry,
    /// Reasoning effort picked in the switcher, when the model supports one.
    pub effort: Option<String>,
}

/// Which view of the inventory is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Grouped list with details.
    Browse,
    /// One table of every model with sortable columns.
    Compare,
}

/// A short message about the last action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// Message text.
    pub text: String,
    /// Whether it reports a failure.
    pub error: bool,
}

/// Layout retained from the last paint, used for pointer routing and paging.
#[derive(Debug, Clone)]
struct Painted {
    area: Rect,
    layout: LayoutNode,
}

/// The switcher's state.
pub struct ModelSwitcher {
    inventory: Inventory,
    current: CurrentChoice,
    choices: ModelChoices,
    query: RefCell<TextInputState>,
    input_policy: TextInputPolicy,
    rows: Vec<Row>,
    active: Option<String>,
    /// The user placed the selection; until then it follows the best default as sources report.
    placed: bool,
    list: Cell<SelectableListState>,
    efforts: BTreeMap<String, String>,
    current_effort: Option<String>,
    layout: Layout,
    notice: Option<Notice>,
    painted: Option<Painted>,
    started: std::time::Instant,
    compare: view::CompareState,
    keymap: crate::keymap::BmuxKeyMap,
}

impl std::fmt::Debug for ModelSwitcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelSwitcher")
            .field("rows", &self.rows.len())
            .field("active", &self.active)
            .field("layout", &self.layout)
            .finish_non_exhaustive()
    }
}

/// Keymap scope of the switcher's actions.
pub const KEYMAP_SCOPE: &str = "model_switcher";

impl ModelSwitcher {
    /// An empty switcher for `current`, with the user's saved `choices` and key bindings.
    /// Discovery fills it.
    #[must_use]
    pub fn new(
        current: CurrentChoice,
        choices: ModelChoices,
        keymap: crate::keymap::BmuxKeyMap,
    ) -> Self {
        let mut input_policy = TextInputPolicy::chat_composer();
        input_policy.keyboard.shift_enter = None;
        input_policy.viewport.max_rows = Some(1);
        input_policy.edge = EdgePolicy::disabled();
        let mut switcher = Self {
            inventory: Inventory::default(),
            current,
            choices,
            query: RefCell::new(TextInputState::new(bmux_text_edit::TextEditBuffer::new())),
            input_policy,
            rows: Vec::new(),
            active: None,
            placed: false,
            list: Cell::new(SelectableListState::new(None)),
            efforts: BTreeMap::new(),
            current_effort: None,
            layout: Layout::Browse,
            notice: None,
            painted: None,
            started: std::time::Instant::now(),
            compare: view::CompareState::default(),
            keymap,
        };
        switcher.refresh();
        switcher
    }

    /// Effort the conversation currently uses; preselected for the current model.
    pub fn set_current_effort(&mut self, effort: Option<String>) {
        self.current_effort = effort;
    }

    /// Apply one discovery report.
    pub fn apply(&mut self, update: SourceUpdate) {
        self.inventory.apply(update);
        self.refresh();
    }

    /// Show a message about the last action.
    pub fn notify(&mut self, notice: Notice) {
        self.notice = Some(notice);
    }

    /// Whether any source is still being checked.
    #[must_use]
    pub fn loading(&self) -> bool {
        self.inventory.loading_any()
    }

    /// The model the conversation currently uses.
    #[must_use]
    pub const fn current(&self) -> &CurrentChoice {
        &self.current
    }

    /// The saved recent and favorite choices.
    #[must_use]
    pub const fn choices(&self) -> &ModelChoices {
        &self.choices
    }

    /// The inventory discovered so far.
    #[must_use]
    pub const fn inventory(&self) -> &Inventory {
        &self.inventory
    }

    /// The entry under the selection, if any.
    #[must_use]
    pub fn active_entry(&self) -> Option<&Entry> {
        let id = self.active.as_deref()?;
        self.rows.iter().find_map(|row| match row {
            Row::Choice {
                id: row_id, entry, ..
            } if row_id == id => self.inventory.entries().get(*entry),
            _ => None,
        })
    }

    fn query_text(&self) -> String {
        self.query.borrow().buffer().text().to_owned()
    }

    /// Rebuild rows, keeping the selection on the same row (or the same model) when it remains.
    fn refresh(&mut self) {
        let previous_key = self.active_entry().map(|entry| entry.key.clone());
        self.rows = self.inventory.rows(
            &self.query_text(),
            &self.current,
            &self.choices.favorites,
            &self.choices.recent,
        );
        let keeps_row = self.placed
            && self.active.as_ref().is_some_and(|active| {
                self.rows
                    .iter()
                    .any(|row| matches!(row, Row::Choice { id, .. } if id == active))
            });
        if keeps_row {
            return;
        }
        let by_key = previous_key.filter(|_| self.placed).and_then(|key| {
            self.rows.iter().find_map(|row| match row {
                Row::Choice { id, entry, .. } if self.inventory.entries()[*entry].key == key => {
                    Some(id.clone())
                }
                _ => None,
            })
        });
        let searching = !self.query_text().trim().is_empty();
        self.active = by_key.or_else(|| {
            // Start on the current model; otherwise (and while searching) on the best ready
            // choice.
            let choices = || {
                self.rows.iter().filter_map(|row| match row {
                    Row::Choice {
                        id, entry, current, ..
                    } => Some((id, *entry, *current)),
                    Row::Heading { .. } => None,
                })
            };
            choices()
                .find(|(_, _, current)| *current && !searching)
                .or_else(|| {
                    choices().find(|(_, entry, _)| {
                        self.inventory.entries()[*entry].readiness.selectable()
                    })
                })
                .or_else(|| choices().next())
                .map(|(id, _, _)| id.clone())
        });
    }

    fn choice_ids(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter_map(|row| match row {
                Row::Choice { id, .. } => Some(id.clone()),
                Row::Heading { .. } => None,
            })
            .collect()
    }

    fn move_selection(&mut self, key: KeyCode) -> Outcome {
        let ids = self.choice_ids();
        if ids.is_empty() {
            return Outcome::Ignored;
        }
        let page = self
            .painted
            .as_ref()
            .and_then(|painted| view::list_rows(&painted.layout))
            .unwrap_or(10)
            .max(1);
        let current = self
            .active
            .as_ref()
            .and_then(|active| ids.iter().position(|id| id == active))
            .unwrap_or(0);
        let last = ids.len() - 1;
        let next = match key {
            KeyCode::Up => current.saturating_sub(1),
            KeyCode::Down => (current + 1).min(last),
            KeyCode::PageUp => current.saturating_sub(page),
            KeyCode::PageDown => (current + page).min(last),
            KeyCode::Home => 0,
            KeyCode::End => last,
            _ => return Outcome::Ignored,
        };
        self.active = Some(ids[next].clone());
        self.placed = true;
        Outcome::Redraw
    }

    /// Effort values of `entry`'s model, in order, with the one picked for it.
    fn efforts_of(&self, entry: &Entry) -> Option<(Vec<String>, String)> {
        // Native tools choose their own effort; Bcode sets it only for its provider routes.
        if matches!(
            entry.model.route,
            bcode_model::execution::ExecutionRoute::ManagedHarness { .. }
        ) {
            return None;
        }
        let reasoning = entry.model.model.reasoning.as_ref()?;
        let values = bcode_model::ordered_reasoning_effort_values(&reasoning.effort_values);
        if values.is_empty() {
            return None;
        }
        let current = self.current.route.is_some()
            && self.current.model_id.as_deref() == Some(entry.model.model.model_id.as_str());
        let picked = self
            .efforts
            .get(&entry.key)
            .cloned()
            .or_else(|| current.then(|| self.current_effort.clone()).flatten())
            .or_else(|| reasoning.default_effort.clone())
            .filter(|value| values.contains(value))
            .unwrap_or_else(|| values[values.len() / 2].clone());
        Some((values, picked))
    }

    fn step_effort(&mut self, forward: bool) -> Outcome {
        let Some(entry) = self.active_entry().cloned() else {
            return Outcome::Ignored;
        };
        let Some((values, picked)) = self.efforts_of(&entry) else {
            return Outcome::Ignored;
        };
        let index = values
            .iter()
            .position(|value| *value == picked)
            .unwrap_or(0);
        let next = if forward {
            (index + 1).min(values.len() - 1)
        } else {
            index.saturating_sub(1)
        };
        self.efforts.insert(entry.key, values[next].clone());
        Outcome::Redraw
    }

    /// Set the effort of the selected model, as a click on its effort chip does.
    fn pick_effort(&mut self, value: &str) -> Outcome {
        let Some(entry) = self.active_entry().cloned() else {
            return Outcome::Ignored;
        };
        if self
            .efforts_of(&entry)
            .is_some_and(|(values, _)| values.iter().any(|known| known == value))
        {
            self.efforts.insert(entry.key, value.to_owned());
            return Outcome::Redraw;
        }
        Outcome::Ignored
    }

    /// Act on the selected row: use it, or start the fix it needs.
    fn activate(&mut self) -> Outcome {
        let Some(entry) = self.active_entry().cloned() else {
            return Outcome::Ignored;
        };
        match (&entry.access, &entry.readiness) {
            (Access::Connect(candidate), _) => Outcome::Connect(candidate.clone()),
            (Access::Setup { .. }, _)
            | (
                _,
                inventory::Readiness::Action {
                    fix: Fix::Setup, ..
                },
            ) => Outcome::Setup,
            (
                Access::Connection {
                    binding_id,
                    application,
                    target,
                },
                inventory::Readiness::Action {
                    fix: Fix::Reapprove,
                    ..
                },
            ) => Outcome::Connect(Box::new(
                bcode_provider_auth_models::external::ExternalAuthCandidate {
                    binding_id: binding_id.clone(),
                    target: target.clone(),
                    application: application.clone(),
                    available: true,
                },
            )),
            (_, inventory::Readiness::Unavailable(reason)) => {
                self.notice = Some(Notice {
                    text: reason.clone(),
                    error: true,
                });
                Outcome::Redraw
            }
            _ => {
                let effort = self.efforts_of(&entry).map(|(_, picked)| picked);
                Outcome::Choose(Box::new(Choice { entry, effort }))
            }
        }
    }

    /// Record a choice as recent; returns the choices to persist.
    pub fn record_choice(&mut self, key: &str) -> ModelChoices {
        self.choices.record_recent(key);
        self.choices.clone()
    }

    fn toggle_favorite(&mut self) -> Outcome {
        let Some(entry) = self.active_entry() else {
            return Outcome::Ignored;
        };
        if matches!(entry.access, Access::Connect(_) | Access::Setup { .. }) {
            return Outcome::Ignored;
        }
        let key = entry.key.clone();
        let favorite = self.choices.toggle_favorite(&key);
        self.placed = true;
        self.notice = Some(Notice {
            text: if favorite {
                "Added to favorites".into()
            } else {
                "Removed from favorites".into()
            },
            error: false,
        });
        self.refresh();
        Outcome::SaveChoices(self.choices.clone())
    }

    /// Keys bound to `action`, for hints.
    fn hint(&self, action: &str) -> String {
        self.keymap.hint(KEYMAP_SCOPE, action)
    }

    /// Handle a key: a bound switcher action, or an edit of the search.
    pub fn key(&mut self, stroke: KeyStroke) -> Outcome {
        let action = self
            .keymap
            .action(KEYMAP_SCOPE, stroke)
            .map(ToOwned::to_owned);
        match action.as_deref() {
            Some("tui.control.cancel") => {
                if self.query_text().is_empty() {
                    return Outcome::Close;
                }
                self.query
                    .replace(TextInputState::new(bmux_text_edit::TextEditBuffer::new()));
                self.placed = false;
                self.refresh();
                return Outcome::Redraw;
            }
            Some("tui.control.submit") => return self.activate(),
            Some("tui.model.compare") => {
                self.layout = match self.layout {
                    Layout::Browse => Layout::Compare,
                    Layout::Compare => Layout::Browse,
                };
                return Outcome::Redraw;
            }
            Some("tui.model.setup") => return Outcome::Setup,
            Some("tui.control.left") => return self.step_effort(false),
            Some("tui.control.right") => return self.step_effort(true),
            Some("tui.model.favorite") => return self.toggle_favorite(),
            Some("tui.model.sort") if self.layout == Layout::Compare => {
                self.compare.cycle(false);
                return Outcome::Redraw;
            }
            Some("tui.model.reverseSort") if self.layout == Layout::Compare => {
                self.compare.cycle(true);
                return Outcome::Redraw;
            }
            Some("tui.control.up") => return self.move_selection(KeyCode::Up),
            Some("tui.control.down") => return self.move_selection(KeyCode::Down),
            Some("tui.control.page_up") => return self.move_selection(KeyCode::PageUp),
            Some("tui.control.page_down") => return self.move_selection(KeyCode::PageDown),
            Some("tui.control.home") => return self.move_selection(KeyCode::Home),
            Some("tui.control.end") => return self.move_selection(KeyCode::End),
            _ => {}
        }
        let before = self.query_text();
        let outcome = TextInputControl::new(&self.input_policy)
            .handle_key(&mut self.query.borrow_mut(), stroke);
        if self.query_text() != before {
            self.notice = None;
            self.placed = false;
            self.refresh();
            return Outcome::Redraw;
        }
        match outcome {
            TextInputOutcome::Ignored => Outcome::Ignored,
            _ => Outcome::Redraw,
        }
    }

    /// Insert pasted text into the search.
    pub fn paste(&mut self, text: &str) -> Outcome {
        let single_line = text.lines().next().unwrap_or_default();
        TextInputControl::new(&self.input_policy)
            .handle_paste(&mut self.query.borrow_mut(), single_line);
        self.placed = false;
        self.refresh();
        Outcome::Redraw
    }

    /// Route a pointer event through the layout last painted.
    pub fn mouse(&mut self, event: &Event, theme: &view::SwitcherTheme) -> Outcome {
        let Some(painted) = self.painted.clone() else {
            return Outcome::Ignored;
        };
        let Event::Mouse(mouse) = event else {
            return Outcome::Ignored;
        };
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            match view::target_at(self, &painted.layout, painted.area, mouse.position) {
                Some(view::Target::Effort(value)) => return self.pick_effort(&value),
                Some(view::Target::Action(action)) => {
                    return match action {
                        view::Action::Use => self.activate(),
                        view::Action::Favorite => self.toggle_favorite(),
                        view::Action::Setup => Outcome::Setup,
                        view::Action::Compare => {
                            self.layout = match self.layout {
                                Layout::Browse => Layout::Compare,
                                Layout::Compare => Layout::Browse,
                            };
                            Outcome::Redraw
                        }
                        view::Action::Close => Outcome::Close,
                    };
                }
                Some(view::Target::SortColumn(column)) => {
                    self.compare.sort_by(column);
                    return Outcome::Redraw;
                }
                Some(view::Target::Outside) => return Outcome::Close,
                None => {}
            }
        }
        let routed = view::route_list_event(self, theme, &painted, event);
        if let Some(id) = routed.pressed {
            // A press selects a row; a press on the row already selected uses it.
            if self.active.as_deref() == Some(id.as_str()) {
                return self.activate();
            }
            self.active = Some(id);
            self.placed = true;
            return Outcome::Redraw;
        }
        if routed.handled {
            Outcome::Redraw
        } else {
            Outcome::Ignored
        }
    }

    /// Paint over `area`, keeping the layout for pointer routing.
    pub fn paint(
        &mut self,
        theme: &view::SwitcherTheme,
        area: Rect,
        cx: &mut bmux_tui::paint::PaintCx<'_, '_>,
    ) {
        if area.width == 0 || area.height == 0 {
            self.painted = None;
            return;
        }
        let layout = view::paint(self, theme, area, cx);
        self.painted = Some(Painted { area, layout });
    }

    /// When the next animation frame is due, while a source is still loading.
    #[must_use]
    pub fn next_frame(&self) -> Option<std::time::Duration> {
        self.loading()
            .then(|| view::spinner_interval(self.started.elapsed()))
    }
}
