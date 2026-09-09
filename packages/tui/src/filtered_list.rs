//! Shared filtered-list state for TUI pickers.

use bmux_tui::component::{LayoutId, LayoutNode, LogicalSize};
use bmux_tui_components::scroll_view::{ScrollView, ScrollViewComponent};
use bmux_tui_components::selectable_list::SelectableListState;

/// Selection and filtering state shared by picker UIs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilteredListState {
    list_state: SelectableListState,
    filtered_indices: Vec<usize>,
}

impl FilteredListState {
    /// Create list state with all item indices visible.
    #[must_use]
    pub fn new(item_count: usize) -> Self {
        let filtered_indices = (0..item_count).collect::<Vec<_>>();
        let mut list_state = SelectableListState::new(None);
        if !filtered_indices.is_empty() {
            list_state.set_selected(Some(0));
        }
        Self {
            list_state,
            filtered_indices,
        }
    }

    /// Synchronize selection visibility and return the BMUX render state.
    pub fn render_state(&mut self, viewport_height: u16) -> &mut SelectableListState {
        let viewport = ScrollViewComponent::viewport_layout(
            LayoutId::new("picker.viewport"),
            LogicalSize::new(1, usize::from(viewport_height)),
            LayoutNode::leaf(
                LayoutId::new("picker.rows"),
                LogicalSize::new(1, self.filtered_indices.len()),
            ),
        );
        if let Some(selected) = self.list_state.selected() {
            ScrollView::new().ensure_visible(&viewport, &mut self.list_state.scroll, selected, 1);
            self.list_state.set_focused(Some(selected));
        }
        &mut self.list_state
    }

    /// Return the current scroll offset.
    #[must_use]
    pub const fn offset(&self) -> usize {
        self.list_state.vertical_scroll()
    }

    /// Return filtered source indices.
    #[must_use]
    pub fn indices(&self) -> &[usize] {
        &self.filtered_indices
    }

    /// Return the selected source index.
    #[must_use]
    pub fn selected_source_index(&self) -> Option<usize> {
        let selected = self.list_state.selected()?;
        self.filtered_indices.get(selected).copied()
    }

    /// Replace filtered indices and keep selection valid.
    pub fn replace_indices(&mut self, filtered_indices: Vec<usize>) {
        self.filtered_indices = filtered_indices;
        if self.filtered_indices.is_empty() {
            self.list_state.set_selected(None);
            self.list_state.set_vertical_scroll(0);
        } else {
            self.list_state.set_selected(Some(
                self.list_state
                    .selected()
                    .unwrap_or(0)
                    .min(self.filtered_indices.len() - 1),
            ));
        }
    }

    /// Move selection down.
    pub fn select_next(&mut self) {
        if !self.filtered_indices.is_empty() {
            let next = self
                .list_state
                .selected()
                .map_or(0, |i| (i + 1).min(self.filtered_indices.len() - 1));
            self.list_state.set_selected(Some(next));
        }
    }

    /// Move selection up.
    pub fn select_previous(&mut self) {
        if !self.filtered_indices.is_empty() {
            self.list_state.set_selected(Some(
                self.list_state.selected().unwrap_or(0).saturating_sub(1),
            ));
        }
    }

    /// Select a visible row by zero-based index.
    pub const fn select_visible(&mut self, row: usize) -> bool {
        if row >= self.filtered_indices.len() {
            return false;
        }
        self.list_state.set_selected(Some(row));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::FilteredListState;

    #[test]
    fn render_state_owns_selection_visibility_synchronization() {
        let mut state = FilteredListState::new(12);
        assert!(state.select_visible(11));
        assert_eq!(state.offset(), 0);

        let rendered = state.render_state(3);
        assert_eq!(rendered.selected(), Some(11));
        assert_eq!(rendered.vertical_scroll(), 9);
        assert_eq!(state.offset(), 9);
    }

    #[test]
    fn filtering_defers_scroll_normalization_until_viewport_is_known() {
        let mut state = FilteredListState::new(12);
        assert!(state.select_visible(11));
        let _rendered = state.render_state(3);
        assert_eq!(state.offset(), 9);

        state.replace_indices(vec![4]);
        assert_eq!(state.selected_source_index(), Some(4));
        assert_eq!(state.offset(), 9);

        let rendered = state.render_state(3);
        assert_eq!(rendered.selected(), Some(0));
        assert_eq!(rendered.vertical_scroll(), 0);
    }
}
