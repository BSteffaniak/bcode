//! TUI startup flow.

#[cfg(all(test, unix))]
#[path = "managed_chat_tests.rs"]
mod managed_chat_tests;

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use bcode_client::{BcodeClient, DaemonAvailability};
use bcode_session_models::SessionId;
use bmux_tui::geometry::Rect;
use bmux_tui::terminal::Terminal;

use super::app::BmuxApp;
use super::effects::{TuiEffect, TuiEffectQueue};
use super::startup_action::StartupTuiAction;
use super::{TuiError, chat_loop, history_flow, root_program, session_flow};

/// Attach to a session and run the active chat loop.
#[allow(clippy::future_not_send, dead_code)]
pub async fn run_event_loop<W: Write>(
    terminal: &mut Terminal<&mut W>,
    session_id: Option<SessionId>,
) -> Result<(), TuiError> {
    Box::pin(run_event_loop_with_startup(
        terminal,
        session_id,
        StartupTuiAction::None,
    ))
    .await
}

/// Attach to a session and run the active chat loop with caller-provided static bundled plugins.
#[allow(clippy::future_not_send)]
pub async fn run_event_loop_with_static_bundled<W: Write>(
    terminal: &mut Terminal<&mut W>,
    session_id: Option<SessionId>,
    static_plugins: &[bcode_plugin::StaticBundledPlugin],
    launch_options: super::TuiLaunchOptions,
) -> Result<(), TuiError> {
    Box::pin(run_event_loop_with_startup_and_static_bundled(
        terminal,
        session_id,
        StartupTuiAction::None,
        static_plugins,
        launch_options,
    ))
    .await
}

/// Attach to a session, run an optional startup action, and run the active chat loop.
#[allow(clippy::future_not_send)]
pub async fn run_event_loop_with_startup<W: Write>(
    terminal: &mut Terminal<&mut W>,
    session_id: Option<SessionId>,
    startup_action: StartupTuiAction,
) -> Result<(), TuiError> {
    Box::pin(run_event_loop_with_startup_and_static_bundled(
        terminal,
        session_id,
        startup_action,
        &[],
        super::TuiLaunchOptions::default(),
    ))
    .await
}

/// Run a plugin-owned surface as the root runtime's complete standalone screen.
///
/// # Errors
///
/// Returns startup, plugin, client, or terminal runtime failures.
#[allow(clippy::future_not_send)]
pub async fn run_standalone_plugin_surface<W: Write>(
    terminal: &mut Terminal<&mut W>,
    plugin_id: impl Into<String>,
    surface: bcode_plugin_sdk::tui::BoxedPluginTuiSurface,
) -> Result<Option<serde_json::Value>, TuiError> {
    let presentation = prepare_presentation(&super::static_bundled_plugins()).await?;
    Box::pin(run_prepared_plugin_surface(
        terminal,
        plugin_id,
        surface,
        presentation,
    ))
    .await
}

/// Mount a plugin surface using resources loaded while the outer shell showed progress.
///
/// # Errors
/// Returns plugin/root runtime errors.
#[allow(clippy::future_not_send)]
pub async fn run_prepared_plugin_surface<W: Write>(
    terminal: &mut Terminal<&mut W>,
    plugin_id: impl Into<String>,
    surface: bcode_plugin_sdk::tui::BoxedPluginTuiSurface,
    presentation: PreparedPresentation,
) -> Result<Option<serde_json::Value>, TuiError> {
    let plugin_id = plugin_id.into();
    let (runtime, handle) = {
        let model =
            prepare_plugin_surface_model(terminal, plugin_id.clone(), surface, presentation);
        root_program::runtime(terminal, model)
    };
    let mut model = Box::pin(root_program::run(runtime, handle)).await?;
    let outcome = model
        .take_plugin_surface_result()
        .filter(|(closed_plugin_id, _)| closed_plugin_id == &plugin_id)
        .and_then(|(_, outcome)| outcome);
    drop(model);
    Ok(outcome)
}

fn prepare_plugin_surface_model<W: Write>(
    terminal: &Terminal<&mut W>,
    plugin_id: String,
    surface: bcode_plugin_sdk::tui::BoxedPluginTuiSurface,
    presentation: PreparedPresentation,
) -> root_program::BcodeRuntimeModel {
    let InitializedTui {
        client,
        settings,
        chat,
        ..
    } = initialize_tui_with_presentation(
        terminal.area(),
        None,
        &super::static_bundled_plugins(),
        super::TuiLaunchOptions::default(),
        presentation,
    );
    let passive_client = client
        .clone()
        .with_daemon_availability(DaemonAvailability::RequireRunning);
    let loop_state =
        chat_loop::ChatLoopState::new(&client, &passive_client, settings.metrics_enabled());
    drop(passive_client);
    drop(client);
    let mut model = root_program::BcodeRuntimeModel::new(chat, settings, loop_state);
    model.queue_standalone_plugin_surface(plugin_id, surface);
    model
}

/// Attach to a session, run an optional startup action, and run the active chat loop with caller-provided static bundled plugins.
#[allow(clippy::future_not_send)]
pub async fn run_event_loop_with_startup_and_static_bundled<W: Write>(
    terminal: &mut Terminal<&mut W>,
    session_id: Option<SessionId>,
    startup_action: StartupTuiAction,
    static_plugins: &[bcode_plugin::StaticBundledPlugin],
    launch_options: super::TuiLaunchOptions,
) -> Result<(), TuiError> {
    let connection = prepare_connection(session_id).await?;
    Box::pin(run_prepared_chat(
        terminal,
        connection,
        startup_action,
        static_plugins,
        launch_options,
    ))
    .await
}

pub struct PreparedConnection {
    session_id: Option<SessionId>,
    publisher: Option<super::resurrection::Publisher>,
    warning: Option<String>,
}

pub async fn prepare_connection(
    session_id: Option<SessionId>,
) -> Result<PreparedConnection, TuiError> {
    let config = bcode_config::load_config();
    let (session_id, publisher, warning) = if let Ok(config) = &config {
        super::resurrection::connect(&config.tui.session_resurrection, session_id).await?
    } else {
        // Preserve the existing in-TUI configuration error reporting path.
        (session_id, None, None)
    };
    Ok(PreparedConnection {
        session_id,
        publisher,
        warning,
    })
}

#[allow(clippy::future_not_send)]
pub async fn run_prepared_chat<W: Write>(
    terminal: &mut Terminal<&mut W>,
    connection: PreparedConnection,
    startup_action: StartupTuiAction,
    static_plugins: &[bcode_plugin::StaticBundledPlugin],
    launch_options: super::TuiLaunchOptions,
) -> Result<(), TuiError> {
    let PreparedConnection {
        session_id,
        publisher,
        warning,
    } = connection;
    let mut initialized =
        initialize_tui(terminal.area(), session_id, static_plugins, launch_options);
    if let Some(warning) = warning {
        initialized.chat.app.set_status(warning);
    }
    Box::pin(run_root(terminal, initialized, startup_action, publisher)).await
}

/// Configuration and plugin resources prepared without constructing terminal or session state.
pub struct PreparedPresentation {
    config: Result<bcode_config::BcodeConfig, bcode_config::ConfigError>,
    streaming_override: Result<
        Option<bcode_session_view_models::StreamingPresentationPolicy>,
        bcode_config::ConfigError,
    >,
    presentation: Result<super::plugin_tui::PluginTuiPresentation, bcode_plugin::PluginLoadError>,
    structure_rules:
        Result<Vec<bcode_presentation_policy::models::PresentationRule>, bcode_config::ConfigError>,
}

/// Prepare synchronous native-plugin/configuration loading on a blocking worker.
///
/// # Errors
/// Returns worker failure; configuration/plugin failures retain the existing in-root reporting.
pub async fn prepare_presentation(
    static_plugins: &[bcode_plugin::StaticBundledPlugin],
) -> Result<PreparedPresentation, TuiError> {
    let plugins = static_plugins.to_vec();
    Ok(tokio::task::spawn_blocking(move || load_presentation(&plugins)).await?)
}

fn load_presentation(static_plugins: &[bcode_plugin::StaticBundledPlugin]) -> PreparedPresentation {
    let config = bcode_config::load_config();
    let streaming_override = bcode_config::load_tui_streaming_presentation_override();
    let structure_rules = bcode_config::load_tui_presentation_rules();
    let presentation_config = config.as_ref().ok();
    let selection =
        presentation_config.map_or_else(bcode_plugin::PluginSelection::all_enabled, |config| {
            let ids =
                bcode_plugin::static_bundled_default_plugin_ids(static_plugins).unwrap_or_default();
            bcode_config::plugin_selection_with_default_plugin_ids(config, &ids)
        });
    let visual = presentation_config
        .map(|config| config.tui.visual_adapters.clone())
        .unwrap_or_default();
    let presentation = super::plugin_tui::load_default_presentation_with_static_bundled(
        &selection,
        visual,
        static_plugins,
        &super::bundled_tui_extensions(),
    );
    PreparedPresentation {
        config,
        streaming_override,
        presentation,
        structure_rules,
    }
}

/// Mount prepared resources, keeping all application/terminal construction on the foreground.
///
/// # Errors
/// Returns root runtime errors.
#[allow(clippy::future_not_send)]
pub async fn run_prepared_chat_with_presentation<W: Write>(
    terminal: &mut Terminal<&mut W>,
    connection: PreparedConnection,
    presentation: PreparedPresentation,
    static_plugins: &[bcode_plugin::StaticBundledPlugin],
    launch_options: super::TuiLaunchOptions,
) -> Result<(), TuiError> {
    let mut initialized = initialize_tui_with_presentation(
        terminal.area(),
        connection.session_id,
        static_plugins,
        launch_options,
        presentation,
    );
    if let Some(warning) = connection.warning {
        initialized.chat.app.set_status(warning);
    }
    Box::pin(run_root(
        terminal,
        initialized,
        StartupTuiAction::None,
        connection.publisher,
    ))
    .await
}

struct InitializedTui {
    client: BcodeClient,
    settings: chat_loop::TuiRuntimeSettings,
    chat: session_flow::ActiveChat,
    declarative_streaming_policy: bcode_session_view_models::StreamingPresentationPolicy,
    streaming_presentation_override: Option<bcode_session_view_models::StreamingPresentationPolicy>,
}

fn initialize_tui(
    terminal_area: Rect,
    session_id: Option<SessionId>,
    static_plugins: &[bcode_plugin::StaticBundledPlugin],
    launch_options: super::TuiLaunchOptions,
) -> InitializedTui {
    initialize_tui_with_presentation(
        terminal_area,
        session_id,
        static_plugins,
        launch_options,
        load_presentation(static_plugins),
    )
}

fn initialize_tui_with_presentation(
    terminal_area: Rect,
    session_id: Option<SessionId>,
    static_plugins: &[bcode_plugin::StaticBundledPlugin],
    launch_options: super::TuiLaunchOptions,
    presentation: PreparedPresentation,
) -> InitializedTui {
    let PreparedPresentation {
        config,
        streaming_override: streaming_presentation_override,
        presentation,
        structure_rules,
    } = presentation;
    let client = config
        .as_ref()
        .map_or_else(
            |_| BcodeClient::default_endpoint(),
            |config| {
                BcodeClient::default_endpoint()
                    .with_request_timeout(Duration::from_secs(config.client.request_timeout_secs))
            },
        )
        .with_interaction_adapters(super::bundled_interaction_adapters("tui"));
    let (event_sender, event_receiver) = history_flow::session_stream_channel();
    let mut app = BmuxApp::new_with_history(session_id, &[], &[], false);
    app.set_execution_mode_indicator(
        match (launch_options.permission_mode, launch_options.tool_policy) {
            (bcode_session_models::TurnPermissionMode::Bypass, _) => {
                Some("DANGER: PERMISSION BYPASS ACTIVE".to_owned())
            }
            (_, bcode_session_models::TurnToolPolicy::Disabled) => {
                Some("TOOLS DISABLED".to_owned())
            }
            _ => None,
        },
    );
    match presentation {
        Ok(presentation) => app.set_plugin_presentation(Arc::new(presentation)),
        Err(error) => app.set_status(format!("plugin presentation unavailable: {error}")),
    }
    let agents = session_flow::AgentCatalog::default();
    agents.refresh_app_agent_metadata(&mut app);
    let launch_working_directory = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let mut settings =
        chat_loop::TuiRuntimeSettings::bootstrap(launch_working_directory.clone(), static_plugins)
            .with_launch_options(launch_options);
    if let Ok(config) = &config {
        settings.set_metrics_enabled(config.metrics.enabled);
    }
    let mut chat = session_flow::ActiveChat {
        app,
        agents,
        attachment: session_flow::ChatSessionAttachment::Draft,
        event_sender,
        event_receiver,
        event_task: None,
        opening_session_progress: None,
        pending_effects: TuiEffectQueue::default(),
    };
    let (declarative_streaming_policy, effective_streaming_override) = apply_initial_config(
        config,
        streaming_presentation_override,
        session_id,
        &launch_working_directory,
        &mut settings,
        &mut chat,
    );
    match structure_rules {
        Ok(rules) => {
            chat.app.apply_user_structure_rules(rules);
        }
        Err(error) => chat
            .app
            .set_status(format!("TUI presentation rules unavailable: {error}")),
    }
    chat.start_effect(TuiEffect::LoadAgentCatalog);
    if let Some(session_id) = session_id {
        let initial_window_request = session_flow::initial_transcript_window_request(
            super::render::transcript_area_for_frame(&chat.app, terminal_area),
        );
        session_flow::start_switch_session(&mut chat, session_id, initial_window_request);
    } else {
        chat.app.set_status("New draft".to_owned());
    }
    InitializedTui {
        client,
        settings,
        chat,
        declarative_streaming_policy,
        streaming_presentation_override: effective_streaming_override,
    }
}

fn apply_initial_config(
    config: Result<bcode_config::BcodeConfig, bcode_config::ConfigError>,
    streaming_override: Result<
        Option<bcode_session_view_models::StreamingPresentationPolicy>,
        bcode_config::ConfigError,
    >,
    session_id: Option<SessionId>,
    launch_working_directory: &std::path::Path,
    settings: &mut chat_loop::TuiRuntimeSettings,
    chat: &mut session_flow::ActiveChat,
) -> (
    bcode_session_view_models::StreamingPresentationPolicy,
    Option<bcode_session_view_models::StreamingPresentationPolicy>,
) {
    let Ok(config) = config else {
        chat.start_effect(TuiEffect::LoadConfig);
        return (
            bcode_session_view_models::StreamingPresentationPolicy::default(),
            None,
        );
    };
    settings.apply_tui_config(&config.tui);
    if let Err(error) = chat.app.apply_plugin_presentation_config(&config) {
        chat.app.set_status(format!(
            "plugin presentation configuration unavailable: {error}"
        ));
    }
    if let Some(presentation) = chat.app.plugin_presentation() {
        settings.integrate_keymap_contributors(presentation.keymap_actions());
    }
    chat.app
        .set_effective_keymap(settings.keymap().registry().clone());
    settings.set_completion_settings(config.presentation.completion);
    chat.app.apply_tui_config(config.tui.clone());
    chat.app
        .apply_configured_structure_rules(config.presentation.rules.clone());
    let declarative = config.presentation.streaming.policy();
    let effective_override = match streaming_override {
        Ok(streaming_override) => {
            let effective = streaming_override.unwrap_or(declarative);
            let _ = chat.app.apply_streaming_presentation_policy(effective);
            streaming_override
        }
        Err(error) => {
            let _ = chat.app.apply_streaming_presentation_policy(declarative);
            chat.app
                .set_status(format!("TUI streaming state unavailable: {error}"));
            None
        }
    };
    if chat.app.managed_execution_session().is_none() {
        chat.start_effect(TuiEffect::ReconcileAuthSecurity {
            config: Box::new(config),
        });
    }
    if session_id.is_none() {
        chat.start_effect(TuiEffect::LoadDraftStatus {
            launch_working_directory: launch_working_directory.to_path_buf(),
        });
    }
    (declarative, effective_override)
}

impl super::TerminalShell {
    /// Explicitly reattach bounded canonical state for one retained managed owner.
    /// Never starts/recreates a native process or replays an accepted mutation.
    /// # Errors
    /// Rejects missing/foreign binding, changed instance/generation/configuration and attach errors.
    // Like the other TerminalShell journeys, this future owns the calling terminal thread.
    #[allow(clippy::future_not_send)]
    pub async fn run_managed_reattach(
        &mut self,
        session_id: SessionId,
        instance_id: &str,
        generation: u64,
        plugins: &[bcode_plugin::StaticBundledPlugin],
        options: super::TuiLaunchOptions,
    ) -> Result<(), TuiError> {
        let config = bcode_config::encode_effective_config(&bcode_config::load_config()?)?;
        let mut retained_instance = None;
        loop {
            if bcode_config::encode_effective_config(&bcode_config::load_config()?)? != config {
                return Err(std::io::Error::other(
                    "Managed reattachment configuration changed; no fallback selected",
                )
                .into());
            }
            let request = history_flow::initial_transcript_window_request(self.terminal()?.area());
            let Some((connection, attached, binding)) = Box::pin(self.wait(
                "Reattaching the existing managed session…",
                prepare_managed_reattachment(
                    bcode_client::ManagedSessionClient::default_endpoint(),
                    session_id,
                    instance_id,
                    generation,
                    request,
                ),
            ))
            .await?
            else {
                return Ok(());
            };
            if retained_instance
                .as_ref()
                .is_some_and(|instance| instance != &binding.instance)
            {
                return Err(std::io::Error::other(
                    "Managed instance changed during auth recovery; reattach explicitly",
                )
                .into());
            }
            retained_instance = Some(binding.instance.clone());
            let Some(commands) = Box::pin(run_managed_chat_bound(
                &mut self.terminal()?,
                connection,
                attached,
                plugins,
                options,
                Some(&binding),
            ))
            .await?
            else {
                return Ok(());
            };
            // Root and its input reader have stopped. /auth login was explicit consent; the
            // only subsequent reentry is bounded observation, never Start or input replay.
            #[cfg(unix)]
            {
                let interface = commands.terminal_auth_interface().await?.ok_or_else(|| {
                    std::io::Error::other("No native terminal auth service declared")
                })?;
                let mut flow = commands.auth_flow().await?;
                let result = self.run_managed_auth_handoff(&mut flow, interface).await;
                if result.is_err() {
                    return Err(std::io::Error::other("Native login outcome unavailable; reattach and inspect /auth status without replay").into());
                }
            }
            #[cfg(not(unix))]
            {
                drop(commands);
                return Err(
                    std::io::Error::other("Native terminal authentication unsupported").into(),
                );
            }
        }
    }
}

async fn prepare_managed_reattachment(
    client: bcode_client::ManagedSessionClient,
    session_id: SessionId,
    instance_id: &str,
    generation: u64,
    request: bcode_session_models::ProjectionWindowRequest,
) -> Result<
    (
        bcode_client::ManagedSessionConnection,
        bcode_client::AttachedSessionHistory,
        bcode_ipc::ManagedSessionTargetSnapshot,
    ),
    TuiError,
> {
    let (connection, attached) = client
        .attach_session_projection_window(session_id, request)
        .await?;
    let binding = connection.binding().await?;
    validate_reattach_binding(
        session_id,
        instance_id,
        generation,
        attached.session.id,
        &binding,
    )?;
    Ok((connection, attached, binding))
}

fn validate_reattach_binding(
    session_id: SessionId,
    instance_id: &str,
    generation: u64,
    attached_session: SessionId,
    binding: &bcode_ipc::ManagedSessionTargetSnapshot,
) -> Result<(), TuiError> {
    if binding.validate().is_err()
        || binding.session_id != session_id
        || attached_session != session_id
        || binding.instance.instance_id != instance_id
        || generation == 0
        || binding.target.generation() != generation
    {
        return Err(std::io::Error::other(
            "Managed reattachment owner/instance/generation mismatch; no fallback selected",
        )
        .into());
    }
    Ok(())
}

/// Run an explicit managed conversation using the same root, reducer and transcript.
/// No ordinary client, model catalog, model status or provider credentials are constructed.
/// # Errors
/// Returns managed binding, canonical attach, configuration or terminal errors.
#[allow(clippy::future_not_send)]
pub async fn run_managed_chat<W: Write + Send>(
    terminal: &mut Terminal<&mut W>,
    connection: bcode_client::ManagedSessionConnection,
    attached: bcode_client::AttachedSessionHistory,
    static_plugins: &[bcode_plugin::StaticBundledPlugin],
    launch_options: super::TuiLaunchOptions,
) -> Result<Option<bcode_client::ManagedSessionCommandHandle>, TuiError> {
    Box::pin(run_managed_chat_bound(
        terminal,
        connection,
        attached,
        static_plugins,
        launch_options,
        None,
    ))
    .await
}

async fn run_managed_chat_bound<W: Write + Send>(
    terminal: &mut Terminal<&mut W>,
    connection: bcode_client::ManagedSessionConnection,
    attached: bcode_client::AttachedSessionHistory,
    static_plugins: &[bcode_plugin::StaticBundledPlugin],
    launch_options: super::TuiLaunchOptions,
    expected_binding: Option<&bcode_ipc::ManagedSessionTargetSnapshot>,
) -> Result<Option<bcode_client::ManagedSessionCommandHandle>, TuiError> {
    let session_id = connection.session_id();
    if attached.session.id != session_id {
        return Err(TuiError::Client(bcode_client::ClientError::Protocol(
            "Managed attachment owner mismatch".into(),
        )));
    }
    let commands = connection.command_handle();
    let binding = commands.binding().await?;
    if let Some(expected) = expected_binding {
        validate_reattach_binding(
            session_id,
            &expected.instance.instance_id,
            expected.target.generation(),
            attached.session.id,
            &binding,
        )?;
        if binding.instance != expected.instance {
            return Err(std::io::Error::other(
                "Managed instance configuration changed before reentry",
            )
            .into());
        }
    }
    let generation = binding.target.generation();
    let PreparedPresentation {
        config,
        streaming_override,
        presentation,
        structure_rules,
    } = prepare_presentation(static_plugins).await?;
    let (event_sender, event_receiver) = history_flow::session_stream_channel();
    let agents = session_flow::AgentCatalog::default();
    let cwd = attached.session.working_directory.clone();
    let mut settings = chat_loop::TuiRuntimeSettings::bootstrap(cwd.clone(), static_plugins)
        .with_launch_options(launch_options);
    let run = {
        let mut chat = session_flow::ActiveChat {
            app: {
                let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
                app.set_managed_execution(session_id);
                if let Ok(rules) = structure_rules {
                    app.apply_user_structure_rules(rules);
                }
                if let Ok(presentation) = presentation {
                    app.set_plugin_presentation(Arc::new(presentation));
                }
                app
            },
            agents,
            attachment: session_flow::ChatSessionAttachment::Opening {
                session_id,
                anchor_sequence: None,
            },
            event_sender: event_sender.clone(),
            event_receiver,
            event_task: None,
            opening_session_progress: None,
            pending_effects: TuiEffectQueue::default(),
        };
        let (declarative, streaming) = apply_initial_config(
            config,
            streaming_override,
            Some(session_id),
            &cwd,
            &mut settings,
            &mut chat,
        );
        let task = tokio::spawn(history_flow::forward_managed_session_events(
            connection,
            event_sender,
        ));
        session_flow::complete_switch_session(&mut chat, session_id, true, Ok((attached, task)));
        chat.app.set_managed_execution(session_id);
        match commands.auth_flow().await {
        Ok(mut auth) => match auth.readiness().await {
            Ok(status) if matches!(status.readiness, bcode_provider_auth_models::managed::AuthReadiness::Ready | bcode_provider_auth_models::managed::AuthReadiness::Verified) => {
                chat.app.set_status("Managed conversation attached; native authentication is ready".into());
            }
            Ok(_) => chat.app.set_status("Managed conversation attached; native authentication requires attention before input".into()),
            Err(_) => chat.app.set_status("Managed conversation attached; native authentication readiness is unavailable".into()),
        },
        Err(_) => chat.app.set_status("Managed conversation attached; native authentication channel is unavailable".into()),
    }
        let mut loop_state = chat_loop::ChatLoopState::new_managed(commands.clone(), generation);
        loop_state.declarative_streaming_policy = declarative;
        loop_state.streaming_presentation_override = streaming;
        run_managed_root_runtime(terminal, chat, settings, loop_state)
    };
    let auth_requested = finish_managed_attachment(run.await, commands.detach_commands()).await?;
    Ok(auth_requested.then_some(commands))
}

// Ordinary exit/error must await command-channel revocation, even if event-task abort
// has not yet been polled. Explicit auth handoff retains commands until login finishes.
async fn finish_managed_attachment(
    result: Result<bool, TuiError>,
    detach: impl std::future::Future<Output = ()>,
) -> Result<bool, TuiError> {
    if !matches!(result, Ok(true)) {
        detach.await;
    }
    result
}

// Keep root construction and the returned model in a separate poll frame from
// presentation/configuration/attachment setup. This is still the same root UI.
fn run_managed_root_runtime<'a, 'b, W: Write + Send + 'b>(
    terminal: &'a mut Terminal<&'b mut W>,
    chat: session_flow::ActiveChat,
    settings: chat_loop::TuiRuntimeSettings,
    loop_state: chat_loop::ChatLoopState,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, TuiError>> + Send + 'a>>
where
    'b: 'a,
{
    let run = {
        let (runtime, handle) = root_program::runtime(
            terminal,
            root_program::BcodeRuntimeModel::new(chat, settings, loop_state),
        );
        Box::pin(root_program::run(runtime, handle))
    };
    Box::pin(async move {
        // run owns the model and drains input/accepted operations before reentry.
        let model = run.await?;
        let auth_requested = model.chat.app.managed_auth_requested();
        drop(model);
        Ok(auth_requested)
    })
}

async fn run_root<W: Write>(
    terminal: &mut Terminal<&mut W>,
    initialized: InitializedTui,
    startup_action: StartupTuiAction,
    publisher: Option<super::resurrection::Publisher>,
) -> Result<(), TuiError> {
    let passive_client = initialized
        .client
        .clone()
        .with_daemon_availability(DaemonAvailability::RequireRunning);
    let mut loop_state = chat_loop::ChatLoopState::new(
        &initialized.client,
        &passive_client,
        initialized.settings.metrics_enabled(),
    );
    loop_state.declarative_streaming_policy = initialized.declarative_streaming_policy;
    loop_state.streaming_presentation_override = initialized.streaming_presentation_override;
    let model = {
        let mut model = root_program::BcodeRuntimeModel::new(
            initialized.chat,
            initialized.settings,
            loop_state,
        );
        model.resurrection = publisher;
        if let StartupTuiAction::OpenRalphHome { repo_path } = startup_action {
            let surface =
                super::ralph_launcher::open_root_ralph_home_surface(repo_path, None).await?;
            model.queue_plugin_surface("bcode.ralph", surface);
        }
        model
    };
    run_root_runtime(terminal, model).await
}

// Construct the runtime and its run future outside the startup coroutine. Merely
// boxing `run_root` would leave these large by-value temporaries in its poll frame.
fn run_root_runtime<W: Write>(
    terminal: &mut Terminal<&mut W>,
    model: root_program::BcodeRuntimeModel,
) -> impl std::future::Future<Output = Result<(), TuiError>> {
    let (runtime, handle) = root_program::runtime(terminal, model);
    let run = Box::pin(root_program::run(runtime, handle));
    async move {
        let _model = run.await?;
        Ok(())
    }
}

#[cfg(test)]
mod managed_reattach_tests {
    use super::*;

    #[tokio::test]
    async fn managed_exit_and_error_await_attachment_teardown() {
        for failed in [false, true] {
            let (entered, entered_rx) = tokio::sync::oneshot::channel();
            let (release, released) = tokio::sync::oneshot::channel();
            let result = if failed {
                Err(TuiError::Io(std::io::Error::other("fixture exit")))
            } else {
                Ok(false)
            };
            let exit = finish_managed_attachment(result, async {
                entered.send(()).unwrap();
                released.await.unwrap();
            });
            tokio::pin!(exit);
            tokio::select! {
                biased;
                _ = &mut exit => panic!("exit returned before command teardown"),
                () = async { entered_rx.await.unwrap(); } => {},
            }
            release.send(()).unwrap();
            assert_eq!(exit.await.is_err(), failed);
        }
    }

    #[tokio::test]
    async fn explicit_auth_handoff_does_not_detach_commands_before_login() {
        let result = finish_managed_attachment(Ok(true), async {
            panic!("auth handoff must retain its command channel");
        })
        .await;
        assert!(result.unwrap());
    }

    fn binding(owner: SessionId) -> bcode_ipc::ManagedSessionTargetSnapshot {
        serde_json::from_value(serde_json::json!({
            "session_id": owner,
            "instance": {"instance_id":"primary","plugin_id":"example.native","executable":"/native",
                "config_namespace":"/native/config","auth":{"provider_id":"example.native","profile":"native",
                "credential_namespace":"/native/config","custody":"native_managed"}},
            "target":{"state":"prepared","session":{"open_id":"open","session_id":owner,
                "instance_id":"primary","generation":1,"native_version":"fixture","requested_native_session_id":null}}
        })).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn explicit_managed_reattach_works_without_unrelated_model_credentials() {
        if std::env::var_os("BCODE_MANAGED_REATTACH_PROBE").is_some() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let sockets = root.path().join("ipc");
        std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700)
            .create(&sockets)
            .unwrap();
        let config = root.path().join("config.toml");
        std::fs::write(
            &config,
            r#"
[model]
provider_plugin_id = "example.unrelated"
auth_profile = "unrelated"
[auth.profiles.unrelated]
backend = "env"
provider_id = "example"
owner_plugin_id = "example.unrelated"
[auth.profiles.unrelated.map.api_key]
env = "BCODE_MANAGED_REATTACH_UNSET_KEY_71E9"
"#,
        )
        .unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime::managed_reattach_tests::managed_reattach_probe",
                "--nocapture",
            ])
            .env("BCODE_MANAGED_REATTACH_PROBE", "1")
            .env("BCODE_CONFIG", config)
            .env("BCODE_STATE_DIR", root.path().join("state"))
            .env("XDG_CONFIG_HOME", root.path().join("config-home"))
            .env(
                "BCODE_SOCKET",
                sockets.canonicalize().unwrap().join("s.sock"),
            )
            .env(
                "BCODE_IPC_ENDPOINT_NAMESPACE",
                bcode_ipc::daemon_namespace(),
            )
            .env_remove("BCODE_IPC_ENDPOINT")
            .env_remove("BCODE_DAEMON_LOG")
            .env_remove("BCODE_CONTEXT")
            .env_remove("BCODE_PROFILE")
            .env_remove("BCODE_MODEL_PROVIDER")
            .env_remove("BCODE_PROVIDER")
            .env_remove("BCODE_MODEL")
            .env_remove("BCODE_MODEL_PROFILE")
            .env_remove("BCODE_AUTH_PROFILE")
            .env_remove("BCODE_AUTH_POOL")
            .env_remove("BCODE_CLI_OVERRIDES_JSON")
            .env_remove("BCODE_MANAGED_REATTACH_UNSET_KEY_71E9")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(unix)]
    #[derive(Default)]
    struct PollProbe {
        polls: std::sync::atomic::AtomicUsize,
        held: tokio::sync::Notify,
        release: tokio::sync::Notify,
        committed: tokio::sync::Notify,
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn managed_reattach_probe() {
        if std::env::var_os("BCODE_MANAGED_REATTACH_PROBE").is_none() {
            return;
        }
        let owner = SessionId::new();
        let listener = bcode_ipc::LocalIpcListener::bind(&bcode_ipc::default_endpoint()).unwrap();
        let probe = Arc::new(PollProbe::default());
        let server_probe = Arc::clone(&probe);
        let (stop_peer, mut stop) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut tasks = Vec::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        tasks.push(tokio::spawn(serve_reattach_observation(accepted.unwrap(), owner, Arc::clone(&server_probe))));
                    }
                    _ = &mut stop => break,
                }
            }
            for task in tasks {
                task.abort();
                let _ = task.await;
            }
        });
        let request = history_flow::initial_transcript_window_request(Rect::new(0, 0, 80, 24));
        let (connection, attached, canonical) = tokio::time::timeout(
            Duration::from_secs(5),
            prepare_managed_reattachment(
                bcode_client::ManagedSessionClient::default_endpoint(),
                owner,
                "primary",
                1,
                request,
            ),
        )
        .await
        .unwrap()
        .unwrap();
        validate_reattach_binding(owner, "primary", 1, attached.session.id, &canonical).unwrap();
        assert_eq!(attached.draft.as_deref(), Some("retained draft"));
        let commands = connection.command_handle();
        let (sender, mut receiver) = history_flow::session_stream_channel();
        let forwarding = tokio::spawn(history_flow::forward_managed_session_events(
            connection, sender,
        ));
        Box::pin(exercise_held_poll_controls(commands, &probe)).await;
        probe.release.notify_one();
        let output = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(output, history_flow::SessionStreamUpdate::Event(event)
            if matches!(*event, bcode_ipc::Event::Session(ref event)
                if matches!(&event.kind, bcode_session_models::SessionEventKind::AssistantMessage { text } if text == "native output after empty page")))
        );
        assert!(probe.polls.load(std::sync::atomic::Ordering::SeqCst) >= 2);
        tokio::time::timeout(Duration::from_secs(5), probe.held.notified())
            .await
            .unwrap();
        // Dropping UI observation cancels its held read and poll futures promptly.
        drop(receiver);
        tokio::time::timeout(Duration::from_secs(1), forwarding)
            .await
            .unwrap()
            .unwrap();
        let polls = probe.polls.load(std::sync::atomic::Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(probe.polls.load(std::sync::atomic::Ordering::SeqCst), polls);
        stop_peer.send(()).unwrap();
        server.await.unwrap();
    }

    #[cfg(unix)]
    async fn exercise_held_poll_controls(
        commands: bcode_client::ManagedSessionCommandHandle,
        probe: &PollProbe,
    ) {
        tokio::time::timeout(Duration::from_secs(5), async {
            probe.held.notified().await;
            let retained = commands
                .prepare_managed_composer_submission(
                    ("poll-input".into(), "poll-attempt".into()),
                    "input during held poll".into(),
                    false,
                    None,
                )
                .await
                .unwrap();
            commands
                .submit_managed_composer_request(retained)
                .await
                .unwrap();
            commands
                .interrupt(1, "poll-interrupt".into())
                .await
                .unwrap();
        })
        .await
        .expect("native poll must not block input/interrupt");
    }

    #[cfg(unix)]
    async fn serve_reattach_observation(
        mut stream: bcode_ipc::LocalIpcStream,
        owner: SessionId,
        probe: Arc<PollProbe>,
    ) {
        use bcode_ipc::{Request, ResponsePayload};
        reattach_handshake(&mut stream).await;
        let envelope = bcode_ipc::recv_envelope(&mut stream).await.unwrap();
        let request = bcode_ipc::decode_request(&envelope.payload).unwrap();
        let attached_stream = matches!(&request, Request::AttachSessionProjectionWindow { .. });
        let payload = match request {
            Request::ManagedSession { request } => ResponsePayload::ManagedSession {
                response: bcode_ipc::ManagedSessionResponse {
                    schema_version: bcode_ipc::harness::MANAGED_SESSION_VERSION,
                    response_json: probe_managed_reply(owner, &request),
                },
            },
            Request::CallPluginService {
                interface_id,
                operation,
                payload,
            } => {
                assert_eq!(
                    interface_id,
                    bcode_ipc::harness::MANAGED_SESSION_INTERFACE_ID
                );
                assert_eq!(operation, "observe");
                let envelope: bcode_ipc::harness::ManagedSessionRequest =
                    serde_json::from_slice(&payload).unwrap();
                let query: bcode_ipc::harness::ManagedSessionPollRequest =
                    serde_json::from_str(&envelope.request_json).unwrap();
                assert_eq!(query.session_id, owner);
                assert_eq!(query.instance_id, "primary");
                assert_eq!(query.generation, 1);
                let poll = probe
                    .polls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if poll == 0 {
                    probe.held.notify_one();
                    probe.release.notified().await;
                } else if poll == 1 {
                    probe.committed.notify_one();
                } else {
                    probe.held.notify_one();
                    std::future::pending::<()>().await;
                }
                ResponsePayload::PluginServiceResult {
                    response: bcode_ipc::PluginServiceResponse {
                        payload: br#"{"committed":true}"#.to_vec(),
                        error: None,
                    },
                }
            }
            Request::AttachSessionProjectionWindow { session_id, .. } => {
                assert_eq!(session_id, owner);
                ResponsePayload::Attached {
                    session_id: owner,
                    session: serde_json::from_value(serde_json::json!({"id":owner,"name":null,"client_count":1,"created_at_ms":0,"updated_at_ms":0,"working_directory":"/workspace"})).unwrap(),
                    history: Vec::new(), input_history: Vec::new(), usage_summary: None, import_warnings: Vec::new(),
                    draft: Some("retained draft".into()), runtime_selection: bcode_session_models::SessionRuntimeSelection::default(), projection_window: None,
                }
            }
            _ => panic!("reattachment must remain bounded observation only"),
        };
        reattach_reply(&mut stream, envelope.request_id, payload).await;
        if attached_stream {
            probe.committed.notified().await;
            let event = bcode_ipc::Event::Session(bcode_session_models::SessionEvent {
                schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 1,
                timestamp_ms: 0,
                session_id: owner,
                provenance: None,
                kind: bcode_session_models::SessionEventKind::AssistantMessage {
                    text: "native output after empty page".into(),
                },
            });
            bcode_ipc::send_envelope(&mut stream, &bcode_ipc::event_envelope(&event).unwrap())
                .await
                .unwrap();
            // Keep the event transport open; receiver drop must stop its companion poller.
            std::future::pending::<()>().await;
        }
    }

    #[cfg(unix)]
    async fn reattach_handshake(stream: &mut bcode_ipc::LocalIpcStream) {
        use bcode_ipc::{Request, ResponsePayload};
        let hello = bcode_ipc::recv_envelope(stream).await.unwrap();
        let Request::Hello {
            runtime_context: Some(context),
            ..
        } = bcode_ipc::decode_request(&hello.payload).unwrap()
        else {
            panic!("configuration-bound hello required")
        };
        assert!(context.provider_context.auth.is_none());
        let config = bcode_config::decode_effective_config(
            context.effective_config_toml.as_deref().unwrap(),
        )
        .unwrap();
        assert!(
            config.auth.profiles["unrelated"]
                .map
                .contains_key("api_key")
        );
        let (_, digest) = bcode_daemon_lifecycle::current_executable_identity().unwrap();
        reattach_reply(
            stream,
            hello.request_id,
            ResponsePayload::Hello {
                protocol_version: bcode_ipc::ProtocolVersion::current(),
                client_id: bcode_session_models::ClientId::new(),
                daemon: bcode_ipc::DaemonStatus {
                    namespace: bcode_ipc::daemon_namespace(),
                    protocol_version: u32::from(bcode_ipc::CURRENT_PROTOCOL_VERSION),
                    artifact_id: Some(bcode_ipc::ArtifactId::current()),
                    build_fingerprint: bcode_ipc::BUILD_FINGERPRINT.into(),
                    executable_digest: Some(digest),
                    storage_writer_epoch: Some(bcode_ipc::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
                    session_event_schema_version: Some(
                        bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                    ),
                    state_location_id: Some(bcode_ipc::state_location_id()),
                    ..bcode_ipc::DaemonStatus::default()
                },
            },
        )
        .await;
    }

    #[cfg(unix)]
    fn probe_managed_reply(
        owner: SessionId,
        request: &bcode_ipc::harness::ManagedSessionRequest,
    ) -> String {
        use bcode_harness_models::{HarnessRequestKind, HarnessServiceResponseKind};
        if request.operation == bcode_ipc::harness::ManagedSessionOperation::Binding {
            let input: serde_json::Value = serde_json::from_str(&request.request_json).unwrap();
            assert_eq!(input["session_id"], serde_json::json!(owner));
            return serde_json::to_string(&Some(binding(owner))).unwrap();
        }
        let call = request.decode_call().unwrap();
        assert_eq!(call.session_id, Some(owner));
        let operation = call.harness.kind.operation();
        let kind = match call.harness.kind {
            HarnessRequestKind::Submit { submission, .. } => {
                assert_eq!(submission.text, "input during held poll");
                HarnessServiceResponseKind::Receipt(bcode_harness_models::HarnessReceipt {
                    submission_id: submission.submission_id,
                    attempt_id: submission.attempt_id,
                    session_id: owner.to_string(),
                    generation: 1,
                    state: bcode_harness_models::HarnessDeliveryState::Accepted,
                    native_request_id: None,
                })
            }
            HarnessRequestKind::Interrupt(control) => HarnessServiceResponseKind::Acknowledged {
                mutation_id: control.mutation_id,
            },
            _ => panic!("only input/control during held poll"),
        };
        serde_json::to_string(&bcode_harness_models::HarnessServiceResponse {
            schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
            request_id: call.harness.request_id,
            operation,
            kind,
        })
        .unwrap()
    }

    #[cfg(unix)]
    async fn reattach_reply(
        stream: &mut bcode_ipc::LocalIpcStream,
        id: u64,
        payload: bcode_ipc::ResponsePayload,
    ) {
        let envelope = bcode_ipc::response_envelope(id, &bcode_ipc::Response::Ok(payload)).unwrap();
        bcode_ipc::send_envelope(stream, &envelope).await.unwrap();
    }

    #[test]
    fn reattachment_rejects_foreign_session_instance_generation_and_invalid_canonical_binding() {
        let owner = SessionId::new();
        let snapshot = binding(owner);
        validate_reattach_binding(owner, "primary", 1, owner, &snapshot).unwrap();
        assert!(
            validate_reattach_binding(SessionId::new(), "primary", 1, owner, &snapshot).is_err()
        );
        assert!(
            validate_reattach_binding(owner, "primary", 1, SessionId::new(), &snapshot).is_err()
        );
        assert!(validate_reattach_binding(owner, "foreign", 1, owner, &snapshot).is_err());
        assert!(validate_reattach_binding(owner, "primary", 2, owner, &snapshot).is_err());
        assert!(validate_reattach_binding(owner, "primary", 0, owner, &snapshot).is_err());
        let mut inconsistent = snapshot;
        inconsistent.instance.instance_id = "foreign".into();
        assert!(validate_reattach_binding(owner, "foreign", 1, owner, &inconsistent).is_err());
    }
}
