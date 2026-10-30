//! Shared producer-owned question form, driven only by canonical snapshots.
use bcode_question_plugin::presentation::{
    DefaultQuestionInput, QuestionInputResult, QuestionInteractionController, QuestionPresentation,
    QuestionSnapshot,
};
use bcode_session_view_models::InteractionViewSummary;
use bmux_tui::{event::Event, geometry::Rect, paint::PaintCx};

#[derive(Clone, Default)]
pub struct QuestionSurface {
    presentation: QuestionPresentation,
    accepted: Option<(InteractionViewSummary, QuestionSnapshot)>,
    committed: Option<InteractionViewSummary>,
    inflight: bool,
    pub visible: bool,
    pub offset: u16,
    pub inline_item: Option<bcode_session_view_models::TranscriptViewItemId>,
    painted_area: Rect,
    committed_area: Rect,
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
        let binding = self.0.resolve_text("question.text", stroke);
        let action = match binding.as_ref() {
            Some(crate::policy::BrowserResolvedBinding::Host { action }) => Some(action.as_str()),
            // Chord prefixes and explicit non-producer bindings are consumed,
            // not reinterpreted as literal input or native default bindings.
            Some(_) => Some(""),
            None => None,
        };
        bcode_question_plugin::presentation::edit_question_key(state, stroke, action)
    }
}

impl QuestionSurface {
    /// Fork all ephemeral input and render state for a speculative output frame.
    pub fn fork(&self) -> Self {
        Self {
            presentation: self.presentation.fork(),
            accepted: self.accepted.clone(),
            committed: self.committed.clone(),
            inflight: self.inflight,
            visible: self.visible,
            offset: self.offset,
            inline_item: self.inline_item.clone(),
            painted_area: self.painted_area,
            committed_area: self.committed_area,
        }
    }
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
                // Validate through the producer checkpoint API, never by reconstructing
                // question transitions or controller fields in the browser adapter.
                QuestionInteractionController::from_snapshot(snapshot.clone()).ok()?;
                Some((interaction.clone(), snapshot))
            });
        if self
            .accepted
            .as_ref()
            .map(|(interaction, _)| &interaction.interaction_id)
            != next
                .as_ref()
                .map(|(interaction, _)| &interaction.interaction_id)
        {
            self.presentation = QuestionPresentation::default();
            self.invalidate_geometry();
        }
        if self.accepted.as_ref().map(|(interaction, _)| interaction)
            != next.as_ref().map(|(interaction, _)| interaction)
        {
            self.inflight = false;
        }
        self.accepted = next;
    }

    pub const fn active(&self) -> bool {
        self.accepted.is_some()
    }

    pub fn height(&mut self, width: u16) -> u16 {
        self.accepted.as_ref().map_or(0, |(_, snapshot)| {
            self.presentation
                .preferred_height(snapshot, width)
                .min(4096)
        })
    }

    pub fn focused_rows(&mut self, width: u16) -> std::ops::Range<u16> {
        self.accepted.as_ref().map_or(0..0, |(_, snapshot)| {
            self.presentation.focused_rows(snapshot, width)
        })
    }

    pub fn interaction_id(&self) -> Option<&str> {
        Some(&self.accepted.as_ref()?.0.interaction_id)
    }

    pub fn invalidate_geometry(&mut self) {
        self.committed = None;
        self.visible = false;
        self.painted_area = Rect::default();
        self.committed_area = Rect::default();
    }

    pub fn paint_slice(&mut self, offset: u16, area: Rect, cx: &mut PaintCx<'_, '_>) {
        // Renderer hit/cursor caches are updated during paint; failed output
        // must not admit those caches against the previous visible scene.
        self.committed = None;
        let Some((_, snapshot)) = &self.accepted else {
            return;
        };
        self.painted_area = area;
        cx.with_child(0, 0, bmux_tui::paint::LocalRect::terminal(area), |cx| {
            self.presentation.render_slice(snapshot, offset, area, cx);
        });
    }

    pub fn commit(&mut self, visible: bool) {
        self.committed_area = if visible {
            self.painted_area
        } else {
            Rect::default()
        };
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

    pub const fn owns_pointer(&self, event: &Event) -> bool {
        match event {
            Event::Mouse(mouse) => {
                self.committed_area.contains(mouse.position)
                    && !matches!(
                        mouse.kind,
                        bmux_tui::event::MouseEventKind::ScrollUp
                            | bmux_tui::event::MouseEventKind::ScrollDown
                    )
            }
            _ => true,
        }
    }

    pub fn input(
        &mut self,
        event: &Event,
        bindings: &crate::policy::PolicyBindings,
    ) -> Option<serde_json::Value> {
        let (interaction, snapshot) = self.accepted.as_ref()?;
        if (!self.visible && matches!(event, Event::Mouse(_)))
            || self.inflight
            || self.committed.as_ref() != Some(interaction)
        {
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

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_question_plugin::presentation::QuestionInputPort;
    use bmux_keyboard::{KeyCode, KeyStroke};
    use bmux_text_edit::{TextEditBuffer, TextMotion};
    use bmux_tui_components::text_input::TextInputState;

    fn bindings() -> crate::policy::PolicyBindings {
        let mut bindings = crate::policy::PolicyBindings::default();
        let stroke = |key: &str| crate::policy::BrowserStroke {
            key: key.into(),
            ctrl: false,
            alt: false,
            shift: false,
            meta: false,
        };
        bindings.replace(serde_json::from_value(serde_json::json!({
            "schema_version":1,"revision":"question-text-test","styles":{},
            "bindings":[
                {"scope":"question.text","chord":[stroke("h")],"binding":{"type":"host","action":"tui.control.left"}},
                {"scope":"question.text","chord":[stroke("d")],"binding":{"type":"host","action":"tui.control.delete"}},
                {"scope":"question.text","chord":[stroke("u")],"binding":{"type":"host","action":"unknown"}},
                {"scope":"question.text","chord":[stroke("e")],"binding":{"type":"host","action":""}},
                {"scope":"question.text","chord":[stroke("i")],"binding":{"type":"ignored"}},
                {"scope":"question.text","chord":[stroke("g"),stroke("g")],"binding":{"type":"host","action":"tui.control.line_start"}}
            ]
        })).unwrap());
        bindings
    }

    fn custom_submit_bindings(
        key: &str,
        binding: &serde_json::Value,
    ) -> crate::policy::PolicyBindings {
        let mut bindings = crate::policy::PolicyBindings::default();
        bindings.replace(
            serde_json::from_value(serde_json::json!({
                "schema_version":1,"revision":"custom-submit-test","styles":{},
                "bindings":[{"scope":"question","chord":[{"key":key,"ctrl":true,
                    "alt":false,"shift":false,"meta":false}],"binding":binding}]
            }))
            .unwrap(),
        );
        bindings
    }

    #[test]
    fn custom_submit_obeys_effective_binding_and_committed_inflight_fences() {
        let request = bcode_question_plugin::parse_question_request(serde_json::json!({
            "questions":[{"question":"Describe","required":true}]
        }))
        .unwrap();
        let mut checkpoint =
            serde_json::to_value(QuestionInteractionController::new(request).snapshot()).unwrap();
        checkpoint["answers"][0]["custom"] = serde_json::json!("browser custom 日本語 🦀 e\u{301}");
        let snapshot: QuestionSnapshot = serde_json::from_value(checkpoint).unwrap();
        let interaction: InteractionViewSummary = serde_json::from_value(serde_json::json!({
            "interaction_id":"custom-submit","producer_id":"bcode.question",
            "exchange_schema":"bcode.question.request","exchange_schema_version":1,
            "kind":"bcode.question","required":true,"snapshot":snapshot,"resolved":false,
            "tool_call_id":null,"title":null,"resolution":null
        }))
        .unwrap();
        for (submit_key, ctrl) in [("Enter", true), ("s", true)] {
            let mut bindings = custom_submit_bindings(
                submit_key,
                &serde_json::json!({"type":"host","action":"question.submit"}),
            );
            let mut surface = QuestionSurface::default();
            surface.accept(std::slice::from_ref(&interaction));
            surface.prepare(&bindings);
            let area = Rect::new(0, 0, 60, surface.height(60));
            let mut buffer = bmux_tui::buffer::Buffer::empty(area);
            let mut frame = bmux_tui::frame::Frame::new(&mut buffer);
            surface.paint_slice(0, area, &mut PaintCx::new(&mut frame));
            surface.visible = true;
            surface.commit(true);
            let submit = Event::Key(bmux_keyboard::KeyStroke::with_modifiers(
                if submit_key == "Enter" {
                    bmux_keyboard::KeyCode::Enter
                } else {
                    bmux_keyboard::KeyCode::Char('s')
                },
                bmux_keyboard::Modifiers {
                    ctrl,
                    ..bmux_keyboard::Modifiers::NONE
                },
            ));
            bindings.begin_event();
            assert!(
                surface
                    .input(&Event::Key(KeyStroke::simple(KeyCode::Enter)), &bindings)
                    .is_none()
            );
            if submit_key != "Enter" {
                bindings.begin_event();
                assert!(
                    surface
                        .input(
                            &Event::Key(bmux_keyboard::parse_key_stroke("ctrl+enter").unwrap()),
                            &bindings
                        )
                        .is_none()
                );
            }
            bindings.begin_event();
            let intent = surface
                .input(&submit, &bindings)
                .expect("configured custom submit");
            let action: serde_json::Value =
                serde_json::from_str(intent["action_json"].as_str().unwrap()).unwrap();
            assert_eq!(action["interaction_id"], "custom-submit");
            assert_eq!(action["input"]["type"], "submit");
            let mut restored =
                QuestionInteractionController::from_snapshot(snapshot.clone()).unwrap();
            let output = serde_json::to_value(restored.handle_input_at_revision(
                snapshot.revision,
                serde_json::from_value(action["input"].clone()).unwrap(),
            ))
            .unwrap();
            assert_eq!(output["type"], "submitted");
            assert_eq!(
                output["payload"]["questions"][0]["custom"],
                "browser custom 日本語 🦀 e\u{301}"
            );
            assert!(surface.input(&submit, &bindings).is_none());
            surface.acknowledge();
            assert!(
                surface.input(&submit, &bindings).is_none(),
                "ack alone must not restore committed geometry"
            );
            surface.commit(true);
            bindings = custom_submit_bindings(submit_key, &serde_json::json!({"type":"ignored"}));
            bindings.begin_event();
            assert!(
                surface.input(&submit, &bindings).is_none(),
                "explicit unbind must not fall back"
            );
        }
    }

    #[test]
    fn failed_candidate_question_geometry_restores_hits_and_editor_cursor() {
        let request = bcode_question_plugin::parse_question_request(serde_json::json!({
            "questions":[{"question":"Describe","required":true}]
        }))
        .unwrap();
        let mut snapshot =
            serde_json::to_value(QuestionInteractionController::new(request).snapshot()).unwrap();
        snapshot["answers"][0]["custom"] = serde_json::json!("界λ");
        let snapshot =
            QuestionInteractionController::from_snapshot(serde_json::from_value(snapshot).unwrap())
                .unwrap()
                .snapshot();
        let interaction: InteractionViewSummary = serde_json::from_value(serde_json::json!({
            "interaction_id":"rollback","producer_id":"bcode.question","exchange_schema":"bcode.question.request",
            "exchange_schema_version":1,"kind":"bcode.question","required":true,"snapshot":snapshot,"resolved":false,
            "tool_call_id":null,"title":null,"resolution":null
        })).unwrap();
        let bindings = crate::policy::PolicyBindings::default();
        let mut question = QuestionSurface::default();
        question.accept(&[interaction]);
        question.prepare(&bindings);
        let height = question.height(40);
        let mut buffer = bmux_tui::buffer::Buffer::empty(Rect::new(0, 0, 40, 80));
        let mut frame = bmux_tui::frame::Frame::new(&mut buffer);
        let committed = Rect::new(0, 0, 40, height);
        question.paint_slice(0, committed, &mut PaintCx::new(&mut frame));
        question.visible = true;
        question.commit(true);
        assert!(
            question
                .input(&Event::Key(KeyStroke::simple(KeyCode::Home)), &bindings)
                .is_none()
        );
        let mut candidate_question = question.fork();
        let candidate = Rect::new(0, 40, 40, height);
        candidate_question.invalidate_geometry();
        candidate_question.paint_slice(0, candidate, &mut PaintCx::new(&mut frame));
        // The enclosing output failed: discard the producer fork without
        // repainting old pixels or resetting editor history/selection.
        drop(candidate_question);
        let mouse = |y| {
            Event::Mouse(bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(1, y),
            ))
        };
        assert!(question.owns_pointer(&mouse(1)));
        assert!(!question.owns_pointer(&mouse(41)));
        let intent = question
            .input(
                &Event::Key(KeyStroke::simple(KeyCode::Char('X'))),
                &bindings,
            )
            .unwrap();
        let action: serde_json::Value =
            serde_json::from_str(intent["action_json"].as_str().unwrap()).unwrap();
        assert_eq!(action["input"]["type"], "change");
        assert_eq!(action["input"]["value"], "X界λ");
    }

    #[test]
    fn custom_text_remaps_unknown_bindings_chords_and_literals() {
        let bindings = bindings();
        let port = EffectiveQuestionPort(&bindings);
        let mut state = TextInputState::new(TextEditBuffer::from_text("界λ"));
        state.buffer_mut().move_cursor(TextMotion::End);
        for character in ['h', 'u', 'e', 'i', 'g'] {
            bindings.begin_event();
            assert!(port.edit_key(&mut state, KeyStroke::simple(KeyCode::Char(character))));
            assert_eq!(state.buffer().text(), "界λ");
        }
        bindings.cancel_chord();
        assert!(port.edit_key(&mut state, KeyStroke::simple(KeyCode::Char('d'))));
        assert_eq!(state.buffer().text(), "界");
        bindings.begin_event();
        assert!(port.edit_key(&mut state, KeyStroke::simple(KeyCode::Char('X'))));
        assert_eq!(state.buffer().text(), "界X");
    }

    #[test]
    fn cursor_and_unknown_actions_do_not_emit_semantic_changes() {
        let bindings = bindings();
        let port = EffectiveQuestionPort(&bindings);
        let request = bcode_question_plugin::parse_question_request(serde_json::json!({
            "questions":[{"question":"Describe","required":true}]
        }))
        .unwrap();
        let controller = QuestionInteractionController::new(request);
        let snapshot = controller.snapshot();
        let original = serde_json::to_value(&snapshot).unwrap();
        let mut form = QuestionPresentation::default();
        form.prepare(&port);
        let area = Rect::new(0, 0, 40, form.preferred_height(&snapshot, 40));
        let mut buffer = bmux_tui::buffer::Buffer::empty(area);
        let mut frame = bmux_tui::frame::Frame::new(&mut buffer);
        form.render_slice(&snapshot, 0, area, &mut PaintCx::new(&mut frame));
        for character in ['h', 'u', 'e', 'i', 'g'] {
            bindings.begin_event();
            assert!(matches!(
                form.input(
                    &Event::Key(KeyStroke::simple(KeyCode::Char(character))),
                    &snapshot,
                    &port
                ),
                QuestionInputResult::Consumed
            ));
            assert_eq!(
                serde_json::to_value(controller.snapshot()).unwrap(),
                original
            );
        }
    }
}
