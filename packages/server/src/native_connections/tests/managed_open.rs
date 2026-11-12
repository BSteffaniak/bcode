//! Synthetic native adapter exercised through the real catalog and transport bridges.
use super::*;
use crate::harness::{HarnessPluginTransport, ManagedHarnessTransport};
use bcode_harness_models::*;

pub(super) fn capabilities() -> HarnessCapabilities {
    let supported = HarnessCapability {
        support: HarnessSupportLevel::Supported,
        constraints: vec![],
    };
    HarnessCapabilities {
        resume: supported.clone(),
        steering: supported.clone(),
        approval: supported.clone(),
        questions: supported.clone(),
        assistance: supported.clone(),
        interrupt: supported.clone(),
        authoritative_settlement: supported.clone(),
        background_continuation: supported.clone(),
        authentication: supported,
    }
}

pub(super) fn open_fixture(context: &NativeServiceContext) -> ServiceResponse {
    let request: HarnessRequest = context.request.payload_json().unwrap();
    let ServiceBridgeRequest::InvokeService(mut lookup) = super::request("fixture-instance") else {
        unreachable!();
    };
    lookup.invocation_id = request.request_id.clone();
    let response = context
        .bridge
        .request(&ServiceBridgeRequest::InvokeService(lookup))
        .unwrap();
    let ServiceBridgeResponse::Service(bcode_tool::ToolInvocationServiceResolution::Responded {
        payload,
    }) = response
    else {
        panic!("native lookup was not admitted");
    };
    let records: NativeConnectionLookupResponse = serde_json::from_value(payload).unwrap();
    assert_eq!(
        records.records[0].prepared.as_ref().unwrap().instance,
        request.instance
    );
    let operation = request.kind.operation();
    let kind = match request.kind {
        HarnessRequestKind::Inspect => HarnessServiceResponseKind::Inspection(HarnessInspection {
            native_version: "fixture-v1".into(),
            executable_identity: request.instance.executable,
            config_namespace: request.instance.config_namespace,
            capabilities: capabilities(),
        }),
        HarnessRequestKind::Open(open) => {
            HarnessServiceResponseKind::Opened(HarnessSessionBinding {
                schema_version: HARNESS_SCHEMA_VERSION,
                session_id: open.session_id,
                instance_id: request.instance.instance_id,
                native_session_id: "synthetic-native".into(),
                native_version: "fixture-v1".into(),
                generation: open.generation,
            })
        }
        HarnessRequestKind::Observe(query) => {
            HarnessServiceResponseKind::Observations(HarnessObservationPage {
                retention: None,
                hydration_requests: vec![],
                inventory_page: None,
                observations: vec![HarnessObservation {
                    observation_id: "synthetic-approval-observation".into(),
                    session_id: query.target.session_id().into(),
                    generation: query.target.generation(),
                    native_correlations: vec![],
                    completeness: HarnessCompleteness::Complete,
                    kind: HarnessObservationKind::Interaction {
                        interaction: HarnessInteraction {
                            request_id: "synthetic-approval".into(),
                            session_id: query.target.session_id().into(),
                            generation: query.target.generation(),
                            kind: HarnessInteractionKind::Approval,
                            prompt: "Allow synthetic native operation?".into(),
                            scope: "synthetic.native.operation".into(),
                            choices: vec![],
                        },
                    },
                }],
                next_after: Some("synthetic-approval-observation".into()),
                completeness: HarnessCompleteness::Complete,
                status: HarnessExecutionStatus {
                    connectivity: HarnessConnectivity::Connected,
                    activity: HarnessActivity::Running,
                    attention: HarnessAttention::None,
                    continuation: HarnessContinuation::Available,
                    completeness: HarnessCompleteness::Complete,
                },
            })
        }
        HarnessRequestKind::Respond { response, .. } => HarnessServiceResponseKind::Acknowledged {
            mutation_id: response.response_id,
        },
        HarnessRequestKind::Interrupt(control) | HarnessRequestKind::Close(control) => {
            HarnessServiceResponseKind::Acknowledged {
                mutation_id: control.mutation_id,
            }
        }
        _ => panic!("unexpected synthetic lifecycle operation"),
    };
    ServiceResponse::json(&HarnessServiceResponse {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: request.request_id,
        operation,
        kind,
    })
    .unwrap()
}

pub(super) fn catalog_state(root: &std::path::Path) -> ServerState {
    let mut state = fixture_state(root);
    let mut registrations = bcode_bundled_plugins::static_bundled_plugins();
    registrations.push(bcode_plugin::StaticBundledPlugin::new(
        MANIFEST,
        bcode_plugin_sdk::static_plugin_vtable!(Fixture, MANIFEST),
    ));
    state.plugins = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
        &bcode_plugin::PluginSelection {
            mode: bcode_plugin::PluginSelectionMode::Explicit,
            enabled: std::collections::BTreeSet::from([
                "fixture.native".into(),
                "bcode.harness".into(),
            ]),
            disabled: std::collections::BTreeSet::new(),
        },
        &registrations,
    )
    .unwrap();
    state
}

#[tokio::test]
async fn consent_publication_restart_catalog_and_managed_open() {
    let root = tempfile::tempdir().unwrap();
    let state = catalog_state(root.path());
    pipeline::approve(&state).await;
    assert!(matches!(
        execute(&state, pipeline::prepare(1)).await,
        NativeConnectionResponse::Record { .. }
    ));
    drop(state);
    let state = Arc::new(catalog_state(root.path()));
    let catalog = crate::plugin_operations::call_service(
        &state,
        "bcode.harness.configuration/v1",
        "catalog".into(),
        b"{}".to_vec(),
    )
    .await
    .unwrap();
    let catalog: serde_json::Value = serde_json::from_slice(&catalog.payload).unwrap();
    let instance: HarnessInstance =
        serde_json::from_value(catalog["instances"]["fixture-instance"].clone()).unwrap();
    assert_eq!(instance, publication().instance);
    let transport = HarnessPluginTransport::new(Arc::clone(&state));
    let request = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "synthetic-open".into(),
        instance,
        kind: HarnessRequestKind::Open(HarnessOpenRequest {
            open_id: "synthetic-open".into(),
            session_id: bcode_session_models::SessionId::new().to_string(),
            generation: 1,
            working_directory: root.path().to_str().unwrap().into(),
            resume: None,
            parent_assistance: None,
        }),
    };
    let response = transport.invoke(&request).await.unwrap();
    drop(transport);
    response.validate_for(&request).unwrap();
    assert!(matches!(
        response.kind,
        HarnessServiceResponseKind::Opened(_)
    ));
    NativeConnectionStore::new(root.path())
        .revoke("candidate", 2, "revoke-after-open")
        .unwrap();
    let catalog = crate::plugin_operations::call_service(
        &state,
        "bcode.harness.configuration/v1",
        "catalog".into(),
        b"{}".to_vec(),
    )
    .await
    .unwrap();
    assert_eq!(catalog.payload, Vec::<u8>::new());
    assert_eq!(catalog.error.unwrap().code, "configuration_disabled");
    drop(state);
}
