use super::*;
use bcode_claude_code::SubmissionUuid;
use serde_json::json;

fn attempt(n: u8) -> HarnessAttemptIdentity {
    HarnessAttemptIdentity {
        submission_id: format!("submission-{n}"),
        attempt_id: format!("00000000-0000-4000-8000-{n:012x}"),
    }
}
fn tracker() -> Tracker {
    let mut tracker = Tracker::new();
    tracker.configure(true, true);
    tracker.retain(attempt(1)).unwrap();
    tracker.event(&system(json!({"subtype":"init"}))).unwrap();
    tracker
}
fn system(mut value: Value) -> Event {
    value["type"] = json!("system");
    Event::Message(NativeMessage::System(value))
}
fn reply(tracker: &mut Tracker) {
    tracker.current = Some(attempt(1));
    tracker
        .event(&Event::Message(NativeMessage::Assistant(
            json!({"type":"assistant","message":{"content":[{
                "type":"tool_use","name":"Bash","id":"tool-1","input":{"run_in_background":true}
            }]}}),
        )))
        .unwrap();
}
fn result(tracker: &mut Tracker, frame: &str) -> Vec<Execution> {
    tracker.frames.insert(frame.into(), vec![attempt(1)]);
    tracker.event(&Event::Message(NativeMessage::Result(json!({"type":"result","uuid":frame,"subtype":"success","is_error":false,"result":"answer"})))).unwrap()
}
#[test]
fn foreground_result_does_not_end_background_or_pending_notification() {
    let mut tracker = tracker();
    reply(&mut tracker);
    let start = tracker.event(&system(json!({"subtype":"task_started","task_id":"shell","run_id":"run1","task_type":"local_bash","tool_use_id":"tool-1"}))).unwrap();
    assert!(start.iter().any(|event| matches!(
        event,
        Execution::BackgroundWork {
            origin: Some(_),
            work_kind: Kind::Command,
            state: State::Running,
            ..
        }
    )));
    let events = result(&mut tracker, "result1");
    assert!(events.iter().any(|event| matches!(event, Execution::SubmissionTerminal { disposition: Disposition::Completed, native_turn_id: None, output: Some(output), .. } if output == "answer")));
    tracker
        .event(&system(
            json!({"subtype":"session_state_changed","state":"idle"}),
        ))
        .unwrap();
    assert_eq!(tracker.work.values().next().unwrap().state, State::Running);
    tracker
        .event(&system(
            json!({"subtype":"background_tasks_changed","tasks":[]}),
        ))
        .unwrap();
    let work = tracker.work.values().next().unwrap();
    assert_eq!(work.state, State::Unknown);
    assert_eq!(work.notification, Notification::Pending);
    assert!(tracker.followups.is_empty());
    let events = tracker.event(&system(json!({"subtype":"task_notification","task_id":"shell","run_id":"run1","status":"completed"}))).unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        Execution::Continuation {
            state: State::Pending,
            ..
        }
    )));
    assert_eq!(
        tracker.work.values().next().unwrap().state,
        State::Completed
    );
    assert_eq!(tracker.terminals.len(), 1);
}
#[test]
fn late_native_wake_completes_separate_continuation_never_reopens_foreground() {
    let mut tracker = tracker();
    reply(&mut tracker);
    result(&mut tracker, "foreground");
    tracker.event(&system(json!({"subtype":"task_started","task_id":"agent","run_id":"run1","task_type":"local_agent","tool_use_id":"tool-1"}))).unwrap();
    let origin = json!({"kind":"task-notification","producer":"session-task","runId":"run1"});
    // Wake can arrive before notification bookkeeping.
    let running = tracker
        .event(&Event::Message(NativeMessage::Assistant(
            json!({"type":"assistant","origin":origin,"message":{"content":[]}}),
        )))
        .unwrap();
    assert!(running.iter().any(|event| matches!(
        event,
        Execution::Continuation {
            state: State::Running,
            origin: Some(_),
            native_turn_id: None,
            ..
        }
    )));
    let frame = json!({"type":"result","uuid":"wake-result","origin":origin,"subtype":"success","is_error":false,"result":"follow-up"});
    let completed = tracker
        .event(&Event::Message(NativeMessage::Result(frame.clone())))
        .unwrap();
    assert!(completed.iter().any(|event| matches!(event, Execution::Continuation { state: State::Completed, output: Some(output), .. } if output == "follow-up")));
    tracker.event(&system(json!({"subtype":"task_notification","task_id":"agent","run_id":"run1","status":"completed"}))).unwrap();
    assert_eq!(
        tracker.followups.values().next().unwrap().state,
        State::Completed
    );
    assert!(
        !tracker
            .event(&Event::Message(NativeMessage::Result(frame)))
            .unwrap()
            .iter()
            .any(|event| matches!(event, Execution::Continuation { .. }))
    );
    assert_eq!(tracker.terminals.len(), 1);
}
#[test]
fn old_run_updates_cannot_reopen_a_terminal_run() {
    let mut tracker = tracker();
    for (run, subtype, status) in [
        ("r1", "task_started", "running"),
        ("r1", "task_notification", "completed"),
        ("r2", "task_started", "running"),
        ("r1", "task_started", "running"),
    ] {
        tracker.event(&system(json!({"subtype":subtype,"task_id":"agent","run_id":run,"task_type":"local_agent","status":status}))).unwrap();
    }
    assert_eq!(
        tracker.work[&("agent".into(), Some("r1".into()))].state,
        State::Completed
    );
    assert_eq!(
        tracker.work[&("agent".into(), Some("r2".into()))].state,
        State::Running
    );
    assert_ne!(
        tracker.work[&("agent".into(), Some("r1".into()))].id,
        tracker.work[&("agent".into(), Some("r2".into()))].id
    );
}
#[test]
fn loss_and_unsupported_activity_reduce_coverage_without_rewriting_terminals() {
    let mut tracker = tracker();
    result(&mut tracker, "foreground");
    tracker
        .event(&system(
            json!({"subtype":"task_started","task_id":"future","task_type":"future_task"}),
        ))
        .unwrap();
    assert!(tracker.coverage().iter().all(|e| matches!(
        e,
        Execution::Coverage {
            background: Coverage::Unknown,
            ..
        }
    )));
    let before = tracker.terminals.clone();
    tracker.disconnected();
    assert_eq!(tracker.terminals, before);
    assert!(tracker.coverage().iter().all(|e| matches!(
        e,
        Execution::Coverage {
            background: Coverage::Unknown,
            continuations: Coverage::Unknown,
            ..
        }
    )));
}
#[test]
fn merged_result_has_exact_terminals_but_no_invented_exclusive_output_or_origin() {
    let mut tracker = tracker();
    tracker.retain(attempt(2)).unwrap();
    let uuids: Vec<_> = [1, 2]
        .into_iter()
        .map(|n| SubmissionUuid::new(attempt(n).attempt_id).unwrap())
        .collect();
    let evidence = SubmissionAttribution {
        native_frame_uuid: "frame".into(),
        primary: uuids[1].clone(),
        consumed: uuids,
        resumed: false,
        stage: AttributionStage::Reply,
    };
    let retained: Vec<_> = [1, 2]
        .into_iter()
        .map(|n| RetainedAttribution {
            submission_id: attempt(n).submission_id,
            attempt_id: attempt(n).attempt_id,
            evidence: evidence.clone(),
        })
        .collect();
    tracker.attribute(&evidence, &retained).unwrap();
    assert!(tracker.current.is_none());
    tracker
        .frames
        .insert("merged".into(), vec![attempt(1), attempt(2)]);
    let events = tracker.event(&Event::Message(NativeMessage::Result(json!({"type":"result","uuid":"merged","subtype":"success","is_error":false,"result":"joint answer"})))).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Execution::SubmissionTerminal { output: None, .. }))
            .count(),
        2
    );
}
#[test]
fn resume_and_duplicate_conflicts_fail_closed_not_new_success() {
    let mut tracker = tracker();
    result(&mut tracker, "foreground");
    let value = json!({"type":"result","uuid":"foreground","subtype":"success","is_error":false,"result":"changed"});
    assert!(
        tracker
            .event(&Event::Message(NativeMessage::Result(value)))
            .is_err()
    );
    tracker
        .event(&system(
            json!({"subtype":"init","resume_reason":"worker_restart"}),
        ))
        .unwrap();
    assert!(tracker.coverage().iter().all(|e| matches!(
        e,
        Execution::Coverage {
            background: Coverage::Unknown,
            continuations: Coverage::Unknown,
            ..
        }
    )));
}
#[test]
fn known_snapshot_then_bookend_and_tool_result_share_one_work_identity() {
    let mut tracker = tracker();
    reply(&mut tracker);
    tracker.event(&system(json!({"subtype":"background_tasks_changed","tasks":[{"task_id":"shell","run_id":"r1","task_type":"local_bash"}]}))).unwrap();
    tracker.event(&system(json!({"subtype":"task_started","task_id":"shell","run_id":"r1","task_type":"local_bash","tool_use_id":"tool-1"}))).unwrap();
    tracker.event(&Event::Message(NativeMessage::User(json!({"type":"user","tool_use_result":{"backgroundTaskId":"shell"},"message":{"content":[{"type":"tool_result","tool_use_id":"tool-1"}]}})))).unwrap();
    assert_eq!(tracker.work.len(), 1);
    assert_eq!(
        tracker.work.values().next().unwrap().origin,
        Some(attempt(1))
    );
    assert!(tracker.coverage().iter().all(|e| matches!(
        e,
        Execution::Coverage {
            background: Coverage::Complete,
            ..
        }
    )));
}
#[test]
fn stream_tool_start_degrades_coverage_until_complete_tool_and_task_evidence() {
    let mut tracker = tracker();
    tracker.current = Some(attempt(1));
    tracker.event(&Event::Message(NativeMessage::Stream(json!({"type":"stream_event",
            "event":{"type":"content_block_start","content_block":{"type":"tool_use","id":"tool-1","name":"Bash"}}})))).unwrap();
    assert!(tracker.coverage().iter().all(|event| matches!(
        event,
        Execution::Coverage {
            background: Coverage::Unknown,
            ..
        }
    )));
    reply(&mut tracker);
    tracker.event(&system(json!({"subtype":"task_started","task_id":"shell","task_type":"local_bash","tool_use_id":"tool-1"}))).unwrap();
    assert!(tracker.coverage().iter().all(|event| matches!(
        event,
        Execution::Coverage {
            background: Coverage::Complete,
            ..
        }
    )));
}

#[test]
fn bounds_unknown_wake_and_retired_attempts_never_fabricate_identity() {
    let mut tracker = tracker();
    let events = tracker.event(&Event::Message(NativeMessage::Assistant(json!({"type":"assistant",
            "origin":{"kind":"task-notification","producer":"session-task"},"message":{"content":[]}})))).unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        Execution::Continuation {
            origin: None,
            source_work_id: None,
            native_turn_id: None,
            state: State::Running,
            ..
        }
    )));
    let mut full = self::tracker();
    for index in 0..MAX_HARNESS_EXECUTION_ITEMS {
        full.event(&system(json!({"subtype":"task_started","task_id":format!("task-{index}"),"run_id":format!("run-{index}"),"task_type":"local_bash"}))).unwrap();
    }
    assert!(
        full.event(&system(
            json!({"subtype":"task_started","task_id":"overflow","task_type":"local_bash"})
        ))
        .is_err()
    );
}

#[test]
fn new_admission_can_retire_canonically_published_foreground_not_pending_work() {
    let mut tracker = tracker();
    result(&mut tracker, "done");
    tracker.check_retain().unwrap();
    assert!(tracker.attempts.contains(&attempt(1)));
    tracker
        .event(&system(
            json!({"subtype":"session_state_changed","state":"idle"}),
        ))
        .unwrap();
    tracker.retire_completed();
    assert!(tracker.attempts.is_empty());
    let mut retained = self::tracker();
    reply(&mut retained);
    retained.event(&system(json!({"subtype":"task_started","task_id":"shell","task_type":"local_bash","tool_use_id":"tool-1"}))).unwrap();
    result(&mut retained, "done");
    retained.check_retain().unwrap();
    assert!(retained.attempts.contains(&attempt(1)));
    assert!(
        retained
            .work
            .values()
            .any(|work| work.notification == Notification::Pending)
    );
}

#[test]
fn failed_foreground_output_is_terminal_without_background_settlement_or_reopen() {
    let mut tracker = tracker();
    tracker.frames.insert("failed".into(), vec![attempt(1)]);
    let events = tracker
        .event(&Event::Message(NativeMessage::Result(
            json!({"type":"result","uuid":"failed",
            "subtype":"error_during_execution","is_error":true}),
        )))
        .unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        Execution::SubmissionTerminal {
            disposition: Disposition::Failed,
            output: None,
            ..
        }
    )));
    tracker
        .frames
        .insert("late-success".into(), vec![attempt(1)]);
    let late = tracker
        .event(&Event::Message(NativeMessage::Result(
            json!({"type":"result","uuid":"late-success",
            "subtype":"success","is_error":false,"result":"too late"}),
        )))
        .unwrap();
    assert!(
        !late
            .iter()
            .any(|event| matches!(event, Execution::SubmissionTerminal { .. }))
    );
    assert!(matches!(
        tracker.terminals[&attempt(1)],
        Execution::SubmissionTerminal {
            disposition: Disposition::Failed,
            ..
        }
    ));
}

#[test]
fn unrelated_delayed_foreground_result_cannot_complete_running_wake() {
    let mut tracker = tracker();
    reply(&mut tracker);
    tracker.event(&system(json!({"subtype":"task_started","task_id":"agent","run_id":"run-b","task_type":"local_agent","tool_use_id":"tool-1"}))).unwrap();
    let origin = json!({"kind":"task-notification","producer":"session-task","runId":"run-b"});
    tracker
        .event(&Event::Message(NativeMessage::Assistant(
            json!({"type":"assistant","origin":origin,"message":{"content":[]}}),
        )))
        .unwrap();
    let active = tracker.active_followup.clone();
    let foreground = result(&mut tracker, "delayed-a");
    assert!(!foreground.iter().any(|event| matches!(
        event,
        Execution::Continuation {
            state: State::Completed,
            ..
        }
    )));
    assert_eq!(tracker.active_followup, active);
    assert_eq!(
        tracker.followups.values().next().unwrap().state,
        State::Running
    );
    let unbound = json!({"type":"result","uuid":"unbound","origin":{"kind":"task-notification","producer":"session-task"},"subtype":"success","is_error":false,"result":"not causally linked"});
    tracker
        .event(&Event::Message(NativeMessage::Result(unbound)))
        .unwrap();
    assert_eq!(tracker.active_followup, active);
    assert_eq!(
        tracker.followups.values().next().unwrap().state,
        State::Running
    );
    let mismatched = json!({"type":"result","uuid":"wrong-run","origin":{"kind":"task-notification","producer":"session-task","runId":"run-c"},"subtype":"success","is_error":false,"result":"wrong run"});
    let mismatch_events = tracker
        .event(&Event::Message(NativeMessage::Result(mismatched)))
        .unwrap();
    assert!(!mismatch_events.iter().any(|event| matches!(
        event,
        Execution::Continuation {
            state: State::Completed,
            ..
        }
    )));
    assert_eq!(tracker.active_followup, active);
    assert_eq!(tracker.followups.len(), 1);
    assert_eq!(
        tracker.followups.values().next().unwrap().state,
        State::Running
    );
    assert!(tracker.coverage().iter().all(|event| matches!(
        event,
        Execution::Coverage {
            continuations: Coverage::Unknown,
            ..
        }
    )));
}

#[test]
fn acknowledged_sequential_attempts_exceed_1024_without_lifetime_cap() {
    let mut tracker = Tracker::new();
    tracker.configure(true, true);
    tracker.event(&system(json!({"subtype":"init"}))).unwrap();
    for serial in 0..1100 {
        let attempt = HarnessAttemptIdentity {
            submission_id: format!("submission-{serial}"),
            attempt_id: format!("00000000-0000-4000-8000-{serial:012x}"),
        };
        tracker.retain(attempt.clone()).unwrap();
        let frame = format!("frame-{serial}");
        tracker.frames.insert(frame.clone(), vec![attempt]);
        let events = tracker.event(&Event::Message(NativeMessage::Result(json!({"type":"result","uuid":frame,"subtype":"success","is_error":false,"result":"answer"})))).unwrap();
        for (index, event) in events.iter().enumerate() {
            tracker.published(event, &format!("obs-{index}"));
        }
        tracker
            .event(&system(
                json!({"subtype":"session_state_changed","state":"idle"}),
            ))
            .unwrap();
        assert_eq!(tracker.retire_completed(), Vec::new());
        for index in 0..events.len() {
            tracker.acknowledge(&format!("obs-{index}"));
        }
        assert_eq!(tracker.retire_completed().len(), 1);
        assert!(
            tracker.attempts.is_empty()
                && tracker.frames.is_empty()
                && tracker.terminals.is_empty()
        );
    }
}

#[test]
fn opaque_task_and_run_pairs_do_not_alias() {
    let mut tracker = tracker();
    tracker
        .event(&system(
            json!({"subtype":"task_started","task_id":"a:b","run_id":"c","task_type":"local_bash"}),
        ))
        .unwrap();
    tracker
        .event(&system(
            json!({"subtype":"task_started","task_id":"a","run_id":"b:c","task_type":"local_bash"}),
        ))
        .unwrap();
    tracker.event(&system(json!({"subtype":"task_notification","task_id":"a:b","run_id":"c","status":"completed"}))).unwrap();
    let first = &tracker.work[&("a:b".into(), Some("c".into()))];
    let second = &tracker.work[&("a".into(), Some("b:c".into()))];
    assert_ne!(first.id, second.id);
    assert_eq!(first.state, State::Completed);
    assert_eq!(second.state, State::Running);
}

#[test]
fn task_only_update_cannot_choose_between_native_runs() {
    let mut tracker = tracker();
    for run in ["r1", "r2"] {
        tracker.event(&system(json!({"subtype":"task_started","task_id":"shell","run_id":run,"task_type":"local_bash"}))).unwrap();
    }
    assert!(
        tracker
            .event(&system(
                json!({"subtype":"task_notification","task_id":"shell","status":"completed"})
            ))
            .is_err()
    );
    assert_eq!(tracker.work.len(), 2);
    assert!(
        tracker
            .work
            .values()
            .all(|work| work.state == State::Running)
    );
}

#[test]
fn hydration_replays_existing_work_and_followup_at_full_cache_capacity() {
    use bcode_harness_models::{
        HarnessCompleteness, HarnessHydrationReply, HarnessHydrationResolution,
        HarnessNativeReference, HarnessNativeReferenceKind, HarnessObservation,
        HarnessObservationKind,
    };
    let mut tracker = tracker();
    for index in 0..MAX_HARNESS_EXECUTION_ITEMS {
        tracker.event(&system(json!({"subtype":"task_started","task_id":format!("task-{index}"),"run_id":"run","task_type":"local_bash"}))).unwrap();
    }
    let work = tracker.work.values().next().unwrap().clone();
    for index in 0..MAX_HARNESS_EXECUTION_ITEMS {
        let id = format!("followup-{index}");
        tracker.followups.insert(
            id.clone(),
            Followup {
                id,
                origin: None,
                work: Some(work.id.clone()),
                state: State::Pending,
                result_frame: None,
                output: None,
            },
        );
    }
    let followup = tracker.followups.values().next().unwrap().clone();
    let observation = |event, serial: &str| HarnessObservation {
        observation_id: serial.into(),
        session_id: "session".into(),
        generation: 7,
        native_correlations: vec!["claude-run:run".into()],
        completeness: HarnessCompleteness::Complete,
        kind: HarnessObservationKind::Execution { event },
    };
    tracker
        .hydrate(&HarnessHydrationReply {
            request_id: "hydrate-existing".into(),
            reference: HarnessNativeReference {
                kind: HarnessNativeReferenceKind::Process,
                native_id: work.task.clone(),
                native_turn_id: None,
            },
            canonical_revision: 1,
            resolution: HarnessHydrationResolution::Found {
                observations: vec![
                    observation(work.event(), "work"),
                    observation(followup.event(), "followup"),
                ],
            },
        })
        .unwrap();
    assert_eq!(tracker.work.len(), MAX_HARNESS_EXECUTION_ITEMS);
    assert_eq!(tracker.followups.len(), MAX_HARNESS_EXECUTION_ITEMS);
    assert_eq!(tracker.work.values().next().unwrap().id, work.id);
    assert_eq!(tracker.followups[&followup.id].state, State::Pending);
}

#[test]
fn retired_assistant_tool_never_assigns_late_task_to_current_attempt() {
    let mut tracker = tracker();
    tracker.acknowledged_terminals.insert(attempt(1));
    assert_eq!(tracker.retire_canonical_attempts(), vec![attempt(1)]);
    tracker.retain(attempt(2)).unwrap();
    tracker.current = Some(attempt(2));
    for frame in ["old-frame", "new-frame"] {
        tracker.event(&Event::Message(NativeMessage::Assistant(json!({
            "type":"assistant", "uuid":frame, "user_message_uuid":attempt(1).attempt_id,
            "message":{"content":[{"type":"tool_use","id":"late-tool","name":"Bash","input":{"run_in_background":true}}]}
        })))).unwrap();
        let events = tracker.event(&system(json!({"subtype":"task_started","task_id":"late-task","run_id":frame,"task_type":"local_bash","tool_use_id":"late-tool"}))).unwrap();
        assert!(!tracker.tools.contains_key("late-tool"));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Execution::BackgroundWork { origin: None, .. }))
        );
        assert!(
            tracker
                .work
                .values()
                .all(|work| work.origin != Some(attempt(2)))
        );
    }
}

#[test]
fn enrichment_is_monotonic_exact_and_never_reassigns_known_origin_or_kind() {
    let mut tracker = tracker();
    reply(&mut tracker);
    tracker.event(&system(json!({"subtype":"task_notification","task_id":"shell","run_id":"r1","status":"completed"}))).unwrap();
    let key = ("shell".into(), Some("r1".into()));
    let work_id = tracker.work[&key].id.clone();
    assert_eq!(tracker.work[&key].kind, Kind::Other);
    assert!(tracker.work[&key].origin.is_none());
    tracker.event(&system(json!({"subtype":"task_started","task_id":"shell","run_id":"r1","task_type":"local_bash","tool_use_id":"tool-1"}))).unwrap();
    assert_eq!(tracker.work[&key].id, work_id);
    assert_eq!(tracker.work[&key].kind, Kind::Command);
    assert_eq!(tracker.work[&key].origin, Some(attempt(1)));
    assert_eq!(tracker.work[&key].state, State::Completed);
    assert_eq!(
        tracker.followups.values().next().unwrap().origin,
        Some(attempt(1))
    );
    tracker.retain(attempt(2)).unwrap();
    tracker
        .tools
        .insert("foreign-tool".into(), Some(attempt(2)));
    assert!(tracker.event(&system(json!({"subtype":"task_started","task_id":"shell","run_id":"r1","task_type":"local_bash","tool_use_id":"foreign-tool"}))).is_err());
    assert!(tracker.event(&system(json!({"subtype":"task_started","task_id":"shell","run_id":"r1","task_type":"local_agent"}))).is_err());
    assert_eq!(tracker.work[&key].origin, Some(attempt(1)));
    assert_eq!(tracker.work[&key].kind, Kind::Command);
}

#[test]
fn observed_notification_alone_never_finishes_followup_and_coverage_is_not_eraser() {
    let mut tracker = tracker();
    tracker.event(&system(json!({"subtype":"task_notification","task_id":"shell","run_id":"r1","status":"completed"}))).unwrap();
    tracker
        .event(&system(
            json!({"subtype":"background_tasks_changed","tasks":[]}),
        ))
        .unwrap();
    assert_eq!(
        tracker.followups.values().next().unwrap().state,
        State::Pending
    );
    assert_eq!(
        tracker.work.values().next().unwrap().notification,
        Notification::Observed
    );
    assert!(tracker.followups.values().next().unwrap().origin.is_none());
}
