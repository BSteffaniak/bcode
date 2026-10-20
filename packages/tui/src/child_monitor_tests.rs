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
    child.explicit_name.clone_from(&child.name);
    let mut grandchild = session_summary(SessionId::new());
    grandchild.parent_id = Some(child.id);
    grandchild.name = Some("running grandchild".to_owned());
    grandchild.explicit_name.clone_from(&grandchild.name);
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
    let presentation = app.monitor_presentation();
    let coordinator = presentation
        .rows
        .iter()
        .find(|row| row.session_id == child.id)
        .expect("loaded coordinator");
    let descendant = presentation
        .rows
        .iter()
        .find(|row| row.session_id == grandchild.id)
        .expect("expanded descendant");
    assert_eq!(coordinator.depth, 0);
    assert!(coordinator.expanded);
    assert_eq!(coordinator.states, vec![SessionActivityState::Idle]);
    assert_eq!(descendant.depth, 1);
    assert_eq!(descendant.states, vec![SessionActivityState::Running]);
    assert!(presentation.load_more);
    assert!(app.child_activity_text().contains("degraded"));
    app.move_monitor_selection(true);
    assert_eq!(app.selected_monitor_session(), Some(grandchild.id));
    assert!(app.transcript().is_empty());
    assert_monitor_resize_preserves_selection(&mut app, grandchild.id);
    drop(app);
}

fn assert_monitor_resize_preserves_selection(app: &mut BmuxApp, selected: SessionId) {
    for (width, height) in [
        (0, 15),
        (1, 15),
        (2, 15),
        (4, 15),
        (20, 15),
        (80, 15),
        (80, 0),
        (80, 1),
        (80, 2),
        (20, 4),
        (3, 15),
        (80, 15),
    ] {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        let mut frame = Frame::new(&mut buffer);
        render::render(app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
        assert_eq!(app.selected_monitor_session(), Some(selected));
        for region in frame
            .hits()
            .regions()
            .iter()
            .filter(|region| region.id.as_str().starts_with("child-monitor:"))
        {
            assert!(width > 0 && height > 0);
            assert!(!region.area.is_empty());
            assert!(region.area.x >= area.x && region.area.right() <= area.right());
            assert!(region.area.y >= area.y && region.area.bottom() <= area.bottom());
            assert!(
                frame
                    .hits()
                    .hit_test(bmux_tui::geometry::Point::new(region.area.x, region.area.y))
                    .is_some()
            );
        }
        if width == 0 || height == 0 {
            assert!(frame.hits().regions().is_empty());
        }
    }
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
            spawning: None,
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
fn monitor_refresh_updates_attention_without_retargeting_selection() {
    let root = SessionId::new();
    let mut first = session_summary(SessionId::new());
    first.parent_id = Some(root);
    let mut second = session_summary(SessionId::new());
    second.parent_id = Some(root);
    let mut app = BmuxApp::new_with_history(Some(root), &[], &[], false);
    let mut children = SessionChildPage {
        children: vec![first.clone(), second.clone()],
        next_after: None,
        complete: true,
        issues: vec![],
    };
    app.apply_monitor_children(root, &children);
    app.open_child_panel();
    let selected = app.selected_monitor_session().expect("loaded selection");
    let other = if selected == first.id {
        second.id
    } else {
        first.id
    };
    app.apply_child_activity(Ok(SessionActivityPage {
        own: SessionActivity {
            session_id: root,
            parent_id: None,
            states: vec![SessionActivityState::Idle],
            evidence: None,
        },
        descendants: vec![
            SessionActivity {
                session_id: selected,
                parent_id: Some(root),
                states: vec![SessionActivityState::Idle],
                evidence: None,
            },
            SessionActivity {
                session_id: other,
                parent_id: Some(root),
                states: vec![SessionActivityState::PermissionWait],
                evidence: None,
            },
        ],
        next_after: None,
        coverage: SessionActivityCoverage::Complete,
        observed_at_ms: 1,
        issues: vec![],
    }));
    assert_eq!(app.selected_monitor_session(), Some(selected));
    let presentation = app.monitor_presentation();
    assert_eq!(
        presentation.details.as_ref().map(|row| row.session_id),
        Some(selected)
    );
    assert_eq!(
        presentation
            .rows
            .iter()
            .find(|row| row.session_id == other)
            .expect("observed sibling")
            .states,
        vec![SessionActivityState::PermissionWait]
    );

    children.children.reverse();
    app.apply_monitor_children(root, &children);
    assert_eq!(app.selected_monitor_session(), Some(selected));
    assert_eq!(
        app.monitor_presentation()
            .details
            .as_ref()
            .map(|row| row.session_id),
        Some(selected)
    );
    app.release_child_panel_focus();
    app.open_child_panel();
    assert_eq!(app.selected_monitor_session(), Some(selected));
    assert!(app.session_view_snapshot().transcript.items.is_empty());
    drop(app);
}

#[test]
fn monitor_typed_details_keep_own_counts_separate_from_descendant_activity() {
    let root = SessionId::new();
    let mut coordinator = session_summary(SessionId::new());
    coordinator.parent_id = Some(root);
    let grandchild = SessionId::new();
    let mut app = BmuxApp::new_with_history(Some(root), &[], &[], false);
    app.apply_monitor_children(
        root,
        &SessionChildPage {
            children: vec![coordinator.clone()],
            next_after: None,
            complete: true,
            issues: vec![],
        },
    );
    app.open_child_panel();
    let unobserved = app.monitor_presentation();
    let details = unobserved.details.expect("selected coordinator");
    assert_eq!(details.states, vec![SessionActivityState::Unknown]);
    assert!(details.evidence.is_none());
    assert_eq!(details.age, "unobserved");

    let evidence = bcode_session_models::SessionActivityEvidence {
        running: 0,
        queued: 0,
        permission_waits: 0,
        input_waits: 0,
        counts_complete: true,
        observed_event_sequence: Some(7),
        current_operation: None,
    };
    app.apply_monitor_activity(
        coordinator.id,
        SessionActivityPage {
            own: SessionActivity {
                session_id: coordinator.id,
                parent_id: Some(root),
                states: vec![SessionActivityState::Idle],
                evidence: Some(evidence.clone()),
            },
            descendants: vec![SessionActivity {
                session_id: grandchild,
                parent_id: Some(coordinator.id),
                states: vec![SessionActivityState::Running, SessionActivityState::Queued],
                evidence: None,
            }],
            next_after: Some(grandchild),
            coverage: SessionActivityCoverage::Partial,
            observed_at_ms: 1,
            issues: vec!["remaining descendants unobserved".to_owned()],
        },
    );
    let details = app.monitor_presentation().details.expect("own details");
    assert_eq!(details.session_id, coordinator.id);
    assert_eq!(details.states, vec![SessionActivityState::Idle]);
    assert_eq!(details.evidence, Some(evidence));
    assert!(details.age.contains("stale"));
    assert!(app.child_activity_text().contains("partial"));
    app.monitor_observation_failed(coordinator.id, "offline");
    assert!(app.child_activity_text().contains("stale/degraded"));
    let retained = app
        .monitor_presentation()
        .details
        .expect("retained own details");
    assert_eq!(retained.session_id, details.session_id);
    assert_eq!(retained.states, details.states);
    assert_eq!(retained.evidence, details.evidence);
    drop(app);
}

#[test]
fn monitor_painted_panel_preserves_status_color_and_visible_chevron_geometry() {
    use crate::monitor_presentation::MonitorAction;
    let (app, child) = monitor_paint_fixture();
    let theme = crate::render::TuiTheme::for_app(&app);
    let styles = monitor_test_styles(theme);
    assert_eq!(theme.raised, app.presented_theme().surfaces.raised);
    for focused in [true, false] {
        for (width, height) in [
            (80, 15),
            (20, 8),
            (8, 5),
            (1, 1),
            (0, 15),
            (80, 0),
            (80, 15),
        ] {
            let area = Rect::new(0, 0, width, height);
            let mut buffer = Buffer::empty(area);
            let hits = {
                let mut frame = Frame::new(&mut buffer);
                crate::monitor_presentation::paint(
                    &app.monitor_presentation(),
                    area,
                    focused,
                    styles,
                    &mut bmux_tui::paint::PaintCx::new(&mut frame),
                )
            };
            for hit in &hits {
                assert!(!hit.rect.is_empty());
                assert_eq!(hit.rect.intersection(area), hit.rect);
            }
            if width >= 20 && height >= 5 {
                assert_eq!(
                    buffer
                        .get(bmux_tui::geometry::Point::new(0, 0))
                        .unwrap()
                        .symbol,
                    "╭"
                );
                let row = hits
                    .iter()
                    .find(|hit| hit.action == MonitorAction::Select(child.id))
                    .expect("visible row");
                let chevron = hits
                    .iter()
                    .find(|hit| hit.action == MonitorAction::ToggleExpansion(child.id))
                    .expect("visible chevron");
                assert_eq!(chevron.rect.intersection(row.rect), chevron.rect);
                assert!(row.rect.width > chevron.rect.width);
                assert_eq!(
                    buffer
                        .get(bmux_tui::geometry::Point::new(
                            chevron.rect.x,
                            chevron.rect.y
                        ))
                        .unwrap()
                        .symbol,
                    ">"
                );
                let status = crate::monitor_presentation::status_styles(
                    theme.text,
                    theme.info,
                    theme.warning,
                    theme.error,
                    theme.muted,
                )
                .for_state(SessionActivityState::Running);
                assert_ne!(status.fg, styles.raised.bg);
                assert!(
                    (row.rect.x..row.rect.right()).any(|x| {
                        buffer
                            .get(bmux_tui::geometry::Point::new(x, row.rect.y))
                            .is_some_and(|cell| cell.style.fg == status.fg)
                    }),
                    "running color must survive selected-row highlighting"
                );
                assert_eq!(
                    buffer
                        .get(bmux_tui::geometry::Point::new(0, 0))
                        .unwrap()
                        .style
                        .bg,
                    styles.raised.bg
                );
                assert_ne!(styles.raised.bg, app.presented_theme().canvas.bg);
            }
            if area.is_empty() {
                assert!(hits.is_empty());
            }
        }
    }
    drop(app);
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

fn monitor_paint_fixture() -> (BmuxApp, bcode_session_models::SessionSummary) {
    let root = SessionId::new();
    let mut child = session_summary(SessionId::new());
    child.parent_id = Some(root);
    child.name = Some("子 👩‍💻 e\u{301}".to_owned());
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
    app.open_child_panel();
    app.apply_monitor_activity(
        child.id,
        SessionActivityPage {
            own: SessionActivity {
                session_id: child.id,
                parent_id: Some(root),
                states: vec![SessionActivityState::Running],
                evidence: None,
            },
            descendants: vec![],
            next_after: None,
            coverage: SessionActivityCoverage::Complete,
            observed_at_ms: 1,
            issues: vec![],
        },
    );
    disable_theme_transition(&mut app);
    (app, child)
}

fn monitor_test_styles(
    theme: crate::render::TuiTheme,
) -> crate::monitor_presentation::MonitorStyles {
    crate::monitor_presentation::MonitorStyles {
        text: theme.text,
        muted: theme.muted,
        border: theme.border,
        focused: theme.focused,
        raised: crate::monitor_presentation::raised_surface(
            theme.raised,
            bmux_tui::style::Style::new(),
            theme.text,
        ),
        selection: theme.selection,
        info: theme.info,
        warning: theme.warning,
        error: theme.error,
    }
}
