//! Interactive theme picker state.

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::list::ListState;

use std::collections::BTreeMap;

use super::filtered_list::FilteredListState;
use super::theme::{ResolvedTheme, ThemeCatalogEntry};

/// Theme picker outcome for one key event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemePickerOutcome {
    /// Selection moved and should be previewed.
    Preview(String),
    /// Persist the selected theme.
    Apply(String),
    /// Close and restore the configured theme.
    Cancel,
    /// Event was ignored.
    Ignored,
}

/// Bounded interactive theme picker state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemePickerState {
    entries: Vec<ThemeCatalogEntry>,
    resolved_previews: BTreeMap<String, ResolvedTheme>,
    diagnostics: Vec<String>,
    list: FilteredListState,
}

impl ThemePickerState {
    /// Create a picker from stable catalog rows.
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

    /// Install themes resolved once from the picker catalog.
    pub fn set_resolved_previews(&mut self, resolved: BTreeMap<String, ResolvedTheme>) {
        self.resolved_previews = resolved;
    }

    /// Return one theme resolved when the picker opened.
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
    pub const fn list_offset(&self) -> usize {
        self.list.offset()
    }

    /// Return selected catalog entry.
    #[must_use]
    pub fn selected_entry(&self) -> Option<&ThemeCatalogEntry> {
        self.list
            .selected_source_index()
            .and_then(|selected| self.entries.get(selected))
    }

    /// Return selected theme id.
    #[must_use]
    pub fn selected_id(&self) -> Option<&str> {
        self.selected_entry().map(|entry| entry.id.as_str())
    }

    /// Select one absolute row and preview it.
    pub fn select_row(&mut self, row: usize) -> ThemePickerOutcome {
        let Some(entry) = self.entries.get(row) else {
            return ThemePickerOutcome::Ignored;
        };
        let _selected = self.list.select_visible(row);
        ThemePickerOutcome::Preview(entry.id.clone())
    }

    /// Activate one absolute row and request persistence.
    pub fn activate_row(&mut self, row: usize) -> ThemePickerOutcome {
        let Some(entry) = self.entries.get(row) else {
            return ThemePickerOutcome::Ignored;
        };
        let _selected = self.list.select_visible(row);
        ThemePickerOutcome::Apply(entry.id.clone())
    }

    /// Handle one picker key.
    pub fn handle_key(&mut self, stroke: KeyStroke) -> ThemePickerOutcome {
        match stroke.key {
            KeyCode::Escape => ThemePickerOutcome::Cancel,
            KeyCode::Enter => self
                .selected_id()
                .map_or(ThemePickerOutcome::Ignored, |id| {
                    ThemePickerOutcome::Apply(id.to_owned())
                }),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            _ => ThemePickerOutcome::Ignored,
        }
    }

    fn move_selection(&mut self, delta: isize) -> ThemePickerOutcome {
        if self.entries.is_empty() {
            return ThemePickerOutcome::Ignored;
        }
        if delta < 0 {
            self.list.select_previous();
        } else {
            self.list.select_next();
        }
        self.selected_id()
            .map_or(ThemePickerOutcome::Ignored, |id| {
                ThemePickerOutcome::Preview(id.to_owned())
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, selected: bool) -> ThemeCatalogEntry {
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

    #[test]
    fn mouse_row_selection_previews_then_activates() {
        let mut picker =
            ThemePickerState::new(vec![entry("one", true), entry("two", false)], Vec::new());
        assert_eq!(
            picker.select_row(1),
            ThemePickerOutcome::Preview("two".to_owned())
        );
        assert_eq!(
            picker.activate_row(1),
            ThemePickerOutcome::Apply("two".to_owned())
        );
        assert_eq!(picker.selected_id(), Some("two"));
    }

    #[test]
    fn resolved_previews_are_retrieved_without_re_resolving() {
        let mut picker = ThemePickerState::new(vec![entry("bcode-dark", true)], Vec::new());
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
        picker.set_resolved_previews(BTreeMap::from([("bcode-dark".to_owned(), target)]));

        assert_eq!(picker.resolved_preview("bcode-dark"), Some(target));
        assert_eq!(picker.resolved_preview("missing"), None);
    }

    #[test]
    fn movement_previews_and_escape_cancels() {
        let mut picker =
            ThemePickerState::new(vec![entry("one", true), entry("two", false)], Vec::new());
        assert_eq!(
            picker.handle_key(KeyStroke::simple(KeyCode::Down)),
            ThemePickerOutcome::Preview("two".to_owned())
        );
        assert_eq!(
            picker.handle_key(KeyStroke::simple(KeyCode::Enter)),
            ThemePickerOutcome::Apply("two".to_owned())
        );
        assert_eq!(
            picker.handle_key(KeyStroke::simple(KeyCode::Escape)),
            ThemePickerOutcome::Cancel
        );
    }
}
