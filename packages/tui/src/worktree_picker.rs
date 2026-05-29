//! TUI worktree picker state.

use bcode_worktree_models::WorktreeInfo;
use bmux_tui::list::{ListItem, ListState};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::{Color, Modifier};
use bmux_tui_components::text_input::TextInputState;

use super::filtered_list::FilteredListState;

/// Worktree picker state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreePickerApp {
    worktrees: Vec<WorktreeInfo>,
    filter: TextInputState,
    list: FilteredListState,
    status: String,
}

impl WorktreePickerApp {
    /// Create a worktree picker.
    #[must_use]
    pub fn new(worktrees: Vec<WorktreeInfo>) -> Self {
        let list = FilteredListState::new(worktrees.len());
        Self {
            worktrees,
            filter: super::text_input_flow::empty_state(),
            list,
            status: "Select a worktree or Esc to cancel".to_owned(),
        }
    }

    /// Return the filter input mutably.
    pub const fn filter_mut(&mut self) -> &mut TextInputState {
        &mut self.filter
    }

    /// Return list state.
    pub const fn list_state_mut(&mut self) -> &mut ListState {
        self.list.list_state_mut()
    }

    /// Return picker status.
    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Set picker status.
    pub fn set_status(&mut self, status: String) {
        self.status = status;
    }

    /// Return visible list items.
    #[must_use]
    pub fn list_items(&self) -> Vec<ListItem> {
        if self.list.indices().is_empty() {
            return vec![empty_item("No matching worktrees")];
        }
        self.list
            .indices()
            .iter()
            .map(|index| worktree_item(&self.worktrees[*index]))
            .collect()
    }

    /// Return selected worktree.
    #[must_use]
    pub fn selected_worktree(&self) -> Option<&WorktreeInfo> {
        let index = self.list.selected_source_index()?;
        self.worktrees.get(index)
    }

    /// Select a visible row by zero-based index.
    pub const fn select_visible(&mut self, row: usize) -> bool {
        self.list.select_visible(row)
    }

    /// Select previous visible worktree.
    pub fn select_previous(&mut self) {
        self.list.select_previous();
    }

    /// Select next visible worktree.
    pub fn select_next(&mut self) {
        self.list.select_next();
    }

    /// Refresh filter state.
    pub fn refresh_filter(&mut self) {
        let query = self.filter.buffer().text().trim().to_ascii_lowercase();
        let indices = self
            .worktrees
            .iter()
            .enumerate()
            .filter_map(|(index, worktree)| worktree_matches(worktree, &query).then_some(index))
            .collect::<Vec<_>>();
        self.list.replace_indices(indices);
    }
}

fn worktree_matches(worktree: &WorktreeInfo, query: &str) -> bool {
    if query.trim().is_empty() {
        return true;
    }
    let haystack = format!(
        "{} {} {}",
        worktree.path.display(),
        worktree.branch.as_deref().unwrap_or(""),
        worktree.commit.as_deref().unwrap_or("")
    )
    .to_ascii_lowercase();
    haystack.contains(query)
}

fn worktree_item(worktree: &WorktreeInfo) -> ListItem {
    let marker = if worktree.is_main { "main" } else { "linked" };
    let branch = worktree.branch.as_deref().unwrap_or("<detached>");
    let commit = worktree.commit.as_deref().unwrap_or("-");
    ListItem::new(Line::from_spans(vec![
        Span::styled(branch.to_owned(), Style::new().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(marker.to_owned(), Style::new().fg(Color::BrightBlack)),
        Span::raw("  "),
        Span::styled(commit.to_owned(), Style::new().fg(Color::BrightBlack)),
        Span::raw("  "),
        Span::raw(worktree.path.display().to_string()),
    ]))
}

fn empty_item(message: &str) -> ListItem {
    ListItem::new(Line::from_spans(vec![Span::styled(
        message.to_owned(),
        Style::new().fg(Color::BrightBlack),
    )]))
}
