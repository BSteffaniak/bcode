use super::*;
use bcode_model::{ProviderErrorCategory, ProviderErrorSource};

#[test]
fn structured_provider_cause_survives_collection_and_response_decoding() {
    let error = ProviderError {
        code: "refresh_blocked".into(),
        category: ProviderErrorCategory::Auth,
        message: "Refresh unavailable; access_token=private-token".into(),
        retryable: false,
        provider_message: Some("private provider body".into()),
        failure: None,
        request_id: Some("request-17".into()),
        diagnostic_context: Box::new(std::collections::BTreeMap::from([(
            "http_status".into(),
            "401".into(),
        )])),
        sources: Box::new(vec![ProviderErrorSource {
            source: "auth-host".into(),
            code: Some("custody_unavailable".into()),
            message: Some("host denied refresh".into()),
        }]),
        retry: Some(Box::new(bcode_model::ProviderRetryHint {
            retry_after_ms: Some(1200),
            retry_at_unix: None,
            source: Some("retry-after".into()),
        })),
    };
    let event = bcode_model::ProviderTurnEvent::Error {
        error: error.clone(),
    };
    let collected = super::super::Collected::default()
        .apply(event.clone())
        .unwrap_err();
    assert!(matches!(&collected, GenerationError::Provider(value) if **value == error));
    let mut response = bcode_plugin_sdk::ServiceResponse::error("startup", "startup failed");
    response.payload = serde_json::to_vec(&event).unwrap();
    let decoded = super::super::decode_response::<serde_json::Value>(response).unwrap_err();
    assert!(matches!(&decoded, GenerationError::Provider(value) if **value == error));
    let diagnostic = decoded.diagnostic();
    assert_eq!(diagnostic["error"]["code"], "refresh_blocked");
    assert_eq!(diagnostic["error"]["category"], "auth");
    assert_eq!(diagnostic["error"]["retry"]["retry_after_ms"], 1200);
    assert_eq!(diagnostic["error"]["request_id"], "request-17");
    assert_eq!(
        diagnostic["error"]["diagnostic_context"]["http_status"],
        "401"
    );
    assert_eq!(
        diagnostic["error"]["sources"][0]["code"],
        "custody_unavailable"
    );
    assert!(!diagnostic.to_string().contains("private-token"));
    assert!(!diagnostic.to_string().contains("private provider body"));
    let combined = decoded.secondary(GenerationError::from("usage persistence failed"));
    assert_eq!(
        combined.diagnostic()["primary"]["error"]["code"],
        "refresh_blocked"
    );
    assert_eq!(
        combined.diagnostic()["secondary"]["message"],
        "usage persistence failed"
    );
}

#[test]
fn service_response_failure_is_not_treated_as_success_or_empty_output() {
    let response = bcode_plugin_sdk::ServiceResponse::error(
        "auth_refresh_unavailable",
        "durable refresh coordination unavailable",
    );
    let error = super::super::decode_response::<serde_json::Value>(response).unwrap_err();
    assert_eq!(error.diagnostic()["code"], "auth_refresh_unavailable");
    assert_eq!(
        error.diagnostic()["message"],
        "durable refresh coordination unavailable"
    );
}
