//! The review's appearance picker: the review's own palette and profile, or Bcode's.
//!
//! * [`AppearanceControl`] owns the appearance the app presents while it runs standalone: the
//!   appearance watcher offers each resolution to it, the picker previews and applies through
//!   it, and it restyles the host. Applying saves the review's own choice in user state
//!   (`[apps.review.theme]`) or removes it so the review follows Bcode's again; Bcode's own
//!   choice is never written here.
//! * [`AppearanceOverlay`] is the picker: a BMUX `picker_frame` over a BMUX `menu` of the rows of
//!   [`AppPicker`] (Follow Bcode, the palettes, the profiles). Moving the cursor restyles the
//!   whole app at once; `enter` applies, `esc` restores what was presented. Pointer events route
//!   through exactly the layout it was painted with.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use bcode_config_layers::user_state::{SavedTuiAppearance, save_app_tui_appearance};
use bcode_tui_theme::appearance::{AppearanceResolution, SavedAppearance};
use bcode_tui_theme::picker::LiveAppearance;
use bcode_tui_theme::picker::app::{AppPicker, AppPickerRow, AppPickerRowKind};
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::component::LayoutNode;
use bmux_tui::composition::{Column, Flex, TextBlock};
use bmux_tui::event::{Event, EventOutcome};
use bmux_tui::geometry::Rect;
use bmux_tui::paint::PaintCx;
use bmux_tui::text::{Line, Span, Text, TextWrap};
use bmux_tui_components::menu::MenuItem;
use bmux_tui_components::picker_frame::PickerFrameComponent;

use super::modal::{self, MenuPanel};
use super::theme::ReviewTheme;
use crate::config::APPEARANCE_APP;

const FRAME_ID: &str = "review.appearance";
const MENU_ID: &str = "review.appearance.menu";
/// Menu item id prefix of a picker row (followed by its index).
const ROW_PREFIX: &str = "row.";

/// Presents an appearance: restyles whatever shows the app.
pub type Present = Box<dyn Fn(&AppearanceResolution) + Send + Sync>;

/// The appearance the app presents, shared by its appearance watcher and its picker.
pub struct AppearanceControl {
    live: LiveAppearance,
    present: Present,
    state_file: PathBuf,
    open_requested: AtomicBool,
}

impl std::fmt::Debug for AppearanceControl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppearanceControl")
            .field("state_file", &self.state_file)
            .finish_non_exhaustive()
    }
}

impl AppearanceControl {
    /// Control starting from `initial` (already presented), presenting with `present` and
    /// saving the review's own choice in the user state file `state_file`.
    #[must_use]
    pub fn new(initial: AppearanceResolution, present: Present, state_file: PathBuf) -> Self {
        Self {
            live: LiveAppearance::new(initial),
            present,
            state_file,
            open_requested: AtomicBool::new(false),
        }
    }

    /// The appearance watcher resolved `resolution`: present it unless the picker previews or
    /// it predates the last applied choice ([`LiveAppearance::offer`]).
    pub fn offer(&self, resolution: AppearanceResolution) {
        self.live
            .offer(resolution, |resolution| (self.present)(resolution));
    }

    /// The latest appearance.
    #[must_use]
    pub fn current(&self) -> AppearanceResolution {
        self.live.current()
    }

    /// Ask the app to open the picker (from a screen that cannot open it itself).
    pub fn request_open(&self) {
        self.open_requested.store(true, Ordering::Release);
    }

    /// Whether the picker was asked for since the last call.
    pub fn take_open_request(&self) -> bool {
        self.open_requested.swap(false, Ordering::AcqRel)
    }

    fn present(&self, resolution: &AppearanceResolution) {
        (self.present)(resolution);
    }

    fn persist(&self, own: Option<&SavedAppearance>) -> Result<(), String> {
        let own = own.map(|own| SavedTuiAppearance {
            theme: own.theme.clone(),
            profile: own.profile.clone(),
        });
        save_app_tui_appearance(&self.state_file, APPEARANCE_APP, own.as_ref())
            .map_err(|error| error.to_string())
    }
}

/// What an input did to the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppearanceOutcome {
    /// Close the picker.
    Close,
    /// The picker changed.
    Redraw,
    /// Not handled (the picker still captures input).
    Ignored,
}

/// The open appearance picker.
pub struct AppearanceOverlay {
    picker: AppPicker,
    menu: MenuPanel,
    error: Option<String>,
    painted: Option<(Rect, LayoutNode)>,
}

impl std::fmt::Debug for AppearanceOverlay {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppearanceOverlay")
            .field("cursor", &self.picker.cursor())
            .finish_non_exhaustive()
    }
}

impl AppearanceOverlay {
    /// Open the picker over `control`'s latest appearance, holding watched appearances back
    /// until it closes.
    #[must_use]
    pub fn open(control: &AppearanceControl, theme: &ReviewTheme) -> Self {
        let picker = AppPicker::open("review", &control.live, display_path);
        let mut overlay = Self {
            picker,
            menu: MenuPanel::new(MENU_ID),
            error: None,
            painted: None,
        };
        overlay.refresh_items(theme);
        overlay
    }

    /// The picker model.
    #[must_use]
    pub const fn picker(&self) -> &AppPicker {
        &self.picker
    }

    /// Handle a key.
    pub fn key(
        &mut self,
        stroke: KeyStroke,
        control: &AppearanceControl,
        theme: &ReviewTheme,
    ) -> AppearanceOutcome {
        if !stroke.modifiers.is_empty() {
            return AppearanceOutcome::Ignored;
        }
        match stroke.key {
            KeyCode::Escape => {
                let _ = self
                    .picker
                    .cancel(&control.live, |resolution| control.present(resolution));
                AppearanceOutcome::Close
            }
            KeyCode::Enter => self.apply(control),
            KeyCode::Char('f') => self.select(0, control, theme),
            key => {
                if !self.menu.navigate(key) {
                    return AppearanceOutcome::Ignored;
                }
                let row = self.menu.active_id().and_then(row_index);
                row.map_or(AppearanceOutcome::Redraw, |row| {
                    self.select(row, control, theme)
                })
            }
        }
    }

    /// Route a pointer event through the painted layout; a click previews the row it lands
    /// on.
    pub fn mouse(
        &mut self,
        event: &Event,
        control: &AppearanceControl,
        theme: &ReviewTheme,
    ) -> AppearanceOutcome {
        let Some((area, layout)) = self.painted.clone() else {
            return AppearanceOutcome::Ignored;
        };
        let outcome = {
            let component = self.component(theme, area);
            modal::route(&component, area, &layout, event)
        };
        if let Some(row) = self.menu.take_activated().as_deref().and_then(row_index) {
            return self.select(row, control, theme);
        }
        if outcome == EventOutcome::Ignored {
            AppearanceOutcome::Ignored
        } else {
            AppearanceOutcome::Redraw
        }
    }

    fn select(
        &mut self,
        row: usize,
        control: &AppearanceControl,
        theme: &ReviewTheme,
    ) -> AppearanceOutcome {
        if let Some(preview) = self.picker.select(row, &control.live) {
            control.present(&preview);
            self.error = None;
            self.refresh_items(theme);
        }
        AppearanceOutcome::Redraw
    }

    fn apply(&mut self, control: &AppearanceControl) -> AppearanceOutcome {
        match self.picker.apply(
            &control.live,
            |own| control.persist(own),
            |resolution| control.present(resolution),
        ) {
            Ok(_) => AppearanceOutcome::Close,
            Err(error) => {
                self.error = Some(format!("could not save the appearance: {error}"));
                // The preview ended; reopen on what is presented now.
                self.picker = AppPicker::open("review", &control.live, display_path);
                AppearanceOutcome::Redraw
            }
        }
    }

    /// Rebuild the menu items from the picker rows, keeping the cursor on its row.
    fn refresh_items(&mut self, theme: &ReviewTheme) {
        let items = self
            .picker
            .rows()
            .iter()
            .enumerate()
            .map(|(index, row)| item(index, row, theme))
            .collect();
        self.menu.set_items(items);
        self.menu
            .activate_id(&format!("{ROW_PREFIX}{}", self.picker.cursor()));
    }

    fn notes(&self, theme: &ReviewTheme) -> Vec<Line> {
        let mut lines = vec![Line::from_spans(vec![Span::styled(
            self.picker.mode_label(),
            theme.muted,
        )])];
        if let Some(pinned_by) = self.picker.pinned_by() {
            lines.push(Line::from_spans(vec![Span::styled(
                format!("The palette is pinned by {pinned_by}; only the profile applies."),
                theme.muted,
            )]));
        }
        if let Some(error) = &self.error {
            lines.push(Line::from_spans(vec![Span::styled(
                error.clone(),
                theme.error,
            )]));
        }
        lines
    }

    fn component<'a>(&'a self, theme: &ReviewTheme, area: Rect) -> PickerFrameComponent<'a> {
        let frame = modal::frame(
            theme,
            " Review appearance ",
            "↑/↓ preview · enter apply · f follow Bcode · esc cancel",
            area.size(),
        );
        let notes = TextBlock::new(Text::from_lines(self.notes(theme))).wrap(TextWrap::Word);
        let body = Column::new()
            .id(format!("{FRAME_ID}.body"))
            .child(notes)
            .flex(Flex::new(1, self.menu.component(theme)));
        PickerFrameComponent::new(FRAME_ID, frame, body)
    }

    /// Paint over `area`, keeping the layout for pointer routing.
    pub fn paint(&mut self, theme: &ReviewTheme, area: Rect, cx: &mut PaintCx<'_, '_>) {
        if area.width == 0 || area.height == 0 {
            self.painted = None;
            return;
        }
        // Rows carry styles of the theme they were built with; the theme changes as the
        // cursor previews.
        self.refresh_items(theme);
        let layout = {
            let component = self.component(theme, area);
            modal::paint(&component, theme, area, cx)
        };
        self.menu.note_layout(&layout);
        self.painted = Some((area, layout));
    }
}

fn row_index(id: &str) -> Option<usize> {
    id.strip_prefix(ROW_PREFIX)?.parse().ok()
}

fn item(index: usize, row: &AppPickerRow, theme: &ReviewTheme) -> MenuItem {
    let id = format!("{ROW_PREFIX}{index}");
    match row.kind {
        AppPickerRowKind::Heading(_) => {
            let mut item = MenuItem::rich(
                id,
                Line::from_spans(vec![Span::styled(row.label.clone(), theme.title)]),
            );
            item.section = true;
            item
        }
        AppPickerRowKind::Pinned => {
            let mut item = MenuItem::rich(id, row_line(row, theme));
            item.section = true;
            item
        }
        AppPickerRowKind::Follow | AppPickerRowKind::Palette(_) | AppPickerRowKind::Profile(_) => {
            MenuItem::rich(id, row_line(row, theme))
        }
    }
}

fn row_line(row: &AppPickerRow, theme: &ReviewTheme) -> Line {
    let mut spans = vec![
        Span::styled(if row.chosen { "● " } else { "  " }, theme.accent),
        Span::styled(row.label.clone(), theme.text),
    ];
    if let Some(detail) = &row.detail {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(detail.clone(), theme.muted));
    }
    Line::from_spans(spans)
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
}
