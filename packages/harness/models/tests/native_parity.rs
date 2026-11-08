//! Behavioral tests for the managed native-parity contract additions.
use bcode_harness_models::*;

fn binding() -> HarnessSessionBinding {
    HarnessSessionBinding {
        schema_version: 1,
        session_id: "session".into(),
        instance_id: "instance".into(),
        native_session_id: "native".into(),
        native_version: "1.0.0".into(),
        generation: 7,
    }
}

fn request(request_id: &str, kind: HarnessRequestKind) -> HarnessRequest {
    HarnessRequest {
        schema_version: 1,
        request_id: request_id.into(),
        instance: HarnessInstance {
            instance_id: "instance".into(),
            plugin_id: "plugin".into(),
            executable: "/bin/native".into(),
            config_namespace: "/config/native".into(),
            auth: HarnessAuthBinding {
                provider_id: "provider".into(),
                profile: "profile".into(),
                credential_namespace: "credentials".into(),
                custody: HarnessAuthCustody::NativeManaged,
            },
        },
        kind,
    }
}

fn observation(id: &str, kind: HarnessObservationKind) -> HarnessObservation {
    HarnessObservation {
        observation_id: id.into(),
        session_id: "session".into(),
        generation: 7,
        native_correlations: vec![],
        completeness: HarnessCompleteness::Complete,
        kind,
    }
}

fn status() -> HarnessExecutionStatus {
    HarnessExecutionStatus {
        connectivity: HarnessConnectivity::Connected,
        activity: HarnessActivity::Running,
        attention: HarnessAttention::None,
        continuation: HarnessContinuation::Unknown,
        completeness: HarnessCompleteness::Partial,
    }
}

#[test]
fn new_observation_variants_are_bounded() {
    let valid = [
        HarnessObservationKind::Reasoning {
            segment_id: Some("segment".into()),
            text: "thinking".into(),
            opaque: false,
        },
        HarnessObservationKind::ToolResult {
            native_tool_id: "tool".into(),
            output: "done".into(),
            is_error: false,
            truncated: false,
        },
        HarnessObservationKind::Usage {
            input_tokens: 1,
            output_tokens: 2,
            cache_read_input_tokens: 3,
            cache_creation_input_tokens: 4,
            total_cost_micros: Some(5),
            duration_ms: Some(6),
            model: Some("model".into()),
        },
        HarnessObservationKind::Text {
            text: "hello".into(),
            segment_id: Some("segment".into()),
        },
        HarnessObservationKind::Tool {
            native_tool_id: "tool".into(),
            name: "Read".into(),
            status: "observed".into(),
            input_json: Some(r#"{"path":"a"}"#.into()),
        },
    ];
    for kind in valid {
        assert!(observation("o", kind).validate().is_ok());
    }
    let invalid = [
        HarnessObservationKind::Tool {
            native_tool_id: "tool".into(),
            name: "Read".into(),
            status: "observed".into(),
            input_json: Some("not json".into()),
        },
        HarnessObservationKind::Tool {
            native_tool_id: "tool".into(),
            name: "Read".into(),
            status: "observed".into(),
            input_json: Some(format!("\"{}\"", "x".repeat(MAX_HARNESS_TOOL_INPUT_BYTES))),
        },
        HarnessObservationKind::ToolResult {
            native_tool_id: String::new(),
            output: String::new(),
            is_error: true,
            truncated: false,
        },
        HarnessObservationKind::ToolResult {
            native_tool_id: "tool".into(),
            output: "x".repeat(MAX_HARNESS_TEXT_BYTES + 1),
            is_error: false,
            truncated: true,
        },
        HarnessObservationKind::Reasoning {
            segment_id: Some(" padded".into()),
            text: String::new(),
            opaque: true,
        },
        HarnessObservationKind::Usage {
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            total_cost_micros: None,
            duration_ms: None,
            model: Some(String::new()),
        },
    ];
    for kind in invalid {
        assert!(observation("o", kind).validate().is_err());
    }
}

#[test]
fn optional_display_fields_preserve_existing_wire_shape() {
    let text = observation(
        "o",
        HarnessObservationKind::Text {
            text: "hello".into(),
            segment_id: None,
        },
    );
    let value = serde_json::to_value(&text).unwrap();
    assert_eq!(
        value["kind"],
        serde_json::json!({"kind": "text", "text": "hello"})
    );
    let decoded: HarnessObservation = serde_json::from_value(value).unwrap();
    assert_eq!(decoded, text);
    let interaction: HarnessInteraction = serde_json::from_value(serde_json::json!({
        "request_id": "r", "session_id": "session", "generation": 7, "kind": "approval",
        "prompt": "p", "scope": "s", "choices": []
    }))
    .unwrap();
    assert_eq!(interaction.tool_name, None);
    let mut interaction = interaction;
    interaction.input_preview = Some("x".repeat(MAX_HARNESS_PREVIEW_BYTES + 1));
    assert!(interaction.validate().is_err());
}

#[test]
fn capabilities_default_new_controls_to_unsupported_and_round_trip() {
    let capability = |support| HarnessCapability {
        support,
        constraints: vec![],
    };
    let mut value = serde_json::json!({});
    for name in [
        "resume",
        "steering",
        "approval",
        "questions",
        "assistance",
        "interrupt",
        "authoritative_settlement",
        "background_continuation",
        "authentication",
    ] {
        value[name] = serde_json::to_value(capability(HarnessSupportLevel::Supported)).unwrap();
    }
    let decoded: HarnessCapabilities = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(decoded.model_switch, HarnessCapability::unsupported());
    assert_eq!(decoded.permission_mode, HarnessCapability::unsupported());
    assert_eq!(serde_json::to_value(&decoded).unwrap(), value);
    let mut supported = decoded;
    supported.model_switch = capability(HarnessSupportLevel::Supported);
    let encoded = serde_json::to_value(&supported).unwrap();
    assert!(encoded.get("model_switch").is_some());
    assert!(encoded.get("permission_mode").is_none());
}

#[test]
fn configure_is_a_retained_mutation_answered_by_acknowledgement() {
    let configure = |mutation_id: &str, setting| {
        request(
            mutation_id,
            HarnessRequestKind::Configure(HarnessConfigureRequest {
                target: HarnessSessionTarget::Confirmed(binding()),
                mutation_id: mutation_id.into(),
                setting,
            }),
        )
    };
    let model = configure(
        "configure-1",
        HarnessSessionSetting::Model {
            model_id: "model".into(),
        },
    );
    assert!(model.validate().is_ok());
    assert_eq!(model.kind.operation(), HarnessOperation::Configure);
    assert_eq!(HarnessOperation::Configure.as_str(), "configure");
    let encoded = serde_json::to_value(&model.kind).unwrap();
    assert_eq!(encoded["operation"], "configure");
    assert_eq!(encoded["request"]["setting"]["kind"], "model");
    let plan = configure(
        "configure-2",
        HarnessSessionSetting::PermissionMode {
            mode: HarnessPermissionMode::Plan,
        },
    );
    assert!(plan.validate().is_ok());
    assert!(
        configure(
            "configure-3",
            HarnessSessionSetting::Model {
                model_id: String::new()
            }
        )
        .validate()
        .is_err()
    );
    let response = |mutation_id: &str| HarnessServiceResponse {
        schema_version: 1,
        request_id: "configure-1".into(),
        operation: HarnessOperation::Configure,
        kind: HarnessServiceResponseKind::Acknowledged {
            mutation_id: mutation_id.into(),
        },
    };
    assert!(response("configure-1").validate_for(&model).is_ok());
    assert_eq!(
        response("other").validate_for(&model),
        Err(HarnessContractError::WrongOwner)
    );
    let unsupported = HarnessServiceResponse {
        kind: HarnessServiceResponseKind::Failure(HarnessFailure::Unsupported),
        ..response("configure-1")
    };
    assert!(unsupported.validate_for(&model).is_ok());
}

#[test]
fn live_sidecar_is_bounded_and_excluded_from_the_retention_digest() {
    let observations = vec![observation(
        "o",
        HarnessObservationKind::Text {
            text: "final".into(),
            segment_id: Some("segment".into()),
        },
    )];
    let digest = harness_observations_digest(&observations).unwrap();
    let mut page = HarnessObservationPage {
        retention: Some(HarnessRetentionPage {
            connection_id: "connection".into(),
            page_id: "page".into(),
            observations_digest: digest.clone(),
        }),
        hydration_requests: Vec::new(),
        inventory_page: None,
        observations,
        next_after: None,
        completeness: HarnessCompleteness::Partial,
        status: status(),
        live: vec![HarnessLiveUpdate {
            stream: HarnessLiveStream::Text,
            segment_id: "segment".into(),
            offset: 0,
            text: "fin".into(),
        }],
    };
    assert!(page.validate_retention().is_ok());
    assert_eq!(
        harness_observations_digest(&page.observations).unwrap(),
        digest
    );
    page.live = vec![page.live[0].clone(); MAX_HARNESS_LIVE_UPDATES + 1];
    assert_eq!(
        page.validate_retention(),
        Err(HarnessContractError::BoundsExceeded)
    );
    page.live.clear();
    let encoded = serde_json::to_value(&page).unwrap();
    assert!(encoded.get("live").is_none());
}

#[test]
fn stream_identity_maps_durable_and_live_items_to_one_key() {
    let durable = harness_stream_identity(7, "segment");
    let live = harness_stream_identity(7, "segment");
    assert_eq!(durable.assistant_item_id(), live.assistant_item_id());
    assert_ne!(
        durable.assistant_item_id(),
        harness_stream_identity(8, "segment").assistant_item_id()
    );
}
