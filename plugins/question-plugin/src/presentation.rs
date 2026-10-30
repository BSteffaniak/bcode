//! Shared question form. Ports provide input bindings, never application or daemon access.
#[cfg(not(feature = "static-bundled"))]
use bcode_tool::InteractionInput;
use bcode_tool::InteractionOutput;
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::{event::Event, geometry::Rect, paint::PaintCx};
use bmux_tui_components::text_input::{TextInputControl, TextInputPolicy, TextInputState};

pub use crate::question_interaction::{
    QuestionFocusTarget, QuestionInteractionController, QuestionSnapshot, QuestionSnapshotError,
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

/// Apply an effective question-text binding through the producer's shared BMUX editor.
///
/// `None` admits printable literal input only; `Some("")` represents an explicit unbind.
/// Unknown/unsupported bound actions are consumed without inserting their shortcut text.
/// Returns whether the key was consumed, not whether text changed. The presentation emits
/// semantic `Change` only when the editor's text actually changes; selection/history-only
/// movement remains local. No default binding lookup is performed here.
pub fn edit_question_key(
    state: &mut TextInputState,
    stroke: KeyStroke,
    resolved_action: Option<&str>,
) -> bool {
    use bmux_keyboard::Modifiers;
    use bmux_tui_components::text_input::{TextInputHistoryAction, TextInputHistoryBindings};
    let policy = TextInputPolicy::chat_composer();
    let control =
        TextInputControl::new(&policy).with_history_bindings(TextInputHistoryBindings::disabled());
    let Some(action) = resolved_action else {
        let modifiers = stroke.modifiers;
        if modifiers.ctrl
            || modifiers.alt
            || modifiers.super_key
            || modifiers.hyper
            || modifiers.meta
        {
            return false;
        }
        let character = match stroke.key {
            KeyCode::Char(character) if !character.is_control() => {
                if modifiers.shift && character.is_ascii_lowercase() {
                    character.to_ascii_uppercase()
                } else {
                    character
                }
            }
            KeyCode::Space => ' ',
            _ => return false,
        };
        control.handle_key(state, KeyStroke::simple(KeyCode::Char(character)));
        return true;
    };
    let history = match action {
        "question.text.undo" => Some(TextInputHistoryAction::Undo),
        "question.text.redo" => Some(TextInputHistoryAction::Redo),
        _ => None,
    };
    if let Some(history) = history {
        control.handle_key_with_history_action(state, stroke, Some(history));
        return true;
    }
    let (key, ctrl, shift) = match action {
        "tui.control.up" => (KeyCode::Up, false, false),
        "tui.control.down" => (KeyCode::Down, false, false),
        "tui.control.left" => (KeyCode::Left, false, false),
        "tui.control.right" => (KeyCode::Right, false, false),
        "tui.control.home" => (KeyCode::Home, false, false),
        "tui.control.end" => (KeyCode::End, false, false),
        "tui.control.backspace" => (KeyCode::Backspace, false, false),
        "tui.control.delete" => (KeyCode::Delete, false, false),
        "tui.control.word_left" => (KeyCode::Left, true, false),
        "tui.control.word_right" => (KeyCode::Right, true, false),
        "tui.control.delete_word_backward" => (KeyCode::Char('w'), true, false),
        "tui.control.delete_word_forward" => (KeyCode::Delete, true, false),
        "tui.control.line_start" => (KeyCode::Char('a'), true, false),
        "tui.control.line_end" => (KeyCode::Char('e'), true, false),
        "tui.control.delete_to_start" => (KeyCode::Char('u'), true, false),
        "tui.control.delete_to_end" => (KeyCode::Char('k'), true, false),
        "tui.control.select_left" => (KeyCode::Left, false, true),
        "tui.control.select_right" => (KeyCode::Right, false, true),
        "tui.control.select_up" => (KeyCode::Up, false, true),
        "tui.control.select_down" => (KeyCode::Down, false, true),
        "tui.control.select_word_left" => (KeyCode::Left, true, true),
        "tui.control.select_word_right" => (KeyCode::Right, true, true),
        "tui.control.newline" => (KeyCode::Enter, false, true),
        _ => return true,
    };
    control.handle_key(
        state,
        KeyStroke::with_modifiers(
            key,
            Modifiers {
                ctrl,
                shift,
                ..Modifiers::NONE
            },
        ),
    );
    true
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
#[derive(Clone)]
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
    /// Fork the complete retained presentation for a speculative frame transaction.
    /// This preserves editor undo/redo history, cursor/selection, committed hit geometry,
    /// layout caches and native focused-field fences without serialization or replay.
    /// Paint only the fork; publish it in place of the original only after the enclosing
    /// frame is accepted. A discarded candidate must never route input independently.
    #[must_use]
    pub fn fork(&self) -> Self {
        self.clone()
    }

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
        let action = self.0.keymap_action("question.text", stroke);
        if action.is_none() && self.0.keymap_is_overridden("question.text", stroke) {
            return edit_question_key(state, stroke, Some(""));
        }
        edit_question_key(state, stroke, action.as_deref())
    }
}

#[cfg(test)]
#[path = "presentation_tests.rs"]
mod tests;

#[cfg(test)]
mod control_semantics_tests {
    use super::*;
    use bcode_tool::InteractionInput;
    use bmux_tui::{
        buffer::Buffer,
        event::{MouseButton, MouseEvent, MouseEventKind},
        frame::Frame,
        geometry::Point,
    };

    #[test]
    fn painted_option_and_submit_semantics_share_hits_and_producer_mouse_transitions() {
        for control in ["radio", "checkbox"] {
            let request = crate::parse_question_request(serde_json::json!({"questions":[{
                "question":"Choose 界👩‍💻", "options":["First", "Second e\u{301}"],
                "control":control,"selection_mode":if control == "radio" { "single" } else { "multiple" },
                "custom":true,"required":true
            }]})).unwrap();
            let mut controller = QuestionInteractionController::new(request);
            let mut form = QuestionPresentation::default();
            for target in ["question-0.option-1", "question.actions.submit"] {
                let snapshot = controller.snapshot();
                let area = Rect::new(3, 4, 50, form.preferred_height(&snapshot, 50));
                let mut buffer = Buffer::empty(area);
                let mut frame = Frame::new(&mut buffer);
                form.render_slice(&snapshot, 0, area, &mut PaintCx::new(&mut frame));
                let region = frame
                    .semantics()
                    .regions()
                    .iter()
                    .find(|region| region.id == target)
                    .expect("individual control semantics");
                let hit = frame
                    .hits()
                    .regions()
                    .iter()
                    .find(|hit| hit.id.as_str() == target)
                    .expect("same measured hit");
                assert_eq!(region.area, hit.area);
                let accessibility = region.accessibility.as_ref().unwrap();
                assert!(
                    accessibility
                        .actions
                        .contains(&bmux_tui::semantic::SemanticAction::Activate)
                );
                let event = Event::Mouse(MouseEvent::new(
                    MouseEventKind::Down(MouseButton::Left),
                    Point::new(region.area.x, region.area.y),
                ));
                let QuestionInputResult::Semantic(input) =
                    form.input(&event, &snapshot, &DefaultQuestionInput)
                else {
                    panic!("producer control input");
                };
                let InteractionInput::Activate { control_id } = &input else {
                    panic!("activation");
                };
                assert_eq!(
                    control_id.as_str(),
                    if target == "question.actions.submit" {
                        "submit"
                    } else {
                        target
                    }
                );
                let output = controller.handle_input_at_revision(snapshot.revision, input);
                if target == "question-0.option-1" {
                    assert_eq!(controller.snapshot().selected_option_indices[0], vec![1]);
                } else {
                    assert!(matches!(output, InteractionOutput::Submitted { .. }));
                }
            }
        }
    }

    #[test]
    fn clipped_controls_publish_only_the_same_visible_source_geometry() {
        let request = crate::parse_question_request(serde_json::json!({"questions":[{
            "question":"Choose", "options":["First", "Second"],"required":true
        }]}))
        .unwrap();
        let snapshot = QuestionInteractionController::new(request).snapshot();
        let mut form = QuestionPresentation::default();
        let area = Rect::new(2, 5, 40, 1);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        form.render_slice(&snapshot, 3, area, &mut PaintCx::new(&mut frame));
        let options: Vec<_> = frame
            .semantics()
            .regions()
            .iter()
            .filter(|region| region.id.starts_with("question-0.option-"))
            .collect();
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].id, "question-0.option-1");
        assert_eq!(options[0].area, area);
        let hit = frame
            .hits()
            .regions()
            .iter()
            .find(|hit| hit.id.as_str() == options[0].id)
            .unwrap();
        assert_eq!(hit.area, area);
        assert!(
            !frame
                .semantics()
                .regions()
                .iter()
                .any(|region| region.id == "question.actions.submit")
        );
    }
}
