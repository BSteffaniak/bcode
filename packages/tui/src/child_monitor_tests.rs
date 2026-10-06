//! Behavioral monitor regressions.
use super::*;
use bcode_session_models::{
    SessionActivity, SessionActivityCoverage, SessionActivityPage, SessionActivityState,
    SessionChildPage,
};
use std::fmt::Write as _;

#[test]
fn monitor_expands_idle_coordinator_and_preserves_transcript() {
    let root = SessionId::new();
    let mut child = session_summary(SessionId::new());
    child.parent_id = Some(root);
    child.name = Some("协调 👩‍💻 e\u{301}".to_owned());
    let mut grandchild = session_summary(SessionId::new());
    grandchild.parent_id = Some(child.id);
    grandchild.name = Some("running grandchild".to_owned());
    let mut app = BmuxApp::new_with_history(Some(root), &[], &[], false);
    app.apply_monitor_children(
        root,
        &SessionChildPage {
            children: vec![child.clone()],
            next_after: None,
            complete: true,
            issues: vec![],
        },
    );
    app.toggle_child_panel();
    assert_eq!(app.selected_monitor_session(), Some(child.id));
    assert_eq!(app.expand_monitor_selected(), Some(child.id));
    app.apply_monitor_children(
        child.id,
        &SessionChildPage {
            children: vec![grandchild.clone()],
            next_after: Some(grandchild.id),
            complete: false,
            issues: vec!["partial catalog".to_owned()],
        },
    );
    app.apply_monitor_activity(
        child.id,
        SessionActivityPage {
            own: SessionActivity {
                evidence: None,
                session_id: child.id,
                parent_id: Some(root),
                states: vec![SessionActivityState::Idle],
            },
            descendants: vec![SessionActivity {
                evidence: None,
                session_id: grandchild.id,
                parent_id: Some(child.id),
                states: vec![SessionActivityState::Running],
            }],
            next_after: None,
            coverage: SessionActivityCoverage::Partial,
            observed_at_ms: 100,
            issues: vec![],
        },
    );
    let rows = app.child_panel_rows().join("\n");
    assert!(rows.contains("idle (not done)"));
    assert!(rows.contains("running grandchild · running"));
    assert!(app.child_activity_text().contains("degraded"));
    app.move_monitor_selection(true);
    assert_eq!(app.selected_monitor_session(), Some(grandchild.id));
    assert!(app.transcript().is_empty());
    for width in [0, 1, 2, 4, 20, 80, 3] {
        let area = Rect::new(0, 0, width, 15);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        render::render(&mut app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
        assert_eq!(app.selected_monitor_session(), Some(grandchild.id));
        for region in frame
            .hits()
            .regions()
            .iter()
            .filter(|region| region.id.as_str().starts_with("child-monitor:"))
        {
            assert!(width > 0);
            assert!(region.area.x >= area.x && region.area.right() <= area.right());
            assert!(region.area.y >= area.y && region.area.bottom() <= area.bottom());
            assert!(
                frame
                    .hits()
                    .hit_test(bmux_tui::geometry::Point::new(region.area.x, region.area.y))
                    .is_some()
            );
        }
        if width == 0 {
            assert!(frame.hits().regions().is_empty());
        }
    }
    drop(app);
}

#[test]
fn canonical_child_relationship_is_projected_once_without_parent_transcript_copy() {
    let child_id = SessionId::new();
    let parent_id = SessionId::new();
    let relationship = event(
        child_id,
        1,
        SessionEventKind::ChildSessionCreated {
            request: Box::new(bcode_session_models::SessionChildCreateRequest {
                child_id,
                parent_id,
                name: None,
                working_directory: "/tmp".into(),
            }),
        },
    );
    let mut app = BmuxApp::new_with_history(
        Some(child_id),
        std::slice::from_ref(&relationship),
        &[],
        false,
    );
    app.absorb_session_event(&relationship);
    let snapshot = app.session_view_snapshot();
    assert_eq!(snapshot.transcript.items.len(), 1);
    let bcode_session_view_models::TranscriptViewItemKind::SystemMessage { message } =
        &snapshot.transcript.items[0].kind
    else {
        panic!("relationship must be semantic transcript content");
    };
    assert!(
        message
            .text
            .contains(&format!("bcode://session/{parent_id}"))
    );
    assert!(message.text.contains("not assignment acceptance"));
    drop(app);
}

#[test]
fn return_navigation_restores_wrapped_source_position_after_resize() {
    let root = SessionId::new();
    let child = SessionId::new();
    let history = vec![event(
        root,
        1,
        SessionEventKind::SystemMessage {
            text: (0..100).fold(String::new(), |mut text, index| {
                writeln!(text, "line {index} Unicode 子 👩‍💻 e\u{301}").unwrap();
                text
            }),
        },
    )];
    let mut original = BmuxApp::new_with_history(Some(root), &history, &[], false);
    let area = Rect::new(0, 0, 40, 12);
    let mut buffer = Buffer::empty(area);
    let mut frame = Frame::new(&mut buffer);
    render::render(
        &mut original,
        &mut bmux_tui::paint::PaintCx::new(&mut frame),
    );
    original.scroll_transcript_to_row_for_test(17);
    render::render(
        &mut original,
        &mut bmux_tui::paint::PaintCx::new(&mut frame),
    );
    let before = original.stable_transcript_anchor().unwrap();
    let before_source = original
        .stable_transcript_source_position_for_test()
        .unwrap();
    let mut visiting = BmuxApp::new_with_history(Some(child), &[], &[], false);
    visiting.take_cross_session_state_from(&original);
    drop(original);
    assert_eq!(visiting.saved_reading_sequence(root), Some(1));
    let mut returned = BmuxApp::new_with_history(Some(root), &history, &[], false);
    returned.take_cross_session_state_from(&visiting);
    drop(visiting);
    returned.restore_saved_reading_position();
    render::render(
        &mut returned,
        &mut bmux_tui::paint::PaintCx::new(&mut frame),
    );
    assert_eq!(returned.stable_transcript_anchor().unwrap().1, before.1);
    let narrower = Rect::new(0, 0, 24, 12);
    let mut narrow_buffer = Buffer::empty(narrower);
    let mut narrow_frame = Frame::new(&mut narrow_buffer);
    render::render(
        &mut returned,
        &mut bmux_tui::paint::PaintCx::new(&mut narrow_frame),
    );
    assert_eq!(returned.saved_reading_sequence(root), Some(1));
    assert_eq!(
        returned.stable_transcript_anchor().unwrap().1,
        returned
            .transcript_row_for_source_position_for_test(before_source.0, before_source.1)
            .unwrap()
    );
    assert!(returned.scroll_offset() > 0);
    drop(returned);
}

#[test]
fn monitor_cache_evicts_and_reload_has_no_total_descendant_limit() {
    let root = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(root), &[], &[], false);
    let mut first = None;
    for _ in 0..6 {
        let children: Vec<_> = (0..64)
            .map(|_| {
                let mut child = session_summary(SessionId::new());
                child.parent_id = Some(root);
                child
            })
            .collect();
        first.get_or_insert_with(|| children[0].clone());
        let next_after = children.last().map(|child| child.id);
        app.apply_monitor_children(
            root,
            &SessionChildPage {
                next_after,
                children,
                complete: true,
                issues: vec![],
            },
        );
        assert_eq!(
            app.monitor_frontier_cursor(root),
            next_after,
            "eviction must not reset root continuation"
        );
    }
    app.toggle_child_panel();
    for _ in 0..300 {
        app.move_monitor_selection(true);
    }
    assert!(app.selected_monitor_session().is_some());
    let first = first.unwrap();
    app.apply_monitor_children(
        root,
        &SessionChildPage {
            children: vec![first.clone()],
            next_after: None,
            complete: true,
            issues: vec![],
        },
    );
    for _ in 0..300 {
        app.move_monitor_selection(false);
    }
    let mut found = false;
    for _ in 0..256 {
        found |= app.selected_monitor_session() == Some(first.id);
        app.move_monitor_selection(true);
    }
    drop(app);
    assert!(found, "evicted child must remain reloadable");
}

#[test]
fn child_activity_refresh_advances_cursor_and_terminal_window_restarts() {
    let root = SessionId::new();
    let later = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(root), &[], &[], false);
    let mut page = SessionActivityPage {
        own: SessionActivity {
            session_id: root,
            parent_id: None,
            states: vec![],
            evidence: None,
        },
        descendants: vec![],
        next_after: Some(later),
        coverage: SessionActivityCoverage::Partial,
        observed_at_ms: 1,
        issues: vec![],
    };
    app.apply_child_activity(Ok(page.clone()));
    let mut request = super::super::effects::TuiEffect::LoadChildActivity {
        session_id: root,
        after: None,
        generation: 0,
    };
    app.stamp_child_effect(&mut request);
    assert!(
        matches!(request, super::super::effects::TuiEffect::LoadChildActivity { after: Some(id), .. } if id == later)
    );
    page.next_after = None;
    app.apply_child_activity(Ok(page));
    app.stamp_child_effect(&mut request);
    assert!(matches!(
        request,
        super::super::effects::TuiEffect::LoadChildActivity { after: None, .. }
    ));
    drop(app);
}

#[test]
fn newer_monitor_request_rejects_stale_even_in_same_session() {
    let root = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(root), &[], &[], false);
    let mut request = super::super::effects::TuiEffect::LoadChildActivity {
        session_id: root,
        after: None,
        generation: 0,
    };
    app.stamp_child_effect(&mut request);
    let super::super::effects::TuiEffect::LoadChildActivity {
        generation: first, ..
    } = request
    else {
        unreachable!()
    };
    app.stamp_child_effect(&mut request);
    let super::super::effects::TuiEffect::LoadChildActivity {
        generation: second, ..
    } = request
    else {
        unreachable!()
    };
    assert!(!app.accepts_child_result(true, root, first));
    assert!(app.accepts_child_result(true, root, second));
    drop(app);
    let reconstructed = BmuxApp::new_with_history(Some(root), &[], &[], false);
    assert!(!reconstructed.accepts_child_result(true, root, second));
    drop(reconstructed);
}

#[test]
fn canonical_parent_intent_is_not_a_card_and_publication_retry_is_one_card() {
    let parent = SessionId::new();
    let child = SessionId::new();
    let intent = event(
        parent,
        1,
        SessionEventKind::SessionChildLinked { child_id: child },
    );
    let published = event(
        parent,
        2,
        SessionEventKind::SessionChildPublished {
            child_id: child,
            title: Some("子 [safe] 👩‍💻 e\u{301}".to_owned()),
        },
    );
    let mut app = BmuxApp::new_with_history(Some(parent), &[intent], &[], false);
    assert!(app.session_view_snapshot().transcript.items.is_empty());
    app.absorb_session_event(&published);
    let mut retry = published.clone();
    retry.sequence = 3;
    app.absorb_session_event(&retry);
    assert_eq!(app.session_view_snapshot().transcript.items.len(), 1);
    app.apply_child_activity(Ok(SessionActivityPage {
        own: SessionActivity {
            session_id: parent,
            parent_id: None,
            states: vec![SessionActivityState::Idle],
            evidence: None,
        },
        descendants: vec![SessionActivity {
            session_id: child,
            parent_id: Some(parent),
            states: vec![SessionActivityState::PermissionWait],
            evidence: None,
        }],
        next_after: None,
        coverage: SessionActivityCoverage::Complete,
        observed_at_ms: 1,
        issues: vec![],
    }));
    assert_eq!(app.session_view_snapshot().transcript.items.len(), 1);
    let bcode_session_view_models::TranscriptViewItemKind::SystemMessage { message } =
        &app.session_view_snapshot().transcript.items[0].kind
    else {
        panic!("child card semantic content");
    };
    assert!(message.text.contains("permission wait"));
    assert!(message.text.contains("stale"));
    assert!(!message.text.contains("completed"));
    drop(app);
}

#[test]
fn unrelated_session_has_no_child_strip_until_monitor_requested() {
    let mut app = BmuxApp::new_with_history(Some(SessionId::new()), &[], &[], false);
    assert!(app.child_activity_text().is_empty());
    app.toggle_child_panel();
    assert!(app.child_activity_text().contains("unknown"));
    app.toggle_child_panel();
    assert!(app.child_activity_text().is_empty());
    drop(app);
}

#[test]
fn successful_activity_refresh_clears_error_and_counts_observed_grandchildren() {
    let root = SessionId::new();
    let coordinator = SessionId::new();
    let grandchild = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(root), &[], &[], false);
    app.apply_child_activity(Err(bcode_client::ClientError::Protocol(
        "offline".to_owned(),
    )));
    assert!(app.child_activity_text().contains("stale/degraded"));
    let mut summary = session_summary(coordinator);
    summary.parent_id = Some(root);
    app.apply_monitor_children(
        root,
        &SessionChildPage {
            children: vec![summary],
            next_after: None,
            complete: true,
            issues: vec![],
        },
    );
    app.apply_child_activity(Ok(SessionActivityPage {
        own: SessionActivity {
            evidence: None,
            session_id: root,
            parent_id: None,
            states: vec![SessionActivityState::Idle],
        },
        descendants: vec![SessionActivity {
            evidence: None,
            session_id: coordinator,
            parent_id: Some(root),
            states: vec![SessionActivityState::Idle],
        }],
        next_after: None,
        coverage: SessionActivityCoverage::Complete,
        observed_at_ms: 1,
        issues: vec![],
    }));
    assert!(!app.child_activity_text().contains("offline"));
    app.apply_monitor_activity(
        coordinator,
        SessionActivityPage {
            own: SessionActivity {
                evidence: None,
                session_id: coordinator,
                parent_id: Some(root),
                states: vec![SessionActivityState::Idle],
            },
            descendants: vec![SessionActivity {
                evidence: None,
                session_id: grandchild,
                parent_id: Some(coordinator),
                states: vec![SessionActivityState::Running],
            }],
            next_after: None,
            coverage: SessionActivityCoverage::Complete,
            observed_at_ms: 2,
            issues: vec![],
        },
    );
    let text = app.child_activity_text();
    drop(app);
    assert!(text.contains("1 known active observed subtree"));
    assert!(text.contains("partial"));
}

#[test]
fn foreign_child_response_does_not_populate_new_session_monitor() {
    let root = SessionId::new();
    let foreign = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(root), &[], &[], false);
    app.apply_monitor_children(
        foreign,
        &SessionChildPage {
            children: vec![session_summary(SessionId::new())],
            next_after: None,
            complete: true,
            issues: vec![],
        },
    );
    assert_eq!(app.selected_monitor_session(), None);
    drop(app);
}
