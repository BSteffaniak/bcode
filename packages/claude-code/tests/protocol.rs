//! Protocol wire behavior against the pinned public SDK contract.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
use bcode_claude_code::{
    CallbackRequest, CallbackResponse, ControlRequest, HookMatcher, PermissionMode,
};
use serde_json::json;

#[test]
fn outbound_controls_match_pinned_protocol() {
    assert_eq!(
        serde_json::to_value(ControlRequest::SetPermissionMode {
            mode: PermissionMode::Plan
        })
        .expect("encode"),
        json!({"subtype":"set_permission_mode","mode":"plan"})
    );
    assert_eq!(
        serde_json::to_value(ControlRequest::Initialize { hooks: None }).expect("encode"),
        json!({"subtype":"initialize","hooks":null})
    );
    assert_eq!(
        serde_json::to_value(HookMatcher {
            matcher: Some("Read".into()),
            callback_ids: vec!["hook_1".into()]
        })
        .expect("encode"),
        json!({"matcher":"Read","hookCallbackIds":["hook_1"]})
    );
}

#[test]
fn unknown_callback_modes_are_not_guessed() {
    assert!(
        serde_json::from_value::<CallbackRequest>(json!({"subtype":"future_approval","input":{}}))
            .is_err()
    );
}

#[test]
fn ask_user_question_stays_native_permission_callback() {
    let callback: CallbackRequest = serde_json::from_value(
        json!({"subtype":"can_use_tool","tool_name":"AskUserQuestion",
        "input":{"questions":[{"question":"Which?"}]},"tool_use_id":"tool-1"}),
    )
    .expect("decode");
    assert!(
        matches!(callback, CallbackRequest::CanUseTool { tool_name, .. } if tool_name == "AskUserQuestion")
    );
    // Only a typed permission response is available, not a local tool-dispatch action.
    let _ = CallbackResponse::Deny {
        message: "not authorized".into(),
        interrupt: false,
    };
}
