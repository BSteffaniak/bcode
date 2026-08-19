//! TUI startup flow.

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
    let initialized = initialize_tui(
        terminal.area(),
        None,
        &super::static_bundled_plugins(),
        super::TuiLaunchOptions::default(),
    );
    let passive_client = initialized
        .client
        .clone()
        .with_daemon_availability(DaemonAvailability::RequireRunning);
    let loop_state = chat_loop::ChatLoopState::new(
        &initialized.client,
        &passive_client,
        initialized.settings.metrics_enabled(),
    );
    let mut model =
        root_program::BcodeRuntimeModel::new(initialized.chat, initialized.settings, loop_state);
    let plugin_id = plugin_id.into();
    model.queue_standalone_plugin_surface(plugin_id.clone(), surface);
    let (runtime, handle) = root_program::runtime(terminal, model);
    let mut model = Box::pin(root_program::run(runtime, handle)).await?;
    Ok(model
        .take_plugin_surface_result()
        .filter(|(closed_plugin_id, _)| closed_plugin_id == &plugin_id)
        .and_then(|(_, outcome)| outcome))
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
    let initialized = initialize_tui(terminal.area(), session_id, static_plugins, launch_options);
    Box::pin(run_root(terminal, initialized, startup_action)).await
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
    let config = bcode_config::load_config();
    let streaming_presentation_override = bcode_config::load_tui_streaming_presentation_override();
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
    let presentation_config = config.as_ref().ok();
    let plugin_selection =
        presentation_config.map_or_else(bcode_plugin::PluginSelection::all_enabled, |config| {
            let default_plugin_ids =
                bcode_plugin::static_bundled_default_plugin_ids(static_plugins).unwrap_or_default();
            bcode_config::plugin_selection_with_default_plugin_ids(config, &default_plugin_ids)
        });
    let visual_adapter_config = presentation_config
        .map(|config| config.tui.visual_adapters.clone())
        .unwrap_or_default();
    match super::plugin_tui::load_default_presentation_with_static_bundled(
        &plugin_selection,
        visual_adapter_config,
        static_plugins,
        &super::bundled_tui_extensions(),
    ) {
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
    chat.app.apply_tui_config(config.tui.clone());
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
    chat.start_effect(TuiEffect::ReconcileAuthSecurity {
        config: Box::new(config),
    });
    if session_id.is_none() {
        chat.start_effect(TuiEffect::LoadDraftStatus {
            launch_working_directory: launch_working_directory.to_path_buf(),
        });
    }
    (declarative, effective_override)
}

async fn run_root<W: Write>(
    terminal: &mut Terminal<&mut W>,
    initialized: InitializedTui,
    startup_action: StartupTuiAction,
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
    let mut model =
        root_program::BcodeRuntimeModel::new(initialized.chat, initialized.settings, loop_state);
    if let StartupTuiAction::OpenRalphHome { repo_path } = startup_action {
        let surface = super::ralph_launcher::open_root_ralph_home_surface(repo_path, None).await?;
        model.queue_plugin_surface("bcode.ralph", surface);
    }
    let (runtime, handle) = root_program::runtime(terminal, model);
    let _model = Box::pin(root_program::run(runtime, handle)).await?;
    Ok(())
}
