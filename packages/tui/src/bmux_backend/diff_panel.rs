//! Diff panel state for the BMUX backend app.

use bmux_tui::diff::{DiffFileSummary, DiffLine};

/// State for inferred edit diff previews.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DiffPanel {
    changed_files: Vec<DiffFileSummary>,
    details: Vec<Vec<DiffLine>>,
    selected_file: Option<usize>,
    visibility: DiffPanelVisibility,
    scroll_offset: usize,
    combined_lines: Vec<DiffLine>,
}

impl DiffPanel {
    /// Create hidden empty diff panel state.
    #[must_use]
    pub(super) const fn new() -> Self {
        Self {
            changed_files: Vec::new(),
            details: Vec::new(),
            selected_file: None,
            visibility: DiffPanelVisibility::Hidden,
            scroll_offset: 0,
            combined_lines: Vec::new(),
        }
    }

    /// Return changed-file summaries.
    #[must_use]
    pub(super) fn changed_files(&self) -> &[DiffFileSummary] {
        &self.changed_files
    }

    /// Return whether the diff panel is visible.
    #[must_use]
    pub(super) fn visible(&self) -> bool {
        self.visibility == DiffPanelVisibility::Visible
    }

    /// Toggle diff panel visibility.
    pub(super) const fn toggle_visible(&mut self) -> bool {
        self.visibility = match self.visibility {
            DiffPanelVisibility::Hidden => DiffPanelVisibility::Visible,
            DiffPanelVisibility::Visible => DiffPanelVisibility::Hidden,
        };
        true
    }

    /// Return selected diff lines, or combined lines when no detail is selected.
    #[must_use]
    pub(super) fn lines(&self) -> &[DiffLine] {
        self.selected_file
            .and_then(|index| self.details.get(index).map(Vec::as_slice))
            .unwrap_or(&self.combined_lines)
    }

    /// Return diff scroll offset.
    #[must_use]
    pub(super) const fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    /// Scroll diff preview up.
    pub(super) fn scroll_up(&mut self, rows: usize) -> bool {
        if rows == 0 || self.combined_lines.is_empty() {
            return false;
        }
        let previous = self.scroll_offset;
        self.scroll_offset = self
            .scroll_offset
            .saturating_add(rows)
            .min(self.combined_lines.len());
        self.scroll_offset != previous
    }

    /// Scroll diff preview down.
    pub(super) const fn scroll_down(&mut self, rows: usize) -> bool {
        let previous = self.scroll_offset;
        self.scroll_offset = self.scroll_offset.saturating_sub(rows);
        self.scroll_offset != previous
    }

    /// Select a changed-file diff detail.
    pub(super) const fn select_file(&mut self, index: usize) -> bool {
        if index >= self.changed_files.len() {
            return false;
        }
        self.selected_file = Some(index);
        self.scroll_offset = 0;
        true
    }

    /// Select next changed file.
    pub(super) fn select_next_file(&mut self) -> bool {
        if self.changed_files.is_empty() {
            return false;
        }
        let next = self.selected_file.map_or(0, |index| {
            index.saturating_add(1).min(self.changed_files.len() - 1)
        });
        self.select_file(next)
    }

    /// Select previous changed file.
    pub(super) fn select_previous_file(&mut self) -> bool {
        if self.changed_files.is_empty() {
            return false;
        }
        let previous = self
            .selected_file
            .map_or(0, |index| index.saturating_sub(1));
        self.select_file(previous)
    }

    /// Record or replace a diff summary and its detail lines.
    pub(super) fn record(&mut self, summary: DiffFileSummary, lines: Vec<DiffLine>) {
        let path = summary.display_path();
        if let Some(existing_index) = self
            .changed_files
            .iter()
            .position(|existing| existing.display_path() == path)
        {
            self.changed_files[existing_index] = summary;
            if let Some(existing_lines) = self.details.get_mut(existing_index) {
                *existing_lines = lines;
            }
            self.selected_file = Some(existing_index);
        } else {
            self.changed_files.push(summary);
            self.details.push(lines);
            self.selected_file = Some(self.changed_files.len().saturating_sub(1));
        }
        self.scroll_offset = 0;
        self.combined_lines = self
            .details
            .iter()
            .flat_map(|detail| detail.iter().cloned())
            .collect();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffPanelVisibility {
    Hidden,
    Visible,
}
