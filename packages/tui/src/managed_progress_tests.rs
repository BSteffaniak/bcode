//! A managed native turn as the attached TUI shows it: the composer's pending row and the
//! foreground activity indicator.

use bcode_harness_models::{
    HARNESS_SCHEMA_VERSION, HarnessActivity, HarnessAttention, HarnessAuthBinding,
    HarnessAuthCustody, HarnessCapabilities, HarnessCapability, HarnessCompleteness,
    HarnessConnectivity, HarnessContinuation, HarnessExecutionStatus, HarnessInstance,
    HarnessObservation, HarnessObservationKind, HarnessObservationPage, HarnessPlacement,
    HarnessSessionBinding, HarnessSessionTarget, HarnessSubmission, HarnessSupportLevel,
};
use bcode_session_models::external_harness::{
    ExternalHarnessOperation, ExternalHarnessOperationRecord, ExternalHarnessOwner,
    ExternalHarnessRequest, ExternalHarnessRequestKind,
};
use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, SessionEvent, SessionEventKind, SessionId,
};
use bcode_session_view_models::TranscriptViewItemKind;

use crate::activity::ActivityState;
use crate::app::BmuxApp;

const GENERATION: u64 = 3;

fn instance() -> HarnessInstance {
    HarnessInstance {
        instance_id: "work".into(),
        plugin_id: "bcode.claude-code".into(),
        executable: "/native/agent".into(),
        config_namespace: "/config/isolated".into(),
        auth: HarnessAuthBinding {
            provider_id: "native-auth".into(),
            profile: "work".into(),
            credential_namespace: "isolated".into(),
            custody: HarnessAuthCustody::NativeManaged,
        },
    }
}

fn capabilities() -> HarnessCapabilities {
    let supported = HarnessCapability {
        support: HarnessSupportLevel::Supported,
        constraints: vec![],
    };
    HarnessCapabilities {
        resume: supported.clone(),
        steering: supported.clone(),
        approval: supported.clone(),
        questions: supported.clone(),
        assistance: supported.clone(),
        interrupt: supported.clone(),
        authoritative_settlement: supported.clone(),
        background_continuation: supported.clone(),
        authentication: supported,
        model_switch: HarnessCapability::unsupported(),
        permission_mode: HarnessCapability::unsupported(),
    }
}

struct Session {
    id: SessionId,
    sequence: u64,
}

impl Session {
    fn event(&mut self, record: ExternalHarnessOperationRecord) -> SessionEvent {
        self.sequence += 1;
        SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            session_id: self.id,
            sequence: self.sequence,
            timestamp_ms: 1_000 * self.sequence,
            provenance: None,
            kind: SessionEventKind::ExternalHarnessOperation {
                operation: Box::new(ExternalHarnessOperation {
                    inventory_commitments: Vec::new(),
                    version: 1,
                    owner: ExternalHarnessOwner {
                        session_id: self.id,
                        generation: GENERATION,
                        artifact_id: "artifact".into(),
                        daemon_id: "daemon".into(),
                    },
                    record,
                }),
            },
        }
    }

    fn claimed(&mut self, submission_id: &str, text: &str) -> SessionEvent {
        let binding = HarnessSessionBinding {
            schema_version: HARNESS_SCHEMA_VERSION,
            session_id: self.id.to_string(),
            instance_id: "work".into(),
            native_session_id: "native".into(),
            native_version: "1.0".into(),
            generation: GENERATION,
        };
        let submission = HarnessSubmission {
            submission_id: submission_id.into(),
            attempt_id: "attempt-1".into(),
            session_id: self.id.to_string(),
            generation: GENERATION,
            text: text.into(),
            placement: HarnessPlacement::NextTurn,
        };
        self.event(ExternalHarnessOperationRecord::DeliveryClaimed {
            request: ExternalHarnessRequest {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: submission_id.into(),
                instance: instance(),
                kind: ExternalHarnessRequestKind::Submit {
                    target: HarnessSessionTarget::Confirmed(binding),
                    submission,
                },
            },
        })
    }

    fn page(&mut self, kinds: Vec<HarnessObservationKind>) -> SessionEvent {
        let observations = kinds
            .into_iter()
            .map(|kind| {
                self.sequence += 1;
                HarnessObservation {
                    observation_id: format!("observation-{}", self.sequence),
                    session_id: self.id.to_string(),
                    generation: GENERATION,
                    native_correlations: vec![],
                    completeness: HarnessCompleteness::Complete,
                    kind,
                }
            })
            .collect();
        self.event(ExternalHarnessOperationRecord::ObservationPage {
            instance: instance(),
            after: None,
            page: HarnessObservationPage {
                retention: None,
                hydration_requests: Vec::new(),
                inventory_page: None,
                observations,
                next_after: None,
                completeness: HarnessCompleteness::Complete,
                status: HarnessExecutionStatus {
                    connectivity: HarnessConnectivity::Connected,
                    activity: HarnessActivity::Running,
                    attention: HarnessAttention::None,
                    continuation: HarnessContinuation::Available,
                    completeness: HarnessCompleteness::Complete,
                },
                live: Vec::new(),
            },
        })
    }
}

#[test]
fn a_managed_turn_replaces_its_sending_row_and_shows_native_progress() {
    let mut session = Session {
        id: SessionId::new(),
        sequence: 0,
    };
    let mut app = BmuxApp::new_with_history(Some(session.id), &[], &[]);
    app.set_managed_execution(session.id);
    app.set_managed_label("Claude Code".into());
    let configured = session.event(ExternalHarnessOperationRecord::Configured {
        instance: instance(),
        capabilities: Box::new(capabilities()),
        open_request: None,
    });
    app.absorb_session_event(&configured);
    assert_eq!(app.activity(), &ActivityState::Idle);

    app.replace_composer_with("run the tests");
    app.stage_submission();
    let staged = app.pending_submissions()[0].id().0.clone();

    // The canonical user message arrives before the submit call returns its receipt: the
    // pending row leaves in the same update, so the two are never shown together.
    let claimed = session.claimed(&staged, "run the tests");
    app.absorb_session_event(&claimed);
    assert_eq!(
        app.pending_submissions().len(),
        0,
        "the pending row left with the canonical message"
    );
    let users = app
        .session_view_snapshot()
        .transcript
        .items
        .iter()
        .filter(|item| matches!(item.kind, TranscriptViewItemKind::UserMessage { .. }))
        .count();
    assert_eq!(users, 1);
    assert_eq!(
        app.activity(),
        &ActivityState::WaitingForProvider {
            provider: "Claude Code".into(),
            round: None,
        }
    );

    let tool = session.page(vec![HarnessObservationKind::Tool {
        native_tool_id: "tool-1".into(),
        name: "Bash".into(),
        status: "observed".into(),
        input_json: None,
    }]);
    app.absorb_session_event(&tool);
    assert_eq!(
        app.activity(),
        &ActivityState::RunningTool {
            name: "Bash".into()
        }
    );

    let reply = session.page(vec![HarnessObservationKind::Text {
        text: "All green.".into(),
        segment_id: None,
    }]);
    app.absorb_session_event(&reply);
    assert!(matches!(
        app.activity(),
        ActivityState::ProviderStream { .. }
    ));

    let result = session.page(vec![HarnessObservationKind::Result {
        text: "All green.".into(),
    }]);
    app.absorb_session_event(&result);
    assert_eq!(app.activity(), &ActivityState::Idle);
}

fn header_row(app: &mut BmuxApp) -> String {
    let mut buffer = bmux_tui::buffer::Buffer::empty(bmux_tui::geometry::Rect::new(0, 0, 120, 30));
    let mut frame = bmux_tui::frame::Frame::new(&mut buffer);
    crate::render::render(app, &mut bmux_tui::paint::PaintCx::new(&mut frame));
    (0..buffer.area().height)
        .filter_map(|row| buffer.row_symbols(row))
        .find(|row| row.contains("bcode"))
        .expect("header row")
}

impl Session {
    fn selected_open(&mut self, effective_model_id: &str) -> [SessionEvent; 2] {
        let request = ExternalHarnessRequest::from_models_request(
            &bcode_harness_models::HarnessModelsRequest {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: "open-1".into(),
                instance: instance(),
                kind: bcode_harness_models::HarnessModelsRequestKind::OpenSelected {
                    open: Box::new(bcode_harness_models::HarnessOpenRequest {
                        open_id: "open-1".into(),
                        session_id: self.id.to_string(),
                        generation: GENERATION,
                        working_directory: "/work".into(),
                        resume: None,
                        parent_assistance: None,
                    }),
                    selection: bcode_harness_models::HarnessModelSelection {
                        model_id: "opus".into(),
                    },
                },
            },
        )
        .unwrap();
        let claimed = self.event(ExternalHarnessOperationRecord::DeliveryClaimed {
            request: request.clone(),
        });
        let completed = self.event(ExternalHarnessOperationRecord::DeliveryCompleted {
            request,
            response: bcode_session_models::external_harness::ExternalHarnessResponse {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: "open-1".into(),
                operation: bcode_harness_models::HarnessOperation::Open,
                kind:
                    bcode_session_models::external_harness::ExternalHarnessResponseKind::Selected {
                        target: HarnessSessionTarget::Prepared(
                            bcode_harness_models::HarnessPreparedSession {
                                open_id: "open-1".into(),
                                session_id: self.id.to_string(),
                                instance_id: "work".into(),
                                generation: GENERATION,
                                native_version: "1.0".into(),
                                requested_native_session_id: None,
                            },
                        ),
                        evidence: bcode_harness_models::HarnessModelEvidence {
                            requested_model_id: "opus".into(),
                            effective_model_id: Some(effective_model_id.into()),
                        },
                    },
            },
        });
        [claimed, completed]
    }
}

#[test]
fn managed_header_names_the_native_model_and_never_the_bcode_selection() {
    let mut session = Session {
        id: SessionId::new(),
        sequence: 0,
    };
    let mut app = BmuxApp::new_with_history(Some(session.id), &[], &[]);
    app.set_managed_execution(session.id);
    app.set_managed_label("Claude Code".into());
    // The Bcode selection a client may still hold (carried over from a previous session, or
    // reported by an older daemon) is not what a managed conversation runs on.
    app.apply_runtime_selection(
        bcode_ipc::SessionRuntimeSelection {
            provider_plugin_id: Some("bcode.openai-compatible".into()),
            requested_model_id: Some("gpt-6.1-sol".into()),
            effective_model_id: Some("gpt-6.1-sol".into()),
            ..bcode_ipc::SessionRuntimeSelection::default()
        },
        None,
    );
    let configured = session.event(ExternalHarnessOperationRecord::Configured {
        instance: instance(),
        capabilities: Box::new(capabilities()),
        open_request: None,
    });
    app.absorb_session_event(&configured);
    let header = header_row(&mut app);
    assert!(header.contains("Claude Code"), "{header}");
    assert!(!header.contains("gpt-6.1-sol"), "{header}");

    for event in session.selected_open("claude-opus-5-5") {
        app.absorb_session_event(&event);
    }
    let header = header_row(&mut app);
    assert!(header.contains("Claude Code · claude-opus-5-5"), "{header}");
    assert!(!header.contains("gpt-6.1-sol"), "{header}");

    let usage = session.page(vec![HarnessObservationKind::Usage {
        input_tokens: 12,
        output_tokens: 1776,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
        total_cost_micros: None,
        duration_ms: None,
        model: Some("claude-sonnet-5-5".into()),
    }]);
    app.absorb_session_event(&usage);
    let header = header_row(&mut app);
    drop(app);
    assert!(
        header.contains("Claude Code · claude-sonnet-5-5"),
        "{header}"
    );
}
