//! Shared question form. Ports provide input bindings, never application or daemon access.
#[cfg(not(feature = "static-bundled"))]
use bcode_tool::InteractionInput;
use bcode_tool::InteractionOutput;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::{event::Event, geometry::Rect, paint::PaintCx};
use bmux_tui_components::text_input::{TextInputControl, TextInputPolicy, TextInputState};

pub use crate::question_interaction::{
    QuestionFocusTarget, QuestionInteractionController, QuestionSnapshot,
};
pub use crate::question_outcome::question_outcome_rows;
use crate::question_tui::QuestionTerminalRenderer;

/// Result of translating a BMUX event, before producer controller application.
#[cfg(feature = "static-bundled")]
pub use bcode_plugin_sdk::tui::TerminalInteractionInput as QuestionInputResult;
#[cfg(not(feature = "static-bundled"))]
#[derive(Debug)]
pub enum QuestionInputResult {
    /// No matching input.
    Ignored,
    /// Editor-only movement or selection.
    Consumed,
    /// Producer-owned semantic transition.
    Semantic(InteractionInput),
}

/// Narrow input policy injected into the shared renderer.
pub trait QuestionInputPort {
    /// Resolve a binding to its registered action.
    fn keymap_action(&self, scope: &str, stroke: KeyStroke) -> Option<String>;
    /// Labels for a registered action.
    fn keymap_keys(&self, scope: &str, action: &str) -> Vec<String>;
    /// Apply an editor key using BMUX text editing; false means unhandled.
    fn edit_key(&self, state: &mut TextInputState, stroke: KeyStroke) -> bool;
}

/// Default portable bindings matching native question defaults.
#[derive(Debug, Default)]
pub struct DefaultQuestionInput;
impl QuestionInputPort for DefaultQuestionInput {
    fn keymap_action(&self, scope: &str, stroke: KeyStroke) -> Option<String> {
        if scope != "question" {
            return None;
        }
        let modifiers = stroke.modifiers;
        if modifiers.alt
            || modifiers.super_key
            || modifiers.hyper
            || modifiers.meta
            || (modifiers.ctrl && (stroke.key != KeyCode::Enter || modifiers.shift))
            || (modifiers.shift && stroke.key != KeyCode::Tab)
        {
            return None;
        }
        let action = match stroke.key {
            KeyCode::Tab if stroke.modifiers.shift => "previous",
            KeyCode::Tab => "next",
            KeyCode::Up => "up",
            KeyCode::Down => "down",
            KeyCode::Left => "left",
            KeyCode::Right => "right",
            KeyCode::Enter if stroke.modifiers.ctrl => "submit",
            KeyCode::Enter => "activate",
            KeyCode::Escape => "cancel",
            KeyCode::Space | KeyCode::Char(' ') => "toggle",
            KeyCode::Char(c @ '0'..='9') => {
                return Some(format!(
                    "question.option.{}",
                    if c == '0' {
                        9
                    } else {
                        c as usize - '1' as usize
                    }
                ));
            }
            _ => return None,
        };
        Some(format!("question.{action}"))
    }
    fn keymap_keys(&self, _scope: &str, action: &str) -> Vec<String> {
        let key = match action {
            "question.next" => "tab",
            "question.previous" => "shift+tab",
            "question.up" => "up",
            "question.down" => "down",
            "question.left" => "left",
            "question.right" => "right",
            "question.activate" => "enter",
            "question.toggle" => "space",
            "question.cancel" => "esc",
            "question.submit" => "ctrl+enter",
            other => {
                return other
                    .strip_prefix("question.option.")
                    .and_then(|s| s.parse::<usize>().ok())
                    .map_or_else(Vec::new, |i| vec![((i + 1) % 10).to_string()]);
            }
        };
        vec![key.into()]
    }
    fn edit_key(&self, state: &mut TextInputState, stroke: KeyStroke) -> bool {
        !matches!(
            TextInputControl::new(&TextInputPolicy::chat_composer()).handle_key(state, stroke),
            bmux_tui_components::text_input::TextInputOutcome::Ignored
        )
    }
}

/// Ephemeral form presentation. Controller state must be replaced from canonical producer snapshots.
pub struct QuestionPresentation {
    renderer: QuestionTerminalRenderer,
}
impl Default for QuestionPresentation {
    fn default() -> Self {
        let mut renderer = QuestionTerminalRenderer::default();
        renderer.refresh_keymap(&DefaultQuestionInput);
        Self { renderer }
    }
}
impl QuestionPresentation {
    /// Apply renderer-owned semantic styles without changing canonical state or geometry.
    pub fn set_theme(&mut self, theme: Option<bmux_tui_components::theme::ComponentTheme>) {
        self.renderer.set_component_theme(theme);
    }
    /// Refresh labels and layout from the host's current bindings before measurement.
    pub fn prepare(&mut self, port: &dyn QuestionInputPort) -> bool {
        self.renderer.refresh_keymap(port)
    }

    /// Measure the same logical rows used by native question rendering.
    #[must_use]
    pub fn preferred_height(&mut self, snapshot: &QuestionSnapshot, width: u16) -> u16 {
        self.renderer.height(snapshot, width)
    }
    /// Focus anchor in logical source rows.
    #[must_use]
    pub fn focused_rows(
        &mut self,
        snapshot: &QuestionSnapshot,
        width: u16,
    ) -> std::ops::Range<u16> {
        self.renderer.focus_rows(snapshot, width)
    }
    /// Paint a clipped logical slice through the caller's root BMUX paint context.
    pub fn render_slice(
        &mut self,
        snapshot: &QuestionSnapshot,
        offset: u16,
        area: Rect,
        cx: &mut PaintCx<'_, '_>,
    ) {
        self.renderer.paint_slice(snapshot, offset, area, cx);
    }
    /// Translate an event without changing authoritative controller state.
    pub fn input(
        &mut self,
        event: &Event,
        snapshot: &QuestionSnapshot,
        port: &dyn QuestionInputPort,
    ) -> QuestionInputResult {
        self.renderer.route_input(event, snapshot, port, true)
    }
    /// Apply a translated event to a local producer controller. The caller owns submission transport.
    pub fn handle_event(
        &mut self,
        event: &Event,
        controller: &mut QuestionInteractionController,
        port: &dyn QuestionInputPort,
    ) -> InteractionOutput {
        match self.input(event, &controller.snapshot(), port) {
            QuestionInputResult::Semantic(input) => controller.handle_input(input),
            QuestionInputResult::Consumed => InteractionOutput::Redraw,
            QuestionInputResult::Ignored => InteractionOutput::None,
        }
    }
}

#[cfg(feature = "static-bundled")]
pub(crate) struct NativeQuestionPort<'a>(pub &'a dyn bcode_plugin_sdk::tui::PluginTuiHost);
#[cfg(feature = "static-bundled")]
impl QuestionInputPort for NativeQuestionPort<'_> {
    fn keymap_action(&self, scope: &str, stroke: KeyStroke) -> Option<String> {
        self.0.keymap_action(scope, stroke)
    }
    fn keymap_keys(&self, scope: &str, action: &str) -> Vec<String> {
        self.0.keymap_keys(scope, action)
    }
    fn edit_key(&self, state: &mut TextInputState, stroke: KeyStroke) -> bool {
        use bcode_plugin_sdk::tui::keymap::controls::{ControlAction, ControlInput};
        use bmux_tui_components::text_input::{TextInputHistoryAction, TextInputHistoryBindings};
        let policy = TextInputPolicy::chat_composer();
        let control = TextInputControl::new(&policy)
            .with_history_bindings(TextInputHistoryBindings::disabled());
        let history = match self.0.keymap_action("question.text", stroke).as_deref() {
            Some("question.text.undo") => Some(TextInputHistoryAction::Undo),
            Some("question.text.redo") => Some(TextInputHistoryAction::Redo),
            _ => None,
        };
        if let Some(history) = history {
            control.handle_key_with_history_action(state, stroke, Some(history));
            return true;
        }
        match ControlInput::from_host(
            self.0,
            "question.text",
            stroke,
            &[ControlAction::TEXT, &[ControlAction::Newline]].concat(),
            true,
        ) {
            ControlInput::Action(action) => {
                control.handle_key(state, action.native_stroke());
            }
            ControlInput::Text(text) => state.buffer_mut().insert_char(text.0),
            ControlInput::Ignored => return false,
        }
        true
    }
}

#[cfg(test)]
#[path = "presentation_tests.rs"]
mod tests;
