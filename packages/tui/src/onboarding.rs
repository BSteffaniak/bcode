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
    status_message: Option<String>,
    pending_confirmation: Option<OnboardingPendingConfirmation>,
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
    /// Toggle/select a draft provider for the focused provider section.
    ToggleProvider,
    /// Toggle/select a draft auth profile/subscription.
    ToggleAuthProfile,
    /// Select a draft model profile.
    SelectModelProfile,
    /// Cycle the draft permission preset.
    CyclePermissionPreset,
    /// Mark session import reviewed.
    ReviewSessionImport,
    /// Mark plugins reviewed.
    ReviewPlugins,
    /// Apply generated setup plan and reconcile actual state.
    ApplyPlan,
    /// Confirm a pending modal/confirmation action.
    Confirm,
    /// Cancel a pending modal/confirmation action.
    CancelConfirmation,
    /// Mark the focused setup section complete.
    Complete,
    /// Mark the focused optional setup section skipped.
    Skip,
    /// Mark first-run onboarding complete from the launch section.
    Launch,
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
    /// Focused section was marked complete.
    Completed(SetupSectionId),
    /// Focused section was skipped.
    Skipped(SetupSectionId),
    /// First-run onboarding completed and launch is allowed.
    LaunchReady,
    /// Onboarding should close.
    Quit,
    /// No state changed.
    Ignored,
}

/// Pending modal/confirmation action in the onboarding shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnboardingPendingConfirmation {
    /// Confirmation title.
    pub title: String,
    /// Confirmation body.
    pub body: String,
    /// Action to run when confirmed.
    pub action: OnboardingInputAction,
}

/// Story/detail content for a setup-map section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnboardingSectionDetail {
    /// Section id.
    pub section_id: SetupSectionId,
    /// Display title.
    pub title: String,
    /// Short story explaining why this section matters.
    pub story: String,
    /// User-facing status label.
    pub status: String,
    /// Suggested actions for this section.
    pub actions: Vec<String>,
}

/// Text render snapshot for the onboarding setup shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnboardingRenderModel {
    /// Setup-map lines.
    pub map_lines: Vec<String>,
    /// Focused section detail/story panel.
    pub focused_detail: OnboardingSectionDetail,
    /// Footer/help lines.
    pub footer_lines: Vec<String>,
    /// Optional degraded settings-state panel.
    pub degraded_panel: Option<SettingsDegradedPanel>,
    /// Optional readiness report.
    pub readiness_report: Option<SetupReadinessReport>,
    /// Optional pending confirmation/modal state.
    pub pending_confirmation: Option<OnboardingPendingConfirmation>,
}

impl OnboardingRenderModel {
    /// Return this render model as a single string for snapshot/smoke validation.
    #[must_use]
    pub fn snapshot_text(&self) -> String {
        let mut lines = self.map_lines.clone();
        lines.extend(self.footer_lines.clone());
        if let Some(panel) = &self.degraded_panel {
            lines.push(panel.message.clone());
        }
        if let Some(report) = &self.readiness_report {
            lines.extend(
                report
                    .items
                    .iter()
                    .map(|item| format!("{}: {}", item.section_id.as_str(), item.title)),
            );
        }
        if let Some(confirmation) = &self.pending_confirmation {
            lines.push(format!("confirm: {}", confirmation.title));
            lines.push(confirmation.body.clone());
        }
        lines.join("\n")
    }

    /// Whether this render model has enough structure for the polished setup-map layout.
    #[must_use]
    pub const fn has_visual_composition(&self) -> bool {
        !self.map_lines.is_empty()
            && !self.focused_detail.title.is_empty()
            && !self.focused_detail.story.is_empty()
            && !self.focused_detail.actions.is_empty()
            && self.footer_lines.len() >= 2
    }

    /// Audit this render model for obvious secret material.
    #[must_use]
    pub fn secret_audit(&self) -> bcode_settings::SecretAuditReport {
        bcode_settings::audit_no_secret_material("onboarding-render", &self.snapshot_text())
    }
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
            status_message: None,
            pending_confirmation: None,
        }
    }

    /// Return reconciled setup sections.
    #[must_use]
    pub fn sections(&self) -> &[ReconciledSetupSection] {
        &self.sections
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

    /// Return the latest user-facing shell status message.
    #[must_use]
    pub fn status_message(&self) -> Option<&str> {
        self.status_message.as_deref()
    }

    /// Mark the focused section complete locally and persist it.
    ///
    /// # Errors
    ///
    /// Returns an error when section completion cannot be persisted.
    pub fn complete_focused_section(
        &mut self,
        store: &SettingsStore,
        completed_at_ms: u64,
    ) -> Result<(), SettingsError> {
        let section_id = self.focused_section();
        store.complete_onboarding_section(section_id, completed_at_ms)?;
        if let Some(section) = self.sections.get_mut(self.focused_index) {
            section.status = SetupSectionStatus::Complete;
            section.visited = true;
        }
        self.status_message = Some(format!(
            "{} marked complete",
            setup_section_label(section_id)
        ));
        Ok(())
    }

    /// Mark the focused section skipped locally and persist it.
    ///
    /// # Errors
    ///
    /// Returns an error when section skip state cannot be persisted.
    pub fn skip_focused_section(
        &mut self,
        store: &SettingsStore,
        skipped_at_ms: u64,
    ) -> Result<(), SettingsError> {
        let section_id = self.focused_section();
        store.skip_onboarding_section(section_id, skipped_at_ms)?;
        if let Some(section) = self.sections.get_mut(self.focused_index) {
            section.status = SetupSectionStatus::Skipped;
            section.visited = true;
        }
        self.status_message = Some(format!("{} skipped", setup_section_label(section_id)));
        Ok(())
    }

    /// Mark first-run onboarding complete when launch is selected.
    ///
    /// # Errors
    ///
    /// Returns an error when onboarding completion cannot be persisted.
    pub fn launch_from_onboarding(
        &mut self,
        store: &SettingsStore,
        completed_at_ms: u64,
    ) -> Result<(), SettingsError> {
        store.complete_onboarding(completed_at_ms)?;
        self.status_message = Some("Onboarding complete — ready to launch Bcode".to_owned());
        Ok(())
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
            OnboardingInputAction::Next | OnboardingInputAction::Previous => {
                Ok(self.handle_navigation_action(action))
            }
            OnboardingInputAction::Select => self.handle_select_action(store, at_ms),
            OnboardingInputAction::ToggleProvider
            | OnboardingInputAction::ToggleAuthProfile
            | OnboardingInputAction::SelectModelProfile
            | OnboardingInputAction::CyclePermissionPreset
            | OnboardingInputAction::ReviewSessionImport
            | OnboardingInputAction::ReviewPlugins => {
                self.handle_draft_action(action, store, at_ms)
            }
            OnboardingInputAction::ApplyPlan => Ok(self.request_apply_confirmation()),
            OnboardingInputAction::Confirm => self.confirm_pending_action(store, at_ms),
            OnboardingInputAction::CancelConfirmation => {
                self.pending_confirmation = None;
                self.status_message = Some("confirmation cancelled".to_owned());
                Ok(OnboardingActionOutcome::Ignored)
            }
            OnboardingInputAction::Complete
            | OnboardingInputAction::Skip
            | OnboardingInputAction::Launch => self.handle_completion_action(action, store, at_ms),
            OnboardingInputAction::Quit => Ok(OnboardingActionOutcome::Quit),
        }
    }

    fn handle_navigation_action(
        &mut self,
        action: OnboardingInputAction,
    ) -> OnboardingActionOutcome {
        match action {
            OnboardingInputAction::Next => self.focus_next(),
            OnboardingInputAction::Previous => self.focus_previous(),
            _ => {}
        }
        OnboardingActionOutcome::FocusChanged(self.focused_section())
    }

    fn handle_select_action(
        &mut self,
        store: &SettingsStore,
        at_ms: u64,
    ) -> Result<OnboardingActionOutcome, SettingsError> {
        self.persist_focus(store, at_ms)?;
        self.status_message = Some(format!(
            "{} selected",
            setup_section_label(self.focused_section())
        ));
        Ok(OnboardingActionOutcome::Selected(self.focused_section()))
    }

    fn handle_draft_action(
        &mut self,
        action: OnboardingInputAction,
        store: &SettingsStore,
        at_ms: u64,
    ) -> Result<OnboardingActionOutcome, SettingsError> {
        match action {
            OnboardingInputAction::ToggleProvider => self.toggle_provider(store, at_ms),
            OnboardingInputAction::ToggleAuthProfile => self.toggle_auth_profile(store, at_ms),
            OnboardingInputAction::SelectModelProfile => self.select_model_profile(store, at_ms),
            OnboardingInputAction::CyclePermissionPreset => {
                self.cycle_permission_preset(store, at_ms)
            }
            OnboardingInputAction::ReviewSessionImport => self.review_session_import(store, at_ms),
            OnboardingInputAction::ReviewPlugins => self.review_plugins(store, at_ms),
            _ => Ok(OnboardingActionOutcome::Ignored),
        }
    }

    fn toggle_provider(
        &mut self,
        store: &SettingsStore,
        at_ms: u64,
    ) -> Result<OnboardingActionOutcome, SettingsError> {
        let draft = store.toggle_draft_provider("openai-compatible", at_ms)?;
        self.status_message = Some(format!(
            "selected providers: {}",
            draft
                .providers
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
        Ok(OnboardingActionOutcome::Selected(SetupSectionId::Providers))
    }

    fn toggle_auth_profile(
        &mut self,
        store: &SettingsStore,
        at_ms: u64,
    ) -> Result<OnboardingActionOutcome, SettingsError> {
        let draft = store.toggle_draft_auth_profile("default", at_ms)?;
        self.status_message = Some(format!(
            "selected auth profiles: {}",
            draft
                .auth_profiles
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
        Ok(OnboardingActionOutcome::Selected(
            SetupSectionId::SecureVault,
        ))
    }

    fn select_model_profile(
        &mut self,
        store: &SettingsStore,
        at_ms: u64,
    ) -> Result<OnboardingActionOutcome, SettingsError> {
        let draft = store.select_draft_model_profile("default", at_ms)?;
        self.status_message = Some(format!(
            "selected model profile: {}",
            draft.model_profile.unwrap_or_else(|| "default".to_owned())
        ));
        Ok(OnboardingActionOutcome::Selected(SetupSectionId::Models))
    }

    fn cycle_permission_preset(
        &mut self,
        store: &SettingsStore,
        at_ms: u64,
    ) -> Result<OnboardingActionOutcome, SettingsError> {
        let draft = store.cycle_draft_permission_preset(at_ms)?;
        self.status_message = Some(format!(
            "permission preset: {}",
            draft
                .permission_preset
                .unwrap_or_else(|| "balanced".to_owned())
        ));
        Ok(OnboardingActionOutcome::Selected(
            SetupSectionId::Permissions,
        ))
    }

    fn review_session_import(
        &mut self,
        store: &SettingsStore,
        at_ms: u64,
    ) -> Result<OnboardingActionOutcome, SettingsError> {
        let mut draft = store.onboarding_draft_setup()?;
        draft.session_import_reviewed = true;
        store.save_onboarding_draft_setup(&draft, at_ms)?;
        self.status_message = Some("session import reviewed".to_owned());
        Ok(OnboardingActionOutcome::Selected(SetupSectionId::Imports))
    }

    fn review_plugins(
        &mut self,
        store: &SettingsStore,
        at_ms: u64,
    ) -> Result<OnboardingActionOutcome, SettingsError> {
        let mut draft = store.onboarding_draft_setup()?;
        draft.plugins_reviewed = true;
        store.save_onboarding_draft_setup(&draft, at_ms)?;
        self.status_message = Some("plugin setup reviewed".to_owned());
        Ok(OnboardingActionOutcome::Selected(SetupSectionId::Plugins))
    }

    fn request_apply_confirmation(&mut self) -> OnboardingActionOutcome {
        self.pending_confirmation = Some(OnboardingPendingConfirmation {
            title: "Apply setup plan?".to_owned(),
            body: "Bcode will persist onboarding metadata, run secure imports only when explicitly requested, and reconcile actual config/auth state after apply. Press y to confirm or n/Esc to cancel.".to_owned(),
            action: OnboardingInputAction::ApplyPlan,
        });
        self.status_message = Some("confirm setup apply".to_owned());
        OnboardingActionOutcome::Ignored
    }

    fn confirm_pending_action(
        &mut self,
        store: &SettingsStore,
        at_ms: u64,
    ) -> Result<OnboardingActionOutcome, SettingsError> {
        let Some(confirmation) = self.pending_confirmation.take() else {
            self.status_message = Some("nothing to confirm".to_owned());
            return Ok(OnboardingActionOutcome::Ignored);
        };
        match confirmation.action {
            OnboardingInputAction::ApplyPlan => self.handle_apply_plan_action(store, at_ms),
            _ => Ok(OnboardingActionOutcome::Ignored),
        }
    }

    fn handle_apply_plan_action(
        &mut self,
        store: &SettingsStore,
        at_ms: u64,
    ) -> Result<OnboardingActionOutcome, SettingsError> {
        let draft = store.onboarding_draft_setup()?;
        let detection_entries = store.detection_cache_entries()?;
        let secure_import_plans =
            bcode_settings::secure_import_plans_from_detection(&detection_entries);
        apply_draft_to_user_config(&draft)?;
        let config = bcode_config::load_config()?;
        let plan = bcode_settings::generate_setup_plan_from_draft(
            &draft,
            &secure_import_plans,
            &bcode_settings::SetupConfigSummary::from_config(&config),
        );
        let applied = store.apply_setup_plan(&plan, at_ms)?;
        let reconciliation = store.reconcile_setup_apply(&config)?;
        store.put_control_state(
            "setup.last_apply_reconciliation",
            &serde_json::to_value(&reconciliation)?,
            at_ms,
        )?;
        self.status_message = Some(format!(
            "applied {} setup actions; {} actions need external/domain follow-up",
            applied.applied_actions.len(),
            applied.skipped_actions.len()
        ));
        Ok(OnboardingActionOutcome::Selected(SetupSectionId::Launch))
    }

    fn handle_completion_action(
        &mut self,
        action: OnboardingInputAction,
        store: &SettingsStore,
        at_ms: u64,
    ) -> Result<OnboardingActionOutcome, SettingsError> {
        match action {
            OnboardingInputAction::Complete => {
                let section_id = self.focused_section();
                self.complete_focused_section(store, at_ms)?;
                Ok(OnboardingActionOutcome::Completed(section_id))
            }
            OnboardingInputAction::Skip => {
                let section_id = self.focused_section();
                self.skip_focused_section(store, at_ms)?;
                Ok(OnboardingActionOutcome::Skipped(section_id))
            }
            OnboardingInputAction::Launch => {
                self.launch_from_onboarding(store, at_ms)?;
                Ok(OnboardingActionOutcome::LaunchReady)
            }
            _ => Ok(OnboardingActionOutcome::Ignored),
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
            focused_detail: self.focused_detail(),
            footer_lines: vec![
                "←/↑ previous  →/↓ next  Enter select  p provider  a auth  m model  r permissions  i import  g plugins  x apply  y confirm  n cancel  c complete  s skip  l launch  Esc close"
                    .to_owned(),
                self.status_message.clone().unwrap_or_else(|| {
                    "Setup state is persisted locally and user config remains TOML-backed."
                        .to_owned()
                }),
            ],
            degraded_panel: (!matches!(health, SettingsDbHealth::Available))
                .then(|| settings_degraded_panel(health)),
            readiness_report,
            pending_confirmation: self.pending_confirmation.clone(),
        }
    }

    /// Build detail/story content for the focused setup section.
    #[must_use]
    pub fn focused_detail(&self) -> OnboardingSectionDetail {
        let section_id = self.focused_section();
        let status = self
            .sections
            .get(self.focused_index)
            .map_or(SetupSectionStatus::Unvisited, |section| section.status);
        OnboardingSectionDetail {
            section_id,
            title: setup_section_label(section_id).to_owned(),
            story: setup_section_story(section_id).to_owned(),
            status: status.as_str().to_owned(),
            actions: setup_section_actions(section_id)
                .iter()
                .map(ToString::to_string)
                .collect(),
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

/// Return the story copy for a setup section.
#[must_use]
pub const fn setup_section_story(section_id: SetupSectionId) -> &'static str {
    match section_id {
        SetupSectionId::Welcome => {
            "Start at Base Camp: Bcode learns enough about your setup to get you coding without busywork."
        }
        SetupSectionId::Detection => {
            "Scout Tower checks existing config, providers, models, plugins, sessions, and environment hints before asking questions."
        }
        SetupSectionId::SecureVault => {
            "Secure Vault keeps provider secrets out of plaintext config and guides them into sshenv-backed encrypted storage."
        }
        SetupSectionId::Providers => {
            "Signal Station connects the AI providers and subscriptions you want Bcode to use."
        }
        SetupSectionId::Models => {
            "Engine Room chooses the model profile that balances speed, cost, and capability for your workflow."
        }
        SetupSectionId::Permissions => {
            "Control Room sets how cautious or autonomous Bcode should be when using tools."
        }
        SetupSectionId::Imports => {
            "Archive Gate can safely review importable session history without repairing or replaying logs on the normal path."
        }
        SetupSectionId::Plugins => {
            "Workshop reviews bundled plugins so powerful behavior stays visible and disableable."
        }
        SetupSectionId::Launch => {
            "Launch Pad reviews the plan, applies selected setup safely, and starts Bcode when everything is ready."
        }
    }
}

/// Return suggested actions for a setup section.
#[must_use]
pub const fn setup_section_actions(section_id: SetupSectionId) -> &'static [&'static str] {
    match section_id {
        SetupSectionId::Welcome => &["Review detected setup", "Choose quick or full setup"],
        SetupSectionId::Detection => &["Run bounded detection", "Inspect safe metadata"],
        SetupSectionId::SecureVault => {
            &["Import credentials securely", "Explain sshenv/device seal"]
        }
        SetupSectionId::Providers => &["Add provider", "Choose default provider"],
        SetupSectionId::Models => &["Pick default model", "Review model profile"],
        SetupSectionId::Permissions => &["Choose permission preset", "Review tool boundaries"],
        SetupSectionId::Imports => &["Review import sources", "Skip import for now"],
        SetupSectionId::Plugins => &["Review bundled plugins", "Disable unwanted plugins"],
        SetupSectionId::Launch => &["Review setup plan", "Launch Bcode"],
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

fn apply_draft_to_user_config(
    draft: &bcode_settings::OnboardingDraftSetup,
) -> Result<(), SettingsError> {
    for provider in &draft.providers {
        match provider.as_str() {
            "openai-compatible" | "openrouter" | "xai" => {
                let provider_name = match provider.as_str() {
                    "openrouter" => "openrouter",
                    "xai" => "xai",
                    _ => "openai",
                };
                bcode_config::set_openai_compatible_sshenv_auth_mode(
                    provider_name,
                    draft
                        .auth_profiles
                        .iter()
                        .next()
                        .cloned()
                        .unwrap_or_else(|| "default".to_owned()),
                    bcode_config::default_auth_vault_path(),
                    draft.model_profile.clone(),
                    bcode_config::AuthMode::ApiKey,
                    None,
                )?;
            }
            "bedrock" => {
                bcode_config::set_bedrock_model_profile(
                    draft.model_profile.as_deref().unwrap_or("default"),
                    "anthropic.claude-3-5-sonnet-20241022-v2:0".to_owned(),
                    None,
                    None,
                    None,
                    &[],
                )?;
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_settings::OnboardingSection;

    struct ConfigEnvGuard;

    impl Drop for ConfigEnvGuard {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var("BCODE_CONFIG");
            }
        }
    }

    fn isolated_config_store() -> (
        tempfile::TempDir,
        SettingsStore,
        std::path::PathBuf,
        ConfigEnvGuard,
    ) {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let config_path = temp.path().join("bcode.toml");
        unsafe {
            std::env::set_var("BCODE_CONFIG", &config_path);
        }
        let store = SettingsStore::from_settings_db_path(temp.path().join("settings.db"));
        (temp, store, config_path, ConfigEnvGuard)
    }

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
        let (_temp, store, config_path, _guard) = isolated_config_store();
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
        assert_eq!(
            shell
                .handle_action(OnboardingInputAction::ToggleProvider, &store, 52)
                .expect("provider toggle should persist"),
            OnboardingActionOutcome::Selected(SetupSectionId::Providers)
        );
        assert_eq!(
            shell
                .handle_action(OnboardingInputAction::ToggleAuthProfile, &store, 53)
                .expect("auth toggle should persist"),
            OnboardingActionOutcome::Selected(SetupSectionId::SecureVault)
        );
        assert_eq!(
            shell
                .handle_action(OnboardingInputAction::SelectModelProfile, &store, 54)
                .expect("model selection should persist"),
            OnboardingActionOutcome::Selected(SetupSectionId::Models)
        );
        assert_eq!(
            shell
                .handle_action(OnboardingInputAction::CyclePermissionPreset, &store, 55)
                .expect("permission preset should persist"),
            OnboardingActionOutcome::Selected(SetupSectionId::Permissions)
        );
        assert_eq!(
            shell
                .handle_action(OnboardingInputAction::ReviewSessionImport, &store, 56)
                .expect("import review should persist"),
            OnboardingActionOutcome::Selected(SetupSectionId::Imports)
        );
        assert_eq!(
            shell
                .handle_action(OnboardingInputAction::ReviewPlugins, &store, 57)
                .expect("plugin review should persist"),
            OnboardingActionOutcome::Selected(SetupSectionId::Plugins)
        );
        let draft = store.onboarding_draft_setup().expect("draft should reload");
        assert!(draft.providers.contains("openai-compatible"));
        assert!(draft.auth_profiles.contains("default"));
        assert_eq!(draft.model_profile.as_deref(), Some("default"));
        assert_eq!(draft.permission_preset.as_deref(), Some("cautious"));
        assert_eq!(
            shell
                .handle_action(OnboardingInputAction::ApplyPlan, &store, 58)
                .expect("setup plan should request confirmation"),
            OnboardingActionOutcome::Ignored
        );
        let render = shell.render_model(&SettingsDbHealth::Available, None);
        assert!(render.pending_confirmation.is_some());
        assert!(render.snapshot_text().contains("Apply setup plan?"));
        assert_eq!(
            shell
                .handle_action(OnboardingInputAction::Confirm, &store, 59)
                .expect("setup plan should apply"),
            OnboardingActionOutcome::Selected(SetupSectionId::Launch)
        );
        assert!(
            store
                .control_state("setup.last_apply_reconciliation")
                .expect("reconciliation should load")
                .is_some()
        );
        let written_config =
            std::fs::read_to_string(&config_path).expect("config should be written");
        assert!(written_config.contains("bcode.openai-compatible"));
        assert!(written_config.contains("backend = \"sshenv\""));
        let render = shell.render_model(&SettingsDbHealth::Available, None);
        assert!(render.has_visual_composition());

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
    fn render_model_snapshot_audits_secret_safe() {
        let summary = SetupConfigSummary::default();
        let shell = OnboardingShell::from_reconciliation(&[], &summary.reconciliation_input());
        let render = shell.render_model(&SettingsDbHealth::Available, None);
        let audit = render.secret_audit();

        assert!(audit.safe);
        assert!(render.snapshot_text().contains("Base Camp"));
        assert!(render.focused_detail.story.contains("Base Camp"));
        assert!(!render.focused_detail.actions.is_empty());
    }

    #[test]
    fn shell_completes_skips_and_launches_sections() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let store = SettingsStore::from_settings_db_path(temp.path().join("settings.db"));
        let summary = SetupConfigSummary::default();
        let mut shell = OnboardingShell::load(&store, &summary).expect("shell should load");

        assert_eq!(
            shell
                .handle_action(OnboardingInputAction::Complete, &store, 60)
                .expect("complete should persist"),
            OnboardingActionOutcome::Completed(SetupSectionId::Welcome)
        );
        shell.focus_next();
        assert_eq!(
            shell
                .handle_action(OnboardingInputAction::Skip, &store, 61)
                .expect("skip should persist"),
            OnboardingActionOutcome::Skipped(SetupSectionId::Detection)
        );
        assert_eq!(
            shell
                .handle_action(OnboardingInputAction::Launch, &store, 62)
                .expect("launch should complete onboarding"),
            OnboardingActionOutcome::LaunchReady
        );

        let sections = store.onboarding_sections().expect("sections should load");
        let progress = store
            .onboarding_progress()
            .expect("progress should load")
            .expect("progress should exist");

        assert!(sections.iter().any(|section| {
            section.section_id == SetupSectionId::Welcome.as_str()
                && section.status == SetupSectionStatus::Complete.as_str()
        }));
        assert!(sections.iter().any(|section| {
            section.section_id == SetupSectionId::Detection.as_str()
                && section.status == SetupSectionStatus::Skipped.as_str()
        }));
        assert!(progress.first_run_completed);
        assert_eq!(
            shell.status_message(),
            Some("Onboarding complete — ready to launch Bcode")
        );
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
