//! In-terminal reviewed settings form. No terminal ownership handoff or stdin prompts.

use bmux_keyboard::KeyCode;
use bmux_text_edit::TextEditBuffer;
use bmux_tui::event::Event;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::Rect;
use bmux_tui::input::TextInput;
use bmux_tui::prelude::{Line, Span, Style, Widget};
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
        }
    }

    /// Handle form input without changing terminal lifecycle.
    pub fn handle_event(&mut self, event: &Event) -> SettingsFormOutcome {
        let policy = TextInputPolicy::default();
        match event {
            Event::Key(key) if key.key == KeyCode::Escape => {
                if self.pending.take().is_none() {
                    return SettingsFormOutcome::Close;
                }
                "Review cancelled; no changes saved.".clone_into(&mut self.status);
            }
            Event::Key(key) if key.key == KeyCode::Enter => self.submit(),
            Event::Key(key) if key.key == KeyCode::Tab && self.pending.is_none() => {
                self.focused = (self.focused + 1) % self.inputs.len();
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
    pub fn render(&mut self, frame: &mut Frame<'_>, theme: &PresentedTheme) {
        let area = frame.area();
        let policy = TextInputPolicy::default();
        let labels = [
            "Configuration file",
            "Field (example: model/profile)",
            "TOML value (empty removes override)",
        ];
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
            TextInput::new(input.buffer())
                .style(theme.text)
                .selection_style(theme.focused)
                .cursor_visible(index == self.focused && self.pending.is_none())
                .render(rect, frame);
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

fn write(frame: &mut Frame<'_>, area: Rect, text: &str, style: Style) {
    frame.write_line_with_fallback_style(
        area,
        &Line::from_spans(vec![Span::styled(text, style)]),
        Style::new(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

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
