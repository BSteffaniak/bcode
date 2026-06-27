//! Model/provider picker flow for the TUI.

use std::io::Write;

use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::{Event, FocusEvent};
use bmux_tui::geometry::Rect;

use super::effects::TuiEffect;
use super::helpers;
use super::keymap::BmuxKeyMap;
use super::picker_mouse::picker_row_from_mouse;
use super::runtime_context::{TuiIo, TuiServices};
use super::{
    TuiError, model_picker, model_picker_render, provider_picker, provider_picker_render,
    session_flow::ActiveChat, text_input_flow,
};

enum ModelProviderPick {
    Selected(Option<String>),
    Canceled,
}

/// Pick and set the active model for the current or next session.
#[allow(clippy::too_many_lines)]
pub async fn pick_model_for_session<W: Write>(
    io: &mut TuiIo<'_, '_, W>,
    services: &TuiServices<'_>,
    chat: &mut ActiveChat,
) -> Result<(), TuiError> {
    let session_id = chat.app.session_id();
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
                picker.focus_filter();
                let _ = text_input_flow::handle_paste(picker.filter_mut(), &text);
                picker.refresh_filter();
            }
            Event::Key(stroke) => match handle_model_picker_key(
                &mut picker,
                services.keymap,
                provider_plugin_id.as_deref(),
                stroke,
            ) {
                ModelPickerAction::Continue => {}
                ModelPickerAction::Cancel => return Ok(()),
                ModelPickerAction::Select(model_id) => {
                    if picker.selected_ignored_model_id().is_some() {
                        picker.set_status(format!(
                            "{model_id} is ignored; press u to remove state ignore or I to hide ignored models"
                        ));
                        continue;
                    }
                    apply_model_selection(chat, session_id, provider_plugin_id.clone(), model_id);
                    return Ok(());
                }
            },
            Event::Mouse(mouse) => {
                if let Some(row) = picker_row_from_mouse(mouse)
                    && picker.select_visible(row)
                    && let Some(model_id) = picker.selected_model_id()
                {
                    if picker.selected_ignored_model_id().is_some() {
                        picker.set_status(format!(
                            "{model_id} is ignored; press u to remove state ignore or I to hide ignored models"
                        ));
                        continue;
                    }
                    apply_model_selection(chat, session_id, provider_plugin_id.clone(), model_id);
                    return Ok(());
                }
            }
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) | Event::Tick | Event::User(_) => {}
        }
    }
}

enum ModelPickerAction {
    Continue,
    Cancel,
    Select(String),
}

fn handle_model_picker_key(
    picker: &mut model_picker::ModelPickerApp,
    keymap: &BmuxKeyMap,
    provider_plugin_id: Option<&str>,
    stroke: KeyStroke,
) -> ModelPickerAction {
    match picker.mode() {
        model_picker::ModelPickerMode::Actions => {
            handle_model_picker_action_key(picker, provider_plugin_id, stroke)
        }
        model_picker::ModelPickerMode::Filter => {
            handle_model_picker_filter_key(picker, keymap, stroke)
        }
    }
}

const fn action_shortcut_allows_shift(stroke: KeyStroke) -> bool {
    !stroke.modifiers.ctrl
        && !stroke.modifiers.alt
        && !stroke.modifiers.super_key
        && !stroke.modifiers.hyper
        && !stroke.modifiers.meta
}

fn handle_model_picker_action_key(
    picker: &mut model_picker::ModelPickerApp,
    provider_plugin_id: Option<&str>,
    stroke: KeyStroke,
) -> ModelPickerAction {
    match stroke.key {
        KeyCode::Escape => ModelPickerAction::Cancel,
        KeyCode::Char('/') if stroke.modifiers.is_empty() => {
            picker.focus_filter();
            ModelPickerAction::Continue
        }
        KeyCode::Char('I') if action_shortcut_allows_shift(stroke) => {
            picker.toggle_show_ignored();
            ModelPickerAction::Continue
        }
        KeyCode::Char('s') if stroke.modifiers.is_empty() => {
            picker.cycle_sort_key();
            ModelPickerAction::Continue
        }
        KeyCode::Char('S') if action_shortcut_allows_shift(stroke) => {
            picker.reverse_sort_direction();
            ModelPickerAction::Continue
        }
        KeyCode::Char('i') if stroke.modifiers.is_empty() => {
            ignore_selected_model(picker, provider_plugin_id);
            ModelPickerAction::Continue
        }
        KeyCode::Char('u') if stroke.modifiers.is_empty() => {
            unignore_selected_model(picker, provider_plugin_id);
            ModelPickerAction::Continue
        }
        KeyCode::Enter => picker
            .selected_model_id()
            .map_or(ModelPickerAction::Continue, ModelPickerAction::Select),
        KeyCode::Up if stroke.modifiers.is_empty() => {
            picker.select_previous();
            ModelPickerAction::Continue
        }
        KeyCode::Down if stroke.modifiers.is_empty() => {
            picker.select_next();
            ModelPickerAction::Continue
        }
        _ => ModelPickerAction::Continue,
    }
}

fn handle_model_picker_filter_key(
    picker: &mut model_picker::ModelPickerApp,
    keymap: &BmuxKeyMap,
    stroke: KeyStroke,
) -> ModelPickerAction {
    match stroke.key {
        KeyCode::Escape => {
            picker.focus_actions();
            ModelPickerAction::Continue
        }
        KeyCode::Enter => picker
            .selected_model_id()
            .map_or(ModelPickerAction::Continue, ModelPickerAction::Select),
        KeyCode::Up if stroke.modifiers.is_empty() => {
            picker.select_previous();
            ModelPickerAction::Continue
        }
        KeyCode::Down if stroke.modifiers.is_empty() => {
            picker.select_next();
            ModelPickerAction::Continue
        }
        _ => {
            if text_input_flow::handle_key(picker.filter_mut(), keymap, stroke)
                != bmux_tui_components::text_input::TextInputOutcome::Ignored
            {
                picker.refresh_filter();
            }
            ModelPickerAction::Continue
        }
    }
}

fn ignore_selected_model(
    picker: &mut model_picker::ModelPickerApp,
    provider_plugin_id: Option<&str>,
) {
    if let Some(model_id) = picker.selected_model_id() {
        let provider = provider_plugin_id.unwrap_or("bcode.openai-compatible");
        match bcode_config::ignore_model_in_state(provider, model_id.clone()) {
            Ok(path) => {
                picker.mark_state_ignored(&model_id);
                picker.set_status(format!("Ignored {model_id} in state ({})", path.display()));
            }
            Err(error) => picker.set_status(format!("Failed to ignore {model_id}: {error}")),
        }
    }
}

fn unignore_selected_model(
    picker: &mut model_picker::ModelPickerApp,
    provider_plugin_id: Option<&str>,
) {
    if let Some(model_id) = picker.selected_ignored_model_id() {
        let provider = provider_plugin_id.unwrap_or("bcode.openai-compatible");
        match bcode_config::unignore_model_in_state(provider, &model_id) {
            Ok(path) => {
                picker.mark_state_unignored(&model_id);
                picker.set_status(format!(
                    "Removed state ignore for {model_id} ({})",
                    path.display()
                ));
            }
            Err(error) => picker.set_status(format!("Failed to unignore {model_id}: {error}")),
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

fn apply_model_selection(
    chat: &mut ActiveChat,
    session_id: Option<bcode_session_models::SessionId>,
    provider_plugin_id: Option<String>,
    model_id: String,
) {
    if let Some(session_id) = session_id {
        chat.start_effect(TuiEffect::SetSessionModel {
            session_id,
            provider_plugin_id,
            model_id,
        });
        chat.app.set_status("applying model…".to_owned());
    } else {
        chat.app
            .apply_local_model_selection(provider_plugin_id, &model_id);
    }
}

#[cfg(test)]
mod tests {
    use bmux_keyboard::{KeyCode, KeyStroke};

    use super::*;

    fn model(model_id: &str) -> bcode_model::ModelInfo {
        bcode_model::ModelInfo {
            model_id: model_id.to_string(),
            display_name: model_id.to_string(),
            is_default: false,
            context_window: None,
            max_output_tokens: None,
            capabilities: std::collections::BTreeSet::new(),
            reasoning: None,
            cache: bcode_model::ModelCacheInfo::default(),
            metadata_source: None,
            pricing: None,
            visibility: bcode_model::ModelVisibility::Visible,
        }
    }

    fn keymap() -> BmuxKeyMap {
        BmuxKeyMap::from_config(&bcode_config::TuiConfig::default())
    }

    #[test]
    fn slash_focuses_model_filter() {
        let mut picker =
            model_picker::ModelPickerApp::new_with_status(vec![model("gpt-5")], "Select");
        let action = handle_model_picker_key(
            &mut picker,
            &keymap(),
            None,
            KeyStroke::simple(KeyCode::Char('/')),
        );

        assert!(matches!(action, ModelPickerAction::Continue));
        assert_eq!(picker.mode(), model_picker::ModelPickerMode::Filter);
        assert_eq!(picker.filter_mut().buffer().text(), "");
    }

    #[test]
    fn plain_sort_key_filters_while_model_filter_is_focused() {
        let mut picker =
            model_picker::ModelPickerApp::new_with_status(vec![model("gpt-5")], "Select");
        picker.focus_filter();
        let action = handle_model_picker_key(
            &mut picker,
            &keymap(),
            None,
            KeyStroke::simple(KeyCode::Char('s')),
        );

        assert!(matches!(action, ModelPickerAction::Continue));
        assert_eq!(picker.filter_mut().buffer().text(), "s");
        assert_eq!(picker.mode(), model_picker::ModelPickerMode::Filter);
    }

    #[test]
    fn escape_exits_model_filter_before_canceling_picker() {
        let mut picker =
            model_picker::ModelPickerApp::new_with_status(vec![model("gpt-5")], "Select");
        picker.focus_filter();
        let first_escape = handle_model_picker_key(
            &mut picker,
            &keymap(),
            None,
            KeyStroke::simple(KeyCode::Escape),
        );
        let second_escape = handle_model_picker_key(
            &mut picker,
            &keymap(),
            None,
            KeyStroke::simple(KeyCode::Escape),
        );

        assert!(matches!(first_escape, ModelPickerAction::Continue));
        assert!(matches!(second_escape, ModelPickerAction::Cancel));
    }
}
