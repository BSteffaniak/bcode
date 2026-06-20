//! Onboarding/setup-map TUI view models and shell helpers.

use bcode_settings::{
    ReconciledSetupSection, SettingsError, SettingsStore, SetupConfigSummary, SetupMapSnapshot,
    SetupReconciliationInput, SetupSectionId, SetupSectionStatus,
};
use bmux_tui_components::stepper::{StepItem, StepStatus};

/// First-run onboarding shell state for the setup-map vertical slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnboardingShell {
    sections: Vec<ReconciledSetupSection>,
    focused_index: usize,
}

impl OnboardingShell {
    /// Build the onboarding shell from persisted settings DB state and real config summary.
    ///
    /// # Errors
    ///
    /// Returns an error when onboarding section state cannot be loaded.
    pub fn load(
        store: &SettingsStore,
        config_summary: &SetupConfigSummary,
    ) -> Result<Self, SettingsError> {
        let persisted_sections = store.onboarding_sections()?;
        let progress = store.onboarding_progress()?;
        let mut input = config_summary.reconciliation_input();
        input.current_section = progress
            .and_then(|progress| progress.last_section)
            .as_deref()
            .and_then(setup_section_id_from_str);
        Ok(Self::from_reconciliation(&persisted_sections, &input))
    }

    /// Build the onboarding shell from persisted section metadata and reconciliation facts.
    #[must_use]
    pub fn from_reconciliation(
        persisted_sections: &[bcode_settings::OnboardingSection],
        input: &SetupReconciliationInput,
    ) -> Self {
        let snapshot = SetupMapSnapshot::from_reconciliation(persisted_sections, input);
        let focused_index = snapshot
            .sections
            .iter()
            .position(|section| section.status == SetupSectionStatus::Current)
            .unwrap_or_default();
        Self {
            sections: snapshot.sections,
            focused_index,
        }
    }

    /// Return the currently focused setup section.
    #[must_use]
    pub fn focused_section(&self) -> SetupSectionId {
        self.sections
            .get(self.focused_index)
            .map_or(SetupSectionId::Welcome, |section| section.section_id)
    }

    /// Move focus to the next setup section.
    pub fn focus_next(&mut self) {
        if self.sections.is_empty() {
            return;
        }
        self.focused_index = (self.focused_index + 1).min(self.sections.len().saturating_sub(1));
        self.mark_current_focus();
    }

    /// Move focus to the previous setup section.
    pub fn focus_previous(&mut self) {
        self.focused_index = self.focused_index.saturating_sub(1);
        self.mark_current_focus();
    }

    /// Persist the current focus as a visited onboarding section.
    ///
    /// # Errors
    ///
    /// Returns an error when the settings store cannot persist resume state.
    pub fn persist_focus(
        &self,
        store: &SettingsStore,
        visited_at_ms: u64,
    ) -> Result<(), SettingsError> {
        store.visit_onboarding_section(self.focused_section(), visited_at_ms)
    }

    /// Return BMUX stepper items for rendering the setup map with existing primitives.
    #[must_use]
    pub fn step_items(&self) -> Vec<StepItem<'static>> {
        self.sections
            .iter()
            .map(|section| {
                StepItem::new(
                    section.section_id.as_str(),
                    setup_section_label(section.section_id),
                )
                .status(step_status(section.status))
            })
            .collect()
    }

    /// Render a compact text map for smoke tests and fallback displays.
    #[must_use]
    pub fn render_text_map(&self) -> String {
        SetupMapSnapshot {
            sections: self.sections.clone(),
        }
        .render_text_map()
    }

    fn mark_current_focus(&mut self) {
        let focused = self.focused_section();
        for section in &mut self.sections {
            if section.section_id == focused {
                section.status = SetupSectionStatus::Current;
                section.visited = true;
            } else if section.status == SetupSectionStatus::Current {
                section.status = if section.visited {
                    SetupSectionStatus::Visited
                } else {
                    SetupSectionStatus::Unvisited
                };
            }
        }
    }
}

/// Return the human label for a setup section.
#[must_use]
pub const fn setup_section_label(section_id: SetupSectionId) -> &'static str {
    match section_id {
        SetupSectionId::Welcome => "Base Camp",
        SetupSectionId::Detection => "Scout Tower",
        SetupSectionId::SecureVault => "Secure Vault",
        SetupSectionId::Providers => "Signal Station",
        SetupSectionId::Models => "Engine Room",
        SetupSectionId::Permissions => "Control Room",
        SetupSectionId::Imports => "Archive Gate",
        SetupSectionId::Plugins => "Workshop",
        SetupSectionId::Launch => "Launch",
    }
}

const fn step_status(status: SetupSectionStatus) -> StepStatus {
    match status {
        SetupSectionStatus::Current => StepStatus::Current,
        SetupSectionStatus::Complete | SetupSectionStatus::Secured => StepStatus::Complete,
        SetupSectionStatus::Recommended | SetupSectionStatus::NeedsAttention => StepStatus::Warning,
        SetupSectionStatus::Blocked => StepStatus::Error,
        SetupSectionStatus::Optional | SetupSectionStatus::Skipped => StepStatus::Disabled,
        SetupSectionStatus::Visited | SetupSectionStatus::Unvisited => StepStatus::Pending,
    }
}

fn setup_section_id_from_str(value: &str) -> Option<SetupSectionId> {
    SetupSectionId::all()
        .into_iter()
        .find(|section| section.as_str() == value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_settings::OnboardingSection;

    #[test]
    fn shell_navigates_and_renders_step_items() {
        let persisted = vec![OnboardingSection {
            section_id: SetupSectionId::Welcome.as_str().to_owned(),
            status: SetupSectionStatus::Visited.as_str().to_owned(),
            visited: true,
            visited_at_ms: Some(1),
            completed_at_ms: None,
            skipped_at_ms: None,
            dismissed: false,
        }];
        let input = SetupReconciliationInput {
            current_section: Some(SetupSectionId::Welcome),
            ..SetupReconciliationInput::default()
        };
        let mut shell = OnboardingShell::from_reconciliation(&persisted, &input);

        assert_eq!(shell.focused_section(), SetupSectionId::Welcome);
        assert!(
            shell
                .render_text_map()
                .starts_with("welcome:current:visited")
        );
        shell.focus_next();
        assert_eq!(shell.focused_section(), SetupSectionId::Detection);
        shell.focus_previous();
        assert_eq!(shell.focused_section(), SetupSectionId::Welcome);
        assert_eq!(shell.step_items().len(), SetupSectionId::all().len());
    }

    #[test]
    fn shell_loads_and_persists_focus() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let store = SettingsStore::from_settings_db_path(temp.path().join("settings.db"));
        let summary = SetupConfigSummary::default();

        let mut shell = OnboardingShell::load(&store, &summary).expect("shell should load");
        shell.focus_next();
        shell
            .persist_focus(&store, 42)
            .expect("focus should persist");
        let reloaded = OnboardingShell::load(&store, &summary).expect("shell should reload");

        assert_eq!(reloaded.focused_section(), SetupSectionId::Detection);
    }
}
