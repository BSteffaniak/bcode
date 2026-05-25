//! TUI thinking settings modal state.

use bcode_ipc::SessionModelStatus;

/// Initially focused thinking setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingDialogFocus {
    /// Focus the local display toggle.
    Display,
    /// Focus the requested reasoning effort.
    Effort,
    /// Focus the requested reasoning summary mode.
    Summary,
}

impl ThinkingDialogFocus {
    const fn row(self) -> usize {
        match self {
            Self::Display => 0,
            Self::Effort => 1,
            Self::Summary => 2,
        }
    }
}

const FALLBACK_EFFORT_VALUES: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh"];
const FALLBACK_SUMMARY_VALUES: &[&str] = &["auto", "concise", "detailed"];

/// Pending thinking settings dialog state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingDialogState {
    visible: bool,
    effort: Option<String>,
    summary: Option<String>,
    effort_values: Vec<String>,
    summary_values: Vec<String>,
    default_effort: Option<String>,
    default_summary: Option<String>,
    source: bcode_model::ModelReasoningCapabilitySource,
    focused_row: usize,
}

impl ThinkingDialogState {
    /// Create state from current UI display and model status.
    #[must_use]
    pub fn new(visible: bool, status: &SessionModelStatus) -> Self {
        let reasoning = status.reasoning.as_ref();
        let source = reasoning.map_or(
            bcode_model::ModelReasoningCapabilitySource::GenericFallback,
            |reasoning| reasoning.source,
        );
        Self {
            visible,
            effort: status.reasoning_effort.clone(),
            summary: status.reasoning_summary.clone(),
            effort_values: reasoning_values_with_fallback(
                reasoning.map(|reasoning| reasoning.effort_values.as_slice()),
                FALLBACK_EFFORT_VALUES,
            ),
            summary_values: reasoning_values_with_fallback(
                reasoning.map(|reasoning| reasoning.summary_values.as_slice()),
                FALLBACK_SUMMARY_VALUES,
            ),
            default_effort: reasoning.and_then(|reasoning| reasoning.default_effort.clone()),
            default_summary: reasoning.and_then(|reasoning| reasoning.default_summary.clone()),
            source,
            focused_row: ThinkingDialogFocus::Display.row(),
        }
    }

    /// Create state with a specific initial focus.
    #[must_use]
    pub fn new_focused(
        visible: bool,
        status: &SessionModelStatus,
        focus: ThinkingDialogFocus,
    ) -> Self {
        let mut state = Self::new(visible, status);
        state.focused_row = focus.row();
        state
    }

    /// Return whether reasoning display is enabled.
    #[must_use]
    pub const fn visible(&self) -> bool {
        self.visible
    }

    /// Return selected effort override.
    #[must_use]
    pub fn effort(&self) -> Option<&str> {
        self.effort.as_deref()
    }

    /// Return selected summary override.
    #[must_use]
    pub fn summary(&self) -> Option<&str> {
        self.summary.as_deref()
    }

    /// Return supported effort values.
    #[must_use]
    pub fn effort_values(&self) -> &[String] {
        &self.effort_values
    }

    /// Return the source of the effort values.
    #[must_use]
    pub const fn effort_values_source(&self) -> bcode_model::ModelReasoningCapabilitySource {
        self.source
    }

    /// Return supported summary values.
    #[must_use]
    pub fn summary_values(&self) -> &[String] {
        &self.summary_values
    }

    /// Return the source of the summary values.
    #[must_use]
    pub const fn summary_values_source(&self) -> bcode_model::ModelReasoningCapabilitySource {
        self.source
    }

    /// Return effective effort label.
    #[must_use]
    pub fn effective_effort_label(&self) -> &str {
        self.effort
            .as_deref()
            .or(self.default_effort.as_deref())
            .unwrap_or("provider default")
    }

    /// Return effective summary label.
    #[must_use]
    pub fn effective_summary_label(&self) -> &str {
        self.summary
            .as_deref()
            .or(self.default_summary.as_deref())
            .unwrap_or("provider default")
    }

    /// Return focused row index.
    #[must_use]
    pub const fn focused_row(&self) -> usize {
        self.focused_row
    }

    /// Focus next row.
    pub const fn focus_next(&mut self) {
        self.focused_row = self.focused_row.saturating_add(1) % Self::row_count();
    }

    /// Focus previous row.
    pub const fn focus_previous(&mut self) {
        if self.focused_row == 0 {
            self.focused_row = Self::row_count().saturating_sub(1);
        } else {
            self.focused_row = self.focused_row.saturating_sub(1);
        }
    }

    /// Cycle/toggle the focused setting.
    pub fn cycle_focused(&mut self) {
        match self.focused_row {
            0 => self.visible = !self.visible,
            1 => self.effort = next_value(self.effort.as_deref(), &self.effort_values),
            2 => self.summary = next_value(self.summary.as_deref(), &self.summary_values),
            _ => {}
        }
    }

    const fn row_count() -> usize {
        3
    }
}

fn next_value(current: Option<&str>, values: &[String]) -> Option<String> {
    if values.is_empty() {
        return current.map(ToOwned::to_owned);
    }
    let next_index = current
        .and_then(|current| values.iter().position(|value| value == current))
        .map_or(0, |index| index.saturating_add(1) % values.len());
    values.get(next_index).cloned()
}

fn reasoning_values_with_fallback(values: Option<&[String]>, fallback: &[&str]) -> Vec<String> {
    let values = values.unwrap_or_default();
    if values.is_empty() {
        fallback.iter().map(|value| (*value).to_owned()).collect()
    } else {
        values.to_vec()
    }
}
