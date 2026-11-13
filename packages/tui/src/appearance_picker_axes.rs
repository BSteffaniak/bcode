//! Per-column list state of the appearance picker.
//!
//! The rows and the selection belong to the shared
//! [`AppearanceSelection`](bcode_tui_theme::picker::AppearanceSelection); each
//! column here keeps the rows it paints, its bounded list scroll state (moved
//! to the selected row by the picker), and, for palettes, the themes resolved
//! for previewing.

use std::collections::BTreeMap;

use bcode_tui_theme::picker::{PaletteAxis, ProfileAxis};
use bmux_tui_components::selectable_list::SelectableListState as ListState;

use super::filtered_list::FilteredListState;
use super::theme::{ResolvedTheme, ThemeCatalogEntry};

pub use bcode_tui_theme::picker::ProfileRow;

/// Palette (complete theme) column: catalog rows, rejected-candidate
/// diagnostics, and palettes resolved lazily for previewing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteAxisState {
    entries: Vec<ThemeCatalogEntry>,
    resolved_previews: BTreeMap<String, ResolvedTheme>,
    diagnostics: Vec<String>,
    list: FilteredListState,
}

impl PaletteAxisState {
    /// Create the column for the palette axis of a selection.
    #[must_use]
    pub fn new(axis: &PaletteAxis) -> Self {
        let mut list = FilteredListState::new(axis.entries().len());
        if let Some(row) = axis.selected_index() {
            let _selected = list.select_visible(row);
        }
        Self {
            entries: axis.entries().to_vec(),
            resolved_previews: BTreeMap::new(),
            diagnostics: axis.diagnostics().to_vec(),
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

    /// Return the catalog entry under the list cursor.
    #[must_use]
    pub fn selected_entry(&self) -> Option<&ThemeCatalogEntry> {
        self.list
            .selected_source_index()
            .and_then(|selected| self.entries.get(selected))
    }

    /// Move the list cursor to `row` (the selection's row).
    pub const fn show_row(&mut self, row: usize) {
        let _selected = self.list.select_visible(row);
    }
}

/// Profile (interface overlay) column: a "Theme default" row followed by the
/// profile catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileAxisState {
    entries: Vec<ProfileRow>,
    list: FilteredListState,
}

impl ProfileAxisState {
    /// Create the column for the profile axis of a selection.
    #[must_use]
    pub fn new(axis: &ProfileAxis) -> Self {
        let mut list = FilteredListState::new(axis.entries().len());
        let _selected = list.select_visible(axis.selected_index());
        Self {
            entries: axis.entries().to_vec(),
            list,
        }
    }

    /// Return rows.
    #[must_use]
    pub fn entries(&self) -> &[ProfileRow] {
        &self.entries
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

    /// Return the row under the list cursor.
    #[must_use]
    pub fn selected_entry(&self) -> Option<&ProfileRow> {
        self.list
            .selected_source_index()
            .and_then(|selected| self.entries.get(selected))
    }

    /// Move the list cursor to `row` (the selection's row).
    pub const fn show_row(&mut self, row: usize) {
        let _selected = self.list.select_visible(row);
    }
}

#[cfg(test)]
mod tests {
    use bcode_tui_theme::picker::{AppearanceAxis, AppearanceSelection, ProfileCatalogEntry};

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

    fn selection(palettes: Vec<ThemeCatalogEntry>, profile: Option<&str>) -> AppearanceSelection {
        let profiles = ["bare", "zen"]
            .into_iter()
            .map(|id| ProfileCatalogEntry {
                id: id.to_owned(),
                display_name: id.to_owned(),
                description: None,
                selected: false,
            })
            .collect();
        AppearanceSelection::new(
            palettes,
            Vec::new(),
            profiles,
            profile.map(str::to_owned),
            AppearanceAxis::Palette,
        )
    }

    #[test]
    fn palette_axis_caches_resolved_previews_until_cleared() {
        let mut axis =
            PaletteAxisState::new(selection(vec![palette("bcode-dark", true)], None).palettes());
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
    fn columns_open_on_the_selected_rows() {
        let selection = selection(vec![palette("one", false), palette("two", true)], None);
        let palettes = PaletteAxisState::new(selection.palettes());
        assert_eq!(
            palettes.selected_entry().map(|entry| entry.id.as_str()),
            Some("two")
        );
        let profiles = ProfileAxisState::new(selection.profiles());
        assert_eq!(
            profiles.selected_entry().map(|entry| entry.id.clone()),
            Some(None)
        );
        assert_eq!(profiles.entries().len(), 3);
    }
}
