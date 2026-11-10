//! Combined palette × profile ("appearance") picker state.
//!
//! This is the only interactive appearance picker: `/appearance`, `/theme`,
//! and `/profile` all open it, focused on the axis they name. The picker
//! composes a [`PaletteAxisState`] and a [`ProfileAxisState`]; one axis is
//! active at a time: the axis keys switch it, and the selection keys move
//! within it. Every movement is reported as a preview outcome so the caller
//! applies it live; cancelling restores both selections the picker opened
//! with.

use bmux_keyboard::KeyStroke;

use super::appearance_picker_axes::{PaletteAxisState, ProfileAxisState, ProfileRow};
use super::theme::{ProfileCatalogEntry, ThemeCatalogEntry};

/// Keymap scope of the appearance picker.
pub const APPEARANCE_PICKER_SCOPE: &str = "appearance_picker";

/// Former single-axis picker scopes, accepted in keybinding configuration as
/// aliases of [`APPEARANCE_PICKER_SCOPE`] (earlier entries take precedence).
pub const LEGACY_PICKER_SCOPES: [&str; 2] = ["theme_picker", "profile_picker"];

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

    /// Return the axis a palette route focuses: `profile.select` focuses
    /// profiles; `theme.select`, `appearance.select`, and any other route
    /// focus palettes.
    #[must_use]
    pub fn for_route(route: &str) -> Self {
        if route == "profile.select" {
            Self::Profile
        } else {
            Self::Palette
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

/// A profile selection waiting to be persisted after the theme write of a
/// combined apply, so the two user-state writes never race.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingProfilePersist {
    /// Profile to persist (`None` clears it).
    pub profile: Option<String>,
}

/// Appearance picker outcome for one input event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppearancePickerOutcome {
    /// The palette selection moved; preview it without persisting.
    PreviewPalette(String),
    /// The profile selection moved; apply it without persisting (`None`
    /// clears the profile overlay).
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
    /// The event was ignored.
    Ignored,
}

/// Bounded interactive palette × profile picker state.
#[derive(Debug, Clone)]
pub struct AppearancePickerState {
    palettes: PaletteAxisState,
    profiles: ProfileAxisState,
    preview: super::appearance_picker_sample::SamplePreview,
    axis: AppearanceAxis,
    original_palette: Option<String>,
    /// Whether the configured profile has a row (`None` always does).
    original_profile_listed: bool,
    /// Whether the user selected a row on each axis. An axis whose
    /// configured value has no row starts on a fallback row the user never
    /// chose, so only an explicit selection counts as a change there.
    palette_chosen: bool,
    profile_chosen: bool,
}

impl AppearancePickerState {
    /// Create a picker from the palette and profile catalogs.
    ///
    /// `active_profile` is the profile active when the picker opened; the
    /// active palette is the catalog row marked `selected`. `axis` is the
    /// axis that starts active.
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
        let profiles = ProfileAxisState::new(profiles, active_profile);
        let original_profile_listed = profiles
            .entries()
            .iter()
            .any(|entry| entry.id.as_deref() == profiles.original());
        Self {
            palettes: PaletteAxisState::new(palettes, diagnostics),
            profiles,
            preview: super::appearance_picker_sample::SamplePreview::default(),
            axis,
            original_palette,
            original_profile_listed,
            palette_chosen: false,
            profile_chosen: false,
        }
    }

    /// Return the active axis.
    #[must_use]
    pub const fn axis(&self) -> AppearanceAxis {
        self.axis
    }

    /// Make `axis` active; returns whether it changed.
    pub fn set_axis(&mut self, axis: AppearanceAxis) -> bool {
        std::mem::replace(&mut self.axis, axis) != axis
    }

    /// Return the palette axis state.
    #[must_use]
    pub const fn palettes(&self) -> &PaletteAxisState {
        &self.palettes
    }

    /// Return the palette axis state mutably (list rendering, preview cache).
    pub const fn palettes_mut(&mut self) -> &mut PaletteAxisState {
        &mut self.palettes
    }

    /// Return the profile axis state.
    #[must_use]
    pub const fn profiles(&self) -> &ProfileAxisState {
        &self.profiles
    }

    /// Return the profile axis state mutably (list rendering).
    pub const fn profiles_mut(&mut self) -> &mut ProfileAxisState {
        &mut self.profiles
    }

    /// Return the sample preview rendered in the preview pane.
    pub const fn preview_mut(&mut self) -> &mut super::appearance_picker_sample::SamplePreview {
        &mut self.preview
    }

    /// Re-resolve the preview's inputs for the selected palette (the
    /// configured theme when none is selected) under `app`'s live profile and
    /// structure configuration. Runs outside drawing; returns whether the
    /// inputs changed.
    pub fn refresh_preview(&mut self, app: &mut super::app::BmuxApp) -> bool {
        let palette = self
            .selected_palette()
            .map_or_else(|| app.tui_config().theme.name.clone(), str::to_owned);
        self.preview.refresh(app, &palette)
    }

    /// Return the palette active when the picker opened, if it is listed.
    #[cfg(test)]
    #[must_use]
    pub fn original_palette(&self) -> Option<&str> {
        self.original_palette.as_deref()
    }

    /// Return the profile active when the picker opened.
    #[must_use]
    pub fn original_profile(&self) -> Option<&str> {
        self.profiles.original()
    }

    /// Return the selected palette id.
    #[must_use]
    pub fn selected_palette(&self) -> Option<&str> {
        self.palettes.selected_id()
    }

    /// Return the selected profile id (`None` for the theme default row).
    #[must_use]
    pub fn selected_profile(&self) -> Option<&str> {
        self.profiles
            .selected_entry()
            .and_then(|entry| entry.id.as_deref())
    }

    /// Whether the palette selection differs from the configured theme.
    ///
    /// When the configured theme has no catalog row, the picker starts on a
    /// fallback row; the palette counts as changed only once the user
    /// selected a row.
    #[must_use]
    pub fn palette_changed(&self) -> bool {
        self.original_palette
            .as_deref()
            .map_or(self.palette_chosen, |original| {
                self.selected_palette() != Some(original)
            })
    }

    /// Whether the profile selection differs from the configured profile,
    /// with the same fallback rule as [`Self::palette_changed`] for a
    /// configured profile that has no row.
    #[must_use]
    pub fn profile_changed(&self) -> bool {
        if self.original_profile_listed {
            self.selected_profile() != self.original_profile()
        } else {
            self.profile_chosen
        }
    }

    /// Return the number of rows on `axis`.
    #[must_use]
    pub fn row_count(&self, axis: AppearanceAxis) -> usize {
        match axis {
            AppearanceAxis::Palette => self.palettes.entries().len(),
            AppearanceAxis::Profile => self.profiles.entries().len(),
        }
    }

    /// Return the list scroll offset of `axis` for hit testing.
    #[must_use]
    pub fn list_offset(&self, axis: AppearanceAxis) -> usize {
        match axis {
            AppearanceAxis::Palette => self.palettes.list_offset(),
            AppearanceAxis::Profile => self.profiles.list_offset(),
        }
    }

    /// Make `axis` active and select one absolute row on it.
    pub fn select_row(&mut self, axis: AppearanceAxis, row: usize) -> AppearancePickerOutcome {
        if row >= self.row_count(axis) {
            return AppearancePickerOutcome::Ignored;
        }
        self.axis = axis;
        let outcome = match axis {
            AppearanceAxis::Palette => palette_outcome(self.palettes.select_row(row)),
            AppearanceAxis::Profile => profile_outcome(self.profiles.select_row(row)),
        };
        self.note_choice(&outcome);
        outcome
    }

    /// Make `axis` active and move its selection one row, as one mouse
    /// wheel notch over its column does (`forward` is down).
    pub fn scroll(&mut self, axis: AppearanceAxis, forward: bool) -> AppearancePickerOutcome {
        self.axis = axis;
        self.step(forward)
    }

    /// Request persistence of both current selections.
    #[must_use]
    pub fn apply(&self) -> AppearancePickerOutcome {
        self.selected_palette()
            .map_or(AppearancePickerOutcome::Ignored, |palette| {
                AppearancePickerOutcome::Apply {
                    palette: palette.to_owned(),
                    profile: self.selected_profile().map(str::to_owned),
                }
            })
    }

    /// Handle one picker key.
    #[cfg(test)]
    pub fn handle_key(&mut self, stroke: KeyStroke) -> AppearancePickerOutcome {
        self.handle_key_with_keymap(
            stroke,
            &super::keymap::BmuxKeyMap::from_config(&bcode_config::TuiConfig::default()),
        )
    }

    /// Handle input using the frontend's effective registry.
    pub fn handle_key_with_keymap(
        &mut self,
        stroke: KeyStroke,
        keymap: &super::keymap::BmuxKeyMap,
    ) -> AppearancePickerOutcome {
        match keymap.action(APPEARANCE_PICKER_SCOPE, stroke) {
            Some("tui.control.cancel") => AppearancePickerOutcome::Cancel,
            Some("tui.control.submit") => self.apply(),
            Some("tui.control.up") => self.step(false),
            Some("tui.control.down") => self.step(true),
            Some("tui.control.left") => self.switch_to(AppearanceAxis::Palette),
            Some("tui.control.right") => self.switch_to(AppearanceAxis::Profile),
            Some("tui.control.focus_next" | "tui.control.focus_previous") => {
                self.switch_to(self.axis.other())
            }
            _ => AppearancePickerOutcome::Ignored,
        }
    }

    fn switch_to(&mut self, axis: AppearanceAxis) -> AppearancePickerOutcome {
        if self.set_axis(axis) {
            AppearancePickerOutcome::AxisChanged(axis)
        } else {
            AppearancePickerOutcome::Ignored
        }
    }

    fn step(&mut self, forward: bool) -> AppearancePickerOutcome {
        let outcome = match self.axis {
            AppearanceAxis::Palette => palette_outcome(self.palettes.step(forward)),
            AppearanceAxis::Profile => profile_outcome(self.profiles.step(forward)),
        };
        self.note_choice(&outcome);
        outcome
    }

    /// Record that the user selected a row on the axis `outcome` previews.
    const fn note_choice(&mut self, outcome: &AppearancePickerOutcome) {
        match outcome {
            AppearancePickerOutcome::PreviewPalette(_) => self.palette_chosen = true,
            AppearancePickerOutcome::PreviewProfile(_) => self.profile_chosen = true,
            _ => {}
        }
    }
}

fn palette_outcome(selected: Option<String>) -> AppearancePickerOutcome {
    selected.map_or(
        AppearancePickerOutcome::Ignored,
        AppearancePickerOutcome::PreviewPalette,
    )
}

fn profile_outcome(selected: Option<&ProfileRow>) -> AppearancePickerOutcome {
    selected.map_or(AppearancePickerOutcome::Ignored, |row| {
        AppearancePickerOutcome::PreviewProfile(row.id.clone())
    })
}

#[cfg(test)]
pub mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use bmux_keyboard::{KeyCode, parse_key_stroke};

    pub fn palette(id: &str, selected: bool) -> ThemeCatalogEntry {
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

    pub fn profile(id: &str) -> ProfileCatalogEntry {
        ProfileCatalogEntry {
            id: id.to_owned(),
            display_name: id.to_owned(),
            description: Some(format!("{id} profile")),
            selected: false,
        }
    }

    pub fn picker(axis: AppearanceAxis) -> AppearancePickerState {
        AppearancePickerState::new(
            vec![palette("dawn", false), palette("dusk", true)],
            Vec::new(),
            vec![profile("bare"), profile("zen")],
            Some("zen".to_owned()),
            axis,
        )
    }

    #[test]
    fn opens_on_both_active_selections() {
        let picker = picker(AppearanceAxis::Profile);
        assert_eq!(picker.axis(), AppearanceAxis::Profile);
        assert_eq!(picker.selected_palette(), Some("dusk"));
        assert_eq!(picker.selected_profile(), Some("zen"));
        assert_eq!(picker.original_palette(), Some("dusk"));
        assert_eq!(picker.original_profile(), Some("zen"));
    }

    #[test]
    fn vertical_keys_move_only_the_active_axis() {
        let mut picker = picker(AppearanceAxis::Palette);
        assert_eq!(
            picker.handle_key(KeyStroke::simple(KeyCode::Up)),
            AppearancePickerOutcome::PreviewPalette("dawn".to_owned())
        );
        assert_eq!(picker.selected_profile(), Some("zen"));
        assert_eq!(
            picker.handle_key(KeyStroke::simple(KeyCode::Right)),
            AppearancePickerOutcome::AxisChanged(AppearanceAxis::Profile)
        );
        assert_eq!(
            picker.handle_key(KeyStroke::simple(KeyCode::Char('k'))),
            AppearancePickerOutcome::PreviewProfile(Some("bare".to_owned()))
        );
        assert_eq!(
            picker.handle_key(KeyStroke::simple(KeyCode::Up)),
            AppearancePickerOutcome::PreviewProfile(None)
        );
        assert_eq!(picker.selected_palette(), Some("dawn"));
        assert_eq!(
            picker.handle_key(KeyStroke::simple(KeyCode::Enter)),
            AppearancePickerOutcome::Apply {
                palette: "dawn".to_owned(),
                profile: None,
            }
        );
        assert_eq!(
            picker.handle_key(KeyStroke::simple(KeyCode::Escape)),
            AppearancePickerOutcome::Cancel
        );
        // Cancel restores what the picker opened with.
        assert_eq!(picker.original_palette(), Some("dusk"));
        assert_eq!(picker.original_profile(), Some("zen"));
    }

    #[test]
    fn axis_keys_switch_columns_and_report_no_change_at_the_edge() {
        let mut picker = picker(AppearanceAxis::Palette);
        assert_eq!(
            picker.handle_key(KeyStroke::simple(KeyCode::Left)),
            AppearancePickerOutcome::Ignored
        );
        assert_eq!(
            picker.handle_key(KeyStroke::simple(KeyCode::Char('l'))),
            AppearancePickerOutcome::AxisChanged(AppearanceAxis::Profile)
        );
        assert_eq!(
            picker.handle_key(KeyStroke::simple(KeyCode::Tab)),
            AppearancePickerOutcome::AxisChanged(AppearanceAxis::Palette)
        );
        assert_eq!(
            picker.handle_key(KeyStroke::simple(KeyCode::Char('h'))),
            AppearancePickerOutcome::Ignored
        );
    }

    #[test]
    fn axis_keys_are_remappable() {
        let mut config = bcode_config::TuiConfig::default();
        config.keybindings.scopes.insert(
            APPEARANCE_PICKER_SCOPE.into(),
            BTreeMap::from([
                ("ctrl+f".into(), "tui.control.right".into()),
                ("right".into(), String::new()),
            ]),
        );
        let keymap = super::super::keymap::BmuxKeyMap::from_config(&config);
        let mut picker = picker(AppearanceAxis::Palette);
        assert_eq!(
            picker.handle_key_with_keymap(KeyStroke::simple(KeyCode::Right), &keymap),
            AppearancePickerOutcome::Ignored
        );
        assert_eq!(
            picker.handle_key_with_keymap(parse_key_stroke("ctrl+f").unwrap(), &keymap),
            AppearancePickerOutcome::AxisChanged(AppearanceAxis::Profile)
        );
    }

    #[test]
    fn legacy_single_axis_scopes_still_remap_the_picker() {
        let mut config = bcode_config::TuiConfig::default();
        config.keybindings.scopes.insert(
            "theme_picker".into(),
            BTreeMap::from([
                ("ctrl+n".into(), "tui.control.down".into()),
                ("escape".into(), String::new()),
            ]),
        );
        let keymap = super::super::keymap::BmuxKeyMap::from_config(&config);
        let mut picker = picker(AppearanceAxis::Palette);
        assert_eq!(
            picker.handle_key_with_keymap(KeyStroke::simple(KeyCode::Down), &keymap),
            AppearancePickerOutcome::Ignored
        );
        assert_eq!(
            picker.handle_key_with_keymap(KeyStroke::simple(KeyCode::Up), &keymap),
            AppearancePickerOutcome::PreviewPalette("dawn".to_owned())
        );
        assert_eq!(
            picker.handle_key_with_keymap(parse_key_stroke("ctrl+n").unwrap(), &keymap),
            AppearancePickerOutcome::PreviewPalette("dusk".to_owned())
        );
        assert_eq!(
            picker.handle_key_with_keymap(KeyStroke::simple(KeyCode::Escape), &keymap),
            AppearancePickerOutcome::Ignored
        );
    }

    #[test]
    fn wheel_scroll_focuses_and_moves_the_scrolled_axis() {
        let mut picker = picker(AppearanceAxis::Palette);
        assert_eq!(
            picker.scroll(AppearanceAxis::Profile, false),
            AppearancePickerOutcome::PreviewProfile(Some("bare".to_owned()))
        );
        assert_eq!(picker.axis(), AppearanceAxis::Profile);
        assert!(picker.profile_changed());
        assert_eq!(
            picker.scroll(AppearanceAxis::Palette, false),
            AppearancePickerOutcome::PreviewPalette("dawn".to_owned())
        );
        assert_eq!(picker.axis(), AppearanceAxis::Palette);
        assert_eq!(picker.selected_profile(), Some("bare"));
    }

    #[test]
    fn row_selection_focuses_its_axis() {
        let mut picker = picker(AppearanceAxis::Palette);
        assert_eq!(
            picker.select_row(AppearanceAxis::Profile, 1),
            AppearancePickerOutcome::PreviewProfile(Some("bare".to_owned()))
        );
        assert_eq!(picker.axis(), AppearanceAxis::Profile);
        assert_eq!(
            picker.select_row(AppearanceAxis::Palette, 9),
            AppearancePickerOutcome::Ignored
        );
        assert_eq!(picker.axis(), AppearanceAxis::Profile);
    }

    #[test]
    fn empty_palette_catalog_cannot_apply() {
        let mut picker = AppearancePickerState::new(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            None,
            AppearanceAxis::Palette,
        );
        assert_eq!(
            picker.handle_key(KeyStroke::simple(KeyCode::Down)),
            AppearancePickerOutcome::Ignored
        );
        assert_eq!(
            picker.handle_key(KeyStroke::simple(KeyCode::Enter)),
            AppearancePickerOutcome::Ignored
        );
    }
}
