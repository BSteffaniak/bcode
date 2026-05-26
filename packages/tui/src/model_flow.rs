//! Model/provider picker flow for the TUI.

use std::io::Write;

use bcode_client::BcodeClient;
use bmux_keyboard::KeyCode;
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;
use bmux_tui::input::{TextInputEnterBehavior, TextInputKeyOutcome};
use bmux_tui::terminal::Terminal;

use super::helpers;
use super::keymap::BmuxKeyMap;
use super::picker_mouse::picker_row_from_mouse;
use super::terminal_events::TerminalEventStream;
use super::{
    TuiError, model_picker, model_picker_render, provider_picker, provider_picker_render,
    session_flow::ActiveChat,
};

/// Show active model status in the transcript.
pub async fn show_model_status(
    client: &BcodeClient,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return Ok(());
    };
    let status = client.session_model_status(session_id).await?;
    let provider = status
        .provider_plugin_id
        .as_deref()
        .unwrap_or("default provider");
    let model = status.model_id.as_deref().unwrap_or("default model");
    let mut lines = vec![format!("Active model: {provider}/{model}")];
    if let Some(reasoning) = status.reasoning {
        lines.push(format!("Reasoning effort: {:?}", reasoning.effort_values));
        lines.push(format!("Reasoning summary: {:?}", reasoning.summary_values));
    }
    let text = lines.join("\n");
    chat.app.set_status(format!("model: {provider}/{model}"));
    chat.app.push_system_note(text);
    Ok(())
}

/// Show server default model status in the transcript.
pub async fn show_server_model_status(
    client: &BcodeClient,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let status = client.server_status().await?;
    let provider = status
        .selected_provider_plugin_id
        .as_deref()
        .unwrap_or("default provider");
    let model = status
        .selected_model_id
        .as_deref()
        .unwrap_or("default model");
    let text = format!("Server default model: {provider}/{model}");
    chat.app.set_status(text.clone());
    chat.app.push_system_note(text);
    Ok(())
}

/// Show daemon runtime status in the transcript.
pub async fn show_runtime_status(
    client: &BcodeClient,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let status = client.server_status().await?;
    let running = status
        .plugin_runtime
        .iter()
        .map(|plugin| plugin.running)
        .sum::<usize>();
    let queued = status
        .plugin_runtime
        .iter()
        .map(|plugin| plugin.queued)
        .sum::<usize>();
    let tool_queued = status
        .plugin_runtime
        .iter()
        .map(|plugin| plugin.queued_tool_execution)
        .sum::<usize>();
    let mut lines = vec![format!(
        "Runtime: {running} running, {queued} queued ({tool_queued} tool queued)"
    )];
    for plugin in status
        .plugin_runtime
        .iter()
        .filter(|plugin| plugin.running > 0 || plugin.queued > 0)
    {
        lines.push(format!(
            "{}: running {}, queued {}",
            plugin.plugin_id, plugin.running, plugin.queued
        ));
    }
    if lines.len() == 1 {
        lines.push("No active plugin work.".to_string());
    }
    let text = lines.join("\n");
    chat.app
        .set_status(format!("runtime: {running} running, {queued} queued"));
    chat.app.push_system_note(text);
    Ok(())
}

/// Pick and set the active model for the current session.
pub async fn pick_model_for_session<W: Write>(
    terminal: &mut Terminal<&mut W>,
    terminal_events: &mut TerminalEventStream,
    client: &BcodeClient,
    chat: &mut ActiveChat,
    keymap: &BmuxKeyMap,
) -> Result<(), TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return Ok(());
    };
    let provider_plugin_id = pick_model_provider(terminal, terminal_events, client, keymap).await?;
    let models = client
        .session_model_list(provider_plugin_id.clone())
        .await?
        .models;
    let status = provider_plugin_id.as_ref().map_or_else(
        || "Select a model".to_owned(),
        |provider| format!("Select a model from {provider}"),
    );
    let mut picker = model_picker::ModelPickerApp::new_with_status(models, status);
    loop {
        terminal.resize(helpers::terminal_area()?);
        terminal.draw(|frame| model_picker_render::render_model_picker(&mut picker, frame))?;
        let Some(event) = terminal_events.recv().await? else {
            continue;
        };
        match event {
            Event::Resize(size) => terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Paste(text) => {
                picker.filter_mut().insert_str(&text);
                picker.refresh_filter();
            }
            Event::Key(stroke) => match stroke.key {
                KeyCode::Escape => return Ok(()),
                KeyCode::Enter => {
                    if let Some(model_id) = picker.selected_model_id() {
                        set_session_model(
                            client,
                            chat,
                            session_id,
                            provider_plugin_id.as_ref(),
                            model_id,
                        )
                        .await;
                        return Ok(());
                    }
                }
                KeyCode::Up => picker.select_previous(),
                KeyCode::Down => picker.select_next(),
                _ => {
                    let outcome = helpers::handle_text_buffer_key(
                        picker.filter_mut(),
                        keymap,
                        stroke,
                        TextInputEnterBehavior::InsertNewline,
                    );
                    if outcome == TextInputKeyOutcome::Edited {
                        picker.refresh_filter();
                    }
                }
            },
            Event::Mouse(mouse) => {
                if let Some(row) = picker_row_from_mouse(mouse)
                    && picker.select_visible(row)
                    && let Some(model_id) = picker.selected_model_id()
                {
                    set_session_model(
                        client,
                        chat,
                        session_id,
                        provider_plugin_id.as_ref(),
                        model_id,
                    )
                    .await;
                    return Ok(());
                }
            }
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::User(_) => {}
        }
    }
}

async fn pick_model_provider<W: Write>(
    terminal: &mut Terminal<&mut W>,
    terminal_events: &mut TerminalEventStream,
    client: &BcodeClient,
    keymap: &BmuxKeyMap,
) -> Result<Option<String>, TuiError> {
    let providers = client
        .plugin_services()
        .await?
        .into_iter()
        .filter(|service| service.interface_id == bcode_model::MODEL_PROVIDER_INTERFACE_ID)
        .collect::<Vec<_>>();
    if providers.len() <= 1 {
        return Ok(providers.first().map(|provider| provider.plugin_id.clone()));
    }
    let mut picker = provider_picker::ProviderPickerApp::new(providers);
    loop {
        terminal.resize(helpers::terminal_area()?);
        terminal
            .draw(|frame| provider_picker_render::render_provider_picker(&mut picker, frame))?;
        let Some(event) = terminal_events.recv().await? else {
            continue;
        };
        match event {
            Event::Resize(size) => terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Paste(text) => {
                picker.filter_mut().insert_str(&text);
                picker.refresh_filter();
            }
            Event::Key(stroke) => match stroke.key {
                KeyCode::Escape => return Ok(None),
                KeyCode::Enter => return Ok(picker.selected_provider_id()),
                KeyCode::Up => picker.select_previous(),
                KeyCode::Down => picker.select_next(),
                _ => {
                    let outcome = helpers::handle_text_buffer_key(
                        picker.filter_mut(),
                        keymap,
                        stroke,
                        TextInputEnterBehavior::InsertNewline,
                    );
                    if outcome == TextInputKeyOutcome::Edited {
                        picker.refresh_filter();
                    }
                }
            },
            Event::Mouse(mouse) => {
                if let Some(row) = picker_row_from_mouse(mouse)
                    && picker.select_visible(row)
                {
                    return Ok(picker.selected_provider_id());
                }
            }
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::User(_) => {}
        }
    }
}

async fn set_session_model(
    client: &BcodeClient,
    chat: &mut ActiveChat,
    session_id: bcode_session_models::SessionId,
    provider_plugin_id: Option<&String>,
    model_id: String,
) {
    if let Err(error) = client
        .set_session_model(session_id, provider_plugin_id.cloned(), model_id.clone())
        .await
    {
        helpers::report_client_error(&mut chat.app, "model selection failed", &error.into());
    } else {
        chat.app.set_status(provider_plugin_id.as_ref().map_or_else(
            || format!("model set to {model_id}"),
            |provider| format!("model set to {provider}/{model_id}"),
        ));
    }
}
