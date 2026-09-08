//! Regression tests for caller-supplied usage valuations.

use bcode_model_catalog::{ModelCatalog, price_session_usage};
use bcode_session_models::{SessionCostEstimate, SessionTokenUsage};

#[test]
fn astra_session_requires_the_reported_cache_write_bucket() {
    let catalog = ModelCatalog::load_bundled().unwrap();
    let mut usage: SessionTokenUsage = serde_json::from_value(serde_json::json!({
        "catalog_provider_id":"openai","catalog_entry_id":"gpt-6-astra",
        "pricing_target":{"provider":"openai","auth_mode":"chatgpt_subscription","api_surface":"chatgpt_codex","integration":"bcode"},
        "input_tokens":11331,"output_tokens":13,"total_tokens":11344,"cached_input_tokens":0,
        "pricing_context":{"service_tier":"standard","invocation_class":"ondemand","billing_scope":"in_region","request_input_tokens":11331}
    })).unwrap();
    assert_eq!(
        price_session_usage(&catalog, &usage),
        SessionCostEstimate::Unavailable {
            reason: bcode_session_models::SessionCostUnavailableReason::ProviderUsageIncomplete,
        }
    );
    for (written, expected) in [(0, 113_960), (11_331, 142_287)] {
        usage.cache_write_input_tokens = Some(written);
        let cost = price_session_usage(&catalog, &usage);
        assert!(
            matches!(cost, SessionCostEstimate::Estimated { total_micros, .. } if total_micros == expected)
        );
    }
    usage.cached_input_tokens = Some(10_000);
    usage.cache_write_input_tokens = Some(1_000);
    assert!(matches!(
        price_session_usage(&catalog, &usage),
        SessionCostEstimate::Estimated {
            total_micros: 26_460,
            ..
        }
    ));
    usage.cache_write_input_tokens = Some(2_000);
    assert_eq!(
        price_session_usage(&catalog, &usage),
        SessionCostEstimate::Unavailable {
            reason: bcode_session_models::SessionCostUnavailableReason::ConflictingUsage,
        }
    );
}

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
