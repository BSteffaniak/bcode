mod canonical_lifecycle;
mod managed_open;
mod pipeline;
use super::*;
use bcode_harness_models::native_connections::{
    NATIVE_CONNECTION_LOOKUP_INTERFACE, NativePreparedConnection,
};
use bcode_harness_models::{HarnessAuthBinding, HarnessAuthCustody, HarnessInstance};
use bcode_plugin_sdk::{
    NativeServiceContext, RustPlugin, ServiceBridgeRequest, ServiceBridgeResponse,
    ServiceCancellation, ServiceResponse,
};
use managed_open::open_fixture;
use std::sync::Arc;

const MANIFEST: &str = r#"
id = "fixture.native"
name = "Native connection fixture"
version = "0.0.1"
[[services]]
interface_id = "bcode.harness/v3"
name = "Fixture harness"
[[services]]
interface_id = "bcode.harness.native-connection/v1"
name = "Fixture preparation"
[runtime]
type = "native"
abi_version = 5
library = "test"
"#;
#[derive(Default)]
struct Fixture;
impl RustPlugin for Fixture {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        if context.request.interface_id == bcode_harness_models::HARNESS_INTERFACE_ID {
            return open_fixture(&context);
        }
        let response = match context.request.operation.as_str() {
            "discover" => NativePreparationResponse::Candidates {
                candidates: vec![publication().candidate],
            },
            "prepare" => {
                let request: NativePrepareRequest = context.request.payload_json().unwrap();
                NativePreparationResponse::Prepared {
                    request_id: request.request_id,
                    connection: Box::new(publication()),
                }
            }
            _ => NativePreparationResponse::Failed {
                error: NativeConnectionError::Unavailable,
            },
        };
        ServiceResponse::json(&response).unwrap()
    }
}
fn publication() -> NativePreparedConnection {
    let candidate = NativeCandidate {
        candidate_id: "candidate".into(),
        plugin_id: "fixture.native".into(),
        executable: "/fixture/bin/native".into(),
        config_namespace: "/fixture/account".into(),
        installation_id: "fixture-artifact".into(),
    };
    NativePreparedConnection {
        instance: HarnessInstance {
            instance_id: "fixture-instance".into(),
            plugin_id: candidate.plugin_id.clone(),
            executable: candidate.executable.clone(),
            config_namespace: candidate.config_namespace.clone(),
            auth: HarnessAuthBinding {
                provider_id: "fixture".into(),
                profile: "native".into(),
                credential_namespace: candidate.config_namespace.clone(),
                custody: HarnessAuthCustody::NativeManaged,
            },
        },
        candidate,
        protocol_profile: "fixture-v1".into(),
        compatibility_evidence: "fixture-evidence".into(),
        isolation_profile: "fixture-isolated".into(),
    }
}
fn request(instance_id: &str) -> ServiceBridgeRequest {
    ServiceBridgeRequest::InvokeService(bcode_tool::ToolInvocationServiceRequest {
        invocation_id: "fixture-call".into(),
        request_id: "lookup".into(),
        route_id: None,
        interface_id: NATIVE_CONNECTION_LOOKUP_INTERFACE.into(),
        operation: "lookup".into(),
        payload: serde_json::to_value(NativeConnectionLookupRequest::Resolve {
            instance_id: instance_id.into(),
        })
        .unwrap(),
    })
}
fn fixture_state(root: &std::path::Path) -> ServerState {
    let mut state = crate::tests::test_server_state(bcode_session::SessionManager::default());
    state.state_root = root.to_owned();
    let registration = bcode_plugin::StaticBundledPlugin::new(
        MANIFEST,
        bcode_plugin_sdk::static_plugin_vtable!(Fixture, MANIFEST),
    );
    state.plugins = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
        &bcode_plugin::PluginSelection {
            mode: bcode_plugin::PluginSelectionMode::Explicit,
            enabled: std::collections::BTreeSet::from(["fixture.native".into()]),
            disabled: std::collections::BTreeSet::new(),
        },
        &[registration],
    )
    .unwrap();
    state
}
#[tokio::test]
async fn instance_bridge_rechecks_canonical_consent_and_exact_binding_without_auth_provider() {
    let root = tempfile::tempdir().unwrap();
    let prepared = publication();
    let store = NativeConnectionStore::new(root.path());
    store.approve(&prepared.candidate, 0, "approve", 1).unwrap();
    store.publish(&prepared, 1, "publish").unwrap();
    let state = Arc::new(fixture_state(root.path()));
    assert!(state.plugins.auth_provider("fixture").is_none());
    let lookup = bridge_for_instance(Arc::clone(&state), prepared.instance.clone());
    let response = lookup
        .request(request("fixture-instance"), ServiceCancellation::default())
        .unwrap();
    let ServiceBridgeResponse::Service(bcode_tool::ToolInvocationServiceResolution::Responded {
        payload,
    }) = response
    else {
        panic!("lookup response");
    };
    let response: NativeConnectionLookupResponse = serde_json::from_value(payload).unwrap();
    assert_eq!(response.records[0].prepared.as_ref(), Some(&prepared));
    assert!(
        lookup
            .request(request("other-instance"), ServiceCancellation::default())
            .is_err()
    );
    let mut changed = prepared.instance;
    changed.executable = "/fixture/other".into();
    let wrong = bridge_for_instance(Arc::clone(&state), changed);
    assert!(
        wrong
            .request(request("fixture-instance"), ServiceCancellation::default())
            .is_err()
    );
    let ServiceBridgeRequest::InvokeService(wire_request) = request("fixture-instance") else {
        panic!("typed service request");
    };
    assert!(decode_lookup_bridge(&wire_request).is_ok());
    let mut invalid = wire_request.clone();
    invalid.route_id = Some("untrusted-route".into());
    assert!(decode_lookup_bridge(&invalid).is_err());
    invalid = wire_request.clone();
    invalid.operation = "prepare".into();
    assert!(decode_lookup_bridge(&invalid).is_err());
    invalid = wire_request;
    invalid.payload = serde_json::json!({"kind":"resolve", "instance_id":"x".repeat(257)});
    assert!(decode_lookup_bridge(&invalid).is_err());
    let revoked = store.revoke("candidate", 2, "revoke").unwrap();
    assert!(!revoked.approved);
    assert!(
        lookup
            .request(request("fixture-instance"), ServiceCancellation::default())
            .is_err()
    );
    assert!(
        super::lookup(
            &state,
            "foreign.plugin",
            NativeConnectionLookupRequest::Resolve {
                instance_id: "fixture-instance".into()
            }
        )
        .is_err()
    );
    drop(state);
}
