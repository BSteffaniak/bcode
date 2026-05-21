//! Model/provider picker flow for the BMUX backend.

use std::io::Write;

use bcode_client::BcodeClient;
use bmux_keyboard::KeyCode;
use bmux_tui::crossterm::poll_event;
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;
use bmux_tui::input::{TextInputEnterBehavior, TextInputKeyOutcome};
use bmux_tui::terminal::Terminal;

use super::keymap::BmuxKeyMap;
use super::picker_mouse::picker_row_from_mouse;
use super::{
    ActiveChat, EVENT_POLL_TIMEOUT, TuiError, handle_text_buffer_key, model_picker,
    model_picker_render, provider_picker, provider_picker_render, report_client_error,
    terminal_area,
};

/// Show active model status in the transcript.
pub(super) async fn show_model_status(
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
    if let Some(info) = status.model {
        lines.push(format!("Display name: {}", info.display_name));
        if let Some(context_window) = info.context_window {
            lines.push(format!("Context window: {context_window}"));
        }
        if let Some(max_output_tokens) = info.max_output_tokens {
            lines.push(format!("Max output tokens: {max_output_tokens}"));
        }
        if !info.capabilities.is_empty() {
            lines.push(format!("Capabilities: {:?}", info.capabilities));
        }
    }
    let text = lines.join("\n");
    chat.app.set_status(format!("model: {provider}/{model}"));
    chat.app.push_system_note(text);
    Ok(())
}

/// Show server default model status in the transcript.
pub(super) async fn show_server_model_status(
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

/// Pick and set the active model for the current session.
pub(super) async fn pick_model_for_session<W: Write>(
    terminal: &mut Terminal<&mut W>,
    client: &BcodeClient,
    chat: &mut ActiveChat,
    keymap: &BmuxKeyMap,
) -> Result<(), TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return Ok(());
    };
    let provider_plugin_id = pick_model_provider(terminal, client, keymap).await?;
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
        terminal.resize(terminal_area()?);
        terminal.draw(|frame| model_picker_render::render_model_picker(&mut picker, frame))?;
        let Some(event) = poll_event(EVENT_POLL_TIMEOUT)? else {
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
                    let outcome = handle_text_buffer_key(
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
        terminal.resize(terminal_area()?);
        terminal
            .draw(|frame| provider_picker_render::render_provider_picker(&mut picker, frame))?;
        let Some(event) = poll_event(EVENT_POLL_TIMEOUT)? else {
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
                    let outcome = handle_text_buffer_key(
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
        report_client_error(&mut chat.app, "model selection failed", &error.into());
    } else {
        chat.app.set_status(provider_plugin_id.as_ref().map_or_else(
            || format!("model set to {model_id}"),
            |provider| format!("model set to {provider}/{model_id}"),
        ));
    }
}
