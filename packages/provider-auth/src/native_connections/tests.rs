use super::*;
use bcode_harness_models::{HarnessAuthBinding, HarnessAuthCustody, HarnessInstance};

fn candidate() -> NativeCandidate {
    NativeCandidate {
        candidate_id: "candidate".into(),
        plugin_id: "fixture.native".into(),
        executable: "/fixture/bin/native".into(),
        config_namespace: "/fixture/home".into(),
        installation_id: "artifact-digest-fixture".into(),
    }
}
fn prepared() -> NativePreparedConnection {
    let candidate = candidate();
    NativePreparedConnection {
        instance: HarnessInstance {
            instance_id: "native-fixture".into(),
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
        protocol_profile: "fixture-protocol-v1".into(),
        compatibility_evidence: "fixture-evidence".into(),
        isolation_profile: "fixture-isolation-v1".into(),
    }
}
#[test]
fn consent_publish_restart_revoke_and_conflicting_retry() {
    let root = tempfile::tempdir().unwrap();
    let store = NativeConnectionStore::new(root.path());
    assert_eq!(store.list().unwrap(), Vec::new());
    assert!(!root.path().join("auth").exists());
    assert_eq!(
        store.publish(&prepared(), 0, "unapproved"),
        Err(NativeConnectionError::NotApproved)
    );
    let approved = store.approve(&candidate(), 0, "approve", 100).unwrap();
    assert_eq!(
        store.approve(&candidate(), 0, "approve", 200).unwrap(),
        approved
    );
    assert!(approved.prepared.is_none());
    let publication = store.publish(&prepared(), 1, "publish").unwrap();
    assert_eq!(
        store.publish(&prepared(), 1, "publish").unwrap(),
        publication
    );
    let restarted = NativeConnectionStore::new(root.path());
    assert_eq!(restarted.list().unwrap(), vec![publication]);
    let revoked = restarted.revoke("candidate", 2, "revoke").unwrap();
    assert!(!revoked.approved && revoked.prepared.is_none());
    assert_eq!(
        restarted.publish(&prepared(), 3, "republish"),
        Err(NativeConnectionError::NotApproved)
    );
    assert_eq!(
        restarted.approve(&candidate(), 3, "revoke", 300),
        Err(NativeConnectionError::Conflict)
    );
    assert_eq!(restarted.revoke("candidate", 2, "revoke").unwrap(), revoked);
}
#[test]
fn drift_duplicate_instance_and_future_state_preserve_authority() {
    let root = tempfile::tempdir().unwrap();
    let store = NativeConnectionStore::new(root.path());
    store.approve(&candidate(), 0, "approve", 1).unwrap();
    let mut drift = prepared();
    drift.candidate.installation_id = "changed".into();
    assert_eq!(
        store.publish(&drift, 1, "drift"),
        Err(NativeConnectionError::Conflict)
    );
    store.publish(&prepared(), 1, "publish").unwrap();
    let mut other = prepared();
    other.candidate.candidate_id = "other".into();
    store
        .approve(&other.candidate, 0, "approve-other", 1)
        .unwrap();
    assert_eq!(
        store.publish(&other, 1, "duplicate"),
        Err(NativeConnectionError::Conflict)
    );
    let path = root.path().join("auth/native-connections.json");
    let future = br#"{"schema_version":99,"entries":{}}"#;
    std::fs::write(&path, future).unwrap();
    assert_eq!(store.list(), Err(NativeConnectionError::StateUnavailable));
    assert_eq!(
        store.approve(&candidate(), 0, "future", 1),
        Err(NativeConnectionError::StateUnavailable)
    );
    assert_eq!(std::fs::read(path).unwrap(), future);
}
