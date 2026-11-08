//! Per-axis list state of the appearance picker.
//!
//! Each axis owns its catalog rows and bounded list state. Axis states only
//! move their selection and report the newly selected id; the combined
//! [`AppearancePickerState`](super::appearance_picker::AppearancePickerState)
//! owns input handling, preview outcomes, and change tracking.

use std::collections::BTreeMap;

use bmux_tui_components::selectable_list::SelectableListState as ListState;

use super::filtered_list::FilteredListState;
use super::theme::{ProfileCatalogEntry, ResolvedTheme, ThemeCatalogEntry};

/// Palette (complete theme) axis: catalog rows, rejected-candidate
/// diagnostics, and palettes resolved lazily for previewing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteAxisState {
    entries: Vec<ThemeCatalogEntry>,
    resolved_previews: BTreeMap<String, ResolvedTheme>,
    diagnostics: Vec<String>,
    list: FilteredListState,
}

impl PaletteAxisState {
    /// Create the axis from stable catalog rows, selecting the row marked
    /// `selected` (or the first row).
    #[must_use]
    pub fn new(entries: Vec<ThemeCatalogEntry>, diagnostics: Vec<String>) -> Self {
        let selected = entries.iter().position(|entry| entry.selected).unwrap_or(0);
        let mut list = FilteredListState::new(entries.len());
        let _selected = list.select_visible(selected);
        Self {
            entries,
            resolved_previews: BTreeMap::new(),
            diagnostics,
            list,
        }
    }

    /// Return catalog rows.
    #[must_use]
    pub fn entries(&self) -> &[ThemeCatalogEntry] {
        &self.entries
    }

    /// Cache one resolved preview (palettes resolve lazily under the profile
    /// being previewed).
    pub fn insert_resolved_preview(&mut self, id: String, resolved: &ResolvedTheme) {
        self.resolved_previews.insert(id, *resolved);
    }

    /// Drop every cached preview, for example after the profile overlay changed.
    pub fn clear_resolved_previews(&mut self) {
        self.resolved_previews.clear();
    }

    /// Return one cached resolved preview.
    #[must_use]
    pub fn resolved_preview(&self, id: &str) -> Option<ResolvedTheme> {
        self.resolved_previews.get(id).copied()
    }

    /// Return bounded rejected-candidate diagnostics.
    #[must_use]
    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    /// Synchronize list visibility before rendering and return its render state.
    pub fn list_render_state(&mut self, viewport_height: u16) -> &mut ListState {
        self.list.render_state(viewport_height)
    }

    /// Return the current list offset for hit testing.
    #[must_use]
    pub fn list_offset(&self) -> usize {
        self.list.offset()
    }

    /// Return the selected catalog entry.
    #[must_use]
    pub fn selected_entry(&self) -> Option<&ThemeCatalogEntry> {
        self.list
            .selected_source_index()
            .and_then(|selected| self.entries.get(selected))
    }

    /// Return the selected theme id.
    #[must_use]
    pub fn selected_id(&self) -> Option<&str> {
        self.selected_entry().map(|entry| entry.id.as_str())
    }

    /// Select one absolute row; returns the newly selected id.
    pub fn select_row(&mut self, row: usize) -> Option<String> {
        let id = self.entries.get(row)?.id.clone();
        let _selected = self.list.select_visible(row);
        Some(id)
    }

    /// Move the selection one row (`forward` is down); returns the newly
    /// selected id.
    pub fn step(&mut self, forward: bool) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        if forward {
            self.list.select_next();
        } else {
            self.list.select_previous();
        }
        self.selected_id().map(str::to_owned)
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

/// Profile (interface overlay) axis: a "Theme default" row followed by the
/// profile catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileAxisState {
    entries: Vec<ProfileRow>,
    original: Option<String>,
    list: FilteredListState,
}

impl ProfileAxisState {
    /// Create the axis from catalog rows; `original` is the active profile.
    #[must_use]
    pub fn new(catalog: Vec<ProfileCatalogEntry>, original: Option<String>) -> Self {
        let entries = std::iter::once(ProfileRow {
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
        let selected = entries
            .iter()
            .position(|entry| entry.id == original)
            .unwrap_or(0);
        let mut list = FilteredListState::new(entries.len());
        let _selected = list.select_visible(selected);
        Self {
            entries,
            original,
            list,
        }
    }

    /// Return rows.
    #[must_use]
    pub fn entries(&self) -> &[ProfileRow] {
        &self.entries
    }

    /// Return the profile active when the picker opened.
    #[must_use]
    pub fn original(&self) -> Option<&str> {
        self.original.as_deref()
    }

    /// Synchronize list visibility before rendering and return its render state.
    pub fn list_render_state(&mut self, viewport_height: u16) -> &mut ListState {
        self.list.render_state(viewport_height)
    }

    /// Return the current list offset for hit testing.
    #[must_use]
    pub fn list_offset(&self) -> usize {
        self.list.offset()
    }

    /// Return the selected row.
    #[must_use]
    pub fn selected_entry(&self) -> Option<&ProfileRow> {
        self.list
            .selected_source_index()
            .and_then(|selected| self.entries.get(selected))
    }

    /// Select one absolute row; returns the newly selected row.
    pub fn select_row(&mut self, row: usize) -> Option<&ProfileRow> {
        if row >= self.entries.len() {
            return None;
        }
        let _selected = self.list.select_visible(row);
        self.entries.get(row)
    }

    /// Move the selection one row (`forward` is down); returns the newly
    /// selected row.
    pub fn step(&mut self, forward: bool) -> Option<&ProfileRow> {
        if forward {
            self.list.select_next();
        } else {
            self.list.select_previous();
        }
        self.selected_entry()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette(id: &str, selected: bool) -> ThemeCatalogEntry {
        ThemeCatalogEntry {
            id: id.to_owned(),
            display_name: id.to_owned(),
            source: "bundled".to_owned(),
            has_dark_variant: false,
            has_light_variant: false,
            validation: "valid".to_owned(),
            selected,
        }
    }

    fn profiles() -> Vec<ProfileCatalogEntry> {
        ["bare", "zen"]
            .into_iter()
            .map(|id| ProfileCatalogEntry {
                id: id.to_owned(),
                display_name: id.to_owned(),
                description: None,
                selected: false,
            })
            .collect()
    }

    #[test]
    fn palette_axis_caches_resolved_previews_until_cleared() {
        let mut axis = PaletteAxisState::new(vec![palette("bcode-dark", true)], Vec::new());
        let catalog =
            super::super::theme::definition::ThemeCatalog::bundled().expect("bundled themes parse");
        let definition = catalog
            .resolve(&super::super::theme::definition::ThemeSelection::new(
                "bcode-dark",
            ))
            .expect("dark resolves");
        let target = super::super::theme::resolved_definition_theme(
            Some(&definition),
            super::super::theme::PENDING_AGENT_METADATA_ACCENT,
        );
        axis.insert_resolved_preview("bcode-dark".to_owned(), &target);
        assert_eq!(axis.resolved_preview("bcode-dark"), Some(target));
        assert_eq!(axis.resolved_preview("missing"), None);
        axis.clear_resolved_previews();
        assert_eq!(axis.resolved_preview("bcode-dark"), None);
    }

    #[test]
    fn no_active_profile_selects_theme_default() {
        let axis = ProfileAxisState::new(profiles(), None);
        assert_eq!(
            axis.selected_entry().map(|entry| entry.id.clone()),
            Some(None)
        );
        assert_eq!(axis.entries().len(), 3);
    }

    #[test]
    fn rows_outside_the_catalog_are_ignored() {
        let mut palettes = PaletteAxisState::new(vec![palette("one", true)], Vec::new());
        assert_eq!(palettes.select_row(1), None);
        assert_eq!(palettes.selected_id(), Some("one"));
        let mut profiles = ProfileAxisState::new(profiles(), Some("zen".to_owned()));
        assert_eq!(profiles.select_row(9), None);
        assert_eq!(profiles.select_row(0).map(|row| row.id.clone()), Some(None));
    }
}
