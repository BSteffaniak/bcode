use super::*;
use bcode_harness_models::native_connections::{
    NativeCandidate, NativeConnectionRecord, NativePreparedConnection,
};

extern "C" fn lookup_bridge(
    input: *const u8,
    length: usize,
    output: *mut u8,
    capacity: usize,
    written: *mut usize,
    data: *mut std::ffi::c_void,
) -> i32 {
    // Test-owned buffers/data remain live for the synchronous callback.
    let request: ServiceBridgeRequest =
        serde_json::from_slice(unsafe { std::slice::from_raw_parts(input, length) }).unwrap();
    let ServiceBridgeRequest::InvokeService(request) = request else {
        panic!("service request");
    };
    assert_eq!(
        request.interface_id,
        bcode_harness_models::native_connections::NATIVE_CONNECTION_LOOKUP_INTERFACE
    );
    assert_eq!(request.operation, "lookup");
    let payload = unsafe { &*data.cast::<serde_json::Value>() }.clone();
    let reply =
        ServiceBridgeResponse::Service(bcode_tool::ToolInvocationServiceResolution::Responded {
            payload,
        });
    let encoded = serde_json::to_vec(&reply).unwrap();
    assert!(encoded.len() <= capacity);
    unsafe {
        std::ptr::copy_nonoverlapping(encoded.as_ptr(), output, encoded.len());
        *written = encoded.len();
    }
    bcode_plugin_sdk::SERVICE_BRIDGE_STATUS_OK
}
fn publication() -> NativeConnectionRecord {
    let instance = HarnessInstance {
        instance_id: "published".into(),
        plugin_id: "fixture.native".into(),
        executable: "/fixture/bin/native".into(),
        config_namespace: "/fixture/account".into(),
        auth: HarnessAuthBinding {
            provider_id: "fixture".into(),
            profile: "native".into(),
            credential_namespace: "/fixture/account".into(),
            custody: HarnessAuthCustody::NativeManaged,
        },
    };
    let candidate = NativeCandidate {
        candidate_id: "candidate".into(),
        plugin_id: instance.plugin_id.clone(),
        executable: instance.executable.clone(),
        config_namespace: instance.config_namespace.clone(),
        installation_id: "fixture-artifact".into(),
    };
    NativeConnectionRecord {
        candidate: candidate.clone(),
        revision: 2,
        approved: true,
        approved_at_unix: 1,
        prepared: Some(NativePreparedConnection {
            candidate,
            instance,
            protocol_profile: "fixture-v1".into(),
            compatibility_evidence: "fixture-evidence".into(),
            isolation_profile: "fixture-isolated".into(),
        }),
    }
}
#[test]
fn host_publication_reaches_actual_catalog_and_prepare_without_config_writes() {
    let record = publication();
    let mut payload = json!({"records":[record]});
    let mut ctx = context(json!({}), CATALOG_OPERATION, &json!({}));
    ctx.bridge = ServiceBridge::new(
        Some(lookup_bridge),
        (&raw mut payload).cast(),
        ServiceCancellation::default(),
    );
    let original_config = ctx.config.config.clone();
    let response = super::super::invoke(&ctx);
    let catalog: InstanceCatalog = response.payload_json().unwrap();
    assert_eq!(
        catalog.instances["published"],
        record.prepared.unwrap().instance
    );
    assert!(catalog.default_instance.is_none());
    ctx.request.operation = PREPARE_OPERATION.into();
    ctx.request.payload =
        serde_json::to_vec(&json!({"selection":{"kind":"instance","instance_id":"published"}}))
            .unwrap();
    let response = super::super::invoke(&ctx);
    let prepared: PreparedInstance = response.payload_json().unwrap();
    assert_eq!(prepared.instance, catalog.instances["published"]);
    assert_eq!(ctx.config.config, original_config);
    payload["records"][0]["approved"] = json!(false);
    assert_eq!(
        super::super::invoke(&ctx).error.unwrap().code,
        "invalid_native_connections"
    );
}
#[test]
fn absent_configuration_without_publications_remains_disabled() {
    for config in [Value::Null, json!({})] {
        let mut payload = json!({"records":[]});
        let mut ctx = context(config, CATALOG_OPERATION, &json!({}));
        ctx.bridge = ServiceBridge::new(
            Some(lookup_bridge),
            (&raw mut payload).cast(),
            ServiceCancellation::default(),
        );
        assert_eq!(
            super::super::invoke(&ctx).error.unwrap().code,
            "configuration_disabled"
        );
    }
}

#[test]
fn explicitly_disabled_configuration_does_not_query_publications() {
    let mut ctx = context(
        json!({"enabled":false,"instances":{}}),
        CATALOG_OPERATION,
        &json!({}),
    );
    // An invalid host response would fail if lookup were attempted.
    let mut payload = json!({"invalid":true});
    ctx.bridge = ServiceBridge::new(
        Some(lookup_bridge),
        (&raw mut payload).cast(),
        ServiceCancellation::default(),
    );
    assert_eq!(
        super::super::invoke(&ctx).error.unwrap().code,
        "configuration_disabled"
    );
}

#[test]
fn null_configuration_can_use_host_publication() {
    let mut payload = json!({"records":[publication()]});
    let mut ctx = context(Value::Null, CATALOG_OPERATION, &json!({}));
    ctx.bridge = ServiceBridge::new(
        Some(lookup_bridge),
        (&raw mut payload).cast(),
        ServiceCancellation::default(),
    );
    let catalog: InstanceCatalog = super::super::invoke(&ctx).payload_json().unwrap();
    assert!(catalog.instances.contains_key("published"));
    assert!(ctx.config.config.is_null());
}

#[test]
fn configuration_edit_validation_is_independent_of_native_publications() {
    assert!(super::super::catalog_from_config(&Value::Null).is_err());
    assert!(super::super::catalog_from_config(&json!({})).is_err());
    let catalog = super::super::catalog_from_config(&super::settings()).unwrap();
    assert!(catalog.instances.contains_key("primary"));
    assert!(!catalog.instances.contains_key("published"));
}

#[test]
fn publication_cannot_replace_configured_identity() {
    let mut record = publication();
    record.prepared.as_mut().unwrap().instance.instance_id = "primary".into();
    let mut instances = BTreeMap::from([(
        "primary".into(),
        record.prepared.as_ref().unwrap().instance.clone(),
    )]);
    assert!(merge_published(&mut instances, vec![record]).is_err());
}
