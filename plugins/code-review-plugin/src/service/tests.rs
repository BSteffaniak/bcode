//! The service adapters, driven through the plugin entry point with JSON payloads.

use bcode_code_review::testing::fixture::{FixtureSpec, GitFixture};
use bcode_code_review_models::{
    CODE_REVIEW_SERVICE_INTERFACE_ID, GetReviewWorkspaceResponse, OP_REVIEW_BLOB_LINES_GET,
    OP_REVIEW_SOURCES_RESOLVE, OP_REVIEW_WORKSPACE_GET, OP_REVIEW_WORKSPACE_UPDATE,
    REVIEW_ERROR_PATH_REJECTED, ResolveReviewSourcesResponse, ReviewSourceKind, ReviewTarget,
    ReviewWorkspace,
};
use bcode_plugin_sdk::prelude::{RustPlugin as _, ServiceResponse};

fn call(
    plugin: &mut crate::CodeReviewPlugin,
    operation: &str,
    payload: &serde_json::Value,
    config: &serde_json::Value,
) -> ServiceResponse {
    let context = serde_json::from_value(serde_json::json!({
        "plugin_id": "bcode.code_review",
        "request": {
            "interface_id": CODE_REVIEW_SERVICE_INTERFACE_ID,
            "operation": operation,
            "payload": serde_json::to_vec(payload).expect("payload"),
        },
        "config": { "config": config },
    }))
    .expect("context");
    plugin.invoke_service(context)
}

#[test]
fn repository_reads_decode_requests_and_map_errors_to_stable_codes() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let mut plugin = crate::CodeReviewPlugin::default();
    let none = serde_json::Value::Null;
    let response = call(
        &mut plugin,
        OP_REVIEW_SOURCES_RESOLVE,
        &serde_json::json!({
            "repo_path": fixture.root(),
            "sources": [{
                "id": "s",
                "label": "Last commit",
                "kind": ReviewSourceKind::LastCommit,
                "included": true,
            }],
        }),
        &none,
    );
    assert!(response.error.is_none(), "{:?}", response.error);
    let resolved: ResolveReviewSourcesResponse = response.payload_json().expect("decode");
    assert_eq!(resolved.sources.len(), 1);

    let response = call(
        &mut plugin,
        OP_REVIEW_BLOB_LINES_GET,
        &serde_json::json!({
            "repo_path": fixture.root(),
            "blob": {"kind": "revision", "revision": {"kind": "working_tree"}, "path": "../x"},
            "start_line": 1,
            "count": 1,
        }),
        &none,
    );
    assert_eq!(
        response.error.map(|error| error.code),
        Some(REVIEW_ERROR_PATH_REJECTED.to_string())
    );

    let response = call(
        &mut plugin,
        OP_REVIEW_SOURCES_RESOLVE,
        &serde_json::json!(1),
        &none,
    );
    assert_eq!(
        response.error.map(|error| error.code),
        Some("invalid_request".to_string())
    );
}

#[test]
fn stored_state_uses_the_configured_state_directory() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let state = tempfile::tempdir().expect("state");
    let config = serde_json::json!({ "state_dir": state.path() });
    let mut plugin = crate::CodeReviewPlugin::default();
    let mut workspace =
        ReviewWorkspace::from_target(fixture.root().to_path_buf(), ReviewTarget::LastCommit);
    workspace.id = "workspace-1".to_owned();

    let response = call(
        &mut plugin,
        OP_REVIEW_WORKSPACE_UPDATE,
        &serde_json::json!({ "repo_path": fixture.root(), "workspace": workspace }),
        &config,
    );
    assert!(response.error.is_none(), "{:?}", response.error);
    assert!(state.path().join("code-review.db").exists());

    let response = call(
        &mut plugin,
        OP_REVIEW_WORKSPACE_GET,
        &serde_json::json!({ "repo_path": fixture.root(), "workspace_id": "workspace-1" }),
        &config,
    );
    let loaded: GetReviewWorkspaceResponse = response.payload_json().expect("decode");
    assert_eq!(
        loaded.workspace.map(|workspace| workspace.id).as_deref(),
        Some("workspace-1")
    );

    let response = call(
        &mut plugin,
        OP_REVIEW_WORKSPACE_GET,
        &serde_json::json!({ "repo_path": fixture.root(), "workspace_id": "workspace-1" }),
        &serde_json::json!({ "state_location": "elsewhere" }),
    );
    assert_eq!(
        response.error.map(|error| error.code),
        Some("invalid_config".to_string())
    );
}
