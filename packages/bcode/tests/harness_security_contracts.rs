//! Security acceptance through public auth coordination and IPC decode APIs.
//! These are application-contract checks, not claims of native auth CLI E2E.
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bcode_provider_auth::managed::{
    AuthWaiter, ManagedAuthAction, ManagedAuthCoordinator, ManagedAuthStep,
};
use bcode_provider_auth_models::managed::{
    AuthCustodyMode, AuthNamespace, AuthReadiness, AuthSensitiveInput, SensitiveAuthInteraction,
};
use std::time::{Duration, Instant};

const SECRET: &str = "fixture-secret-DO-NOT-PERSIST";
fn waiter(session: &str) -> AuthWaiter {
    AuthWaiter {
        session_id: session.into(),
        generation: 7,
        ownership_fence: "daemon-owner-7".into(),
    }
}
fn namespace() -> AuthNamespace {
    AuthNamespace {
        plugin_id: "fixture.native".into(),
        credential_namespace: "isolated-native-home".into(),
    }
}

#[test]
fn shared_native_auth_waiters_do_not_duplicate_login_or_cancel_other_session() {
    let now = Instant::now();
    let mut auth =
        ManagedAuthCoordinator::new("boot-1".into(), 4, 4, Duration::from_secs(60)).unwrap();
    let first = auth
        .begin_or_join(
            namespace(),
            AuthCustodyMode::NativeManaged,
            "native-login".into(),
            waiter("session-a"),
            now,
        )
        .unwrap();
    let command = first.command.unwrap();
    let handle = first.snapshot.handle;
    let other = auth
        .begin_or_join(
            namespace(),
            AuthCustodyMode::NativeManaged,
            "native-login".into(),
            waiter("session-b"),
            now,
        )
        .unwrap();
    assert!(
        other.command.is_none(),
        "one native login, not one per waiting session"
    );
    assert_eq!(other.snapshot.handle, handle);
    auth.accept(
        &command.ticket,
        ManagedAuthStep {
            terminal: false,
            readiness: AuthReadiness::LoginRequired,
            state: Some(SECRET.into()),
            interaction: Some(SensitiveAuthInteraction::Input {
                request_id: "hidden-step-1".into(),
            }),
        },
        now,
    )
    .unwrap();
    assert!(
        auth.cancel_waiter(&handle, &waiter("session-a"))
            .unwrap()
            .is_none()
    );
    assert!(auth.snapshot(&handle, &waiter("session-a")).is_err());
    let snapshot = auth.snapshot(&handle, &waiter("session-b")).unwrap();
    assert!(!format!("{snapshot:?}").contains(SECRET));
    let mut stale = waiter("session-b");
    stale.generation = 6;
    assert!(auth.continue_flow(&handle, &stale, None, now).is_err());
    let continuation = auth
        .continue_flow(
            &handle,
            &waiter("session-b"),
            Some(AuthSensitiveInput::new("hidden-step-1".into(), SECRET.into()).unwrap()),
            now,
        )
        .unwrap();
    assert!(!format!("{continuation:?}").contains(SECRET));
    assert!(matches!(
        continuation.action,
        ManagedAuthAction::Continue(_)
    ));
    assert!(
        auth.continue_flow(&handle, &waiter("session-b"), None, now)
            .is_err()
    );
    // An old callback must not overwrite the current in-flight step.
    assert!(
        auth.accept(
            &command.ticket,
            ManagedAuthStep {
                terminal: true,
                readiness: AuthReadiness::Ready,
                state: None,
                interaction: None,
            },
            now
        )
        .is_err()
    );
    let restarted =
        ManagedAuthCoordinator::new("boot-2".into(), 4, 4, Duration::from_secs(60)).unwrap();
    assert!(
        restarted.snapshot(&handle, &waiter("session-b")).is_err(),
        "daemon restart cannot resurrect callbacks from a former epoch"
    );
}

#[test]
fn managed_ipc_history_rejects_unbounded_and_forged_authority_without_echoing() {
    use bcode_ipc::harness::{ManagedSessionOperation, ManagedSessionRequest};
    let request = ManagedSessionRequest {
        schema_version: 1,
        operation: ManagedSessionOperation::History,
        request_json: serde_json::json!({"session_id":"11111111-1111-4111-8111-111111111111",
            "after":null,"limit":2,"ownership_fence":SECRET})
        .to_string(),
    };
    assert!(request.decode_history().is_err());
    assert!(!format!("{request:?}").contains(SECRET));
    let bounded = ManagedSessionRequest {
        schema_version: 1,
        operation: ManagedSessionOperation::History,
        request_json: serde_json::json!({"session_id":"11111111-1111-4111-8111-111111111111",
            "after":null,"limit":129})
        .to_string(),
    };
    assert!(bounded.decode_history().is_err());
}
