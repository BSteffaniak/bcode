//! Resizable, collapsible and closable panels, remembered per launch directory.
//!
//! The screen is a tree of BMUX panel groups: the root splits the side column from the main
//! column, the side column stacks the side windows, the main column stacks the main content
//! over the command log, and the main content may split staged and unstaged sections.
//! Dividers sit on the adjacent panes' borders and can be dragged; sizes, collapsed and closed
//! windows are saved for the directory the client was started in.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use bmux_tui::geometry::Rect;
use bmux_tui::style::{Modifier, Style};
use bmux_tui_components::panel_group::{
    PanelDividerMode, PanelGroup, PanelGroupAxis, PanelGroupConstraints, PanelGroupPolicy,
    PanelGroupResizePolicy, PanelGroupState, PanelGroupStyles, PanelSize,
};
use serde::{Deserialize, Serialize};

use crate::app::View;
use crate::config::{CollapsedWindows, LayoutConfig, ScreenMode};
use crate::theme::GitTheme;

/// Layout id of the group splitting the side column from the main column.
pub const ROOT: &str = "panels.root";
/// Layout id of the side-window stack.
pub const SIDE: &str = "panels.side";
/// Layout id of the main content over the command log.
pub const MAIN: &str = "panels.main";
/// Layout id of the staged and unstaged sections.
pub const SECTIONS: &str = "panels.sections";

/// Every group, innermost first: the order pointer presses are offered to them.
pub const GROUPS: [&str; 4] = [SECTIONS, MAIN, SIDE, ROOT];

/// Rows a collapsed side window keeps: its title.
pub const COLLAPSED_ROWS: u16 = 1;
/// Smallest bordered panel: two border rows around one content row.
const MIN_PANEL: u16 = 3;
/// Smallest column width either side of the root divider.
const MIN_COLUMN: u16 = 12;

/// A layout change requested from a menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutCommand {
    /// Show a closed side window again.
    Reopen(usize),
    /// Restore the configured layout and forget the saved one.
    Reset,
}

/// Runtime panel layout.
#[derive(Debug)]
pub struct Panels {
    /// Side column beside (or above) the main column; one width for every screen mode.
    pub root: RefCell<PanelGroupState>,
    /// Visible side windows.
    pub side: RefCell<PanelGroupState>,
    /// Main content and the command log.
    pub main: RefCell<PanelGroupState>,
    /// Staged and unstaged sections.
    pub sections: RefCell<PanelGroupState>,
    /// Side windows (by layout index) the user collapsed.
    pub collapsed: BTreeSet<usize>,
    /// Side windows (by layout index) the user closed.
    pub closed: BTreeSet<usize>,
    /// Layout indexes of the side windows in `side`, in order.
    pub visible: Vec<usize>,
    /// Whether the user resized the side windows (turning off focus expansion).
    pub side_resized: bool,
    /// Group areas in the last painted frame.
    pub areas: RefCell<BTreeMap<&'static str, Rect>>,
    /// Expanded size of each side window, kept while it is closed.
    side_sizes: BTreeMap<usize, PanelSize>,
    /// Where the layout is saved, when remembering is on.
    store: Option<PathBuf>,
    /// Session state read with the layout, for the application to apply.
    pub restored: Option<crate::session::Session>,
    /// What was last written (or found) on disk, so unchanged state is not rewritten.
    last_written: Option<String>,
}

impl Panels {
    /// Build the configured layout, then apply what was saved for this directory.
    #[must_use]
    pub fn new(config: &LayoutConfig, windows: &[Vec<View>], store: Option<PathBuf>) -> Self {
        // One width serves every screen mode; it starts at the width configured for the
        // starting mode.
        let side_width = match config.screen_mode {
            ScreenMode::Half => config.half_side_panel_width,
            ScreenMode::Normal | ScreenMode::Full => config.side_panel_width,
        }
        .clamp(10, 90);
        let mut panels = Self {
            root: RefCell::new(column_split(side_width)),
            side: RefCell::new(PanelGroupState::new(Vec::new())),
            main: RefCell::new(PanelGroupState::new(Vec::new())),
            sections: RefCell::new(PanelGroupState::new([
                PanelSize::flex(1),
                PanelSize::flex(1),
            ])),
            collapsed: BTreeSet::new(),
            closed: BTreeSet::new(),
            visible: Vec::new(),
            side_resized: false,
            areas: RefCell::new(BTreeMap::new()),
            side_sizes: BTreeMap::new(),
            store,
            restored: None,
            last_written: None,
        };
        panels
            .sections
            .borrow_mut()
            .set_constraints([PanelGroupConstraints::new(MIN_PANEL, None); 2]);
        for (index, tabs) in windows.iter().enumerate() {
            panels.side_sizes.insert(index, default_side_size(tabs));
        }
        for name in &config.collapsed {
            if let Some(index) = window_named(windows, name) {
                panels.collapsed.insert(index);
            }
        }
        if let Some(saved) = panels.store.as_deref().and_then(load) {
            panels.restore(&saved, windows, config.screen_mode);
        }
        panels
    }

    /// Bring panel states in line with the windows, focus, screen mode and command log.
    pub fn sync(&mut self, view: &PanelView<'_>) {
        let windows = view.windows;
        let half = view.screen_mode == ScreenMode::Half;
        let hidden_in_half = half && view.config.collapsed_windows == CollapsedWindows::Hidden;
        let mut visible: Vec<usize> = (0..windows.len())
            .filter(|index| !self.closed.contains(index))
            .filter(|index| !hidden_in_half || Some(*index) == view.focused)
            .collect();
        if visible.is_empty() {
            visible = (0..windows.len()).take(1).collect();
        }
        if visible != self.visible {
            self.remember_side_sizes();
            let sizes: Vec<PanelSize> = visible
                .iter()
                .map(|index| {
                    self.side_sizes
                        .get(index)
                        .copied()
                        .unwrap_or_else(|| default_side_size(&windows[*index]))
                })
                .collect();
            let mut side = PanelGroupState::new(sizes);
            side.set_constraints(vec![
                PanelGroupConstraints::new(MIN_PANEL, None);
                visible.len()
            ]);
            *self.side.borrow_mut() = side;
            self.visible = visible;
        }
        let mut side = self.side.borrow_mut();
        // Focus expansion until the user sizes the windows themselves.
        if view.config.expand_focused && !self.side_resized {
            for (position, index) in self.visible.iter().enumerate() {
                if windows[*index] == [View::Status] {
                    continue;
                }
                let weight = if Some(*index) == view.focused {
                    view.config.expanded_weight.max(1)
                } else {
                    1
                };
                side.set_size(position, PanelSize::flex(weight));
            }
        }
        for (position, index) in self.visible.iter().enumerate() {
            let collapse = self.collapsed.contains(index) || (half && Some(*index) != view.focused);
            if collapse {
                side.collapse(position, COLLAPSED_ROWS);
            } else {
                side.expand(position);
            }
        }
        drop(side);
        let panels = 1 + usize::from(view.command_log);
        if self.main.borrow().sizes().len() != panels {
            let mut sizes = vec![PanelSize::flex(1)];
            let mut constraints = vec![PanelGroupConstraints::new(MIN_PANEL, None)];
            if view.command_log {
                sizes.push(PanelSize::fixed(
                    view.config.command_log.height.max(MIN_PANEL),
                ));
                constraints.push(PanelGroupConstraints::new(MIN_PANEL, None));
            }
            let mut main = PanelGroupState::new(sizes);
            main.set_constraints(constraints);
            *self.main.borrow_mut() = main;
        }
    }

    /// Position of a side window among the visible ones.
    #[must_use]
    pub fn position(&self, index: usize) -> Option<usize> {
        self.visible.iter().position(|visible| *visible == index)
    }

    /// Whether a side window is shown collapsed (by the user or by half screen mode).
    #[must_use]
    pub fn is_collapsed(&self, index: usize) -> bool {
        self.position(index)
            .is_some_and(|position| self.side.borrow().is_collapsed(position))
    }

    /// Record that the user resized a group, for persistence and focus expansion.
    pub fn resized(&mut self, group: &str) {
        if group == SIDE {
            self.side_resized = true;
            self.remember_side_sizes();
        }
    }

    /// Forget saved and changed sizes, collapsed and closed windows.
    pub fn reset(&mut self, config: &LayoutConfig, windows: &[Vec<View>]) {
        let store = self.store.take();
        if let Some(path) = &store {
            let _ = std::fs::remove_file(path);
        }
        *self = Self::new(config, windows, None);
        self.store = store;
    }

    /// Save the layout and session for this directory, if remembering is on and anything
    /// changed since the last save.
    pub fn save(&mut self, windows: &[Vec<View>], session: &crate::session::Session) {
        let Some(text) = self.serialize(windows, session) else {
            return;
        };
        if self.last_written.as_deref() == Some(text.as_str()) {
            return;
        }
        let Some(path) = &self.store else {
            return;
        };
        if let Some(parent) = path.parent()
            && std::fs::create_dir_all(parent).is_ok()
        {
            let partial = path.with_extension("toml.partial");
            if std::fs::write(&partial, &text).is_ok() && std::fs::rename(&partial, path).is_ok() {
                self.last_written = Some(text);
            }
        }
    }

    /// Treat the current state as saved, without writing (after restoring it).
    pub fn mark_saved(&mut self, windows: &[Vec<View>], session: &crate::session::Session) {
        self.last_written = self.serialize(windows, session);
    }

    fn serialize(
        &mut self,
        windows: &[Vec<View>],
        session: &crate::session::Session,
    ) -> Option<String> {
        self.remember_side_sizes();
        self.store.as_ref()?;
        let key = |index: &usize| window_key(&windows[*index]);
        let saved = Saved {
            session: session.clone(),
            version: 1,
            root: self
                .root
                .borrow()
                .sizes()
                .iter()
                .copied()
                .map(Into::into)
                .collect(),
            root_half: Vec::new(),
            side: self
                .side_sizes
                .iter()
                .filter(|(index, _)| **index < windows.len())
                .map(|(index, size)| (key(index), (*size).into()))
                .collect(),
            side_resized: self.side_resized,
            collapsed: self.collapsed.iter().map(key).collect(),
            closed: self.closed.iter().map(key).collect(),
            sections: self
                .sections
                .borrow()
                .sizes()
                .iter()
                .copied()
                .map(Into::into)
                .collect(),
            command_log: self
                .main
                .borrow()
                .sizes()
                .get(1)
                .map(|size| SavedSize::from(*size)),
        };
        toml::to_string(&saved).ok()
    }

    fn restore(&mut self, saved: &Saved, windows: &[Vec<View>], configured_mode: ScreenMode) {
        self.restored = Some(saved.session.clone());
        let lookup =
            |name: &String| (0..windows.len()).find(|index| window_key(&windows[*index]) == *name);
        // Layouts saved while half screen mode had its own width keep the width of the mode
        // the session was in (or, for saves without a session, the configured starting mode).
        let half = saved.session.screen_mode.unwrap_or(configured_mode) == ScreenMode::Half;
        let root = if saved.root_half.len() == 2 && half {
            &saved.root_half
        } else {
            &saved.root
        };
        if root.len() == 2 {
            self.root.borrow_mut().set_sizes(
                root.iter()
                    .copied()
                    .map(PanelSize::from)
                    .collect::<Vec<_>>(),
            );
        }
        if saved.sections.len() == 2 {
            self.sections.borrow_mut().set_sizes(
                saved
                    .sections
                    .iter()
                    .copied()
                    .map(PanelSize::from)
                    .collect::<Vec<_>>(),
            );
        }
        for (name, size) in &saved.side {
            if let Some(index) = lookup(name) {
                self.side_sizes.insert(index, (*size).into());
            }
        }
        self.side_resized = saved.side_resized;
        self.collapsed = saved.collapsed.iter().filter_map(lookup).collect();
        self.closed = saved.closed.iter().filter_map(lookup).collect();
        // Never restore a layout with every side window closed.
        if self.closed.len() >= windows.len() {
            self.closed.clear();
        }
        if let Some(size) = saved.command_log {
            let mut main = PanelGroupState::new([PanelSize::flex(1), size.into()]);
            main.set_constraints([PanelGroupConstraints::new(MIN_PANEL, None); 2]);
            *self.main.borrow_mut() = main;
        }
    }

    fn remember_side_sizes(&mut self) {
        let side = self.side.borrow();
        for (position, index) in self.visible.iter().enumerate() {
            if let Some(size) = side.expanded_size(position) {
                self.side_sizes.insert(*index, size);
            }
        }
    }
}

/// What panel layout depends on, captured from the application each update.
pub struct PanelView<'a> {
    /// Layout settings.
    pub config: &'a LayoutConfig,
    /// Side windows and their tabs.
    pub windows: &'a [Vec<View>],
    /// Focused (or last focused) side window.
    pub focused: Option<usize>,
    /// Screen mode.
    pub screen_mode: ScreenMode,
    /// Whether the command log is shown.
    pub command_log: bool,
}

/// A resizable group: overlay dividers on the panes' shared borders, flex preserved.
///
/// Side by side, the handle covers only the right pane's left border, because the left pane's
/// right border holds its scrollbar. Stacked, it covers only the upper pane's bottom border,
/// because the lower pane's top border holds its title and buttons.
#[must_use]
pub const fn group(axis: PanelGroupAxis, theme: &GitTheme) -> PanelGroup {
    let (before, after) = match axis {
        PanelGroupAxis::Horizontal => (0, 1),
        PanelGroupAxis::Vertical => (1, 0),
    };
    PanelGroup::new(axis)
        .policy(PanelGroupPolicy {
            resize: PanelGroupResizePolicy::live().preserving_flex(true),
            ..PanelGroupPolicy::resize_only()
        })
        .divider_mode(PanelDividerMode::Overlay { before, after })
        .styles(PanelGroupStyles {
            divider: Style::new(),
            hovered_divider: theme.border_focused,
            active_divider: theme.border_focused.add_modifier(Modifier::BOLD),
        })
}

fn column_split(side_percent: u16) -> PanelGroupState {
    let mut state = PanelGroupState::new([
        PanelSize::flex(side_percent),
        PanelSize::flex(100 - side_percent),
    ]);
    state.set_constraints([PanelGroupConstraints::new(MIN_COLUMN, None); 2]);
    state
}

/// The status window is a fixed summary row; the others share the height.
fn default_side_size(tabs: &[View]) -> PanelSize {
    if tabs == [View::Status] {
        PanelSize::fixed(MIN_PANEL)
    } else {
        PanelSize::flex(1)
    }
}

/// Stable name of a side window in saved layouts: its tabs, in order.
#[must_use]
pub fn window_key(tabs: &[View]) -> String {
    tabs.iter()
        .map(|view| crate::layout::view_name(*view))
        .collect::<Vec<_>>()
        .join("+")
}

fn window_named(windows: &[Vec<View>], name: &str) -> Option<usize> {
    windows.iter().position(|tabs| {
        tabs.iter()
            .any(|view| crate::layout::view_name(*view).eq_ignore_ascii_case(name.trim()))
    })
}

/// Where layouts are saved: `$XDG_STATE_HOME/bcode-git/layouts`, else
/// `~/.local/state/bcode-git/layouts`.
#[must_use]
pub fn default_store_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("state"))
        })?;
    Some(base.join("bcode-git").join("layouts"))
}

/// The saved-layout file for a launch directory inside `dir`.
#[must_use]
pub fn store_file(dir: &Path, cwd: &Path) -> PathBuf {
    dir.join(format!(
        "{:016x}.toml",
        fnv1a(cwd.to_string_lossy().as_bytes())
    ))
}

/// Stable 64-bit FNV-1a, so file names survive toolchain changes.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn load(path: &Path) -> Option<Saved> {
    let text = std::fs::read_to_string(path).ok()?;
    toml::from_str::<Saved>(&text)
        .ok()
        .filter(|saved| saved.version == 1)
}

/// The saved layout of one launch directory.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    version: u32,
    root: Vec<SavedSize>,
    /// Half screen mode's former separate width, read from older saves only.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    root_half: Vec<SavedSize>,
    side: BTreeMap<String, SavedSize>,
    side_resized: bool,
    collapsed: Vec<String>,
    closed: Vec<String>,
    sections: Vec<SavedSize>,
    command_log: Option<SavedSize>,
    session: crate::session::Session,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum SavedSize {
    Fixed(u16),
    Flex(u16),
}

impl From<PanelSize> for SavedSize {
    fn from(size: PanelSize) -> Self {
        match size {
            PanelSize::Fixed(cells) => Self::Fixed(cells),
            PanelSize::Flex(weight) => Self::Flex(weight),
        }
    }
}

impl From<SavedSize> for PanelSize {
    fn from(size: SavedSize) -> Self {
        match size {
            SavedSize::Fixed(cells) => Self::Fixed(cells),
            SavedSize::Flex(weight) => Self::Flex(weight.max(1)),
        }
    }
}
