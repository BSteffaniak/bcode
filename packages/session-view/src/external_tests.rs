use super::*;
use bcode_harness_models::{
    HARNESS_SCHEMA_VERSION, HarnessAuthBinding, HarnessAuthCustody, HarnessChoice,
    HarnessDeliveryState, HarnessReceipt,
};
use bcode_session_models::external_backend::{
    ExternalBackendEnvelope, ExternalBackendPage, ExternalBackendRecord,
};
use bcode_session_models::{CURRENT_SESSION_EVENT_SCHEMA_VERSION, SessionEvent, SessionId};

fn source() -> (HarnessInstance, HarnessSessionBinding) {
    (
        HarnessInstance {
            instance_id: "work".into(),
            plugin_id: "disabled-plugin".into(),
            executable: "/native/agent".into(),
            config_namespace: "/config/isolated".into(),
            auth: HarnessAuthBinding {
                provider_id: "native-auth".into(),
                profile: "work".into(),
                credential_namespace: "isolated".into(),
                custody: HarnessAuthCustody::NativeManaged,
            },
        },
        HarnessSessionBinding {
            schema_version: HARNESS_SCHEMA_VERSION,
            session_id: SessionId::new().to_string(),
            instance_id: "work".into(),
            native_session_id: "native".into(),
            native_version: "1.0".into(),
            generation: 7,
        },
    )
}

#[test]
fn native_acceptance_is_visible_but_never_completion_or_tool_execution() {
    let (instance, binding) = source();
    let (provenance, _) = external_metadata(&instance, &binding).unwrap();
    let observed = observation(
        &binding,
        HarnessObservationKind::SubmissionAccepted {
            submission_id: "input-7".into(),
            attempt_id: "attempt-2".into(),
            native_turn_id: "native-turn".into(),
        },
    );
    let item = external_transcript_item(&observed, 1, &provenance).unwrap();
    assert_eq!(item.category, ExternalTranscriptCategory::Status);
    assert!(item.text.contains("input-7"));
    assert!(item.text.contains("attempt-2"));
    assert!(item.text.contains("native-turn"));
    assert!(
        item.text
            .contains("not settlement, assignment completion, or replay authority")
    );
    let mut view = crate::SessionView::new();
    view.apply_event(&event(
        &binding,
        1,
        ExternalBackendRecord::Observation {
            observation: observed,
        },
    ));
    // Acceptance is lifecycle evidence, not conversation content.
    assert_eq!(view.snapshot().transcript.items.len(), 0);
    assert_eq!(view.snapshot().latest_sequence, Some(1));
    assert!(view.snapshot().tools.is_empty());
    assert_eq!(view.snapshot().permissions, Vec::new());
}

fn capabilities() -> HarnessCapabilities {
    let capability = HarnessCapability {
        support: HarnessSupportLevel::Supported,
        constraints: vec![],
    };
    HarnessCapabilities {
        model_switch: bcode_harness_models::HarnessCapability::unsupported(),
        permission_mode: bcode_harness_models::HarnessCapability::unsupported(),
        resume: capability.clone(),
        steering: capability.clone(),
        approval: capability.clone(),
        questions: capability.clone(),
        assistance: capability.clone(),
        interrupt: capability.clone(),
        authoritative_settlement: capability.clone(),
        background_continuation: capability.clone(),
        authentication: capability,
    }
}

fn event(
    binding: &HarnessSessionBinding,
    sequence: u64,
    record: ExternalBackendRecord,
) -> SessionEvent {
    SessionEvent {
        schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        session_id: binding.session_id.parse().unwrap(),
        sequence,
        timestamp_ms: 1,
        provenance: None,
        kind: ExternalBackendEnvelope { version: 1, record }
            .into_event_kind()
            .unwrap(),
    }
}

fn observation(
    binding: &HarnessSessionBinding,
    kind: HarnessObservationKind,
) -> HarnessObservation {
    HarnessObservation {
        observation_id: "observation".into(),
        session_id: binding.session_id.clone(),
        generation: binding.generation,
        native_correlations: vec![],
        completeness: HarnessCompleteness::Partial,
        kind,
    }
}

#[test]
fn disabled_plugin_history_needs_no_native_resume_and_tools_are_only_observations() {
    let (instance, binding) = source();
    let page = ExternalBackendPage {
        events: vec![event(
            &binding,
            9,
            ExternalBackendRecord::Observation {
                observation: observation(
                    &binding,
                    HarnessObservationKind::Tool {
                        input_json: None,
                        native_tool_id: "tool".into(),
                        name: "shell".into(),
                        status: "observed".into(),
                    },
                ),
            },
        )],
        next_after_sequence: Some(9),
        has_more: true,
    };
    let view = external_history_page(&instance, &binding, &[], &page).unwrap();
    assert_eq!(view.actions, Vec::new());
    assert_eq!(view.interactions, Vec::new());
    assert_eq!(
        view.transcript[0].category,
        ExternalTranscriptCategory::NativeTool
    );
    assert!(transcript_fallback(&view.transcript[0]).contains("not executed by Bcode"));
    assert!(transcript_fallback(&view.transcript[0]).contains("disabled-plugin"));
    assert!(view.has_more);
    assert_eq!(view.next_after_sequence, Some(9));
}

#[test]
fn idle_result_and_interrupt_never_claim_assignment_completion() {
    let (_, binding) = source();
    let provenance = ExternalProvenanceView {
        session_id: binding.session_id.clone(),
        instance_id: binding.instance_id.clone(),
        plugin_id: "plugin".into(),
        native_session_id: binding.native_session_id.clone(),
        generation: binding.generation,
    };
    let item = external_transcript_item(
        &observation(
            &binding,
            HarnessObservationKind::Result {
                text: "done".into(),
            },
        ),
        3,
        &provenance,
    )
    .unwrap();
    assert!(item.text.contains("not assignment completion"));
    let status = external_activity(&HarnessExecutionStatus {
        connectivity: HarnessConnectivity::Disconnected,
        activity: HarnessActivity::Idle,
        attention: HarnessAttention::Assistance,
        continuation: HarnessContinuation::Unavailable,
        completeness: HarnessCompleteness::Gap,
    });
    assert!(status.execution.contains("not assignment completion"));
    assert!(status.attention.contains("assistance"));
    assert!(status.continuation.contains("history remains readable"));
    assert_eq!(status.completeness, "observation gap");
}

#[test]
fn question_approval_and_assistance_require_current_application_admission() {
    let (instance, binding) = source();
    let (provenance, _) = external_metadata(&instance, &binding).unwrap();
    for kind in [
        HarnessInteractionKind::Question,
        HarnessInteractionKind::Approval,
        HarnessInteractionKind::Assistance,
    ] {
        let interaction = HarnessInteraction {
            input_preview: None,
            tool_name: None,
            request_id: "request".into(),
            session_id: binding.session_id.clone(),
            generation: binding.generation,
            kind,
            prompt: "Need input".into(),
            scope: "native scope".into(),
            choices: vec![HarnessChoice {
                choice_id: "choice".into(),
                label: "Choice".into(),
            }],
        };
        for availability in [
            ExternalInteractionAvailability::Historical,
            ExternalInteractionAvailability::Stale,
            ExternalInteractionAvailability::Unavailable,
            ExternalInteractionAvailability::NotAuthorized,
        ] {
            let view =
                external_interaction(&interaction, &provenance, &capabilities(), availability)
                    .unwrap();
            assert!(!view.response.enabled);
            assert_ne!(view.response.reason, "");
            assert_eq!(view.scope, "native scope");
        }
        assert!(
            external_interaction(
                &interaction,
                &provenance,
                &capabilities(),
                ExternalInteractionAvailability::Admitted
            )
            .unwrap()
            .response
            .enabled
        );
    }
}

#[test]
fn unknown_capability_and_stale_provenance_fail_closed() {
    let capability = HarnessCapability {
        support: HarnessSupportLevel::Unknown,
        constraints: vec![],
    };
    assert!(
        !external_action(
            ExternalActionKind::Steer,
            &capability,
            ExternalInteractionAvailability::Admitted
        )
        .enabled
    );
    let capability = HarnessCapability {
        support: HarnessSupportLevel::Constrained,
        constraints: vec!["exact expected native turn required".into()],
    };
    let action = external_action(
        ExternalActionKind::Steer,
        &capability,
        ExternalInteractionAvailability::Admitted,
    );
    assert!(action_label(&action).contains("exact expected native turn"));
    let (instance, binding) = source();
    let (mut provenance, _) = external_metadata(&instance, &binding).unwrap();
    provenance.generation += 1;
    assert!(
        external_transcript_item(
            &observation(
                &binding,
                HarnessObservationKind::Text {
                    segment_id: None,
                    text: "x".into()
                }
            ),
            1,
            &provenance
        )
        .is_err()
    );
}

#[test]
fn uncertain_receipt_history_never_offers_mutation_replay() {
    let (instance, binding) = source();
    let receipt = HarnessReceipt {
        submission_id: "submission".into(),
        attempt_id: "attempt".into(),
        session_id: binding.session_id.clone(),
        generation: binding.generation,
        state: HarnessDeliveryState::Uncertain,
        native_request_id: None,
    };
    let view = external_history_page(
        &instance,
        &binding,
        &[],
        &ExternalBackendPage {
            events: vec![event(
                &binding,
                1,
                ExternalBackendRecord::Receipt { receipt },
            )],
            next_after_sequence: None,
            has_more: false,
        },
    )
    .unwrap();
    assert_eq!(view.actions, Vec::new());
    assert!(view.transcript[0].text.contains("must not replay"));
}

#[test]
fn malformed_or_oversized_history_is_rejected_without_hiding_missing_entries() {
    let (instance, binding) = source();
    let entry = event(
        &binding,
        1,
        ExternalBackendRecord::Observation {
            observation: observation(
                &binding,
                HarnessObservationKind::Text {
                    segment_id: None,
                    text: "hello".into(),
                },
            ),
        },
    );
    let page = ExternalBackendPage {
        events: vec![entry.clone(), entry.clone()],
        next_after_sequence: None,
        has_more: false,
    };
    assert!(external_history_page(&instance, &binding, &[], &page).is_err());
    let page = ExternalBackendPage {
        events: vec![entry; MAX_EXTERNAL_VIEW_ITEMS + 1],
        next_after_sequence: None,
        has_more: true,
    };
    assert!(external_history_page(&instance, &binding, &[], &page).is_err());
}

#[test]
fn retained_old_generation_remains_readable_but_cannot_replace_current_activity() {
    let (instance, mut current) = source();
    let mut old = current.clone();
    old.generation = 6;
    old.native_session_id = "old-native".into();
    current.native_session_id = "current-native".into();
    let entry = event(
        &old,
        1,
        ExternalBackendRecord::Observation {
            observation: observation(
                &old,
                HarnessObservationKind::Status {
                    status: HarnessExecutionStatus {
                        connectivity: HarnessConnectivity::Connected,
                        activity: HarnessActivity::Settled,
                        attention: HarnessAttention::None,
                        continuation: HarnessContinuation::Available,
                        completeness: HarnessCompleteness::Complete,
                    },
                },
            ),
        },
    );
    let view = external_history_page(
        &instance,
        &current,
        &[(instance.clone(), old)],
        &ExternalBackendPage {
            events: vec![entry],
            next_after_sequence: None,
            has_more: false,
        },
    )
    .unwrap();
    assert_eq!(
        view.transcript[0].provenance.native_session_id,
        "old-native"
    );
    assert_eq!(view.provenance.native_session_id, "current-native");
    assert_eq!(view.activity.execution, "execution unknown");
}

#[test]
fn shared_fallback_preserves_source_and_removes_terminal_controls() {
    let (instance, binding) = source();
    let view = external_history_page(
        &instance,
        &binding,
        &[],
        &ExternalBackendPage {
            events: vec![event(
                &binding,
                1,
                ExternalBackendRecord::Observation {
                    observation: observation(
                        &binding,
                        HarnessObservationKind::Text {
                            segment_id: None,
                            text: "hello\u{1b}[31m界👩‍💻e\u{301}".into(),
                        },
                    ),
                },
            )],
            next_after_sequence: None,
            has_more: false,
        },
    )
    .unwrap();
    let blocks = external_fallback_blocks(&view).unwrap();
    let metadata = blocks
        .iter()
        .find(|block| block.id == "external:metadata")
        .unwrap();
    assert!(metadata.text.contains("disabled-plugin"));
    let observation = blocks
        .iter()
        .find(|block| block.id.contains("observation"))
        .unwrap();
    assert!(!observation.text.contains('\u{1b}'));
    assert!(observation.text.contains("界👩‍💻e\u{301}"));
    assert_eq!(view.transcript[0].text, "hello\u{1b}[31m界👩‍💻e\u{301}");
}

#[test]
fn prepared_selection_is_not_a_confirmed_native_conversation() {
    let (_, binding) = source();
    let prepared = bcode_harness_models::HarnessPreparedSession {
        open_id: "open".into(),
        session_id: binding.session_id.clone(),
        instance_id: binding.instance_id.clone(),
        generation: binding.generation,
        native_version: binding.native_version.clone(),
        requested_native_session_id: Some("requested-only".into()),
    };
    let text = prepared_session_text(&prepared).unwrap();
    assert!(text.contains("requested-only (unconfirmed)"));
    assert!(text.contains("No native conversation identity confirmed"));
    assert!(text.contains("does not mean the conversation was resumed, created, or settled"));
    assert!(text.contains("instead of recreating it or replaying input"));
    assert!(!text.contains("/native send"));
    let (instance, _) = source();
    let (provenance, _) = external_metadata(&instance, &binding).unwrap();
    let observation = observation(
        &binding,
        HarnessObservationKind::BindingConfirmed {
            open_id: "open".into(),
            binding: binding.clone(),
        },
    );
    let item = external_transcript_item(&observation, 1, &provenance).unwrap();
    assert!(item.text.contains("Native identity confirmed"));
    assert!(item.text.contains("not settlement"));
}

#[test]
fn prepared_service_outcome_is_visible_in_real_session_view_without_claiming_identity() {
    use bcode_session_models::external_harness::{
        ExternalHarnessOperation, ExternalHarnessOperationRecord, ExternalHarnessOwner,
        ExternalHarnessRequest, ExternalHarnessRequestKind, ExternalHarnessResponse,
        ExternalHarnessResponseKind,
    };
    let (instance, binding) = source();
    let session_id = binding.session_id.parse().unwrap();
    let prepared = bcode_harness_models::HarnessPreparedSession {
        open_id: "open".into(),
        session_id: binding.session_id.clone(),
        instance_id: binding.instance_id.clone(),
        generation: binding.generation,
        native_version: binding.native_version.clone(),
        requested_native_session_id: Some("requested-only".into()),
    };
    let request = ExternalHarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "open".into(),
        instance,
        kind: ExternalHarnessRequestKind::Open(bcode_harness_models::HarnessOpenRequest {
            open_id: "open".into(),
            session_id: binding.session_id.clone(),
            generation: binding.generation,
            working_directory: "/workspace".into(),
            resume: None,
            parent_assistance: None,
        }),
    };
    let operation = ExternalHarnessOperation {
        inventory_commitments: Vec::new(),
        version: 1,
        owner: ExternalHarnessOwner {
            session_id,
            generation: binding.generation,
            artifact_id: "artifact".into(),
            daemon_id: "daemon".into(),
        },
        record: ExternalHarnessOperationRecord::DeliveryCompleted {
            request,
            response: ExternalHarnessResponse {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: "open".into(),
                operation: bcode_harness_models::HarnessOperation::Open,
                kind: ExternalHarnessResponseKind::Prepared(prepared),
            },
        },
    };
    let event = SessionEvent {
        schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        session_id,
        sequence: 1,
        timestamp_ms: 0,
        provenance: None,
        kind: bcode_session_models::SessionEventKind::ExternalHarnessOperation {
            operation: Box::new(operation),
        },
    };
    let mut view = crate::SessionView::new();
    view.apply_event(&event);
    assert_eq!(view.snapshot().transcript.items.len(), 1);
    let bcode_session_view_models::TranscriptViewItemKind::SystemMessage { message } =
        &view.snapshot().transcript.items[0].kind
    else {
        panic!("prepared state must remain visible");
    };
    assert_eq!(message.text, "Disabled is ready. Send a message to start.");
    assert!(view.snapshot().tools.is_empty());
    // The generic fallback keeps the exact unconfirmed-selection evidence.
    let fallback = canonical_external_text(&event).unwrap().unwrap();
    assert!(fallback.contains("requested-only (unconfirmed)"));
    assert!(fallback.contains("No native conversation identity confirmed"));
}

#[test]
fn canonical_external_events_enter_the_real_shared_transcript_once_without_plugin() {
    use bcode_session_view_models::{TextFormat, TranscriptViewItemKind};
    let (instance, binding) = source();
    let events = [
        event(
            &binding,
            1,
            ExternalBackendRecord::Binding {
                instance,
                binding: binding.clone(),
            },
        ),
        event(
            &binding,
            2,
            ExternalBackendRecord::Observation {
                observation: observation(
                    &binding,
                    HarnessObservationKind::Interaction {
                        interaction: HarnessInteraction {
                            input_preview: None,
                            tool_name: None,
                            request_id: "approval".into(),
                            session_id: binding.session_id.clone(),
                            generation: binding.generation,
                            kind: HarnessInteractionKind::Approval,
                            prompt: "Run native shell?".into(),
                            scope: "workspace only".into(),
                            choices: vec![],
                        },
                    },
                ),
            },
        ),
    ];
    let mut view = crate::SessionView::default();
    for event in &events {
        view.apply_event(event);
    }
    view.apply_event(&events[1]);
    assert_eq!(view.snapshot().transcript.items.len(), 2);
    let TranscriptViewItemKind::SystemMessage { message } =
        &view.snapshot().transcript.items[1].kind
    else {
        panic!("native interaction must be a compact transcript notice");
    };
    assert_eq!(message.format, TextFormat::PlainText);
    assert_eq!(
        message.text,
        "Disabled is asking for approval: Run native shell?"
    );
    // The dialog-facing projection carries the full request exactly once.
    let pending = &view.snapshot().pending_external_interactions;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].request_id, "approval");
    assert_eq!(pending[0].scope, "workspace only");
    assert!(view.snapshot().tools.is_empty());
    assert_eq!(view.snapshot().permissions, Vec::new());
    // The generic fallback still documents the manual response path for this exact request.
    let fallback = canonical_external_text(&events[1]).unwrap().unwrap();
    assert!(fallback.contains("workspace only"));
    assert!(fallback.contains(&format!(
        "/native respond {} approval <new-response-id> <JSON>",
        binding.generation
    )));
}

#[test]
fn bounded_external_event_reducer_matches_ordinary_and_rolls_back_on_budget_failure() {
    use bcode_session_view_models::retention::ObservationRetentionLimits;
    let (_, binding) = source();
    let event = event(
        &binding,
        1,
        ExternalBackendRecord::Observation {
            observation: observation(
                &binding,
                HarnessObservationKind::Tool {
                    input_json: None,
                    native_tool_id: "native-tool".into(),
                    name: "shell".into(),
                    status: "observed".into(),
                },
            ),
        },
    );
    let limits = ObservationRetentionLimits {
        scalar_capacity_bytes: 16 * 1024 * 1024,
        collection_entries: 1_000_000,
        value_nodes: 2_000_000,
        depth: 128,
        copies: 32,
    };
    let mut bounded = crate::SessionView::default();
    let mut ordinary = bounded.clone();
    let mut usage = bounded.observation_retention_usage(limits).unwrap();
    let before = bounded.snapshot().clone();
    let before_usage = usage;
    assert!(
        bounded
            .apply_event_bounded(
                &event,
                &mut usage,
                ObservationRetentionLimits {
                    scalar_capacity_bytes: 0,
                    ..limits
                }
            )
            .is_err()
    );
    assert_eq!(bounded.snapshot(), &before);
    assert_eq!(usage, before_usage);
    bounded
        .apply_event_bounded(&event, &mut usage, limits)
        .unwrap();
    ordinary.apply_event(&event);
    assert_eq!(bounded.snapshot(), ordinary.snapshot());
    assert_eq!(usage, bounded.observation_retention_usage(limits).unwrap());
}

#[test]
fn arbitrary_native_extension_detail_is_not_public_history() {
    let (instance, binding) = source();
    let (provenance, _) = external_metadata(&instance, &binding).unwrap();
    let item = external_transcript_item(
        &observation(
            &binding,
            HarnessObservationKind::Vendor {
                namespace: "extension".into(),
                detail: serde_json::json!({"unapproved": "private"}),
            },
        ),
        2,
        &provenance,
    )
    .unwrap();
    assert!(!item.text.contains("private"));
    assert!(item.text.contains("detail omitted"));
}
