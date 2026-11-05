#[cfg(unix)]
mod ipc;

use super::*;
use bcode_harness_models::{
    HarnessPlacement, HarnessPreparedSession, HarnessSessionTarget, HarnessSubmission,
};

fn snapshot(session_id: SessionId) -> ManagedSessionTargetSnapshot {
    let instance = serde_json::from_value(serde_json::json!({
        "instance_id": "primary", "plugin_id": "example.native",
        "executable": "/native", "config_namespace": "/native/config",
        "auth": {"provider_id": "example", "profile": "native", "credential_namespace": "/native/config", "custody": "native_managed"}
    })).unwrap();
    ManagedSessionTargetSnapshot {
        session_id,
        instance,
        target: HarnessSessionTarget::Prepared(HarnessPreparedSession {
            open_id: "retained-open".into(),
            session_id: session_id.to_string(),
            instance_id: "primary".into(),
            generation: 1,
            native_version: "verified-version".into(),
            requested_native_session_id: None,
        }),
    }
}

#[test]
fn terminal_auth_discovery_requires_unique_exact_bound_plugin_declaration() {
    let service =
        |plugin: &str, interface: &str| bcode_plugin_sdk::application::PluginServiceSummary {
            plugin_id: plugin.into(),
            interface_id: interface.into(),
            name: None,
            description: None,
            workflow_blocks: Vec::new(),
        };
    let generic = bcode_harness_models::TERMINAL_AUTH_INTERFACE_ID;
    assert_eq!(select_terminal_auth_interface("owner", &[]).unwrap(), None);
    let unrelated = [
        service("foreign", generic),
        service("owner", "vendor.handoff/v1"),
        service("owner", "bcode.harness.terminal-auth/v2"),
    ];
    assert_eq!(
        select_terminal_auth_interface("owner", &unrelated).unwrap(),
        None
    );
    let mut services = unrelated.to_vec();
    services.push(service("owner", generic));
    assert_eq!(
        select_terminal_auth_interface("owner", &services).unwrap(),
        Some(generic.to_owned())
    );
    services.push(service("owner", generic));
    assert!(select_terminal_auth_interface("owner", &services).is_err());
}

#[test]
fn binding_requires_exact_canonical_owner_without_promoting_prepared() {
    let owner = SessionId::new();
    let mut evidence = snapshot(owner);
    verify_binding(owner, &evidence).unwrap();
    assert!(matches!(evidence.target, HarnessSessionTarget::Prepared(_)));
    assert!(verify_binding(SessionId::new(), &evidence).is_err());
    evidence.instance.instance_id = "other".into();
    assert!(verify_binding(owner, &evidence).is_err());
    let mut evidence = snapshot(owner);
    if let HarnessSessionTarget::Prepared(prepared) = &mut evidence.target {
        prepared.session_id = SessionId::new().to_string();
    }
    assert!(verify_binding(owner, &evidence).is_err());
}

#[test]
fn retained_input_is_confined_to_bound_session_and_submit_operation() {
    let owner = SessionId::new();
    let evidence = snapshot(owner);
    let request = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "submission".into(),
        instance: evidence.instance,
        kind: HarnessRequestKind::Submit {
            target: evidence.target,
            submission: HarnessSubmission {
                submission_id: "submission".into(),
                attempt_id: "attempt".into(),
                session_id: owner.to_string(),
                generation: 1,
                text: "explicit next-turn input".into(),
                placement: HarnessPlacement::NextTurn,
            },
        },
    };
    let retained = ExternalHarnessRequest::from_request(&request).unwrap();
    verify_submission(owner, &retained).unwrap();
    assert!(verify_submission(SessionId::new(), &retained).is_err());
    let control = HarnessControlRequest {
        target: snapshot(owner).target,
        mutation_id: "interrupt".into(),
    };
    let interrupt = HarnessRequest {
        request_id: "interrupt".into(),
        kind: HarnessRequestKind::Interrupt(control),
        ..request
    };
    let retained = ExternalHarnessRequest::from_request(&interrupt).unwrap();
    assert!(verify_submission(owner, &retained).is_err());
}

#[test]
fn foreign_and_global_events_never_escape_session_attachment() {
    let owner = SessionId::new();
    assert!(event_matches(
        owner,
        &Event::SessionViewResyncRequired { session_id: owner }
    ));
    assert!(!event_matches(
        owner,
        &Event::SessionViewResyncRequired {
            session_id: SessionId::new()
        }
    ));
    assert!(!event_matches(
        owner,
        &Event::SessionCatalogUpdated { revision: 42 }
    ));
}
