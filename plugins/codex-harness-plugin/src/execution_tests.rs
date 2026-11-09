//! Behavioral lifecycle reducer tests; no native subprocess or provider calls.
use super::*;
use bcode_harness_models::HarnessPlacement;
use serde_json::json;

fn tracker() -> Execution {
    let mut execution = Execution::default();
    execution
        .accept(
            "turn",
            &HarnessSubmission {
                session_id: "session".into(),
                generation: 1,
                submission_id: "submission".into(),
                attempt_id: "attempt".into(),
                text: "test".into(),
                placement: HarnessPlacement::NextTurn,
            },
        )
        .unwrap();
    execution
}

#[test]
fn more_than_1024_sequential_turns_retire_contexts_without_lifetime_cap() {
    let mut execution = Execution::default();
    for serial in 0..2048 {
        let turn = format!("turn-{serial}");
        let submission = HarnessSubmission {
            session_id: "session".into(),
            generation: 1,
            submission_id: format!("submission-{serial}"),
            attempt_id: format!("attempt-{serial}"),
            text: "test".into(),
            placement: HarnessPlacement::NextTurn,
        };
        // Models an already native-guarded, acknowledged new submission. It does
        // not make public app-server turn/start conditionally safe.
        execution.accept(&turn, &submission).unwrap();
        execution.observe("item/completed", &json!({
            "turnId":turn,"item":{"type":"agentMessage","phase":"final_answer","text":"answer"}
        })).unwrap();
        let events = execution
            .observe(
                "turn/completed",
                &json!({
                    "turn":{"id":turn,"status":"completed"}
                }),
            )
            .unwrap();
        assert!(events.iter().any(|event| matches!(event,
            HarnessExecutionEvent::SubmissionTerminal { attempt, output: Some(output), .. }
            if attempt.attempt_id == submission.attempt_id && output == "answer")));
        assert!(execution.retained_count() <= 1);
    }
    // A retired native ID arriving late cannot create a reopened live turn.
    let events = execution
        .observe("turn/started", &json!({"turn":{"id":"turn-0"}}))
        .unwrap();
    assert!(matches!(
        &events[0],
        HarnessExecutionEvent::Continuation {
            origin: None,
            native_turn_id: None,
            state: HarnessWorkState::Unknown,
            ..
        }
    ));
    assert!(!execution.tracks_turn("turn-0"));
    assert_eq!(
        execution
            .observe(
                "turn/completed",
                &json!({
                    "turn":{"id":"turn-0","status":"completed"}
                })
            )
            .unwrap(),
        [] as [bcode_harness_models::HarnessExecutionEvent; 0]
    );
}

#[test]
fn retirement_keeps_work_and_notification_responsibility_over_1024_turns() {
    let mut execution = tracker();
    execution.observe("item/started", &json!({
        "turnId":"turn","item":{"type":"commandExecution","id":"command","processId":"12","status":"inProgress"}
    })).unwrap();
    execution
        .observe(
            "turn/completed",
            &json!({"turn":{"id":"turn","status":"failed"}}),
        )
        .unwrap();
    for serial in 0..2048 {
        let turn = format!("next-{serial}");
        execution
            .accept(
                &turn,
                &HarnessSubmission {
                    session_id: "session".into(),
                    generation: 1,
                    submission_id: format!("submission-{serial}"),
                    attempt_id: format!("attempt-{serial}"),
                    text: "test".into(),
                    placement: HarnessPlacement::NextTurn,
                },
            )
            .unwrap();
        execution
            .observe(
                "turn/completed",
                &json!({"turn":{"id":turn,"status":"failed"}}),
            )
            .unwrap();
        assert!(execution.retained_count() <= 3);
    }
    assert!(execution.tracks_turn("turn"));
    let events = execution.observe("item/completed", &json!({
        "turnId":"turn","item":{"type":"commandExecution","id":"command","processId":"12","status":"completed"}
    })).unwrap();
    assert!(matches!(&events[0], HarnessExecutionEvent::BackgroundWork {
        origin: Some(origin), state: HarnessWorkState::Completed,
        notification: HarnessNotificationState::Pending, ..
    } if origin.attempt_id == "attempt"));
    execution.retire_completed_foreground();
    assert!(execution.tracks_turn("turn"));
    assert_eq!(execution.work.len(), 1);
}

#[test]
fn retirement_keeps_live_linked_continuation_context() {
    let mut execution = tracker();
    execution
        .observe(
            "turn/completed",
            &json!({"turn":{"id":"turn","status":"failed"}}),
        )
        .unwrap();
    execution
        .observe(
            "turn/started",
            &json!({"turn":{"id":"followup","rootTurnId":"turn"}}),
        )
        .unwrap();
    execution.retire_completed_foreground();
    assert!(execution.tracks_turn("turn"));
    assert!(execution.tracks_turn("followup"));
    execution
        .observe(
            "turn/completed",
            &json!({"turn":{"id":"followup","status":"completed"}}),
        )
        .unwrap();
    execution.retire_completed_foreground();
    assert!(execution.turns.is_empty());
}

#[test]
fn canonical_record_ack_allows_2048_work_items_and_hydrates_exact_identity() {
    let mut execution = tracker();
    let mut saved = None;
    for serial in 0..2048 {
        let id = format!("command-{serial}");
        let events = execution.observe("item/started", &json!({
            "turnId":"turn", "item":{"type":"commandExecution","id":id,"processId":serial.to_string(),"status":"inProgress"}
        })).unwrap();
        let observations = events
            .into_iter()
            .enumerate()
            .map(|(index, event)| bcode_harness_models::HarnessObservation {
                observation_id: format!("observation-{serial}-{index}"),
                session_id: "session".into(),
                generation: 1,
                native_correlations: vec!["turn".into(), id.clone()],
                completeness: bcode_harness_models::HarnessCompleteness::Complete,
                kind: bcode_harness_models::HarnessObservationKind::Execution { event },
            })
            .collect::<Vec<_>>();
        if serial == 0 {
            saved = Some(observations.clone());
        }
        execution.retire_acknowledged(&observations);
        assert!(execution.work.is_empty());
    }
    let saved = saved.unwrap();
    execution.hydrate(&saved).unwrap();
    let events=execution.observe("item/completed", &json!({
        "turnId":"turn","item":{"type":"commandExecution","id":"command-0","processId":"0","status":"failed"}
    })).unwrap();
    let bcode_harness_models::HarnessObservationKind::Execution {
        event: HarnessExecutionEvent::BackgroundWork {
            work_id: original, ..
        },
    } = &saved[0].kind
    else {
        panic!("work")
    };
    assert!(matches!(&events[0],HarnessExecutionEvent::BackgroundWork {
        work_id,state:HarnessWorkState::Failed,notification:HarnessNotificationState::Pending,..
    } if work_id==original));
}

#[test]
fn failed_and_interrupted_foreground_do_not_need_an_answer() {
    for status in ["failed", "interrupted"] {
        let mut execution = tracker();
        let events = execution
            .observe(
                "turn/completed",
                &json!({
                    "turn":{"id":"turn","status":status}
                }),
            )
            .unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            HarnessExecutionEvent::SubmissionTerminal { output: None, .. }
        )));
        assert_eq!(
            execution
                .observe(
                    "turn/completed",
                    &json!({
                        "turn":{"id":"turn","status":"completed"}
                    })
                )
                .unwrap(),
            [] as [bcode_harness_models::HarnessExecutionEvent; 0]
        );
    }
}

#[test]
fn successful_foreground_requires_complete_preterminal_answer() {
    let mut execution = tracker();
    let events = execution
        .observe(
            "turn/completed",
            &json!({
                "turn":{"id":"turn","status":"completed"}
            }),
        )
        .unwrap();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, HarnessExecutionEvent::SubmissionTerminal { .. }))
    );
    execution
        .observe(
            "item/completed",
            &json!({
                "turnId":"turn", "item":{"type":"agentMessage","phase":"final_answer","text":"late"}
            }),
        )
        .unwrap();
    assert!(execution.turns["turn"].output.is_none());
}

#[test]
fn empty_inventory_preserves_running_work_and_pending_notification() {
    let mut execution = tracker();
    execution.observe("item/started", &json!({
        "turnId":"turn","item":{"type":"commandExecution","id":"command","processId":"12","status":"inProgress"}
    })).unwrap();
    assert_eq!(
        execution
            .inventory(&json!({"data":[],"nextCursor":null}))
            .unwrap(),
        [] as [bcode_harness_models::HarnessExecutionEvent; 0]
    );
    let work = &execution.work["codex-work:turn:command"];
    assert_eq!(work.state, HarnessWorkState::Running);
    assert!(matches!(
        work_event(work),
        HarnessExecutionEvent::BackgroundWork {
            notification: HarnessNotificationState::Pending,
            ..
        }
    ));
}

#[test]
fn inventory_only_process_has_no_guessed_origin() {
    let mut execution = tracker();
    let events = execution
        .inventory(&json!({"data":[{"itemId":"unseen","processId":"33"}],"nextCursor":null}))
        .unwrap();
    assert!(matches!(&events[0], HarnessExecutionEvent::BackgroundWork {
        origin: None, native_task_id: Some(id), state: HarnessWorkState::Running, ..
    } if id == "33"));
}

#[test]
fn subagent_call_completion_is_not_agent_completion() {
    let mut execution = tracker();
    let params = json!({"turnId":"turn","item":{
        "type":"collabAgentToolCall","id":"spawn","status":"completed",
        "receiverThreadIds":["native-agent"],"agentsStates":{"native-agent":{"status":"running"}}
    }});
    let events = execution.observe("item/completed", &params).unwrap();
    assert!(matches!(
        &events[0],
        HarnessExecutionEvent::BackgroundWork {
            work_kind: HarnessWorkKind::Subagent,
            state: HarnessWorkState::Running,
            ..
        }
    ));
    let mut ended = params;
    ended["item"]["agentsStates"]["native-agent"]["status"] = json!("completed");
    let events = execution.observe("item/completed", &ended).unwrap();
    assert!(matches!(
        &events[0],
        HarnessExecutionEvent::BackgroundWork {
            state: HarnessWorkState::Completed,
            notification: HarnessNotificationState::Pending,
            ..
        }
    ));
}

#[test]
fn unrelated_native_turn_is_unknown_origin_not_new_primary() {
    let mut execution = tracker();
    let events = execution
        .observe("turn/started", &json!({"turn":{"id":"automatic"}}))
        .unwrap();
    assert!(matches!(&events[0], HarnessExecutionEvent::Continuation {
        origin: None, native_turn_id: Some(id), state: HarnessWorkState::Running, ..
    } if id == "automatic"));
    let events = execution
        .observe(
            "turn/completed",
            &json!({"turn":{"id":"automatic","status":"completed"}}),
        )
        .unwrap();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, HarnessExecutionEvent::SubmissionTerminal { .. }))
    );
}

#[test]
fn lifecycle_inventory_overflow_fails_closed() {
    let mut execution = tracker();
    let terminals = (0..=MAX_HARNESS_EXECUTION_ITEMS)
        .map(|id| {
            json!({
                "itemId":format!("item-{id}"),"processId":id.to_string()
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        execution.inventory(&json!({"data":terminals})),
        Err(CodexError::Bounds)
    );
}
