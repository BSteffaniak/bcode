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
async fn monitor_slash_outcome_opens_refocuses_and_refreshes_children() {
    let session_id = SessionId::new();
    let mut model = root_test_model_with_history(session_id, &[]);
    for _ in 0..2 {
        bmux_tui_runtime::Program::update(
            &mut model,
            bmux_tui_runtime::RuntimeEvent::Message(super::BcodeRuntimeMessage::EffectCompleted(
                Box::new(crate::effects::TuiEffectResult::SlashCommandExecuted {
                    message: "/monitor".into(),
                    result: Ok(crate::slash_commands::SlashCommandOutcome::OpenChildMonitor),
                }),
            )),
        )
        .expect("monitor slash completion");
        assert!(model.chat.app.child_panel_open());
        assert!(model.chat.app.child_panel_focused());
        let (effects, _) = model.chat.pending_effects.drain_runtime();
        assert!(effects.iter().any(|(_, effect)| matches!(effect,
            crate::effects::TuiEffect::LoadSessionChildren { parent_id, .. } if *parent_id == session_id
        )));
        assert!(effects.iter().any(|(_, effect)| matches!(effect,
            crate::effects::TuiEffect::LoadChildActivity { session_id: id, .. } if *id == session_id
        )));
        model.chat.app.release_child_panel_focus();
    }
}

#[tokio::test]
async fn monitor_mouse_row_selects_and_chevron_expands_without_opening_session() {
    let root = SessionId::new();
    let child = SessionId::new();
    let mut model = root_test_model_with_history(root, &[]);
    let summary = bcode_session_models::SessionSummary {
        id: child,
        parent_id: Some(root),
        name: Some("子 👩‍💻 e\u{301}".to_owned()),
        explicit_name: Some("子 👩‍💻 e\u{301}".to_owned()),
        derived_title: None,
        automatic_title: None,
        automatic_title_source_sequence: None,
        title_source: bcode_session_models::SessionTitleSource::Explicit,
        client_count: 0,
        created_at_ms: 1,
        updated_at_ms: 2,
        working_directory: "/tmp/bcode-tui-test".into(),
        import: None,
        execution: None,
        location: None,
    };
    model.chat.app.apply_monitor_children(
        root,
        &bcode_session_models::SessionChildPage {
            children: vec![summary],
            next_after: None,
            complete: true,
            issues: vec![],
        },
    );
    model.chat.app.open_child_panel();
    let area = bmux_tui::geometry::Rect::new(0, 0, 80, 24);
    let mut buffer = bmux_tui::buffer::Buffer::empty(area);
    let mut frame = bmux_tui::frame::Frame::new(&mut buffer);
    crate::render::render(
        &mut model.chat.app,
        &mut bmux_tui::paint::PaintCx::new(&mut frame),
    );
    let row_key = format!("child-monitor:{child}");
    let expand_key = format!("child-monitor-expand:{child}");
    let row = frame
        .hits()
        .regions()
        .iter()
        .find(|hit| hit.id.as_str() == row_key)
        .expect("painted row")
        .area;
    let chevron = frame
        .hits()
        .regions()
        .iter()
        .find(|hit| hit.id.as_str() == expand_key)
        .expect("painted chevron")
        .area;
    let point = bmux_tui::geometry::Point::new(row.right() - 1, row.y);
    assert!(!chevron.contains(point));
    model.committed_hits = frame.hits().clone();
    model.chat.app.release_child_panel_focus();
    model.route_terminal_event(Event::Mouse(bmux_tui::event::MouseEvent::new(
        bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
        point,
    )));
    assert_eq!(model.chat.app.selected_monitor_session(), Some(child));
    assert!(model.chat.app.child_panel_focused());
    assert!(model.chat.app.expanded_monitor_sessions().is_empty());
    assert_eq!(model.chat.opening_session_id(), None);
    model.route_terminal_event(Event::Mouse(bmux_tui::event::MouseEvent::new(
        bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
        bmux_tui::geometry::Point::new(chevron.x, chevron.y),
    )));
    assert!(model.chat.app.expanded_monitor_sessions().contains(&child));
    assert_eq!(model.chat.opening_session_id(), None);
    model.route_terminal_event(Event::Key(KeyStroke::simple(KeyCode::Enter)));
    assert_eq!(model.chat.opening_session_id(), Some(child));
    drop(model);
}

#[tokio::test]
async fn monitor_toggle_refocuses_from_composer_and_closes_from_monitor() {
    let mut model = root_test_model_with_history(SessionId::new(), &[]);
    let toggle = KeyStroke::with_modifiers(
        KeyCode::Char('g'),
        Modifiers {
            ctrl: true,
            ..Modifiers::NONE
        },
    );
    model.route_terminal_event(Event::Key(toggle));
    assert!(model.chat.app.child_panel_open());
    assert!(model.chat.app.child_panel_focused());

    model.route_terminal_event(Event::Key(KeyStroke::simple(KeyCode::Escape)));
    assert!(model.chat.app.child_panel_open());
    assert!(!model.chat.app.child_panel_focused());
    model.route_terminal_event(Event::Key(KeyStroke::simple(KeyCode::Char('子'))));
    assert_eq!(model.chat.app.composer().text(), "子");

    model.route_terminal_event(Event::Key(toggle));
    assert!(model.chat.app.child_panel_open());
    assert!(model.chat.app.child_panel_focused());
    assert_eq!(model.chat.app.composer().text(), "子");
    model.route_terminal_event(Event::Key(toggle));
    assert!(!model.chat.app.child_panel_open());
    assert!(!model.chat.app.child_panel_focused());
    model.route_terminal_event(Event::Key(toggle));
    assert!(model.chat.app.child_panel_open());
    assert!(model.chat.app.child_panel_focused());
    assert_eq!(model.chat.app.composer().text(), "子");
    model.route_terminal_event(Event::Key(toggle));
    assert!(!model.chat.app.child_panel_open());
    assert!(!model.chat.app.child_panel_focused());
    assert_eq!(model.chat.app.composer().text(), "子");
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
