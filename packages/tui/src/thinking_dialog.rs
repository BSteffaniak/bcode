//! TUI reasoning output settings modal state.

use bcode_ipc::SessionModelStatus;
use bmux_keyboard::{KeyCode, KeyStroke};

/// Outcome from one reasoning-settings dialog keyboard update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingDialogOutcome {
    /// Focus or a setting changed and the dialog remains open.
    Handled,
    /// Apply the current settings and close the dialog.
    Apply,
    /// Close the dialog without applying the current settings.
    Cancel,
    /// The key is not owned by the reasoning-settings dialog.
    Ignored,
}

/// Initially focused reasoning output setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingDialogFocus {
    /// Focus the local display toggle.
    Display,
    /// Focus the local readable reasoning mode.
    Mode,
    /// Focus the requested reasoning effort.
    Effort,
    /// Focus the requested reasoning summary mode.
    Summary,
}

impl ThinkingDialogFocus {
    const fn row(self) -> usize {
        match self {
            Self::Display => 0,
            Self::Mode => 1,
            Self::Effort => 2,
            Self::Summary => 3,
        }
    }
}

/// Pending reasoning output settings dialog state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingDialogState {
    supported: bool,
    visible: bool,
    mode: bcode_config::TuiThinkingMode,
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
    pub fn new(
        visible: bool,
        mode: bcode_config::TuiThinkingMode,
        status: &SessionModelStatus,
    ) -> Self {
        let reasoning = status.reasoning.as_ref();
        let source = reasoning.map_or(
            bcode_model::ModelReasoningCapabilitySource::GenericFallback,
            |reasoning| reasoning.source,
        );
        Self {
            supported: reasoning.is_some(),
            visible,
            mode,
            effort: status.reasoning_effort.clone(),
            summary: status.reasoning_summary.clone(),
            effort_values: reasoning
                .map(|reasoning| {
                    bcode_model::ordered_reasoning_effort_values(&reasoning.effort_values)
                })
                .unwrap_or_default(),
            summary_values: reasoning
                .map(|reasoning| reasoning.summary_values.clone())
                .unwrap_or_default(),
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
        mode: bcode_config::TuiThinkingMode,
        status: &SessionModelStatus,
        focus: ThinkingDialogFocus,
    ) -> Self {
        let mut state = Self::new(visible, mode, status);
        state.focused_row = focus.row();
        state
    }

    /// Return whether the current model advertises reasoning support.
    #[must_use]
    pub const fn supported(&self) -> bool {
        self.supported
    }

    /// Return whether reasoning display is enabled.
    #[must_use]
    pub const fn visible(&self) -> bool {
        self.visible
    }

    /// Return the selected readable reasoning display mode.
    #[must_use]
    pub const fn mode(&self) -> bcode_config::TuiThinkingMode {
        self.mode
    }

    /// Return the readable reasoning display-mode label.
    #[must_use]
    pub const fn mode_label(&self) -> &'static str {
        match self.mode {
            bcode_config::TuiThinkingMode::All => "all",
            bcode_config::TuiThinkingMode::Summary => "summaries/milestones",
            bcode_config::TuiThinkingMode::Raw => "raw details",
        }
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

    /// Return effective visible reasoning summary label.
    #[must_use]
    pub fn effective_summary_label(&self) -> &str {
        self.summary
            .as_deref()
            .or(self.default_summary.as_deref())
            .unwrap_or("not requested")
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

    /// Handle one keyboard input through the dialog's interaction policy.
    pub fn handle_key(&mut self, stroke: KeyStroke) -> ThinkingDialogOutcome {
        match stroke.key {
            KeyCode::Up => {
                self.focus_previous();
                ThinkingDialogOutcome::Handled
            }
            KeyCode::Down => {
                self.focus_next();
                ThinkingDialogOutcome::Handled
            }
            KeyCode::Char(' ') => {
                self.cycle_focused();
                ThinkingDialogOutcome::Handled
            }
            KeyCode::Enter => ThinkingDialogOutcome::Apply,
            KeyCode::Escape => ThinkingDialogOutcome::Cancel,
            _ => ThinkingDialogOutcome::Ignored,
        }
    }

    /// Cycle/toggle the focused setting.
    pub fn cycle_focused(&mut self) {
        match self.focused_row {
            0 => self.visible = !self.visible,
            1 => {
                self.mode = match self.mode {
                    bcode_config::TuiThinkingMode::All => bcode_config::TuiThinkingMode::Summary,
                    bcode_config::TuiThinkingMode::Summary => bcode_config::TuiThinkingMode::Raw,
                    bcode_config::TuiThinkingMode::Raw => bcode_config::TuiThinkingMode::All,
                };
            }
            2 if self.supported => {
                self.effort = next_value(self.effort.as_deref(), &self.effort_values);
            }
            3 if self.supported => {
                self.summary = next_value(self.summary.as_deref(), &self.summary_values);
            }
            _ => {}
        }
    }

    const fn row_count() -> usize {
        4
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

#[cfg(test)]
mod tests {
    use super::{ThinkingDialogOutcome, ThinkingDialogState};
    use bmux_keyboard::{KeyCode, KeyStroke};

    fn status() -> bcode_ipc::SessionModelStatus {
        bcode_ipc::SessionModelStatus {
            provider_plugin_id: None,
            requested_model_id: None,
            effective_model_id: None,
            model_id: None,
            context_window: None,
            context_occupancy: None,
            request_context_error: None,
            auth_profile: None,
            context_format_version: None,
            compatibility_key: None,
            max_output_tokens: None,
            reasoning: None,
            reasoning_effort: None,
            reasoning_summary: None,
            prompt_cache_mode: None,
            conversation_reuse_mode: None,
            compaction_mode: None,
            compaction_backend: None,
            proactive_compaction_threshold_percent: None,
            cache: None,
            metadata_source: None,
            pricing: None,
        }
    }

    #[test]
    fn keyboard_policy_owns_focus_changes_apply_and_cancel() {
        let mut dialog =
            ThinkingDialogState::new(false, bcode_config::TuiThinkingMode::All, &status());

        assert_eq!(
            dialog.handle_key(KeyStroke::simple(KeyCode::Down)),
            ThinkingDialogOutcome::Handled
        );
        assert_eq!(dialog.focused_row(), 1);
        assert_eq!(
            dialog.handle_key(KeyStroke::simple(KeyCode::Enter)),
            ThinkingDialogOutcome::Apply
        );
        assert_eq!(
            dialog.handle_key(KeyStroke::simple(KeyCode::Escape)),
            ThinkingDialogOutcome::Cancel
        );
        assert_eq!(
            dialog.handle_key(KeyStroke::simple(KeyCode::Char('x'))),
            ThinkingDialogOutcome::Ignored
        );
    }
}
