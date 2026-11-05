//! Synthetic credential callback coverage; never opens a vault or native process.
use super::*;
use bcode_plugin_sdk::{
    NativeServiceContext, PluginConfigContext, PluginCredentialSelection, ServiceBridge,
    ServiceCancellation, ServiceRequest,
};
use std::sync::atomic::{AtomicBool, Ordering};

struct Grant {
    selection: PluginCredentialSelection,
    live: AtomicBool,
}

extern "C" fn credential_callback(
    input: *const u8,
    length: usize,
    output: *mut u8,
    capacity: usize,
    written: *mut usize,
    data: *mut std::ffi::c_void,
) -> i32 {
    if input.is_null() || output.is_null() || written.is_null() || data.is_null() {
        return bcode_plugin_sdk::SERVICE_BRIDGE_STATUS_FAILED;
    }
    // SAFETY: the test retains Grant and SDK-owned buffers for this synchronous call.
    let (grant, bytes) = unsafe {
        (
            &*data.cast::<Grant>(),
            std::slice::from_raw_parts(input, length),
        )
    };
    let Some(encoded) = bytes.strip_prefix(bcode_plugin_sdk::SENSITIVE_CREDENTIAL_PREFIX) else {
        return bcode_plugin_sdk::SERVICE_BRIDGE_STATUS_FAILED;
    };
    let Ok(selection) = serde_json::from_slice::<PluginCredentialSelection>(encoded) else {
        return bcode_plugin_sdk::SERVICE_BRIDGE_STATUS_FAILED;
    };
    if !grant.live.load(Ordering::SeqCst) || selection != grant.selection {
        return bcode_plugin_sdk::SERVICE_BRIDGE_STATUS_FAILED;
    }
    let secret = b"synthetic-api-material";
    if capacity < secret.len() {
        return bcode_plugin_sdk::SERVICE_BRIDGE_STATUS_FAILED;
    }
    // SAFETY: capacity was checked and written points to the SDK's live length variable.
    unsafe {
        std::ptr::copy_nonoverlapping(secret.as_ptr(), output, secret.len());
        *written = secret.len();
    }
    bcode_plugin_sdk::SERVICE_BRIDGE_STATUS_OK
}

fn instance() -> HarnessInstance {
    HarnessInstance {
        instance_id: "api-instance".into(),
        plugin_id: crate::PLUGIN_ID.into(),
        executable: "/isolated/claude".into(),
        config_namespace: "/isolated/config".into(),
        auth: bcode_harness_models::HarnessAuthBinding {
            provider_id: "claude-code".into(),
            profile: "selected-api".into(),
            credential_namespace: "vault:selected-api".into(),
            custody: HarnessAuthCustody::VaultManaged,
        },
    }
}

fn configured() -> InstanceConfig {
    serde_json::from_value(serde_json::json!({
        "executable":"/isolated/claude", "config_directory":"/isolated/config",
        "exact_version":"fixture", "setting_sources":[],
        "billing":{"route":"api_key", "provider_id":"claude-code",
            "profile":"selected-api", "credential_namespace":"vault:selected-api"}
    }))
    .expect("non-secret configuration")
}

fn context(grant: &Grant) -> NativeServiceContext {
    NativeServiceContext {
        plugin_id: crate::PLUGIN_ID.into(),
        request: ServiceRequest {
            interface_id: bcode_harness_models::HARNESS_INTERFACE_ID.into(),
            operation: "open".into(),
            payload: Vec::new(),
        },
        config: PluginConfigContext::default(),
        events: bcode_plugin_sdk::ServiceEventEmitter::default(),
        diagnostics: None,
        cancellation: ServiceCancellation::default(),
        bridge: ServiceBridge::new(
            Some(credential_callback),
            std::ptr::from_ref(grant).cast_mut().cast(),
            ServiceCancellation::default(),
        ),
        transient_progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
    }
}

#[test]
fn exact_api_selection_and_revoked_or_cancelled_callback_fail_closed() {
    let config = configured();
    let instance = instance();
    let selection = config.credential_selection(&instance).unwrap().unwrap();
    let grant = Grant {
        selection: selection.clone(),
        live: AtomicBool::new(true),
    };
    let context = context(&grant);
    let secret = context.credential(&selection).unwrap();
    assert_eq!(secret.expose(), "synthetic-api-material");
    assert!(!format!("{secret:?}").contains("synthetic-api-material"));
    for field in 0..5 {
        let mut wrong = selection.clone();
        match field {
            0 => wrong.profile = "other".into(),
            1 => wrong.provider_id = "other".into(),
            2 => wrong.credential_namespace = "other".into(),
            3 => wrong.custody = HarnessAuthCustody::NativeManaged,
            _ => wrong.credential_id = "unrelated".into(),
        }
        assert!(context.credential(&wrong).is_err());
    }
    grant.live.store(false, Ordering::SeqCst);
    assert!(context.credential(&selection).is_err());
    grant.live.store(true, Ordering::SeqCst);
    context.cancellation.cancel();
    assert!(context.credential(&selection).is_err());
}

#[test]
fn selected_api_secret_enters_only_api_environment() {
    let config = configured();
    let instance = instance();
    let selection = config.credential_selection(&instance).unwrap().unwrap();
    let grant = Grant {
        selection: selection.clone(),
        live: AtomicBool::new(true),
    };
    let context = context(&grant);
    let mut options = LaunchOptions {
        executable: "/isolated/claude".into(),
        expected_version: "fixture".into(),
        cwd: "/isolated/work".into(),
        config_directory: "/isolated/config".into(),
        environment: NativeEnvironment::default(),
        billing_route: BillingRoute::ApiKey,
        settings: SettingsInheritance::ExplicitSources,
        setting_sources: Vec::new(),
        resume_session_id: None,
        settlement_support: SettlementSupport::Unknown,
        auth_support: NativeAuthSupport::Unknown,
        limits: Limits::default(),
    };
    InstanceConfig::inject_api_credential(&mut options, &context, &selection).unwrap();
    assert_eq!(options.environment.0.len(), 1);
    assert_eq!(
        options.environment.0["ANTHROPIC_API_KEY"],
        "synthetic-api-material"
    );
    assert!(
        !serde_json::to_string(&context)
            .unwrap()
            .contains("synthetic-api-material")
    );
    assert!(!format!("{context:?}").contains("synthetic-api-material"));
    options.environment.0.clear();
    options.billing_route = BillingRoute::NativeAccount;
    assert!(InstanceConfig::inject_api_credential(&mut options, &context, &selection).is_err());
    assert!(options.environment.0.is_empty());
}

#[test]
fn api_inspection_is_nonsecret_and_does_not_require_credential_readiness() {
    use bcode_plugin_sdk::RustPlugin as _;
    let instance = instance();
    let selection = configured()
        .credential_selection(&instance)
        .unwrap()
        .unwrap();
    let denied = Grant {
        selection,
        live: AtomicBool::new(false),
    };
    let mut context = context(&denied);
    let config = serde_json::json!({
        "executable":"/isolated/claude", "config_directory":"/isolated/config",
        "exact_version":"fixture", "setting_sources":[],
        "billing":{"route":"api_key", "provider_id":"claude-code",
            "profile":"selected-api", "credential_namespace":"vault:selected-api"}
    });
    context.config.config = serde_json::json!({"instances":{"api-instance":config}});
    context.request.operation = "inspect".into();
    context.request.payload = serde_json::to_vec(&bcode_harness_models::HarnessRequest {
        schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
        request_id: "inspect".into(),
        instance,
        kind: bcode_harness_models::HarnessRequestKind::Inspect,
    })
    .unwrap();
    let response = crate::ClaudeCodePlugin::default().invoke_service(context);
    assert!(response.error.is_none());
    let response: bcode_harness_models::HarnessServiceResponse =
        serde_json::from_slice(&response.payload).unwrap();
    assert!(matches!(
        &response.kind,
        bcode_harness_models::HarnessServiceResponseKind::Inspection(_)
    ));
    assert!(
        !serde_json::to_string(&response)
            .unwrap()
            .contains("synthetic-api-material")
    );
}

#[test]
fn api_configuration_rejects_selection_mismatch_and_missing_host_grant() {
    let config = configured();
    let instance = instance();
    assert!(config.credential_selection(&instance).unwrap().is_some());
    for field in 0..6 {
        let mut wrong = instance.clone();
        match field {
            0 => wrong.auth.profile = "other".into(),
            1 => wrong.auth.provider_id = "other".into(),
            2 => wrong.auth.credential_namespace = "other".into(),
            3 => wrong.auth.custody = HarnessAuthCustody::NativeManaged,
            4 => wrong.auth.custody = HarnessAuthCustody::External,
            _ => wrong.instance_id = "other".into(),
        }
        if field == 5 {
            // Configuration selection is keyed by instance ID in the plugin service;
            // the sensitive host callback must independently reject a different ID.
            let mut selection = config.credential_selection(&instance).unwrap().unwrap();
            selection.instance_id = wrong.instance_id;
            let grant = Grant {
                selection: config.credential_selection(&instance).unwrap().unwrap(),
                live: AtomicBool::new(true),
            };
            assert!(context(&grant).credential(&selection).is_err());
        } else {
            assert!(config.credential_selection(&wrong).is_err());
        }
    }
    // The old semantic map must not satisfy the sensitive accessor.
    let selection = config.credential_selection(&instance).unwrap().unwrap();
    let denied = Grant {
        selection: selection.clone(),
        live: AtomicBool::new(false),
    };
    let mut missing = context(&denied);
    missing.bridge = ServiceBridge::default();
    missing.config.secrets.insert(
        format!("{}/claude-code/api_key", crate::PLUGIN_ID),
        "unrelated-secret".into(),
    );
    assert!(missing.credential(&selection).is_err());
    assert!(
        config
            .options(&instance, "/isolated/work".into(), None)
            .is_err()
    );
}
