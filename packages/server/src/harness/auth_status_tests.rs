//! Real client/host readiness routing without native execution or login authority.
use super::*;
use bcode_harness_models::*;
use bcode_plugin_sdk::prelude::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

const MANIFEST: &str = r#"
id = "test.status-only"
name = "Status-only auth fixture"
version = "0.0.1"
[[services]]
class = "service"
interface_id = "bcode.harness/v3"
name = "Status-only native harness"
[concurrency]
type = "exclusive"
[runtime]
type = "native"
abi_version = 5
library = "unused-status-fixture"
"#;
static STATUS_CALLS: AtomicUsize = AtomicUsize::new(0);
static MUTATION_CALLS: AtomicUsize = AtomicUsize::new(0);
#[derive(Default)]
struct StatusOnly;
impl RustPlugin for StatusOnly {
    fn invoke_service(&mut self, context: NativeServiceContext) -> ServiceResponse {
        let request: HarnessRequest = serde_json::from_slice(&context.request.payload).unwrap();
        let kind = if matches!(
            request.kind,
            HarnessRequestKind::AuthStatus { verify: false }
        ) {
            STATUS_CALLS.fetch_add(1, Ordering::SeqCst);
            HarnessServiceResponseKind::AuthStatus(
                bcode_provider_auth_models::managed::ManagedAuthStatus {
                    namespace: bcode_provider_auth_models::managed::AuthNamespace {
                        plugin_id: request.instance.plugin_id.clone(),
                        credential_namespace: request.instance.auth.credential_namespace.clone(),
                    },
                    custody: request.instance.auth.custody,
                    readiness: bcode_provider_auth_models::managed::AuthReadiness::Ready,
                },
            )
        } else {
            MUTATION_CALLS.fetch_add(1, Ordering::SeqCst);
            HarnessServiceResponseKind::Failure(HarnessFailure::Unsupported)
        };
        ServiceResponse::json(&HarnessServiceResponse {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: request.request_id.clone(),
            operation: request.kind.operation(),
            kind,
        })
        .unwrap()
    }
}

async fn retained_status_fixture(
    root: &std::path::Path,
) -> (
    bcode_session::SessionManager,
    bcode_session_models::SessionId,
) {
    let sessions = bcode_session::SessionManager::persistent_with_metrics_and_lease_owner(
        root,
        bcode_metrics::MetricsRegistry::disabled(),
        bcode_session::lease::SessionLeaseOwnerContext {
            build_fingerprint: Some("status-fixture-artifact".into()),
            daemon_instance_id: Some("prior-owner".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let session = sessions
        .create_session(None, root.to_path_buf())
        .await
        .unwrap();
    let owner = bcode_session_models::external_harness::ExternalHarnessOwner {
        session_id: session.id,
        generation: 1,
        artifact_id: "status-fixture-artifact".into(),
        daemon_id: "prior-owner".into(),
    };
    let instance = HarnessInstance {
        instance_id: "primary".into(),
        plugin_id: "test.status-only".into(),
        executable: "/not-executed".into(),
        config_namespace: "/native/config".into(),
        auth: HarnessAuthBinding {
            provider_id: "native".into(),
            profile: "native".into(),
            credential_namespace: "/native/config".into(),
            custody: HarnessAuthCustody::NativeManaged,
        },
    };
    let capability = HarnessCapability {
        support: HarnessSupportLevel::Unsupported,
        constraints: vec![
            "Retained pre-correction capability; status must reinspect the enabled plugin".into(),
        ],
    };
    let capabilities = HarnessCapabilities {
        resume: capability.clone(),
        steering: capability.clone(),
        approval: capability.clone(),
        questions: capability.clone(),
        assistance: capability.clone(),
        interrupt: capability.clone(),
        authoritative_settlement: capability.clone(),
        background_continuation: capability.clone(),
        authentication: capability,
    };
    let open = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "fixture-open".into(),
        instance: instance.clone(),
        kind: HarnessRequestKind::Open(HarnessOpenRequest {
            open_id: "fixture-open".into(),
            session_id: session.id.to_string(),
            generation: 1,
            working_directory: root.to_str().unwrap().into(),
            resume: None,
            parent_assistance: None,
        }),
    };
    sessions
        .prepare_external_harness_open(&owner, &open, capabilities, None)
        .await
        .unwrap();
    sessions
        .claim_external_harness_request(&owner, &open)
        .await
        .unwrap();
    sessions
        .complete_external_harness_request(
            &owner,
            &open,
            &HarnessServiceResponse {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: open.request_id.clone(),
                operation: HarnessOperation::Open,
                kind: HarnessServiceResponseKind::Prepared(HarnessPreparedSession {
                    open_id: "fixture-open".into(),
                    session_id: session.id.to_string(),
                    instance_id: "primary".into(),
                    generation: 1,
                    native_version: "fixture".into(),
                    requested_native_session_id: None,
                }),
            },
        )
        .await
        .unwrap();
    (sessions, session.id)
}

#[tokio::test]
async fn constrained_status_only_readiness_crosses_real_ipc_without_login_or_submission() {
    let root = tempfile::tempdir().unwrap();
    let (sessions, session_id) = retained_status_fixture(root.path()).await;
    let bundle = bcode_plugin::StaticBundledPlugin::new(
        MANIFEST,
        bcode_plugin_sdk::static_plugin_vtable!(StatusOnly, MANIFEST),
    );
    let plugins = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
        &bcode_plugin::PluginSelection::all_enabled(),
        &[bundle],
    )
    .unwrap();
    let state = Arc::new(crate::tests::test_server_state_with_plugins(
        sessions, plugins,
    ));
    install_canonical_backend(&state).unwrap();
    let sockets = crate::tests::test_socket_dir().unwrap();
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(sockets.path().join("status.sock"));
    let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).unwrap();
    let serving = Arc::clone(&state);
    let server = tokio::spawn(async move {
        crate::handle_client(listener.accept().await.unwrap(), serving)
            .await
            .unwrap();
    });
    let client = bcode_client::BcodeClient::new(endpoint).with_runtime_context(Some(
        bcode_ipc::ClientRuntimeContext {
            working_directory: Some(root.path().to_path_buf()),
            ..Default::default()
        },
    ));
    let mut connection = client.connect("status-only-preflight").await.unwrap();
    let input = bcode_ipc::harness::ManagedSessionAuthRequest {
        session_id,
        instance_id: "primary".into(),
        handle: None,
        input: None,
        sensitive_interaction: false,
    };
    let status_before = STATUS_CALLS.load(Ordering::SeqCst);
    let response = connection
        .managed_session_auth(bcode_ipc::ManagedSessionRequest {
            schema_version: bcode_ipc::harness::MANAGED_SESSION_VERSION,
            operation: bcode_ipc::harness::ManagedSessionOperation::AuthStatus,
            request_json: serde_json::to_string(&input).unwrap(),
        })
        .await
        .unwrap();
    let status: bcode_provider_auth_models::managed::ManagedAuthStatus =
        serde_json::from_str(&response.response_json).unwrap();
    assert_eq!(
        status.readiness,
        bcode_provider_auth_models::managed::AuthReadiness::Ready
    );
    assert_eq!(status.namespace.credential_namespace, "/native/config");
    assert_eq!(STATUS_CALLS.load(Ordering::SeqCst), status_before + 1);
    // A readable constrained capability does not manufacture method/inactivity proof.
    let denied = connection
        .managed_session_auth(bcode_ipc::ManagedSessionRequest {
            schema_version: bcode_ipc::harness::MANAGED_SESSION_VERSION,
            operation: bcode_ipc::harness::ManagedSessionOperation::AuthBegin,
            request_json: serde_json::to_string(&input).unwrap(),
        })
        .await;
    assert!(matches!(
        denied,
        Err(bcode_client::ClientError::Server { .. })
    ));
    assert_eq!(MUTATION_CALLS.load(Ordering::SeqCst), 0);
    drop(connection);
    tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    drop(state);
}
