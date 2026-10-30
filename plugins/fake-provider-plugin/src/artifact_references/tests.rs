use super::*;
use bcode_model::{ImageMetadata, ToolResult};
use serde_json::{Value, json};

fn request() -> ModelTurnRequest {
    serde_json::from_value(json!({
        "session_id":"00000000-0000-0000-0000-000000000000",
        "turn_id":"references", "model_id":MODEL,
        "messages":[{"role":"user","content":[{"type":"text","text":
            "tool-call filesystem.read {\"path\":\"large.png\"}\ntool-call shell.run {\"command\":\"printf done\"}"}]}],
        "provider_context":{"settings":{"fake_prompt_tool_directives":"true"}}
    })).unwrap()
}

fn image() -> ImageRefContent {
    ImageRefContent {
        path: "/nonexistent/test-owned/image.png".into(),
        mime_type: "image/png".into(),
        artifact_id: Some("artifact-1".into()),
        reference_key: Some("inline-image".into()),
        metadata: ImageMetadata {
            width: Some(16),
            height: Some(16),
            byte_len: Some(200_100),
            source_path: Some("/nonexistent/test-owned/image.png".into()),
        },
    }
}

fn result(
    call_id: &str,
    content: Vec<ToolResultContent>,
    is_error: bool,
) -> bcode_model::ModelMessage {
    bcode_model::ModelMessage {
        role: MessageRole::Tool,
        content: vec![ContentBlock::ToolResult {
            result: ToolResult {
                call_id: call_id.into(),
                output: "opaque tool output".into(),
                is_error,
                content,
            },
        }],
    }
}

fn with_reference() -> ModelTurnRequest {
    let mut request = request();
    request.messages.push(result(
        "read-1",
        vec![ToolResultContent::ImageRef { image: image() }],
        false,
    ));
    request
}

#[test]
fn listed_and_selected_capabilities_are_reference_only() {
    let service = |operation: &str| bcode_plugin_sdk::ServiceRequest {
        interface_id: bcode_model::MODEL_PROVIDER_INTERFACE_ID.into(),
        operation: operation.into(),
        payload: serde_json::to_vec(&json!({"selected_model_id":MODEL})).unwrap(),
    };
    let listing = crate::FakeProviderPlugin::models(&service(bcode_model::OP_MODELS));
    assert!(listing.error.is_none());
    let listing: bcode_model::ModelList = serde_json::from_slice(&listing.payload).unwrap();
    let model = listing
        .models
        .iter()
        .find(|model| model.model_id == MODEL)
        .unwrap();
    assert!(model.is_default);
    assert_eq!(
        listing
            .models
            .iter()
            .filter(|model| model.is_default)
            .count(),
        1
    );
    assert_eq!(model.max_image_input_base64_bytes, None);
    let capability =
        crate::FakeProviderPlugin::capabilities_response(&service(bcode_model::OP_CAPABILITIES));
    assert!(capability.error.is_none());
    let capability: bcode_model::ProviderCapabilities =
        serde_json::from_slice(&capability.payload).unwrap();
    let mut expected = crate::fake_feature_support();
    support(&mut expected);
    assert_eq!(model.feature_support.media_input, expected.media_input);
    assert_eq!(capability.feature_support.media_input, expected.media_input);
    let echo = listing
        .models
        .iter()
        .find(|model| model.model_id == "fake-echo")
        .unwrap();
    assert_eq!(
        echo.feature_support.media_input,
        crate::fake_feature_support().media_input
    );
    let panels = listing
        .models
        .iter()
        .find(|model| model.model_id == crate::vision::MODEL)
        .unwrap();
    let mut panel_expected = crate::fake_feature_support();
    crate::vision::support(&mut panel_expected);
    assert_eq!(
        panels.feature_support.media_input,
        panel_expected.media_input
    );
}

#[test]
fn real_reference_admission_preserves_both_scripted_tools_and_acknowledges_batch() {
    let mut request = request();
    assert!(crate::validate_fake_request(&request).is_none());
    let first = crate::scripted_prompt_tool_call(&request, 1).unwrap();
    assert_eq!(first.name, "filesystem.read");
    request.messages.push(result(
        &first.id,
        vec![ToolResultContent::ImageRef { image: image() }],
        false,
    ));
    assert!(crate::validate_fake_request(&request).is_none());
    let second = crate::scripted_prompt_tool_call(&request, 2).unwrap();
    assert_eq!(second.name, "shell.run");
    assert_eq!(second.arguments, json!({"command":"printf done"}));
    request.messages.push(result(&second.id, vec![], false));
    assert!(crate::scripted_prompt_tool_call(&request, 3).is_none());
    let text = crate::fake_response_text(&request, Some("last output"), "unused").unwrap();
    let ack: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        ack,
        json!({"type":"fake_tool_artifact_reference_ack", "pixel_understanding":false,
        "results":[{"call_id":first.id,"is_error":false,"image_reference_count":1},
        {"call_id":second.id,"is_error":false,"image_reference_count":0}]})
    );
    // Already-batched results follow the same path: no tool result is discarded.
    let mut batch = request.clone();
    let last = batch.messages.pop().unwrap();
    batch.messages[1].content.extend(last.content);
    assert_eq!(acknowledgment(&batch).unwrap(), text);
    // An unexpected failure remains terminal to the directive sequencer.
    let mut denied = request.clone();
    let ContentBlock::ToolResult { result } = &mut denied.messages[1].content[0] else {
        panic!("result")
    };
    result.is_error = true;
    denied.messages.pop();
    assert!(crate::scripted_prompt_tool_call(&denied, 2).is_none());
    let ack: Value = serde_json::from_str(&acknowledgment(&denied).unwrap()).unwrap();
    assert_eq!(ack["results"][0]["is_error"], true);
    // A later text-only activation never acknowledges an older image again.
    request.messages.push(bcode_model::ModelMessage {
        role: MessageRole::User,
        content: vec![ContentBlock::Text {
            text: "next activation".into(),
        }],
    });
    assert!(acknowledgment(&request).is_none());
}

#[test]
fn malformed_and_conflicting_references_fail_admission() {
    let original = image();
    let mut invalid = Vec::new();
    let mut value = original.clone();
    value.artifact_id = None;
    invalid.push(value);
    let mut value = original.clone();
    value.reference_key = Some(String::new());
    invalid.push(value);
    let mut value = original.clone();
    value.mime_type = "text/html".into();
    invalid.push(value);
    let mut value = original.clone();
    value.path = "x".repeat(4097);
    invalid.push(value);
    let mut value = original.clone();
    value.metadata.width = Some(0);
    invalid.push(value);
    let mut value = original.clone();
    value.metadata.height = None;
    invalid.push(value);
    let mut value = original.clone();
    value.artifact_id = Some("bad\nidentity".into());
    invalid.push(value);
    let mut value = original.clone();
    value.metadata.byte_len = Some(64 * 1024 * 1024 + 1);
    invalid.push(value);
    let mut value = original.clone();
    value.metadata.source_path = Some("different".into());
    invalid.push(value);
    for image in invalid {
        let mut request = request();
        request.messages.push(result(
            "read-1",
            vec![ToolResultContent::ImageRef { image }],
            false,
        ));
        assert_eq!(
            crate::validate_fake_request(&request).unwrap().code,
            "fake_artifact_reference_invalid"
        );
        assert!(acknowledgment(&request).is_none());
    }
    let mut conflict = with_reference();
    let mut changed = original;
    changed.metadata.width = Some(17);
    conflict.messages.push(result(
        "read-2",
        vec![ToolResultContent::ImageRef { image: changed }],
        false,
    ));
    assert_eq!(
        crate::validate_fake_request(&conflict).unwrap().code,
        "fake_artifact_reference_invalid"
    );
    let mut wrong_role = with_reference();
    wrong_role.messages[1].role = MessageRole::Assistant;
    assert!(crate::validate_fake_request(&wrong_role).is_some());
    let mut oversized = request();
    oversized.messages.push(result(
        "many",
        vec![ToolResultContent::ImageRef { image: image() }; 65],
        false,
    ));
    assert!(crate::validate_fake_request(&oversized).is_some());
}

#[test]
fn metadata_is_interpreted_without_dereferencing_and_duplicate_calls_fail_closed() {
    for mime in ["image/png", "image/jpeg", "image/webp"] {
        let mut image = image();
        image.mime_type = mime.into();
        image.path = "https://invalid.example/never-fetch-this".into();
        image.metadata.source_path = Some(image.path.clone());
        let mut request = request();
        request.messages.push(result(
            "read-1",
            vec![ToolResultContent::ImageRef { image }],
            false,
        ));
        let before = request.clone();
        assert!(crate::validate_fake_request(&request).is_none());
        let ack = acknowledgment(&request).unwrap();
        assert!(!ack.contains("never-fetch-this"));
        assert_eq!(request, before);
        request.messages.push(result("read-1", vec![], false));
        assert_eq!(
            crate::validate_fake_request(&request).unwrap().code,
            "fake_artifact_reference_invalid"
        );
        assert!(acknowledgment(&request).is_none());
    }
}

#[test]
fn echo_and_vision_stay_strict_and_inline_images_remain_unsupported() {
    for model in ["fake-echo", crate::vision::MODEL] {
        let mut request = with_reference();
        request.model_id = model.into();
        assert_eq!(
            crate::validate_fake_request(&request).unwrap().code,
            "fake_feature_unsupported"
        );
    }
    let inline = bcode_model::ImageContent {
        mime_type: "image/png".into(),
        data_base64: "AA==".into(),
        metadata: ImageMetadata::default(),
    };
    let mut tool = request();
    tool.messages.push(result(
        "read-1",
        vec![ToolResultContent::Image {
            image: inline.clone(),
        }],
        false,
    ));
    assert_eq!(
        crate::validate_fake_request(&tool).unwrap().code,
        "fake_feature_unsupported"
    );
    let mut direct = request();
    direct.messages[0]
        .content
        .push(ContentBlock::Image { image: inline });
    assert_eq!(
        crate::validate_fake_request(&direct).unwrap().code,
        "fake_image_input_unsupported"
    );
}
