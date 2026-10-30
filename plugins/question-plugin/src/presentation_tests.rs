//! Behavior tests that also run in the SDK-free presentation feature graph.
use super::*;
use bcode_tool::{InteractionControlId, InteractionValue};
use bmux_tui::event::{MouseButton, MouseEvent, MouseEventKind};
use bmux_tui::{buffer::Buffer, frame::Frame, geometry::Point};
use serde_json::json;

fn controller() -> QuestionInteractionController {
    QuestionInteractionController::new(crate::parse_question_request(json!({"questions":[
        {"question":"選択 👩‍💻 é", "options":["Alpha", "Beta"], "custom":true,"custom_mode":"exclusive","required":true},
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
        &Event::Paste("自分 👩‍💻 é".into()),
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
