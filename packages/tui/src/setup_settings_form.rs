//! In-terminal reviewed settings form. No terminal ownership handoff or stdin prompts.

use bmux_keyboard::KeyCode;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::component::{Component, Constraints, LayoutCx};
use bmux_tui::event::Event;
use bmux_tui::geometry::Rect;
use bmux_tui::paint::{LocalRect, PaintCx};
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui_components::text_input::TextInputComponent;
use bmux_tui_components::text_input::{TextInputControl, TextInputPolicy, TextInputState};

use super::theme::PresentedTheme;

/// Form event outcome, adapted at the terminal boundary.
pub enum SettingsFormOutcome {
    /// Keep the current form open.
    Continue,
    /// Close the form without leaving setup.
    Close,
}

/// Non-secret fields and the exact pending edit awaiting explicit confirmation.
pub struct SetupSettingsForm {
    inputs: [TextInputState; 3],
    focused: usize,
    status: String,
    pending: Option<bcode_config::edit::ConfigEdit>,
    model_profiles: Option<Vec<(String, bcode_config::ResolvedModelSelection)>>,
    selected_profile: usize,
    create_context: bool,
    discovered_models: Option<(Option<String>, String, String, Vec<String>)>,
    account_selection: Option<(Box<bcode_config::BcodeConfig>, Vec<String>)>,
    selection_snapshot: Option<Box<bcode_config::BcodeConfig>>,
    contexts: Option<Vec<(String, String)>>,
}

impl SetupSettingsForm {
    /// Open a form with an explicit suggested destination and key.
    #[must_use]
    pub fn new(path: &std::path::Path, key: &str) -> Self {
        Self {
            inputs: [path.to_string_lossy().into_owned(), key.to_owned(), String::new()]
                .map(|value| TextInputState::new(TextEditBuffer::from_text(value))),
            focused: 1,
            status: "Tab changes fields. Enter reviews. Empty value removes the override. Esc returns to setup.".to_owned(),
            pending: None,
            model_profiles: None,
            selected_profile: 0,
            create_context: false,
            contexts: None,
            selection_snapshot: None,
            account_selection: None,
            discovered_models: None,
        }
    }

    /// Retain the effective configuration against which asynchronous choices were obtained.
    pub fn with_selection_snapshot(mut self, config: bcode_config::BcodeConfig) -> Self {
        self.selection_snapshot = Some(Box::new(config));
        self
    }

    /// Review a new empty context without selecting it or copying credentials.
    pub fn create_context(path: &std::path::Path) -> Self {
        let mut form = Self::new(path, "");
        form.create_context = true;
        "Enter a stable context ID and display label. Enter reviews; Esc cancels. No accounts are copied.".clone_into(&mut form.status);
        form
    }

    /// Choose among existing user-defined contexts.
    pub fn contexts(path: &std::path::Path, config: &bcode_config::BcodeConfig) -> Self {
        let mut form = Self::new(path, "contexts/active");
        form.focused = 0;
        form.contexts = Some(
            config
                .contexts
                .iter()
                .flat_map(|contexts| contexts.entries.iter())
                .map(|(id, context)| {
                    (
                        id.clone(),
                        context.label.clone().unwrap_or_else(|| id.clone()),
                    )
                })
                .collect(),
        );
        form.refresh_profile();
        form
    }

    /// Choose a context-local account without reading or changing its credentials.
    pub fn accounts(path: &std::path::Path, config: bcode_config::BcodeConfig) -> Self {
        let mut form = Self::new(path, "context account selection");
        form.focused = 0;
        let names = config
            .active_context
            .as_ref()
            .and_then(|id| config.contexts.as_ref()?.entries.get(id))
            .map(|context| context.auth.profiles.keys().cloned().collect())
            .unwrap_or_default();
        form.account_selection = Some((Box::new(config), names));
        form.refresh_profile();
        form
    }

    /// Present catalog-resolved models for an explicitly selected account.
    pub fn discovered_models(
        path: &std::path::Path,
        context: Option<String>,
        provider: String,
        account: String,
        models: bcode_model::ModelList,
    ) -> Self {
        let mut form = Self::new(path, "model selection");
        form.focused = 0;
        form.discovered_models = Some((
            context,
            provider,
            account,
            models
                .models
                .into_iter()
                .filter(|model| matches!(model.visibility, bcode_model::ModelVisibility::Visible))
                .map(|model| model.model_id)
                .collect(),
        ));
        form.refresh_profile();
        form
    }

    /// Select a configured profile while retaining an explicit, reviewed edit destination.
    pub fn models(path: &std::path::Path, config: &bcode_config::BcodeConfig) -> Self {
        let key = config.active_context.as_ref().map_or_else(
            || "model/profile".to_owned(),
            |context| format!("contexts/entries/{context}/model/profile"),
        );
        let mut form = Self::new(path, &key);
        form.focused = 0;
        form.model_profiles = Some(
            config
                .model
                .profiles
                .keys()
                .filter_map(|name| {
                    config
                        .resolved_model_profile(name)
                        .map(|selection| (name.clone(), selection))
                })
                .collect(),
        );
        form.refresh_profile();
        form
    }

    fn refresh_profile(&mut self) {
        if let Some((config, accounts)) = &self.account_selection {
            if let Some(account) = accounts.get(self.selected_profile) {
                self.inputs[2] = TextInputState::new(TextEditBuffer::from_text(account.clone()));
                self.status = format!(
                    "Context: {}; account: {account}. Up/Down selects; Enter reviews provider/account selection and clearing model/pool overrides.",
                    config.active_context.as_deref().unwrap_or("none")
                );
            } else {
                "No context accounts. Select a context and connect an account first."
                    .clone_into(&mut self.status);
            }
            return;
        }
        if let Some((context, provider, account, models)) = &self.discovered_models {
            if let Some(model) = models.get(self.selected_profile) {
                self.inputs[2] = TextInputState::new(TextEditBuffer::from_text(model.clone()));
                self.status = format!(
                    "Context: {}; provider: {provider}; account: {account}; model: {model}. Up/Down selects; Enter reviews.",
                    context.as_deref().unwrap_or("global")
                );
            } else {
                "No available models returned. Esc returns to setup.".clone_into(&mut self.status);
            }
            return;
        }
        if let Some(contexts) = &self.contexts {
            if let Some((id, label)) = contexts.get(self.selected_profile) {
                self.inputs[2] = TextInputState::new(TextEditBuffer::from_text(
                    toml::Value::String(id.clone()).to_string(),
                ));
                self.status = format!(
                    "{label} ({id}). Up/Down selects; Enter reviews the configuration edit."
                );
            } else {
                "No contexts defined. Esc returns; press N to create one."
                    .clone_into(&mut self.status);
            }
            return;
        }
        let Some(profiles) = &self.model_profiles else {
            return;
        };
        let Some((name, selection)) = profiles.get(self.selected_profile) else {
            "No configured model profiles. Esc returns to setup; use Settings to configure a model.".clone_into(&mut self.status);
            return;
        };
        self.inputs[2] = TextInputState::new(TextEditBuffer::from_text(
            toml::Value::String(name.clone()).to_string(),
        ));
        self.status = format!(
            "{} — provider: {}; model: {}; account: {}; pool: {}. Up/Down selects; Enter reviews.",
            name,
            selection
                .provider_plugin_id
                .as_deref()
                .unwrap_or("not selected"),
            selection.model_id.as_deref().unwrap_or("not selected"),
            selection.auth_profile.as_deref().unwrap_or("default"),
            selection.auth_pool.as_deref().unwrap_or("none"),
        );
    }

    /// Handle form input without changing terminal lifecycle.
    pub fn handle_event(&mut self, event: &Event) -> SettingsFormOutcome {
        let policy = TextInputPolicy::default();
        if self.pending.is_none()
            && let Some(count) = self
                .contexts
                .as_ref()
                .map(Vec::len)
                .or_else(|| {
                    self.account_selection
                        .as_ref()
                        .map(|(_, accounts)| accounts.len())
                })
                .or_else(|| self.model_profiles.as_ref().map(Vec::len))
                .or_else(|| {
                    self.discovered_models
                        .as_ref()
                        .map(|(_, _, _, models)| models.len())
                })
            && let Event::Key(key) = event
            && matches!(key.key, KeyCode::Up | KeyCode::Down)
        {
            if key.key == KeyCode::Up {
                self.selected_profile = self.selected_profile.saturating_sub(1);
            } else {
                self.selected_profile = (self.selected_profile + 1).min(count.saturating_sub(1));
            }
            self.refresh_profile();
            return SettingsFormOutcome::Continue;
        }
        match event {
            Event::Key(key) if key.key == KeyCode::Escape => {
                if self.pending.take().is_none() {
                    return SettingsFormOutcome::Close;
                }
                "Review cancelled; no changes saved.".clone_into(&mut self.status);
            }
            Event::Key(key) if key.key == KeyCode::Enter => self.submit(),
            Event::Key(key) if key.key == KeyCode::Tab && self.pending.is_none() => {
                if self.model_profiles.is_some()
                    || self.account_selection.is_some()
                    || self.contexts.is_some()
                    || self.discovered_models.is_some()
                {
                    self.focused = 0;
                } else {
                    self.focused = (self.focused + 1) % self.inputs.len();
                }
            }
            Event::Key(key) if self.pending.is_none() => {
                let _ =
                    TextInputControl::new(&policy).handle_key(&mut self.inputs[self.focused], *key);
            }
            Event::Paste(text) if self.pending.is_none() => {
                let _ = TextInputControl::new(&policy)
                    .handle_paste(&mut self.inputs[self.focused], text);
            }
            _ => {}
        }
        SettingsFormOutcome::Continue
    }

    fn submit(&mut self) {
        if let Some(expected) = &self.selection_snapshot {
            let current = bcode_config::load_config();
            if !current
                .as_ref()
                .is_ok_and(|current| current == expected.as_ref())
            {
                self.pending = None;
                "Configuration changed since these choices were loaded. Return to setup and reload the picker before saving.".clone_into(&mut self.status);
                return;
            }
        }
        if self.contexts.as_ref().is_some_and(Vec::is_empty) {
            self.refresh_profile();
            return;
        }
        if let Some(profiles) = &self.model_profiles {
            let Some((_, selection)) = profiles.get(self.selected_profile) else {
                self.refresh_profile();
                return;
            };
            if let Err(error) = selection.validate_selection() {
                self.status = error.to_string();
                return;
            }
        }
        if let Some(edit) = self.pending.take() {
            self.status = match edit.apply() {
                Ok(_) => "Saved. Higher-priority overrides may still apply. Esc returns to setup."
                    .to_owned(),
                Err(_) => "Could not save safely. Configuration may have changed; review again."
                    .to_owned(),
            };
            return;
        }
        let path = self.inputs[0].buffer().text();
        let key = self.inputs[1].buffer().text();
        let value = self.inputs[2].buffer().text();
        if let Some((config, accounts)) = &self.account_selection {
            let Some(account) = accounts.get(self.selected_profile) else {
                return;
            };
            match bcode_config::edit::plan_context_account_selection(path.into(), config, account) {
                Ok(edit) => {
                    self.pending = Some(edit);
                    "Review context/account selection. Enter saves; then M discovers models. No credentials change.".clone_into(&mut self.status);
                }
                Err(_) => "Cannot safely select account. Review context configuration."
                    .clone_into(&mut self.status),
            }
            return;
        }
        if let Some((context, provider, account, models)) = &self.discovered_models {
            let Some(model) = models.get(self.selected_profile) else {
                return;
            };
            match bcode_config::edit::plan_scoped_model_selection(
                path.into(),
                context.as_deref(),
                provider,
                model,
                Some(account),
            ) {
                Ok(edit) => {
                    self.pending = Some(edit);
                    "Review provider, account, model, context and file. Enter saves this selection; Esc cancels.".clone_into(&mut self.status);
                }
                Err(_) => "Cannot safely edit model selection. Review configuration."
                    .clone_into(&mut self.status),
            }
            return;
        }
        if self.create_context {
            match bcode_config::edit::plan_context_creation(path.into(), key, value) {
                Ok(edit) => {
                    self.pending = Some(edit);
                    "Review the file, stable ID, and label. Enter creates an empty context; Esc cancels.".clone_into(&mut self.status);
                }
                Err(_) => "Cannot create context: check the ID, duplicate definitions, and destination file.".clone_into(&mut self.status),
            }
            return;
        }
        let parsed = if value.trim().is_empty() {
            None
        } else {
            let Ok(parsed) = toml::from_str::<toml::Value>(&format!("value = {value}")) else {
                "Invalid TOML value. Strings must be quoted; booleans use true/false."
                    .clone_into(&mut self.status);
                return;
            };
            parsed.get("value").cloned()
        };
        let keys = key.split('/').map(str::to_owned).collect::<Vec<_>>();
        match bcode_config::edit::plan_field_edit(path.into(), &keys, parsed) {
            Ok(edit) => {
                "Review the destination, field and value above. Enter confirms this exact edit; Esc cancels.".clone_into(&mut self.status);
                self.pending = Some(edit);
            }
            Err(_) => "Unsupported field or invalid configuration. Use connection controls for secrets and theme controls for appearance.".clone_into(&mut self.status),
        }
    }

    /// Render form fields and review state within the existing terminal.
    pub fn render(&mut self, frame: &mut PaintCx<'_, '_>, theme: &PresentedTheme) {
        let area = Rect::new(0, 0, frame.area().width, frame.area().height);
        let policy = TextInputPolicy::default();
        let labels = if self.create_context {
            [
                "Configuration file",
                "Stable context ID",
                "Display label (plain text)",
            ]
        } else {
            [
                "Configuration file",
                "Field (example: model/profile)",
                "TOML value (empty removes override)",
            ]
        };
        let title = if self.pending.is_some() {
            "Review settings change"
        } else {
            "Settings"
        };
        write(
            frame,
            Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), 1),
            title,
            theme.focused,
        );
        for (index, (input, label)) in self.inputs.iter_mut().zip(labels).enumerate() {
            let y = area
                .y
                .saturating_add(3 + u16::try_from(index).unwrap_or(0) * 3);
            if y.saturating_add(1) >= area.bottom() {
                break;
            }
            write(
                frame,
                Rect::new(area.x + 2, y, area.width.saturating_sub(4), 1),
                label,
                theme.muted,
            );
            let rect = Rect::new(area.x + 2, y + 1, area.width.saturating_sub(4), 1);
            input.set_content_area(rect, &policy);
            let retained = std::cell::RefCell::new(input.clone());
            let policy = TextInputPolicy::default();
            let editor =
                TextInputComponent::new(format!("setup_settings_form.{index}"), &retained, &policy)
                    .style(theme.text)
                    .selection_style(theme.focused)
                    .focused(index == self.focused && self.pending.is_none());
            let layout = editor.layout(Constraints::tight(rect.size()), &mut LayoutCx::new());
            frame.with_child(
                i32::from(rect.x),
                i64::from(rect.y),
                LocalRect::new(0, 0, rect.width, rect.height),
                |cx| editor.paint(&layout, cx),
            );
            *input = retained.into_inner();
            input.set_content_area(rect, &policy);
        }
        write(
            frame,
            Rect::new(
                area.x + 1,
                area.bottom().saturating_sub(2),
                area.width.saturating_sub(2),
                1,
            ),
            &self.status,
            theme.text,
        );
    }
}

fn write(frame: &mut PaintCx<'_, '_>, area: Rect, text: &str, style: Style) {
    frame.write_line_with_fallback_style(
        LocalRect::terminal(area),
        &Line::from_spans(vec![Span::styled(text, style)]),
        Style::new(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_picker_snapshot_never_creates_a_review_or_writes_a_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bcode.toml");
        let expected = bcode_config::BcodeConfig {
            active_context: Some("nonexistent-snapshot-context".to_owned()),
            ..Default::default()
        };
        let mut form =
            SetupSettingsForm::new(&path, "model/profile").with_selection_snapshot(expected);
        form.inputs[2] = TextInputState::new(TextEditBuffer::from_text("'profile'"));
        form.submit();
        assert!(form.pending.is_none());
        assert!(!path.exists());
        assert!(form.status.contains("reload the picker"));
    }

    #[test]
    fn account_picker_selects_provider_and_clears_stale_model_without_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bcode.toml");
        std::fs::write(&path, "[contexts]\nactive = 'custom'\n[contexts.entries.custom.model]\nprofile = 'old'\nmodel_id = 'old-model'\nauth_pool = 'old-pool'\n[contexts.entries.custom.auth.profiles.account]\nbackend = 'env'\nowner_plugin_id = 'example.plugin'\n").unwrap();
        let config = bcode_config::load_config_from_paths(std::slice::from_ref(&path)).unwrap();
        let mut form = SetupSettingsForm::accounts(&path, config);
        let before = std::fs::read(&path).unwrap();
        form.submit();
        assert!(form.pending.is_some());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        form.submit();
        let selected = bcode_config::load_config_from_paths(&[path]).unwrap();
        assert_eq!(
            selected.model.provider_plugin_id.as_deref(),
            Some("example.plugin")
        );
        assert!(selected.model.profile.is_none());
        assert!(selected.model.model_id.is_none());
        assert!(selected.model.auth_pool.is_none());
        assert_eq!(
            selected.model.auth_profile.as_deref(),
            Some("ctx-6-custom-account")
        );
        assert_eq!(selected.auth.profiles.len(), 1);
    }

    #[test]
    fn discovered_model_selection_writes_context_account_and_model_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bcode.toml");
        let models: bcode_model::ModelList = serde_json::from_value(serde_json::json!({"models":[{"model_id":"catalog-model","display_name":"Catalog model"}]})).unwrap();
        let mut form = SetupSettingsForm::discovered_models(
            &path,
            Some("custom".to_owned()),
            "example.plugin".to_owned(),
            "account".to_owned(),
            models,
        );
        form.submit();
        assert!(form.pending.is_some());
        assert!(!path.exists());
        form.submit();
        let value: toml::Value = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let model = &value["contexts"]["entries"]["custom"]["model"];
        assert_eq!(model["model_id"].as_str(), Some("catalog-model"));
        assert_eq!(model["auth_profile"].as_str(), Some("account"));
        assert_eq!(model["provider_plugin_id"].as_str(), Some("example.plugin"));
        assert!(value.get("model").is_none());
    }

    #[test]
    fn context_creation_and_selection_require_separate_reviewed_edits() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bcode.toml");
        let mut creation = SetupSettingsForm::create_context(&path);
        creation.inputs[1] = TextInputState::new(TextEditBuffer::from_text("custom-id"));
        creation.inputs[2] = TextInputState::new(TextEditBuffer::from_text("Custom label"));
        creation.submit();
        assert!(creation.pending.is_some());
        assert!(!path.exists());
        creation.submit();
        let config = bcode_config::load_config_from_paths(std::slice::from_ref(&path)).unwrap();
        assert!(config.active_context.is_none());
        assert!(config.auth.profiles.is_empty());
        let mut picker = SetupSettingsForm::contexts(&path, &config);
        assert!(picker.status.contains("Custom label"));
        picker.submit();
        assert!(picker.pending.is_some());
        picker.submit();
        let selected = bcode_config::load_config_from_paths(&[path]).unwrap();
        assert_eq!(selected.active_context.as_deref(), Some("custom-id"));
        assert!(selected.auth.profiles.is_empty());
    }

    #[test]
    fn context_creation_cancel_and_duplicate_preserve_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bcode.toml");
        let mut form = SetupSettingsForm::create_context(&path);
        form.inputs[1] = TextInputState::new(TextEditBuffer::from_text("custom"));
        form.submit();
        form.handle_event(&Event::Key(bmux_keyboard::KeyStroke::simple(
            KeyCode::Escape,
        )));
        assert!(form.pending.is_none());
        assert!(!path.exists());
        form.submit();
        form.submit();
        let before = std::fs::read(&path).unwrap();
        form.submit();
        assert!(form.pending.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn configured_profile_picker_requires_review_and_preserves_local_names() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bcode.toml");
        let config: bcode_config::BcodeConfig = toml::from_str(
            r#"
[model.profiles."custom account"]
provider_plugin_id = "example.provider"
model_id = "example-model"
auth_profile = "local-account"
"#,
        )
        .unwrap();
        let mut form = SetupSettingsForm::models(&path, &config);
        assert!(form.status.contains("local-account"));
        form.submit();
        assert!(form.pending.is_some());
        assert!(!path.exists());
        form.submit();
        let saved: bcode_config::BcodeConfig =
            toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(saved.model.profile.as_deref(), Some("custom account"));
    }

    #[test]
    fn context_picker_edits_only_the_selected_context() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bcode.toml");
        let mut config = bcode_config::BcodeConfig {
            active_context: Some("custom-context".to_owned()),
            ..Default::default()
        };
        config.model.profiles.insert(
            "fast".to_owned(),
            bcode_config::ModelProfileConfig {
                provider_plugin_id: "example.provider".to_owned(),
                model_id: Some("example-model".to_owned()),
                ..Default::default()
            },
        );
        let mut form = SetupSettingsForm::models(&path, &config);
        form.submit();
        assert!(form.pending.is_some());
        form.submit();
        let saved: toml::Value = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert!(saved.get("model").is_none());
        assert_eq!(
            saved["contexts"]["entries"]["custom-context"]["model"]["profile"].as_str(),
            Some("fast")
        );
    }

    #[test]
    fn empty_model_picker_does_not_remove_existing_selection() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bcode.toml");
        let mut form = SetupSettingsForm::models(&path, &bcode_config::BcodeConfig::default());
        form.submit();
        assert!(form.pending.is_none());
        assert!(!path.exists());
    }

    #[test]
    fn review_and_cancel_never_write_configuration() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("bcode.toml");
        let mut form = SetupSettingsForm::new(&path, "model/profile");
        form.inputs[2] = TextInputState::new(TextEditBuffer::from_text("\"work\""));
        form.submit();
        assert!(form.pending.is_some());
        assert!(!path.exists());
        form.pending = None;
        assert!(!path.exists());
    }

    #[test]
    fn second_confirmation_applies_reviewed_edit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("bcode.toml");
        let mut form = SetupSettingsForm::new(&path, "model/profile");
        form.inputs[2] = TextInputState::new(TextEditBuffer::from_text("\"work\""));
        form.submit();
        form.submit();
        let config: bcode_config::BcodeConfig =
            toml::from_str(&std::fs::read_to_string(path).expect("saved")).expect("valid config");
        assert_eq!(config.model.profile.as_deref(), Some("work"));
    }
}
