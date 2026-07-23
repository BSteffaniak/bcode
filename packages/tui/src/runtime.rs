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

/// Attach to a session, run an optional startup action, and run the active chat loop with caller-provided static bundled plugins.
#[allow(clippy::future_not_send)]
pub async fn run_event_loop_with_startup_and_static_bundled<W: Write>(
    terminal: &mut Terminal<&mut W>,
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
        .with_interaction_adapters(bcode_bundled_plugins::interaction_adapters("tui"));
    let daemon_host = super::daemon_host::TuiDaemonHost::new(static_plugins);
    let mut terminal_events = TuiInput::start();
    let (event_sender, event_receiver) = mpsc::unbounded_channel();
    let mut app = BmuxApp::new_with_history(session_id, &[], &[], false);
    match super::plugin_tui::load_default_presentation_with_static_bundled(static_plugins) {
        Ok(presentation) => app.set_plugin_presentation(Arc::new(presentation)),
        Err(error) => app.set_status(format!("plugin presentation unavailable: {error}")),
    }
    let agents = session_flow::AgentCatalog::default();
    agents.refresh_app_agent_metadata(&mut app);
    let launch_working_directory = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let mut settings = chat_loop::TuiRuntimeSettings::bootstrap(launch_working_directory.clone());
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
        chat_loop::run_with_client(
            terminal,
            &mut terminal_events,
            &client,
            &mut settings,
            &mut chat,
            startup_action,
            daemon_host,
        )
        .await
    };
    if let Some(event_task) = chat.event_task.take() {
        event_task.abort();
    }
    result
}
