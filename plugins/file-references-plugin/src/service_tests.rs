use super::*;
use bcode_plugin_sdk::composer::{ComposerDocument, PluginComposerContributor};
use bcode_plugin_sdk::{PluginConfigContext, ServiceCancellation};

fn context() -> NativeServiceContext {
    let document = ComposerDocument {
        document_id: "doc".into(),
        revision: 1,
        text: "@src".into(),
        cursor: 4,
        selection: None,
    };
    let request = ComposerSearchRequest {
        contribution_id: crate::CONTRIBUTION_ID.into(),
        request_id: "search".into(),
        activation: crate::FileReferenceContributor.activate(&document).unwrap(),
        document,
        max_results: 8,
        interaction_state: std::collections::BTreeMap::new(),
        action_id: None,
    };
    NativeServiceContext {
        diagnostics: None,
        plugin_id: "bcode.file-references".into(),
        request: ServiceRequest {
            interface_id: COMPOSER_INTERFACE_ID.into(),
            operation: OP_COMPOSER_SEARCH.into(),
            payload: serde_json::to_vec(&request).unwrap(),
        },
        config: PluginConfigContext::default(),
        events: ServiceEventEmitter::default(),
        cancellation: ServiceCancellation::default(),
        bridge: ServiceBridge::default(),
        transient_progress_limits: TransientProgressLimits::default(),
    }
}

fn invoke(context: NativeServiceContext) -> ServiceResponse {
    FileReferencesPlugin.invoke_service(context)
}

#[test]
fn malformed_payload_and_unsupported_routes_are_bounded_errors() {
    for payload in [b"{".to_vec(), vec![0xff], b"{}".to_vec()] {
        let mut context = context();
        context.request.payload = payload;
        assert_eq!(invoke(context).error.unwrap().code, "invalid_request");
    }
    let mut context = context();
    context.request.operation = "unknown".into();
    assert_eq!(invoke(context).error.unwrap().code, "unsupported_service");
}

#[test]
fn unavailable_and_bad_config_return_valid_error_snapshots() {
    for (config, expected) in [
        (serde_json::Value::Null, "search_unavailable"),
        (
            serde_json::json!({"workspace_root":"/untrusted"}),
            "invalid_config",
        ),
        (serde_json::json!({"max_results":"bad"}), "invalid_config"),
    ] {
        let mut context = context();
        let request = context
            .request
            .payload_json::<ComposerSearchRequest>()
            .unwrap();
        context.config.config = config;
        let response = invoke(context);
        assert!(response.error.is_none());
        let snapshot = response.payload_json::<ComposerSnapshot>().unwrap();
        assert_eq!(
            snapshot.state,
            ComposerSnapshotState::Error {
                code: expected.into()
            }
        );
        assert!(snapshot.candidates.is_empty());
        snapshot.validate_for(&request).unwrap();
    }
}

extern "C" fn cancelled_reply(
    _request: *const u8,
    _request_len: usize,
    output: *mut u8,
    capacity: usize,
    output_len: *mut usize,
    _data: *mut std::ffi::c_void,
) -> i32 {
    let response =
        ServiceBridgeResponse::Service(bcode_tool::ToolInvocationServiceResolution::Cancelled);
    let Ok(bytes) = serde_json::to_vec(&response) else {
        return 1;
    };
    if bytes.len() > capacity {
        return 1;
    }
    // SAFETY: bridge provides valid output buffers and capacity was checked.
    unsafe {
        *output_len = bytes.len();
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), output, bytes.len());
    }
    bcode_plugin_sdk::SERVICE_BRIDGE_STATUS_OK
}

#[test]
fn contribution_dispatch_accepts_own_and_legacy_only() {
    for selector in [
        None,
        Some(""),
        Some(crate::CONTRIBUTION_ID),
        Some("another"),
    ] {
        let mut context = context();
        let mut payload: serde_json::Value =
            serde_json::from_slice(&context.request.payload).unwrap();
        if let Some(selector) = selector {
            payload["contribution_id"] = selector.into();
        } else {
            payload.as_object_mut().unwrap().remove("contribution_id");
        }
        context.request.payload = serde_json::to_vec(&payload).unwrap();
        let response = invoke(context);
        if selector == Some("another") {
            assert_eq!(response.error.unwrap().code, "unsupported_contribution");
        } else {
            assert!(response.error.is_none());
            assert_eq!(
                response.payload_json::<ComposerSnapshot>().unwrap().state,
                ComposerSnapshotState::Error {
                    code: "search_unavailable".into()
                }
            );
        }
    }
}

#[test]
fn cancellation_before_search_and_in_nested_reply_are_truthful() {
    for nested in [false, true] {
        let mut context = context();
        if nested {
            context.bridge = ServiceBridge::new(
                Some(cancelled_reply),
                std::ptr::null_mut(),
                context.cancellation.clone(),
            );
        } else {
            context.cancellation.cancel();
        }
        let snapshot = invoke(context).payload_json::<ComposerSnapshot>().unwrap();
        assert_eq!(
            snapshot.state,
            ComposerSnapshotState::Error {
                code: "cancelled".into()
            }
        );
        assert!(snapshot.candidates.is_empty());
    }
}
