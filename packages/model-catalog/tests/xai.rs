#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bcode_model_catalog::{
    ModelCatalog, load_embedded_catalog, overlay_remote_catalog, overlay_remote_live,
};
use bcode_model_catalog_models::{
    BcodeSupportStatus, CatalogPricingBucket, LiveCatalogSnapshot, ModelSupportTarget,
};

fn target(surface: &str) -> ModelSupportTarget {
    ModelSupportTarget::new("xai", "api_key", surface, Some("bcode"))
}

#[test]
fn cached_startup_ignores_unusable_overlays_without_network_or_writes() {
    use bcode_model_catalog::{RemoteCatalogOptions, load_cached_catalog};
    let cache = tempfile::tempdir().unwrap();
    let options = RemoteCatalogOptions {
        cache_dir: cache.path().to_path_buf(),
        base_url: "unsupported://offline.invalid".to_string(),
        disabled: false,
        ..RemoteCatalogOptions::disabled()
    };
    let baseline = load_embedded_catalog().unwrap();
    assert_eq!(load_cached_catalog(options.clone()).unwrap(), baseline);
    assert_eq!(std::fs::read_dir(cache.path()).unwrap().count(), 0);
    for contents in [b"not json".to_vec(), vec![b' '; 16 * 1024 * 1024 + 1]] {
        std::fs::write(cache.path().join("catalog.json"), &contents).unwrap();
        assert_eq!(load_cached_catalog(options.clone()).unwrap(), baseline);
        assert_eq!(
            std::fs::read(cache.path().join("catalog.json")).unwrap(),
            contents
        );
    }
    let mut future = baseline.clone();
    future.schema_version = "999.0.0".to_string();
    std::fs::write(
        cache.path().join("catalog.json"),
        serde_json::to_vec(&future).unwrap(),
    )
    .unwrap();
    assert_eq!(load_cached_catalog(options.clone()).unwrap(), baseline);
    let mut overlay = baseline.clone();
    overlay.providers.get_mut("xai").unwrap().default_model_id = Some("grok-4.5".to_string());
    std::fs::write(
        cache.path().join("catalog.json"),
        serde_json::to_vec(&overlay).unwrap(),
    )
    .unwrap();
    assert_eq!(
        load_cached_catalog(options.clone()).unwrap().providers["xai"]
            .default_model_id
            .as_deref(),
        Some("grok-4.5")
    );
    let expired = RemoteCatalogOptions {
        max_stale: std::time::Duration::ZERO,
        ..options
    };
    assert_eq!(load_cached_catalog(expired).unwrap(), baseline);
}

#[test]
fn cached_live_startup_supplements_baseline_with_target_qualified_new_ids() {
    use bcode_model_catalog::{RemoteCatalogOptions, load_cached_catalog};
    let cache = tempfile::tempdir().unwrap();
    let snapshot: LiveCatalogSnapshot = serde_json::from_value(serde_json::json!({
        "schema_version": "1.0.0",
        "provider_id": "xai",
        "generated_at": "2026-10-08T00:00:00Z",
        "target": {"provider": "xai", "auth_mode": "api_key", "api_surface": "responses_api", "integration": "bcode"},
        "models": {"grok-future-discovered": {"model_id": "grok-future-discovered"}}
    })).unwrap();
    std::fs::write(
        cache.path().join("live-xai.json"),
        serde_json::to_vec(&snapshot).unwrap(),
    )
    .unwrap();
    let options = RemoteCatalogOptions {
        cache_dir: cache.path().to_path_buf(),
        base_url: "unsupported://offline.invalid".to_string(),
        disabled: false,
        ..RemoteCatalogOptions::disabled()
    };
    let catalog = ModelCatalog::new(load_cached_catalog(options).unwrap());
    let choices =
        catalog.provider_models_for_support_target("xai", &target("responses_api"), false);
    assert!(choices.iter().any(|model| model.model_id == "grok-4.7"));
    assert!(
        choices
            .iter()
            .any(|model| model.model_id == "grok-future-discovered")
    );
    assert!(
        !catalog
            .provider_models_for_support_target("xai", &target("chat_completions"), false)
            .iter()
            .any(|model| model.model_id == "grok-future-discovered")
    );
    let entry = catalog.model("xai", "grok-future-discovered").unwrap();
    assert_eq!(entry.pricing, None);
    assert_eq!(entry.context_window, None);
    assert_eq!(entry.bcode_support, BcodeSupportStatus::Unknown);
    assert_eq!(std::fs::read_dir(cache.path()).unwrap().count(), 1);
}

#[test]
fn legacy_xai_concrete_ids_do_not_fabricate_current_prices() {
    let catalog = ModelCatalog::new(load_embedded_catalog().unwrap());
    for id in [
        "grok-4",
        "grok-3",
        "grok-4-fast-reasoning",
        "grok-4-fast-non-reasoning",
    ] {
        let entry = catalog.model("xai", id).unwrap();
        assert_eq!(entry.pricing, None);
        assert_eq!(entry.max_output_tokens, None);
        assert_eq!(
            entry.status,
            bcode_model_catalog_models::CatalogModelStatus::Unknown
        );
    }
    assert_eq!(
        catalog.model("xai", "grok-4-latest").unwrap().model_id,
        "grok-4"
    );
    assert_eq!(catalog.model("xai", "grok-3").unwrap().context_window, None);
}

#[test]
fn xai_offline_inventory_has_current_and_older_documented_choices() {
    let catalog = ModelCatalog::new(load_embedded_catalog().unwrap());
    let provider = catalog.provider("xai").unwrap();
    assert_eq!(provider.default_model_id.as_deref(), Some("grok-4.7"));
    let models =
        catalog.provider_models_for_support_target("xai", &target("chat_completions"), false);
    for id in ["grok-4.7", "grok-4.6", "grok-4.5", "grok-4.3"] {
        assert!(models.iter().any(|model| model.model_id == id));
        let entry = catalog.model("xai", id).unwrap();
        assert_eq!(entry.max_output_tokens, None);
        assert_eq!(entry.capabilities.parallel_tool_calls, None);
    }
    assert_eq!(
        catalog.model("xai", "grok-4.7").unwrap().context_window,
        Some(500_000)
    );
    assert_eq!(
        catalog.model("xai", "grok-4.3").unwrap().context_window,
        Some(1_000_000)
    );
    assert_eq!(
        catalog.model("xai", "grok-build-latest").unwrap().model_id,
        "grok-4.5"
    );
    assert!(catalog.model("xai", "grok-future-discovered").is_none());
}

#[test]
fn xai_support_routes_separate_multi_agent_from_chat_completions() {
    let catalog = ModelCatalog::new(load_embedded_catalog().unwrap());
    let chat = catalog.fallback_model_ids_for_support_target("xai", &target("chat_completions"));
    let responses = catalog.fallback_model_ids_for_support_target("xai", &target("responses_api"));
    assert!(!chat.contains(&"grok-4.20-multi-agent-0309".to_string()));
    assert!(!responses.contains(&"grok-4.20-multi-agent-0309".to_string()));
    let multi_agent = catalog.model("xai", "grok-4.20-multi-agent-0309").unwrap();
    assert_eq!(multi_agent.bcode_support, BcodeSupportStatus::Unsupported);
    assert!(!multi_agent.capabilities.tool_use);
    assert!(multi_agent.deployments.is_empty());
    assert!(
        catalog
            .provider_models_for_support_target("xai", &target("chatgpt_codex"), false)
            .is_empty()
    );
}

#[test]
fn xai_prices_cover_the_inclusive_long_context_boundary() {
    let document = load_embedded_catalog().unwrap();
    let provider = &document.providers["xai"];
    for (id, input, cache, output) in [
        ("grok-4.7", 2_000_000, 500_000, 6_000_000),
        ("grok-4.6", 2_000_000, 500_000, 6_000_000),
        ("grok-4.5", 2_000_000, 300_000, 6_000_000),
        ("grok-4.3", 1_250_000, 200_000, 2_500_000),
    ] {
        let pricing = provider.models[id].pricing.as_ref().unwrap();
        assert_eq!(pricing.input_micros, Some(input));
        assert_eq!(pricing.cached_input_micros, Some(cache));
        assert_eq!(pricing.output_micros, Some(output));
        for (bucket, base) in [
            (CatalogPricingBucket::Input, input),
            (CatalogPricingBucket::CacheReadInput, cache),
            (CatalogPricingBucket::Output, output),
        ] {
            let short = pricing
                .rules
                .iter()
                .find(|rule| {
                    rule.bucket == bucket
                        && rule.invocation_class.is_none()
                        && rule.max_request_input_tokens == Some(199_999)
                })
                .unwrap();
            let long = pricing
                .rules
                .iter()
                .find(|rule| {
                    rule.bucket == bucket
                        && rule.invocation_class.is_none()
                        && rule.min_request_input_tokens == Some(200_000)
                })
                .unwrap();
            assert_eq!(short.price_micros, base);
            assert_eq!(long.price_micros, base * 2);
        }
    }
}

#[test]
fn partial_remote_xai_refresh_supplements_baseline_and_updates_prices() {
    let mut baseline = load_embedded_catalog().unwrap();
    let mut remote = baseline.clone();
    remote.providers.retain(|id, _| id == "xai");
    let provider = remote.providers.get_mut("xai").unwrap();
    provider.models.retain(|id, _| id == "grok-4.7");
    provider.fallback_model_ids = vec!["grok-4.7".to_string()];
    let pricing = provider
        .models
        .get_mut("grok-4.7")
        .unwrap()
        .pricing
        .as_mut()
        .unwrap();
    pricing.input_micros = Some(3_000_000);
    pricing.rules.clear();
    overlay_remote_catalog(&mut baseline, &remote);
    let provider = &baseline.providers["xai"];
    assert!(provider.models.contains_key("grok-4.3"));
    assert!(
        provider
            .fallback_model_ids
            .contains(&"grok-4.3".to_string())
    );
    assert_eq!(
        provider.models["grok-4.7"]
            .pricing
            .as_ref()
            .unwrap()
            .input_micros,
        Some(3_000_000)
    );
    assert_eq!(provider.models["grok-4.7"].deployments.len(), 2);
}

#[test]
fn future_xai_live_ids_do_not_inherit_family_prices_or_limits() {
    let mut document = load_embedded_catalog().unwrap();
    let snapshot: LiveCatalogSnapshot = serde_json::from_value(serde_json::json!({
        "schema_version": "1.0.0",
        "provider_id": "xai",
        "generated_at": "2026-10-08T00:00:00Z",
        "source": "provider_live",
        "models": {"grok-future-discovered": {"model_id": "grok-future-discovered"}}
    }))
    .unwrap();
    overlay_remote_live(&mut document, &[snapshot]);
    let catalog = ModelCatalog::new(document);
    let entry = catalog.model("xai", "grok-future-discovered").unwrap();
    assert_eq!(entry.context_window, None);
    assert_eq!(entry.max_output_tokens, None);
    assert_eq!(entry.pricing, None);
    assert!(!entry.capabilities.tool_use);
    assert_eq!(entry.bcode_support, BcodeSupportStatus::Unknown);
    assert!(
        catalog
            .fallback_model_ids_for_support_target("xai", &target("chat_completions"))
            .contains(&"grok-4.7".to_string())
    );
}
