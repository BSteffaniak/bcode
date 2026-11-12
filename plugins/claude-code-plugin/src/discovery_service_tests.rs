use super::*;
use bcode_plugin_sdk::{RustPlugin, ServiceRequest};

fn context(payload: &serde_json::Value) -> NativeServiceContext {
    NativeServiceContext {
        plugin_id: crate::PLUGIN_ID.into(),
        request: ServiceRequest {
            interface_id: CLAUDE_DISCOVERY_INTERFACE_ID.into(),
            operation: CLAUDE_DISCOVERY_OPERATION.into(),
            payload: serde_json::to_vec(payload).unwrap(),
        },
        config: bcode_plugin_sdk::PluginConfigContext::default(),
        events: bcode_plugin_sdk::ServiceEventEmitter::default(),
        diagnostics: None,
        cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
        bridge: bcode_plugin_sdk::ServiceBridge::default(),
        transient_progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
    }
}

#[test]
fn invalid_service_queries_fail_before_discovery_or_runtime_creation() {
    for payload in [
        serde_json::json!({"schema_version":2,"request_id":"query"}),
        serde_json::json!({"schema_version":1,"request_id":""}),
        serde_json::json!({"schema_version":1,"request_id":"query","search_path":"/untrusted"}),
        serde_json::json!({"schema_version":1,"request_id":"x".repeat(1025)}),
    ] {
        let context = context(&payload);
        assert!(
            service_with(&context, || panic!("must not discover"))
                .error
                .is_some()
        );
        let mut plugin = crate::ClaudeCodePlugin::default();
        assert!(plugin.invoke_service(context).error.is_some());
        assert!(plugin.runtime.is_none());
        assert!(plugin.processes.is_empty());
    }
    let mut context = context(&serde_json::json!({"schema_version":1,"request_id":"query"}));
    context.request.operation = "launch".into();
    assert!(
        service_with(&context, || panic!("must not launch"))
            .error
            .is_some()
    );
}

#[test]
fn synthetic_service_response_is_bounded_passive_and_explicitly_unknown() {
    use crate::discovery::{ClaudeCodeDiscovery, discover_candidates_in};
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };
    let root = std::env::temp_dir().join(format!(
        "bcode-claude-service-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let bin = root.join("bin");
    let home = root.join("home");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(home.join(".claude")).unwrap();
    let marker = root.join("executed");
    let executable = bin.join("claude");
    fs::write(
        &executable,
        format!("#!/bin/sh\ntouch '{}'\nexit 99\n", marker.display()),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        // A credential-shaped unreadable file must never be opened by the query.
        let credentials = home.join(".claude/.credentials.json");
        fs::write(&credentials, "synthetic-secret-must-not-appear").unwrap();
        fs::set_permissions(credentials, fs::Permissions::from_mode(0o000)).unwrap();
    }
    let input = ClaudeCodeDiscovery {
        home,
        search_path: bin.into_os_string(),
        config_directory: None,
    };
    let context = context(&serde_json::json!({"schema_version":1,"request_id":"query"}));
    let response = service_with(&context, || discover_candidates_in(&input));
    assert!(response.error.is_none());
    let decoded: ClaudeDiscoveryResponse = serde_json::from_slice(&response.payload).unwrap();
    assert_eq!(decoded.request_id, "query");
    assert_eq!(decoded.candidates.len(), 1);
    assert_eq!(
        decoded.candidates[0].compatibility,
        ClaudeDiscoveryEvidence::Unknown
    );
    assert_eq!(
        decoded.candidates[0].readiness,
        ClaudeDiscoveryEvidence::Unknown
    );
    assert_eq!(decoded.required_setup.len(), 3);
    assert!(!String::from_utf8_lossy(&response.payload).contains("synthetic-secret"));
    assert!(!marker.exists());
    let candidate = discover_candidates_in(&input).pop().unwrap();
    let capped = service_with(&context, || vec![candidate; MAX_DISCOVERY_CANDIDATES + 1]);
    let decoded: ClaudeDiscoveryResponse = serde_json::from_slice(&capped.payload).unwrap();
    assert_eq!(decoded.candidates.len(), MAX_DISCOVERY_CANDIDATES);
    assert!(decoded.truncated);
    assert!(capped.payload.len() <= MAX_RESPONSE_BYTES);
    assert!(
        bounded_path(std::path::Path::new(
            &"x".repeat(MAX_DISCOVERY_PATH_BYTES + 1)
        ))
        .is_none()
    );
    assert!(bounded_path(std::path::Path::new("/control\npath")).is_none());
    fs::remove_dir_all(root).unwrap();
}
