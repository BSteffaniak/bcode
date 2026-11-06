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
fn native_setup_combines_exact_proposals_and_rejects_changed_preparation() {
    let instance: HarnessInstance =
        serde_json::from_value(settings()["instances"]["primary"]["instance"].clone()).unwrap();
    let candidate = bcode_harness_models::HarnessInstallationCandidate {
        plugin_id: instance.plugin_id.clone(),
        executable: bcode_harness_models::HarnessCandidateExecutableIdentity {
            canonical_path: instance.executable.clone(),
            byte_len: 12,
            modified_unix_nanos: Some(10),
            file_key: None,
        },
        native_version: "exact-version".into(),
        config_namespace: instance.config_namespace.clone(),
        custody: instance.auth.custody,
        readiness: bcode_harness_models::HarnessCandidateReadiness::ExistingNativeLogin,
    };
    let request = bcode_harness_models::HarnessPrepareCandidateRequest {
        schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
        request_id: "prepare-exact".into(),
        instance_id: instance.instance_id.clone(),
        candidate: candidate.clone(),
    };
    let native = bcode_harness_models::HarnessPreparedCandidate {
        candidate,
        instance,
        adapter_settings: json!({"native_owned": "opaque-setting"}),
    };
    let mut response = bcode_harness_models::HarnessPrepareCandidateResponse {
        schema_version: request.schema_version,
        request_id: request.request_id.clone(),
        plugin_id: native.instance.plugin_id.clone(),
        kind: bcode_harness_models::HarnessPrepareCandidateResponseKind::Prepared(Box::new(
            native.clone(),
        )),
    };
    let payload = PrepareNativeSetupRequest {
        request: request.clone(),
        response: response.clone(),
        adapter_configuration: json!({"instances": {"other": {"keep": true}}, "unrelated": true}),
        make_default: true,
    };
    let result = invoke(
        json!({}),
        PREPARE_SETUP_OPERATION,
        &serde_json::to_value(payload).unwrap(),
    );
    assert!(result.error.is_none());
    let prepared: PreparedNativeSetup = result.payload_json().unwrap();
    assert_eq!(prepared.native, native);
    assert_eq!(
        prepared.adapter_configuration["instances"]["other"],
        json!({"keep": true})
    );
    assert_eq!(
        prepared.adapter_configuration["instances"]["primary"],
        native.adapter_settings
    );
    assert_eq!(
        prepared.harness.configuration["default_instance"],
        json!("primary")
    );
    assert!(prepared.revalidate(&request, &response).is_ok());
    let mut changed = native;
    changed.adapter_settings = json!({"native_owned": "changed"});
    response.kind =
        bcode_harness_models::HarnessPrepareCandidateResponseKind::Prepared(Box::new(changed));
    assert!(prepared.revalidate(&request, &response).is_err());
    response.request_id = "different-request".into();
    assert!(prepared.revalidate(&request, &response).is_err());
}

#[test]
fn candidate_setup_requires_exact_binding_and_fresh_correlated_membership() {
    let instance: HarnessInstance =
        serde_json::from_value(settings()["instances"]["primary"]["instance"].clone()).unwrap();
    let candidate = bcode_harness_models::HarnessInstallationCandidate {
        plugin_id: instance.plugin_id.clone(),
        executable: bcode_harness_models::HarnessCandidateExecutableIdentity {
            canonical_path: instance.executable.clone(),
            byte_len: 12,
            modified_unix_nanos: Some(10),
            file_key: Some("device:inode".into()),
        },
        native_version: "exact-version".into(),
        config_namespace: instance.config_namespace.clone(),
        custody: instance.auth.custody,
        readiness: bcode_harness_models::HarnessCandidateReadiness::ExistingNativeLogin,
    };
    let setup = PrepareCandidateSetupRequest {
        candidate: candidate.clone(),
        setup: PrepareSetupRequest {
            instance,
            make_default: true,
        },
    };
    let request = bcode_harness_models::HarnessCandidatesRequest {
        schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
        request_id: "fresh-discovery".into(),
    };
    let mut response = bcode_harness_models::HarnessCandidatesResponse {
        schema_version: request.schema_version,
        request_id: request.request_id.clone(),
        plugin_id: candidate.plugin_id.clone(),
        kind: bcode_harness_models::HarnessCandidatesResponseKind::Candidates {
            candidates: vec![candidate.clone()],
            complete: true,
        },
    };
    assert!(setup.revalidate(&request, &response).is_ok());
    let prepared = invoke(
        json!({}),
        PREPARE_SETUP_OPERATION,
        &serde_json::to_value(&setup).unwrap(),
    );
    assert!(prepared.error.is_none());
    response.request_id = "stale-discovery".into();
    assert!(setup.revalidate(&request, &response).is_err());
    response.request_id = request.request_id;
    let mut changed = candidate;
    changed.executable.byte_len += 1;
    response.kind = bcode_harness_models::HarnessCandidatesResponseKind::Candidates {
        candidates: vec![changed],
        complete: true,
    };
    assert!(
        setup
            .revalidate(
                &bcode_harness_models::HarnessCandidatesRequest {
                    schema_version: response.schema_version,
                    request_id: response.request_id.clone(),
                },
                &response
            )
            .is_err()
    );
    let mut mismatch = setup;
    mismatch.setup.instance.config_namespace = "/other/config".into();
    assert!(mismatch.validate().is_err());
}

#[test]
fn setup_prepares_an_explicit_candidate_without_mutation_or_default_inference() {
    let instance = settings()["instances"]["primary"]["instance"].clone();
    let response = invoke(
        json!({}),
        PREPARE_SETUP_OPERATION,
        &json!({"instance": instance, "make_default": false}),
    );
    assert!(response.error.is_none());
    let setup: PreparedInstanceSetup = response.payload_json().unwrap();
    assert_eq!(
        setup.prepared.selection,
        InstanceSelection::Instance {
            instance_id: "primary".into()
        }
    );
    assert!(setup.configuration.get("default_instance").is_none());
    assert_eq!(
        setup.configuration["instances"]["primary"]["instance"],
        instance
    );
    // The returned settings feed the existing catalog/prepare path, not a parallel resolver.
    let catalog_response = invoke(setup.configuration, CATALOG_OPERATION, &json!({}));
    let catalog: InstanceCatalog = catalog_response.payload_json().unwrap();
    assert_eq!(catalog.instances["primary"], setup.prepared.instance);
    assert!(catalog.default_instance.is_none());
    assert!(
        !response
            .payload_text()
            .unwrap()
            .contains("never-return-this")
    );
}

#[test]
fn setup_preserves_other_instances_and_requires_explicit_default_consent() {
    let config = settings();
    let mut instance = config["instances"]["primary"]["instance"].clone();
    instance["instance_id"] = json!("secondary");
    for make_default in [false, true] {
        let response = invoke(
            config.clone(),
            PREPARE_SETUP_OPERATION,
            &json!({"instance": instance, "make_default": make_default}),
        );
        assert!(response.error.is_none());
        let setup: PreparedInstanceSetup = response.payload_json().unwrap();
        assert_eq!(
            setup.configuration["instances"]["primary"],
            config["instances"]["primary"]
        );
        assert_eq!(
            setup.configuration["default_instance"],
            json!(if make_default { "secondary" } else { "primary" })
        );
    }
}

#[test]
fn setup_rejects_overwrites_disabled_configuration_and_inline_secrets() {
    let config = settings();
    let mut changed = config["instances"]["primary"]["instance"].clone();
    changed["auth"]["profile"] = json!("different-account");
    let response = invoke(
        config.clone(),
        PREPARE_SETUP_OPERATION,
        &json!({"instance": changed, "make_default": true}),
    );
    assert_eq!(response.error.unwrap().code, "instance_conflict");
    let mut disabled = config.clone();
    disabled["enabled"] = json!(false);
    let response = invoke(
        disabled,
        PREPARE_SETUP_OPERATION,
        &json!({"instance": config["instances"]["primary"]["instance"], "make_default": true}),
    );
    assert_eq!(response.error.unwrap().code, "configuration_disabled");
    changed["auth"]["api_key"] = json!("must-not-escape");
    let response = invoke(
        json!({}),
        PREPARE_SETUP_OPERATION,
        &json!({"instance": changed, "make_default": true}),
    );
    assert_eq!(response.error.as_ref().unwrap().code, "invalid_request");
    assert!(!response.error.unwrap().message.contains("must-not-escape"));
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
    for config in [config, json!({"enabled": true, "instances": []})] {
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
fn absent_configuration_is_unconfigured_rather_than_invalid() {
    for config in [Value::Null, json!({})] {
        for (operation, payload) in [
            (CATALOG_OPERATION, json!({})),
            (PREPARE_OPERATION, json!({"selection": {"kind": "default"}})),
        ] {
            let response = invoke(config.clone(), operation, &payload);
            assert_eq!(response.error.unwrap().code, "configuration_disabled");
        }
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
