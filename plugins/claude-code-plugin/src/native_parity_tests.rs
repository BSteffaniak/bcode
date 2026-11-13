//! Native-parity normalization: segment identities, reasoning, tool input/output, usage,
//! approval previews, live sidecar and mid-session configuration surfaces.
use super::*;
use bcode_claude_code::CallbackResponse;
use bcode_harness_models::{
    HarnessLiveStream, MAX_HARNESS_PREVIEW_BYTES, MAX_HARNESS_TEXT_BYTES,
    MAX_HARNESS_TOOL_INPUT_BYTES,
};

fn assistant(value: serde_json::Value) -> Event {
    Event::Message(NativeMessage::Assistant(value))
}
fn stream(event: &serde_json::Value) -> Event {
    Event::Message(NativeMessage::Stream(
        json!({"type":"stream_event","parent_tool_use_id":null,"event":event}),
    ))
}

#[test]
fn per_block_assistant_frames_share_message_segment_numbering() {
    let mut normalizer = normalizer();
    let mut kinds = Vec::new();
    for block in [
        json!({"type":"thinking","thinking":"Plan the change","signature":"SECRET_SIGNATURE"}),
        json!({"type":"redacted_thinking","data":"SECRET_REDACTED"}),
        json!({"type":"text","text":"Done."}),
    ] {
        kinds.extend(
            normalizer
                .event(assistant(json!({"type":"assistant","message":{
                    "id":"msg_01","model":"claude-sonnet-4-6","content":[block]}})))
                .into_iter()
                .map(|observation| observation.kind),
        );
    }
    assert_eq!(
        kinds,
        vec![
            HarnessObservationKind::Reasoning {
                segment_id: Some("msg_01:r0".into()),
                text: "Plan the change".into(),
                opaque: false,
            },
            HarnessObservationKind::Reasoning {
                segment_id: Some("msg_01:r1".into()),
                text: String::new(),
                opaque: true,
            },
            HarnessObservationKind::Text {
                segment_id: Some("msg_01:t0".into()),
                text: "Done.".into(),
            },
        ]
    );
    assert!(!serde_json::to_string(&kinds).unwrap().contains("SECRET"));
    // Text and reasoning are numbered separately, so a streamed block without a durable
    // frame cannot shift the next segment. A new message restarts numbering.
    let next = normalizer.event(assistant(json!({"message":{"id":"msg_02","content":[
        {"type":"text","text":"a"},{"type":"text","text":"b"}]}})));
    let segments: Vec<_> = next
        .iter()
        .filter_map(|observation| match &observation.kind {
            HarnessObservationKind::Text { segment_id, .. } => segment_id.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(segments, ["msg_02:t0", "msg_02:t1"]);
    // Without a usable native message identity, text stays unsegmented.
    let anonymous = normalizer.event(assistant(json!({"message":{"content":[
        {"type":"text","text":"plain"}]}})));
    assert!(matches!(
        &anonymous[0].kind,
        HarnessObservationKind::Text {
            segment_id: None,
            ..
        }
    ));
}

#[test]
fn oversized_tool_input_reasoning_and_text_are_truncated_to_valid_bounded_display() {
    let mut normalizer = normalizer();
    let huge = "\"quoted\"\n".repeat(MAX_HARNESS_TOOL_INPUT_BYTES);
    let observations = normalizer.event(assistant(json!({"message":{"id":"msg_big","content":[
        {"type":"tool_use","id":"toolu_big","name":"Write","input":{"content":huge}},
        {"type":"thinking","thinking":"é".repeat(MAX_HARNESS_TEXT_BYTES)},
        {"type":"text","text":"ü".repeat(MAX_HARNESS_TEXT_BYTES)},
    ]}})));
    assert_eq!(observations.len(), 3);
    let HarnessObservationKind::Tool {
        input_json: Some(input),
        ..
    } = &observations[0].kind
    else {
        panic!("expected bounded tool input");
    };
    assert!(input.len() <= MAX_HARNESS_TOOL_INPUT_BYTES);
    let summary: serde_json::Value = serde_json::from_str(input).unwrap();
    assert_eq!(summary["truncated"], true);
    assert!(summary["original_bytes"].as_u64().unwrap() > MAX_HARNESS_TOOL_INPUT_BYTES as u64);
    let HarnessObservationKind::Reasoning { text, opaque, .. } = &observations[1].kind else {
        panic!("expected reasoning");
    };
    assert!(!opaque);
    assert!(text.len() <= MAX_HARNESS_TEXT_BYTES);
    assert!(text.ends_with("[truncated]"));
    // Oversized assistant text still finalizes its segment instead of dropping the message.
    let HarnessObservationKind::Text {
        text,
        segment_id: Some(segment),
    } = &observations[2].kind
    else {
        panic!("expected segmented assistant text");
    };
    assert_eq!(segment, "msg_big:t0");
    assert!(text.len() <= MAX_HARNESS_TEXT_BYTES);
    assert!(text.ends_with("[truncated]"));
    for observation in &observations {
        observation.validate().unwrap();
    }
}

#[test]
fn user_tool_results_become_bounded_untrusted_output() {
    let mut normalizer = normalizer();
    let observations = normalizer.event(Event::Message(NativeMessage::User(json!({
        "type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"toolu_1","content":"     1\tfile list"},
            {"type":"tool_result","tool_use_id":"toolu_2","is_error":true,"content":[
                {"type":"text","text":"failed"},{"type":"image","source":{"data":"SECRET_IMAGE"}},
                {"type":"tool_reference","tool_name":"WebSearch"}]},
            {"type":"tool_result","tool_use_id":"toolu_3","content":"x".repeat(MAX_HARNESS_TEXT_BYTES + 10)},
            {"type":"text","text":"prompt echo is not a tool result"},
        ]}}))));
    let kinds: Vec<_> = observations.iter().map(|value| &value.kind).collect();
    assert_eq!(kinds.len(), 3);
    assert_eq!(
        kinds[0],
        &HarnessObservationKind::ToolResult {
            native_tool_id: "toolu_1".into(),
            // Numbered native reads display without raw tabs.
            output: "     1    file list".into(),
            is_error: false,
            truncated: false,
        }
    );
    assert_eq!(
        kinds[1],
        &HarnessObservationKind::ToolResult {
            native_tool_id: "toolu_2".into(),
            output: "failed\n[image]\n[tool: WebSearch]".into(),
            is_error: true,
            truncated: false,
        }
    );
    let HarnessObservationKind::ToolResult {
        output, truncated, ..
    } = kinds[2]
    else {
        panic!("expected truncated tool result");
    };
    assert!(*truncated && output.len() <= MAX_HARNESS_TEXT_BYTES);
    assert!(!serde_json::to_string(&kinds).unwrap().contains("SECRET"));
}

#[test]
fn result_usage_reports_tokens_cost_duration_and_latest_model() {
    let mut normalizer = normalizer();
    normalizer.event(Event::Message(NativeMessage::System(
        json!({"type":"system","subtype":"init","model":"claude-opus-4-1"}),
    )));
    let result = json!({"type":"result","result":"ok","total_cost_usd":0.012_345_6,
        "duration_ms":1500,"usage":{"input_tokens":10,"output_tokens":20,
        "cache_read_input_tokens":30,"cache_creation_input_tokens":40}});
    let usage = |observations: Vec<HarnessObservation>| {
        observations
            .into_iter()
            .find_map(|observation| match observation.kind {
                kind @ HarnessObservationKind::Usage { .. } => Some(kind),
                _ => None,
            })
    };
    assert_eq!(
        usage(normalizer.event(Event::Message(NativeMessage::Result(result.clone())))),
        Some(HarnessObservationKind::Usage {
            input_tokens: 10,
            output_tokens: 20,
            cache_read_input_tokens: 30,
            cache_creation_input_tokens: 40,
            total_cost_micros: Some(12_346),
            duration_ms: Some(1500),
            model: Some("claude-opus-4-1".into()),
        })
    );
    // The assistant message model supersedes init; synthetic placeholders are ignored.
    normalizer.event(assistant(
        json!({"message":{"id":"m","model":"claude-sonnet-4-6","content":[]}}),
    ));
    normalizer.event(assistant(
        json!({"message":{"id":"n","model":"<synthetic>","content":[]}}),
    ));
    let Some(HarnessObservationKind::Usage {
        model,
        total_cost_micros,
        ..
    }) = usage(normalizer.event(Event::Message(NativeMessage::Result(result))))
    else {
        panic!("expected usage");
    };
    assert_eq!(model.as_deref(), Some("claude-sonnet-4-6"));
    assert_eq!(
        total_cost_micros,
        Some(0),
        "an unchanged running total adds no cost"
    );
    // Malformed costs are omitted; a result without usage reports none.
    let Some(HarnessObservationKind::Usage {
        total_cost_micros, ..
    }) = usage(normalizer.event(Event::Message(NativeMessage::Result(
        json!({"result":"ok","total_cost_usd":-1.0,"usage":{}}),
    ))))
    else {
        panic!("expected usage");
    };
    assert_eq!(total_cost_micros, None);
    assert_eq!(
        usage(normalizer.event(Event::Message(NativeMessage::Result(
            json!({"result":"ok"})
        )))),
        None
    );
}

#[test]
fn approvals_carry_tool_name_and_bounded_pretty_preview_without_dispatch() {
    let mut normalizer = normalizer();
    let observations = normalizer.event(callback("Bash", json!({"command":"cargo test"})));
    let HarnessObservationKind::Interaction { interaction } = &observations[0].kind else {
        panic!("expected approval");
    };
    assert_eq!(interaction.kind, HarnessInteractionKind::Approval);
    assert_eq!(interaction.tool_name.as_deref(), Some("Bash"));
    assert_eq!(
        interaction.input_preview.as_deref(),
        Some("{\n  \"command\": \"cargo test\"\n}")
    );
    normalizer.complete_callback("callback");
    let observations = normalizer.event(callback(
        "Write",
        json!({"content":"é".repeat(MAX_HARNESS_PREVIEW_BYTES)}),
    ));
    let HarnessObservationKind::Interaction { interaction } = &observations[0].kind else {
        panic!("expected approval");
    };
    let preview = interaction.input_preview.as_deref().unwrap();
    assert!(preview.len() <= MAX_HARNESS_PREVIEW_BYTES);
    assert!(preview.ends_with("[truncated]"));
    interaction.validate().unwrap();
    // Questions keep their established shape and never present as tool approvals.
    normalizer.complete_callback("callback");
    let observations = normalizer.event(callback(
        "AskUserQuestion",
        json!({"questions":[{"question":"Which?","options":[{"label":"One"}]}]}),
    ));
    let HarnessObservationKind::Interaction { interaction } = &observations[0].kind else {
        panic!("expected question");
    };
    assert_eq!(interaction.tool_name, None);
    assert_eq!(interaction.input_preview, None);
}

#[test]
fn every_question_form_is_answerable() {
    let mut normalizer = normalizer();
    // A multi-select question offers its options; several may be typed in one answer.
    let observations = normalizer.event(callback(
        "AskUserQuestion",
        json!({"questions":[{"question":"Which?","multiSelect":true,
            "options":[{"label":"One","description":"The first"},{"label":"Two"}]}]}),
    ));
    let HarnessObservationKind::Interaction { interaction } = &observations[0].kind else {
        panic!("expected a question");
    };
    assert_eq!(interaction.kind, HarnessInteractionKind::Question);
    assert!(interaction.prompt.starts_with("Which?"));
    assert_eq!(interaction.choices.len(), 2);
    assert_eq!(
        interaction.choices[0].description.as_deref(),
        Some("The first")
    );
    interaction.validate().unwrap();
    normalizer.complete_callback("callback");

    // Several questions are answered in one reply that covers every question.
    let input = json!({"questions":[
        {"question":"Scope?","options":[{"label":"Small"},{"label":"Large"}]},
        {"question":"Deadline?","options":[]}]});
    let observations = normalizer.event(callback("AskUserQuestion", input));
    let HarnessObservationKind::Interaction { interaction } = &observations[0].kind else {
        panic!("expected a question");
    };
    assert_eq!(
        interaction.prompt,
        "1. Scope? (Small / Large)\n2. Deadline?\nAnswer each question in one reply."
    );
    assert_eq!(interaction.choices.len(), 0);
    let response = HarnessResponse {
        response_id: "answer".into(),
        request_id: interaction.request_id.clone(),
        session_id: interaction.session_id.clone(),
        generation: interaction.generation,
        value: HarnessResponseValue::Answer {
            choice_id: None,
            text: "Small, by Friday".into(),
        },
    };
    let CallbackResponse::Allow { updated_input } = normalizer.response(&response).unwrap() else {
        panic!("expected the answer to allow the question");
    };
    assert_eq!(
        updated_input["answers"],
        json!({"Scope?": "Small, by Friday", "Deadline?": "Small, by Friday"})
    );
}

#[test]
fn live_sidecar_is_offered_once_waking_pages_and_matching_durable_identity() {
    let (prepared, target, mut state) = eof_stream_fixture();
    state.normalizer.enable_retention();
    let observe = eof_observe_request(target);
    assert!(!state.ready_to_offer());
    for event in [
        stream(&json!({"type":"message_start","message":{"id":"msg_live"}})),
        stream(
            &json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        ),
        stream(
            &json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}),
        ),
        stream(
            &json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}),
        ),
    ] {
        let observations = state.normalizer.event(event);
        assert!(state.push(observations));
    }
    // Live text alone makes a waiting observe ready, without durable evidence.
    assert!(state.ready_to_offer());
    assert!(!state.durable_ready());
    let HarnessServiceResponseKind::Observations(page) = state
        .observation_page(&prepared, &observe, &Settlement::Running)
        .unwrap()
    else {
        panic!("expected page");
    };
    page.validate_retention().unwrap();
    assert_eq!(page.observations, Vec::new());
    assert_eq!(page.live.len(), 1);
    assert_eq!(page.live[0].stream, HarnessLiveStream::Text);
    assert_eq!(page.live[0].text, "Hello");
    let live_segment = page.live[0].segment_id.clone();
    // A live-only page transfers no durable observations, so it carries no retention page
    // and never forces a canonical commit per coalesced live window.
    assert_eq!(page.retention, None);
    // Repeating the unacknowledged page never repeats offered live text.
    let HarnessServiceResponseKind::Observations(repeat) = state
        .observation_page(&prepared, &observe, &Settlement::Running)
        .unwrap()
    else {
        panic!("expected page");
    };
    assert_eq!(repeat.live, Vec::new());
    assert_eq!(repeat.retention, page.retention);
    // The durable per-block frame for the same native block maps to the same segment.
    let durable = state.normalizer.event(assistant(json!({"type":"assistant",
        "message":{"id":"msg_live","content":[{"type":"text","text":"Hello"}]}})));
    assert!(
        durable.is_empty(),
        "durable records are staged until pumped"
    );
    let pumped = state.normalizer.pump_batch(8, 8);
    assert!(pumped.iter().any(|observation| matches!(
        &observation.kind,
        HarnessObservationKind::Text { segment_id: Some(segment), text }
            if *segment == live_segment && text == "Hello"
    )));
    // Losing the connection discards untransmitted live text.
    state
        .normalizer
        .event(stream(&json!({"type":"content_block_delta","index":0,
        "delta":{"type":"text_delta","text":"!"}})));
    assert!(state.normalizer.live_pending());
    state.disconnected();
    assert!(!state.normalizer.live_pending());
}

#[test]
fn configuration_capabilities_and_failures_are_explicit() {
    use bcode_claude_code::Error;
    for settlement in [
        bcode_claude_code::SettlementSupport::Unknown,
        bcode_claude_code::SettlementSupport::VerifiedSessionState,
    ] {
        let value = capabilities::capabilities(settlement);
        assert_eq!(value.model_switch.support, HarnessSupportLevel::Supported);
        assert_eq!(
            value.permission_mode.support,
            HarnessSupportLevel::Supported
        );
        value.validate().unwrap();
    }
    // Admission refusal happens before any native write; everything else is uncertain.
    assert_eq!(
        configure_failure(&Error::InvalidConfiguration("active turn")),
        HarnessFailure::Conflict
    );
    assert_eq!(
        configure_failure(&Error::Capacity),
        HarnessFailure::Unavailable
    );
    for error in [Error::Protocol, Error::Uncertain, Error::Closed] {
        assert_eq!(configure_failure(&error), HarnessFailure::Uncertain);
    }
}

#[test]
fn cumulative_native_cost_becomes_per_result_increments() {
    let mut normalizer = normalizer();
    let mut cost = |total: f64| {
        normalizer
            .event(Event::Message(NativeMessage::Result(json!({
                "result":"ok","total_cost_usd":total,"usage":{"input_tokens":1}
            }))))
            .into_iter()
            .find_map(|observation| match observation.kind {
                HarnessObservationKind::Usage {
                    total_cost_micros, ..
                } => total_cost_micros,
                _ => None,
            })
    };
    // Claude reports a running process total: $0.10, then $0.25, then a background result.
    assert_eq!(cost(0.10), Some(100_000));
    assert_eq!(cost(0.25), Some(150_000));
    assert_eq!(cost(0.25), Some(0));
    // A smaller total means the native total restarted; it is all new cost.
    assert_eq!(cost(0.02), Some(20_000));
}
