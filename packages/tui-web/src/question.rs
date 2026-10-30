//! Shared producer-owned question form, driven only by canonical snapshots.
use bcode_question_plugin::presentation::{
    DefaultQuestionInput, QuestionInputResult, QuestionPresentation, QuestionSnapshot,
};
use bcode_session_view_models::InteractionViewSummary;
use bmux_tui::{event::Event, geometry::Rect, paint::PaintCx};

#[derive(Default)]
pub struct QuestionSurface {
    presentation: QuestionPresentation,
    accepted: Option<(InteractionViewSummary, QuestionSnapshot)>,
    committed: Option<InteractionViewSummary>,
    inflight: bool,
}

struct EffectiveQuestionPort<'a>(&'a crate::policy::PolicyBindings);
impl bcode_question_plugin::presentation::QuestionInputPort for EffectiveQuestionPort<'_> {
    fn keymap_action(&self, scope: &str, stroke: bmux_keyboard::KeyStroke) -> Option<String> {
        if self.0.policy.is_none() {
            return bcode_question_plugin::presentation::QuestionInputPort::keymap_action(
                &DefaultQuestionInput,
                scope,
                stroke,
            );
        }
        match self.0.resolve(scope, stroke) {
            Some(crate::policy::BrowserResolvedBinding::Host { action }) => Some(action),
            _ => None,
        }
    }
    fn keymap_keys(&self, scope: &str, action: &str) -> Vec<String> {
        let Some(policy) = &self.0.policy else {
            return bcode_question_plugin::presentation::QuestionInputPort::keymap_keys(
                &DefaultQuestionInput,
                scope,
                action,
            );
        };
        policy.bindings.iter().filter(|binding| binding.scope == scope && matches!(&binding.binding, crate::policy::BrowserResolvedBinding::Host { action: id } if id == action))
            .map(|binding| binding.chord.iter().map(|key| format!("{}{}{}{}{}", if key.ctrl {"ctrl+"} else {""}, if key.alt {"alt+"} else {""}, if key.shift {"shift+"} else {""}, if key.meta {"meta+"} else {""}, key.key)).collect::<Vec<_>>().join(" ")).collect()
    }
    fn edit_key(
        &self,
        state: &mut bmux_tui_components::text_input::TextInputState,
        stroke: bmux_keyboard::KeyStroke,
    ) -> bool {
        if self.0.policy.is_none() {
            return bcode_question_plugin::presentation::QuestionInputPort::edit_key(
                &DefaultQuestionInput,
                state,
                stroke,
            );
        }
        let Some(binding) = self.0.resolve("question.text", stroke) else {
            return false;
        };
        match binding.session() {
            bcode_tui_components::session_ui::bindings::SessionKeyBinding::Selection(motion) => {
                state
                    .buffer_mut()
                    .move_cursor_with_selection(motion, bmux_text_edit::SelectionMode::Extend);
                true
            }
            binding => {
                bcode_tui_components::session_ui::bindings::apply_editor_binding(state, binding)
                    .is_some()
            }
        }
    }
}

impl QuestionSurface {
    pub fn prepare(&mut self, bindings: &crate::policy::PolicyBindings) {
        self.presentation.set_theme(
            bindings
                .policy
                .as_ref()
                .map(crate::policy::BrowserPolicy::component_theme),
        );
        self.presentation.prepare(&EffectiveQuestionPort(bindings));
    }
    pub fn accept(&mut self, interactions: &[InteractionViewSummary]) {
        let schema_valid = |interaction: &&InteractionViewSummary| {
            !interaction.resolved
                && interaction.producer_id.as_deref() == Some("bcode.question")
                && interaction.exchange_schema.as_deref() == Some("bcode.question.request")
                && interaction.exchange_schema_version == Some(1)
        };
        let next = interactions
            .iter()
            .find(schema_valid)
            .and_then(|interaction| {
                let snapshot =
                    serde_json::from_value::<QuestionSnapshot>(interaction.snapshot.clone()?)
                        .ok()?;
                // Reuse the producer's bounded request parser, never a browser
                // interpretation of question count, option or text limits.
                bcode_question_plugin::parse_question_request(
                    serde_json::to_value(&snapshot.request).ok()?,
                )
                .ok()?;
                Some((interaction.clone(), snapshot))
            });
        if self.accepted.as_ref().map(|(interaction, _)| interaction)
            != next.as_ref().map(|(interaction, _)| interaction)
        {
            self.presentation = QuestionPresentation::default();
            self.inflight = false;
        }
        self.accepted = next;
    }

    pub const fn active(&self) -> bool {
        self.accepted.is_some()
    }

    pub fn paint(&mut self, area: Rect, cx: &mut PaintCx<'_, '_>) -> bool {
        // Renderer hit/cursor caches are updated during paint. Until output
        // commits they cannot be used with the previous visible scene.
        self.committed = None;
        let Some((_, snapshot)) = &self.accepted else {
            return false;
        };
        let height = self.presentation.preferred_height(snapshot, area.width);
        let focused = self.presentation.focused_rows(snapshot, area.width);
        let offset = focused
            .end
            .saturating_sub(area.height)
            .min(height.saturating_sub(area.height));
        self.presentation.render_slice(snapshot, offset, area, cx);
        true
    }

    pub fn commit(&mut self, visible: bool) {
        self.committed = visible
            .then(|| {
                self.accepted
                    .as_ref()
                    .map(|(interaction, _)| interaction.clone())
            })
            .flatten();
    }

    pub fn acknowledge(&mut self) {
        self.inflight = false;
        self.committed = None;
    }

    pub fn input(
        &mut self,
        event: &Event,
        bindings: &crate::policy::PolicyBindings,
    ) -> Option<serde_json::Value> {
        let (interaction, snapshot) = self.accepted.as_ref()?;
        if self.inflight || self.committed.as_ref() != Some(interaction) {
            return None;
        }
        match self
            .presentation
            .input(event, snapshot, &EffectiveQuestionPort(bindings))
        {
            QuestionInputResult::Semantic(input) => {
                let input = serde_json::to_value(input).ok()?;
                // Tick and host-owned final resolution are never browser actions.
                if input.get("type").and_then(serde_json::Value::as_str) == Some("tick") {
                    return None;
                }
                self.inflight = true;
                Some(
                    serde_json::json!({"type":"control", "action_json": serde_json::json!({
                    "type":"interaction", "interaction_id": interaction.interaction_id, "input": input,
                }).to_string()}),
                )
            }
            QuestionInputResult::Consumed | QuestionInputResult::Ignored => None,
        }
    }
}
