//! Root-owned model picker input semantics.

use bcode_plugin_sdk::path::display_from_current_dir;
use bmux_keyboard::{KeyCode, KeyStroke};

use super::keymap::BmuxKeyMap;
use super::{model_picker, text_input_flow};

pub enum ModelPickerAction {
    Continue,
    Cancel,
    Select(String),
}

pub fn handle_model_picker_key(
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
                picker.set_status(format!(
                    "Ignored {model_id} in state ({})",
                    display_from_current_dir(&path)
                ));
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
                    display_from_current_dir(&path)
                ));
            }
            Err(error) => picker.set_status(format!("Failed to unignore {model_id}: {error}")),
        }
    }
}
