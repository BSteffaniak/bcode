//! Onboarding/setup-map TUI view models and shell helpers.

use bcode_settings::{
    ReconciledSetupSection, SettingsDbHealth, SettingsDegradedPanel, SettingsError, SettingsStore,
    SetupConfigSummary, SetupMapSnapshot, SetupReadinessReport, SetupReconciliationInput,
    SetupSectionId, SetupSectionStatus, settings_degraded_panel,
};
use bmux_tui_components::stepper::{StepItem, StepStatus};

/// First-run onboarding shell state for the setup-map vertical slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnboardingShell {
    sections: Vec<ReconciledSetupSection>,
    focused_index: usize,
}

/// Automated onboarding walkthrough smoke-test report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnboardingWalkthroughReport {
    /// Initial focused setup section.
    pub initial_section: SetupSectionId,
    /// Focused section after moving next.
    pub next_section: SetupSectionId,
    /// Focused section after moving back.
    pub previous_section: SetupSectionId,
    /// Number of BMUX stepper items produced.
    pub step_count: usize,
    /// Compact text map rendered during the walkthrough.
    pub rendered_map: String,
    /// Whether persisted focus survived reload.
    pub persisted_focus_reloaded: bool,
}

/// High-level onboarding input actions handled by the setup-map shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingInputAction {
    /// Move focus to the next setup section.
    Next,
    /// Move focus to the previous setup section.
    Previous,
    /// Persist/currently select the focused section.
    Select,
    /// Quit/close onboarding.
    Quit,
}

/// Outcome of handling an onboarding input action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingActionOutcome {
    /// Focus moved to another section.
    FocusChanged(SetupSectionId),
    /// Focus was persisted/selected.
    Selected(SetupSectionId),
    /// Onboarding should close.
    Quit,
    /// No state changed.
    Ignored,
}

/// Text render snapshot for the onboarding setup shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnboardingRenderModel {
    /// Setup-map lines.
    pub map_lines: Vec<String>,
    /// Footer/help lines.
    pub footer_lines: Vec<String>,
    /// Optional degraded settings-state panel.
    pub degraded_panel: Option<SettingsDegradedPanel>,
    /// Optional readiness report.
    pub readiness_report: Option<SetupReadinessReport>,
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

    /// Handle a high-level onboarding input action.
    ///
    /// # Errors
    ///
    /// Returns an error when selecting the focused section cannot be persisted.
    pub fn handle_action(
        &mut self,
        action: OnboardingInputAction,
        store: &SettingsStore,
        at_ms: u64,
    ) -> Result<OnboardingActionOutcome, SettingsError> {
        match action {
            OnboardingInputAction::Next => {
                self.focus_next();
                Ok(OnboardingActionOutcome::FocusChanged(
                    self.focused_section(),
                ))
            }
            OnboardingInputAction::Previous => {
                self.focus_previous();
                Ok(OnboardingActionOutcome::FocusChanged(
                    self.focused_section(),
                ))
            }
            OnboardingInputAction::Select => {
                self.persist_focus(store, at_ms)?;
                Ok(OnboardingActionOutcome::Selected(self.focused_section()))
            }
            OnboardingInputAction::Quit => Ok(OnboardingActionOutcome::Quit),
        }
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

    /// Build a text render model for onboarding/control-center shell rendering.
    #[must_use]
    pub fn render_model(
        &self,
        health: &SettingsDbHealth,
        readiness_report: Option<SetupReadinessReport>,
    ) -> OnboardingRenderModel {
        let map_lines = self
            .sections
            .iter()
            .map(|section| {
                format!(
                    "{} [{}]{}",
                    setup_section_label(section.section_id),
                    section.status.as_str(),
                    if section.visited { " ✓" } else { "" }
                )
            })
            .collect();
        OnboardingRenderModel {
            map_lines,
            footer_lines: vec![
                "←/↑ previous  →/↓ next  Enter select  Esc close".to_owned(),
                "Setup state is persisted locally and user config remains TOML-backed.".to_owned(),
            ],
            degraded_panel: (!matches!(health, SettingsDbHealth::Available))
                .then(|| settings_degraded_panel(health)),
            readiness_report,
        }
    }

    /// Run a non-interactive first-run walkthrough smoke test.
    ///
    /// # Errors
    ///
    /// Returns an error when focus persistence or reload fails.
    pub fn smoke_walkthrough(
        store: &SettingsStore,
        config_summary: &SetupConfigSummary,
        visited_at_ms: u64,
    ) -> Result<OnboardingWalkthroughReport, SettingsError> {
        let mut shell = Self::load(store, config_summary)?;
        let initial_section = shell.focused_section();
        let rendered_map = shell.render_text_map();
        let step_count = shell.step_items().len();
        shell.focus_next();
        let next_section = shell.focused_section();
        shell.persist_focus(store, visited_at_ms)?;
        let reloaded = Self::load(store, config_summary)?;
        let persisted_focus_reloaded = reloaded.focused_section() == next_section;
        shell.focus_previous();
        let previous_section = shell.focused_section();
        Ok(OnboardingWalkthroughReport {
            initial_section,
            next_section,
            previous_section,
            step_count,
            rendered_map,
            persisted_focus_reloaded,
        })
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
    fn shell_handles_actions_and_builds_render_model() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let store = SettingsStore::from_settings_db_path(temp.path().join("settings.db"));
        let summary = SetupConfigSummary::default();
        let mut shell = OnboardingShell::load(&store, &summary).expect("shell should load");

        assert_eq!(
            shell
                .handle_action(OnboardingInputAction::Next, &store, 50)
                .expect("next should succeed"),
            OnboardingActionOutcome::FocusChanged(SetupSectionId::Detection)
        );
        assert_eq!(
            shell
                .handle_action(OnboardingInputAction::Select, &store, 51)
                .expect("select should persist"),
            OnboardingActionOutcome::Selected(SetupSectionId::Detection)
        );
        let render = shell.render_model(&SettingsDbHealth::Available, None);

        assert!(
            render
                .map_lines
                .iter()
                .any(|line| line.contains("Scout Tower"))
        );
        assert!(
            render
                .footer_lines
                .iter()
                .any(|line| line.contains("Enter"))
        );
        assert!(render.degraded_panel.is_none());
    }

    #[test]
    fn render_model_includes_degraded_panel() {
        let summary = SetupConfigSummary::default();
        let shell = OnboardingShell::from_reconciliation(&[], &summary.reconciliation_input());
        let render = shell.render_model(
            &SettingsDbHealth::Unavailable {
                message: "bad db".to_owned(),
            },
            None,
        );

        assert!(render.degraded_panel.is_some());
    }

    #[test]
    fn smoke_walkthrough_reports_navigation_and_persistence() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let store = SettingsStore::from_settings_db_path(temp.path().join("settings.db"));
        let summary = SetupConfigSummary::default();

        let report = OnboardingShell::smoke_walkthrough(&store, &summary, 43)
            .expect("smoke walkthrough should pass");

        assert_eq!(report.initial_section, SetupSectionId::Welcome);
        assert_eq!(report.next_section, SetupSectionId::Detection);
        assert_eq!(report.previous_section, SetupSectionId::Welcome);
        assert_eq!(report.step_count, SetupSectionId::all().len());
        assert!(report.rendered_map.contains("welcome"));
        assert!(report.persisted_focus_reloaded);
    }
}
