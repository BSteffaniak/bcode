//! Combined palette × profile ("appearance") picker state.
//!
//! This is the only interactive appearance picker: `/appearance`, `/theme`,
//! and `/profile` all open it, focused on the axis they name. The selection
//! is the shared [`AppearanceSelection`] every Bcode program's picker uses;
//! one axis is active at a time: the axis keys switch it, and the selection
//! keys move within it. Every movement is reported as a preview outcome so
//! the caller applies it live; cancelling restores both selections the
//! picker opened with. The column lists ([`PaletteAxisState`],
//! [`ProfileAxisState`]) follow the selection for scrolling and hit testing.

use bcode_tui_theme::picker::AppearanceSelection;
use bmux_keyboard::KeyStroke;

use super::appearance_picker_axes::{PaletteAxisState, ProfileAxisState};
use super::theme::{ProfileCatalogEntry, ThemeCatalogEntry};

pub use bcode_tui_theme::picker::AppearanceAxis;
/// Appearance picker outcome for one input event.
pub use bcode_tui_theme::picker::AppearanceOutcome as AppearancePickerOutcome;

/// Keymap scope of the appearance picker.
pub const APPEARANCE_PICKER_SCOPE: &str = "appearance_picker";

/// Former single-axis picker scopes, accepted in keybinding configuration as
/// aliases of [`APPEARANCE_PICKER_SCOPE`] (earlier entries take precedence).
pub const LEGACY_PICKER_SCOPES: [&str; 2] = ["theme_picker", "profile_picker"];

/// Return the axis a palette route focuses: `profile.select` focuses
/// profiles; `theme.select`, `appearance.select`, and any other route focus
/// palettes.
#[must_use]
pub fn axis_for_route(route: &str) -> AppearanceAxis {
    if route == "profile.select" {
        AppearanceAxis::Profile
    } else {
        AppearanceAxis::Palette
    }
}

/// A profile selection waiting to be persisted after the theme write of a
/// combined apply, so the two user-state writes never race.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingProfilePersist {
    /// Profile to persist (`None` clears it).
    pub profile: Option<String>,
}

/// Bounded interactive palette × profile picker state.
///
/// The selection itself (axes, rows, change tracking, outcomes) is the shared
/// [`AppearanceSelection`]; this state adds the list scroll state of each
/// column, the palettes resolved for previewing, and the sample preview.
#[derive(Debug, Clone)]
pub struct AppearancePickerState {
    selection: AppearanceSelection,
    palettes: PaletteAxisState,
    profiles: ProfileAxisState,
    preview: super::appearance_picker_sample::SamplePreview,
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
        let selection =
            AppearanceSelection::new(palettes, diagnostics, profiles, active_profile, axis);
        let mut state = Self {
            palettes: PaletteAxisState::new(selection.palettes()),
            profiles: ProfileAxisState::new(selection.profiles()),
            selection,
            preview: super::appearance_picker_sample::SamplePreview::default(),
        };
        state.sync_lists();
        state
    }

    /// Return the active axis.
    #[must_use]
    pub const fn axis(&self) -> AppearanceAxis {
        self.selection.axis()
    }

    /// Make `axis` active; returns whether it changed.
    pub fn set_axis(&mut self, axis: AppearanceAxis) -> bool {
        self.selection.set_axis(axis)
    }

    /// Return the palette list state.
    #[must_use]
    pub const fn palettes(&self) -> &PaletteAxisState {
        &self.palettes
    }

    /// Return the palette list state mutably (list rendering, preview cache).
    pub const fn palettes_mut(&mut self) -> &mut PaletteAxisState {
        &mut self.palettes
    }

    /// Return the profile list state.
    #[must_use]
    pub const fn profiles(&self) -> &ProfileAxisState {
        &self.profiles
    }

    /// Return the profile list state mutably (list rendering).
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
        self.selection.original_palette()
    }

    /// Return the profile active when the picker opened.
    #[must_use]
    pub fn original_profile(&self) -> Option<&str> {
        self.selection.original_profile()
    }

    /// Return the selected palette id.
    #[must_use]
    pub fn selected_palette(&self) -> Option<&str> {
        self.selection.selected_palette()
    }

    /// Return the selected profile id (`None` for the theme default row).
    #[cfg(test)]
    #[must_use]
    pub fn selected_profile(&self) -> Option<&str> {
        self.selection.selected_profile()
    }

    /// Whether the palette selection differs from the configured theme
    /// ([`AppearanceSelection::palette_changed`]).
    #[must_use]
    pub fn palette_changed(&self) -> bool {
        self.selection.palette_changed()
    }

    /// Whether the profile selection differs from the configured profile
    /// ([`AppearanceSelection::profile_changed`]).
    #[must_use]
    pub fn profile_changed(&self) -> bool {
        self.selection.profile_changed()
    }

    /// Return the number of rows on `axis`.
    #[must_use]
    pub fn row_count(&self, axis: AppearanceAxis) -> usize {
        self.selection.row_count(axis)
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
        let outcome = self.selection.select_row(axis, row);
        self.sync_lists();
        outcome
    }

    /// Make `axis` active and move its selection one row, as one mouse
    /// wheel notch over its column does (`forward` is down).
    pub fn scroll(&mut self, axis: AppearanceAxis, forward: bool) -> AppearancePickerOutcome {
        let outcome = self.selection.scroll(axis, forward);
        self.sync_lists();
        outcome
    }

    /// Request persistence of both current selections.
    #[must_use]
    pub fn apply(&self) -> AppearancePickerOutcome {
        self.selection.apply()
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
        let outcome = match keymap.action(APPEARANCE_PICKER_SCOPE, stroke) {
            Some("tui.control.cancel") => AppearancePickerOutcome::Cancel,
            Some("tui.control.submit") => self.apply(),
            Some("tui.control.up") => self.selection.step(false),
            Some("tui.control.down") => self.selection.step(true),
            Some("tui.control.left") => self.selection.switch_to(AppearanceAxis::Palette),
            Some("tui.control.right") => self.selection.switch_to(AppearanceAxis::Profile),
            Some("tui.control.focus_next" | "tui.control.focus_previous") => {
                self.selection.toggle_axis()
            }
            _ => AppearancePickerOutcome::Ignored,
        };
        self.sync_lists();
        outcome
    }

    /// Move each column's list cursor to the selection's row.
    const fn sync_lists(&mut self) {
        if let Some(row) = self.selection.selected_index(AppearanceAxis::Palette) {
            self.palettes.show_row(row);
        }
        if let Some(row) = self.selection.selected_index(AppearanceAxis::Profile) {
            self.profiles.show_row(row);
        }
    }
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
