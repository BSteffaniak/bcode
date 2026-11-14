use super::*;
use bcode_harness_models::{
    HarnessExecutionEvent, HarnessHydrationReply, HarnessHydrationResolution,
    HarnessNotificationState, HarnessObservationKind, HarnessWorkState,
};

fn normalizer() -> Normalizer {
    let binding = bcode_harness_models::HarnessSessionBinding {
        schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
        session_id: "session".into(),
        instance_id: "instance".into(),
        native_session_id: "native".into(),
        native_version: "2.1.308 (Claude Code)".into(),
        generation: 7,
    };
    let mut normalizer = Normalizer::new(binding);
    normalizer.configure_execution(true, true);
    normalizer.enable_retention();
    normalizer
}
fn system(mut value: Value) -> Event {
    value["type"] = json!("system");
    Event::Message(NativeMessage::System(value))
}
fn key(observation: &HarnessObservation) -> Option<String> {
    match &observation.kind {
        HarnessObservationKind::Execution {
            event: HarnessExecutionEvent::BackgroundWork { work_id, .. },
        } => Some(format!("work:{work_id}")),
        HarnessObservationKind::Execution {
            event:
                HarnessExecutionEvent::Continuation {
                    continuation_id, ..
                },
        } => Some(format!("followup:{continuation_id}")),
        _ => None,
    }
}
fn drain(
    normalizer: &mut Normalizer,
    canonical: &mut std::collections::BTreeMap<String, HarnessObservation>,
) -> Vec<HarnessObservation> {
    let mut collected = Vec::new();
    for _ in 0..10000 {
        let observations = normalizer.pump();
        if let Some(request) = normalizer.hydration_requests().first().cloned() {
            let matching: Vec<_> = canonical
                .values()
                .filter(|item| request.reference.matches_observation(item))
                .cloned()
                .collect();
            let work_ids: Vec<_> = matching
                .iter()
                .filter_map(|item| match &item.kind {
                    HarnessObservationKind::Execution {
                        event: HarnessExecutionEvent::BackgroundWork { work_id, .. },
                    } => Some(work_id.clone()),
                    _ => None,
                })
                .collect();
            let mut found = matching;
            found.extend(canonical.values().filter(|item| matches!(&item.kind,
                HarnessObservationKind::Execution { event: HarnessExecutionEvent::Continuation { source_work_id: Some(id), .. } } if work_ids.contains(id))).cloned());
            let reply = HarnessHydrationReply {
                request_id: request.request_id,
                reference: request.reference,
                canonical_revision: 1,
                resolution: if found.is_empty() {
                    HarnessHydrationResolution::Absent
                } else {
                    HarnessHydrationResolution::Found {
                        observations: found,
                    }
                },
            };
            normalizer.apply_hydration(&[reply]).unwrap();
            continue;
        }
        for observation in &observations {
            if let Some(key) = key(observation) {
                canonical.insert(key, observation.clone());
            }
        }
        for observation in &observations {
            normalizer.acknowledge_execution(&observation.observation_id);
            normalizer.acknowledge_record(observation);
        }
        collected.extend(observations);
        if normalizer.staging.events.is_empty() {
            break;
        }
    }
    assert!(normalizer.staging.events.is_empty());
    collected
}
#[test]
fn snapshot_over_32_is_positive_paged_and_1100_tasks_do_not_cap_one_attempt() {
    let mut normalizer = normalizer();
    normalizer
        .retain_submission(&bcode_harness_models::HarnessSubmission {
            submission_id: "long-attempt".into(),
            attempt_id: "00000000-0000-4000-8000-000000000001".into(),
            session_id: "session".into(),
            generation: 7,
            text: "observe native work".into(),
            placement: bcode_harness_models::HarnessPlacement::NextTurn,
        })
        .unwrap();
    let mut canonical = std::collections::BTreeMap::new();
    normalizer.event(system(json!({"subtype":"init"})));
    drain(&mut normalizer, &mut canonical);
    let tasks: Vec<_> = (0..80).map(|index| json!({"task_id":format!("task-{index}"),"run_id":format!("run-{index}"),"task_type":"local_bash"})).collect();
    normalizer.event(system(
        json!({"subtype":"background_tasks_changed","tasks":tasks}),
    ));
    let observations = drain(&mut normalizer, &mut canonical);
    assert_eq!(
        observations
            .iter()
            .filter(|item| matches!(
                item.kind,
                HarnessObservationKind::Execution {
                    event: HarnessExecutionEvent::BackgroundWork { .. }
                }
            ))
            .count(),
        80
    );
    assert_eq!(
        normalizer.inventory_page().unwrap().consistency,
        HarnessInventoryConsistency::NonAtomic
    );
    assert!(normalizer.inventory_page().unwrap().next_cursor.is_none());
    for index in 80..1100 {
        normalizer.event(system(json!({"subtype":"task_started","task_id":format!("task-{index}"),"run_id":format!("run-{index}"),"task_type":"local_bash"})));
        drain(&mut normalizer, &mut canonical);
    }
    assert!(!normalizer.execution.has_cached_work());
    assert_eq!(canonical.len(), 1100);
    normalizer.event(system(
        json!({"subtype":"background_tasks_changed","tasks":[]}),
    ));
    drain(&mut normalizer, &mut canonical);
    assert!(canonical.values().all(|item| matches!(
        item.kind,
        HarnessObservationKind::Execution {
            event: HarnessExecutionEvent::BackgroundWork {
                state: HarnessWorkState::Running,
                notification: HarnessNotificationState::Pending,
                ..
            }
        }
    )));
}
#[test]
fn late_notification_and_exact_causal_result_hydrate_original_identity_and_debt() {
    let mut normalizer = normalizer();
    let mut canonical = std::collections::BTreeMap::new();
    normalizer.event(system(
        json!({"subtype":"task_started","task_id":"shell","run_id":"r1","task_type":"local_bash"}),
    ));
    drain(&mut normalizer, &mut canonical);
    let original = canonical.values().next().unwrap().clone();
    normalizer.event(system(
        json!({"subtype":"task_notification","task_id":"shell","run_id":"r1","status":"failed"}),
    ));
    let observations = drain(&mut normalizer, &mut canonical);
    assert!(observations.iter().any(|item| matches!(&item.kind, HarnessObservationKind::Execution {
        event: HarnessExecutionEvent::BackgroundWork { work_id, state: HarnessWorkState::Failed, notification: HarnessNotificationState::Observed, .. }}
        if matches!(&original.kind, HarnessObservationKind::Execution {event: HarnessExecutionEvent::BackgroundWork {work_id: old,..}} if old == work_id))));
    normalizer.event(system(
        json!({"subtype":"background_tasks_changed","tasks":[]}),
    ));
    drain(&mut normalizer, &mut canonical);
    assert!(canonical.values().any(|item| matches!(
        item.kind,
        HarnessObservationKind::Execution {
            event: HarnessExecutionEvent::Continuation {
                state: HarnessWorkState::Pending,
                ..
            }
        }
    )));
    normalizer.event(Event::Message(NativeMessage::Result(json!({"type":"result","uuid":"wake-result","subtype":"success","is_error":false,"result":"wake output",
        "origin":{"kind":"task-notification","producer":"session-task","runId":"r1"}}))));
    // Task-run lookup needs a shared namespace contract. Process/Agent references
    // match task IDs, not run IDs; never fabricate a native turn or item identity.
    normalizer.pump();
    let pending = normalizer.hydration_requests();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].reference.native_id, "r1");
    assert!(
        !canonical
            .values()
            .any(|item| pending[0].reference.matches_observation(item))
    );
    assert!(canonical.values().any(|item| matches!(
        item.kind,
        HarnessObservationKind::Execution {
            event: HarnessExecutionEvent::Continuation {
                state: HarnessWorkState::Pending,
                ..
            }
        }
    )));
    assert!(canonical.values().any(|item| matches!(
        item.kind,
        HarnessObservationKind::Execution {
            event: HarnessExecutionEvent::BackgroundWork {
                state: HarnessWorkState::Failed,
                ..
            }
        }
    )));
}
#[test]
fn conflicting_canonical_hydration_is_atomic_and_does_not_reopen_terminal_work() {
    let mut normalizer = normalizer();
    let mut canonical = std::collections::BTreeMap::new();
    normalizer.event(system(json!({"subtype":"task_notification","task_id":"shell","run_id":"r1","status":"failed","task_type":"local_bash"})));
    drain(&mut normalizer, &mut canonical);
    normalizer.event(system(
        json!({"subtype":"task_started","task_id":"shell","run_id":"r1","task_type":"local_bash"}),
    ));
    normalizer.pump();
    let request = normalizer.hydration_requests().pop().unwrap();
    let observations: Vec<_> = canonical.values().cloned().collect();
    let reply = HarnessHydrationReply {
        request_id: request.request_id.clone(),
        reference: request.reference,
        canonical_revision: 1,
        resolution: HarnessHydrationResolution::Found { observations },
    };
    normalizer
        .apply_hydration(std::slice::from_ref(&reply))
        .unwrap();
    let output = normalizer.pump();
    assert!(output.iter().any(|item| matches!(
        item.kind,
        HarnessObservationKind::Execution {
            event: HarnessExecutionEvent::BackgroundWork {
                state: HarnessWorkState::Failed,
                ..
            }
        }
    )));
    for item in &output {
        normalizer.acknowledge_execution(&item.observation_id);
    }
    let mut conflicting = canonical
        .values()
        .filter(|item| {
            matches!(
                item.kind,
                HarnessObservationKind::Execution {
                    event: HarnessExecutionEvent::BackgroundWork { .. }
                }
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    if let HarnessObservationKind::Execution {
        event: HarnessExecutionEvent::BackgroundWork { state, .. },
    } = &mut conflicting[0].kind
    {
        *state = HarnessWorkState::Running;
    }
    let invalid = HarnessHydrationReply {
        resolution: HarnessHydrationResolution::Found {
            observations: conflicting,
        },
        ..reply
    };
    assert!(normalizer.execution.hydrate(&invalid).is_err());
}

#[test]
fn genuine_global_tool_id_hydrates_without_inventing_turn_and_supplied_turn_stays_exact() {
    use bcode_harness_models::{HarnessNativeReference, HarnessNativeReferenceKind};
    let mut normalizer = normalizer();
    let item = HarnessObservation {
        observation_id: "native-work".into(),
        session_id: "session".into(),
        generation: 7,
        native_correlations: vec![
            "tool-global-1".into(),
            "claude-tool:tool-global-1".into(),
            "claude-run:run-1".into(),
        ],
        completeness: bcode_harness_models::HarnessCompleteness::Complete,
        kind: HarnessObservationKind::Execution {
            event: HarnessExecutionEvent::BackgroundWork {
                work_id: "canonical-work-1".into(),
                native_task_id: Some("shell-1".into()),
                origin: None,
                parent_work_id: None,
                work_kind: bcode_harness_models::HarnessWorkKind::Command,
                label: "Native background work".into(),
                state: HarnessWorkState::Running,
                notification: HarnessNotificationState::Pending,
            },
        },
    };
    let reference = HarnessNativeReference {
        kind: HarnessNativeReferenceKind::Item,
        native_id: "tool-global-1".into(),
        native_turn_id: None,
    };
    reference.validate().unwrap();
    assert!(reference.matches_observation(&item));
    let scoped = HarnessNativeReference {
        native_turn_id: Some("genuine-native-turn".into()),
        ..reference.clone()
    };
    assert!(!scoped.matches_observation(&item));
    let mut scoped_evidence = item.clone();
    scoped_evidence
        .native_correlations
        .push("genuine-native-turn".into());
    assert!(scoped.matches_observation(&scoped_evidence));
    assert!(
        !HarnessNativeReference {
            native_turn_id: Some("different-turn".into()),
            ..reference.clone()
        }
        .matches_observation(&scoped_evidence)
    );
    normalizer.event(Event::Message(NativeMessage::User(json!({"type":"user",
        "tool_use_result":{"backgroundTaskId":"shell-1"},
        "message":{"content":[{"type":"tool_result","tool_use_id":"tool-global-1"}]}}))));
    normalizer.pump();
    let request = normalizer.hydration_requests().pop().unwrap();
    assert_eq!(request.reference, reference);
    normalizer
        .apply_hydration(&[HarnessHydrationReply {
            request_id: request.request_id,
            reference,
            canonical_revision: 1,
            resolution: HarnessHydrationResolution::Found {
                observations: vec![item],
            },
        }])
        .unwrap();
    let observations = normalizer.pump();
    assert!(observations.iter().any(|observation| matches!(&observation.kind,
        HarnessObservationKind::Execution { event: HarnessExecutionEvent::BackgroundWork { work_id, .. } }
            if work_id == "canonical-work-1")));
}

#[test]
fn a_foreign_reply_is_rejected_and_an_unanswerable_lookup_never_holds_the_stream() {
    let mut normalizer = normalizer();
    normalizer.event(system(
        json!({"subtype":"task_notification","task_id":"unknown","status":"completed"}),
    ));
    normalizer.pump();
    let request = normalizer.hydration_requests().pop().unwrap();
    let mut reply = HarnessHydrationReply {
        request_id: "foreign".into(),
        reference: request.reference,
        canonical_revision: 1,
        resolution: HarnessHydrationResolution::Absent,
    };
    assert!(normalizer.apply_hydration(&[reply.clone()]).is_err());
    reply.request_id = request.request_id;
    reply.resolution = HarnessHydrationResolution::Unavailable;
    normalizer.apply_hydration(&[reply]).unwrap();
    // The lookup is given up, not retried forever: the staged record is processed with
    // background coverage unknown, and nothing else is requested for it.
    assert_eq!(normalizer.hydration_requests(), Vec::new());
    let processed = normalizer.pump();
    assert!(
        processed.iter().any(|observation| matches!(
            &observation.kind,
            HarnessObservationKind::Subagent { native_agent_id, .. } if native_agent_id == "unknown"
        )),
        "the held task notification is processed: {processed:?}"
    );
    assert!(normalizer.pump().is_empty() || normalizer.hydration_requests().is_empty());
}
