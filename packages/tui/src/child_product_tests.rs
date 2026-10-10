//! Product-boundary child monitor tests: canonical event, projection, render and input routing.
use super::tests::root_test_model_with_history;
use bcode_session_models::{SessionEvent, SessionEventKind, SessionId};
use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
use bmux_tui::event::Event;

#[tokio::test]
async fn published_parent_card_opens_child_through_root_route_without_parent_history_noise() {
    let parent = SessionId::new();
    let child = SessionId::new();
    let history = [
        SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id: parent,
            provenance: None,
            kind: SessionEventKind::SessionChildLinked { child_id: child },
        },
        SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 2,
            timestamp_ms: 2,
            session_id: parent,
            provenance: None,
            kind: SessionEventKind::SessionChildPublished {
                child_id: child,
                title: Some("子 coordinator 👩‍💻 e\u{301}".to_owned()),
            },
        },
    ];
    let mut model = root_test_model_with_history(parent, &history);
    assert_eq!(
        model
            .chat
            .app
            .session_view_snapshot()
            .transcript
            .items
            .len(),
        1
    );
    let area = bmux_tui::geometry::Rect::new(0, 0, 80, 18);
    let mut buffer = bmux_tui::buffer::Buffer::empty(area);
    let mut frame = bmux_tui::frame::Frame::new(&mut buffer);
    crate::render::render(
        &mut model.chat.app,
        &mut bmux_tui::paint::PaintCx::new(&mut frame),
    );
    let link = frame.hits().regions().iter().find(|region| {
        let Some(id) = crate::markdown_interaction::contribution_id_from_hit(region.id.as_str()) else { return false; };
        matches!(model.chat.app.visible_markdown_contribution(id), Some(bcode_markdown_render::MarkdownContributionKind::Link { destination: bcode_markdown_render::MarkdownDestination::Fragment(fragment), .. }) if fragment == &format!("bcode-session-{child}"))
    }).expect("rendered child card has a visible internal-navigation mouse target");
    let rect = link.area;
    model.committed_hits = frame.hits().clone();
    model.route_terminal_event(Event::Mouse(bmux_tui::event::MouseEvent::new(
        bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
        bmux_tui::geometry::Point::new(rect.x, rect.y),
    )));
    assert_eq!(model.chat.opening_session_id(), Some(child));
    for width in [0, 1, 2, 3, 8, 20, 80] {
        let area = bmux_tui::geometry::Rect::new(0, 0, width, 18);
        let mut buffer = bmux_tui::buffer::Buffer::empty(area);
        let mut frame = bmux_tui::frame::Frame::new(&mut buffer);
        crate::render::render(
            &mut model.chat.app,
            &mut bmux_tui::paint::PaintCx::new(&mut frame),
        );
        // Tiny viewports may contain no whole visible link grapheme. Any retained
        // mouse target must remain within the same painted viewport geometry.
        for hit in frame.hits().regions().iter().filter(|hit| {
            crate::markdown_interaction::contribution_id_from_hit(hit.id.as_str()).is_some()
        }) {
            assert_eq!(hit.area.intersection(area), hit.area);
            assert!(!hit.area.is_empty());
        }
    }
    drop(model);
}

#[tokio::test]
async fn monitor_explicit_focus_escape_returns_composer_and_typing_is_not_swallowed() {
    let mut model = root_test_model_with_history(SessionId::new(), &[]);
    let ctrl_g = KeyStroke::with_modifiers(
        KeyCode::Char('g'),
        Modifiers {
            ctrl: true,
            ..Modifiers::NONE
        },
    );
    model.route_terminal_event(Event::Key(ctrl_g));
    assert!(model.chat.app.child_panel_focused());
    model.route_terminal_event(Event::Key(KeyStroke::simple(KeyCode::Escape)));
    assert!(!model.chat.app.child_panel_focused());
    assert!(model.chat.app.child_panel_open());
    model.route_terminal_event(Event::Key(KeyStroke::simple(KeyCode::Char('n'))));
    assert_eq!(model.chat.app.composer().text(), "n");
    drop(model);
}
