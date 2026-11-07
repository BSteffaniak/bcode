//! Source-boundary tests only: no transport, credentials or native processes.
use super::*;
use serde_json::{Value, json};

fn proposal() -> Value {
    let candidate = json!({
        "plugin_id": "example.native",
        "executable": {"canonical_path": "/opt/native", "byte_len": 12,
            "modified_unix_nanos": null, "file_key": null},
        "native_version": "1.0.0", "config_namespace": "/native/config",
        "custody": "native_managed", "readiness": "login_required"
    });
    json!({
        "request": {"schema_version": bcode_harness_models::HARNESS_SCHEMA_VERSION,
            "request_id": "prepare", "instance_id": "primary", "candidate": candidate},
        "response": {"schema_version": bcode_harness_models::HARNESS_SCHEMA_VERSION,
            "request_id": "prepare", "plugin_id": "example.native", "kind": {"kind": "prepared",
            "response": {"candidate": candidate, "instance": {
                "instance_id": "primary", "plugin_id": "example.native",
                "executable": "/opt/native", "config_namespace": "/native/config",
                "auth": {"provider_id": "example", "profile": "native",
                    "credential_namespace": "/native/config", "custody": "native_managed"}
            }, "adapter_settings": {"native_owned": true}}}},
        "adapter_configuration": {}, "make_default": false
    })
}

fn accepts(value: &Value) -> bool {
    validate_route(
        CONFIGURATION_INTERFACE,
        "prepare_setup",
        &serde_json::to_vec(value).unwrap(),
    )
    .is_ok()
}

#[test]
fn native_setup_has_its_own_bounded_allowance_without_widening_other_routes() {
    let mut value = proposal();
    value["adapter_configuration"]["preserved"] = json!("x".repeat(5000));
    let payload = serde_json::to_vec(&value).unwrap();
    assert!(payload.len() > MAX_CONFIGURATION_REQUEST);
    assert!(accepts(&value));
    for operation in ["catalog", "prepare", "open", "auth_begin", "unknown"] {
        assert!(validate_route(CONFIGURATION_INTERFACE, operation, &payload).is_err());
    }
    assert!(validate_route("arbitrary.service/v1", "prepare_setup", &payload).is_err());
    assert!(
        validate_route(
            bcode_harness_models::HARNESS_CANDIDATES_INTERFACE_ID,
            "prepare_candidate",
            &payload
        )
        .is_err()
    );
    let mut at_limit = payload;
    at_limit.resize(MAX_SETUP_REQUEST, b' ');
    assert!(validate_route(CONFIGURATION_INTERFACE, "prepare_setup", &at_limit).is_ok());
    at_limit.push(b' ');
    assert!(validate_route(CONFIGURATION_INTERFACE, "prepare_setup", &at_limit).is_err());
}

#[test]
fn native_setup_rejects_unknown_malformed_and_uncorrelated_proposals() {
    assert!(accepts(&proposal()));
    for pointer in [
        "/request/request_id",
        "/response/request_id",
        "/response/plugin_id",
        "/response/kind/response/instance/instance_id",
        "/response/kind/response/instance/executable",
        "/response/kind/response/candidate/native_version",
        "/response/kind/response/instance/auth/custody",
    ] {
        let mut value = proposal();
        *value.pointer_mut(pointer).unwrap() = json!("changed");
        assert!(!accepts(&value), "accepted changed {pointer}");
    }
    for pointer in [
        "",
        "/request",
        "/request/candidate",
        "/request/candidate/executable",
        "/response",
        "/response/kind",
        "/response/kind/response",
        "/response/kind/response/instance",
        "/response/kind/response/instance/auth",
    ] {
        let mut value = proposal();
        value
            .pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("unknown".into(), json!(true));
        assert!(!accepts(&value), "accepted unknown field in {pointer}");
    }
    for value in [
        json!({}),
        json!({"instance": {}, "make_default": false}),
        json!(null),
    ] {
        assert!(!accepts(&value));
    }
    let mut value = proposal();
    value["adapter_configuration"] = json!([]);
    assert!(!accepts(&value));
    value = proposal();
    value["request"]["schema_version"] = json!(65535);
    assert!(!accepts(&value));
}

#[test]
fn candidate_preparation_decoder_requires_exact_operation_version_and_shape() {
    let value = proposal();
    let mut envelope = ManagedSessionRequest {
        schema_version: bcode_ipc::harness::MANAGED_SESSION_VERSION,
        operation: ManagedSessionOperation::PrepareCandidate,
        request_json: value["request"].to_string(),
    };
    assert!(envelope.decode_prepare_candidate().is_ok());
    envelope.operation = ManagedSessionOperation::DiscoverCandidates;
    assert!(envelope.decode_prepare_candidate().is_err());
    envelope.operation = ManagedSessionOperation::PrepareCandidate;
    envelope.schema_version = u16::MAX;
    assert!(envelope.decode_prepare_candidate().is_err());
    envelope.schema_version = bcode_ipc::harness::MANAGED_SESSION_VERSION;
    envelope.request_json = json!({"request_id": "prepare", "unknown": true}).to_string();
    assert!(envelope.decode_prepare_candidate().is_err());
}
