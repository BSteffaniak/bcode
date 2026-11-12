use super::*;
use crate::HarnessAuthBinding;
fn fixture() -> NativePreparedConnection {
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
#[test]
fn preparation_rejects_wrong_owner_custody_namespace_and_future_schema() {
    let prepared = fixture();
    assert!(prepared.validate().is_ok());
    let mut invalid = prepared.clone();
    invalid.instance.plugin_id = "foreign".into();
    assert!(invalid.validate().is_err());
    invalid = prepared.clone();
    invalid.instance.auth.custody = HarnessAuthCustody::VaultManaged;
    assert!(invalid.validate().is_err());
    invalid = prepared.clone();
    invalid.instance.auth.credential_namespace = "/foreign".into();
    assert!(invalid.validate().is_err());
    let mut request = NativePrepareRequest {
        schema_version: 1,
        request_id: "prepare".into(),
        candidate: prepared.candidate,
    };
    assert!(request.validate().is_ok());
    request.schema_version = 2;
    assert_eq!(
        request.validate(),
        Err(HarnessContractError::UnsupportedVersion)
    );
}
#[test]
fn revoked_publication_and_inline_credentials_fail_closed() {
    let prepared = fixture();
    let mut record = NativeConnectionRecord {
        candidate: prepared.candidate.clone(),
        revision: 1,
        approved: true,
        approved_at_unix: 1,
        prepared: Some(prepared),
    };
    assert!(record.validate().is_ok());
    record.approved = false;
    assert!(record.validate().is_err());
    let mut encoded = serde_json::to_value(record).unwrap();
    encoded["api_key"] = serde_json::json!("synthetic-secret");
    assert!(serde_json::from_value::<NativeConnectionRecord>(encoded).is_err());
}
