use super::*;
use bcode_claude_code::{Event, NativeMessage, Settlement};
use bcode_harness_models::{
    HarnessActivity, HarnessAssistanceGrant, HarnessAuthBinding, HarnessContractError,
    HarnessObservationKind, HarnessResponseValue, HarnessSessionBinding,
};
use serde_json::json;

#[test]
fn eof_after_confirmation_and_interaction_keeps_retained_evidence_readable() {
    let prepared = HarnessPreparedSession {
        open_id: "open-eof".into(),
        session_id: "session".into(),
        instance_id: "instance".into(),
        generation: 7,
        native_version: "tested-version".into(),
        requested_native_session_id: Some("native".into()),
    };
    let prepared_target = HarnessSessionTarget::Prepared(prepared.clone());
    let mut stream = StreamState {
        normalizer: Normalizer::with_target(prepared_target.clone(), prepared.open_id.clone()),
        observations: VecDeque::new(),
        gap: false,
        acknowledged: None,
    };
    let confirmation = stream.normalizer.confirmation("native").unwrap();
    assert!(stream.push(confirmation));
    let interaction = stream
        .normalizer
        .event(callback("Bash", json!({"command":"true"})));
    assert!(stream.push(interaction));
    // The reader's EOF path invalidates live callbacks, not the evidence queue.
    stream.disconnected();
    assert!(stream.normalizer.pending.is_empty());
    let mut observe = HarnessObserveRequest {
        target: prepared_target,
        after: None,
        limit: 8,
        wait_millis: 0,
    };
    let HarnessServiceResponseKind::Observations(page) = stream
        .observation_page(&prepared, &observe, &Settlement::Uncertain)
        .unwrap()
    else {
        panic!("expected retained page")
    };
    assert_eq!(page.observations.len(), 2);
    assert!(matches!(
        &page.observations[0].kind,
        HarnessObservationKind::BindingConfirmed { .. }
    ));
    assert!(matches!(
        &page.observations[1].kind,
        HarnessObservationKind::Interaction { .. }
    ));
    assert_eq!(page.completeness, HarnessCompleteness::Gap);
    assert_eq!(
        page.status.connectivity,
        bcode_harness_models::HarnessConnectivity::Disconnected
    );
    assert_eq!(page.status.activity, HarnessActivity::Unknown);
    let response = HarnessResponse {
        response_id: "late-answer".into(),
        request_id: "callback".into(),
        session_id: "session".into(),
        generation: 7,
        value: HarnessResponseValue::Approval { approved: true },
    };
    assert!(stream.normalizer.response(&response).is_err());
    // The same canonical confirmed target also reads the retained page after EOF.
    observe.target = stream.normalizer.target.clone();
    assert!(
        stream
            .observation_page(&prepared, &observe, &Settlement::Uncertain)
            .is_ok()
    );
    let HarnessSessionTarget::Confirmed(mut stale) = observe.target.clone() else {
        panic!("expected confirmed target")
    };
    stale.generation += 1;
    observe.target = HarnessSessionTarget::Confirmed(stale);
    assert!(matches!(
        stream.observation_page(&prepared, &observe, &Settlement::Uncertain),
        Err(HarnessFailure::StaleGeneration)
    ));
    let mut wrong_prepared = prepared.clone();
    wrong_prepared.open_id = "another-open".into();
    observe.target = HarnessSessionTarget::Prepared(wrong_prepared);
    assert!(matches!(
        stream.observation_page(&prepared, &observe, &Settlement::Uncertain),
        Err(HarnessFailure::StaleGeneration)
    ));
    observe.target = stream.normalizer.target.clone();
    observe.after = page.next_after;
    let HarnessServiceResponseKind::Observations(drained) = stream
        .observation_page(&prepared, &observe, &Settlement::Uncertain)
        .unwrap()
    else {
        panic!("expected acknowledged page")
    };
    assert_eq!(drained.observations, Vec::new());
    assert_eq!(
        drained.status.connectivity,
        bcode_harness_models::HarnessConnectivity::Disconnected
    );
}

fn binding() -> HarnessSessionBinding {
    HarnessSessionBinding {
        schema_version: HARNESS_SCHEMA_VERSION,
        session_id: "session".into(),
        instance_id: "instance".into(),
        native_session_id: "native".into(),
        native_version: "tested-version".into(),
        generation: 7,
    }
}
fn normalizer() -> Normalizer {
    Normalizer::new(binding())
}
fn callback(name: &str, input: serde_json::Value) -> Event {
    Event::Callback {
        request_id: "callback".into(),
        request: bcode_claude_code::CallbackRequest::CanUseTool {
            tool_name: name.into(),
            input,
            tool_use_id: Some("native-tool".into()),
        },
    }
}
#[test]
fn sdk_settlement_observations_match_declared_capability() {
    let verified =
        capabilities::capabilities(bcode_claude_code::SettlementSupport::VerifiedSessionState);
    assert_eq!(
        verified.authoritative_settlement.support,
        bcode_harness_models::HarnessSupportLevel::Supported
    );
    let mut normalizer = normalizer();
    normalizer.turn_id = Some("native-turn".into());
    // Settled is supplied only by the SDK's verified idle/result/task lifecycle.
    let observations = normalizer.event(Event::Settlement(Settlement::Settled));
    assert!(observations.iter().any(|value| matches!(
        &value.kind,
        HarnessObservationKind::Settlement { native_turn_id } if native_turn_id == "native-turn"
    )));

    let unknown = capabilities::capabilities(bcode_claude_code::SettlementSupport::Unknown);
    assert_eq!(
        unknown.authoritative_settlement.support,
        bcode_harness_models::HarnessSupportLevel::Unsupported
    );
    // Unknown executable support (or an unknown native task class) yields the
    // SDK's Unsupported snapshot, not a terminal observation, even with a turn ID.
    let observations = normalizer.event(Event::Settlement(Settlement::Unsupported));
    assert!(
        !observations
            .iter()
            .any(|value| matches!(value.kind, HarnessObservationKind::Settlement { .. }))
    );
    assert!(observations.iter().any(|value| matches!(
        &value.kind,
        HarnessObservationKind::Status { status } if status.activity == HarnessActivity::Unknown
    )));
}

#[test]
fn result_does_not_settle_background_work() {
    let mut normalizer = normalizer();
    let observations = normalizer.event(Event::Message(NativeMessage::Result(
        json!({"result":"done"}),
    )));
    assert!(observations.iter().any(
        |value| matches!(&value.kind, HarnessObservationKind::Result { text } if text == "done")
    ));
    assert!(
        !observations
            .iter()
            .any(|value| matches!(value.kind, HarnessObservationKind::Settlement { .. }))
    );
    assert_eq!(
        normalize::status(&Settlement::Running).activity,
        HarnessActivity::Running
    );
}
#[test]
fn observed_tools_are_data_not_dispatch_and_arguments_stay_private() {
    let mut normalizer = normalizer();
    let observations = normalizer.event(Event::Message(NativeMessage::Assistant(json!({"message":{"content":[{"type":"tool_use","id":"native-tool","name":"Bash","input":{"secret":"DO_NOT_PERSIST"}}]}}))));
    assert!(
        matches!(&observations[0].kind, HarnessObservationKind::Tool { native_tool_id, .. } if native_tool_id == "native-tool")
    );
    assert!(
        !serde_json::to_string(&observations)
            .unwrap()
            .contains("DO_NOT_PERSIST")
    );
}
#[test]
fn stale_generation_never_gets_a_callback_response() {
    let mut normalizer = normalizer();
    normalizer.event(callback("Bash", json!({"command":"true"})));
    let response = HarnessResponse {
        response_id: "response".into(),
        request_id: "callback".into(),
        session_id: "session".into(),
        generation: 6,
        value: HarnessResponseValue::Approval { approved: true },
    };
    assert!(matches!(
        normalizer.response(&response),
        Err(HarnessContractError::StaleGeneration)
    ));
}
#[test]
fn cancellation_invalidates_pending_callback() {
    let mut normalizer = normalizer();
    normalizer.event(callback("Bash", json!({})));
    normalizer.event(Event::CallbackCancelled {
        request_id: "callback".into(),
    });
    let response = HarnessResponse {
        response_id: "response".into(),
        request_id: "callback".into(),
        session_id: "session".into(),
        generation: 7,
        value: HarnessResponseValue::Approval { approved: true },
    };
    assert!(normalizer.response(&response).is_err());
}
#[test]
fn question_returns_updated_native_input_without_bcode_execution() {
    let mut normalizer = normalizer();
    let observations = normalizer.event(callback(
        "AskUserQuestion",
        json!({"questions":[{"question":"Which?","options":[{"label":"One"},{"label":"Two"}]}]}),
    ));
    assert!(
        matches!(&observations[0].kind, HarnessObservationKind::Interaction { interaction } if interaction.kind == HarnessInteractionKind::Question)
    );
    let response = HarnessResponse {
        response_id: "response".into(),
        request_id: "callback".into(),
        session_id: "session".into(),
        generation: 7,
        value: HarnessResponseValue::Answer {
            choice_id: Some("1".into()),
            text: String::new(),
        },
    };
    let bcode_claude_code::CallbackResponse::Allow { updated_input } =
        normalizer.response(&response).unwrap()
    else {
        panic!("expected native input update")
    };
    assert_eq!(updated_input["answers"]["Which?"], "Two");
}
#[test]
fn multi_question_forms_fail_closed_instead_of_dropping_questions() {
    let mut normalizer = normalizer();
    let observations = normalizer.event(callback("AskUserQuestion", json!({"questions":[{},{}]})));
    assert!(normalizer.pending.is_empty());
    assert!(matches!(
        observations[0].kind,
        HarnessObservationKind::Diagnostic { .. }
    ));
}
#[test]
fn arbitrary_mcp_requests_do_not_acquire_parent_assistance() {
    let mut normalizer = normalizer();
    let observations = normalizer.event(Event::Callback { request_id: "callback".into(), request: bcode_claude_code::CallbackRequest::McpMessage { server_name: "untrusted".into(), message: json!({"method":"tools/call","params":{"name":"request_parent_assistance","arguments":{"question":"help"}}}) } });
    assert!(normalizer.pending.is_empty());
    assert!(matches!(
        observations[0].kind,
        HarnessObservationKind::Diagnostic { .. }
    ));
}
#[test]
fn named_assistance_callback_without_host_scope_fails_closed() {
    let mut normalizer = normalizer();
    let observations = normalizer.event(Event::Callback { request_id: "callback".into(), request: bcode_claude_code::CallbackRequest::McpMessage { server_name: "bcode-parent".into(), message: json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"request_parent_assistance","arguments":{"question":"help"}}}) } });
    assert!(normalizer.pending.is_empty());
    assert!(matches!(
        &observations[0].kind,
        HarnessObservationKind::Diagnostic { .. }
    ));
}
fn assistance_grant(scope: &str) -> HarnessAssistanceGrant {
    HarnessAssistanceGrant {
        admission_id: "grant-1".into(),
        child_session_id: "session".into(),
        child_generation: 7,
        parent_session_id: "parent".into(),
        parent_generation: 12,
        scope: scope.into(),
    }
}
fn admitted_assistance_event() -> Event {
    Event::Callback {
        request_id: "parent-call".into(),
        request: bcode_claude_code::CallbackRequest::McpMessage {
            server_name: "bcode-parent".into(),
            message: json!({"jsonrpc":"2.0","id":42,"method":"tools/call","params":{"name":"request_parent_assistance","arguments":{"question":"Which API should I use?"}}}),
        },
    }
}
#[test]
fn admitted_parent_call_is_pending_while_other_observations_continue() {
    let mut normalizer = normalizer();
    normalizer.assistance_grant = Some(assistance_grant("grant-1: parent text advice only"));
    let observations = normalizer.event(admitted_assistance_event());
    assert!(
        matches!(&observations[0].kind, HarnessObservationKind::Interaction { interaction }
        if interaction.kind == HarnessInteractionKind::Assistance && interaction.generation == 7)
    );
    let text = normalizer.event(Event::Message(NativeMessage::Assistant(
        json!({"message":{"content":[{"type":"text","text":"background progress"}]}}),
    )));
    assert!(matches!(&text[0].kind, HarnessObservationKind::Text { .. }));
    assert!(normalizer.pending.contains_key("parent-call"));
    let mut response = HarnessResponse {
        response_id: "answer-1".into(),
        request_id: "parent-call".into(),
        session_id: "session".into(),
        generation: 6,
        value: HarnessResponseValue::Assistance {
            text: "Use the typed API".into(),
        },
    };
    assert!(matches!(
        normalizer.response(&response),
        Err(HarnessContractError::StaleGeneration)
    ));
    response.generation = 7;
    let bcode_claude_code::CallbackResponse::Mcp { response: native } =
        normalizer.response(&response).unwrap()
    else {
        panic!("expected native MCP answer")
    };
    assert_eq!(native["id"], 42);
    assert_eq!(native["result"]["content"][0]["text"], "Use the typed API");
    normalizer.assistance_grant = None;
    assert!(normalizer.response(&response).is_err());
}
#[test]
fn assistance_answer_and_cancel_require_exact_immutable_parent_grant() {
    let mut normalizer = normalizer();
    let grant = assistance_grant("same scope text");
    normalizer.assistance_grant = Some(grant.clone());
    normalizer.event(admitted_assistance_event());
    let mut response = HarnessResponse {
        response_id: "response".into(),
        request_id: "parent-call".into(),
        session_id: "session".into(),
        generation: 7,
        value: HarnessResponseValue::Assistance {
            text: "answer".into(),
        },
    };
    assert!(normalizer.response(&response).is_ok());
    for changed in [
        HarnessAssistanceGrant {
            admission_id: "another-admission".into(),
            ..grant.clone()
        },
        HarnessAssistanceGrant {
            parent_generation: 13,
            ..grant.clone()
        },
        HarnessAssistanceGrant {
            parent_session_id: "other-parent".into(),
            ..grant.clone()
        },
    ] {
        normalizer.assistance_grant = Some(changed);
        assert!(normalizer.response(&response).is_err());
        response.value = HarnessResponseValue::Cancel;
        assert!(normalizer.response(&response).is_err());
        response.value = HarnessResponseValue::Assistance {
            text: "answer".into(),
        };
    }
    normalizer.assistance_grant = Some(grant);
    response.value = HarnessResponseValue::Cancel;
    let bcode_claude_code::CallbackResponse::Mcp { response: native } =
        normalizer.response(&response).unwrap()
    else {
        panic!("expected MCP cancellation")
    };
    assert_eq!(native["id"], 42);
    assert!(native.get("error").is_some());
    normalizer.invalidate_callbacks();
    assert!(normalizer.response(&response).is_err());
}
#[test]
fn assistance_grant_cannot_authorize_another_child_generation() {
    let mut normalizer = normalizer();
    normalizer.assistance_grant = Some(HarnessAssistanceGrant {
        child_generation: 8,
        ..assistance_grant("scope")
    });
    normalizer.event(admitted_assistance_event());
    assert!(normalizer.pending.is_empty());
}
#[test]
fn native_cancellation_invalidates_assistance_without_stopping_observations() {
    let mut normalizer = normalizer();
    normalizer.assistance_grant = Some(assistance_grant("host-admitted text only"));
    normalizer.event(admitted_assistance_event());
    normalizer.event(Event::CallbackCancelled {
        request_id: "parent-call".into(),
    });
    let response = HarnessResponse {
        response_id: "cancelled-answer".into(),
        request_id: "parent-call".into(),
        session_id: "session".into(),
        generation: 7,
        value: HarnessResponseValue::Assistance {
            text: "too late".into(),
        },
    };
    assert!(normalizer.response(&response).is_err());
    assert_ne!(
        normalizer.event(Event::Message(NativeMessage::Result(
            json!({"result":"native result"})
        ))),
        Vec::new()
    );
}
#[test]
fn bounded_queue_never_silently_evicts_unacknowledged_history() {
    let mut stream = StreamState {
        normalizer: normalizer(),
        observations: VecDeque::new(),
        gap: false,
        acknowledged: None,
    };
    for _ in 0..MAX_RETAINED_OBSERVATIONS {
        let observations = stream
            .normalizer
            .event(Event::Message(NativeMessage::Result(
                json!({"result":"text"}),
            )));
        assert!(stream.push(observations));
    }
    let original = stream.observations.front().unwrap().observation_id.clone();
    let observations = stream
        .normalizer
        .event(Event::Message(NativeMessage::Result(
            json!({"result":"overflow"}),
        )));
    assert!(!stream.push(observations));
    assert!(stream.gap);
    assert_eq!(stream.observations.len(), MAX_RETAINED_OBSERVATIONS);
    assert_eq!(
        stream.observations.front().unwrap().observation_id,
        original
    );
}
#[test]
fn prepared_open_never_invents_confirmation_and_first_init_promotes_once() {
    let prepared = HarnessPreparedSession {
        open_id: "open".into(),
        session_id: "session".into(),
        instance_id: "instance".into(),
        generation: 7,
        native_version: "tested-version".into(),
        requested_native_session_id: Some("requested-native".into()),
    };
    let target = HarnessSessionTarget::Prepared(prepared.clone());
    assert!(
        matches!(open_response(&target), HarnessServiceResponseKind::Prepared(value) if value == prepared)
    );
    let mut normalizer = Normalizer::with_target(target, "open".into());
    assert!(normalizer.confirmation("different-native").is_err());
    assert!(matches!(
        normalizer.target,
        HarnessSessionTarget::Prepared(_)
    ));
    let observations = normalizer.confirmation("requested-native").unwrap();
    assert_eq!(observations.len(), 1);
    assert!(
        matches!(&observations[0].kind, HarnessObservationKind::BindingConfirmed { open_id, binding }
        if open_id == "open" && binding.native_session_id == "requested-native" && binding.generation == 7)
    );
    assert!(matches!(
        open_response(&normalizer.target),
        HarnessServiceResponseKind::Opened(_)
    ));
    assert_eq!(
        normalizer.confirmation("requested-native").unwrap(),
        Vec::new()
    );
    assert!(normalizer.confirmation("changed-native").is_err());
}
#[test]
fn prepared_target_supports_scoped_interaction_without_sentinel_binding() {
    let prepared = HarnessPreparedSession {
        open_id: "open".into(),
        session_id: "session".into(),
        instance_id: "instance".into(),
        generation: 7,
        native_version: "tested-version".into(),
        requested_native_session_id: Some("requested-native".into()),
    };
    let mut normalizer =
        Normalizer::with_target(HarnessSessionTarget::Prepared(prepared), "open".into());
    normalizer.assistance_grant = Some(assistance_grant("host text-only grant"));
    let observations = normalizer.event(admitted_assistance_event());
    assert!(
        matches!(&observations[0].kind, HarnessObservationKind::Interaction { interaction }
        if interaction.kind == HarnessInteractionKind::Assistance && interaction.session_id == "session")
    );
    assert!(matches!(
        normalizer.target,
        HarnessSessionTarget::Prepared(_)
    ));
}
#[test]
fn raw_ids_results_and_settlement_do_not_establish_submission_acceptance() {
    let mut normalizer = normalizer();
    let mut observations = normalizer.event(Event::Message(NativeMessage::Assistant(json!({
        "uuid": "message-uuid", "turn_id": "unverified-turn", "request_id": "request",
        "message": {"id": "message-id", "content": [{"type": "text", "text": "hello"}]}
    }))));
    observations.extend(normalizer.event(Event::Message(NativeMessage::User(json!({
        "uuid": "echo-id", "turn_id": "unverified-turn"
    })))));
    observations.extend(
        normalizer.event(Event::Message(NativeMessage::Result(json!({
            "uuid": "result-id", "turn_id": "unverified-turn", "result": "done"
        })))),
    );
    observations.extend(normalizer.event(Event::Settlement(Settlement::Settled)));
    assert!(normalizer.turn_id.is_none());
    assert!(observations.iter().any(|value| matches!(&value.kind,
        HarnessObservationKind::Result { text } if text == "done")));
    assert!(!observations.iter().any(|value| matches!(
        &value.kind,
        HarnessObservationKind::SubmissionAccepted { .. }
            | HarnessObservationKind::Settlement { .. }
    )));
    assert!(observations.iter().any(|value| matches!(&value.kind,
        HarnessObservationKind::Diagnostic { code, .. } if code == "settlement_turn_unavailable")));
}

#[test]
fn native_settlement_does_not_invent_a_turn_id() {
    let mut normalizer = normalizer();
    let observations = normalizer.event(Event::Settlement(Settlement::Settled));
    assert!(
        !observations
            .iter()
            .any(|value| matches!(&value.kind, HarnessObservationKind::Settlement { .. }))
    );
}
#[test]
fn oversized_or_malformed_requests_do_not_start_a_runtime() {
    for payload in [vec![b'x'; MAX_HARNESS_WIRE_BYTES + 1], b"not-json".to_vec()] {
        let mut plugin = ClaudeCodePlugin::default();
        let context = NativeServiceContext {
            plugin_id: PLUGIN_ID.into(),
            request: bcode_plugin_sdk::ServiceRequest {
                interface_id: HARNESS_INTERFACE_ID.into(),
                operation: "open".into(),
                payload,
            },
            config: bcode_plugin_sdk::PluginConfigContext::default(),
            events: bcode_plugin_sdk::ServiceEventEmitter::default(),
            cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
            bridge: bcode_plugin_sdk::ServiceBridge::default(),
            transient_progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
            diagnostics: None,
        };
        assert!(plugin.invoke_service(context).error.is_some());
        assert!(plugin.runtime.is_none());
        assert!(plugin.processes.is_empty());
    }
}
#[test]
fn handoff_is_explicit_namespace_specific_and_nonexecuting() {
    use bcode_claude_code::{
        BillingRoute, LaunchOptions, Limits, NativeAuthSupport, NativeEnvironment,
        SettingsInheritance, SettlementSupport,
    };
    let instance = HarnessInstance {
        instance_id: "instance".into(),
        plugin_id: PLUGIN_ID.into(),
        executable: "/explicit/claude".into(),
        config_namespace: "/isolated/claude".into(),
        auth: HarnessAuthBinding {
            provider_id: "claude-code".into(),
            profile: "native".into(),
            credential_namespace: "/isolated/claude".into(),
            custody: HarnessAuthCustody::NativeManaged,
        },
    };
    let mut options = LaunchOptions {
        executable: "/explicit/claude".into(),
        expected_version: "tested".into(),
        cwd: "/workspace".into(),
        config_directory: "/isolated/claude".into(),
        environment: NativeEnvironment::default(),
        billing_route: BillingRoute::NativeAccount,
        settings: SettingsInheritance::Native,
        setting_sources: Vec::new(),
        resume_session_id: None,
        settlement_support: SettlementSupport::Unknown,
        auth_support: NativeAuthSupport::VerifiedStatusAndLogin,
        limits: Limits::default(),
    };
    let handoff = native_login_handoff_data(&instance, &options).unwrap();
    assert_eq!(handoff.arguments, ["auth", "login"]);
    assert_eq!(handoff.method.method_id, "native-login");
    assert_eq!(handoff.method.support, HarnessSupportLevel::Supported);
    assert_eq!(
        handoff.method.custody,
        bcode_provider_auth_models::managed::AuthCustodyMode::NativeManaged
    );
    assert_eq!(
        handoff.method.execution,
        NativeLoginExecution::ClientTerminal
    );
    assert_eq!(
        handoff.method.native_inactivity,
        NativeLoginInactivity::Unknown
    );
    let decoded: NativeLoginHandoffData =
        serde_json::from_value(serde_json::to_value(&handoff).unwrap()).unwrap();
    assert_eq!(decoded, handoff);
    assert_eq!(handoff.environment["CLAUDE_CONFIG_DIR"], "/isolated/claude");
    assert_eq!(handoff.namespace.credential_namespace, "/isolated/claude");
    assert_eq!(handoff.executable, options.executable);
    options
        .environment
        .0
        .insert("ANTHROPIC_API_KEY".into(), "must-not-extract".into());
    assert!(native_login_handoff_data(&instance, &options).is_err());
    options.environment.0.clear();
    options.config_directory = "/different/claude".into();
    assert!(native_login_handoff_data(&instance, &options).is_err());
}
#[test]
fn unrelated_native_subagents_are_not_bcode_children() {
    let mut normalizer = normalizer();
    let observations = normalizer.event(Event::Message(NativeMessage::System(
        json!({"subtype":"task_started","task_id":"native-child"}),
    )));
    assert!(
        matches!(&observations[0].kind, HarnessObservationKind::Subagent { native_agent_id, .. } if native_agent_id == "native-child")
    );
}
