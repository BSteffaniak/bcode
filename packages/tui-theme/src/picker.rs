//! The appearance picker model every Bcode program shares.
//!
//! A program that lets the user choose its appearance lists the catalog
//! ([`theme_catalog_view`], [`profile_catalog`]) and keeps one selection per axis in an
//! [`AppearanceSelection`]. An app with its own appearance (see
//! [`crate::appearance::AppearanceSources::app_saved`]) also presents each movement live
//! through a [`LiveAppearance`], which keeps the app's appearance watcher from overriding a
//! preview and from presenting a stale appearance after a choice was saved, and lets the user
//! return the app to following Bcode's choice. Rendering, keybindings and writing the state
//! file stay with the program.

pub mod app;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use crate::appearance::{AppearanceResolution, SavedAppearance};
use crate::definition::{ThemeCatalog, ThemeKind};
use crate::discovery::{ThemeDiscoveryDiagnostic, ThemeExternalSource, ThemeSourceKind};

/// One bounded theme catalog row for selection and diagnostics UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeCatalogEntry {
    /// Stable theme id.
    pub id: String,
    /// Human-readable display name.
    pub display_name: String,
    /// Bundled, user, project, or explicit origin label.
    pub source: String,
    /// Whether a dark variant patch is available.
    pub has_dark_variant: bool,
    /// Whether a light variant patch is available.
    pub has_light_variant: bool,
    /// Validation state for this accepted catalog definition.
    pub validation: String,
    /// Whether this is the active selection.
    pub selected: bool,
}

/// One bounded effective catalog plus rejected-candidate diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeCatalogView {
    /// Selectable, fully validated definitions.
    pub entries: Vec<ThemeCatalogEntry>,
    /// Bounded secret-safe rejected candidate summaries.
    pub diagnostics: Vec<String>,
}

/// One selectable interface profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileCatalogEntry {
    /// Stable profile id.
    pub id: String,
    /// Display name without the `(profile)` suffix.
    pub display_name: String,
    /// Optional one-line description.
    pub description: Option<String>,
    /// Whether this is the active profile.
    pub selected: bool,
}

/// The selectable palettes (complete themes) of `catalog`, in catalog order, with `selected`
/// marked. `sources` labels external themes; `display_path` shows the path of each skipped
/// candidate in `diagnostics`.
#[must_use]
pub fn theme_catalog_view(
    catalog: &ThemeCatalog,
    sources: &BTreeMap<String, ThemeExternalSource>,
    diagnostics: &[ThemeDiscoveryDiagnostic],
    selected: &str,
    display_path: impl Fn(&Path) -> String,
) -> ThemeCatalogView {
    let entries = catalog
        .definitions()
        .filter(|definition| definition.kind() == ThemeKind::Theme)
        .map(|definition| ThemeCatalogEntry {
            id: definition.id().to_owned(),
            display_name: definition.display_name().to_owned(),
            source: sources
                .get(definition.id())
                .map_or("bundled", |external| match external.kind {
                    ThemeSourceKind::User => "user",
                    ThemeSourceKind::Project => "project",
                    ThemeSourceKind::Explicit => "explicit",
                })
                .to_owned(),
            has_dark_variant: definition.has_dark_variant(),
            has_light_variant: definition.has_light_variant(),
            validation: "valid".to_owned(),
            selected: definition.id() == selected,
        })
        .collect();
    let diagnostics = diagnostics
        .iter()
        .map(|diagnostic| format!("{}: {}", display_path(&diagnostic.path), diagnostic.message))
        .collect();
    ThemeCatalogView {
        entries,
        diagnostics,
    }
}

/// The selectable profiles of `catalog` in stable id order, with `selected` marked.
#[must_use]
pub fn profile_catalog(catalog: &ThemeCatalog, selected: Option<&str>) -> Vec<ProfileCatalogEntry> {
    catalog
        .definitions()
        .filter(|definition| definition.kind() == ThemeKind::Profile)
        .map(|definition| ProfileCatalogEntry {
            id: definition.id().to_owned(),
            display_name: definition
                .display_name()
                .trim_end_matches(" (profile)")
                .to_owned(),
            description: definition.description().map(str::to_owned),
            selected: selected == Some(definition.id()),
        })
        .collect()
}

impl AppearanceResolution {
    /// The palettes of this appearance's catalog, with the presented one marked
    /// ([`theme_catalog_view`]).
    #[must_use]
    pub fn theme_catalog_view(&self, display_path: impl Fn(&Path) -> String) -> ThemeCatalogView {
        theme_catalog_view(
            &self.catalog,
            &self.sources,
            &self.diagnostics,
            &self.preferences.name,
            display_path,
        )
    }

    /// The profiles of this appearance's catalog, with the presented one marked
    /// ([`profile_catalog`]).
    #[must_use]
    pub fn profile_catalog(&self) -> Vec<ProfileCatalogEntry> {
        profile_catalog(&self.catalog, self.preferences.profile.as_deref())
    }

    /// An app's appearance picker over this appearance's catalog, on the presented palette
    /// and profile, following Bcode's choice when the app has none of its own
    /// ([`AppearanceSelection::following_bcode`]).
    #[must_use]
    pub fn app_selection(
        &self,
        axis: AppearanceAxis,
        display_path: impl Fn(&Path) -> String,
    ) -> AppearanceSelection {
        let mut selection = self.selection(axis, display_path);
        selection.following = Some(self.follows_bcode());
        selection
    }

    /// An appearance picker over this appearance's catalog, on the presented palette and
    /// profile.
    #[must_use]
    pub fn selection(
        &self,
        axis: AppearanceAxis,
        display_path: impl Fn(&Path) -> String,
    ) -> AppearanceSelection {
        let view = self.theme_catalog_view(display_path);
        AppearanceSelection::new(
            view.entries,
            view.diagnostics,
            self.profile_catalog(),
            self.preferences.profile.clone(),
            axis,
        )
    }
}

/// One of the picker's two selection axes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AppearanceAxis {
    /// Color palette (a complete theme).
    Palette,
    /// Interface profile overlay (chrome and structure).
    Profile,
}

impl AppearanceAxis {
    /// Return the other axis.
    #[must_use]
    pub const fn other(self) -> Self {
        match self {
            Self::Palette => Self::Profile,
            Self::Profile => Self::Palette,
        }
    }

    /// Return the axis column label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Palette => "Palette",
            Self::Profile => "Profile",
        }
    }
}

/// What one picker input asks the program to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppearanceOutcome {
    /// The palette selection moved; preview it without persisting.
    PreviewPalette(String),
    /// The profile selection moved; preview it without persisting (`None` clears the profile
    /// overlay).
    PreviewProfile(Option<String>),
    /// The active axis changed; nothing needs to be applied.
    AxisChanged(AppearanceAxis),
    /// Persist both selections.
    Apply {
        /// Selected palette (theme) id.
        palette: String,
        /// Selected profile id (`None` for the theme's own presentation).
        profile: Option<String>,
    },
    /// Close and restore both selections the picker opened with.
    Cancel,
    /// The input changed nothing.
    Ignored,
}

/// The palette axis: catalog rows, rejected-candidate diagnostics and the selected row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteAxis {
    entries: Vec<ThemeCatalogEntry>,
    diagnostics: Vec<String>,
    selected: Option<usize>,
}

impl PaletteAxis {
    /// Create the axis from catalog rows, selecting the row marked `selected` (or the first).
    #[must_use]
    pub fn new(entries: Vec<ThemeCatalogEntry>, diagnostics: Vec<String>) -> Self {
        let selected = (!entries.is_empty())
            .then(|| entries.iter().position(|entry| entry.selected).unwrap_or(0));
        Self {
            entries,
            diagnostics,
            selected,
        }
    }

    /// Catalog rows.
    #[must_use]
    pub fn entries(&self) -> &[ThemeCatalogEntry] {
        &self.entries
    }

    /// Bounded rejected-candidate diagnostics.
    #[must_use]
    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    /// Index of the selected row.
    #[must_use]
    pub const fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    /// The selected row.
    #[must_use]
    pub fn selected_entry(&self) -> Option<&ThemeCatalogEntry> {
        self.entries.get(self.selected?)
    }

    /// The selected theme id.
    #[must_use]
    pub fn selected_id(&self) -> Option<&str> {
        self.selected_entry().map(|entry| entry.id.as_str())
    }

    fn select_row(&mut self, row: usize) -> Option<String> {
        let id = self.entries.get(row)?.id.clone();
        self.selected = Some(row);
        Some(id)
    }

    fn step(&mut self, forward: bool) -> Option<String> {
        let selected = step_index(self.selected, self.entries.len(), forward)?;
        self.select_row(selected)
    }
}

/// One profile row; `id` is `None` for the theme's own presentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileRow {
    /// Profile id, or `None` for no profile overlay.
    pub id: Option<String>,
    /// Display name.
    pub display_name: String,
    /// One-line description.
    pub description: Option<String>,
}

/// The profile axis: a "Theme default" row followed by the profile catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileAxis {
    rows: Vec<ProfileRow>,
    original: Option<String>,
    selected: usize,
}

impl ProfileAxis {
    /// Create the axis from catalog rows; `original` is the active profile.
    #[must_use]
    pub fn new(catalog: Vec<ProfileCatalogEntry>, original: Option<String>) -> Self {
        let rows = std::iter::once(ProfileRow {
            id: None,
            display_name: "Theme default".to_owned(),
            description: Some("No profile overlay: the theme's own chrome and structure.".into()),
        })
        .chain(catalog.into_iter().map(|entry| ProfileRow {
            id: Some(entry.id),
            display_name: entry.display_name,
            description: entry.description,
        }))
        .collect::<Vec<_>>();
        let selected = rows.iter().position(|row| row.id == original).unwrap_or(0);
        Self {
            rows,
            original,
            selected,
        }
    }

    /// Rows.
    #[must_use]
    pub fn entries(&self) -> &[ProfileRow] {
        &self.rows
    }

    /// The profile active when the picker opened.
    #[must_use]
    pub fn original(&self) -> Option<&str> {
        self.original.as_deref()
    }

    /// Index of the selected row.
    #[must_use]
    pub const fn selected_index(&self) -> usize {
        self.selected
    }

    /// The selected row.
    #[must_use]
    pub fn selected_entry(&self) -> Option<&ProfileRow> {
        self.rows.get(self.selected)
    }

    fn select_row(&mut self, row: usize) -> Option<&ProfileRow> {
        if row >= self.rows.len() {
            return None;
        }
        self.selected = row;
        self.rows.get(row)
    }

    fn step(&mut self, forward: bool) -> Option<&ProfileRow> {
        let selected = step_index(Some(self.selected), self.rows.len(), forward)?;
        self.select_row(selected)
    }
}

/// The row one step from `selected` among `len` rows, clamped to the ends.
fn step_index(selected: Option<usize>, len: usize, forward: bool) -> Option<usize> {
    let last = len.checked_sub(1)?;
    Some(match selected {
        None => 0,
        Some(index) if forward => (index + 1).min(last),
        Some(index) => index.saturating_sub(1).min(last),
    })
}

/// Bounded palette × profile picker selection.
///
/// One axis is active at a time: the axis commands switch it and the selection commands move
/// within it. Every movement is reported as a preview outcome so the program presents it
/// live; cancelling restores both selections the picker opened with.
#[derive(Debug, Clone)]
pub struct AppearanceSelection {
    palettes: PaletteAxis,
    profiles: ProfileAxis,
    axis: AppearanceAxis,
    original_palette: Option<String>,
    /// Whether the active profile has a row (`None` always does).
    original_profile_listed: bool,
    /// Whether the user selected a row on each axis. An axis whose active value has no row
    /// starts on a fallback row the user never chose, so only an explicit selection counts as
    /// a change there.
    palette_chosen: bool,
    profile_chosen: bool,
    /// For an app's picker, whether the selection follows Bcode's choice instead of saving
    /// the app's own; `None` for Bcode's own picker.
    following: Option<bool>,
}

impl AppearanceSelection {
    /// Create a selection from the palette and profile catalogs.
    ///
    /// `active_profile` is the profile active when the picker opened; the active palette is
    /// the catalog row marked `selected`. `axis` is the axis that starts active.
    #[must_use]
    pub fn new(
        palettes: Vec<ThemeCatalogEntry>,
        diagnostics: Vec<String>,
        profiles: Vec<ProfileCatalogEntry>,
        active_profile: Option<String>,
        axis: AppearanceAxis,
    ) -> Self {
        let original_palette = palettes
            .iter()
            .find(|entry| entry.selected)
            .map(|entry| entry.id.clone());
        let profiles = ProfileAxis::new(profiles, active_profile);
        let original_profile_listed = profiles
            .entries()
            .iter()
            .any(|row| row.id.as_deref() == profiles.original());
        Self {
            palettes: PaletteAxis::new(palettes, diagnostics),
            profiles,
            axis,
            original_palette,
            original_profile_listed,
            palette_chosen: false,
            profile_chosen: false,
            following: None,
        }
    }

    /// For an app's picker ([`AppearanceResolution::app_selection`]), whether the selection
    /// follows Bcode's choice instead of saving the app's own; `None` for Bcode's own picker.
    /// Selecting a row ends following.
    #[must_use]
    pub const fn following_bcode(&self) -> Option<bool> {
        self.following
    }

    /// Select the rows of `palette` and `profile` without counting them as the user's choice;
    /// a value without a row keeps the current row.
    fn show(&mut self, palette: &str, profile: Option<&str>) {
        if let Some(row) = self
            .palettes
            .entries()
            .iter()
            .position(|entry| entry.id == palette)
        {
            let _ = self.palettes.select_row(row);
        }
        if let Some(row) = self
            .profiles
            .entries()
            .iter()
            .position(|row| row.id.as_deref() == profile)
        {
            let _ = self.profiles.select_row(row);
        }
    }

    /// The active axis.
    #[must_use]
    pub const fn axis(&self) -> AppearanceAxis {
        self.axis
    }

    /// Make `axis` active; returns whether it changed.
    pub fn set_axis(&mut self, axis: AppearanceAxis) -> bool {
        std::mem::replace(&mut self.axis, axis) != axis
    }

    /// Make `axis` active, reporting [`AppearanceOutcome::AxisChanged`] when it was not.
    pub fn switch_to(&mut self, axis: AppearanceAxis) -> AppearanceOutcome {
        if self.set_axis(axis) {
            AppearanceOutcome::AxisChanged(axis)
        } else {
            AppearanceOutcome::Ignored
        }
    }

    /// Make the other axis active.
    pub fn toggle_axis(&mut self) -> AppearanceOutcome {
        self.switch_to(self.axis.other())
    }

    /// The palette axis.
    #[must_use]
    pub const fn palettes(&self) -> &PaletteAxis {
        &self.palettes
    }

    /// The profile axis.
    #[must_use]
    pub const fn profiles(&self) -> &ProfileAxis {
        &self.profiles
    }

    /// The palette active when the picker opened, if it is listed.
    #[must_use]
    pub fn original_palette(&self) -> Option<&str> {
        self.original_palette.as_deref()
    }

    /// The profile active when the picker opened.
    #[must_use]
    pub fn original_profile(&self) -> Option<&str> {
        self.profiles.original()
    }

    /// The selected palette id.
    #[must_use]
    pub fn selected_palette(&self) -> Option<&str> {
        self.palettes.selected_id()
    }

    /// The selected profile id (`None` for the theme default row).
    #[must_use]
    pub fn selected_profile(&self) -> Option<&str> {
        self.profiles
            .selected_entry()
            .and_then(|row| row.id.as_deref())
    }

    /// Index of the selected row on `axis`.
    #[must_use]
    pub const fn selected_index(&self, axis: AppearanceAxis) -> Option<usize> {
        match axis {
            AppearanceAxis::Palette => self.palettes.selected_index(),
            AppearanceAxis::Profile => Some(self.profiles.selected_index()),
        }
    }

    /// Whether the palette selection differs from the active palette.
    ///
    /// When the active palette has no catalog row, the picker starts on a fallback row; the
    /// palette counts as changed only once the user selected a row.
    #[must_use]
    pub fn palette_changed(&self) -> bool {
        self.original_palette
            .as_deref()
            .map_or(self.palette_chosen, |original| {
                self.selected_palette() != Some(original)
            })
    }

    /// Whether the profile selection differs from the active profile, with the same fallback
    /// rule as [`Self::palette_changed`] for an active profile that has no row.
    #[must_use]
    pub fn profile_changed(&self) -> bool {
        if self.original_profile_listed {
            self.selected_profile() != self.original_profile()
        } else {
            self.profile_chosen
        }
    }

    /// The choices to save in user state for this selection, over `prior`: an axis the user
    /// changed replaces its saved choice; an unchanged axis keeps it.
    #[must_use]
    pub fn saved_choice(&self, prior: &SavedAppearance) -> SavedAppearance {
        SavedAppearance {
            theme: if self.palette_changed() {
                self.selected_palette().map(str::to_owned)
            } else {
                prior.theme.clone()
            },
            profile: if self.profile_changed() {
                self.selected_profile().map(str::to_owned)
            } else {
                prior.profile.clone()
            },
        }
    }

    /// The number of rows on `axis`.
    #[must_use]
    pub fn row_count(&self, axis: AppearanceAxis) -> usize {
        match axis {
            AppearanceAxis::Palette => self.palettes.entries().len(),
            AppearanceAxis::Profile => self.profiles.entries().len(),
        }
    }

    /// Make `axis` active and select one absolute row on it.
    pub fn select_row(&mut self, axis: AppearanceAxis, row: usize) -> AppearanceOutcome {
        if row >= self.row_count(axis) {
            return AppearanceOutcome::Ignored;
        }
        self.axis = axis;
        let outcome = match axis {
            AppearanceAxis::Palette => palette_outcome(self.palettes.select_row(row)),
            AppearanceAxis::Profile => profile_outcome(self.profiles.select_row(row)),
        };
        self.note_choice(&outcome);
        outcome
    }

    /// Make `axis` active and move its selection one row, as one mouse wheel notch over its
    /// column does (`forward` is down).
    pub fn scroll(&mut self, axis: AppearanceAxis, forward: bool) -> AppearanceOutcome {
        self.axis = axis;
        self.step(forward)
    }

    /// Move the active axis's selection one row (`forward` is down).
    pub fn step(&mut self, forward: bool) -> AppearanceOutcome {
        let outcome = match self.axis {
            AppearanceAxis::Palette => palette_outcome(self.palettes.step(forward)),
            AppearanceAxis::Profile => profile_outcome(self.profiles.step(forward)),
        };
        self.note_choice(&outcome);
        outcome
    }

    /// Request persistence of both current selections.
    #[must_use]
    pub fn apply(&self) -> AppearanceOutcome {
        self.selected_palette()
            .map_or(AppearanceOutcome::Ignored, |palette| {
                AppearanceOutcome::Apply {
                    palette: palette.to_owned(),
                    profile: self.selected_profile().map(str::to_owned),
                }
            })
    }

    /// Record that the user selected a row on the axis `outcome` previews.
    const fn note_choice(&mut self, outcome: &AppearanceOutcome) {
        let chosen = match outcome {
            AppearanceOutcome::PreviewPalette(_) => &mut self.palette_chosen,
            AppearanceOutcome::PreviewProfile(_) => &mut self.profile_chosen,
            _ => return,
        };
        *chosen = true;
        if self.following.is_some() {
            self.following = Some(false);
        }
    }
}

fn palette_outcome(selected: Option<String>) -> AppearanceOutcome {
    selected.map_or(
        AppearanceOutcome::Ignored,
        AppearanceOutcome::PreviewPalette,
    )
}

fn profile_outcome(selected: Option<&ProfileRow>) -> AppearanceOutcome {
    selected.map_or(AppearanceOutcome::Ignored, |row| {
        AppearanceOutcome::PreviewProfile(row.id.clone())
    })
}

/// The saved choices an appearance was resolved from: Bcode's and the app's own.
type SavedChoices = (SavedAppearance, Option<SavedAppearance>);

fn saved_choices(resolution: &AppearanceResolution) -> SavedChoices {
    (resolution.saved.clone(), resolution.app_saved.clone())
}

/// The appearance an app presents while its watcher and its picker both change it.
///
/// The app's appearance watcher [offers](Self::offer) every resolution it makes; while the
/// picker previews, offers are recorded but not presented, so the preview stays on screen and
/// cancelling restores the latest watched appearance. After a choice is
/// [applied](Self::apply), resolutions made from the state saved before it are dropped, so a
/// watcher tick that read the file before the write cannot bring the previous appearance
/// back.
///
/// The picker's choice is the app's own ([`AppearanceResolution::app_saved`]): applying saves
/// it for this app only, or, when the selection
/// [follows Bcode](AppearanceSelection::following_bcode), removes it so the app follows Bcode's
/// choice again.
#[derive(Debug)]
pub struct LiveAppearance {
    state: Mutex<LiveState>,
}

#[derive(Debug)]
struct LiveState {
    latest: AppearanceResolution,
    previewing: bool,
    /// The saved choices replaced by the last apply, until a resolution of anything else
    /// arrives.
    superseded: Option<SavedChoices>,
}

impl LiveState {
    /// The app's own choice `selection` stands for over the latest appearance; `None` follows
    /// Bcode's.
    fn app_choice(&self, selection: &AppearanceSelection) -> Option<SavedAppearance> {
        (selection.following_bcode() != Some(true))
            .then(|| selection.saved_choice(self.latest.effective_saved()))
    }
}

impl LiveAppearance {
    /// Start from `resolution`, which the app presents.
    #[must_use]
    pub const fn new(resolution: AppearanceResolution) -> Self {
        Self {
            state: Mutex::new(LiveState {
                latest: resolution,
                previewing: false,
                superseded: None,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LiveState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The watcher resolved `resolution`. Unless it is stale (made from the choices the last
    /// apply replaced), it becomes the latest appearance, and unless the picker previews,
    /// `present` shows it; both happen under one lock, so a preview that starts meanwhile is
    /// never overwritten. Returns whether `present` ran.
    pub fn offer(
        &self,
        resolution: AppearanceResolution,
        present: impl FnOnce(&AppearanceResolution),
    ) -> bool {
        let mut state = self.lock();
        if state.superseded.as_ref() == Some(&saved_choices(&resolution)) {
            return false;
        }
        state.superseded = None;
        state.latest = resolution;
        if state.previewing {
            return false;
        }
        present(&state.latest);
        true
    }

    /// The latest appearance (presented unless the picker previews).
    #[must_use]
    pub fn current(&self) -> AppearanceResolution {
        self.lock().latest.clone()
    }

    /// Whether the picker previews.
    #[must_use]
    pub fn is_previewing(&self) -> bool {
        self.lock().previewing
    }

    /// The picker opened: hold watched appearances back until it closes. Returns the latest
    /// appearance, which the picker lists ([`AppearanceResolution::app_selection`]).
    #[must_use]
    pub fn begin_preview(&self) -> AppearanceResolution {
        let mut state = self.lock();
        state.previewing = true;
        state.latest.clone()
    }

    /// The appearance `selection` previews: exactly what applying it presents.
    #[must_use]
    pub fn preview(&self, selection: &AppearanceSelection) -> AppearanceResolution {
        let state = self.lock();
        let app = state.app_choice(selection);
        state.latest.with_saved(&state.latest.saved, app.as_ref())
    }

    /// Make `selection` follow Bcode's choice (`follow`) or keep the app's own. Starting to
    /// follow shows Bcode's palette and profile in the lists; stopping keeps the rows, which
    /// then become the app's own choice. Returns the appearance to preview.
    pub fn set_following(
        &self,
        selection: &mut AppearanceSelection,
        follow: bool,
    ) -> AppearanceResolution {
        selection.following = Some(follow);
        if follow {
            let bcode = self.bcode_appearance();
            selection.show(
                &bcode.preferences.name,
                bcode.preferences.profile.as_deref(),
            );
            return bcode;
        }
        self.preview(selection)
    }

    /// The appearance the app presents while it follows Bcode's choice.
    #[must_use]
    pub fn bcode_appearance(&self) -> AppearanceResolution {
        let state = self.lock();
        state.latest.with_saved(&state.latest.saved, None)
    }

    /// The picker closed without applying. Returns the latest appearance, to present again.
    #[must_use]
    pub fn cancel(&self) -> AppearanceResolution {
        let mut state = self.lock();
        state.previewing = false;
        state.latest.clone()
    }

    /// Save `selection` with `persist`, which receives the app's own choice to write (`None`
    /// removes it, so the app follows Bcode's), and end the preview. Nothing is written when
    /// the choice is unchanged. Returns the applied appearance, to present; it becomes the
    /// latest, and resolutions of the previous saved choices are ignored from now on.
    ///
    /// # Errors
    ///
    /// * The error of `persist`; the preview ends and the latest appearance is kept (present
    ///   [`Self::current`]).
    pub fn apply<E>(
        &self,
        selection: &AppearanceSelection,
        persist: impl FnOnce(Option<&SavedAppearance>) -> Result<(), E>,
    ) -> Result<AppearanceResolution, E> {
        let mut state = self.lock();
        state.previewing = false;
        let app = state.app_choice(selection);
        if app == state.latest.app_saved {
            return Ok(state.latest.clone());
        }
        persist(app.as_ref())?;
        let prior = saved_choices(&state.latest);
        state.latest = state.latest.with_saved(&state.latest.saved, app.as_ref());
        state.superseded = Some(prior);
        Ok(state.latest.clone())
    }
}

#[cfg(test)]
mod tests {
    use bmux_tui::capabilities::TerminalBackground;

    use super::*;
    use crate::appearance::{AppearanceSources, ThemePreferences};

    fn palette(id: &str, selected: bool) -> ThemeCatalogEntry {
        ThemeCatalogEntry {
            id: id.to_owned(),
            display_name: id.to_owned(),
            source: "bundled".to_owned(),
            has_dark_variant: true,
            has_light_variant: false,
            validation: "valid".to_owned(),
            selected,
        }
    }

    fn profile(id: &str) -> ProfileCatalogEntry {
        ProfileCatalogEntry {
            id: id.to_owned(),
            display_name: id.to_owned(),
            description: None,
            selected: false,
        }
    }

    fn selection(axis: AppearanceAxis) -> AppearanceSelection {
        AppearanceSelection::new(
            vec![palette("dawn", false), palette("dusk", true)],
            Vec::new(),
            vec![profile("bare"), profile("zen")],
            Some("zen".to_owned()),
            axis,
        )
    }

    fn resolution(dir: &Path, configured: &str, saved: SavedAppearance) -> AppearanceResolution {
        AppearanceSources {
            configured: ThemePreferences {
                name: configured.to_owned(),
                ..ThemePreferences::default()
            },
            saved,
            user_config_dir: dir.to_path_buf(),
            project_root: dir.to_path_buf(),
            ..AppearanceSources::default()
        }
        .resolve(TerminalBackground::Unknown)
    }

    fn id(resolution: &AppearanceResolution) -> String {
        resolution.resolved.as_ref().expect("resolves").id.clone()
    }

    #[test]
    fn selection_opens_on_both_active_choices_and_moves_only_the_active_axis() {
        let mut selection = selection(AppearanceAxis::Palette);
        assert_eq!(selection.selected_palette(), Some("dusk"));
        assert_eq!(selection.selected_profile(), Some("zen"));
        assert_eq!(
            selection.step(false),
            AppearanceOutcome::PreviewPalette("dawn".to_owned())
        );
        // Clamped at the top.
        assert_eq!(
            selection.step(false),
            AppearanceOutcome::PreviewPalette("dawn".to_owned())
        );
        assert_eq!(selection.selected_profile(), Some("zen"));
        assert_eq!(
            selection.toggle_axis(),
            AppearanceOutcome::AxisChanged(AppearanceAxis::Profile)
        );
        assert_eq!(
            selection.switch_to(AppearanceAxis::Profile),
            AppearanceOutcome::Ignored
        );
        assert_eq!(
            selection.step(false),
            AppearanceOutcome::PreviewProfile(Some("bare".to_owned()))
        );
        assert_eq!(
            selection.step(false),
            AppearanceOutcome::PreviewProfile(None)
        );
        assert_eq!(
            selection.apply(),
            AppearanceOutcome::Apply {
                palette: "dawn".to_owned(),
                profile: None,
            }
        );
        assert_eq!(selection.original_palette(), Some("dusk"));
        assert_eq!(selection.original_profile(), Some("zen"));
    }

    #[test]
    fn saved_choice_replaces_only_the_changed_axes() {
        let prior = SavedAppearance {
            theme: Some("dusk".to_owned()),
            profile: Some("zen".to_owned()),
        };
        let mut selection = selection(AppearanceAxis::Palette);
        assert_eq!(selection.saved_choice(&prior), prior);
        let _ = selection.step(false);
        assert_eq!(
            selection.saved_choice(&prior),
            SavedAppearance {
                theme: Some("dawn".to_owned()),
                profile: Some("zen".to_owned()),
            }
        );
        let _ = selection.scroll(AppearanceAxis::Profile, false);
        assert_eq!(selection.axis(), AppearanceAxis::Profile);
        assert_eq!(
            selection.saved_choice(&prior).profile.as_deref(),
            Some("bare")
        );
    }

    #[test]
    fn rows_outside_the_catalog_and_empty_catalogs_are_ignored() {
        let mut selection = selection(AppearanceAxis::Palette);
        assert_eq!(
            selection.select_row(AppearanceAxis::Profile, 9),
            AppearanceOutcome::Ignored
        );
        assert_eq!(selection.axis(), AppearanceAxis::Palette);
        assert_eq!(
            selection.select_row(AppearanceAxis::Profile, 1),
            AppearanceOutcome::PreviewProfile(Some("bare".to_owned()))
        );
        assert_eq!(selection.axis(), AppearanceAxis::Profile);
        let mut empty = AppearanceSelection::new(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
            AppearanceAxis::Palette,
        );
        assert_eq!(empty.step(true), AppearanceOutcome::Ignored);
        assert_eq!(empty.apply(), AppearanceOutcome::Ignored);
        assert_eq!(empty.profiles().entries().len(), 1);
    }

    #[test]
    fn an_unlisted_active_value_counts_as_changed_only_once_chosen() {
        let mut selection = AppearanceSelection::new(
            vec![palette("dawn", false)],
            Vec::new(),
            vec![profile("bare")],
            Some("gone".to_owned()),
            AppearanceAxis::Palette,
        );
        assert!(!selection.palette_changed() && !selection.profile_changed());
        let _ = selection.step(true);
        assert!(selection.palette_changed());
        let _ = selection.scroll(AppearanceAxis::Profile, true);
        assert!(selection.profile_changed());
    }

    #[test]
    fn catalogs_list_palettes_and_profiles_with_the_presented_ones_marked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let resolution = resolution(
            dir.path(),
            "nord",
            SavedAppearance {
                theme: None,
                profile: Some("structured".to_owned()),
            },
        );
        let view = resolution.theme_catalog_view(|path| path.display().to_string());
        let selected: Vec<_> = view.entries.iter().filter(|entry| entry.selected).collect();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, "nord");
        assert!(view.entries.iter().all(|entry| entry.source == "bundled"));
        let profiles = resolution.profile_catalog();
        assert!(
            profiles
                .iter()
                .any(|entry| entry.id == "structured" && entry.selected)
        );
        assert!(
            !profiles
                .iter()
                .any(|entry| entry.display_name.ends_with("(profile)"))
        );
        let selection =
            resolution.selection(AppearanceAxis::Profile, |path| path.display().to_string());
        assert_eq!(selection.selected_palette(), Some("nord"));
        assert_eq!(selection.selected_profile(), Some("structured"));
    }

    fn select(selection: &mut AppearanceSelection, palette: &str) {
        let row = selection
            .palettes()
            .entries()
            .iter()
            .position(|entry| entry.id == palette)
            .expect("bundled palette");
        let _ = selection.select_row(AppearanceAxis::Palette, row);
    }

    fn app_resolution(
        dir: &Path,
        bcode: SavedAppearance,
        app: Option<SavedAppearance>,
    ) -> AppearanceResolution {
        AppearanceSources {
            saved: bcode,
            app_saved: app,
            user_config_dir: dir.to_path_buf(),
            project_root: dir.to_path_buf(),
            ..AppearanceSources::default()
        }
        .resolve(TerminalBackground::Unknown)
    }

    fn theme(id: &str) -> SavedAppearance {
        SavedAppearance {
            theme: Some(id.to_owned()),
            profile: None,
        }
    }

    #[test]
    fn preview_matches_apply_and_cancel_restores_the_latest_appearance() {
        let dir = tempfile::tempdir().expect("tempdir");
        let live = LiveAppearance::new(resolution(dir.path(), "nord", SavedAppearance::default()));
        let opened = live.begin_preview();
        let mut selection =
            opened.app_selection(AppearanceAxis::Palette, |path| path.display().to_string());
        assert_eq!(selection.following_bcode(), Some(true));
        select(&mut selection, "dracula");
        assert_eq!(selection.following_bcode(), Some(false));
        assert_eq!(id(&live.preview(&selection)), "dracula");

        // The watcher resolves meanwhile: recorded, not presented.
        let watched = resolution(dir.path(), "catppuccin-mocha", SavedAppearance::default());
        assert!(!live.offer(watched, |_| panic!("presented during a preview")));
        assert_eq!(id(&live.cancel()), "catppuccin-mocha");
        assert!(!live.is_previewing());
        let mut presented = None;
        assert!(live.offer(
            resolution(dir.path(), "nord", SavedAppearance::default()),
            |resolution| presented = Some(id(resolution)),
        ));
        assert_eq!(presented.as_deref(), Some("nord"));
    }

    #[test]
    fn apply_saves_the_apps_own_choice_and_ignores_resolutions_of_the_previous_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let live = LiveAppearance::new(app_resolution(dir.path(), theme("nord"), None));
        let mut selection = live
            .begin_preview()
            .app_selection(AppearanceAxis::Palette, |path| path.display().to_string());
        // Applying an unmoved picker keeps following Bcode and writes nothing.
        assert!(
            live.apply(&selection, |_| Err("written"))
                .expect("nothing to write")
                .follows_bcode()
        );
        let _ = live.begin_preview();
        select(&mut selection, "dracula");
        let mut persisted = None;
        let applied = live
            .apply(&selection, |app| {
                persisted = Some(app.cloned());
                Ok::<(), ()>(())
            })
            .expect("persisted");
        assert_eq!(id(&applied), "dracula");
        let own = persisted
            .expect("persist ran")
            .expect("the app's own choice");
        assert_eq!(own.theme.as_deref(), Some("dracula"));
        assert_eq!(applied.saved.theme.as_deref(), Some("nord"));

        // A watcher tick that read the state before the write is dropped...
        assert!(
            !live.offer(app_resolution(dir.path(), theme("nord"), None), |_| panic!(
                "stale appearance presented"
            ),)
        );
        assert_eq!(id(&live.current()), "dracula");
        // ...the one that read the write presents the same theme...
        let mut presented = None;
        assert!(live.offer(
            app_resolution(dir.path(), theme("nord"), Some(own.clone())),
            |resolution| presented = Some(id(resolution)),
        ));
        assert_eq!(presented.as_deref(), Some("dracula"));
        // ...and a later change of Bcode's choice leaves the app's own in place.
        let mut presented = None;
        assert!(live.offer(
            app_resolution(dir.path(), theme("catppuccin-mocha"), Some(own)),
            |resolution| presented = Some(id(resolution)),
        ));
        assert_eq!(presented.as_deref(), Some("dracula"));
    }

    #[test]
    fn following_bcode_again_previews_and_saves_bcodes_choice() {
        let dir = tempfile::tempdir().expect("tempdir");
        let live = LiveAppearance::new(app_resolution(
            dir.path(),
            theme("nord"),
            Some(theme("dracula")),
        ));
        let mut selection = live
            .begin_preview()
            .app_selection(AppearanceAxis::Palette, |path| path.display().to_string());
        assert_eq!(selection.following_bcode(), Some(false));
        assert_eq!(selection.selected_palette(), Some("dracula"));
        let following = live.set_following(&mut selection, true);
        assert_eq!(id(&following), "nord");
        assert_eq!(selection.selected_palette(), Some("nord"));
        assert_eq!(id(&live.preview(&selection)), "nord");
        let mut persisted = Some(None);
        let applied = live
            .apply(&selection, |app| {
                persisted = Some(app.cloned());
                Ok::<(), ()>(())
            })
            .expect("persisted");
        assert_eq!(persisted, Some(None));
        assert!(applied.follows_bcode());
        // Following again, the app takes Bcode's later choices live.
        let mut presented = None;
        assert!(live.offer(
            app_resolution(dir.path(), theme("catppuccin-mocha"), None),
            |resolution| presented = Some(id(resolution)),
        ));
        assert_eq!(presented.as_deref(), Some("catppuccin-mocha"));

        // Leaving following without moving saves what is shown as the app's own choice.
        let mut selection = live
            .begin_preview()
            .app_selection(AppearanceAxis::Palette, |path| path.display().to_string());
        let own = live.set_following(&mut selection, false);
        assert_eq!(id(&own), "catppuccin-mocha");
        let applied = live
            .apply(&selection, |app| {
                assert_eq!(
                    app.and_then(|app| app.theme.as_deref()),
                    Some("catppuccin-mocha")
                );
                Ok::<(), ()>(())
            })
            .expect("persisted");
        assert!(!applied.follows_bcode());
    }

    #[test]
    fn a_failed_save_ends_the_preview_and_keeps_the_latest_appearance() {
        let dir = tempfile::tempdir().expect("tempdir");
        let live = LiveAppearance::new(resolution(dir.path(), "nord", SavedAppearance::default()));
        let mut selection = live
            .begin_preview()
            .app_selection(AppearanceAxis::Palette, |path| path.display().to_string());
        let _ = selection.step(true);
        assert_eq!(
            live.apply(&selection, |_| Err("read-only")).err(),
            Some("read-only")
        );
        assert!(!live.is_previewing());
        assert_eq!(id(&live.current()), "nord");
    }

    #[test]
    fn a_pinned_palette_previews_and_applies_only_the_profile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pinned = AppearanceSources {
            configured: ThemePreferences {
                name: "nord".to_owned(),
                ..ThemePreferences::default()
            },
            saved: SavedAppearance {
                theme: Some("dracula".to_owned()),
                profile: None,
            },
            user_config_dir: dir.path().to_path_buf(),
            project_root: dir.path().to_path_buf(),
            theme_pinned_by: Some("bcode-git.toml [theme] name".to_owned()),
            ..AppearanceSources::default()
        }
        .resolve(TerminalBackground::Unknown);
        assert_eq!(id(&pinned), "nord");
        assert_eq!(pinned.saved.theme.as_deref(), Some("dracula"));
        let live = LiveAppearance::new(pinned);
        let mut selection = live
            .begin_preview()
            .app_selection(AppearanceAxis::Palette, |path| path.display().to_string());
        let _ = selection.step(true);
        assert_eq!(id(&live.preview(&selection)), "nord");
        let _ = selection.scroll(AppearanceAxis::Profile, true);
        assert!(live.preview(&selection).preferences.profile.is_some());
    }
}
