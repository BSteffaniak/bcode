//! Regression tests for caller-supplied usage valuations.

use bcode_model_catalog::{ModelCatalog, price_session_usage};
use bcode_session_models::{SessionCostEstimate, SessionTokenUsage};

#[test]
fn sol_and_luna_catalog_metadata_and_pricing() {
    let catalog = ModelCatalog::load_bundled().unwrap();
    let target = bcode_model_catalog_models::ModelSupportTarget {
        provider: "openai".to_owned(),
        auth_mode: "api_key".to_owned(),
        api_surface: "responses_api".to_owned(),
        integration: Some("bcode".to_owned()),
    };
    let models = catalog.provider_models_for_support_target("openai", &target, false);
    let fallback = catalog.fallback_model_ids_for_support_target("openai", &target);
    for (id, standard, long) in [
        ("gpt-6-sol", 552_700, 1_100_404),
        ("gpt-6-luna", 27_635, 55_020),
    ] {
        let entry = catalog.model("openai", id).unwrap();
        assert_eq!(entry.model_id, id);
        assert_eq!(
            catalog
                .model("openai", &format!("{id}-snapshot"))
                .unwrap()
                .model_id,
            id
        );
        assert_eq!(entry.family.as_deref(), Some("gpt-6"));
        assert!(entry.capabilities.image_input);
        assert!(entry.capabilities.tool_use);
        assert!(entry.capabilities.structured_outputs);
        assert!(entry.capabilities.prompt_cache);
        let reasoning = entry.reasoning.as_ref().unwrap();
        assert_eq!(
            reasoning.effort_values,
            ["none", "low", "medium", "high", "xhigh", "max"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
        assert_eq!(reasoning.default_effort.as_deref(), Some("medium"));
        let model = models.iter().find(|model| model.model_id == id).unwrap();
        assert_eq!(model.context_window, Some(1_050_000));
        assert_eq!(model.max_output_tokens, Some(128_000));
        assert!(fallback.iter().any(|model| model == id));
        for (input, expected) in [(272_000, standard), (272_001, long)] {
            let usage: SessionTokenUsage = serde_json::from_value(serde_json::json!({
                "catalog_provider_id":"openai", "catalog_entry_id":id,
                "pricing_target":target,
                "input_tokens":input, "output_tokens":1_000, "total_tokens":input + 1_000,
                "cached_input_tokens":1_000, "cache_write_input_tokens":1_000,
                "pricing_context":{"service_tier":"standard","invocation_class":"ondemand","request_input_tokens":input}
            })).unwrap();
            assert!(
                matches!(
                    price_session_usage(&catalog, &usage),
                    SessionCostEstimate::Estimated { total_micros, .. } if total_micros == expected
                ),
                "{id} input={input}: {:?}",
                price_session_usage(&catalog, &usage)
            );
        }
    }
    assert_eq!(
        catalog.model("openai", "gpt-6").unwrap().model_id,
        "gpt-6-astra"
    );
    for id in ["gpt-5.6-sol", "gpt-5.6-luna"] {
        assert_eq!(catalog.model("openai", id).unwrap().model_id, id);
    }
}

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
