//! TUI startup flow.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use bcode_client::BcodeClient;
use bcode_session_models::SessionId;
use bmux_tui::terminal::Terminal;
use tokio::sync::mpsc;

use super::app::BmuxApp;
use super::effects::{TuiEffect, TuiEffectQueue};
use super::startup_action::StartupTuiAction;
use super::terminal_events::TuiInput;
use super::{TuiError, chat_loop, session_flow};

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
) -> Result<(), TuiError> {
    Box::pin(run_event_loop_with_startup_and_static_bundled(
        terminal,
        session_id,
        StartupTuiAction::None,
        static_plugins,
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
    ))
    .await
}

/// Attach to a session and run the native viewer using a caller-owned input stream.
#[allow(clippy::future_not_send)]
pub async fn run_event_loop_with_input<W: Write>(
    terminal: &mut Terminal<&mut W>,
    terminal_events: &mut TuiInput,
    session_id: SessionId,
) -> Result<(), TuiError> {
    Box::pin(run_event_loop_with_input_and_static_bundled(
        terminal,
        terminal_events,
        Some(session_id),
        StartupTuiAction::None,
        &super::static_bundled_plugins(),
    ))
    .await
}

/// Attach to a session, run an optional startup action, and run the active chat loop with caller-provided static bundled plugins.
#[allow(clippy::future_not_send)]
pub async fn run_event_loop_with_startup_and_static_bundled<W: Write>(
    terminal: &mut Terminal<&mut W>,
    session_id: Option<SessionId>,
    startup_action: StartupTuiAction,
    static_plugins: &[bcode_plugin::StaticBundledPlugin],
) -> Result<(), TuiError> {
    let mut terminal_events = TuiInput::start();
    Box::pin(run_event_loop_with_input_and_static_bundled(
        terminal,
        &mut terminal_events,
        session_id,
        startup_action,
        static_plugins,
    ))
    .await
}

#[allow(clippy::future_not_send)]
async fn run_event_loop_with_input_and_static_bundled<W: Write>(
    terminal: &mut Terminal<&mut W>,
    terminal_events: &mut TuiInput,
    session_id: Option<SessionId>,
    startup_action: StartupTuiAction,
    static_plugins: &[bcode_plugin::StaticBundledPlugin],
) -> Result<(), TuiError> {
    let config = bcode_config::load_config();
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
    let (event_sender, event_receiver) = mpsc::unbounded_channel();
    let mut app = BmuxApp::new_with_history(session_id, &[], &[], false);
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
        chat_loop::TuiRuntimeSettings::bootstrap(launch_working_directory.clone(), static_plugins);
    if let Ok(config) = &config {
        settings.set_metrics_enabled(config.metrics.enabled);
    }
    let mut chat = session_flow::ActiveChat {
        app,
        agents,
        session_id: None,
        event_sender,
        event_receiver,
        event_task: None,
        opening_session_id: None,
        opening_session_progress: None,
        opening_session_anchor_sequence: None,
        pending_effects: TuiEffectQueue::default(),
    };
    match config {
        Ok(config) => {
            settings.apply_tui_config(&config.tui);
            chat.app.apply_tui_config(config.tui.clone());
            chat.start_effect(TuiEffect::ReconcileAuthSecurity {
                config: Box::new(config),
            });
            if session_id.is_none() {
                chat.start_effect(TuiEffect::LoadDraftStatus {
                    launch_working_directory: launch_working_directory.clone(),
                });
            }
        }
        Err(_) => chat.start_effect(TuiEffect::LoadConfig),
    }
    chat.start_effect(TuiEffect::LoadAgentCatalog);
    if let Some(session_id) = session_id {
        let initial_window_request = session_flow::initial_transcript_window_request(
            super::render::transcript_area_for_frame(&chat.app, terminal.area()),
        );
        session_flow::start_switch_session(&mut chat, session_id, initial_window_request);
    } else {
        chat.app.set_status("New draft".to_owned());
    }
    let result = {
        Box::pin(chat_loop::run_with_client(
            terminal,
            terminal_events,
            &client,
            &mut settings,
            &mut chat,
            startup_action,
        ))
        .await
    };
    if let Some(event_task) = chat.event_task.take() {
        event_task.abort();
    }
    result
}
