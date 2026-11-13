//! Root-owned model picker input semantics.

use bcode_plugin_sdk::path::display_from_current_dir;
use bmux_keyboard::KeyStroke;

use super::keymap::BmuxKeyMap;
use super::{model_picker, text_input_flow};

pub enum ModelPickerAction {
    Continue,
    Cancel,
    Select(String),
    /// Select a model together with an approved external credential connection.
    SelectWithConnection {
        model_id: String,
        binding_id: String,
    },
    /// Approve a connection (first time, or again after an account change), then reload.
    Approve(Box<bcode_provider_auth_models::external::ExternalAuthCandidate>),
    Check(String),
}

/// Submit the highlighted row, honoring connection rows.
pub fn submit(picker: &mut model_picker::ModelPickerApp) -> ModelPickerAction {
    match picker.selected_connection() {
        Some(model_picker::ConnectionChoice::Use {
            model_id,
            binding_id,
        }) => ModelPickerAction::SelectWithConnection {
            model_id,
            binding_id,
        },
        Some(model_picker::ConnectionChoice::Approve(candidate)) => {
            ModelPickerAction::Approve(candidate)
        }
        Some(model_picker::ConnectionChoice::Connect(candidate)) => {
            if picker.confirm_connect() {
                ModelPickerAction::Approve(candidate)
            } else {
                ModelPickerAction::Continue
            }
        }
        Some(model_picker::ConnectionChoice::Unavailable(message)) => {
            picker.set_status(message);
            ModelPickerAction::Continue
        }
        None => picker
            .selected_model_id()
            .map_or(ModelPickerAction::Continue, ModelPickerAction::Select),
    }
}

pub fn handle_model_picker_key(
    picker: &mut model_picker::ModelPickerApp,
    keymap: &BmuxKeyMap,
    provider_plugin_id: Option<&str>,
    stroke: KeyStroke,
) -> ModelPickerAction {
    if picker.selected_connection().is_some()
        && matches!(
            keymap.action("model_picker_actions", stroke),
            Some("tui.model.check" | "tui.model.ignore" | "tui.model.unignore")
        )
    {
        picker.set_status(
            "Connection rows use the provider's recommended models; check and ignore apply to the provider's own rows",
        );
        return ModelPickerAction::Continue;
    }
    if picker.is_execution_picker()
        && matches!(
            keymap.action("model_picker_actions", stroke),
            Some("tui.model.check" | "tui.model.ignore" | "tui.model.unignore")
        )
    {
        picker.set_status("Execution discovery does not authorize provider checks or provider-specific ignore changes");
        return ModelPickerAction::Continue;
    }
    match picker.mode() {
        model_picker::ModelPickerMode::ConfirmCheck => {
            match keymap.action("model_picker_actions", stroke) {
                Some("tui.control.submit") => picker
                    .confirm_check()
                    .map_or(ModelPickerAction::Continue, ModelPickerAction::Check),
                Some("tui.control.cancel") => {
                    picker.decline_check();
                    ModelPickerAction::Continue
                }
                _ => ModelPickerAction::Continue,
            }
        }
        model_picker::ModelPickerMode::Checking => {
            // Closing does not claim that the server-side request was cancelled.
            if keymap.action("model_picker_actions", stroke) == Some("tui.control.cancel") {
                ModelPickerAction::Cancel
            } else {
                ModelPickerAction::Continue
            }
        }
        model_picker::ModelPickerMode::Actions => {
            handle_model_picker_action_key(picker, keymap, provider_plugin_id, stroke)
        }
        model_picker::ModelPickerMode::Filter => {
            handle_model_picker_filter_key(picker, keymap, stroke)
        }
    }
}

fn handle_model_picker_action_key(
    picker: &mut model_picker::ModelPickerApp,
    keymap: &BmuxKeyMap,
    provider_plugin_id: Option<&str>,
    stroke: KeyStroke,
) -> ModelPickerAction {
    match keymap.action("model_picker_actions", stroke) {
        Some("tui.control.cancel") => ModelPickerAction::Cancel,
        Some("tui.model.check") => {
            picker.request_check();
            ModelPickerAction::Continue
        }
        Some("tui.model.filter") => {
            picker.focus_filter();
            ModelPickerAction::Continue
        }
        Some("tui.model.showIgnored") => {
            picker.toggle_show_ignored();
            ModelPickerAction::Continue
        }
        Some("tui.model.sort") => {
            picker.cycle_sort_key();
            ModelPickerAction::Continue
        }
        Some("tui.model.reverseSort") => {
            picker.reverse_sort_direction();
            ModelPickerAction::Continue
        }
        Some("tui.model.ignore") => {
            ignore_selected_model(picker, provider_plugin_id);
            ModelPickerAction::Continue
        }
        Some("tui.model.unignore") => {
            unignore_selected_model(picker, provider_plugin_id);
            ModelPickerAction::Continue
        }
        Some("tui.control.submit") => submit(picker),
        Some("tui.control.up") => {
            picker.select_previous();
            ModelPickerAction::Continue
        }
        Some("tui.control.down") => {
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
    match keymap.action("model_picker", stroke) {
        Some("tui.control.cancel") => {
            picker.focus_actions();
            ModelPickerAction::Continue
        }
        Some("tui.control.submit") => submit(picker),
        Some("tui.control.up") => {
            picker.select_previous();
            ModelPickerAction::Continue
        }
        Some("tui.control.down") => {
            picker.select_next();
            ModelPickerAction::Continue
        }
        _ => {
            if text_input_flow::handle_key_in_scope(
                picker.filter_mut(),
                keymap,
                "model_picker",
                stroke,
            ) != bmux_tui_components::text_input::TextInputOutcome::Ignored
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

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_provider_auth_models::external::ExternalConnectionStatus;
    use model_picker::connection_tests::{connection, picker};

    #[test]
    fn connecting_discovered_credentials_needs_a_second_enter() {
        let mut app = picker(&[]);
        app.add_unconnected(vec![
            bcode_provider_auth_models::external::ExternalAuthCandidate {
                binding_id: "binding-new".into(),
                target: bcode_provider_auth_models::external::ExternalAuthTarget {
                    plugin_id: "provider.plugin".into(),
                    provider_id: "provider".into(),
                    method_id: "chatgpt".into(),
                    source_id: "codex-chatgpt".into(),
                },
                application: "Codex".into(),
                available: true,
            },
        ]);
        assert!(matches!(submit(&mut app), ModelPickerAction::Continue));
        assert!(app.status().contains("Press Enter again"));
        assert!(matches!(
            submit(&mut app),
            ModelPickerAction::Approve(candidate) if candidate.binding_id == "binding-new"
        ));
    }

    #[test]
    fn submitting_a_connection_row_selects_with_its_binding_or_explains_the_fix() {
        let mut app = picker(&[]);
        app.add_connections(vec![connection(
            "binding-ready",
            ExternalConnectionStatus::Ready {
                expires_at_unix: None,
            },
        )]);
        assert!(matches!(
            submit(&mut app),
            ModelPickerAction::SelectWithConnection { binding_id, .. } if binding_id == "binding-ready"
        ));
        let mut app = picker(&[]);
        app.add_connections(vec![connection(
            "binding-missing",
            ExternalConnectionStatus::SourceUnavailable,
        )]);
        assert!(matches!(submit(&mut app), ModelPickerAction::Continue));
        assert!(app.status().contains("sign in to Codex"));
    }
}
