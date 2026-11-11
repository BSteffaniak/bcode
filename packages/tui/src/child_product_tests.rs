//! Product-boundary child monitor tests: canonical event, projection, render and input routing.
use super::tests::root_test_model_with_history;
use bcode_session_models::SessionId;
use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
use bmux_tui::event::Event;

fn child_summary(
    id: SessionId,
    parent_id: Option<SessionId>,
) -> bcode_session_models::SessionSummary {
    bcode_session_models::SessionSummary {
        id,
        parent_id,
        name: None,
        explicit_name: None,
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
    }
}

#[tokio::test]
async fn maximal_monitor_and_picker_watch_admission_preserves_every_selected_window() {
    use bcode_session_models::{SessionActivityState, SessionChildPage};
    let root = SessionId::new();
    let mut model = root_test_model_with_history(root, &[]);
    let branches: Vec<_> = (0..7)
        .map(|_| child_summary(SessionId::new(), Some(root)))
        .collect();
    model.chat.app.apply_monitor_children(
        root,
        &SessionChildPage {
            children: branches.clone(),
            next_after: None,
            complete: true,
            issues: vec![],
        },
    );
    let initial = model.desired_child_watch_scope().unwrap();
    assert_eq!(initial.children.len(), 8);
    let mut snapshot = watch_snapshot(&initial, SessionId::new(), SessionActivityState::Idle);
    snapshot.children[0].as_mut().unwrap().children = branches;
    for page in snapshot.activity.iter_mut().flatten() {
        page.next_after = Some(SessionId::new());
    }
    model
        .chat
        .app
        .apply_child_watch_snapshot(&initial, snapshot);
    let monitor = model.desired_child_watch_scope().unwrap();
    assert_eq!(monitor.activity.len(), 16);
    let mut picker = crate::session_picker::SessionPickerApp::new(Vec::new());
    let mut picker_windows = Vec::new();
    for _ in 0..4 {
        let id = SessionId::new();
        picker.retain_child_watch_window(id, None);
        picker_windows.push(bcode_session_models::SessionChildQuery {
            parent_id: id,
            after: None,
            limit: 64,
        });
    }
    model.loop_state.install_child_watch_picker_for_test(picker);
    let combined = model.desired_child_watch_scope().unwrap();
    drop(model);
    assert_eq!(combined.children.len(), 12);
    assert_eq!(combined.activity.len(), 20);
    for query in monitor.children.iter().chain(&picker_windows) {
        assert!(combined.children.contains(query));
    }
    let admitted = crate::effects::activity_watch_batches(&combined);
    assert!(admitted.len() > 1);
    let mut activity = Vec::new();
    let mut children = Vec::new();
    for scope in admitted {
        let candidates: usize = scope
            .activity
            .iter()
            .map(|target| target.query.limit + 1)
            .sum::<usize>()
            + scope
                .children
                .iter()
                .map(|query| query.limit)
                .sum::<usize>();
        assert!(candidates <= 512);
        assert!(scope.activity.len() <= 16);
        assert!(scope.children.len() <= 8);
        assert!(
            scope
                .activity
                .iter()
                .all(|target| (1..=128).contains(&target.query.limit))
        );
        assert!(
            scope
                .children
                .iter()
                .all(|query| (1..=128).contains(&query.limit))
        );
        activity.extend(scope.activity);
        children.extend(scope.children);
    }
    assert_eq!(
        activity, combined.activity,
        "no activity window starved by admission"
    );
    assert_eq!(
        children, combined.children,
        "no loaded relationship window starved by admission"
    );
}

fn watch_snapshot(
    scope: &bcode_session_models::SessionActivityWatchScope,
    child: SessionId,
    state: bcode_session_models::SessionActivityState,
) -> bcode_session_models::SessionActivityWatchSnapshot {
    use bcode_session_models::{
        SessionActivity, SessionActivityCoverage, SessionActivityPage, SessionActivityState,
        SessionChildPage,
    };
    bcode_session_models::SessionActivityWatchSnapshot {
        activity: scope
            .activity
            .iter()
            .map(|target| {
                Ok(SessionActivityPage {
                    own: SessionActivity {
                        session_id: target.session_id,
                        parent_id: None,
                        states: vec![SessionActivityState::Idle],
                        evidence: None,
                    },
                    descendants: vec![SessionActivity {
                        session_id: child,
                        parent_id: Some(target.session_id),
                        states: vec![state],
                        evidence: None,
                    }],
                    next_after: None,
                    coverage: SessionActivityCoverage::Complete,
                    observed_at_ms: 1,
                    issues: Vec::new(),
                })
            })
            .collect(),
        children: scope
            .children
            .iter()
            .map(|_| {
                Ok(SessionChildPage {
                    children: Vec::new(),
                    next_after: None,
                    complete: true,
                    issues: Vec::new(),
                })
            })
            .collect(),
    }
}

#[tokio::test]
async fn pushed_activity_updates_closed_monitor_attention_and_reset_removes_old_work() {
    use bcode_session_models::{
        SessionActivityState, SessionActivityWatchDelivery, SessionActivityWatchHandle,
        SessionActivityWatchUpdate,
    };
    let root = SessionId::new();
    let child = SessionId::new();
    let mut model = root_test_model_with_history(root, &[]);
    let scope = model.desired_child_watch_scope().expect("attached scope");
    model.child_watch_scope = Some(scope.clone());
    model.child_watch_navigation = Some(model.chat.app.child_watch_navigation());
    model.child_watch_generation = 7;
    let handle = SessionActivityWatchHandle {
        watch_id: SessionId::new(),
        epoch: SessionId::new(),
    };
    for (sequence, state, update) in [
        (
            0,
            SessionActivityState::PermissionWait,
            SessionActivityWatchUpdate::Initial,
        ),
        (
            1,
            SessionActivityState::InputWait,
            SessionActivityWatchUpdate::Changed,
        ),
        (
            2,
            SessionActivityState::Failed,
            SessionActivityWatchUpdate::Reset,
        ),
    ] {
        let snapshot = watch_snapshot(&scope, child, state);
        bmux_tui_runtime::Program::update(
            &mut model,
            bmux_tui_runtime::RuntimeEvent::Message(super::BcodeRuntimeMessage::ChildWatch {
                generation: 7,
                result: Ok(SessionActivityWatchDelivery {
                    handle,
                    sequence,
                    update,
                    snapshot,
                }),
            }),
        )
        .expect("push admitted");
        assert!(!model.chat.app.child_panel_open());
        let label = model.chat.app.child_activity_text();
        assert!(
            label.contains(if sequence < 2 {
                "1 known active"
            } else {
                "0 known active"
            }),
            "{label}"
        );
    }
    let unchanged = model.desired_child_watch_scope();
    let mut snapshot = watch_snapshot(&scope, child, SessionActivityState::Failed);
    for page in snapshot.activity.iter_mut().flatten() {
        page.observed_at_ms = 999;
    }
    model.chat.app.apply_child_watch_snapshot(&scope, snapshot);
    assert_eq!(
        model.desired_child_watch_scope(),
        unchanged,
        "timestamps never reopen a scope"
    );
    drop(model);
}

#[tokio::test]
async fn old_watch_scope_cannot_overwrite_new_navigation_even_back_to_same_session() {
    use bcode_session_models::{
        SessionActivityState, SessionActivityWatchDelivery, SessionActivityWatchHandle,
        SessionActivityWatchUpdate,
    };
    let root = SessionId::new();
    let mut model = root_test_model_with_history(root, &[]);
    let scope = model.desired_child_watch_scope().unwrap();
    model.child_watch_scope = Some(scope.clone());
    model.child_watch_navigation = Some(model.chat.app.child_watch_navigation());
    model.child_watch_generation = 3;
    let summary = child_summary(root, None);
    let mut other = summary.clone();
    other.id = SessionId::new();
    model.chat.app.apply_session_summary(&other);
    model.chat.app.apply_session_summary(&summary);
    bmux_tui_runtime::Program::update(
        &mut model,
        bmux_tui_runtime::RuntimeEvent::Message(super::BcodeRuntimeMessage::ChildWatch {
            generation: 3,
            result: Ok(SessionActivityWatchDelivery {
                handle: SessionActivityWatchHandle {
                    watch_id: SessionId::new(),
                    epoch: SessionId::new(),
                },
                sequence: 0,
                update: SessionActivityWatchUpdate::Initial,
                snapshot: watch_snapshot(&scope, SessionId::new(), SessionActivityState::Running),
            }),
        }),
    )
    .expect("old delivery discarded");
    assert!(
        !model
            .chat
            .app
            .child_activity_text()
            .contains("1 known active")
    );
    drop(model);
}

#[tokio::test]
async fn expanded_and_collapsed_loaded_branch_receives_push_and_reconnect_resnapshot() {
    use bcode_session_models::{
        SessionActivityState, SessionActivityWatchDelivery, SessionActivityWatchHandle,
        SessionActivityWatchUpdate, SessionChildPage,
    };
    let root = SessionId::new();
    let branch = SessionId::new();
    let leaf = SessionId::new();
    let mut model = root_test_model_with_history(root, &[]);
    let branch_summary = child_summary(branch, Some(root));
    model.chat.app.apply_monitor_children(
        root,
        &SessionChildPage {
            children: vec![branch_summary.clone()],
            next_after: None,
            complete: true,
            issues: vec![],
        },
    );
    model.chat.app.select_monitor_session(branch);
    assert_eq!(model.chat.app.expand_monitor_selected(), Some(branch));
    let scope = model.desired_child_watch_scope().unwrap();
    assert!(scope.children.iter().any(|query| query.parent_id == branch));
    model.child_watch_scope = Some(scope.clone());
    model.child_watch_navigation = Some(model.chat.app.child_watch_navigation());
    model.child_watch_generation = 4;
    for state in [
        SessionActivityState::PermissionWait,
        SessionActivityState::Idle,
    ] {
        let mut snapshot = watch_snapshot(&scope, leaf, state);
        for (query, page) in scope.children.iter().zip(&mut snapshot.children) {
            if query.parent_id == root {
                page.as_mut().unwrap().children.push(branch_summary.clone());
            }
        }
        let handle = SessionActivityWatchHandle {
            watch_id: SessionId::new(),
            epoch: SessionId::new(),
        };
        bmux_tui_runtime::Program::update(
            &mut model,
            bmux_tui_runtime::RuntimeEvent::Message(super::BcodeRuntimeMessage::ChildWatch {
                generation: 4,
                result: Ok(SessionActivityWatchDelivery {
                    handle,
                    sequence: 0,
                    update: SessionActivityWatchUpdate::Initial,
                    snapshot,
                }),
            }),
        )
        .unwrap();
        let context = model.chat.app.monitor_context();
        assert_eq!(
            context.descendant_badges[&branch].attention,
            usize::from(state == SessionActivityState::PermissionWait)
        );
        if state == SessionActivityState::PermissionWait {
            bmux_tui_runtime::Program::update(
                &mut model,
                bmux_tui_runtime::RuntimeEvent::Message(super::BcodeRuntimeMessage::ChildWatch {
                    generation: 4,
                    result: Err("disconnected".into()),
                }),
            )
            .unwrap();
            assert!(
                model
                    .chat
                    .app
                    .child_activity_text()
                    .contains("disconnected")
            );
        }
    }
    assert!(
        !model
            .chat
            .app
            .child_activity_text()
            .contains("disconnected")
    );
    model.chat.app.select_monitor_session(branch);
    assert_eq!(model.chat.app.expand_monitor_selected(), None);
    assert!(
        model
            .desired_child_watch_scope()
            .unwrap()
            .children
            .iter()
            .any(|query| query.parent_id == branch),
        "collapsed loaded frontier remains observed"
    );
    drop(model);
}

#[tokio::test]
async fn child_watch_discovers_bounded_pages_and_retains_explicit_loaded_windows() {
    use bcode_session_models::SessionActivityState;
    let mut model = root_test_model_with_history(SessionId::new(), &[]);
    let initial = model.desired_child_watch_scope().unwrap();
    let root = initial.children[0].parent_id;
    let cursor = SessionId::new();
    let mut snapshot = watch_snapshot(&initial, SessionId::new(), SessionActivityState::Idle);
    snapshot.children[0].as_mut().unwrap().next_after = Some(cursor);
    snapshot.activity[0].as_mut().unwrap().next_after = Some(cursor);
    model
        .chat
        .app
        .apply_child_watch_snapshot(&initial, snapshot);
    let loaded = model.desired_child_watch_scope().unwrap();
    assert!(
        loaded
            .children
            .iter()
            .any(|query| query.after == Some(cursor))
    );
    for _ in 0..20 {
        let mut effect = crate::effects::TuiEffect::LoadSessionChildren {
            parent_id: root,
            after: Some(SessionId::new()),
            generation: 0,
        };
        assert!(model.chat.app.stamp_child_effect(&mut effect));
    }
    let bounded = model.desired_child_watch_scope().unwrap();
    drop(model);
    assert!(bounded.children.len() <= 8);
    assert!(bounded.activity.len() <= 16);
    assert!(
        bounded
            .children
            .iter()
            .any(|query| query.parent_id == root && query.after.is_none())
    );
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
    assert_eq!(
        model.chat.app.expanded_monitor_sessions(),
        [] as [bcode_session_models::SessionId; 0]
    );
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
