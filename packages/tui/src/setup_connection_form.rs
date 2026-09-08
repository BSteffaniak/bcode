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

pub struct ConnectionForm {
    importing: bool,
    fields: [TextInputState; 4],
    focus: usize,
    secret: zeroize::Zeroizing<String>,
    review: bool,
    status: String,
}

impl ConnectionForm {
    pub fn new(importing: bool) -> Self {
        Self {
            importing,
            fields: [String::new(), "api_key".to_owned(), String::new(), bcode_config::default_auth_vault_path().display().to_string()]
                .map(|text| TextInputState::new(TextEditBuffer::from_text(text))),
            focus: 0, secret: zeroize::Zeroizing::new(String::new()), review: false,
            status: "Provider ID, method, profile, vault, then secret. Tab moves fields; Enter reviews; Esc returns.".to_owned(),
        }
    }

    pub fn handle_event(&mut self, event: &Event) -> bool {
        match event {
            Event::Key(key) if key.key == KeyCode::Escape => {
                if self.review {
                    self.review = false;
                } else {
                    return true;
                }
            }
            Event::Key(key) if key.key == KeyCode::Tab && !self.review => {
                self.focus = (self.focus + 1) % 5;
            }
            Event::Key(key) if key.key == KeyCode::Enter => {
                if self.review {
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

    pub fn render(&mut self, frame: &mut Frame<'_>, theme: &PresentedTheme) {
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
