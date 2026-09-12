//! Runtime-owned first-run onboarding screen.

use std::io::Write;

use bmux_keyboard::KeyCode;
use bmux_tui::damage::Damage;
use bmux_tui::event::Event;
use bmux_tui::geometry::Rect;
use bmux_tui::terminal::Terminal;
use bmux_tui_runtime::{
    Invalidation, Lifecycle, PresentReport, Presenter, Program, RuntimeEvent, Update,
};

use super::{TuiError, current_time_ms, onboarding, onboarding_render};

/// Runtime-owned onboarding messages.
pub enum OnboardingMessage {
    /// Terminal input backend failure.
    /// Poll background authentication progress without blocking input.
    AuthProgress,
    LaunchValidated {
        generation: u64,
        result: Result<(), String>,
    },
    ModelsDiscovered {
        generation: u64,
        result: Result<Box<super::setup_settings_form::SetupSettingsForm>, String>,
    },
    InputFailed(std::io::Error),
}

/// Serialized onboarding state owned by the BMUX runtime.
pub struct OnboardingProgram {
    store: bcode_settings::SettingsStore,
    shell: onboarding::OnboardingShell,
    health: bcode_settings::SettingsDbHealth,
    readiness: Option<bcode_settings::SetupReadinessReport>,
    theme: super::theme::PresentedTheme,
    area: Rect,
    continuation: bcode_settings::SetupContinuation,
    launch_generation: u64,
    model_discovery_requested: bool,
    launch_pending: bool,
    connection_form: Option<super::setup_connection_form::ConnectionForm>,
    settings_form: Option<super::setup_settings_form::SetupSettingsForm>,
}

impl OnboardingProgram {
    /// Create onboarding state for the current terminal area.
    pub fn new(
        store: bcode_settings::SettingsStore,
        shell: onboarding::OnboardingShell,
        theme: &super::theme::PresentedTheme,
        area: Rect,
    ) -> Result<Self, TuiError> {
        let health = store.health();
        let readiness = store.readiness_report()?;
        Ok(Self {
            store,
            shell,
            health,
            readiness,
            theme: *theme,
            area,
            continuation: bcode_settings::SetupContinuation::Close,
            launch_generation: 0,
            launch_pending: false,
            model_discovery_requested: false,
            connection_form: None,
            settings_form: None,
        })
    }

    /// Requested application continuation after terminal teardown.
    #[must_use]
    pub const fn continuation(&self) -> bcode_settings::SetupContinuation {
        self.continuation
    }

    fn refresh_persisted_state(&mut self) -> Result<(), TuiError> {
        self.health = self.store.health();
        self.readiness = self.store.readiness_report()?;
        Ok(())
    }

    fn auth_progress_command() -> bmux_tui_runtime::Command<OnboardingMessage> {
        bmux_tui_runtime::Command::concurrent(async {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            Some(OnboardingMessage::AuthProgress)
        })
    }

    fn open_settings(&mut self, key: KeyCode) {
        let path = bcode_config::default_config_dir().join("bcode.toml");
        self.settings_form = match key {
            KeyCode::Char('N') => {
                Some(super::setup_settings_form::SetupSettingsForm::create_context(&path))
            }
            KeyCode::Char('o') => {
                if let Ok(config) = bcode_config::load_config() {
                    Some(super::setup_settings_form::SetupSettingsForm::contexts(
                        &path, &config,
                    ))
                } else {
                    self.shell.set_status_message(
                        "Cannot load context definitions. Review Settings.".to_owned(),
                    );
                    None
                }
            }
            _ => Some(super::setup_settings_form::SetupSettingsForm::new(
                &path,
                "model/profile",
            )),
        };
    }

    fn complete_model_discovery(
        &mut self,
        generation: u64,
        result: Result<Box<super::setup_settings_form::SetupSettingsForm>, String>,
    ) -> Update<OnboardingMessage> {
        if generation == self.launch_generation && self.model_discovery_requested {
            self.model_discovery_requested = false;
            match result {
                Ok(form) => self.settings_form = Some(*form),
                Err(message) => self.shell.set_status_message(message),
            }
        }
        Update {
            invalidation: Invalidation::Redraw,
            ..Update::none()
        }
    }

    fn pending_commands(
        &self,
        previous: u64,
        had_connection: bool,
    ) -> Vec<bmux_tui_runtime::Command<OnboardingMessage>> {
        if self.model_discovery_requested && previous != self.launch_generation {
            let generation = self.launch_generation;
            vec![bmux_tui_runtime::Command::concurrent(async move {
                Some(OnboardingMessage::ModelsDiscovered {
                    generation,
                    result: discover_setup_models().await.map(Box::new),
                })
            })]
        } else if self.launch_pending && previous != self.launch_generation {
            vec![launch_validation_command(self.launch_generation)]
        } else if !had_connection && self.connection_form.is_some() {
            vec![Self::auth_progress_command()]
        } else {
            Vec::new()
        }
    }

    fn start_model_discovery(&mut self, code: KeyCode) -> bool {
        self.model_discovery_requested = false;
        self.launch_generation = self.launch_generation.wrapping_add(1);
        if code != KeyCode::Char('M') {
            return false;
        }
        self.model_discovery_requested = true;
        self.shell.set_status_message(
            "Discovering models for the context's account… Any key cancels.".to_owned(),
        );
        true
    }

    fn handle_key(&mut self, code: KeyCode) -> Result<Lifecycle, TuiError> {
        if self.launch_pending {
            self.launch_pending = false;
            self.launch_generation = self.launch_generation.wrapping_add(1);
        }
        if self.start_model_discovery(code) {
            return Ok(Lifecycle::Continue);
        }
        let code = if code == KeyCode::Enter && !self.shell.has_pending_confirmation() {
            use bcode_settings::SetupSectionId;
            match self.shell.focused_section() {
                SetupSectionId::Providers => KeyCode::Char('p'),
                SetupSectionId::SecureVault => KeyCode::Char('a'),
                SetupSectionId::Models => KeyCode::Char('m'),
                SetupSectionId::Launch => KeyCode::Char('l'),
                SetupSectionId::Welcome => KeyCode::Down,
                SetupSectionId::Detection
                | SetupSectionId::Permissions
                | SetupSectionId::Imports
                | SetupSectionId::Plugins => {
                    let message = match self.shell.focused_section() {
                        SetupSectionId::Detection => {
                            "Press a to review a credential import, or p to connect a provider."
                        }
                        SetupSectionId::Permissions => {
                            "Permission editing is not implemented in this setup screen yet. Your existing policy is unchanged."
                        }
                        SetupSectionId::Imports => {
                            "Session import is not implemented in this setup screen yet. It is optional; press s to skip."
                        }
                        _ => {
                            "Plugin editing is available in Settings: press g and choose plugins/enabled or plugins/disabled."
                        }
                    };
                    self.shell.set_status_message(message.to_owned());
                    return Ok(Lifecycle::Continue);
                }
            }
        } else {
            code
        };
        match code {
            KeyCode::Char('N' | 'o' | 'r' | 'g' | 'x')
                if !self.shell.has_pending_confirmation() =>
            {
                self.open_settings(code);
                Ok(Lifecycle::Continue)
            }
            KeyCode::Char('p' | 'a' | 'm') if !self.shell.has_pending_confirmation() => {
                if code == KeyCode::Char('m') {
                    match bcode_config::load_config() {
                        Ok(config) => {
                            self.settings_form = Some(super::setup_settings_form::SetupSettingsForm::models(
                                &bcode_config::default_config_dir().join("bcode.toml"),
                                &config,
                            ));
                        }
                        Err(_) => self.shell.set_status_message("Configuration could not be loaded. Review Settings before selecting a model.".to_owned()),
                    }
                } else {
                    self.connection_form = Some(super::setup_connection_form::ConnectionForm::new(
                        code == KeyCode::Char('a'),
                    ));
                }
                Ok(Lifecycle::Continue)
            }
            KeyCode::Escape | KeyCode::Char('q') => {
                let was_confirming = self.shell.has_pending_confirmation();
                self.shell.handle_action(
                    onboarding::OnboardingInputAction::CancelConfirmation,
                    &self.store,
                    current_time_ms(),
                )?;
                Ok(if was_confirming {
                    Lifecycle::Continue
                } else {
                    Lifecycle::Abort
                })
            }
            KeyCode::Right | KeyCode::Down | KeyCode::Char('j') => {
                self.shell.focus_next();
                Ok(Lifecycle::Continue)
            }
            KeyCode::Left | KeyCode::Up | KeyCode::Char('k') => {
                self.shell.focus_previous();
                Ok(Lifecycle::Continue)
            }
            _ => {
                let Some(action) = onboarding_action_for_key(code) else {
                    return Ok(Lifecycle::Continue);
                };
                let outcome = self
                    .shell
                    .handle_action(action, &self.store, current_time_ms())?;
                if outcome == onboarding::OnboardingActionOutcome::LaunchReady {
                    return Ok(self.finish_launch_selection(inspect_launch_selection()));
                }
                Ok(Lifecycle::Continue)
            }
        }
    }
    fn complete_launch_validation(
        &mut self,
        generation: u64,
        result: Result<(), String>,
    ) -> Update<OnboardingMessage> {
        if !self.launch_pending || generation != self.launch_generation {
            return Update::none();
        }
        self.launch_pending = false;
        let lifecycle = match result {
            Ok(()) => {
                self.continuation = bcode_settings::SetupContinuation::Launch;
                Lifecycle::Exit
            }
            Err(message) => {
                self.shell.set_status_message(message);
                Lifecycle::Continue
            }
        };
        Update {
            invalidation: Invalidation::Redraw,
            lifecycle,
            ..Update::none()
        }
    }

    fn finish_launch_selection(
        &mut self,
        selection: Result<bcode_config::ResolvedModelSelection, String>,
    ) -> Lifecycle {
        let result = selection.and_then(|selection| {
            selection
                .validate_selection()
                .map_err(|error| error.to_string())
        });
        if let Err(message) = result {
            self.shell.set_status_message(message);
            self.continuation = bcode_settings::SetupContinuation::Close;
            return Lifecycle::Continue;
        }
        self.launch_pending = true;
        self.launch_generation = self.launch_generation.wrapping_add(1);
        self.shell.set_status_message(
            "Validating provider configuration… Any key cancels this launch attempt.".to_owned(),
        );
        Lifecycle::Continue
    }
}

async fn discover_setup_models() -> Result<super::setup_settings_form::SetupSettingsForm, String> {
    let (config, context, provider, local, client) = tokio::task::spawn_blocking(|| {
        let mut config = bcode_config::load_config().map_err(|_| "Cannot load configuration".to_owned())?;
        let context = config.active_context.clone().ok_or_else(|| "Select a context before discovering account-specific models.".to_owned())?;
        let accounts = &config.contexts.as_ref().ok_or_else(|| "Missing contexts".to_owned())?.entries[&context].auth.profiles;
        if accounts.len() != 1 { return Err("Model discovery currently requires exactly one declared account in the selected context. Select a model profile for multi-account setups.".to_owned()); }
        let (local, account) = accounts.iter().next().ok_or_else(|| "Connect an account first".to_owned())?;
        let local = local.clone();
        let provider = account.owner_plugin_id.clone().ok_or_else(|| "Account ownership is missing".to_owned())?;
        let definition = config.contexts.as_mut().unwrap().entries.get_mut(&context).unwrap();
        definition.model.provider_plugin_id = Some(provider.clone());
        definition.model.auth_profile = Some(local.clone());
        definition.model.profile = None;
        definition.model.auth_pool = None;
        let encoded = bcode_config::encode_effective_config(&config).map_err(|_| "Cannot prepare discovery".to_owned())?;
        let resolved = bcode_config::load_config_from_paths_with_overrides(&[], &bcode_config::ConfigLoadOverrides::default().with_cli_config_toml(Some(encoded))).map_err(|_| "Cannot resolve discovery context".to_owned())?;
        let selection = resolved.resolved_model_selection();
        let auth = bcode_provider_auth::try_resolve_provider_request_context(bcode_provider_auth::ProviderRequestContextResolution { config: &resolved, selection }).map_err(|_| "Account is unavailable".to_owned())?;
        let runtime = bcode_ipc::ClientRuntimeContext {
            effective_config_toml: Some(Box::new(bcode_config::encode_effective_config(&resolved).map_err(|_| "Cannot encode discovery context".to_owned())?)),
            selected_provider_plugin_id: Some(provider.clone()), provider_context: auth,
            ..Default::default()
        };
        let client = bcode_client::BcodeClient::default_endpoint().with_runtime_context(Some(runtime));
        Ok::<_, String>((resolved, context, provider, local, client))
    }).await.map_err(|_| "Could not prepare model discovery".to_owned())??;
    let models = client
        .session_model_list(Some(provider.clone()))
        .await
        .map_err(|_| "Model discovery failed. Check Connections and retry.".to_owned())?;
    let _ = config;
    Ok(
        super::setup_settings_form::SetupSettingsForm::discovered_models(
            &bcode_config::default_config_dir().join("bcode.toml"),
            Some(context),
            provider,
            local,
            models,
        ),
    )
}

fn launch_validation_command(generation: u64) -> bmux_tui_runtime::Command<OnboardingMessage> {
    bmux_tui_runtime::Command::concurrent(async move {
        let result = validate_launch_provider().await;
        Some(OnboardingMessage::LaunchValidated { generation, result })
    })
}

async fn validate_launch_provider() -> Result<(), String> {
    let (selection, client) = tokio::task::spawn_blocking(|| {
        let selection = inspect_launch_selection()?;
        let client = bcode_client::BcodeClient::default_endpoint();
        Ok::<_, String>((selection, client))
    })
    .await
    .map_err(|_| "Could not prepare provider validation. Retry from setup.".to_owned())??;
    let validation = client
        .validate_provider_config(
            selection
                .provider_plugin_id
                .clone()
                .ok_or_else(|| "Select a provider.".to_owned())?,
        )
        .await
        .map_err(|_| {
            "Provider validation could not run. Review Connections and retry; setup remains open."
                .to_owned()
        })?;
    if !validation.valid {
        return Err(
            "Provider configuration is not ready. Review Connections and Models before launching."
                .to_owned(),
        );
    }
    let available = client
        .model_available_for_setup(
            selection
                .provider_plugin_id
                .clone()
                .ok_or_else(|| "Select a provider.".to_owned())?,
            selection
                .model_id
                .as_deref()
                .ok_or_else(|| "Select a model.".to_owned())?,
        )
        .await
        .map_err(|_| {
            "Could not load the model catalog. Check the connection and retry from setup."
                .to_owned()
        })?;
    if !available {
        return Err("The selected model could not be verified in the available catalog. Review Models before launching.".to_owned());
    }
    let current = tokio::task::spawn_blocking(inspect_launch_selection)
        .await
        .map_err(|_| "Could not recheck launch selection. Retry from setup.".to_owned())??;
    if current != selection {
        return Err("Model or account selection changed during validation. Review the selection and launch again.".to_owned());
    }
    Ok(())
}

fn inspect_launch_selection() -> Result<bcode_config::ResolvedModelSelection, String> {
    let config = bcode_config::load_config().map_err(|_| {
        "Configuration could not be loaded. Review Settings before launching.".to_owned()
    })?;
    let selection = config.resolved_model_selection();
    selection
        .validate_selection()
        .map_err(|error| error.to_string())?;
    bcode_provider_auth::inspect_auth_selection(&bcode_provider_auth::ProviderRequestContextResolution {
        config: &config, selection: selection.clone(),
    }).map_err(|_| "The selected account or pool could not be verified. Review Connections and authentication metadata before launching.".to_owned())?;
    Ok(selection)
}

impl Program for OnboardingProgram {
    type Message = OnboardingMessage;
    type Error = TuiError;

    fn update(
        &mut self,
        event: RuntimeEvent<Self::Message>,
    ) -> Result<Update<Self::Message>, Self::Error> {
        if let RuntimeEvent::Message(OnboardingMessage::ModelsDiscovered { generation, result }) =
            event
        {
            return Ok(self.complete_model_discovery(generation, result));
        }
        if let RuntimeEvent::Message(OnboardingMessage::LaunchValidated { generation, result }) =
            event
        {
            return Ok(self.complete_launch_validation(generation, result));
        }
        let previous_launch_generation = self.launch_generation;
        if matches!(
            event,
            RuntimeEvent::Message(OnboardingMessage::AuthProgress)
        ) {
            return Ok(Update {
                invalidation: Invalidation::Redraw,
                commands: if self.connection_form.is_some() {
                    vec![Self::auth_progress_command()]
                } else {
                    Vec::new()
                },
                ..Update::none()
            });
        }
        let had_connection = self.connection_form.is_some();
        if let RuntimeEvent::Terminal(ref terminal_event) = event
            && !matches!(terminal_event, Event::Resize(_))
            && let Some(form) = &mut self.connection_form
        {
            if form.handle_event(terminal_event) {
                self.connection_form = None;
            }
            return Ok(Update {
                invalidation: Invalidation::Redraw,
                ..Update::none()
            });
        }
        if let RuntimeEvent::Terminal(ref terminal_event) = event
            && !matches!(terminal_event, Event::Resize(_))
            && let Some(form) = &mut self.settings_form
        {
            if matches!(
                form.handle_event(terminal_event),
                super::setup_settings_form::SettingsFormOutcome::Close
            ) {
                self.settings_form = None;
            }
            return Ok(Update {
                invalidation: Invalidation::Redraw,
                ..Update::none()
            });
        }
        let mut lifecycle = Lifecycle::Continue;
        let invalidation = match event {
            RuntimeEvent::Terminal(Event::Resize(size)) => {
                self.area = Rect::new(0, 0, size.width, size.height);
                Invalidation::Redraw
            }
            RuntimeEvent::Terminal(Event::Key(key)) => {
                lifecycle = self.handle_key(key.key)?;
                Invalidation::Redraw
            }
            RuntimeEvent::Terminal(Event::Mouse(mouse)) => {
                let area = onboarding_render::onboarding_board_area(self.area);
                if area.contains(mouse.position)
                    && matches!(
                        mouse.kind,
                        bmux_tui::event::MouseEventKind::Up(bmux_tui::event::MouseButton::Left)
                    )
                {
                    self.shell
                        .focus_section_index(usize::from(mouse.position.y - area.y));
                }
                Invalidation::Redraw
            }
            RuntimeEvent::Terminal(
                Event::Paste(_) | Event::Focus(_) | Event::Tick | Event::User(_),
            )
            | RuntimeEvent::Timer(_)
            | RuntimeEvent::Message(
                OnboardingMessage::ModelsDiscovered { .. }
                | OnboardingMessage::LaunchValidated { .. },
            ) => Invalidation::None,
            RuntimeEvent::Message(OnboardingMessage::AuthProgress) => Invalidation::Redraw,
            RuntimeEvent::Message(OnboardingMessage::InputFailed(error)) => {
                return Err(error.into());
            }
        };
        self.refresh_persisted_state()?;
        Ok(Update {
            commands: self.pending_commands(previous_launch_generation, had_connection),
            invalidation,
            lifecycle,
            ..Update::none()
        })
    }
}

const fn onboarding_action_for_key(code: KeyCode) -> Option<onboarding::OnboardingInputAction> {
    match code {
        KeyCode::Enter => Some(onboarding::OnboardingInputAction::Select),
        KeyCode::Char('p') => Some(onboarding::OnboardingInputAction::ToggleProvider),
        KeyCode::Char('a') => Some(onboarding::OnboardingInputAction::ToggleAuthProfile),
        KeyCode::Char('m') => Some(onboarding::OnboardingInputAction::SelectModelProfile),
        KeyCode::Char('r') => Some(onboarding::OnboardingInputAction::CyclePermissionPreset),
        KeyCode::Char('i') => Some(onboarding::OnboardingInputAction::ReviewSessionImport),
        KeyCode::Char('g') => Some(onboarding::OnboardingInputAction::ReviewPlugins),
        KeyCode::Char('x') => Some(onboarding::OnboardingInputAction::ApplyPlan),
        KeyCode::Char('y') => Some(onboarding::OnboardingInputAction::Confirm),
        KeyCode::Char('n') => Some(onboarding::OnboardingInputAction::CancelConfirmation),
        KeyCode::Char('c') => Some(onboarding::OnboardingInputAction::Complete),
        KeyCode::Char('s') => Some(onboarding::OnboardingInputAction::Skip),
        KeyCode::Char('l') => Some(onboarding::OnboardingInputAction::Launch),
        _ => None,
    }
}

/// Onboarding presenter at the terminal-specific boundary.
pub struct OnboardingPresenter<'a, 'b, W> {
    terminal: &'a mut Terminal<&'b mut W>,
}

impl<'a, 'b, W> OnboardingPresenter<'a, 'b, W> {
    /// Create a presenter around the caller-owned terminal.
    pub const fn new(terminal: &'a mut Terminal<&'b mut W>) -> Self {
        Self { terminal }
    }
}

impl<W: Write> Presenter<OnboardingProgram> for OnboardingPresenter<'_, '_, W> {
    type Error = std::io::Error;

    fn resize(&mut self, size: bmux_tui::geometry::Size) {
        self.terminal
            .resize(Rect::new(0, 0, size.width, size.height));
    }

    fn reset(&mut self, _reason: bmux_tui_runtime::ResetReason) {
        self.terminal.reset();
    }

    fn present(&mut self, program: &mut OnboardingProgram) -> Result<PresentReport, Self::Error> {
        let stats = self.terminal.draw_damage(Damage::Full, |frame| {
            if let Some(form) = &mut program.connection_form {
                form.render(frame, &program.theme);
                return;
            }
            if let Some(form) = &mut program.settings_form {
                form.render(frame, &program.theme);
                return;
            }
            onboarding_render::render_onboarding(
                &program.shell,
                frame,
                &program.health,
                program.readiness.clone(),
                &program.theme,
            );
        })?;
        Ok(PresentReport {
            changed_cells: stats.changed_cells,
            full_repaint: stats.full_repaint,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::onboarding_action_for_key;
    use bmux_keyboard::KeyCode;
    use bmux_tui_runtime::Program as _;

    #[test]
    fn incomplete_launch_stays_inside_setup_and_can_be_retried() {
        let temp = tempfile::tempdir().unwrap();
        let store =
            bcode_settings::SettingsStore::from_settings_db_path(temp.path().join("settings.db"));
        let shell = crate::onboarding::OnboardingShell::from_reconciliation(
            &[],
            &bcode_settings::SetupConfigSummary::default().reconciliation_input(),
        );
        let theme = crate::theme::resolve_configured_theme(
            &bcode_config::TuiConfig::default(),
            temp.path(),
        );
        let mut program = super::OnboardingProgram::new(
            store,
            shell,
            &theme,
            bmux_tui::geometry::Rect::new(0, 0, 80, 24),
        )
        .unwrap();
        let mut selection = bcode_config::ResolvedModelSelection::default();
        for provider in [None, Some("example.provider".to_owned())] {
            selection.provider_plugin_id = provider;
            assert_eq!(
                program.finish_launch_selection(Ok(selection.clone())),
                bmux_tui_runtime::Lifecycle::Continue
            );
            assert_eq!(
                program.continuation(),
                bcode_settings::SetupContinuation::Close
            );
        }
        assert_eq!(
            program.finish_launch_selection(Err("Configuration unavailable".to_owned())),
            bmux_tui_runtime::Lifecycle::Continue
        );
        selection.model_id = Some("example-model".to_owned());
        assert_eq!(
            program.finish_launch_selection(Ok(selection)),
            bmux_tui_runtime::Lifecycle::Continue
        );
        assert!(program.launch_pending);
        let stale = program
            .update(bmux_tui_runtime::RuntimeEvent::Message(
                super::OnboardingMessage::LaunchValidated {
                    generation: program.launch_generation.wrapping_sub(1),
                    result: Ok(()),
                },
            ))
            .unwrap();
        assert_eq!(stale.lifecycle, bmux_tui_runtime::Lifecycle::Continue);
        let done = program
            .update(bmux_tui_runtime::RuntimeEvent::Message(
                super::OnboardingMessage::LaunchValidated {
                    generation: program.launch_generation,
                    result: Ok(()),
                },
            ))
            .unwrap();
        assert_eq!(done.lifecycle, bmux_tui_runtime::Lifecycle::Exit);
        assert_eq!(
            program.continuation(),
            bcode_settings::SetupContinuation::Launch
        );
    }

    #[test]
    fn failed_and_cancelled_launch_attempts_cannot_be_completed_by_late_success() {
        let temp = tempfile::tempdir().unwrap();
        let store =
            bcode_settings::SettingsStore::from_settings_db_path(temp.path().join("settings.db"));
        let shell = crate::onboarding::OnboardingShell::from_reconciliation(
            &[],
            &bcode_settings::SetupConfigSummary::default().reconciliation_input(),
        );
        let theme = crate::theme::resolve_configured_theme(
            &bcode_config::TuiConfig::default(),
            temp.path(),
        );
        let mut program = super::OnboardingProgram::new(
            store,
            shell,
            &theme,
            bmux_tui::geometry::Rect::new(0, 0, 80, 24),
        )
        .unwrap();
        let selection = bcode_config::ResolvedModelSelection {
            provider_plugin_id: Some("example.provider".to_owned()),
            model_id: Some("example-model".to_owned()),
            ..Default::default()
        };
        program.finish_launch_selection(Ok(selection.clone()));
        let failed_generation = program.launch_generation;
        let failure = program
            .complete_launch_validation(failed_generation, Err("Provider unavailable".to_owned()));
        assert_eq!(failure.lifecycle, bmux_tui_runtime::Lifecycle::Continue);
        assert!(!program.launch_pending);
        assert_eq!(
            program
                .complete_launch_validation(failed_generation, Ok(()))
                .lifecycle,
            bmux_tui_runtime::Lifecycle::Continue
        );
        program.finish_launch_selection(Ok(selection));
        let cancelled_generation = program.launch_generation;
        assert_eq!(
            program.handle_key(KeyCode::Down).unwrap(),
            bmux_tui_runtime::Lifecycle::Continue
        );
        assert!(!program.launch_pending);
        assert_eq!(
            program
                .complete_launch_validation(cancelled_generation, Ok(()))
                .lifecycle,
            bmux_tui_runtime::Lifecycle::Continue
        );
        assert_eq!(
            program.continuation(),
            bcode_settings::SetupContinuation::Close
        );
    }

    #[test]
    fn editing_actions_never_request_terminal_exit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store =
            bcode_settings::SettingsStore::from_settings_db_path(temp.path().join("settings.db"));
        let summary = bcode_settings::SetupConfigSummary::default();
        let shell = crate::onboarding::OnboardingShell::from_reconciliation(
            &[],
            &summary.reconciliation_input(),
        );
        let theme = crate::theme::resolve_configured_theme(
            &bcode_config::TuiConfig::default(),
            temp.path(),
        );
        let mut program = super::OnboardingProgram::new(
            store,
            shell,
            &theme,
            bmux_tui::geometry::Rect::new(0, 0, 80, 24),
        )
        .expect("program");
        for (section, importing) in [
            (bcode_settings::SetupSectionId::Providers, false),
            (bcode_settings::SetupSectionId::SecureVault, true),
        ] {
            let index = program
                .shell
                .sections()
                .iter()
                .position(|item| item.section_id == section)
                .expect("section");
            program.shell.focus_section_index(index);
            assert_eq!(
                program.handle_key(KeyCode::Enter).expect("select"),
                bmux_tui_runtime::Lifecycle::Continue
            );
            assert!(
                program.connection_form.is_some(),
                "Enter must open connection/import editor ({importing})"
            );
            program.connection_form = None;
        }
        let index = program
            .shell
            .sections()
            .iter()
            .position(|item| item.section_id == bcode_settings::SetupSectionId::Models)
            .expect("models");
        program.shell.focus_section_index(index);
        program.handle_key(KeyCode::Enter).expect("select models");
        assert!(program.settings_form.is_some());
        program.settings_form = None;
        for key in ['p', 'a', 'm', 'r', 'g', 'x'] {
            assert_eq!(
                program.handle_key(KeyCode::Char(key)).expect("input"),
                bmux_tui_runtime::Lifecycle::Continue
            );
            assert_eq!(
                program.continuation(),
                bcode_settings::SetupContinuation::Close
            );
            program.connection_form = None;
            program.settings_form = None;
        }
    }

    #[test]
    fn onboarding_runtime_maps_product_actions_without_terminal_types() {
        assert!(onboarding_action_for_key(KeyCode::Enter).is_some());
        assert!(onboarding_action_for_key(KeyCode::Char('p')).is_some());
        assert!(onboarding_action_for_key(KeyCode::Char('l')).is_some());
        assert_eq!(onboarding_action_for_key(KeyCode::F(1)), None);
    }
}
