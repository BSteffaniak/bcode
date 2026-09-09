//! Native connection/import form. Never reads stdin or owns terminal lifecycle.

use super::theme::PresentedTheme;
use bmux_keyboard::KeyCode;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::input::TextInput;
use bmux_tui::prelude::{Line, Span, Style, Widget};
use bmux_tui_components::text_input::{TextInputControl, TextInputPolicy, TextInputState};
use std::collections::BTreeMap;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Presentation {
    Guided,
    Advanced,
}

pub struct ConnectionForm {
    presentation: Presentation,
    providers: Vec<bcode_provider_auth_models::AuthProviderContribution>,
    picker: Option<bool>,
    selected: usize,
    query: String,
    interactive: bool,
    device: Option<super::setup_device_login::DeviceLogin>,
    importing: bool,
    fields: [TextInputState; 4],
    focus: usize,
    secret: zeroize::Zeroizing<String>,
    review: bool,
    status: String,
}

impl ConnectionForm {
    pub fn new(importing: bool) -> Self {
        let providers = load_connection_choices().unwrap_or_default();
        Self {
            presentation: Presentation::Guided,
            providers, picker: Some(false), selected: 0, query: String::new(), interactive: false,
            device: None,
            importing,
            fields: [String::new(), "api_key".to_owned(), String::new(), bcode_config::default_auth_vault_path().display().to_string()]
                .map(|text| TextInputState::new(TextEditBuffer::from_text(text))),
            focus: 0, secret: zeroize::Zeroizing::new(String::new()), review: false,
            status: "Provider ID, method, profile, vault, then secret. Tab moves fields; Enter reviews; Esc returns.".to_owned(),
        }
    }

    pub fn handle_event(&mut self, event: &Event) -> bool {
        if self.picker.is_some() {
            return self.handle_picker(event);
        }
        if let Some(device) = &mut self.device {
            device.refresh();
            handle_auth_prompt(device, event);
            if matches!(event, Event::Key(key) if key.key == KeyCode::Escape) {
                if device.terminal {
                    self.device = None;
                } else {
                    device.cancel();
                }
            }
            return false;
        }
        match event {
            Event::Key(key) if key.key == KeyCode::Escape => {
                if self.review {
                    self.review = false;
                } else {
                    return true;
                }
            }
            Event::Key(key) if key.key == KeyCode::F(2) => {
                self.presentation = if self.presentation == Presentation::Guided {
                    Presentation::Advanced
                } else {
                    Presentation::Guided
                };
                self.focus = if self.presentation == Presentation::Advanced {
                    2
                } else {
                    4
                };
            }
            Event::Key(key) if key.key == KeyCode::Tab && !self.review => {
                self.focus = if self.presentation == Presentation::Advanced {
                    (self.focus + 1) % 5
                } else {
                    4
                };
            }
            Event::Key(key) if key.key == KeyCode::Enter => {
                if self.review && !self.importing && self.interactive {
                    self.device = Some(super::setup_device_login::DeviceLogin::start(
                        self.fields[0].buffer().text().to_owned(),
                        self.fields[1].buffer().text().to_owned(),
                        self.fields[2].buffer().text().to_owned(),
                        self.fields[3].buffer().text().to_owned(),
                    ));
                    self.review = false;
                } else if self.review {
                    self.status = self.save().map_or_else(
                        |message| message,
                        |()| "Credentials saved. Esc returns to setup.".to_owned(),
                    );
                    self.secret.clear();
                    self.review = false;
                } else {
                    self.review = true;
                    "Review destination and provider. Enter saves; Esc cancels. No remote verification is performed.".clone_into(&mut self.status);
                }
            }
            Event::Key(key) if !self.review && self.focus == 4 => match key.key {
                KeyCode::Char(character) if self.secret.len() < 4096 => self.secret.push(character),
                KeyCode::Backspace => {
                    self.secret.pop();
                }
                _ => {}
            },
            Event::Paste(text) if !self.review && self.focus == 4 => {
                let remaining = 4096_usize.saturating_sub(self.secret.len());
                self.secret
                    .extend(text.chars().filter(|c| !c.is_control()).take(remaining));
            }
            Event::Key(key) if !self.review => {
                let _ = TextInputControl::new(&TextInputPolicy::default())
                    .handle_key(&mut self.fields[self.focus], *key);
            }
            Event::Paste(text) if !self.review => {
                let _ = TextInputControl::new(&TextInputPolicy::default())
                    .handle_paste(&mut self.fields[self.focus], text);
            }
            _ => {}
        }
        false
    }

    fn choices(&self) -> Vec<(usize, String)> {
        let labels = if self.picker == Some(true) {
            self.providers
                .iter()
                .find(|provider| provider.provider_id == self.fields[0].buffer().text())
                .map(|provider| {
                    provider
                        .methods
                        .iter()
                        .map(|method| match method {
                            bcode_provider_auth_models::AuthMethodContribution::SecretFields {
                                display_name,
                                ..
                            }
                            | bcode_provider_auth_models::AuthMethodContribution::Interactive {
                                display_name,
                                ..
                            } => display_name.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            self.providers
                .iter()
                .map(|provider| provider.display_name.clone())
                .collect::<Vec<_>>()
        };
        labels
            .into_iter()
            .enumerate()
            .filter(|(_, label)| label.to_lowercase().contains(&self.query.to_lowercase()))
            .collect()
    }

    fn handle_picker(&mut self, event: &Event) -> bool {
        match event {
            Event::Key(key) => match key.key {
                KeyCode::Escape => {
                    if self.picker == Some(true) {
                        self.picker = Some(false);
                        self.query.clear();
                        self.selected = 0;
                    } else {
                        return true;
                    }
                }
                KeyCode::Char(character) => {
                    self.query.push(character);
                    self.selected = 0;
                }
                KeyCode::Backspace => {
                    self.query.pop();
                    self.selected = 0;
                }
                KeyCode::Down => self.selected = self.selected.saturating_add(1),
                KeyCode::Up => self.selected = self.selected.saturating_sub(1),
                KeyCode::Enter => self.select_choice(),
                _ => {}
            },
            Event::Paste(text) => {
                self.query.extend(
                    text.chars()
                        .filter(|character| !character.is_control())
                        .take(256),
                );
                self.selected = 0;
            }
            _ => {}
        }
        false
    }

    fn select_choice(&mut self) {
        let choices = self.choices();
        let Some((index, _)) = choices.get(self.selected.min(choices.len().saturating_sub(1)))
        else {
            return;
        };
        if self.picker == Some(false) {
            let provider = &self.providers[*index];
            self.fields[0] =
                TextInputState::new(TextEditBuffer::from_text(provider.provider_id.clone()));
            if let Ok(config) = bcode_config::load_config() {
                let name = bcode_provider_auth::enrollment::new_profile_name(
                    &config,
                    &bcode_config::load_runtime_auth_subscriptions(),
                    &provider.provider_id,
                );
                self.fields[2] = TextInputState::new(TextEditBuffer::from_text(name));
            }
            self.picker = Some(true);
        } else {
            let Some(provider) = self
                .providers
                .iter()
                .find(|provider| provider.provider_id == self.fields[0].buffer().text())
            else {
                return;
            };
            let method = &provider.methods[*index];
            self.interactive = matches!(
                method,
                bcode_provider_auth_models::AuthMethodContribution::Interactive { .. }
            );
            self.fields[1] =
                TextInputState::new(TextEditBuffer::from_text(method.method_id().to_owned()));
            self.focus = 4;
            self.review = self.interactive;
            "Enter confirms connection using the selected method and default secure destination. Tab reviews advanced fields.".clone_into(&mut self.status);
            self.picker = None;
        }
        self.query.clear();
        self.selected = 0;
    }

    fn save(&self) -> Result<(), String> {
        let config =
            bcode_config::load_config().map_err(|_| "Cannot load configuration".to_owned())?;
        let selection = bcode_config::plugin_selection_with_default_plugin_ids(
            &config,
            std::iter::empty::<&str>(),
        );
        let mut host = bcode_plugin::PluginHost::load_defaults_with_static_bundled(
            &selection,
            &super::static_bundled_plugins(),
        )
        .map_err(|_| "Cannot load enabled authentication providers".to_owned())?;
        let result = self.save_with_host(&config, &host);
        let cleanup = host
            .deactivate_all()
            .map_err(|_| "Authentication provider cleanup failed".to_owned());
        result.and(cleanup)
    }

    fn save_with_host(
        &self,
        config: &bcode_config::BcodeConfig,
        host: &bcode_plugin::PluginHost,
    ) -> Result<(), String> {
        let provider_id = self.fields[0].buffer().text();
        let method_id = self.fields[1].buffer().text();
        let provider = host
            .auth_provider_registry()
            .get(provider_id)
            .ok_or_else(|| "Unknown or disabled authentication provider".to_owned())?;
        let method = provider
            .contribution
            .methods
            .iter()
            .find(|method| method.method_id() == method_id)
            .ok_or_else(|| "Unknown authentication method".to_owned())?;
        let bcode_provider_auth_models::AuthMethodContribution::SecretFields { fields, .. } =
            method
        else {
            return Err(
                "This method requires an interactive authentication flow; no credentials saved."
                    .to_owned(),
            );
        };
        if fields.len() != 1 {
            return Err(
                "This method requires multiple credential fields; no credentials saved.".to_owned(),
            );
        }
        let imported;
        let secret = if self.importing {
            let index = self.secret.parse::<usize>().map_err(|_| {
                "Enter a provider-declared source index in the source field".to_owned()
            })?;
            let home =
                std::env::home_dir().ok_or_else(|| "Home directory unavailable".to_owned())?;
            imported = bcode_provider_auth::discovery::read_selected(&home, &fields[0], index)
                .map_err(|_| "Source is unavailable or incompatible".to_owned())?
                .ok_or_else(|| "Credential source is empty".to_owned())?;
            imported.as_str()
        } else {
            self.secret.as_str()
        };
        fields[0]
            .validation
            .validate_secret(secret)
            .map_err(|_| "Credential does not meet provider requirements".to_owned())?;
        let profile = self.fields[2].buffer().text();
        let vault = self.fields[3].buffer().text();
        let prepared = bcode_provider_auth::enrollment::prepare(
            config,
            &bcode_config::load_runtime_auth_subscriptions(),
            &provider.contribution,
            &provider.plugin_id,
            method_id,
            bcode_provider_auth::enrollment::EnrollmentDestination {
                profile: (!profile.is_empty()).then(|| profile.to_owned()),
                vault: (!vault.is_empty()).then(|| vault.into()),
                recipient_key: None,
            },
        )
        .map_err(|_| "Profile ownership or destination is inconsistent".to_owned())?;
        let lifecycle = bcode_provider_auth::lifecycle::AuthVaultLifecycle::new(
            &prepared.resolved,
            provider_id,
            &provider.plugin_id,
            method,
        )
        .map_err(|_| "Profile method or ownership is inconsistent".to_owned())?;
        lifecycle.import_new(BTreeMap::from([(fields[0].credential_id.clone(), secret.to_owned())]))
            .map_err(|_| "Could not save without overwriting existing credentials. Choose a new profile or inspect the vault.".to_owned())?;
        if prepared.publish_runtime {
            bcode_provider_auth::enrollment::publish(&prepared.resolved).map_err(|_| "Credential saved, but profile publication failed. Inspect authentication state before retrying.".to_owned())?;
        }
        Ok(())
    }

    fn render_picker(&self, frame: &mut Frame<'_>, theme: &PresentedTheme) {
        let area = frame.area();
        write(
            frame,
            area,
            1,
            if self.picker == Some(true) {
                "How would you like to connect?"
            } else {
                "Choose a provider"
            },
            theme.focused,
        );
        write(
            frame,
            area,
            3,
            &format!("Search: {}", self.query),
            theme.text,
        );
        let choices = self.choices();
        let selected = self.selected.min(choices.len().saturating_sub(1));
        let height = usize::from(area.height.saturating_sub(8));
        let start = selected.saturating_sub(height.saturating_sub(1));
        for (row, (_, label)) in choices.iter().enumerate().skip(start).take(height) {
            write(
                frame,
                area,
                5 + u16::try_from(row - start).unwrap_or(0),
                &format!("{} {label}", if row == selected { ">" } else { " " }),
                if row == selected {
                    theme.focused
                } else {
                    theme.text
                },
            );
        }
        if choices.is_empty() {
            write(
                frame,
                area,
                5,
                "No matching enabled providers or methods.",
                theme.muted,
            );
        }
        write(
            frame,
            area,
            area.height.saturating_sub(2),
            "Type to search • ↑/↓ select • Enter continue • Esc back",
            theme.muted,
        );
    }

    fn render_guided(&self, frame: &mut Frame<'_>, theme: &PresentedTheme) {
        let area = frame.area();
        let provider = self
            .providers
            .iter()
            .find(|provider| provider.provider_id == self.fields[0].buffer().text());
        let name = provider.map_or("Connection", |provider| provider.display_name.as_str());
        write(frame, area, 1, name, theme.focused);
        write(
            frame,
            area,
            3,
            if self.interactive {
                "Ready to start browser authorization"
            } else {
                "Enter your credential (paste supported)"
            },
            theme.text,
        );
        if !self.interactive {
            write(
                frame,
                area,
                5,
                if self.secret.is_empty() {
                    "Credential: (empty)"
                } else {
                    "Credential: ********"
                },
                theme.text,
            );
        }
        write(
            frame,
            area,
            7,
            &format!("Secure profile: {}", self.fields[2].buffer().text()),
            theme.muted,
        );
        write(
            frame,
            area,
            8,
            &format!("Vault: {}", self.fields[3].buffer().text()),
            theme.muted,
        );
        write(
            frame,
            area,
            area.height.saturating_sub(3),
            "Enter continue/confirm • F2 Advanced • Esc back",
            theme.focused,
        );
        write(
            frame,
            area,
            area.height.saturating_sub(2),
            &self.status,
            theme.text,
        );
    }

    pub fn render(&mut self, frame: &mut Frame<'_>, theme: &PresentedTheme) {
        if self.picker.is_some() {
            self.render_picker(frame, theme);
            return;
        }
        if let Some(device) = &mut self.device {
            device.refresh();
            let area = frame.area();
            if render_auth_prompt(device, frame, area, theme) {
                return;
            }
            for (index, text) in device.lines.iter().enumerate() {
                write(
                    frame,
                    area,
                    u16::try_from(index).unwrap_or(0).saturating_add(2),
                    text,
                    theme.text,
                );
            }
            return;
        }
        if self.presentation == Presentation::Guided {
            self.render_guided(frame, theme);
            return;
        }
        let area = frame.area();
        let labels = [
            "Provider",
            "Method",
            "Profile (empty: provider default)",
            "Vault",
        ];
        write(
            frame,
            area,
            0,
            if self.importing {
                "Import credential — enter the provider-declared source index in the secret field"
            } else {
                "Connect provider — credentials remain masked"
            },
            theme.focused,
        );
        for (index, input) in self.fields.iter_mut().enumerate() {
            let row = 2 + u16::try_from(index).unwrap_or(0) * 2;
            write(frame, area, row, labels[index], theme.muted);
            let rect = Rect::new(
                area.x.saturating_add(1),
                area.y.saturating_add(row + 1),
                area.width.saturating_sub(2),
                1,
            );
            input.set_content_area(rect, &TextInputPolicy::default());
            TextInput::new(input.buffer())
                .style(theme.text)
                .cursor_visible(self.focus == index && !self.review)
                .render(rect, frame);
        }
        write(
            frame,
            area,
            10,
            if self.secret.is_empty() {
                "Secret: (empty)"
            } else {
                "Secret: ********"
            },
            theme.text,
        );
        write(
            frame,
            area,
            area.height.saturating_sub(2),
            &self.status,
            theme.muted,
        );
    }
}

fn render_auth_prompt(
    device: &super::setup_device_login::DeviceLogin,
    frame: &mut Frame<'_>,
    area: Rect,
    theme: &PresentedTheme,
) -> bool {
    if let Some(bcode_provider_auth_models::AuthFlowEffect::Prompt {
        message, choices, ..
    }) = &device.prompt
    {
        write(frame, area, 1, message, theme.focused);
        for (index, choice) in choices
            .iter()
            .enumerate()
            .take(usize::from(area.height.saturating_sub(7)))
        {
            write(
                frame,
                area,
                3 + u16::try_from(index).unwrap_or(0),
                choice,
                theme.text,
            );
        }
        write(
            frame,
            area,
            area.height.saturating_sub(3),
            &format!("Answer: {}", device.answer),
            theme.text,
        );
        write(
            frame,
            area,
            area.height.saturating_sub(2),
            "Enter submits • Esc cancels sign-in",
            theme.muted,
        );
        return true;
    }
    false
}

fn handle_auth_prompt(device: &mut super::setup_device_login::DeviceLogin, event: &Event) {
    if device.prompt.is_some() {
        match event {
            Event::Key(key) => match key.key {
                KeyCode::Enter => device.submit_answer(),
                KeyCode::Char(character) if device.answer.len() < 4096 => {
                    device.answer.push(character);
                }
                KeyCode::Backspace => {
                    device.answer.pop();
                }
                _ => {}
            },
            Event::Paste(text) => {
                let remaining = 4096_usize.saturating_sub(device.answer.len());
                device
                    .answer
                    .extend(text.chars().filter(|c| !c.is_control()).take(remaining));
            }
            _ => {}
        }
    }
}

fn load_connection_choices()
-> Result<Vec<bcode_provider_auth_models::AuthProviderContribution>, String> {
    let config = bcode_config::load_config().map_err(|_| "Configuration unavailable".to_owned())?;
    let selection =
        bcode_config::plugin_selection_with_default_plugin_ids(&config, std::iter::empty::<&str>());
    let mut host = bcode_plugin::PluginHost::load_defaults_with_static_bundled(
        &selection,
        &super::static_bundled_plugins(),
    )
    .map_err(|_| "Provider registration unavailable".to_owned())?;
    let mut providers = host
        .auth_provider_registry()
        .providers()
        .into_iter()
        .map(|provider| provider.contribution.clone())
        .collect::<Vec<_>>();
    providers.sort_by(|left, right| left.display_name.cmp(&right.display_name));
    host.deactivate_all()
        .map_err(|_| "Provider cleanup failed".to_owned())?;
    Ok(providers)
}

fn write(frame: &mut Frame<'_>, area: Rect, row: u16, text: &str, style: Style) {
    if row < area.height {
        frame.write_line_with_fallback_style(
            Rect::new(
                area.x.saturating_add(1),
                area.y.saturating_add(row),
                area.width.saturating_sub(2),
                1,
            ),
            &Line::from_spans(vec![Span::styled(text, style)]),
            Style::new(),
        );
    }
}
