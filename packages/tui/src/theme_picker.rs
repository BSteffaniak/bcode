//! Interactive theme picker state.

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::list::ListState;

use super::theme::ThemeCatalogEntry;

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
    diagnostics: Vec<String>,
    list: ListState,
}

impl ThemePickerState {
    /// Create a picker from stable catalog rows.
    #[must_use]
    pub fn new(entries: Vec<ThemeCatalogEntry>, diagnostics: Vec<String>) -> Self {
        let mut list = ListState::default();
        list.select(entries.iter().position(|entry| entry.selected).or(Some(0)));
        Self {
            entries,
            diagnostics,
            list,
        }
    }

    /// Return catalog rows.
    #[must_use]
    pub fn entries(&self) -> &[ThemeCatalogEntry] {
        &self.entries
    }

    /// Return bounded rejected-candidate diagnostics.
    #[must_use]
    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    /// Return list state mutably for rendering.
    pub const fn list_mut(&mut self) -> &mut ListState {
        &mut self.list
    }

    /// Return the current list offset for hit testing.
    #[must_use]
    pub const fn list_offset(&self) -> usize {
        self.list.offset
    }

    /// Return selected theme id.
    #[must_use]
    pub fn selected_id(&self) -> Option<&str> {
        self.list
            .selected
            .and_then(|index| self.entries.get(index))
            .map(|entry| entry.id.as_str())
    }

    /// Select one absolute row and preview it.
    pub fn select_row(&mut self, row: usize) -> ThemePickerOutcome {
        let Some(entry) = self.entries.get(row) else {
            return ThemePickerOutcome::Ignored;
        };
        self.list.select(Some(row));
        ThemePickerOutcome::Preview(entry.id.clone())
    }

    /// Activate one absolute row and request persistence.
    pub fn activate_row(&mut self, row: usize) -> ThemePickerOutcome {
        let Some(entry) = self.entries.get(row) else {
            return ThemePickerOutcome::Ignored;
        };
        self.list.select(Some(row));
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
        let current = self.list.selected.unwrap_or(0);
        let last = self.entries.len().saturating_sub(1);
        let next = if delta < 0 {
            current.saturating_sub(1)
        } else {
            current.saturating_add(1).min(last)
        };
        self.list.select(Some(next));
        ThemePickerOutcome::Preview(self.entries[next].id.clone())
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
