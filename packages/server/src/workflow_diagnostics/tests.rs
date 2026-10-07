use super::*;

fn fixture() -> (
    WorkflowDiagnosticArtifactRequest,
    bcode_workflow_store::DispatchReceipt,
    Vec<u8>,
) {
    let request = WorkflowDiagnosticArtifactRequest {
        version: 1,
        run_id: "run".into(),
        dispatch_identity: "dispatch".into(),
        artifact_id: "diagnostic".into(),
        offset: 0,
        length: 100,
    };
    let bytes = b"private synthetic diagnostic".to_vec();
    let uri = format!(
        "bcode-artifact://invocation/{}/{}",
        hex::encode(Sha256::digest(request.dispatch_identity.as_bytes())),
        hex::encode(Sha256::digest(request.artifact_id.as_bytes()))
    );
    let receipt = bcode_workflow_store::DispatchReceipt {
        run_id: request.run_id.clone(),
        node_id: "node".into(),
        activation_id: "activation".into(),
        attempt: 1,
        dispatch_identity: request.dispatch_identity.clone(),
        admitted_at_ms: 0,
        receipt: serde_json::json!({"diagnostics": {
            "version":1,"parent_session_id":SessionId::new(),
            "artifacts":[{"artifact_id":request.artifact_id,"byte_len":bytes.len(),"reference":{
                "uri":uri,"byte_len":bytes.len(),"sha256":hex::encode(Sha256::digest(&bytes)),
                "content_type":"application/json","metadata":{"sensitive":true}
            }}]
        }}),
    };
    (request, receipt, bytes)
}

#[test]
fn overflow_coverage_is_unknown_for_historical_receipts_and_explicit_for_new_receipts() {
    let (request, mut receipt, _) = fixture();
    assert_eq!(
        summaries(&receipt, &request.run_id, &request.dispatch_identity)
            .unwrap()
            .omitted_artifacts,
        None
    );
    receipt.receipt["diagnostics"]["omitted_artifacts"] = serde_json::json!(0);
    assert_eq!(
        summaries(&receipt, &request.run_id, &request.dispatch_identity)
            .unwrap()
            .omitted_artifacts,
        Some(0)
    );
    receipt.receipt["diagnostics"]["omitted_artifacts"] = serde_json::json!(1);
    assert!(matches!(
        summaries(&receipt, &request.run_id, &request.dispatch_identity),
        Err(Error::VerificationFailed)
    ));
    receipt.receipt["diagnostics"]["omitted_artifacts"] = serde_json::json!("future-count");
    assert!(matches!(
        summaries(&receipt, &request.run_id, &request.dispatch_identity),
        Err(Error::VerificationFailed)
    ));
}

#[test]
fn selects_only_exact_receipt_and_verifies_published_digest() {
    let (request, receipt, bytes) = fixture();
    let selected = select(&receipt, &request).unwrap();
    assert!(verify_bytes(&selected, &bytes).is_ok());
    assert_eq!(
        verify_bytes(&selected, b"changed"),
        Err(Error::VerificationFailed)
    );
    let mut wrong = request.clone();
    wrong.dispatch_identity = "other".into();
    assert!(matches!(
        select(&receipt, &wrong),
        Err(Error::VerificationFailed)
    ));
    wrong = request;
    wrong.artifact_id = "missing".into();
    assert!(matches!(select(&receipt, &wrong), Err(Error::Unavailable)));
}

#[test]
fn historical_missing_digest_and_missing_receipt_metadata_are_unavailable() {
    let (request, mut receipt, _) = fixture();
    receipt.receipt["diagnostics"]["artifacts"][0]["reference"]
        .as_object_mut()
        .unwrap()
        .remove("sha256");
    assert!(matches!(
        select(&receipt, &request),
        Err(Error::Unavailable)
    ));
    receipt.receipt = serde_json::json!({});
    assert!(matches!(
        select(&receipt, &request),
        Err(Error::Unavailable)
    ));
}

#[test]
fn rejects_future_versions_duplicates_and_forged_capabilities() {
    let (request, mut receipt, _) = fixture();
    receipt.receipt["diagnostics"]["version"] = serde_json::json!(2);
    assert!(matches!(
        select(&receipt, &request),
        Err(Error::UnsupportedVersion)
    ));
    let (_, mut receipt, _) = fixture();
    let artifact = receipt.receipt["diagnostics"]["artifacts"][0].clone();
    receipt.receipt["diagnostics"]["artifacts"]
        .as_array_mut()
        .unwrap()
        .push(artifact);
    assert!(matches!(
        select(&receipt, &request),
        Err(Error::VerificationFailed)
    ));
    let (_, mut receipt, _) = fixture();
    receipt.receipt["diagnostics"]["artifacts"][0]["reference"]["uri"] =
        serde_json::json!("/private/path");
    assert!(matches!(
        select(&receipt, &request),
        Err(Error::VerificationFailed)
    ));
}

#[test]
fn discovery_returns_only_recorded_public_metadata_and_validates_every_reference() {
    let (request, mut receipt, _) = fixture();
    let listed = summaries(&receipt, &request.run_id, &request.dispatch_identity).unwrap();
    assert_eq!(listed.artifacts.len(), 1);
    assert_eq!(listed.artifacts[0].artifact_id, request.artifact_id);
    let value = serde_json::to_value(&listed).unwrap();
    let fields = value["artifacts"][0].as_object().unwrap();
    assert_eq!(fields.len(), 4);
    assert!(!fields.contains_key("uri"));
    assert!(!fields.contains_key("metadata"));
    let mut invalid = receipt.receipt["diagnostics"]["artifacts"][0].clone();
    invalid["artifact_id"] = serde_json::json!("different");
    receipt.receipt["diagnostics"]["artifacts"]
        .as_array_mut()
        .unwrap()
        .push(invalid);
    let listed = summaries(&receipt, &request.run_id, &request.dispatch_identity).unwrap();
    assert_eq!(listed.artifacts.len(), 1);
    assert_eq!(listed.unavailable.len(), 1);
    // Invalid siblings do not prevent an exact valid artifact from being exported.
    assert!(select(&receipt, &request).is_ok());
}

#[test]
fn empty_recorded_discovery_is_not_a_missing_receipt() {
    let (request, mut receipt, _) = fixture();
    receipt.receipt["diagnostics"]["artifacts"] = serde_json::json!([]);
    assert!(
        summaries(&receipt, &request.run_id, &request.dispatch_identity)
            .unwrap()
            .artifacts
            .is_empty()
    );
    receipt.receipt = serde_json::json!({});
    assert!(matches!(
        summaries(&receipt, &request.run_id, &request.dispatch_identity),
        Err(Error::Unavailable)
    ));
}

#[test]
fn range_and_artifact_bounds_fail_closed() {
    let (mut request, mut receipt, _) = fixture();
    request.length = MAX_WORKFLOW_DIAGNOSTIC_RANGE_BYTES + 1;
    assert_eq!(validate(&request), Err(Error::InvalidRequest));
    request.length = 0;
    assert_eq!(validate(&request), Err(Error::InvalidRequest));
    request.length = 1;
    receipt.receipt["diagnostics"]["artifacts"][0]["byte_len"] =
        serde_json::json!(MAX_WORKFLOW_DIAGNOSTIC_ARTIFACT_BYTES + 1);
    assert!(matches!(
        select(&receipt, &request),
        Err(Error::VerificationFailed)
    ));
}
