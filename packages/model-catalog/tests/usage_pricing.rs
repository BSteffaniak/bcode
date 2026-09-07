//! Regression tests for caller-supplied usage valuations.

use bcode_model_catalog::{ModelCatalog, price_session_usage};
use bcode_session_models::{SessionCostEstimate, SessionTokenUsage};

#[test]
fn supplied_snapshot_prices_recorded_model_not_embedded_cost() {
    let mut document = ModelCatalog::load_bundled().unwrap().document().clone();
    let entry = document
        .providers
        .get_mut("openai")
        .unwrap()
        .models
        .get_mut("gpt-6-astra")
        .unwrap();
    entry.deployments.clear();
    entry.pricing = Some(
        serde_json::from_value(serde_json::json!({
            "currency":"USD", "unit":"per_million_tokens", "input_micros":1_000_000,
            "cached_input_micros":100_000,"output_micros":2_000_000,"revision":"supplied"
        }))
        .unwrap(),
    );
    let usage: SessionTokenUsage = serde_json::from_value(serde_json::json!({
        "request_id":"attempt","observation_id":"attempt:usage","terminal":true,
        "request":{"provider_plugin_id":"bcode.openai-compatible","effective_model_id":"gpt-6-astra",
          "request_id":"attempt","model_turn_id":"turn","round":0,"request_fingerprint":"f","context_epoch":0},
        "catalog_provider_id":"openai","catalog_entry_id":"gpt-6-astra",
        "input_tokens":100,"cached_input_tokens":90,"output_tokens":10,
        "cost":{"status":"estimated","currency":"USD","total_micros":9999,"source":"old"}
    })).unwrap();
    let before = serde_json::to_vec(&usage).unwrap();
    let cost = price_session_usage(&ModelCatalog::new(document.clone()), &usage);
    assert!(
        matches!(cost, SessionCostEstimate::Estimated { total_micros:39, revision: Some(ref revision), .. } if revision == "supplied")
    );
    document.providers.clear();
    assert!(matches!(
        price_session_usage(&ModelCatalog::new(document), &usage),
        SessionCostEstimate::Unavailable { .. }
    ));
    assert_eq!(serde_json::to_vec(&usage).unwrap(), before);
}
