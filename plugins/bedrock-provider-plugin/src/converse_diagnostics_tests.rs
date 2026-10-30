//! Behavioral fixtures for provider-owned DTOs, not raw HTTP-equivalence claims.
use super::*;
use serde_json::json;

#[test]
fn final_builder_dto_contains_adapted_messages_tools_cache_reasoning_and_output_config() {
    let request: super::super::ModelTurnRequest = serde_json::from_value(json!({
        "session_id": "00000000-0000-0000-0000-000000000000",
        "turn_id": "fixture", "model_id": "fixture-model",
        "system_prompt": "system fixture",
        "messages": [{"role":"user", "content":[{"type":"text", "text":"user fixture"}]}]
    }))
    .unwrap();
    let adapted =
        super::super::build_converse_request(&request, "resolved-model".into(), None).unwrap();
    let input = ConverseStreamInputBuilder::default()
        .model_id(adapted.model_id)
        .set_messages(Some(adapted.messages))
        .set_system(Some(adapted.system))
        .set_tool_config(adapted.tool_config)
        .set_inference_config(adapted.inference_config)
        .set_output_config(adapted.output_config)
        .set_additional_model_request_fields(adapted.additional_model_request_fields);
    let value = serde_json::to_value(Wire(&input)).unwrap();
    assert_eq!(value["dto_format"], "bcode.bedrock.converse.request");
    assert_eq!(value["dto_version"], 1);
    assert_eq!(value["model_id"], "resolved-model");
    assert_eq!(value["body"]["messages"][0]["role"], "user");
    assert_eq!(
        value["body"]["messages"][0]["content"][0]["text"],
        "user fixture"
    );
    assert_eq!(value["body"]["system"][0]["text"], "system fixture");
    assert!(value["body"].get("modelId").is_none());

    let tool = ToolSpecification::builder()
        .name("fixture_tool")
        .input_schema(ToolInputSchema::Json(Document::Object(HashMap::new())))
        .strict(true)
        .build()
        .unwrap();
    let input = input
        .tool_config(
            ToolConfiguration::builder()
                .tools(Tool::ToolSpec(tool))
                .tool_choice(ToolChoice::Any(AnyToolChoice::builder().build()))
                .build()
                .unwrap(),
        )
        .inference_config(
            InferenceConfiguration::builder()
                .max_tokens(17)
                .temperature(0.25)
                .build(),
        )
        .output_config(OutputConfig::builder().effort("high").build())
        .system(SystemContentBlock::CachePoint(
            CachePointBlock::builder()
                .r#type(CachePointType::Default)
                .build()
                .unwrap(),
        ))
        .additional_model_request_fields(Document::Object(HashMap::from([(
            "thinking".into(),
            Document::Object(HashMap::from([
                ("type".into(), Document::String("enabled".into())),
                (
                    "budget_tokens".into(),
                    Document::Number(Number::PosInt(1024)),
                ),
            ])),
        )])));
    let value = serde_json::to_value(Wire(&input)).unwrap();
    assert_eq!(
        value["body"]["toolConfig"]["tools"][0]["toolSpec"]["name"],
        "fixture_tool"
    );
    assert_eq!(
        value["body"]["toolConfig"]["tools"][0]["toolSpec"]["strict"],
        true
    );
    assert_eq!(value["body"]["toolConfig"]["toolChoice"], json!({"any":{}}));
    assert_eq!(value["body"]["inferenceConfig"]["maxTokens"], 17);
    assert_eq!(value["body"]["outputConfig"]["effort"], "high");
    assert_eq!(value["body"]["system"][1]["cachePoint"]["type"], "default");
    assert_eq!(
        value["body"]["additionalModelRequestFields"]["thinking"]["budget_tokens"],
        1024
    );
}

#[test]
fn binary_and_tool_content_use_provider_wire_shapes_not_debug_output() {
    let image = ImageBlock::builder()
        .format(ImageFormat::Png)
        .source(ImageSource::Bytes(Blob::new([0_u8, 1, 2])))
        .build()
        .unwrap();
    assert_eq!(
        serde_json::to_value(Wire(&image)).unwrap(),
        json!({"format":"png", "source":{"bytes":"AAEC"}})
    );
    let reasoning = ReasoningContentBlock::ReasoningText(
        ReasoningTextBlock::builder()
            .text("private reasoning")
            .signature("opaque signature")
            .build()
            .unwrap(),
    );
    assert_eq!(
        serde_json::to_value(Wire(&reasoning)).unwrap(),
        json!({"reasoningText":{"text":"private reasoning", "signature":"opaque signature"}})
    );
    let result = ToolResultBlock::builder()
        .tool_use_id("call-1")
        .content(ToolResultContentBlock::Json(Document::Object(
            HashMap::from([("answer".into(), Document::Number(Number::PosInt(42)))]),
        )))
        .status(ToolResultStatus::Success)
        .build()
        .unwrap();
    assert_eq!(
        serde_json::to_value(Wire(&result)).unwrap(),
        json!({"toolUseId":"call-1", "content":[{"json":{"answer":42}}], "status":"success"})
    );
}

#[test]
fn all_received_stream_event_kinds_preserve_provider_members() {
    let events = [
        ConverseStreamOutput::MessageStart(
            MessageStartEvent::builder()
                .role(ConversationRole::Assistant)
                .build()
                .unwrap(),
        ),
        ConverseStreamOutput::ContentBlockStart(
            ContentBlockStartEvent::builder()
                .content_block_index(2)
                .start(ContentBlockStart::ToolUse(
                    ToolUseBlockStart::builder()
                        .tool_use_id("call-1")
                        .name("fixture_tool")
                        .build()
                        .unwrap(),
                ))
                .build()
                .unwrap(),
        ),
        ConverseStreamOutput::ContentBlockDelta(
            ContentBlockDeltaEvent::builder()
                .content_block_index(2)
                .delta(ContentBlockDelta::ToolUse(
                    ToolUseBlockDelta::builder()
                        .input("{\"answer\":42}")
                        .build()
                        .unwrap(),
                ))
                .build()
                .unwrap(),
        ),
        ConverseStreamOutput::ContentBlockStop(
            ContentBlockStopEvent::builder()
                .content_block_index(2)
                .build()
                .unwrap(),
        ),
        ConverseStreamOutput::MessageStop(
            MessageStopEvent::builder()
                .stop_reason(StopReason::ToolUse)
                .build()
                .unwrap(),
        ),
        ConverseStreamOutput::Metadata(
            ConverseStreamMetadataEvent::builder()
                .usage(
                    TokenUsage::builder()
                        .input_tokens(17)
                        .output_tokens(3)
                        .total_tokens(20)
                        .cache_read_input_tokens(10)
                        .build()
                        .unwrap(),
                )
                .metrics(
                    ConverseStreamMetrics::builder()
                        .latency_ms(123)
                        .build()
                        .unwrap(),
                )
                .build(),
        ),
    ];
    let values = events
        .iter()
        .map(|event| serde_json::to_value(StreamRecord(event)).unwrap())
        .collect::<Vec<_>>();
    for value in &values {
        assert_eq!(value["dto_format"], "bcode.bedrock.converse.stream");
        assert_eq!(value["dto_version"], 1);
    }
    assert_eq!(values[0]["event"]["messageStart"]["role"], "assistant");
    assert_eq!(
        values[1]["event"]["contentBlockStart"]["start"]["toolUse"]["toolUseId"],
        "call-1"
    );
    assert_eq!(
        values[2]["event"]["contentBlockDelta"]["delta"]["toolUse"]["input"],
        "{\"answer\":42}"
    );
    assert_eq!(
        values[3]["event"]["contentBlockStop"]["contentBlockIndex"],
        2
    );
    assert_eq!(values[4]["event"]["messageStop"]["stopReason"], "tool_use");
    assert_eq!(
        values[5]["event"]["metadata"]["usage"]["cacheReadInputTokens"],
        10
    );
    assert_eq!(values[5]["event"]["metadata"]["metrics"]["latencyMs"], 123);
}

#[test]
fn oversized_binary_is_not_allocated_as_an_unbounded_base64_string() {
    let blob = Blob::new(vec![0_u8; super::super::diagnostics::MAX_PAYLOAD_BYTES + 1]);
    assert!(serde_json::to_value(Wire(&blob)).is_err());
}
