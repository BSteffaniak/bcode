//! Model/provider picker flow for the TUI.

use std::io::Write;

use bcode_client::BcodeClient;
use bmux_keyboard::KeyCode;
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;

use super::effects::TuiEffect;
use super::helpers;
use super::picker_mouse::picker_row_from_mouse;
use super::runtime_context::{TuiIo, TuiServices};
use super::{
    TuiError, model_picker, model_picker_render, provider_picker, provider_picker_render,
    session_flow::ActiveChat, text_input_flow,
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
    let status = match client.session_model_status(session_id).await {
        Ok(status) => status,
        Err(error) => {
            helpers::report_client_issue(&mut chat.app, "model status unavailable", &error);
            return Ok(());
        }
    };
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
    let status = match client.server_status().await {
        Ok(status) => status,
        Err(error) => {
            helpers::report_client_issue(&mut chat.app, "server model status unavailable", &error);
            return Ok(());
        }
    };
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
    let status = match client.server_status().await {
        Ok(status) => status,
        Err(error) => {
            helpers::report_client_issue(&mut chat.app, "runtime status unavailable", &error);
            return Ok(());
        }
    };
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

enum ModelProviderPick {
    Selected(Option<String>),
    Canceled,
}

/// Pick and set the active model for the current session.
pub async fn pick_model_for_session<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let Some(session_id) = chat.app.session_id() else {
        chat.app.set_status("No active session".to_owned());
        return Ok(());
    };
    let provider_plugin_id = match pick_model_provider(io, services, chat).await? {
        Some(ModelProviderPick::Selected(provider)) => provider,
        Some(ModelProviderPick::Canceled) | None => return Ok(()),
    };
    let models = match services
        .passive_client
        .session_model_list(provider_plugin_id.clone())
        .await
    {
        Ok(list) => list.models,
        Err(error) => {
            helpers::report_client_issue(&mut chat.app, "model list unavailable", &error);
            return Ok(());
        }
    };
    let status = provider_plugin_id.as_ref().map_or_else(
        || "Select a model".to_owned(),
        |provider| format!("Select a model from {provider}"),
    );
    let mut picker = model_picker::ModelPickerApp::new_with_status(models, status);
    loop {
        io.terminal.resize(helpers::terminal_area()?);
        io.terminal.draw(|frame| {
            model_picker_render::render_model_picker(&mut picker, frame, services.theme);
        })?;
        let Some(event) = io.input.recv().await? else {
            continue;
        };
        match event {
            Event::Resize(size) => io.terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Paste(text) => {
                let _ = text_input_flow::handle_paste(picker.filter_mut(), &text);
                picker.refresh_filter();
            }
            Event::Key(stroke) => match stroke.key {
                KeyCode::Escape => return Ok(()),
                KeyCode::Enter => {
                    if let Some(model_id) = picker.selected_model_id() {
                        start_set_session_model(
                            chat,
                            session_id,
                            provider_plugin_id.clone(),
                            model_id,
                        );
                        return Ok(());
                    }
                }
                KeyCode::Up => picker.select_previous(),
                KeyCode::Down => picker.select_next(),
                _ => {
                    if text_input_flow::handle_key(picker.filter_mut(), services.keymap, stroke)
                        != bmux_tui_components::text_input::TextInputOutcome::Ignored
                    {
                        picker.refresh_filter();
                    }
                }
            },
            Event::Mouse(mouse) => {
                if let Some(row) = picker_row_from_mouse(mouse)
                    && picker.select_visible(row)
                    && let Some(model_id) = picker.selected_model_id()
                {
                    start_set_session_model(chat, session_id, provider_plugin_id.clone(), model_id);
                    return Ok(());
                }
            }
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::User(_) => {}
        }
    }
}

async fn pick_model_provider<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<Option<ModelProviderPick>, TuiError> {
    let providers = match services.passive_client.plugin_services().await {
        Ok(services) => services
            .into_iter()
            .filter(|service| service.interface_id == bcode_model::MODEL_PROVIDER_INTERFACE_ID)
            .collect::<Vec<_>>(),
        Err(error) => {
            helpers::report_client_issue(&mut chat.app, "model providers unavailable", &error);
            return Ok(None);
        }
    };
    if providers.len() <= 1 {
        return Ok(Some(ModelProviderPick::Selected(
            providers.first().map(|provider| provider.plugin_id.clone()),
        )));
    }
    let mut picker = provider_picker::ProviderPickerApp::new(providers);
    loop {
        io.terminal.resize(helpers::terminal_area()?);
        io.terminal.draw(|frame| {
            provider_picker_render::render_provider_picker(&mut picker, frame, services.theme);
        })?;
        let Some(event) = io.input.recv().await? else {
            continue;
        };
        match event {
            Event::Resize(size) => io.terminal.resize(Rect::new(0, 0, size.width, size.height)),
            Event::Paste(text) => {
                let _ = text_input_flow::handle_paste(picker.filter_mut(), &text);
                picker.refresh_filter();
            }
            Event::Key(stroke) => match stroke.key {
                KeyCode::Escape => return Ok(Some(ModelProviderPick::Canceled)),
                KeyCode::Enter => {
                    return Ok(Some(ModelProviderPick::Selected(
                        picker.selected_provider_id(),
                    )));
                }
                KeyCode::Up => picker.select_previous(),
                KeyCode::Down => picker.select_next(),
                _ => {
                    if text_input_flow::handle_key(picker.filter_mut(), services.keymap, stroke)
                        != bmux_tui_components::text_input::TextInputOutcome::Ignored
                    {
                        picker.refresh_filter();
                    }
                }
            },
            Event::Mouse(mouse) => {
                if let Some(row) = picker_row_from_mouse(mouse)
                    && picker.select_visible(row)
                {
                    return Ok(Some(ModelProviderPick::Selected(
                        picker.selected_provider_id(),
                    )));
                }
            }
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::User(_) => {}
        }
    }
}

fn start_set_session_model(
    chat: &mut ActiveChat,
    session_id: bcode_session_models::SessionId,
    provider_plugin_id: Option<String>,
    model_id: String,
) {
    chat.start_effect(TuiEffect::SetSessionModel {
        session_id,
        provider_plugin_id,
        model_id,
    });
    chat.app.set_status("applying model…".to_owned());
}
