use super::*;
use bcode_plugin_sdk::{PluginConfigContext, ServiceCancellation};
use serde_json::{Value, json};

fn settings() -> Value {
    json!({
        "enabled": true,
        "default_instance": "primary",
        "instances": {
            "primary": {
                "enabled": true,
                "instance": {
                    "instance_id": "primary",
                    "plugin_id": "example.native",
                    "executable": "/opt/example/native",
                    "config_namespace": "/example/config",
                    "auth": {
                        "provider_id": "example",
                        "profile": "native",
                        "credential_namespace": "/example/config",
                        "custody": "native_managed"
                    }
                }
            }
        }
    })
}

fn context(config: Value, operation: &str, payload: &Value) -> NativeServiceContext {
    NativeServiceContext {
        plugin_id: crate::PLUGIN_ID.into(),
        request: ServiceRequest {
            interface_id: INTERFACE_ID.into(),
            operation: operation.into(),
            payload: serde_json::to_vec(payload).unwrap(),
        },
        config: PluginConfigContext {
            config,
            // Neither host credentials nor redacted display configuration is catalog input.
            secrets: BTreeMap::from([("unrelated/secret".into(), "never-return-this".into())]),
            ..PluginConfigContext::default()
        },
        events: ServiceEventEmitter::default(),
        diagnostics: None,
        cancellation: ServiceCancellation::default(),
        bridge: ServiceBridge::default(),
        transient_progress_limits: TransientProgressLimits::default(),
    }
}

fn invoke(config: Value, operation: &str, payload: &Value) -> ServiceResponse {
    // Exercise actual service dispatch with no bridge installed: resolution cannot require
    // a provider call, native process, credential read, or session mutation.
    crate::HarnessPlugin.invoke_service(context(config, operation, payload))
}

#[test]
fn catalog_and_explicit_default_return_the_exact_non_secret_instance() {
    let response = invoke(settings(), CATALOG_OPERATION, &json!({}));
    assert!(response.error.is_none());
    let catalog: InstanceCatalog = response.payload_json().unwrap();
    assert_eq!(catalog.default_instance.as_deref(), Some("primary"));
    assert_eq!(catalog.instances.len(), 1);
    let response = invoke(
        settings(),
        PREPARE_OPERATION,
        &json!({"selection": {"kind": "default"}}),
    );
    assert!(response.error.is_none());
    let prepared: PreparedInstance = response.payload_json().unwrap();
    assert_eq!(prepared.selection, InstanceSelection::Default {});
    assert_eq!(prepared.instance, catalog.instances["primary"]);
    assert!(
        !response
            .payload_text()
            .unwrap()
            .contains("never-return-this")
    );
}

#[test]
fn exact_selection_never_falls_back_to_default_or_only_entry() {
    let response = invoke(
        settings(),
        PREPARE_OPERATION,
        &json!({"selection": {"kind": "instance", "instance_id": "missing"}}),
    );
    assert_eq!(response.error.unwrap().code, "instance_unavailable");
    let mut config = settings();
    config.as_object_mut().unwrap().remove("default_instance");
    let response = invoke(
        config.clone(),
        PREPARE_OPERATION,
        &json!({"selection": {"kind": "default"}}),
    );
    assert_eq!(response.error.unwrap().code, "instance_unavailable");
    let response = invoke(
        config,
        PREPARE_OPERATION,
        &json!({"selection": {"kind": "instance", "instance_id": "primary"}}),
    );
    assert!(response.error.is_none());
    assert_eq!(
        response
            .payload_json::<PreparedInstance>()
            .unwrap()
            .instance
            .instance_id,
        "primary"
    );
}

#[test]
fn invalid_disabled_and_mismatched_configuration_fails_closed() {
    for (pointer, value, code) in [
        ("/enabled", json!(false), "configuration_disabled"),
        (
            "/instances/primary/instance/instance_id",
            json!("other"),
            "invalid_configuration",
        ),
        (
            "/instances/primary/instance/executable",
            json!(""),
            "invalid_configuration",
        ),
        (
            "/instances/primary/enabled",
            json!(false),
            "invalid_configuration",
        ),
        (
            "/default_instance",
            json!("missing"),
            "invalid_configuration",
        ),
    ] {
        let mut config = settings();
        *config.pointer_mut(pointer).unwrap() = value;
        let response = invoke(config, CATALOG_OPERATION, &json!({}));
        assert_eq!(response.error.unwrap().code, code);
    }
    let mut config = settings();
    config.as_object_mut().unwrap().remove("default_instance");
    config["instances"]["primary"]["enabled"] = json!(false);
    let response = invoke(
        config,
        PREPARE_OPERATION,
        &json!({"selection": {"kind": "instance", "instance_id": "primary"}}),
    );
    assert_eq!(response.error.unwrap().code, "instance_unavailable");
}

#[test]
fn malformed_and_secret_bearing_settings_never_escape_in_errors() {
    let mut config = settings();
    config["instances"]["primary"]["instance"]["auth"]["api_key"] = json!("private-value");
    for config in [
        config,
        Value::Null,
        json!({}),
        json!({"enabled": true, "instances": []}),
    ] {
        let response = invoke(config, CATALOG_OPERATION, &json!({}));
        assert_eq!(
            response.error.as_ref().unwrap().code,
            "invalid_configuration"
        );
        assert_eq!(response.payload, Vec::<u8>::new());
        assert!(!response.error.unwrap().message.contains("private-value"));
    }
}

#[test]
fn explicit_reference_overrides_default_without_changing_auth_custody() {
    let mut config = settings();
    let mut secondary = config["instances"]["primary"].clone();
    secondary["instance"]["instance_id"] = json!("secondary");
    secondary["instance"]["auth"]["profile"] = json!("api");
    secondary["instance"]["auth"]["custody"] = json!("vault_managed");
    config["instances"]["secondary"] = secondary;
    let response = invoke(
        config,
        PREPARE_OPERATION,
        &json!({"selection": {"kind": "instance", "instance_id": "secondary"}}),
    );
    assert!(response.error.is_none());
    let prepared: PreparedInstance = response.payload_json().unwrap();
    assert_eq!(prepared.instance.instance_id, "secondary");
    assert_eq!(prepared.instance.auth.profile, "api");
    assert_eq!(
        prepared.instance.auth.custody,
        HarnessAuthCustody::VaultManaged
    );
}

#[test]
fn oversized_configuration_and_wrong_context_are_not_discovered() {
    let mut config = settings();
    config["instances"]["primary"]["instance"]["executable"] = json!("x".repeat(MAX_CONFIG_BYTES));
    assert_eq!(
        invoke(config, CATALOG_OPERATION, &json!({}))
            .error
            .unwrap()
            .code,
        "invalid_configuration"
    );
    let mut ctx = context(settings(), CATALOG_OPERATION, &json!({}));
    ctx.plugin_id = "other.plugin".into();
    assert_eq!(
        crate::HarnessPlugin.invoke_service(ctx).error.unwrap().code,
        "invalid_context"
    );
    assert_eq!(
        invoke(settings(), CATALOG_OPERATION, &json!({"unexpected": true}))
            .error
            .unwrap()
            .code,
        "invalid_request"
    );
}

#[test]
fn unknown_fields_versions_and_oversized_requests_are_rejected() {
    for payload in [
        json!({}),
        json!({"selection": {"kind": "automatic"}}),
        json!({"selection": {"kind": "default", "instance_id": "primary"}}),
        json!({"selection": {"kind": "default"}, "credential": "private"}),
    ] {
        assert_eq!(
            invoke(settings(), PREPARE_OPERATION, &payload)
                .error
                .unwrap()
                .code,
            "invalid_request"
        );
    }
    let mut ctx = context(settings(), PREPARE_OPERATION, &json!({}));
    ctx.request.payload = vec![b' '; MAX_REQUEST_BYTES + 1];
    assert_eq!(
        crate::HarnessPlugin.invoke_service(ctx).error.unwrap().code,
        "invalid_request"
    );
    let mut ctx = context(settings(), CATALOG_OPERATION, &json!({}));
    ctx.request.interface_id = "bcode.harness.configuration/v2".into();
    assert_eq!(
        crate::HarnessPlugin.invoke_service(ctx).error.unwrap().code,
        "unsupported_operation"
    );
}
