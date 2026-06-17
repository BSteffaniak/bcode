//! TUI startup flow.

use std::io::Write;

use bcode_client::BcodeClient;
use bcode_session_models::SessionId;
use bmux_tui::terminal::Terminal;
use tokio::sync::mpsc;

use super::app::BmuxApp;
use super::keymap::BmuxKeyMap;
use super::render::TuiTheme;
use super::runtime_context::{TuiIo, TuiServices};
use super::startup_action::StartupTuiAction;
use super::terminal_events::TuiInput;
use super::{TuiError, chat_loop, ralph_flow, session_flow};

fn auth_security_status(config: &bcode_config::BcodeConfig) -> Option<String> {
    let selection = config.resolved_model_selection();
    let auth_profile_name = std::env::var(bcode_config::BCODE_AUTH_PROFILE_ENV)
        .ok()
        .filter(|profile| !profile.trim().is_empty())
        .or(selection.auth_profile)?;
    let auth_profile = config.auth.profiles.get(&auth_profile_name)?;
    if auth_profile.backend != "sshenv" {
        return None;
    }
    let vault = auth_profile.settings.get("vault").map_or_else(
        bcode_config::default_auth_vault_path,
        std::path::PathBuf::from,
    );
    let profile = auth_profile
        .settings
        .get("profile")
        .map_or(auth_profile_name.as_str(), String::as_str);
    let policy = bcode_provider_auth::security::device_seal_policy_for_auth_profile(auth_profile);
    let report = bcode_provider_auth::security::reconcile_auth_vault_security_report(
        &vault,
        profile,
        policy,
        auth_profile
            .settings
            .get("recipient_key")
            .map(String::as_str),
    );
    report
        .diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic.severity
                == bcode_provider_auth::security::AuthSecurityDiagnosticSeverity::Error
        })
        .or_else(|| {
            report.diagnostics.iter().find(|diagnostic| {
                diagnostic.severity
                    == bcode_provider_auth::security::AuthSecurityDiagnosticSeverity::Warning
            })
        })
        .map(|diagnostic| format!("⚠ {} Run `bcode auth status`.", diagnostic.message))
}

/// Attach to a session and run the active chat loop.
pub async fn run_event_loop<W: Write>(
    terminal: &mut Terminal<&mut W>,
    session_id: Option<SessionId>,
) -> Result<(), TuiError> {
    run_event_loop_with_startup(terminal, session_id, StartupTuiAction::None).await
}

/// Attach to a session, run an optional startup action, and run the active chat loop.
pub async fn run_event_loop_with_startup<W: Write>(
    terminal: &mut Terminal<&mut W>,
    session_id: Option<SessionId>,
    startup_action: StartupTuiAction,
) -> Result<(), TuiError> {
    let client = BcodeClient::default_endpoint();
    let config = bcode_config::load_config()?;
    let auth_security_status = auth_security_status(&config);
    let keymap = BmuxKeyMap::from_config(&config.tui);
    let mouse_scroll_rows = config.tui.mouse.effective_scroll_rows();
    let mut terminal_events = TuiInput::start();
    let (event_sender, event_receiver) = mpsc::unbounded_channel();
    let (async_event_sender, async_event_receiver) = mpsc::unbounded_channel();
    let mut app = BmuxApp::new_with_history(session_id, &[], &[], false);
    app.apply_tui_config(config.tui);
    let agents = session_flow::AgentCatalog::load(&client).await?;
    agents.refresh_app_agent_metadata(&mut app);
    let mut chat = session_flow::ActiveChat {
        app,
        agents,
        session_id: None,
        event_sender,
        event_receiver,
        event_task: None,
        async_event_sender,
        async_event_receiver,
        session_open_task: None,
        status_hydration_task: None,
        opening_session_id: None,
    };
    if let Some(session_id) = session_id {
        let initial_window_request = session_flow::initial_transcript_window_request(
            super::render::transcript_area_for_frame(&chat.app, terminal.area()),
        );
        session_flow::start_switch_session(&client, &mut chat, session_id, initial_window_request);
    } else if let Some(status) = auth_security_status {
        chat.app.set_status(status);
    } else {
        chat.app
            .set_status("New draft session; send a message to save it".to_owned());
    }
    if startup_action == StartupTuiAction::OpenRalphHome {
        let mut io = TuiIo {
            terminal,
            input: &mut terminal_events,
        };
        let services = TuiServices {
            client: &client,
            keymap: &keymap,
            theme: TuiTheme::for_app(&chat.app),
        };
        ralph_flow::open_home(&mut io, &services, &mut chat).await?;
    }
    let result = {
        chat_loop::run_with_client(
            terminal,
            &mut terminal_events,
            &client,
            &keymap,
            &mut chat,
            mouse_scroll_rows,
        )
        .await
    };
    if let Some(event_task) = chat.event_task.take() {
        event_task.abort();
    }
    if let Some(open_task) = chat.session_open_task.take() {
        open_task.abort();
    }
    if let Some(hydration_task) = chat.status_hydration_task.take() {
        hydration_task.abort();
    }
    result
}
