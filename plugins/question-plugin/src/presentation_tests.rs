//! Behavior tests that also run in the SDK-free presentation feature graph.
use super::*;
use bcode_tool::{InteractionControlId, InteractionInput, InteractionValue};
use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::{buffer::Buffer, frame::Frame, geometry::Point};
use serde_json::json;

fn controller() -> QuestionInteractionController {
    QuestionInteractionController::new(crate::parse_question_request(json!({"questions":[
        {"question":"選択 👩‍💻 e\u{301}", "options":["Alpha", "Beta"], "custom":true,"custom_mode":"exclusive","required":true},
        {"question":"Checkbox", "options":["One","Two"],"control":"checkbox","selection_mode":"multiple","custom":true,"custom_mode":"additional"}
    ]})).unwrap())
}
fn key(code: KeyCode) -> Event {
    Event::Key(KeyStroke::simple(code))
}
fn paint(
    form: &mut QuestionPresentation,
    snapshot: &QuestionSnapshot,
    area: Rect,
    offset: u16,
) -> Buffer {
    let mut buffer = Buffer::empty(area);
    let mut backend = Frame::new(&mut buffer);
    form.render_slice(snapshot, offset, area, &mut PaintCx::new(&mut backend));
    buffer
}
#[cfg(feature = "static-bundled")]
#[test]
fn rejected_fork_preserves_native_focused_field_identity_and_geometry() {
    use bcode_plugin_sdk::tui::TerminalInteractionRenderer;
    let mut controller = QuestionInteractionController::new(
        crate::parse_question_request(
            json!({"questions":[{"question":"Describe", "required":true}]}),
        )
        .unwrap(),
    );
    controller.handle_input(InteractionInput::Change {
        control_id: InteractionControlId::new("question-0.custom"),
        value: InteractionValue::String("界👩‍💻".into()),
    });
    let snapshot = controller.snapshot();
    let mut committed = QuestionPresentation::default();
    paint(&mut committed, &snapshot, Rect::new(2, 3, 40, 12), 0);
    let before = committed
        .renderer
        .focused_text_field(&snapshot)
        .expect("visible field");
    let mut candidate = committed.fork();
    assert_eq!(
        candidate.renderer.focused_text_field(&snapshot),
        Some(before.clone())
    );
    paint(&mut candidate, &snapshot, Rect::new(0, 0, 0, 0), 100);
    drop(candidate);
    assert_eq!(
        committed.renderer.focused_text_field(&snapshot),
        Some(before)
    );
}

#[test]
fn rejected_candidate_paint_preserves_committed_editor_history_selection_and_hits() {
    let mut controller = QuestionInteractionController::new(
        crate::parse_question_request(
            json!({"questions":[{"question":"Describe", "required":true}]}),
        )
        .unwrap(),
    );
    let mut committed = QuestionPresentation::default();
    let area = Rect::new(2, 3, 40, 12);
    let initial = controller.snapshot();
    paint(&mut committed, &initial, area, 0);
    committed.handle_event(
        &Event::Paste("界👩‍💻e\u{301}".into()),
        &mut controller,
        &DefaultQuestionInput,
    );
    let selected = Event::Key(bmux_keyboard::parse_key_stroke("shift+left").unwrap());
    committed.handle_event(&selected, &mut controller, &DefaultQuestionInput);
    let checkpoint = controller.snapshot();
    paint(&mut committed, &checkpoint, area, 0);
    let mut candidate = committed.fork();
    let mut candidate_controller =
        QuestionInteractionController::from_snapshot(checkpoint.clone()).unwrap();
    candidate.handle_event(
        &Event::Paste("discarded".into()),
        &mut candidate_controller,
        &DefaultQuestionInput,
    );
    let candidate_snapshot = candidate_controller.snapshot();
    paint(
        &mut candidate,
        &candidate_snapshot,
        Rect::new(0, 0, 1, 1),
        5,
    );
    drop(candidate); // Host rejects the candidate frame; committed state was never touched.
    committed.handle_event(
        &Event::Paste("X".into()),
        &mut controller,
        &DefaultQuestionInput,
    );
    assert_eq!(
        controller.snapshot().answers[0].custom.as_deref(),
        Some("界👩‍💻X")
    );
    committed.handle_event(
        &Event::Key(bmux_keyboard::parse_key_stroke("ctrl+z").unwrap()),
        &mut controller,
        &DefaultQuestionInput,
    );
    assert_eq!(
        controller.snapshot().answers[0].custom.as_deref(),
        Some("界👩‍💻e\u{301}")
    );
    committed.handle_event(
        &Event::Key(bmux_keyboard::parse_key_stroke("ctrl+y").unwrap()),
        &mut controller,
        &DefaultQuestionInput,
    );
    assert_eq!(
        controller.snapshot().answers[0].custom.as_deref(),
        Some("界👩‍💻X")
    );

    let controller = self::controller();
    let snapshot = controller.snapshot();
    let mut committed = QuestionPresentation::default();
    paint(&mut committed, &snapshot, Rect::new(2, 3, 60, 20), 0);
    let mut candidate = committed.fork();
    paint(&mut candidate, &snapshot, Rect::new(0, 0, 0, 0), 100);
    drop(candidate);
    let input = committed.input(
        &Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(4, 5),
        )),
        &snapshot,
        &DefaultQuestionInput,
    );
    let QuestionInputResult::Semantic(InteractionInput::Activate { control_id }) = input else {
        panic!("committed option hit must survive rejected paint");
    };
    assert_eq!(control_id.as_str(), "question-0.option-0");
}

#[test]
fn resolved_editor_actions_move_select_delete_insert_and_consume_unknown() {
    use bmux_text_edit::TextEditBuffer;
    use bmux_tui_components::text_input::TextInputState;
    let mut state = TextInputState::new(TextEditBuffer::from_text("界👩‍💻e\u{301}"));
    state
        .buffer_mut()
        .move_cursor(bmux_text_edit::TextMotion::End);
    let shortcut = KeyStroke::simple(KeyCode::Char('x'));
    assert!(edit_question_key(
        &mut state,
        shortcut,
        Some("tui.control.select_left")
    ));
    assert_eq!(state.buffer().text(), "界👩‍💻e\u{301}");
    assert!(edit_question_key(
        &mut state,
        KeyStroke::simple(KeyCode::Char('λ')),
        None
    ));
    assert_eq!(state.buffer().text(), "界👩‍💻λ");
    assert!(edit_question_key(
        &mut state,
        shortcut,
        Some("question.text.undo")
    ));
    assert_eq!(state.buffer().text(), "界👩‍💻e\u{301}");
    assert!(edit_question_key(
        &mut state,
        shortcut,
        Some("question.text.redo")
    ));
    assert_eq!(state.buffer().text(), "界👩‍💻λ");
    assert!(edit_question_key(
        &mut state,
        shortcut,
        Some("tui.control.line_start")
    ));
    assert!(edit_question_key(
        &mut state,
        shortcut,
        Some("tui.control.delete")
    ));
    assert_eq!(state.buffer().text(), "👩‍💻λ");
    for action in ["", "unknown", "tui.control.submit"] {
        assert!(edit_question_key(&mut state, shortcut, Some(action)));
        assert_eq!(state.buffer().text(), "👩‍💻λ");
    }
    assert!(edit_question_key(
        &mut state,
        shortcut,
        Some("tui.control.line_end")
    ));
    assert!(edit_question_key(
        &mut state,
        shortcut,
        Some("tui.control.newline")
    ));
    assert_eq!(state.buffer().text(), "👩‍💻λ\n");
}

#[cfg(feature = "static-bundled")]
#[test]
fn all_resolved_editor_actions_match_native_control_behavior() {
    use bcode_plugin_sdk::tui::keymap::controls::ControlAction;
    use bmux_text_edit::TextEditBuffer;
    use bmux_tui_components::text_input::{TextInputControl, TextInputPolicy, TextInputState};
    for action in ControlAction::TEXT
        .iter()
        .copied()
        .chain([ControlAction::Newline])
    {
        let mut expected =
            TextInputState::new(TextEditBuffer::from_text("界👩‍💻e\u{301} word\nnext"));
        expected
            .buffer_mut()
            .move_cursor(bmux_text_edit::TextMotion::WordRight);
        let mut actual = expected.clone();
        TextInputControl::new(&TextInputPolicy::chat_composer())
            .handle_key(&mut expected, action.native_stroke());
        assert!(edit_question_key(
            &mut actual,
            KeyStroke::simple(KeyCode::Char('x')),
            Some(&action.id())
        ));
        assert_eq!(
            actual.buffer().text(),
            expected.buffer().text(),
            "{action:?}"
        );
        // Replacing the active selection also checks cursor/selection correspondence.
        actual.buffer_mut().insert_char('X');
        expected.buffer_mut().insert_char('X');
        assert_eq!(
            actual.buffer().text(),
            expected.buffer().text(),
            "{action:?}"
        );
    }
}

#[test]
fn clipped_custom_editor_click_replaces_at_the_same_unicode_source_position() {
    let mut controller = QuestionInteractionController::new(
        crate::parse_question_request(
            json!({"questions":[{"question":"Describe", "required":true}]}),
        )
        .unwrap(),
    );
    controller.handle_input(InteractionInput::Change {
        control_id: InteractionControlId::new("question-0.custom"),
        value: InteractionValue::String("一行\n二行 👩‍💻\n三行 e\u{301}".into()),
    });
    let snapshot = controller.snapshot();
    let mut full_form = QuestionPresentation::default();
    let height = full_form.preferred_height(&snapshot, 40);
    for offset in 3..6 {
        let mut full_controller =
            QuestionInteractionController::from_snapshot(snapshot.clone()).unwrap();
        let mut slice_controller =
            QuestionInteractionController::from_snapshot(snapshot.clone()).unwrap();
        let mut full_form = QuestionPresentation::default();
        let mut slice_form = QuestionPresentation::default();
        paint(&mut full_form, &snapshot, Rect::new(0, 0, 40, height), 0);
        paint(&mut slice_form, &snapshot, Rect::new(3, 5, 40, 1), offset);
        full_form.handle_event(
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(3, offset),
            )),
            &mut full_controller,
            &DefaultQuestionInput,
        );
        slice_form.handle_event(
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(6, 5),
            )),
            &mut slice_controller,
            &DefaultQuestionInput,
        );
        full_form.handle_event(
            &Event::Paste("X".into()),
            &mut full_controller,
            &DefaultQuestionInput,
        );
        slice_form.handle_event(
            &Event::Paste("X".into()),
            &mut slice_controller,
            &DefaultQuestionInput,
        );
        assert_eq!(
            slice_controller.snapshot().answers,
            full_controller.snapshot().answers
        );
    }
}

#[test]
fn checkpoint_reconstruction_preserves_open_and_closed_state_and_rejects_corruption() {
    let mut original = controller();
    original.handle_input(InteractionInput::Change {
        control_id: InteractionControlId::new("question-0.custom"),
        value: InteractionValue::String("説明 👩‍💻 e\u{301}".into()),
    });
    original.handle_input(InteractionInput::Activate {
        control_id: InteractionControlId::new("question-1.option-1"),
    });
    let checkpoint = original.snapshot();
    let mut restored = QuestionInteractionController::from_snapshot(checkpoint.clone()).unwrap();
    assert_eq!(restored.snapshot(), checkpoint);
    assert_eq!(
        restored.handle_input_at_revision(checkpoint.revision - 1, InteractionInput::Cancel),
        InteractionOutput::None
    );
    let result = restored.handle_input_at_revision(checkpoint.revision, InteractionInput::Submit);
    assert!(matches!(result, InteractionOutput::Submitted { .. }));
    let terminal = restored.snapshot();
    assert!(terminal.closed);
    let mut restored = QuestionInteractionController::from_snapshot(terminal.clone()).unwrap();
    assert_eq!(
        restored.handle_input(InteractionInput::Cancel),
        InteractionOutput::None
    );
    assert_eq!(restored.snapshot(), terminal);
    let mut bad = checkpoint.clone();
    bad.answers.clear();
    assert!(QuestionInteractionController::from_snapshot(bad).is_err());
    let mut bad = checkpoint.clone();
    bad.selected_option_indices[1] = vec![100];
    assert!(QuestionInteractionController::from_snapshot(bad).is_err());
    let mut bad = checkpoint;
    bad.focused_control_id = InteractionControlId::new("spoofed");
    assert!(QuestionInteractionController::from_snapshot(bad).is_err());
}

#[test]
fn nonzero_slice_cells_and_option_hits_are_the_same_logical_rows() {
    let controller = controller();
    let snapshot = controller.snapshot();
    let mut form = QuestionPresentation::default();
    let height = form.preferred_height(&snapshot, 60);
    let full = paint(&mut form, &snapshot, Rect::new(0, 0, 60, height), 0);
    for offset in 1..height {
        let area = Rect::new(3, 5, 60, 1);
        let slice = paint(&mut form, &snapshot, area, offset);
        for x in 0..60 {
            assert_eq!(
                slice.get(Point::new(3 + x, 5)),
                full.get(Point::new(x, offset))
            );
        }
        let click = Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(5, 5),
        ));
        let sliced = form.input(&click, &snapshot, &DefaultQuestionInput);
        paint(&mut form, &snapshot, Rect::new(0, 0, 60, height), 0);
        let original = form.input(
            &Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Left),
                Point::new(2, offset),
            )),
            &snapshot,
            &DefaultQuestionInput,
        );
        match (sliced, original) {
            // Custom fields can focus instead of activate; compare producer identities too.
            (
                QuestionInputResult::Semantic(InteractionInput::Activate { control_id: a }),
                QuestionInputResult::Semantic(InteractionInput::Activate { control_id: b }),
            )
            | (
                QuestionInputResult::Semantic(InteractionInput::Focus { control_id: a }),
                QuestionInputResult::Semantic(InteractionInput::Focus { control_id: b }),
            ) => assert_eq!(a, b),
            (QuestionInputResult::Ignored, QuestionInputResult::Ignored)
            | (QuestionInputResult::Consumed, QuestionInputResult::Consumed) => {}
            _ => panic!("slice hit differs from full logical row"),
        }
    }
}

#[test]
fn stale_translations_and_terminal_outcomes_cannot_mutate_answers() {
    for cancel in [false, true] {
        let mut controller = controller();
        let stale = controller.snapshot().revision;
        controller.handle_input(InteractionInput::Activate {
            control_id: InteractionControlId::new("question-0.option-0"),
        });
        let selected = controller.snapshot();
        assert_eq!(
            controller.handle_input_at_revision(stale, InteractionInput::Cancel),
            InteractionOutput::None
        );
        assert_eq!(controller.snapshot(), selected);
        let result = controller.handle_input_at_revision(
            selected.revision,
            if cancel {
                InteractionInput::Cancel
            } else {
                InteractionInput::Submit
            },
        );
        let InteractionOutput::Submitted { payload } = result else {
            panic!("expected producer completion proposal");
        };
        if cancel {
            assert_eq!(payload["status"], "dismissed");
        }
        let terminal = controller.snapshot();
        // Serialized native/dynamic service state must preserve the same closure.
        let mut restored: QuestionInteractionController =
            serde_json::from_value(serde_json::to_value(&controller).unwrap()).unwrap();
        for controller in [&mut controller, &mut restored] {
            for input in [
                InteractionInput::Activate {
                    control_id: InteractionControlId::new("question-0.option-1"),
                },
                InteractionInput::Submit,
                InteractionInput::Cancel,
            ] {
                assert_eq!(controller.handle_input(input), InteractionOutput::None);
                assert_eq!(controller.snapshot(), terminal);
            }
        }
    }
}

#[test]
fn mouse_requires_geometry_committed_for_the_current_snapshot() {
    let mut controller = controller();
    let mut form = QuestionPresentation::default();
    let before = controller.snapshot();
    let area = Rect::new(3, 4, 60, form.preferred_height(&before, 60));
    paint(&mut form, &before, area, 0);
    let click = Event::Mouse(MouseEvent::new(
        MouseEventKind::Down(MouseButton::Left),
        Point::new(5, 6),
    ));
    assert!(matches!(
        form.input(&click, &before, &DefaultQuestionInput),
        QuestionInputResult::Semantic(InteractionInput::Activate { .. })
    ));
    controller.handle_input(InteractionInput::Activate {
        control_id: InteractionControlId::new("question-0.option-1"),
    });
    let current = controller.snapshot();
    assert!(matches!(
        form.input(&click, &current, &DefaultQuestionInput),
        QuestionInputResult::Ignored
    ));
    paint(&mut form, &current, area, 0);
    assert!(matches!(
        form.input(&click, &current, &DefaultQuestionInput),
        QuestionInputResult::Semantic(InteractionInput::Activate { .. })
    ));
}

#[test]
fn portable_unicode_editor_selection_replacement_and_history() {
    let mut controller = QuestionInteractionController::new(
        crate::parse_question_request(
            json!({"questions":[{"question":"Describe", "required":true}]}),
        )
        .unwrap(),
    );
    let mut form = QuestionPresentation::default();
    let snapshot = controller.snapshot();
    paint(&mut form, &snapshot, Rect::new(0, 0, 40, 12), 0);
    form.handle_event(
        &Event::Paste("説明 👩‍💻 e\u{301}".into()),
        &mut controller,
        &DefaultQuestionInput,
    );
    let original = controller.snapshot().answers[0].custom.clone();
    assert_eq!(original.as_deref(), Some("説明 👩‍💻 e\u{301}"));
    // Six graphemes: two CJK, space, one ZWJ emoji, space, one combining cluster.
    for _ in 0..6 {
        form.handle_event(
            &Event::Key(bmux_keyboard::parse_key_stroke("shift+left").unwrap()),
            &mut controller,
            &DefaultQuestionInput,
        );
    }
    form.handle_event(
        &Event::Paste("X".into()),
        &mut controller,
        &DefaultQuestionInput,
    );
    assert_eq!(
        controller.snapshot().answers[0].custom.as_deref(),
        Some("X")
    );
    form.handle_event(
        &Event::Key(bmux_keyboard::parse_key_stroke("ctrl+z").unwrap()),
        &mut controller,
        &DefaultQuestionInput,
    );
    assert_eq!(controller.snapshot().answers[0].custom, original);
    form.handle_event(
        &Event::Key(bmux_keyboard::parse_key_stroke("ctrl+y").unwrap()),
        &mut controller,
        &DefaultQuestionInput,
    );
    assert_eq!(
        controller.snapshot().answers[0].custom.as_deref(),
        Some("X")
    );
}

#[test]
fn keyboard_validation_radio_custom_checkbox_and_submission() {
    let mut controller = controller();
    let mut form = QuestionPresentation::default();
    assert!(!matches!(
        controller.handle_input(InteractionInput::Submit),
        InteractionOutput::Submitted { .. }
    ));
    assert!(controller.snapshot().validation_error.is_some());
    form.handle_event(&key(KeyCode::Enter), &mut controller, &DefaultQuestionInput);
    assert_eq!(controller.snapshot().selected_option_indices[0], vec![0]);
    form.handle_event(
        &key(KeyCode::Char('2')),
        &mut controller,
        &DefaultQuestionInput,
    );
    assert_eq!(controller.snapshot().selected_option_indices[0], vec![1]);
    form.handle_event(&key(KeyCode::Tab), &mut controller, &DefaultQuestionInput);
    assert_eq!(
        controller.snapshot().focus,
        QuestionFocusTarget::Custom { question_index: 0 }
    );
    form.handle_event(
        &Event::Paste("自分 👩‍💻 e\u{301}".into()),
        &mut controller,
        &DefaultQuestionInput,
    );
    assert!(controller.snapshot().selected_option_indices[0].is_empty());
    form.handle_event(&key(KeyCode::Tab), &mut controller, &DefaultQuestionInput);
    form.handle_event(&key(KeyCode::Space), &mut controller, &DefaultQuestionInput);
    form.handle_event(&key(KeyCode::Tab), &mut controller, &DefaultQuestionInput);
    form.handle_event(&key(KeyCode::Space), &mut controller, &DefaultQuestionInput);
    assert_eq!(controller.snapshot().selected_option_indices[1], vec![0, 1]);
    let output = controller.handle_input(InteractionInput::Submit);
    let InteractionOutput::Submitted { payload } = output else {
        panic!("valid answers submit");
    };
    let crate::QuestionResolutionPayload::Answered { questions } =
        serde_json::from_value(payload).unwrap()
    else {
        panic!("answers");
    };
    let outcome = crate::answered_outcome(&controller.snapshot().request, questions).unwrap();
    assert!(crate::format_question_outcome_output(&outcome).contains("自分"));
}
#[test]
fn paint_resize_clipping_clears_stale_mouse_targets() {
    let mut controller = controller();
    let mut form = QuestionPresentation::default();
    for width in [0, 1, 2, 4, 12, 60] {
        let snapshot = controller.snapshot();
        let height = form.preferred_height(&snapshot, width);
        assert!(height > 0);
        let focus = form.focused_rows(&snapshot, width);
        assert!(focus.end <= height);
        paint(
            &mut form,
            &snapshot,
            Rect::new(0, 0, width, height.min(20)),
            0,
        );
        paint(&mut form, &snapshot, Rect::new(0, 0, 0, 0), height);
        let event = Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            Point::new(0, 2),
        ));
        assert!(matches!(
            form.input(&event, &snapshot, &DefaultQuestionInput),
            QuestionInputResult::Ignored
        ));
    }
    controller.handle_input(InteractionInput::Change {
        control_id: InteractionControlId::new("question-0.custom"),
        value: InteractionValue::String("answer".into()),
    });
    assert!(matches!(
        controller.handle_input(InteractionInput::Cancel),
        InteractionOutput::Submitted { .. }
    ));
}
#[test]
fn visible_mouse_control_uses_painted_geometry() {
    let controller = controller();
    let snapshot = controller.snapshot();
    let mut form = QuestionPresentation::default();
    let height = form.preferred_height(&snapshot, 60);
    paint(&mut form, &snapshot, Rect::new(0, 0, 60, height), 0);
    // Prompt occupies one row after the title; the first option is painted on row two.
    let event = Event::Mouse(MouseEvent::new(
        MouseEventKind::Down(MouseButton::Left),
        Point::new(2, 2),
    ));
    let QuestionInputResult::Semantic(InteractionInput::Activate { control_id }) =
        form.input(&event, &snapshot, &DefaultQuestionInput)
    else {
        panic!("visible option hit");
    };
    assert_eq!(control_id.as_str(), "question-0.option-0");
}

#[test]
fn shared_artifact_rows_preserve_selected_and_custom_outcomes() {
    let mut controller = controller();
    controller.handle_input(InteractionInput::Change {
        control_id: InteractionControlId::new("question-0.custom"),
        value: InteractionValue::String("説明 👩‍💻".into()),
    });
    let snapshot = controller.snapshot();
    let outcome = crate::answered_outcome(&snapshot.request, snapshot.answers).unwrap();
    let rows = question_outcome_rows(
        &outcome,
        bcode_tui_components::tool_card::ToolCardStyle::from_component_theme(None),
    );
    let text = rows
        .iter()
        .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
        .collect::<Vec<_>>()
        .join("");
    assert!(text.contains("説明 👩‍💻"));
    assert!(text.contains("Question outcome"));
    let dismissed = crate::dismissed_outcome(&snapshot.request);
    assert!(crate::format_question_outcome_output(&dismissed).contains("Dismissed"));
}

#[test]
fn backward_navigation_and_cancel_are_shared_keyboard_decisions() {
    let mut controller = controller();
    let mut form = QuestionPresentation::default();
    form.handle_event(
        &Event::Key(KeyStroke::with_modifiers(
            KeyCode::Tab,
            bmux_keyboard::Modifiers {
                shift: true,
                ..bmux_keyboard::Modifiers::NONE
            },
        )),
        &mut controller,
        &DefaultQuestionInput,
    );
    assert_eq!(controller.snapshot().focus, QuestionFocusTarget::Cancel);
    let InteractionOutput::Submitted { payload } = form.handle_event(
        &key(KeyCode::Escape),
        &mut controller,
        &DefaultQuestionInput,
    ) else {
        panic!("cancel outcome");
    };
    assert_eq!(payload["status"], "dismissed");
}
