//! Deferred-auth client route guards; no transport or native process is needed.
use super::*;

#[test]
fn execution_catalog_uses_managed_application_not_native_plugin_route() {
    let query = bcode_ipc::harness::ManagedExecutionCatalogRequest {
        request_id: "catalog".into(),
        instances: Vec::new(),
        provider_plugin_id: None,
    };
    let envelope = ManagedSessionRequest {
        schema_version: bcode_ipc::harness::MANAGED_SESSION_VERSION,
        operation: ManagedSessionOperation::ExecutionCatalog,
        request_json: serde_json::to_string(&query).unwrap(),
    };
    let bytes = serde_json::to_vec(&envelope).unwrap();
    assert!(validate_route(MANAGED_SESSION_INTERFACE_ID, "execution_catalog", &bytes).is_ok());
    assert!(validate_route(MANAGED_SESSION_INTERFACE_ID, "start_selected", &bytes).is_err());
    assert!(
        validate_route(
            bcode_harness_models::HARNESS_MODELS_INTERFACE_ID,
            "discover_models",
            &bytes
        )
        .is_err()
    );
    assert!(validate_route(bcode_harness_models::HARNESS_INTERFACE_ID, "open", &bytes).is_err());
}

#[test]
fn catalog_cannot_smuggle_a_native_selected_mutation() {
    let envelope = ManagedSessionRequest {
        schema_version: bcode_ipc::harness::MANAGED_SESSION_VERSION,
        operation: ManagedSessionOperation::ExecutionCatalog,
        request_json: serde_json::json!({
            "request_id": "catalog", "instances": [], "provider_plugin_id": null,
            "operation": "open_selected", "request": {"model_id": "unadmitted"},
        })
        .to_string(),
    };
    assert!(
        validate_route(
            MANAGED_SESSION_INTERFACE_ID,
            "execution_catalog",
            &serde_json::to_vec(&envelope).unwrap()
        )
        .is_err()
    );
}

#[test]
fn candidates_allow_no_native_launch_inputs_or_direct_vendor_route() {
    let mut envelope = ManagedSessionRequest {
        schema_version: bcode_ipc::harness::MANAGED_SESSION_VERSION,
        operation: ManagedSessionOperation::DiscoverCandidates,
        request_json: serde_json::json!({"request_id":"candidate-query"}).to_string(),
    };
    let bytes = serde_json::to_vec(&envelope).unwrap();
    assert!(validate_route(MANAGED_SESSION_INTERFACE_ID, "discover_candidates", &bytes).is_ok());
    assert!(
        validate_route(
            bcode_harness_models::HARNESS_CANDIDATES_INTERFACE_ID,
            "discover_candidates",
            &bytes
        )
        .is_err()
    );
    envelope.request_json = serde_json::json!({
        "request_id":"candidate-query", "executable":"/client/supplied/path",
    })
    .to_string();
    assert!(
        validate_route(
            MANAGED_SESSION_INTERFACE_ID,
            "discover_candidates",
            &serde_json::to_vec(&envelope).unwrap()
        )
        .is_err()
    );
}
