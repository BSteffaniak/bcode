#![cfg(unix)]
//! Behavioral offline subprocess tests. They never launch Codex or contact a provider.
use bcode_codex_harness_plugin::{
    auth::CodexAuth,
    session::CodexSession,
    transport::{CodexError, Launch},
};
use bcode_harness_models::{
    HARNESS_SCHEMA_VERSION, HarnessActivity, HarnessAssistanceGrant, HarnessAuthBinding,
    HarnessCompleteness, HarnessDeliveryState, HarnessObservation, HarnessObservationKind,
    HarnessPlacement, HarnessReceipt, HarnessResponse, HarnessResponseValue, HarnessSessionBinding,
    HarnessSubmission, HarnessTerminalDisposition,
};
use bcode_provider_auth_models::managed::{AuthCustodyMode, AuthReadiness};
use std::os::unix::fs::PermissionsExt as _;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn fixture() -> (TempDir, Launch) {
    // Publish one immutable executable before any fixture process is spawned. Rewriting
    // per-test scripts concurrently with fork/exec can transiently fail with ETXTBSY
    // when a sibling child inherits a still-writable script descriptor before exec.
    // Only executable bytes are shared; protocol logs, markers and namespaces remain isolated.
    static EXECUTABLE: std::sync::OnceLock<(TempDir, std::path::PathBuf)> =
        std::sync::OnceLock::new();
    let (_, executable) = EXECUTABLE.get_or_init(|| {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("fake-codex");
        std::fs::write(&executable, include_str!("fixtures/app_server.py")).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        (directory, executable)
    });
    let temp = tempfile::tempdir().unwrap();
    let launch = Launch {
        executable: executable.clone(),
        codex_home: temp.path().join("codex-home"),
        working_directory: temp.path().to_owned(),
        ephemeral_credentials: false,
        timeout: Duration::from_secs(2),
    };
    (temp, launch)
}
fn binding() -> HarnessSessionBinding {
    HarnessSessionBinding {
        schema_version: HARNESS_SCHEMA_VERSION,
        session_id: "bcode-session".into(),
        instance_id: "instance".into(),
        native_session_id: "retained-open".into(),
        native_version: "pending".into(),
        generation: 7,
    }
}
fn submission(text: &str) -> HarnessSubmission {
    HarnessSubmission {
        submission_id: format!("submit-{text}"),
        attempt_id: format!("attempt-{text}"),
        session_id: "bcode-session".into(),
        generation: 7,
        text: text.into(),
        placement: HarnessPlacement::Steer {
            expected_native_turn_id: "turn-1".into(),
        },
    }
}
fn observations(session: &mut CodexSession, want_interaction: bool) -> Vec<HarnessObservation> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut all = Vec::new();
    while Instant::now() < deadline {
        let (page, _) = session.observe(16).unwrap();
        all.extend(page);
        if want_interaction
            && all.iter().any(|observation| {
                matches!(observation.kind, HarnessObservationKind::Interaction { .. })
            })
        {
            break;
        }
        if !want_interaction
            && all.iter().any(|observation| {
                matches!(
                    &observation.kind,
                    HarnessObservationKind::NativeTurnTerminal { .. }
                )
            })
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    all
}
fn calls(temp: &TempDir) -> Vec<serde_json::Value> {
    std::fs::read_to_string(temp.path().join("methods.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn start_resume_and_metadata_only_read_preserve_namespace() {
    let (_temp, launch) = fixture();
    let mut session = CodexSession::open(&launch, binding(), false).unwrap();
    assert_eq!(session.binding().native_session_id, "native-thread");
    assert_eq!(session.binding().native_version, "0.0.0-pinned-fixture");
    assert_eq!(session.reconcile().unwrap().activity, HarnessActivity::Idle);
    let retained = session.binding().clone();
    drop(session);
    let resumed = CodexSession::open(&launch, retained, true).unwrap();
    assert_eq!(resumed.binding().native_session_id, "native-thread");
}

#[test]
fn namespace_mismatch_fails_before_thread_mutation() {
    let (temp, launch) = fixture();
    std::fs::write(temp.path().join("wrong-home"), "").unwrap();
    assert!(matches!(
        CodexSession::open(&launch, binding(), false),
        Err(CodexError::Configuration)
    ));
    assert!(
        !calls(&temp)
            .iter()
            .any(|call| call["method"] == "thread/start")
    );
}

#[test]
fn steering_names_expected_turn_and_deduplicates_exact_attempt() {
    let (temp, launch) = fixture();
    let mut session = CodexSession::open(&launch, binding(), false).unwrap();
    let mut input = submission("steer");
    input.placement = HarnessPlacement::Steer {
        expected_native_turn_id: "turn-1".into(),
    };
    let first = session.submit(&input).unwrap();
    assert_eq!(session.submit(&input).unwrap(), first);
    let mut stale = submission("stale");
    stale.generation = 6;
    assert_eq!(session.submit(&stale), Err(CodexError::Stale));
    input.text = "conflicting duplicate".into();
    assert_eq!(session.submit(&input), Err(CodexError::Stale));
    let calls = calls(&temp);
    let steer = calls
        .iter()
        .filter(|call| call["method"] == "turn/steer")
        .collect::<Vec<_>>();
    assert_eq!(steer.len(), 1);
    assert_eq!(steer[0]["expectedTurnId"], "turn-1");
}

#[test]
fn lost_ack_blocks_new_mutations_and_does_not_replay() {
    let (temp, launch) = fixture();
    let mut session = CodexSession::open(&launch, binding(), false).unwrap();
    let input = submission("lost-ack");
    assert_eq!(
        session.submit(&input).unwrap().state,
        HarnessDeliveryState::Uncertain
    );
    assert_eq!(
        session.submit(&input).unwrap().state,
        HarnessDeliveryState::Uncertain
    );
    assert_eq!(
        session.submit(&submission("later")),
        Err(CodexError::Uncertain)
    );
    let (unaccepted, _) = session.observe(1).unwrap();
    assert!(!unaccepted.iter().any(|event| matches!(
        event.kind,
        HarnessObservationKind::SubmissionAccepted { .. }
    )));
    assert_eq!(
        calls(&temp)
            .iter()
            .filter(|call| call["method"] == "turn/steer")
            .count(),
        1
    );
}

#[test]
fn continuously_drains_stderr_and_normalizes_correlated_turn_completion() {
    let (_temp, launch) = fixture();
    let mut session = CodexSession::open(&launch, binding(), false).unwrap();
    assert_eq!(
        session.submit(&submission("stderr")).unwrap().state,
        HarnessDeliveryState::NativeAccepted
    );
    let events = observations(&mut session, false);
    assert!(
        events
            .iter()
            .any(|event| matches!(event.kind, HarnessObservationKind::Text { .. }))
    );
    assert!(events.iter().any(|event| matches!(&event.kind, HarnessObservationKind::NativeTurnTerminal { native_turn_id, disposition: HarnessTerminalDisposition::Completed } if native_turn_id == "turn-1")));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.kind, HarnessObservationKind::Settlement { .. }))
    );
    assert!(
        events
            .iter()
            .filter(|event| matches!(
                event.kind,
                HarnessObservationKind::Status { .. } | HarnessObservationKind::Settlement { .. }
            ))
            .any(|event| event.native_correlations.contains(&"turn-1".to_owned()))
    );
}

#[test]
fn approvals_questions_and_assistance_are_generation_fenced_one_shot_callbacks() {
    for (text, value) in [
        (
            "approval",
            HarnessResponseValue::Approval { approved: false },
        ),
        (
            "question",
            HarnessResponseValue::Answer {
                choice_id: None,
                text: "choice".into(),
            },
        ),
        (
            "assistance",
            HarnessResponseValue::Assistance {
                text: "context".into(),
            },
        ),
    ] {
        let (_temp, launch) = fixture();
        let grant = HarnessAssistanceGrant {
            admission_id: "assistance-admission".into(),
            child_session_id: "bcode-session".into(),
            child_generation: 7,
            parent_session_id: "parent".into(),
            parent_generation: 1,
            scope: "context assistance".into(),
        };
        let mut session =
            CodexSession::open_admitted(&launch, binding(), false, None, Some(&grant)).unwrap();
        session.submit(&submission(text)).unwrap();
        let interaction = observations(&mut session, true)
            .into_iter()
            .find_map(|event| {
                if let HarnessObservationKind::Interaction { interaction } = event.kind {
                    Some(interaction)
                } else {
                    None
                }
            })
            .expect("native callback must be normalized");
        let mut response = HarnessResponse {
            response_id: "response".into(),
            request_id: interaction.request_id,
            session_id: "bcode-session".into(),
            generation: 6,
            value,
        };
        assert_eq!(session.respond(&response), Err(CodexError::Stale));
        response.generation = 7;
        session.respond(&response).unwrap();
        assert_eq!(session.respond(&response), Err(CodexError::Stale));
    }
}

fn result_fixture_events(session: &mut CodexSession, limit: usize) -> Vec<HarnessObservation> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut events = Vec::new();
    while Instant::now() < deadline {
        let (page, _) = session.observe(limit).unwrap();
        assert!(page.len() <= limit);
        let ended = page.iter().any(|event| {
            matches!(&event.kind,
            HarnessObservationKind::Text { text } if text == "result-fixture-end")
        });
        events.extend(page);
        if ended {
            return events;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("fixture must reach the ordered end marker");
}

#[test]
fn completed_final_agent_item_is_complete_correlated_output_before_separate_terminal() {
    for limit in [1, 16] {
        let (_temp, launch) = fixture();
        let mut session = CodexSession::open(&launch, binding(), false).unwrap();
        session.submit(&submission("result-final")).unwrap();
        let events = result_fixture_events(&mut session, limit);
        let results = events
            .iter()
            .enumerate()
            .filter(|(_, event)| matches!(event.kind, HarnessObservationKind::Result { .. }))
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 1);
        let (result_index, result) = results[0];
        assert_eq!(result.completeness, HarnessCompleteness::Complete);
        assert_eq!(result.native_correlations, ["turn-1", "message-1"]);
        assert_eq!(
            result.kind,
            HarnessObservationKind::Result {
                text: "Complete answer: 界 👋".into(),
            }
        );
        assert!(
            events[..result_index]
                .iter()
                .any(|event| matches!(&event.kind,
            HarnessObservationKind::Text { text } if text == "partial streamed text"))
        );
        let terminal_index = events.iter().position(|event| matches!(&event.kind,
            HarnessObservationKind::NativeTurnTerminal { native_turn_id, disposition: HarnessTerminalDisposition::Completed } if native_turn_id == "turn-1")).unwrap();
        assert!(result_index < terminal_index);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.kind, HarnessObservationKind::Settlement { .. }))
        );
        assert!(events[..result_index].iter().any(|event| matches!(&event.kind,
            HarnessObservationKind::SubmissionAccepted { native_turn_id, .. } if native_turn_id == "turn-1")));
    }
}

#[test]
fn delta_started_partial_unphased_and_stale_items_never_become_authoritative_results() {
    for text in [
        "result-delta-only",
        "result-started",
        "result-commentary",
        "result-partial",
        "result-unphased",
        "result-unknown-phase",
        "result-wrong-turn",
        "result-missing-turn",
        "result-foreign-thread",
        "result-late",
    ] {
        let (_temp, launch) = fixture();
        let mut session = CodexSession::open(&launch, binding(), false).unwrap();
        session.submit(&submission(text)).unwrap();
        let events = result_fixture_events(&mut session, 1);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.kind, HarnessObservationKind::Result { .. })),
            "{text}"
        );
        // Terminal disposition without a final item is lifecycle evidence, not output.
        assert!(
            events.iter().any(|event| matches!(
                event.kind,
                HarnessObservationKind::NativeTurnTerminal { .. }
            )),
            "{text}"
        );
    }
}

#[test]
fn observed_but_unaccepted_turn_cannot_supply_an_assignment_result() {
    let (_temp, launch) = fixture();
    let mut session = CodexSession::open(&launch, binding(), false).unwrap();
    session
        .submit(&submission("result-unaccepted-turn"))
        .unwrap();
    let events = result_fixture_events(&mut session, 1);
    assert!(!events.iter().any(|event| matches!(
        event.kind,
        HarnessObservationKind::Result { .. } | HarnessObservationKind::Settlement { .. }
    )));
}

#[test]
fn final_item_does_not_independently_prove_successful_terminal_lifecycle() {
    for text in ["result-held", "result-interrupted", "result-failed"] {
        let (_temp, launch) = fixture();
        let mut session = CodexSession::open(&launch, binding(), false).unwrap();
        session.submit(&submission(text)).unwrap();
        let events = result_fixture_events(&mut session, 16);
        assert!(
            events
                .iter()
                .any(|event| matches!(event.kind, HarnessObservationKind::Result { .. }))
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.kind, HarnessObservationKind::Settlement { .. }))
        );
        if text == "result-held" {
            assert_eq!(session.status().activity, HarnessActivity::Running);
        }
    }
}

#[test]
fn oversized_or_malformed_final_item_fences_instead_of_truncating_output() {
    for (text, expected) in [
        ("result-oversized", CodexError::Bounds),
        ("result-malformed", CodexError::Protocol),
    ] {
        let (_temp, launch) = fixture();
        let mut session = CodexSession::open(&launch, binding(), false).unwrap();
        session.submit(&submission(text)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let error = loop {
            match session.observe(1) {
                Ok((page, _)) => assert!(
                    !page
                        .iter()
                        .any(|event| matches!(event.kind, HarnessObservationKind::Result { .. }))
                ),
                Err(error) => break error,
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(error, expected);
        assert_eq!(session.observe(1).unwrap().1, HarnessCompleteness::Gap);
    }
}

#[test]
fn native_terminal_dispositions_are_complete_exact_and_never_assert_background_quiescence() {
    for (text, expected) in [
        ("terminal-completed", HarnessTerminalDisposition::Completed),
        ("terminal-failed", HarnessTerminalDisposition::Failed),
        (
            "terminal-interrupted",
            HarnessTerminalDisposition::Interrupted,
        ),
        ("terminal-background", HarnessTerminalDisposition::Completed),
        ("terminal-duplicate", HarnessTerminalDisposition::Completed),
    ] {
        for limit in [1, 16] {
            let (_temp, launch) = fixture();
            let mut session = CodexSession::open(&launch, binding(), false).unwrap();
            session.submit(&submission(text)).unwrap();
            let events = result_fixture_events(&mut session, limit);
            let terminals = events
                .iter()
                .filter(|event| {
                    matches!(
                        event.kind,
                        HarnessObservationKind::NativeTurnTerminal { .. }
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(terminals.len(), 1, "{text}");
            assert_eq!(terminals[0].completeness, HarnessCompleteness::Complete);
            assert_eq!(terminals[0].native_correlations, ["turn-1"]);
            assert_ne!(session.status().activity, HarnessActivity::Settled);
            assert_eq!(
                terminals[0].kind,
                HarnessObservationKind::NativeTurnTerminal {
                    native_turn_id: "turn-1".into(),
                    disposition: expected,
                }
            );
            assert!(!events.iter().any(|event| matches!(
                event.kind,
                HarnessObservationKind::Settlement { .. } | HarnessObservationKind::Result { .. }
            )));
            assert!(
                !serde_json::to_string(&events)
                    .unwrap()
                    .contains("sensitive native error")
            );
        }
    }
}

#[test]
fn foreign_and_stale_terminal_events_cannot_terminalize_the_accepted_turn() {
    for text in ["terminal-wrong-turn", "terminal-foreign"] {
        let (_temp, launch) = fixture();
        let mut session = CodexSession::open(&launch, binding(), false).unwrap();
        session.submit(&submission(text)).unwrap();
        let events = result_fixture_events(&mut session, 1);
        assert!(!events.iter().any(|event| matches!(
            event.kind,
            HarnessObservationKind::NativeTurnTerminal { .. }
                | HarnessObservationKind::Settlement { .. }
        )));
    }
}

#[test]
fn invalid_terminal_or_process_loss_never_supplies_terminal_or_settlement_proof() {
    for text in [
        "terminal-invalid",
        "terminal-contradictory",
        "terminal-lost",
    ] {
        let (_temp, launch) = fixture();
        let mut session = CodexSession::open(&launch, binding(), false).unwrap();
        session.submit(&submission(text)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match session.observe(1) {
                Ok((events, completeness)) => {
                    assert!(!events.iter().any(|event| matches!(
                        event.kind,
                        HarnessObservationKind::NativeTurnTerminal { .. }
                            | HarnessObservationKind::Settlement { .. }
                    )));
                    if completeness == HarnessCompleteness::Gap {
                        break;
                    }
                }
                Err(error) => {
                    assert_eq!(error, CodexError::Protocol);
                    break;
                }
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[test]
fn native_question_batch_is_answered_once_after_each_exact_question() {
    let (temp, launch) = fixture();
    let mut session = CodexSession::open(&launch, binding(), false).unwrap();
    session.submit(&submission("multi-question")).unwrap();
    let first = observations(&mut session, true)
        .into_iter()
        .find_map(|event| match event.kind {
            HarnessObservationKind::Interaction { interaction } => Some(interaction),
            _ => None,
        })
        .unwrap();
    assert_eq!(first.prompt, "Choose one");
    assert_eq!(first.choices[0].label, "First");
    let first_response = HarnessResponse {
        response_id: "answer-first".into(),
        request_id: first.request_id,
        session_id: "bcode-session".into(),
        generation: 7,
        value: HarnessResponseValue::Answer {
            choice_id: Some("option-0".into()),
            text: String::new(),
        },
    };
    session.respond(&first_response).unwrap();
    assert_eq!(session.respond(&first_response), Err(CodexError::Stale));
    assert!(!calls(&temp).iter().any(|call| call["callback"] == 91));
    let second = observations(&mut session, true)
        .into_iter()
        .find_map(|event| match event.kind {
            HarnessObservationKind::Interaction { interaction } => Some(interaction),
            _ => None,
        })
        .unwrap();
    assert_eq!(second.prompt, "Explain why");
    assert!(second.choices.is_empty());
    assert_ne!(second.request_id, first_response.request_id);
    let mut second_response = HarnessResponse {
        response_id: "answer-second".into(),
        request_id: second.request_id,
        session_id: "bcode-session".into(),
        generation: 6,
        value: HarnessResponseValue::Answer {
            choice_id: None,
            text: "Because".into(),
        },
    };
    assert_eq!(session.respond(&second_response), Err(CodexError::Stale));
    second_response.generation = 7;
    session.respond(&second_response).unwrap();
    assert_eq!(session.respond(&second_response), Err(CodexError::Stale));
    observations(&mut session, false);
    let replies = calls(&temp)
        .into_iter()
        .filter(|call| call["callback"] == 91)
        .collect::<Vec<_>>();
    assert_eq!(replies.len(), 1);
    assert_eq!(
        replies[0]["result"],
        serde_json::json!({"answers": {
            "q1": {"answers": ["First"]}, "q2": {"answers": ["Because"]}
        }})
    );
}

#[test]
fn cancelling_question_batch_discards_partial_answers() {
    let (temp, launch) = fixture();
    let mut session = CodexSession::open(&launch, binding(), false).unwrap();
    session.submit(&submission("multi-question")).unwrap();
    for (index, value) in [
        HarnessResponseValue::Answer {
            choice_id: None,
            text: "partial".into(),
        },
        HarnessResponseValue::Cancel,
    ]
    .into_iter()
    .enumerate()
    {
        let interaction = observations(&mut session, true)
            .into_iter()
            .find_map(|event| match event.kind {
                HarnessObservationKind::Interaction { interaction } => Some(interaction),
                _ => None,
            })
            .unwrap();
        session
            .respond(&HarnessResponse {
                response_id: format!("response-{index}"),
                request_id: interaction.request_id,
                session_id: "bcode-session".into(),
                generation: 7,
                value,
            })
            .unwrap();
    }
    observations(&mut session, false);
    let replies = calls(&temp)
        .into_iter()
        .filter(|call| call["callback"] == 91)
        .collect::<Vec<_>>();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["result"], serde_json::json!({"answers": {}}));
}

#[test]
fn native_resolution_revokes_only_the_matching_owned_callback() {
    for (text, revoked) in [
        ("resolved-question", true),
        ("foreign-resolved-question", false),
    ] {
        let (_temp, launch) = fixture();
        let mut session = CodexSession::open(&launch, binding(), false).unwrap();
        session.submit(&submission(text)).unwrap();
        let mut interaction = None;
        // The fixture sends resolution before this text marker, so draining through
        // the marker proves the notification was consumed without a timing guess.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut marker = false;
        while !marker && Instant::now() < deadline {
            for event in session.observe(16).unwrap().0 {
                match event.kind {
                    HarnessObservationKind::Interaction { interaction: value } => {
                        interaction = Some(value)
                    }
                    HarnessObservationKind::Text { .. } => marker = true,
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(marker);
        let response = HarnessResponse {
            response_id: "resolved-answer".into(),
            request_id: interaction.unwrap().request_id,
            session_id: "bcode-session".into(),
            generation: 7,
            value: HarnessResponseValue::Cancel,
        };
        assert_eq!(
            session.respond(&response),
            if revoked {
                Err(CodexError::Stale)
            } else {
                Ok(())
            }
        );
    }
}

#[test]
fn interrupt_requires_exact_retained_turn_and_observed_terminal_evidence() {
    let (temp, launch) = fixture();
    let mut session = CodexSession::open(&launch, binding(), false).unwrap();
    assert_eq!(session.interrupt_active(7), Err(CodexError::Uncertain));
    assert_eq!(session.interrupt(7, "turn-1"), Err(CodexError::Stale));
    session.submit(&submission("approval")).unwrap();
    let interaction = observations(&mut session, true)
        .into_iter()
        .find_map(|event| match event.kind {
            HarnessObservationKind::Interaction { interaction } => Some(interaction),
            _ => None,
        })
        .unwrap();
    assert_eq!(session.interrupt(6, "turn-1"), Err(CodexError::Stale));
    assert_eq!(session.interrupt(7, "wrong-turn"), Err(CodexError::Stale));
    session.interrupt_active(7).unwrap();
    // RPC acknowledgment alone must not mutate the observed lifecycle status.
    assert_ne!(session.status().activity, HarnessActivity::Interrupted);
    let events = observations(&mut session, false);
    assert!(events.iter().any(|event| matches!(&event.kind,
        HarnessObservationKind::Status { status } if status.activity == HarnessActivity::Interrupted)));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.kind, HarnessObservationKind::Settlement { .. }))
    );
    assert_eq!(
        session.respond(&HarnessResponse {
            response_id: "after-interrupt".into(),
            request_id: interaction.request_id,
            session_id: "bcode-session".into(),
            generation: 7,
            value: HarnessResponseValue::Approval { approved: true },
        }),
        Err(CodexError::Stale)
    );
    assert_eq!(session.interrupt_active(7), Err(CodexError::Uncertain));
    let interrupts = calls(&temp)
        .into_iter()
        .filter(|call| call["method"] == "turn/interrupt")
        .collect::<Vec<_>>();
    assert_eq!(interrupts.len(), 1);
    assert_eq!(interrupts[0]["turnId"], "turn-1");
}

#[test]
fn lost_interrupt_ack_remains_uncertain_without_successful_settlement() {
    let (temp, launch) = fixture();
    let mut session = CodexSession::open(&launch, binding(), false).unwrap();
    session.submit(&submission("approval")).unwrap();
    observations(&mut session, true);
    std::fs::write(temp.path().join("lost-interrupt-ack"), "").unwrap();
    assert_eq!(session.interrupt_active(7), Err(CodexError::Uncertain));
    let (events, completeness) = session.observe(16).unwrap();
    assert_eq!(completeness, HarnessCompleteness::Gap);
    assert!(events.is_empty());
    assert_ne!(session.status().activity, HarnessActivity::Interrupted);
    assert_eq!(session.interrupt_active(7), Err(CodexError::Uncertain));
    assert_eq!(
        calls(&temp)
            .iter()
            .filter(|call| call["method"] == "turn/interrupt")
            .count(),
        1
    );
}

#[test]
fn malformed_or_secret_question_batches_never_expose_a_partial_question() {
    for text in ["duplicate-question", "secret-question"] {
        let (_temp, launch) = fixture();
        let mut session = CodexSession::open(&launch, binding(), false).unwrap();
        session.submit(&submission(text)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut finished = false;
        while !finished && Instant::now() < deadline {
            match session.observe(16) {
                Ok((events, _)) => {
                    assert!(!events.iter().any(|event| matches!(
                        event.kind,
                        HarnessObservationKind::Interaction { .. }
                    )));
                    finished = events.iter().any(|event| {
                        matches!(
                            &event.kind,
                            HarnessObservationKind::NativeTurnTerminal { .. }
                        )
                    });
                }
                Err(error) => {
                    assert_eq!(text, "duplicate-question");
                    assert_eq!(error, CodexError::Protocol);
                    finished = true;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(finished);
    }
}

#[test]
fn unregistered_dynamic_tool_is_denied_not_dispatched_and_foreign_requests_are_ignored() {
    for text in ["dynamic", "foreign"] {
        let (_temp, launch) = fixture();
        let mut session = CodexSession::open(&launch, binding(), false).unwrap();
        session.submit(&submission(text)).unwrap();
        let events = observations(&mut session, false);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.kind, HarnessObservationKind::Interaction { .. }))
        );
    }
}

#[test]
fn acknowledgment_mapping_requires_exact_response_shape_and_retained_attempt() {
    for placement in [
        HarnessPlacement::NextTurn,
        HarnessPlacement::Steer {
            expected_native_turn_id: "turn-1".into(),
        },
    ] {
        let (_temp, launch) = fixture();
        let mut session = CodexSession::open(&launch, binding(), false).unwrap();
        let mut input = submission("wrong-ack-shape");
        input.placement = placement;
        assert_eq!(
            session.submit(&input).unwrap().state,
            HarnessDeliveryState::Uncertain
        );
        let (events, _) = session.observe(1).unwrap();
        assert!(!events.iter().any(|event| matches!(
            event.kind,
            HarnessObservationKind::SubmissionAccepted { .. }
        )));
    }
    let (_temp, launch) = fixture();
    let mut session = CodexSession::open(&launch, binding(), false).unwrap();
    let mut rejected = submission("rejected-turn");
    rejected.placement = HarnessPlacement::Steer {
        expected_native_turn_id: "wrong-turn".into(),
    };
    assert_eq!(
        session.submit(&rejected).unwrap().state,
        HarnessDeliveryState::Rejected
    );
    let events = observations(&mut session, false);
    assert!(!events.iter().any(|event| matches!(
        event.kind,
        HarnessObservationKind::SubmissionAccepted { .. }
    )));
}

#[test]
fn steering_mapping_is_drained_once_and_backpressures_unobserved_acceptance() {
    let (_temp, launch) = fixture();
    let mut session = CodexSession::open(&launch, binding(), false).unwrap();
    let input = submission("steer-mapping");
    let receipt = session.submit(&input).unwrap();
    assert_ne!(receipt.native_request_id.as_deref(), Some("turn-1"));
    assert_eq!(session.submit(&input).unwrap(), receipt);
    assert_eq!(
        session.submit(&submission("second-mapping")),
        Err(CodexError::Bounds)
    );
    let (events, _) = session.observe(1).unwrap();
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0].kind, HarnessObservationKind::SubmissionAccepted { submission_id, attempt_id, native_turn_id }
        if submission_id == &input.submission_id && attempt_id == &input.attempt_id && native_turn_id == "turn-1")
    );
    assert_eq!(events[0].session_id, input.session_id);
    assert_eq!(events[0].generation, input.generation);
    let remaining = observations(&mut session, false);
    assert!(!remaining.iter().any(|event| matches!(
        event.kind,
        HarnessObservationKind::SubmissionAccepted { .. }
    )));
}

#[test]
fn vault_key_requires_verified_ephemeral_store_before_delivery() {
    let (temp, mut launch) = fixture();
    launch.ephemeral_credentials = true;
    std::fs::write(temp.path().join("wrong-store"), "").unwrap();
    let result = CodexSession::open_with_key(&launch, binding(), false, Some("sensitive-key"));
    assert!(
        matches!(&result, Err(CodexError::Configuration)),
        "expected effective-storage policy rejection before key delivery, got {:?}",
        result.as_ref().err()
    );
    assert!(
        !calls(&temp)
            .iter()
            .any(|call| call["method"] == "account/login/start")
    );
}

#[test]
fn native_device_login_returns_sensitive_auth_effect_and_correlated_completion() {
    let (_temp, launch) = fixture();
    let binding = HarnessAuthBinding {
        provider_id: "codex".into(),
        profile: "native".into(),
        credential_namespace: launch.codex_home.to_string_lossy().into_owned(),
        custody: AuthCustodyMode::NativeManaged,
    };
    let mut auth = CodexAuth::open(&launch, binding).unwrap();
    let step = auth.begin("flow", "device").unwrap();
    assert!(step.interaction.is_some());
    assert!(!step.complete);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let step = auth.poll("flow").unwrap();
        if step.complete {
            assert_eq!(step.status.readiness, AuthReadiness::Ready);
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(auth.poll("flow").unwrap().complete);
}

#[test]
fn fresh_first_turn_consumes_construction_permit_and_later_next_turn_is_not_sent() {
    let (temp, launch) = fixture();
    let mut session = CodexSession::open(&launch, binding(), false).unwrap();
    let mut input = submission("first");
    input.placement = HarnessPlacement::NextTurn;
    let receipt = session.submit(&input).unwrap();
    assert_eq!(receipt.state, HarnessDeliveryState::NativeAccepted);
    assert_eq!(session.submit(&input).unwrap(), receipt); // lost-ack retry: no resend
    assert_ne!(receipt.native_request_id.as_deref(), Some("turn-1"));
    let (accepted, _) = session.observe(1).unwrap();
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].session_id, input.session_id);
    assert_eq!(accepted[0].generation, input.generation);
    assert!(
        matches!(&accepted[0].kind, HarnessObservationKind::SubmissionAccepted {
        submission_id, attempt_id, native_turn_id
    } if submission_id == &input.submission_id && attempt_id == &input.attempt_id && native_turn_id == "turn-1")
    );
    let mut second = submission("second");
    second.placement = HarnessPlacement::NextTurn;
    assert_eq!(session.submit(&second), Err(CodexError::Unsupported));
    let remaining = observations(&mut session, false);
    assert!(!remaining.iter().any(|event| matches!(
        event.kind,
        HarnessObservationKind::SubmissionAccepted { .. }
    )));
    assert_eq!(session.submit(&second), Err(CodexError::Unsupported)); // terminal is NOT an idle barrier
    let calls = calls(&temp);
    assert_eq!(
        calls
            .iter()
            .filter(|call| call["method"] == "turn/start")
            .count(),
        1
    );
    let disable = calls
        .iter()
        .position(|call| call["method"] == "remoteControl/disable")
        .unwrap();
    let verify = calls
        .iter()
        .position(|call| call["method"] == "remoteControl/status/read")
        .unwrap();
    let create = calls
        .iter()
        .position(|call| call["method"] == "thread/start")
        .unwrap();
    assert!(disable < verify && verify < create);
}

#[test]
fn resumed_thread_has_no_fresh_first_input_permit() {
    let (temp, launch) = fixture();
    let session = CodexSession::open(&launch, binding(), false).unwrap();
    let retained = session.binding().clone();
    drop(session);
    let mut resumed = CodexSession::open(&launch, retained, true).unwrap();
    let mut input = submission("resumed-next");
    input.placement = HarnessPlacement::NextTurn;
    assert_eq!(resumed.submit(&input), Err(CodexError::Unsupported));
    assert!(
        !calls(&temp)
            .iter()
            .any(|call| call["method"] == "turn/start")
    );
}

#[test]
fn fresh_first_turn_lost_ack_permanently_fences_and_is_never_replayed() {
    let (temp, launch) = fixture();
    let mut session = CodexSession::open(&launch, binding(), false).unwrap();
    let mut input = submission("lost-ack");
    input.placement = HarnessPlacement::NextTurn;
    assert_eq!(
        session.submit(&input).unwrap().state,
        HarnessDeliveryState::Uncertain
    );
    assert_eq!(
        session.submit(&input).unwrap().state,
        HarnessDeliveryState::Uncertain
    );
    assert_eq!(
        session.submit(&submission("later-steer")),
        Err(CodexError::Uncertain)
    );
    let mut second = submission("later-next");
    second.placement = HarnessPlacement::NextTurn;
    assert_eq!(session.submit(&second), Err(CodexError::Uncertain));
    assert_eq!(
        calls(&temp)
            .iter()
            .filter(|call| call["method"] == "turn/start")
            .count(),
        1
    );
}

#[test]
fn remote_control_must_be_disabled_before_create_and_activation_fences_input() {
    let (temp, launch) = fixture();
    std::fs::write(temp.path().join("remote-refuses-disable"), "").unwrap();
    assert!(matches!(
        CodexSession::open(&launch, binding(), false),
        Err(CodexError::Unsupported)
    ));
    assert!(
        !calls(&temp)
            .iter()
            .any(|call| call["method"] == "thread/start")
    );
    let (temp, launch) = fixture();
    std::fs::write(temp.path().join("remote-activates"), "").unwrap();
    match CodexSession::open(&launch, binding(), false) {
        Ok(mut session) => {
            std::thread::sleep(Duration::from_millis(100));
            let mut input = submission("remote-blocked");
            input.placement = HarnessPlacement::NextTurn;
            assert_eq!(session.submit(&input), Err(CodexError::Uncertain));
        }
        Err(error) => assert_eq!(error, CodexError::Uncertain),
    }
    assert!(
        !calls(&temp)
            .iter()
            .any(|call| call["method"] == "turn/start")
    );
}

#[test]
fn failed_and_interrupted_turns_are_not_successful_settlement() {
    for text in ["failed", "interrupted"] {
        let (_temp, launch) = fixture();
        let mut session = CodexSession::open(&launch, binding(), false).unwrap();
        session.submit(&submission(text)).unwrap();
        let events = observations(&mut session, false);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.kind, HarnessObservationKind::Settlement { .. }))
        );
        assert!(
            !serde_json::to_string(&events)
                .unwrap()
                .contains("sensitive native error")
        );
        if text == "failed" {
            assert!(events.iter().any(|event| matches!(&event.kind, HarnessObservationKind::Diagnostic { code, .. } if code == "native_turn_failed")));
        } else {
            assert_eq!(session.status().activity, HarnessActivity::Interrupted);
        }
    }
}

#[test]
fn server_callback_overflow_fails_closed_with_gap_and_never_settles() {
    let (_temp, launch) = fixture();
    let mut session = CodexSession::open(&launch, binding(), false).unwrap();
    session.submit(&submission("overflow")).unwrap();
    let (accepted, _) = session.observe(1).unwrap();
    assert!(matches!(
        accepted[0].kind,
        HarnessObservationKind::SubmissionAccepted { .. }
    ));
    // Allow continuous reader to hit its bound without observe driving the pipe.
    std::thread::sleep(Duration::from_millis(150));
    let (events, completeness) = session.observe(32).unwrap();
    assert_eq!(completeness, HarnessCompleteness::Gap);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.kind, HarnessObservationKind::Settlement { .. }))
    );
    assert!(matches!(
        session.submit(&submission("after-gap")),
        Ok(HarnessReceipt {
            state: HarnessDeliveryState::Uncertain,
            ..
        }) | Err(CodexError::Uncertain)
    ));
}

#[test]
fn blocked_writer_timeout_fences_later_delivery_and_drop_does_not_wait() {
    let (temp, mut launch) = fixture();
    launch.timeout = Duration::from_millis(250);
    std::fs::write(temp.path().join("stop-reading"), "").unwrap();
    let mut session = CodexSession::open(&launch, binding(), false).unwrap();
    let mut input = submission("blocked");
    input.text = "x".repeat(60 * 1024); // exceeds pipe capacity, within semantic input bound
    let started = Instant::now();
    assert_eq!(
        session.submit(&input).unwrap().state,
        HarnessDeliveryState::Uncertain
    );
    assert_eq!(
        session.submit(&submission("must-not-send")),
        Err(CodexError::Uncertain)
    );
    drop(session);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(
        !calls(&temp)
            .iter()
            .any(|call| call["method"] == "turn/steer")
    );
}

#[test]
fn login_cancel_distinguishes_not_found_and_does_not_replay() {
    use bcode_provider_auth_models::managed::ManagedAuthFailure;
    for not_found in [false, true] {
        let (temp, launch) = fixture();
        std::fs::write(temp.path().join("pending-login"), "").unwrap();
        if not_found {
            std::fs::write(temp.path().join("cancel-not-found"), "").unwrap();
        }
        let binding = HarnessAuthBinding {
            provider_id: "codex".into(),
            profile: "native".into(),
            credential_namespace: launch.codex_home.to_string_lossy().into_owned(),
            custody: AuthCustodyMode::NativeManaged,
        };
        let mut auth = CodexAuth::open(&launch, binding).unwrap();
        auth.begin("flow", "device").unwrap();
        let canceled = auth.cancel("flow").unwrap();
        assert_eq!(
            canceled.status.readiness,
            if not_found {
                AuthReadiness::Blocked(ManagedAuthFailure::Uncertain)
            } else {
                AuthReadiness::LoginRequired
            }
        );
        assert_eq!(auth.cancel("flow").unwrap().status, canceled.status);
        assert_eq!(
            calls(&temp)
                .iter()
                .filter(|call| call["method"] == "account/login/cancel")
                .count(),
            1
        );
    }
}

#[test]
fn correlated_login_completion_survives_followup_status_failure() {
    let (temp, launch) = fixture();
    std::fs::write(temp.path().join("fail-status"), "").unwrap();
    let binding = HarnessAuthBinding {
        provider_id: "codex".into(),
        profile: "native".into(),
        credential_namespace: launch.codex_home.to_string_lossy().into_owned(),
        custody: AuthCustodyMode::NativeManaged,
    };
    let mut auth = CodexAuth::open(&launch, binding).unwrap();
    auth.begin("flow", "browser").unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let step = auth.poll("flow").unwrap();
        if step.complete {
            assert_eq!(step.status.readiness, AuthReadiness::Unknown);
            assert!(auth.poll("flow").unwrap().complete);
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
}
