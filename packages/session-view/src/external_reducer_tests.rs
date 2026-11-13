//! Behavioral tests for native-like managed external presentation in both reducer paths.

use bcode_harness_models::{
    HARNESS_SCHEMA_VERSION, HarnessActivity, HarnessAttention, HarnessAuthBinding,
    HarnessAuthCustody, HarnessCapabilities, HarnessCapability, HarnessChoice, HarnessCompleteness,
    HarnessConnectivity, HarnessContinuation, HarnessDeliveryState, HarnessExecutionStatus,
    HarnessFailure, HarnessInstance, HarnessInteraction, HarnessInteractionKind,
    HarnessObservation, HarnessObservationKind, HarnessObservationPage, HarnessOperation,
    HarnessPlacement, HarnessReceipt, HarnessResponse, HarnessResponseValue, HarnessSessionBinding,
    HarnessSessionTarget, HarnessSubmission, HarnessSupportLevel, harness_stream_identity,
};
use bcode_session_models::external_backend::{ExternalBackendEnvelope, ExternalBackendRecord};
use bcode_session_models::external_harness::{
    ExternalHarnessOperation, ExternalHarnessOperationRecord, ExternalHarnessOwner,
    ExternalHarnessRequest, ExternalHarnessRequestKind, ExternalHarnessResponse,
    ExternalHarnessResponseKind,
};
use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, SessionEvent, SessionEventKind, SessionId,
    SessionLiveEvent, SessionLiveEventKind, TextStreamOperation, TextStreamUpdate,
};
use bcode_session_view_models::external::ExternalInteractionKind;
use bcode_session_view_models::retention::ObservationRetentionLimits;
use bcode_session_view_models::{
    ReasoningContentAvailability, ToolInvocationViewStatus, TranscriptViewItem,
    TranscriptViewItemKind,
};

use crate::SessionView;
use crate::external_projection::{EXTERNAL_HARNESS_FALLBACK_LABEL, harness_display_label};

const GENERATION: u64 = 7;

struct Fixture {
    session_id: SessionId,
    sequence: u64,
}

fn limits() -> ObservationRetentionLimits {
    ObservationRetentionLimits {
        scalar_capacity_bytes: 16 * 1024 * 1024,
        collection_entries: 1_000_000,
        value_nodes: 2_000_000,
        depth: 128,
        copies: 32,
    }
}

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

fn idle() -> HarnessExecutionStatus {
    HarnessExecutionStatus {
        connectivity: HarnessConnectivity::Connected,
        activity: HarnessActivity::Idle,
        attention: HarnessAttention::None,
        continuation: HarnessContinuation::Available,
        completeness: HarnessCompleteness::Complete,
    }
}

impl Fixture {
    fn new() -> Self {
        Self {
            session_id: SessionId::new(),
            sequence: 0,
        }
    }

    fn binding(&self, generation: u64) -> HarnessSessionBinding {
        HarnessSessionBinding {
            schema_version: HARNESS_SCHEMA_VERSION,
            session_id: self.session_id.to_string(),
            instance_id: "work".into(),
            native_session_id: "native".into(),
            native_version: "1.0".into(),
            generation,
        }
    }

    fn operation(
        &mut self,
        generation: u64,
        record: ExternalHarnessOperationRecord,
    ) -> SessionEvent {
        self.sequence += 1;
        SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            session_id: self.session_id,
            sequence: self.sequence,
            timestamp_ms: 1_000 * self.sequence,
            provenance: None,
            kind: SessionEventKind::ExternalHarnessOperation {
                operation: Box::new(ExternalHarnessOperation {
                    inventory_commitments: Vec::new(),
                    version: 1,
                    owner: ExternalHarnessOwner {
                        session_id: self.session_id,
                        generation,
                        artifact_id: "artifact".into(),
                        daemon_id: "daemon".into(),
                    },
                    record,
                }),
            },
        }
    }

    fn configured(&mut self) -> SessionEvent {
        self.operation(
            GENERATION,
            ExternalHarnessOperationRecord::Configured {
                instance: instance(),
                capabilities: Box::new(capabilities()),
                open_request: None,
            },
        )
    }

    fn observation(
        &self,
        generation: u64,
        id: &str,
        kind: HarnessObservationKind,
    ) -> HarnessObservation {
        HarnessObservation {
            observation_id: id.into(),
            session_id: self.session_id.to_string(),
            generation,
            native_correlations: vec![],
            completeness: HarnessCompleteness::Complete,
            kind,
        }
    }

    fn page_at(
        &mut self,
        generation: u64,
        kinds: Vec<(&str, HarnessObservationKind)>,
    ) -> SessionEvent {
        let observations = kinds
            .into_iter()
            .map(|(id, kind)| self.observation(generation, id, kind))
            .collect();
        self.operation(
            generation,
            ExternalHarnessOperationRecord::ObservationPage {
                instance: instance(),
                after: None,
                page: HarnessObservationPage {
                    retention: None,
                    hydration_requests: Vec::new(),
                    inventory_page: None,
                    observations,
                    next_after: None,
                    completeness: HarnessCompleteness::Complete,
                    status: idle(),
                    live: Vec::new(),
                },
            },
        )
    }

    fn page(&mut self, kinds: Vec<(&str, HarnessObservationKind)>) -> SessionEvent {
        self.page_at(GENERATION, kinds)
    }

    fn request(kind: ExternalHarnessRequestKind, request_id: &str) -> ExternalHarnessRequest {
        ExternalHarnessRequest {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: request_id.into(),
            instance: instance(),
            kind,
        }
    }

    fn submit_request(&self, text: &str) -> ExternalHarnessRequest {
        Self::request(
            ExternalHarnessRequestKind::Submit {
                target: HarnessSessionTarget::Confirmed(self.binding(GENERATION)),
                submission: HarnessSubmission {
                    submission_id: "submission-1".into(),
                    attempt_id: "attempt-1".into(),
                    session_id: self.session_id.to_string(),
                    generation: GENERATION,
                    text: text.into(),
                    placement: HarnessPlacement::NextTurn,
                },
            },
            "submission-1",
        )
    }

    fn respond_request(&self, request_id: &str) -> ExternalHarnessRequest {
        Self::request(
            ExternalHarnessRequestKind::Respond {
                target: HarnessSessionTarget::Confirmed(self.binding(GENERATION)),
                response: HarnessResponse {
                    response_id: "response-1".into(),
                    request_id: request_id.into(),
                    session_id: self.session_id.to_string(),
                    generation: GENERATION,
                    value: HarnessResponseValue::Approval { approved: true },
                },
            },
            "response-1",
        )
    }

    fn completed(
        &mut self,
        request: ExternalHarnessRequest,
        operation: HarnessOperation,
        kind: ExternalHarnessResponseKind,
    ) -> SessionEvent {
        let response = ExternalHarnessResponse {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: request.request_id.clone(),
            operation,
            kind,
        };
        self.operation(
            GENERATION,
            ExternalHarnessOperationRecord::DeliveryCompleted { request, response },
        )
    }

    fn interaction(&self, generation: u64, request_id: &str) -> HarnessInteraction {
        HarnessInteraction {
            request_id: request_id.into(),
            session_id: self.session_id.to_string(),
            generation,
            kind: HarnessInteractionKind::Approval,
            prompt: "Run the test suite?".into(),
            scope: "workspace".into(),
            choices: vec![HarnessChoice {
                choice_id: "allow".into(),
                label: "Allow".into(),
            }],
            tool_name: Some("Bash".into()),
            input_preview: Some("{\"command\":\"cargo test\"}".into()),
        }
    }

    fn envelope(&mut self, record: ExternalBackendRecord) -> SessionEvent {
        self.sequence += 1;
        SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            session_id: self.session_id,
            sequence: self.sequence,
            timestamp_ms: 1_000 * self.sequence,
            provenance: None,
            kind: ExternalBackendEnvelope { version: 1, record }
                .into_event_kind()
                .unwrap(),
        }
    }

    fn live_text(
        &self,
        segment: &str,
        revision: u64,
        offset: usize,
        text: &str,
    ) -> SessionLiveEvent {
        let identity = harness_stream_identity(GENERATION, segment);
        SessionLiveEvent {
            session_id: self.session_id,
            kind: SessionLiveEventKind::AssistantTextStreamUpdated {
                output_position: None,
                turn_id: identity.turn_id,
                segment_id: identity.segment_id,
                segment_order: identity.segment_order,
                update: TextStreamUpdate {
                    generation: 0,
                    first_revision: revision,
                    revision,
                    operation: TextStreamOperation::Append {
                        expected_offset: offset,
                        text: text.into(),
                    },
                },
            },
        }
    }

    fn live_reasoning(&self, segment: &str, text: &str) -> SessionLiveEvent {
        let identity = harness_stream_identity(GENERATION, segment);
        SessionLiveEvent {
            session_id: self.session_id,
            kind: SessionLiveEventKind::AssistantReasoningTextStreamUpdated {
                output_position: None,
                activity_id: identity.reasoning_activity_id().to_owned(),
                activity_order: 0,
                part_id: identity.reasoning_part_id().to_owned(),
                part_order: 0,
                kind: bcode_session_models::ReasoningContentKind::Legacy,
                role: bcode_session_models::ReasoningContentRole::Unknown,
                turn_id: identity.turn_id,
                update: TextStreamUpdate {
                    generation: 0,
                    first_revision: 1,
                    revision: 1,
                    operation: TextStreamOperation::Append {
                        expected_offset: 0,
                        text: text.into(),
                    },
                },
            },
        }
    }
}

/// Ordinary and bounded reducers kept in lockstep with an exact bounded ledger.
struct Pair {
    ordinary: SessionView,
    bounded: SessionView,
    ledger: bcode_session_view_models::retention::ObservationRetentionUsage,
}

impl Pair {
    fn new() -> Self {
        let bounded = SessionView::new();
        let ledger = bounded.observation_retention_usage(limits()).unwrap();
        Self {
            ordinary: bounded.clone(),
            bounded,
            ledger,
        }
    }

    fn durable(&mut self, event: &SessionEvent) {
        self.ordinary.apply_event(event);
        self.bounded
            .apply_event_bounded(event, &mut self.ledger, limits())
            .unwrap();
        self.check();
    }

    fn live(&mut self, event: &SessionLiveEvent) {
        self.ordinary.apply_live_event(event);
        self.bounded
            .apply_live_event_bounded(event, &mut self.ledger, limits())
            .unwrap();
        self.check();
    }

    fn check(&self) {
        assert_eq!(self.bounded.snapshot(), self.ordinary.snapshot());
        assert_eq!(
            self.ledger,
            self.bounded.observation_retention_usage(limits()).unwrap()
        );
    }

    fn pending_count(&self) -> usize {
        self.ordinary.snapshot().pending_external_interactions.len()
    }

    fn items(&self) -> &[TranscriptViewItem] {
        &self.ordinary.snapshot().transcript.items
    }
}

fn assistant_text(item: &TranscriptViewItem) -> Option<(&str, Option<&str>)> {
    match &item.kind {
        TranscriptViewItemKind::AssistantMessage { message } => {
            Some((message.text.as_str(), message.display_label.as_deref()))
        }
        _ => None,
    }
}

fn system_text(item: &TranscriptViewItem) -> Option<&str> {
    match &item.kind {
        TranscriptViewItemKind::SystemMessage { message } => Some(message.text.as_str()),
        _ => None,
    }
}

#[test]
fn display_label_is_derived_generically_from_the_plugin_identity() {
    assert_eq!(harness_display_label("bcode.claude-code"), "Claude Code");
    assert_eq!(harness_display_label("codex-harness-plugin"), "Codex");
    assert_eq!(harness_display_label("vendor/agent_cli"), "Agent Cli");
    assert_eq!(harness_display_label(""), EXTERNAL_HARNESS_FALLBACK_LABEL);
    assert_eq!(harness_display_label("bcode."), "Bcode");
}

fn full_turn_page(fixture: &mut Fixture) -> SessionEvent {
    fixture.page(vec![
        (
            "accepted",
            HarnessObservationKind::SubmissionAccepted {
                submission_id: "submission-1".into(),
                attempt_id: "attempt-1".into(),
                native_turn_id: "turn".into(),
            },
        ),
        (
            "thinking",
            HarnessObservationKind::Reasoning {
                segment_id: Some("msg-1:0".into()),
                text: "I should run cargo test.".into(),
                opaque: false,
            },
        ),
        (
            "text",
            HarnessObservationKind::Text {
                text: "Running the **tests** now.".into(),
                segment_id: Some("msg-1:1".into()),
            },
        ),
        (
            "tool",
            HarnessObservationKind::Tool {
                native_tool_id: "toolu_1".into(),
                name: "Bash".into(),
                status: "running".into(),
                input_json: Some("{\"command\":\"cargo test\"}".into()),
            },
        ),
        (
            "tool-result",
            HarnessObservationKind::ToolResult {
                native_tool_id: "toolu_1".into(),
                output: "test result: ok".into(),
                is_error: false,
                truncated: true,
            },
        ),
        (
            "final",
            HarnessObservationKind::Text {
                text: "All tests pass.".into(),
                segment_id: Some("msg-2:0".into()),
            },
        ),
        (
            "result",
            HarnessObservationKind::Result {
                text: "All tests pass.".into(),
            },
        ),
        (
            "usage",
            HarnessObservationKind::Usage {
                input_tokens: 100,
                output_tokens: 20,
                cache_read_input_tokens: 1_000,
                cache_creation_input_tokens: 50,
                total_cost_micros: Some(12_345),
                duration_ms: Some(900),
                model: Some("native-model".into()),
            },
        ),
        ("status", HarnessObservationKind::Status { status: idle() }),
    ])
}

#[test]
fn a_full_turn_projects_native_items_in_both_reducers() {
    let mut fixture = Fixture::new();
    let mut pair = Pair::new();
    pair.durable(&fixture.configured());
    let claimed = fixture.operation(
        GENERATION,
        ExternalHarnessOperationRecord::DeliveryClaimed {
            request: fixture.submit_request("Please run the tests"),
        },
    );
    pair.durable(&claimed);
    let page = full_turn_page(&mut fixture);
    pair.durable(&page);

    let items = pair.items();
    assert_eq!(items.len(), 5, "{items:#?}");
    let TranscriptViewItemKind::UserMessage { message } = &items[0].kind else {
        panic!("submitted input must show as the user's message");
    };
    assert_eq!(message.text, "Please run the tests");

    let TranscriptViewItemKind::ReasoningActivity { activity } = &items[1].kind else {
        panic!("native reasoning must be a reasoning activity");
    };
    assert_eq!(activity.text(), "I should run cargo test.");
    assert!(activity.finished);
    let reasoning_identity = harness_stream_identity(GENERATION, "msg-1:0");
    assert_eq!(items[1].id.get(), reasoning_identity.reasoning_item_id());

    let identity = harness_stream_identity(GENERATION, "msg-1:1");
    assert_eq!(items[2].id.get(), identity.assistant_item_id());
    assert_eq!(
        assistant_text(&items[2]),
        Some(("Running the **tests** now.", Some("Claude Code")))
    );

    let TranscriptViewItemKind::ToolInvocation { tool } = &items[3].kind else {
        panic!("observed native tool must be a tool card");
    };
    assert_eq!(tool.tool_call_id, "external:7:toolu_1");
    assert_eq!(
        tool.producer_plugin_id.as_deref(),
        Some("bcode.claude-code")
    );
    assert_eq!(tool.tool_name.as_deref(), Some("Bash"));
    assert_eq!(
        tool.arguments_json.as_deref(),
        Some("{\"command\":\"cargo test\"}")
    );
    assert_eq!(tool.status, ToolInvocationViewStatus::Finished);
    assert_eq!(tool.is_error, Some(false));
    let result = tool.result_text.as_deref().unwrap();
    assert!(result.starts_with("test result: ok"));
    assert!(result.contains("truncated"));
    assert!(tool.timing.started_at_ms.is_some() && tool.timing.finished_at_ms.is_some());

    // The result duplicates the final assistant text, so it adds nothing.
    assert_eq!(
        assistant_text(&items[4]),
        Some(("All tests pass.", Some("Claude Code")))
    );

    assert_native_usage_and_no_bcode_tool_state(pair.ordinary.snapshot());
}

fn assert_native_usage_and_no_bcode_tool_state(
    snapshot: &bcode_session_view_models::SessionViewSnapshot,
) {
    let usage = snapshot.runtime.latest_usage.as_ref().unwrap();
    assert_eq!(usage.input_tokens, Some(1_150));
    assert_eq!(usage.output_tokens, Some(20));
    assert_eq!(usage.cached_input_tokens, Some(1_000));
    assert_eq!(usage.cache_write_input_tokens, Some(50));
    assert_eq!(
        snapshot.runtime.cost.totals_micros.get("USD"),
        Some(&12_345)
    );
    assert_eq!(snapshot.runtime.cumulative_metered_tokens, 1_170);

    // Observed native tools never become Bcode tool, permission, or dispatch state.
    assert!(snapshot.tools.is_empty());
    assert!(snapshot.active_invocations.is_empty());
    assert_eq!(snapshot.permissions.len(), 0);
    assert_eq!(snapshot.interactions.len(), 0);
    assert!(snapshot.execution_lifecycle.session_id.is_some());
}

#[test]
fn distinct_result_text_and_unsegmented_text_are_assistant_messages() {
    let mut fixture = Fixture::new();
    let mut pair = Pair::new();
    let page = fixture.page(vec![
        (
            "text",
            HarnessObservationKind::Text {
                text: "Working on it.".into(),
                segment_id: None,
            },
        ),
        (
            "blank",
            HarnessObservationKind::Text {
                text: "  ".into(),
                segment_id: None,
            },
        ),
        (
            "result",
            HarnessObservationKind::Result {
                text: "Done: 3 files changed.".into(),
            },
        ),
    ]);
    pair.durable(&page);
    let texts = pair
        .items()
        .iter()
        .filter_map(assistant_text)
        .collect::<Vec<_>>();
    assert_eq!(
        texts,
        vec![
            ("Working on it.", Some("Claude Code")),
            ("Done: 3 files changed.", Some("Claude Code"))
        ]
    );
}

#[test]
fn lifecycle_evidence_adds_no_transcript_items() {
    let mut fixture = Fixture::new();
    let mut pair = Pair::new();
    pair.durable(&fixture.configured());
    let page = fixture.page(vec![
        (
            "accepted",
            HarnessObservationKind::SubmissionAccepted {
                submission_id: "s".into(),
                attempt_id: "a".into(),
                native_turn_id: "t".into(),
            },
        ),
        (
            "consumed",
            HarnessObservationKind::SubmissionConsumed {
                submission_id: "s".into(),
                attempt_id: "a".into(),
                native_frame_id: "f".into(),
            },
        ),
        (
            "attributed",
            HarnessObservationKind::SubmissionResultObserved {
                submission_id: "s".into(),
                attempt_id: "a".into(),
                native_frame_id: "f".into(),
                is_error: false,
                queued_user_sends: Some(0),
            },
        ),
        (
            "bound",
            HarnessObservationKind::BindingConfirmed {
                open_id: "open".into(),
                binding: fixture.binding(GENERATION),
            },
        ),
        ("status", HarnessObservationKind::Status { status: idle() }),
        (
            "terminal",
            HarnessObservationKind::NativeTurnTerminal {
                native_turn_id: "t".into(),
                disposition: bcode_harness_models::HarnessTerminalDisposition::Completed,
            },
        ),
        (
            "settled",
            HarnessObservationKind::Settlement {
                native_turn_id: "t".into(),
            },
        ),
        (
            "vendor",
            HarnessObservationKind::Vendor {
                namespace: "extension".into(),
                detail: serde_json::json!({"private": true}),
            },
        ),
    ]);
    pair.durable(&page);
    let receipt = fixture.completed(
        fixture.submit_request("hello"),
        HarnessOperation::Submit,
        ExternalHarnessResponseKind::Receipt(HarnessReceipt {
            submission_id: "submission-1".into(),
            attempt_id: "attempt-1".into(),
            session_id: fixture.session_id.to_string(),
            generation: GENERATION,
            state: HarnessDeliveryState::Accepted,
            native_request_id: None,
        }),
    );
    pair.durable(&receipt);
    assert!(pair.items().is_empty(), "{:#?}", pair.items());
    assert_eq!(
        pair.ordinary.snapshot().latest_sequence,
        Some(fixture.sequence)
    );
}

#[test]
fn diagnostics_failures_and_preparation_are_compact_friendly_notices() {
    let mut fixture = Fixture::new();
    let mut pair = Pair::new();
    pair.durable(&fixture.configured());
    let page = fixture.page(vec![(
        "diagnostic",
        HarnessObservationKind::Diagnostic {
            code: "rate_limited".into(),
            message: "Usage limit reached".into(),
        },
    )]);
    pair.durable(&page);
    let failed = fixture.completed(
        fixture.submit_request("hello"),
        HarnessOperation::Submit,
        ExternalHarnessResponseKind::Failure(HarnessFailure::Unavailable),
    );
    pair.durable(&failed);
    let texts = pair
        .items()
        .iter()
        .filter_map(system_text)
        .collect::<Vec<_>>();
    assert_eq!(
        texts,
        vec![
            "Claude Code: Usage limit reached (rate_limited)",
            "Claude Code could not deliver your message: it is unavailable right now."
        ]
    );
    for text in texts {
        assert!(!text.contains("not settlement"));
        assert!(!text.contains("assignment"));
    }
}

#[test]
fn durable_text_supersedes_the_live_stream_with_the_shared_identity() {
    let mut fixture = Fixture::new();
    let mut pair = Pair::new();
    pair.durable(&fixture.configured());
    pair.live(&fixture.live_text("msg-1:0", 1, 0, "Hel"));
    pair.live(&fixture.live_text("msg-1:0", 2, 3, "lo"));
    let identity = harness_stream_identity(GENERATION, "msg-1:0");
    {
        let items = pair.items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id.get(), identity.assistant_item_id());
        assert!(items[0].streaming);
        assert_eq!(
            assistant_text(&items[0]),
            Some(("Hello", Some("Claude Code")))
        );
    }
    let page = fixture.page(vec![(
        "text",
        HarnessObservationKind::Text {
            text: "Hello, world.".into(),
            segment_id: Some("msg-1:0".into()),
        },
    )]);
    pair.durable(&page);
    pair.live(&fixture.live_text("msg-1:0", 3, 5, " late"));
    let items = pair.items();
    assert_eq!(
        items.len(),
        1,
        "durable text replaces the live item in place"
    );
    assert!(!items[0].streaming);
    assert_eq!(
        assistant_text(&items[0]),
        Some(("Hello, world.", Some("Claude Code")))
    );
}

#[test]
fn durable_reasoning_supersedes_live_reasoning_and_respects_visibility() {
    let mut fixture = Fixture::new();
    let mut pair = Pair::new();
    pair.live(&fixture.live_reasoning("msg-1:0", "Consider"));
    assert_eq!(pair.items().len(), 1);
    let page = fixture.page(vec![
        (
            "thinking",
            HarnessObservationKind::Reasoning {
                segment_id: Some("msg-1:0".into()),
                text: "Consider the failing test first.".into(),
                opaque: false,
            },
        ),
        (
            "redacted",
            HarnessObservationKind::Reasoning {
                segment_id: Some("msg-1:1".into()),
                text: "[redacted]".into(),
                opaque: true,
            },
        ),
    ]);
    pair.durable(&page);
    let items = pair.items();
    assert_eq!(items.len(), 2);
    let TranscriptViewItemKind::ReasoningActivity { activity } = &items[0].kind else {
        panic!("reasoning activity expected");
    };
    assert_eq!(activity.text(), "Consider the failing test first.");
    let TranscriptViewItemKind::ReasoningActivity { activity } = &items[1].kind else {
        panic!("reasoning activity expected");
    };
    assert_eq!(
        activity.content_availability(),
        ReasoningContentAvailability::Withheld,
        "opaque placeholder text is never shown"
    );

    let mut hidden = SessionView::new();
    hidden.set_reasoning_visible(false);
    hidden.apply_event(&page);
    let TranscriptViewItemKind::ReasoningActivity { activity } =
        &hidden.snapshot().transcript.items[0].kind
    else {
        panic!("reasoning activity expected");
    };
    assert_eq!(
        activity.content_availability(),
        ReasoningContentAvailability::Filtered
    );
}

#[test]
fn agent_announcements_are_labelled_not_shown_and_compaction_is_counted() {
    use crate::external_projection::{split_agent_block, with_agent_block};
    let announced = with_agent_block("Plan", Some("Plan before changing files."), "what next?");
    assert_eq!(split_agent_block(&announced), (Some("Plan"), "what next?"));
    assert_eq!(split_agent_block("plain"), (None, "plain"));
    let mut fixture = Fixture::new();
    let mut pair = Pair::new();
    pair.durable(&fixture.configured());
    let claimed = fixture.operation(
        GENERATION,
        ExternalHarnessOperationRecord::DeliveryClaimed {
            request: fixture.submit_request(&announced),
        },
    );
    pair.durable(&claimed);
    {
        let snapshot = pair.ordinary.snapshot();
        let TranscriptViewItemKind::UserMessage { message } = &snapshot.transcript.items[0].kind
        else {
            panic!("user message");
        };
        assert_eq!(message.text, "what next?");
        assert_eq!(message.display_label.as_deref(), Some("Agent: Plan"));
    }
    let page = fixture.page(vec![(
        "compacted",
        HarnessObservationKind::Vendor {
            namespace: bcode_harness_models::NATIVE_CONTEXT_NAMESPACE.into(),
            detail: serde_json::json!({"code": "compacted"}),
        },
    )]);
    pair.durable(&page);
    assert_eq!(pair.ordinary.snapshot().native_context_compactions, 1);
}

#[test]
fn a_question_is_one_card_that_shows_the_answer() {
    let mut fixture = Fixture::new();
    let mut pair = Pair::new();
    pair.durable(&fixture.configured());
    let mut question = fixture.interaction(GENERATION, "question-1");
    question.kind = HarnessInteractionKind::Question;
    question.prompt = "What should I plan?".into();
    question.tool_name = None;
    question.choices = vec![
        HarnessChoice {
            choice_id: "0".into(),
            label: "Just testing".into(),
        },
        HarnessChoice {
            choice_id: "1".into(),
            label: "Review work".into(),
        },
    ];
    let page = fixture.page(vec![(
        "question",
        HarnessObservationKind::Interaction {
            interaction: question,
        },
    )]);
    pair.durable(&page);
    let card = |pair: &Pair| {
        pair.ordinary
            .snapshot()
            .transcript
            .items
            .iter()
            .filter_map(system_text)
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        card(&pair),
        [
            "Claude Code asks: What should I plan?\n  1. Just testing\n  2. Review work\nPick an option, or type your own answer and send it."
        ]
    );
    let mut answer = fixture.respond_request("question-1");
    if let ExternalHarnessRequestKind::Respond { response, .. } = &mut answer.kind {
        response.value = HarnessResponseValue::Answer {
            choice_id: None,
            text: "what model are you?".into(),
        };
    }
    let claimed = fixture.operation(
        GENERATION,
        ExternalHarnessOperationRecord::DeliveryClaimed { request: answer },
    );
    pair.durable(&claimed);
    assert_eq!(
        card(&pair),
        [
            "Claude Code asks: What should I plan?\n  1. Just testing\n  2. Review work\nYou answered: what model are you?"
        ]
    );
}

#[test]
fn pending_interactions_follow_requests_responses_and_generations() {
    let mut fixture = Fixture::new();
    let mut pair = Pair::new();
    pair.durable(&fixture.configured());
    let interaction = fixture.interaction(GENERATION, "approval-1");
    let page = fixture.page(vec![(
        "interaction",
        HarnessObservationKind::Interaction { interaction },
    )]);
    pair.durable(&page);
    {
        let snapshot = pair.ordinary.snapshot();
        assert_eq!(snapshot.pending_external_interactions.len(), 1);
        let pending = &snapshot.pending_external_interactions[0];
        assert_eq!(pending.request_id, "approval-1");
        assert_eq!(pending.provenance.generation, GENERATION);
        assert_eq!(pending.interaction_kind, ExternalInteractionKind::Approval);
        assert_eq!(pending.tool_name.as_deref(), Some("Bash"));
        assert_eq!(
            pending.input_preview.as_deref(),
            Some("{\"command\":\"cargo test\"}")
        );
        assert_eq!(pending.choices.len(), 1);
        assert!(pending.response.enabled);
        assert_eq!(
            system_text(&snapshot.transcript.items[0]),
            Some("Claude Code is asking for approval to run Bash")
        );
    }

    // A failed response leaves the request answerable.
    let failed = fixture.completed(
        fixture.respond_request("approval-1"),
        HarnessOperation::Respond,
        ExternalHarnessResponseKind::Failure(HarnessFailure::Conflict),
    );
    pair.durable(&failed);
    assert_eq!(
        pair.ordinary.snapshot().pending_external_interactions.len(),
        1
    );

    let answered = fixture.completed(
        fixture.respond_request("approval-1"),
        HarnessOperation::Respond,
        ExternalHarnessResponseKind::Acknowledged {
            mutation_id: "response-1".into(),
        },
    );
    pair.durable(&answered);
    assert_eq!(pair.pending_count(), 0);

    // A newer generation makes older requests unanswerable.
    let page = fixture.page(vec![(
        "interaction-2",
        HarnessObservationKind::Interaction {
            interaction: fixture.interaction(GENERATION, "approval-2"),
        },
    )]);
    pair.durable(&page);
    assert_eq!(
        pair.ordinary.snapshot().pending_external_interactions.len(),
        1
    );
    let newer = fixture.page_at(
        GENERATION + 1,
        vec![("idle", HarnessObservationKind::Status { status: idle() })],
    );
    pair.durable(&newer);
    assert_eq!(pair.pending_count(), 0);
}

#[test]
fn withdrawn_interactions_stop_pending_without_a_notice() {
    let mut fixture = Fixture::new();
    let mut pair = Pair::new();
    pair.durable(&fixture.configured());
    let page = fixture.page(vec![
        (
            "interaction-1",
            HarnessObservationKind::Interaction {
                interaction: fixture.interaction(GENERATION, "approval-1"),
            },
        ),
        (
            "interaction-2",
            HarnessObservationKind::Interaction {
                interaction: fixture.interaction(GENERATION, "approval-2"),
            },
        ),
    ]);
    pair.durable(&page);
    assert_eq!(pair.pending_count(), 2);
    let items = pair.items().len();
    let withdrawn = fixture.page(vec![(
        "withdrawn-1",
        HarnessObservationKind::InteractionWithdrawn {
            request_id: "approval-1".into(),
        },
    )]);
    pair.durable(&withdrawn);
    let pending = &pair.ordinary.snapshot().pending_external_interactions;
    assert_eq!(pending.len(), 1, "only the withdrawn prompt is dropped");
    assert_eq!(pending[0].request_id, "approval-2");
    assert_eq!(
        pair.items().len(),
        items,
        "withdrawal adds no transcript item"
    );
}

#[test]
fn direct_interaction_records_are_resolved_by_response_records() {
    let mut fixture = Fixture::new();
    let mut pair = Pair::new();
    let interaction = fixture.interaction(GENERATION, "question-1");
    let observed = fixture.envelope(ExternalBackendRecord::Observation {
        observation: fixture.observation(
            GENERATION,
            "interaction",
            HarnessObservationKind::Interaction {
                interaction: interaction.clone(),
            },
        ),
    });
    pair.durable(&observed);
    let pending = &pair.ordinary.snapshot().pending_external_interactions;
    assert_eq!(pending.len(), 1);
    assert!(
        pending[0].response.enabled,
        "unknown capabilities remain answerable; the daemon rechecks"
    );
    let responded = fixture.envelope(ExternalBackendRecord::Response {
        response: HarnessResponse {
            response_id: "response".into(),
            request_id: "question-1".into(),
            session_id: fixture.session_id.to_string(),
            generation: GENERATION,
            value: HarnessResponseValue::Approval { approved: false },
        },
        interaction,
    });
    pair.durable(&responded);
    assert_eq!(pair.pending_count(), 0);
}

#[test]
fn history_window_rebuild_keeps_external_presentation_state() {
    let mut fixture = Fixture::new();
    let configured = fixture.configured();
    let interaction = fixture.page(vec![(
        "interaction",
        HarnessObservationKind::Interaction {
            interaction: fixture.interaction(GENERATION, "approval-1"),
        },
    )]);
    let text = fixture.page(vec![(
        "text",
        HarnessObservationKind::Text {
            text: "Hello".into(),
            segment_id: Some("msg-1:0".into()),
        },
    )]);
    let mut view = SessionView::new();
    view.apply_history(&[configured, interaction]);
    view.rebuild_history_window(std::slice::from_ref(&text));
    let snapshot = view.snapshot();
    assert_eq!(snapshot.pending_external_interactions.len(), 1);
    assert_eq!(
        assistant_text(&snapshot.transcript.items[0]),
        Some(("Hello", Some("Claude Code")))
    );
}

#[test]
fn rejected_budget_leaves_view_and_ledger_unchanged() {
    let mut fixture = Fixture::new();
    let mut view = SessionView::new();
    let mut ledger = view.observation_retention_usage(limits()).unwrap();
    view.apply_event_bounded(&fixture.configured(), &mut ledger, limits())
        .unwrap();
    let page = fixture.page(vec![
        (
            "text",
            HarnessObservationKind::Text {
                text: "a".repeat(4_096),
                segment_id: Some("msg-1:0".into()),
            },
        ),
        (
            "tool",
            HarnessObservationKind::Tool {
                native_tool_id: "toolu_1".into(),
                name: "Bash".into(),
                status: "running".into(),
                input_json: Some("{}".into()),
            },
        ),
        (
            "interaction",
            HarnessObservationKind::Interaction {
                interaction: fixture.interaction(GENERATION, "approval-1"),
            },
        ),
    ]);
    let before = view.snapshot().clone();
    let before_ledger = ledger;
    let denied = ObservationRetentionLimits {
        scalar_capacity_bytes: ledger.scalar_capacity_bytes,
        ..limits()
    };
    assert!(
        view.apply_event_bounded(&page, &mut ledger, denied)
            .is_err()
    );
    assert_eq!(view.snapshot(), &before);
    assert_eq!(ledger, before_ledger);
    assert_eq!(ledger, view.observation_retention_usage(limits()).unwrap());

    view.apply_event_bounded(&page, &mut ledger, limits())
        .unwrap();
    assert_eq!(view.snapshot().transcript.items.len(), 3);
    assert_eq!(view.snapshot().pending_external_interactions.len(), 1);
    assert_eq!(ledger, view.observation_retention_usage(limits()).unwrap());
}

#[test]
fn invalid_canonical_evidence_is_surfaced_without_partial_projection() {
    let mut fixture = Fixture::new();
    let mut page = fixture.page(vec![(
        "text",
        HarnessObservationKind::Text {
            text: "Hello".into(),
            segment_id: None,
        },
    )]);
    if let SessionEventKind::ExternalHarnessOperation { operation } = &mut page.kind {
        operation.version = 2;
    }
    let mut ordinary = SessionView::new();
    ordinary.apply_event(&page);
    assert_eq!(ordinary.snapshot().transcript.items.len(), 1);
    assert!(
        system_text(&ordinary.snapshot().transcript.items[0])
            .is_some_and(|text| text.contains("invalid canonical evidence"))
    );
    let mut bounded = SessionView::new();
    let mut ledger = bounded.observation_retention_usage(limits()).unwrap();
    let before = ledger;
    assert!(
        bounded
            .apply_event_bounded(&page, &mut ledger, limits())
            .is_err()
    );
    assert_eq!(ledger, before);
    assert_eq!(bounded.snapshot().transcript.items.len(), 0);
}

#[test]
fn journal_rollback_restores_every_touched_owner() {
    let mut fixture = Fixture::new();
    let mut view = SessionView::new();
    view.apply_event(&fixture.configured());
    view.apply_live_event(&fixture.live_text("msg-1:0", 1, 0, "Hel"));
    view.apply_live_event(&fixture.live_reasoning("msg-0:0", "Think"));
    let interaction = fixture.page(vec![(
        "interaction",
        HarnessObservationKind::Interaction {
            interaction: fixture.interaction(GENERATION, "approval-1"),
        },
    )]);
    view.apply_event(&interaction);
    view.snapshot.transcript.items.shrink_to_fit();
    let before = view.snapshot().clone();
    let capacity = view.snapshot().transcript.items.capacity();
    let usage = view.observation_retention_usage(limits()).unwrap();
    // Durable text for a live segment terminalizes its stream.
    let text = fixture.page(vec![(
        "text",
        HarnessObservationKind::Text {
            text: "Hello".into(),
            segment_id: Some("msg-1:0".into()),
        },
    )]);
    let page = fixture.page_at(
        GENERATION + 1,
        vec![
            (
                "thinking",
                HarnessObservationKind::Reasoning {
                    segment_id: Some("msg-0:0".into()),
                    text: "Thinking done".into(),
                    opaque: false,
                },
            ),
            (
                "tool",
                HarnessObservationKind::Tool {
                    native_tool_id: "toolu_1".into(),
                    name: "Bash".into(),
                    status: "running".into(),
                    input_json: None,
                },
            ),
            (
                "usage",
                HarnessObservationKind::Usage {
                    input_tokens: 1,
                    output_tokens: 2,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                    total_cost_micros: Some(3),
                    duration_ms: None,
                    model: None,
                },
            ),
        ],
    );
    for event in [&text, &page] {
        let journal = view.reduce_external_event(event).unwrap();
        assert_ne!(view.snapshot(), &before);
        view.rollback_external(journal);
        assert_eq!(view.snapshot(), &before);
        assert_eq!(view.snapshot().transcript.items.capacity(), capacity);
        assert_eq!(view.observation_retention_usage(limits()).unwrap(), usage);
    }
}

#[test]
fn bounded_accounting_matches_full_inspection_across_many_turns() {
    let mut fixture = Fixture::new();
    let mut pair = Pair::new();
    pair.durable(&fixture.configured());
    for index in 0..20 {
        let segment = format!("msg-{index}:0");
        pair.live(&fixture.live_text(&segment, 1, 0, "partial"));
        let tool_id = format!("toolu_{index}");
        let usage_id = format!("usage-{index}");
        let page = fixture.page(vec![
            (
                "text",
                HarnessObservationKind::Text {
                    text: format!("Reply {index}"),
                    segment_id: Some(segment.clone()),
                },
            ),
            (
                "tool",
                HarnessObservationKind::Tool {
                    native_tool_id: tool_id.clone(),
                    name: "Read".into(),
                    status: "running".into(),
                    input_json: Some(r#"{"path":"src/lib.rs"}"#.into()),
                },
            ),
            (
                "tool-result",
                HarnessObservationKind::ToolResult {
                    native_tool_id: tool_id,
                    output: "fn main() {}".into(),
                    is_error: index % 2 == 0,
                    truncated: false,
                },
            ),
            (
                usage_id.as_str(),
                HarnessObservationKind::Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                    total_cost_micros: Some(100),
                    duration_ms: None,
                    model: None,
                },
            ),
        ]);
        pair.durable(&page);
    }
    let snapshot = pair.ordinary.snapshot();
    assert_eq!(snapshot.transcript.items.len(), 40);
    assert_eq!(snapshot.runtime.cost.totals_micros.get("USD"), Some(&2_000));
    let failed = snapshot
        .transcript
        .items
        .iter()
        .filter(|item| {
            matches!(&item.kind, TranscriptViewItemKind::ToolInvocation { tool }
                if tool.status == ToolInvocationViewStatus::Failed)
        })
        .count();
    assert_eq!(failed, 10);
}

#[test]
fn daemon_usage_summary_supersedes_native_usage_without_double_counting() {
    let mut fixture = Fixture::new();
    let mut pair = Pair::new();
    pair.durable(&fixture.configured());
    let page = full_turn_page(&mut fixture);
    pair.durable(&page);
    // Before any daemon summary, native usage is observed from canonical events.
    assert!(pair.ordinary.snapshot().runtime.cumulative_metered_tokens > 0);
    // The daemon summary is authoritative and already includes native harness usage, so a
    // refresh replaces locally observed totals rather than adding to them.
    let summary = bcode_session_models::SessionUsageSummary {
        through_sequence: Some(page.sequence),
        cumulative_metered_tokens: 1_200,
        totals_micros: std::iter::once(("USD".to_owned(), 12_345)).collect(),
        estimated_usage_count: 1,
        observed_usage_count: 1,
        ..bcode_session_models::SessionUsageSummary::default()
    };
    for view in [&mut pair.ordinary, &mut pair.bounded] {
        view.set_usage_summary(summary.clone());
        let runtime = &view.snapshot().runtime;
        assert_eq!(runtime.cumulative_metered_tokens, 1_200);
        assert_eq!(runtime.cost.totals_micros.get("USD"), Some(&12_345));
        assert_eq!(runtime.cost.observed_usage_count, 1);
    }
    // Replaying covered native evidence after the checkpoint does not count it again.
    pair.ordinary.apply_event(&page);
    let runtime = &pair.ordinary.snapshot().runtime;
    assert_eq!(runtime.cumulative_metered_tokens, 1_200);
    assert_eq!(runtime.cost.totals_micros.get("USD"), Some(&12_345));
}

#[test]
fn reopened_view_uses_daemon_summary_for_native_usage_outside_loaded_history() {
    let mut fixture = Fixture::new();
    let configured = fixture.configured();
    let earlier = full_turn_page(&mut fixture);
    let loaded = full_turn_page(&mut fixture);
    // Reopen: the daemon summary covers both turns although only the last is loaded.
    let summary = bcode_session_models::SessionUsageSummary {
        through_sequence: Some(loaded.sequence),
        cumulative_metered_tokens: 2_400,
        totals_micros: std::iter::once(("USD".to_owned(), 24_690)).collect(),
        estimated_usage_count: 2,
        observed_usage_count: 2,
        ..bcode_session_models::SessionUsageSummary::default()
    };
    let mut view = SessionView::new();
    view.set_usage_summary(summary);
    view.apply_event(&configured);
    view.apply_event(&loaded);
    let runtime = &view.snapshot().runtime;
    assert_eq!(runtime.cumulative_metered_tokens, 2_400);
    assert_eq!(runtime.cost.totals_micros.get("USD"), Some(&24_690));
    assert_eq!(runtime.cost.observed_usage_count, 2);
    assert!(earlier.sequence < loaded.sequence);
}

/// Canonical evidence shaped exactly like a decoded native page entry.
fn native_kind(value: serde_json::Value) -> HarnessObservationKind {
    serde_json::from_value(value).unwrap()
}

/// One canonical operation recorded by an explicit daemon lifetime.
fn operation_by(
    fixture: &mut Fixture,
    generation: u64,
    daemon_id: &str,
    record: ExternalHarnessOperationRecord,
) -> SessionEvent {
    operation_daemon(fixture.operation(generation, record), daemon_id)
}

/// Claimed and completed records for one exact request, as the daemon commits them.
fn delivered(
    fixture: &mut Fixture,
    generation: u64,
    daemon_id: &str,
    request: &ExternalHarnessRequest,
    operation: HarnessOperation,
    kind: ExternalHarnessResponseKind,
) -> [SessionEvent; 2] {
    let claimed = operation_by(
        fixture,
        generation,
        daemon_id,
        ExternalHarnessOperationRecord::DeliveryClaimed {
            request: request.clone(),
        },
    );
    let response = ExternalHarnessResponse {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: request.request_id.clone(),
        operation,
        kind,
    };
    let completed = operation_by(
        fixture,
        generation,
        daemon_id,
        ExternalHarnessOperationRecord::DeliveryCompleted {
            request: request.clone(),
            response,
        },
    );
    [claimed, completed]
}

/// Submit one turn and receive its accepted receipt.
fn native_turn_submission(
    fixture: &mut Fixture,
    generation: u64,
    daemon_id: &str,
    target: &HarnessSessionTarget,
    turn: u32,
) -> [SessionEvent; 2] {
    let submission = HarnessSubmission {
        submission_id: format!("submit-{turn}"),
        attempt_id: format!("attempt-{turn}"),
        session_id: fixture.session_id.to_string(),
        generation,
        text: format!("turn-{turn}"),
        placement: HarnessPlacement::NextTurn,
    };
    let request = Fixture::request(
        ExternalHarnessRequestKind::Submit {
            target: target.clone(),
            submission: submission.clone(),
        },
        &submission.submission_id,
    );
    let receipt = HarnessReceipt {
        submission_id: submission.submission_id.clone(),
        attempt_id: submission.attempt_id.clone(),
        session_id: submission.session_id,
        generation,
        state: HarnessDeliveryState::Accepted,
        native_request_id: None,
    };
    delivered(
        fixture,
        generation,
        daemon_id,
        &request,
        HarnessOperation::Submit,
        ExternalHarnessResponseKind::Receipt(receipt),
    )
}

/// The lifecycle and host-tool telemetry a native turn starts with; none of it is content.
fn native_turn_preamble(turn: u32) -> Vec<(String, HarnessObservationKind)> {
    vec![
        (
            format!("t{turn}-telemetry-1"),
            HarnessObservationKind::Vendor {
                namespace: "claude_code.host_tools".into(),
                detail: serde_json::json!({"code": "host_tools_native_status", "message": "connected"}),
            },
        ),
        (
            format!("t{turn}-telemetry-2"),
            HarnessObservationKind::Vendor {
                namespace: "claude_code.host_tools".into(),
                detail: serde_json::json!({"code": "host_tools_queue_revoked", "message": "exact host queue scope revoked"}),
            },
        ),
        (
            format!("t{turn}-coverage"),
            native_kind(
                serde_json::json!({"kind": "execution", "event": {"event": "coverage",
                "attempt": {"submission_id": format!("submit-{turn}"), "attempt_id": format!("attempt-{turn}")},
                "background": "unknown", "continuations": "unknown"}}),
            ),
        ),
    ]
}

/// Streamed content of one native turn: thinking, text, one tool, its result, a reply, usage.
fn native_turn_content(turn: u32, with_tool_call: bool) -> Vec<(String, HarnessObservationKind)> {
    let submission = format!("submit-{turn}");
    let attempt = format!("attempt-{turn}");
    let message = format!("msg_fixture_turn-{turn}");
    let tool = format!("toolu_fixture_turn-{turn}");
    let mut kinds = vec![
        (
            format!("t{turn}-consumed"),
            native_kind(serde_json::json!({"kind": "submission_consumed",
                "submission_id": submission, "attempt_id": attempt, "native_frame_id": format!("frame-{turn}-a")})),
        ),
        (
            format!("t{turn}-thinking"),
            HarnessObservationKind::Reasoning {
                segment_id: Some(format!("{message}:0")),
                text: format!("Considering turn-{turn}"),
                opaque: false,
            },
        ),
        (
            format!("t{turn}-text"),
            HarnessObservationKind::Text {
                text: format!("Streaming **reply** turn-{turn}"),
                segment_id: Some(format!("{message}:1")),
            },
        ),
    ];
    if with_tool_call {
        kinds.push((
            format!("t{turn}-tool"),
            HarnessObservationKind::Tool {
                native_tool_id: tool.clone(),
                name: "Read".into(),
                status: "observed".into(),
                input_json: Some("{\"file_path\":\"/fixture/notes.txt\"}".into()),
            },
        ));
    }
    kinds.extend([
        (
            format!("t{turn}-tool-result"),
            HarnessObservationKind::ToolResult {
                native_tool_id: tool,
                output: "fixture notes".into(),
                is_error: false,
                truncated: false,
            },
        ),
        (
            format!("t{turn}-consumed-reply"),
            native_kind(serde_json::json!({"kind": "submission_consumed",
                "submission_id": submission, "attempt_id": attempt, "native_frame_id": format!("frame-{turn}-b")})),
        ),
        (
            format!("t{turn}-reply"),
            HarnessObservationKind::Text {
                text: format!("reply turn-{turn}"),
                segment_id: None,
            },
        ),
        (
            format!("t{turn}-result-observed"),
            native_kind(serde_json::json!({"kind": "submission_result_observed",
                "submission_id": submission, "attempt_id": attempt, "native_frame_id": format!("frame-{turn}-c"),
                "is_error": false, "queued_user_sends": 0})),
        ),
        (
            format!("t{turn}-terminal"),
            native_kind(serde_json::json!({"kind": "execution", "event": {"event": "submission_terminal",
                "attempt": {"submission_id": submission, "attempt_id": attempt}, "native_turn_id": null,
                "native_frame_id": format!("frame-{turn}-c"), "disposition": "completed",
                "output": format!("reply turn-{turn}")}})),
        ),
        (
            format!("t{turn}-result"),
            HarnessObservationKind::Result {
                text: format!("reply turn-{turn}"),
            },
        ),
        (
            format!("t{turn}-usage"),
            HarnessObservationKind::Usage {
                input_tokens: 12,
                output_tokens: 34,
                cache_read_input_tokens: 5,
                cache_creation_input_tokens: 6,
                total_cost_micros: Some(1_200),
                duration_ms: Some(1_500),
                model: Some("claude-fixture".into()),
            },
        ),
    ]);
    kinds
}

fn page_of(
    fixture: &mut Fixture,
    generation: u64,
    kinds: &[(String, HarnessObservationKind)],
) -> SessionEvent {
    fixture.page_at(
        generation,
        kinds
            .iter()
            .map(|(id, kind)| (id.as_str(), kind.clone()))
            .collect(),
    )
}

/// The canonical stream of a full native Claude Code conversation (as committed by the daemon
/// for open, streamed turn with approval, second turn, model and plan-mode changes, a clean
/// daemon stop, explicit recovery and a resumed turn) projects into native transcript items.
#[test]
#[allow(clippy::too_many_lines)] // One end-to-end conversation keeps its stream order readable.
fn native_conversation_stream_projects_native_items_without_lifecycle_noise() {
    const RECOVERED: u64 = GENERATION + 1;
    let mut fixture = Fixture::new();
    let mut pair = Pair::new();
    let session = fixture.session_id.to_string();
    let durable_all = |pair: &mut Pair, events: &[SessionEvent]| {
        for event in events {
            pair.durable(event);
        }
    };

    let configured = operation_by(
        &mut fixture,
        GENERATION,
        "daemon",
        ExternalHarnessOperationRecord::Configured {
            instance: instance(),
            capabilities: Box::new(capabilities()),
            open_request: None,
        },
    );
    pair.durable(&configured);
    let prepared = bcode_harness_models::HarnessPreparedSession {
        open_id: "open-1".into(),
        session_id: session.clone(),
        instance_id: "work".into(),
        generation: GENERATION,
        native_version: "1.0".into(),
        requested_native_session_id: Some("native".into()),
    };
    let open = Fixture::request(
        ExternalHarnessRequestKind::Open(bcode_harness_models::HarnessOpenRequest {
            open_id: "open-1".into(),
            session_id: session.clone(),
            generation: GENERATION,
            working_directory: "/workspace".into(),
            resume: None,
            parent_assistance: None,
        }),
        "open-1",
    );
    let events = delivered(
        &mut fixture,
        GENERATION,
        "daemon",
        &open,
        HarnessOperation::Open,
        ExternalHarnessResponseKind::Prepared(prepared.clone()),
    );
    durable_all(&mut pair, &events);

    // Turn 1: the first submission targets the prepared session; output streams live first.
    let prepared_target = HarnessSessionTarget::Prepared(prepared);
    let events = native_turn_submission(&mut fixture, GENERATION, "daemon", &prepared_target, 1);
    durable_all(&mut pair, &events);
    let mut preamble = native_turn_preamble(1);
    preamble.insert(
        2,
        (
            "t1-bound".into(),
            HarnessObservationKind::BindingConfirmed {
                open_id: "open-1".into(),
                binding: fixture.binding(GENERATION),
            },
        ),
    );
    let page = page_of(&mut fixture, GENERATION, &preamble);
    pair.durable(&page);
    let idle_page = page_of(&mut fixture, GENERATION, &[]);
    pair.durable(&idle_page);
    pair.live(&fixture.live_reasoning("msg_fixture_turn-1:0", "Considering turn-1"));
    pair.live(&fixture.live_text("msg_fixture_turn-1:1", 1, 0, "Streaming "));
    pair.live(&fixture.live_text("msg_fixture_turn-1:1", 2, 10, "**reply** turn-1"));
    let streamed = pair.items().len();

    // Native permission arrives with the finished segment, then the tool waits for approval.
    let mut content = native_turn_content(1, true);
    let after_approval = content.split_off(4);
    let mut first = vec![(
        "t1-approval".to_owned(),
        HarnessObservationKind::Interaction {
            interaction: HarnessInteraction {
                request_id: "native-permission-turn-1".into(),
                session_id: session.clone(),
                generation: GENERATION,
                kind: HarnessInteractionKind::Approval,
                prompt: "Allow native Claude tool Read?".into(),
                scope: "Native tool Read; untrusted display only".into(),
                choices: vec![],
                tool_name: Some("Read".into()),
                input_preview: Some("{\"file_path\":\"/fixture/notes.txt\"}".into()),
            },
        },
    )];
    first.extend(content);
    let page = page_of(&mut fixture, GENERATION, &first);
    pair.durable(&page);
    assert_eq!(
        pair.pending_count(),
        1,
        "native approval is a pending dialog"
    );
    assert_eq!(
        pair.items().len(),
        streamed + 2,
        "durable reasoning and text replace their live items in place; only the notice and \
         the tool card are new: {:#?}",
        pair.items()
    );
    let confirmed = HarnessSessionTarget::Confirmed(fixture.binding(GENERATION));
    let approval = Fixture::request(
        ExternalHarnessRequestKind::Respond {
            target: confirmed.clone(),
            response: HarnessResponse {
                response_id: "approve-submit-1".into(),
                request_id: "native-permission-turn-1".into(),
                session_id: session.clone(),
                generation: GENERATION,
                value: HarnessResponseValue::Approval { approved: true },
            },
        },
        "approve-submit-1",
    );
    let events = delivered(
        &mut fixture,
        GENERATION,
        "daemon",
        &approval,
        HarnessOperation::Respond,
        ExternalHarnessResponseKind::Acknowledged {
            mutation_id: "approve-submit-1".into(),
        },
    );
    durable_all(&mut pair, &events);
    assert_eq!(
        pair.pending_count(),
        0,
        "an acknowledged approval resolves the dialog"
    );
    let page = page_of(&mut fixture, GENERATION, &after_approval);
    pair.durable(&page);

    // Turn 2 on the same native process.
    let events = native_turn_submission(&mut fixture, GENERATION, "daemon", &confirmed, 2);
    durable_all(&mut pair, &events);
    let page = page_of(&mut fixture, GENERATION, &native_turn_preamble(2));
    pair.durable(&page);
    let page = page_of(&mut fixture, GENERATION, &native_turn_content(2, true));
    pair.durable(&page);

    // Model switch and plan mode are acknowledged settings, not conversation content.
    for (mutation, setting) in [
        (
            "configure-model-1",
            bcode_harness_models::HarnessSessionSetting::Model {
                model_id: "claude-fixture".into(),
            },
        ),
        (
            "configure-mode-1",
            bcode_harness_models::HarnessSessionSetting::PermissionMode {
                mode: bcode_harness_models::HarnessPermissionMode::Plan,
            },
        ),
    ] {
        let request = Fixture::request(
            ExternalHarnessRequestKind::Configure(bcode_harness_models::HarnessConfigureRequest {
                target: confirmed.clone(),
                mutation_id: mutation.into(),
                setting,
            }),
            mutation,
        );
        let events = delivered(
            &mut fixture,
            GENERATION,
            "daemon",
            &request,
            HarnessOperation::Configure,
            ExternalHarnessResponseKind::Acknowledged {
                mutation_id: mutation.into(),
            },
        );
        durable_all(&mut pair, &events);
    }

    // Clean daemon stop: the native transport is closed and its release recorded.
    let close = Fixture::request(
        ExternalHarnessRequestKind::Close(bcode_harness_models::HarnessControlRequest {
            target: confirmed,
            mutation_id: "shutdown-1".into(),
        }),
        "shutdown-1",
    );
    let events = delivered(
        &mut fixture,
        GENERATION,
        "daemon",
        &close,
        HarnessOperation::Close,
        ExternalHarnessResponseKind::Acknowledged {
            mutation_id: "shutdown-1".into(),
        },
    );
    durable_all(&mut pair, &events);
    let release = bcode_session_models::external_harness::ExternalHarnessTransportRelease {
        close_request_id: "shutdown-1".into(),
        instance: instance(),
        binding: fixture.binding(GENERATION),
        lifetime: bcode_session_models::external_harness::ExternalHarnessLifetime {
            daemon_instance_id: "daemon".into(),
            artifact_id: "artifact".into(),
            state_location_id: "location".into(),
        },
        reap_id: "open-1".into(),
    };
    let released = operation_by(
        &mut fixture,
        GENERATION,
        "daemon",
        ExternalHarnessOperationRecord::TransportReleased {
            release: release.clone(),
        },
    );
    let release_sequence = released.sequence;
    pair.durable(&released);

    // Reopen: explicit recovery resumes the exact native conversation under a new generation.
    let resumed_binding = fixture.binding(RECOVERED);
    let session_id = fixture.session_id;
    let resume = Fixture::request(
        ExternalHarnessRequestKind::Open(bcode_harness_models::HarnessOpenRequest {
            open_id: "recover-1".into(),
            session_id: session,
            generation: RECOVERED,
            working_directory: "/workspace".into(),
            resume: Some(resumed_binding.clone()),
            parent_assistance: None,
        }),
        "recover-1",
    );
    let recovery = operation_by(
        &mut fixture,
        RECOVERED,
        "daemon-2",
        ExternalHarnessOperationRecord::RecoveryPrepared {
            recovery: Box::new(
                bcode_session_models::external_harness::ExternalHarnessRecovery {
                    previous_owner: ExternalHarnessOwner {
                        session_id,
                        generation: GENERATION,
                        artifact_id: "artifact".into(),
                        daemon_id: "daemon".into(),
                    },
                    release_event_sequence: release_sequence,
                    release,
                    capabilities: capabilities(),
                    resume_request: resume.clone(),
                    abandoned: false,
                },
            ),
        },
    );
    pair.durable(&recovery);
    let events = delivered(
        &mut fixture,
        RECOVERED,
        "daemon-2",
        &resume,
        HarnessOperation::Open,
        ExternalHarnessResponseKind::Opened(resumed_binding.clone()),
    );
    durable_all(&mut pair, &events);

    // Turn 3 continues on the resumed native session.
    let resumed = HarnessSessionTarget::Confirmed(resumed_binding);
    let events = native_turn_submission(&mut fixture, RECOVERED, "daemon-2", &resumed, 3);
    durable_all(&mut pair, &events);
    let page = page_of(&mut fixture, RECOVERED, &native_turn_preamble(3));
    let page = operation_daemon(page, "daemon-2");
    pair.durable(&page);
    let page = page_of(&mut fixture, RECOVERED, &native_turn_content(3, true));
    let page = operation_daemon(page, "daemon-2");
    pair.durable(&page);

    // Every item is a native transcript kind; the only notices are the documented compact
    // preparation, attention, release and recovery notices.
    let mut shape = Vec::new();
    for item in pair.items() {
        shape.push(match &item.kind {
            TranscriptViewItemKind::UserMessage { message } => format!("user:{}", message.text),
            TranscriptViewItemKind::AssistantMessage { message } => {
                assert_eq!(message.display_label.as_deref(), Some("Claude Code"));
                assert_eq!(
                    message.format,
                    bcode_session_view_models::TextFormat::Markdown
                );
                format!("assistant:{}", message.text)
            }
            TranscriptViewItemKind::ReasoningActivity { activity } => {
                assert!(activity.finished);
                format!("reasoning:{}", activity.text())
            }
            TranscriptViewItemKind::ToolInvocation { tool } => {
                assert_eq!(tool.status, ToolInvocationViewStatus::Finished);
                assert_eq!(tool.is_error, Some(false));
                assert_eq!(
                    tool.arguments_json.as_deref(),
                    Some("{\"file_path\":\"/fixture/notes.txt\"}")
                );
                format!(
                    "tool:{}={}",
                    tool.tool_name.as_deref().unwrap_or_default(),
                    tool.result_text.as_deref().unwrap_or_default()
                )
            }
            TranscriptViewItemKind::SystemMessage { message } => {
                format!("notice:{}", message.text)
            }
            other => panic!("non-native transcript item {other:?}"),
        });
    }
    let turn = |n: u32| {
        vec![
            format!("reasoning:Considering turn-{n}"),
            format!("assistant:Streaming **reply** turn-{n}"),
            "tool:Read=fixture notes".to_owned(),
            format!("assistant:reply turn-{n}"),
        ]
    };
    let mut expected = vec!["user:turn-1".to_owned()];
    // Live-streamed items keep their place; the approval notice follows the streamed text.
    let mut first_turn = turn(1);
    first_turn.insert(
        2,
        "notice:Claude Code is asking for approval to run Read".to_owned(),
    );
    expected.extend(first_turn);
    expected.push("user:turn-2".to_owned());
    expected.extend(turn(2));
    // Releasing and resuming the native process is invisible, like a native conversation.
    expected.push("user:turn-3".to_owned());
    expected.extend(turn(3));
    assert_eq!(shape, expected);

    // Live items were finalized under their shared identity rather than duplicated.
    let identity = harness_stream_identity(GENERATION, "msg_fixture_turn-1:1");
    assert_eq!(
        pair.items()
            .iter()
            .filter(|item| item.id.get() == identity.assistant_item_id())
            .count(),
        1
    );
    let snapshot = pair.ordinary.snapshot();
    assert_eq!(snapshot.pending_external_interactions.len(), 0);
    let usage = snapshot.runtime.latest_usage.as_ref().unwrap();
    assert_eq!(usage.output_tokens, Some(34));
    assert_eq!(usage.cached_input_tokens, Some(5));
    // Observed native tools never become Bcode tool, permission or dispatch state.
    assert!(snapshot.tools.is_empty());
    assert!(snapshot.active_invocations.is_empty());
    assert_eq!(snapshot.permissions.len(), 0);
    assert_eq!(snapshot.interactions.len(), 0);
}

/// Re-attribute one recorded operation to another daemon lifetime.
fn operation_daemon(mut event: SessionEvent, daemon_id: &str) -> SessionEvent {
    if let SessionEventKind::ExternalHarnessOperation { operation } = &mut event.kind {
        daemon_id.clone_into(&mut operation.owner.daemon_id);
    }
    event
}

#[test]
fn bounded_ledger_stays_exact_when_an_empty_page_follows_execution_evidence() {
    let mut fixture = Fixture::new();
    let mut pair = Pair::new();
    pair.durable(&fixture.configured());
    // Execution evidence grows retained lifecycle collections; the next (empty) page must
    // release exactly what was charged for them, so `Pair` compares the ledger each step.
    let page = page_of(&mut fixture, GENERATION, &native_turn_preamble(1));
    pair.durable(&page);
    for _ in 0..3 {
        let idle = page_of(&mut fixture, GENERATION, &[]);
        pair.durable(&idle);
    }
    let page = page_of(&mut fixture, GENERATION, &native_turn_content(1, true));
    pair.durable(&page);
    let idle = page_of(&mut fixture, GENERATION, &[]);
    pair.durable(&idle);
}
