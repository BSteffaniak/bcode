//! Offline-first model lists with bounded, supplementary provider discovery.

use std::time::Duration;

use bcode_model::{ModelInfo, ModelMetadataSource};
use bcode_model_catalog::ModelCatalog;
use bcode_model_catalog_models::ModelSupportTarget;

use super::{
    ModelResponseItem, Settings, catalog_provider_id, model_ids_with_selected,
    model_infos_from_ids_without_catalog, model_infos_from_items_without_catalog, startup,
};

pub const LIVE_TIMEOUT: Duration = Duration::from_secs(2);
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
pub const MAX_LIVE_MODELS: usize = 2048;

/// Identify the provider-owned endpoint, not arbitrary URLs containing `x.ai`.
#[must_use]
pub fn is_xai_provider(base_url: Option<&str>, dialect: Option<&str>) -> bool {
    dialect.is_some_and(|value| {
        value.eq_ignore_ascii_case("xai") || value.eq_ignore_ascii_case("x.ai")
    }) || base_url
        .and_then(|url| reqwest::Url::parse(url).ok())
        .is_some_and(|url| url.host_str() == Some("api.x.ai"))
}

pub fn recommended_xai_model(
    catalog: &ModelCatalog,
    dialect: super::OpenAiCompatibleDialect,
) -> Option<String> {
    let provider = catalog.provider("xai")?;
    let target = ModelSupportTarget::new("xai", "api_key", dialect.metadata_value(), Some("bcode"));
    let models = catalog.provider_models_for_support_target("xai", &target, false);
    provider
        .default_model_id
        .iter()
        .chain(provider.fallback_model_ids.iter())
        .find(|id| models.iter().any(|model| model.model_id == **id))
        .cloned()
        .or_else(|| models.first().map(|model| model.model_id.clone()))
}

pub fn offline_models(settings: &Settings, selected: Option<&str>) -> Vec<ModelInfo> {
    let catalog = startup::offline_catalog();
    offline_models_with_catalog(settings, selected, catalog.as_ref())
}

pub fn offline_models_with_catalog(
    settings: &Settings,
    selected: Option<&str>,
    catalog: Option<&ModelCatalog>,
) -> Vec<ModelInfo> {
    let ids = model_ids_with_selected(&settings.model_ids, selected);
    let preferred = selected.or(settings.default_model.as_deref());
    let mut configured = model_infos_from_ids_without_catalog(&ids, preferred);
    if catalog_provider_id(settings) != Some("xai") {
        return configured;
    }
    for model in &mut configured {
        model.capabilities.clear();
    }
    let mut models = if let Some(catalog) = catalog {
        let target = ModelSupportTarget::new(
            "xai",
            "api_key",
            settings.dialect.metadata_value(),
            Some("bcode"),
        );
        catalog.merge_provider_models_for_target(
            "xai",
            configured,
            !settings.model_ids_are_explicit,
            Some(&target),
        )
    } else {
        configured
    };
    let recommended = catalog.and_then(|catalog| recommended_xai_model(catalog, settings.dialect));
    let default = preferred.or(recommended.as_deref());
    for model in &mut models {
        model.is_default = default == Some(model.model_id.as_str());
    }
    models
}

pub fn merge_live(
    settings: &Settings,
    mut baseline: Vec<ModelInfo>,
    items: Vec<ModelResponseItem>,
    catalog: Option<&ModelCatalog>,
) -> Vec<ModelInfo> {
    // An ID-only response cannot establish price, context, or tool support.
    let mut live = model_infos_from_items_without_catalog(items, None);
    for model in &mut live {
        model.is_default = false;
        model.capabilities.clear();
        model.metadata_source = Some(ModelMetadataSource::ProviderLive);
    }
    let target = ModelSupportTarget::new(
        "xai",
        "api_key",
        settings.dialect.metadata_value(),
        Some("bcode"),
    );
    for model in live {
        let model = catalog.map_or_else(
            || model.clone(),
            |catalog| catalog.enrich_model_for_target("xai", model.clone(), &target),
        );
        if let Some(existing) = baseline
            .iter_mut()
            .find(|entry| entry.model_id == model.model_id)
        {
            // Live facts supplement, never erase known catalog metadata or user selection.
            if model.pricing.is_some() {
                existing.pricing = model.pricing;
            }
            if model.context_window.is_some() {
                existing.context_window = model.context_window;
            }
            if model.max_output_tokens.is_some() {
                existing.max_output_tokens = model.max_output_tokens;
            }
        } else {
            baseline.push(model);
        }
    }
    baseline
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xai_settings() -> Settings {
        let mut settings = crate::tests::test_settings(
            crate::AuthSettings::ApiKey("fixture".into()),
            crate::OpenAiCompatibleDialect::ChatCompletions,
        );
        settings.catalog_provider_id = Some("xai".into());
        settings.default_model = None;
        settings.model_ids.clear();
        settings.model_ids_are_explicit = false;
        settings
    }

    fn fixture(
        status: u16,
        body: &str,
        delay: Duration,
    ) -> (String, std::thread::JoinHandle<usize>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let body = body.to_owned();
        let worker = std::thread::spawn(move || {
            let mut deadline = std::time::Instant::now() + Duration::from_secs(15);
            let mut requests = 0;
            while std::time::Instant::now() < deadline {
                if let Ok((mut stream, _)) = listener.accept() {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_millis(500)))
                        .unwrap();
                    let mut buffer = [0; 4096];
                    let mut request = Vec::new();
                    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                        let read = stream.read(&mut buffer).expect("fixture request headers");
                        assert!(read > 0, "request closed before complete headers");
                        request.extend_from_slice(&buffer[..read]);
                        assert!(request.len() <= 16 * 1024, "bounded fixture headers");
                    }
                    requests += 1;
                    deadline =
                        std::time::Instant::now() + LIVE_TIMEOUT + Duration::from_millis(400);
                    std::thread::sleep(delay);
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                } else {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            requests
        });
        (format!("http://{address}"), worker)
    }

    #[test]
    fn cached_catalog_hydrates_new_ids_prices_and_selected_dialect() {
        use bcode_model_catalog::{RemoteCatalogOptions, load_cached_catalog};
        let cache = tempfile::tempdir().unwrap();
        let options = RemoteCatalogOptions {
            cache_dir: cache.path().to_path_buf(),
            base_url: "unsupported://offline.invalid".into(),
            disabled: false,
            ..RemoteCatalogOptions::disabled()
        };
        let mut document = bcode_model_catalog::load_embedded_catalog().unwrap();
        let provider = document.providers.get_mut("xai").unwrap();
        let original_id = provider.default_model_id.clone().unwrap();
        let mut new = provider.models[&original_id].clone();
        new.model_id = "fixture-cached-responses".into();
        new.display_name = "Cached fixture model".into();
        new.aliases.clear();
        new.supported_by.clear();
        new.deployments
            .retain(|deployment| deployment.target.api_surface == "responses_api");
        new.pricing.as_mut().unwrap().input_micros = Some(123_456);
        provider.models.insert(new.model_id.clone(), new);
        provider.default_model_id = Some("fixture-cached-responses".into());
        let contents = serde_json::to_vec(&document).unwrap();
        let path = cache.path().join("catalog.json");
        std::fs::write(&path, &contents).unwrap();
        let catalog = ModelCatalog::new(load_cached_catalog(options.clone()).unwrap());
        let mut settings = xai_settings();
        settings.dialect = crate::OpenAiCompatibleDialect::ResponsesApi;
        let models = offline_models_with_catalog(&settings, None, Some(&catalog));
        let cached = models
            .iter()
            .find(|model| model.model_id == "fixture-cached-responses")
            .unwrap();
        assert!(cached.is_default);
        assert_eq!(
            cached
                .pricing
                .as_ref()
                .unwrap()
                .input
                .as_ref()
                .unwrap()
                .micros,
            123_456
        );
        assert!(
            cached
                .capabilities
                .contains(&bcode_model::ModelCapability::ToolCalls)
        );
        settings.model_ids_are_explicit = true;
        settings.model_ids = vec![original_id.clone()];
        let explicit = offline_models_with_catalog(&settings, Some(&original_id), Some(&catalog));
        assert_eq!(explicit.len(), 1);
        assert_eq!(explicit[0].model_id, original_id);
        assert!(explicit[0].is_default);
        settings.dialect = crate::OpenAiCompatibleDialect::ChatCompletions;
        assert_ne!(
            recommended_xai_model(&catalog, settings.dialect).as_deref(),
            Some("fixture-cached-responses")
        );
        assert_eq!(std::fs::read(&path).unwrap(), contents);
        assert_eq!(std::fs::read_dir(cache.path()).unwrap().count(), 1);
        std::fs::write(&path, b"malformed optional cache").unwrap();
        let fallback = ModelCatalog::new(load_cached_catalog(options).unwrap());
        assert!(fallback.provider("xai").is_some());
        assert!(fallback.model("xai", "fixture-cached-responses").is_none());
    }

    #[test]
    fn offline_baseline_preserves_selection_and_unknown_metadata() {
        let settings = xai_settings();
        let models = offline_models(&settings, Some("fixture-remembered"));
        assert!(
            models.len() > 1,
            "bundled xAI baseline must be usable offline"
        );
        let remembered = models
            .iter()
            .find(|model| model.model_id == "fixture-remembered")
            .unwrap();
        assert!(remembered.is_default);
        assert!(remembered.pricing.is_none());
        assert!(remembered.context_window.is_none());
        assert!(remembered.capabilities.is_empty());
        let recommended =
            recommended_xai_model(&startup::offline_catalog().unwrap(), settings.dialect).unwrap();
        assert!(models.iter().any(|model| model.model_id == recommended));
    }

    #[tokio::test]
    async fn failures_empty_malformed_and_timeout_keep_baseline_without_retry() {
        for (status, body, delay) in [
            (503, "unavailable", Duration::ZERO),
            (200, r#"{"data":[]}"#, Duration::ZERO),
            (200, "malformed", Duration::ZERO),
            (200, r#"{"data":[{}]}"#, Duration::ZERO),
            (
                200,
                r#"{"data":[]}"#,
                LIVE_TIMEOUT + Duration::from_millis(100),
            ),
        ] {
            let (url, worker) = fixture(status, body, delay);
            let mut settings = xai_settings();
            settings.base_url = url;
            let baseline = offline_models(&settings, Some("fixture-remembered"));
            let models =
                crate::resolve_model_candidates(&settings, "fixture", Some("fixture-remembered"))
                    .await
                    .unwrap();
            assert_eq!(models, baseline);
            assert_eq!(worker.join().unwrap(), 1, "must not issue a second fetch");
        }
    }

    #[tokio::test]
    async fn live_ids_supplement_catalog_without_inventing_metadata() {
        // Catalog setup is outside the mock server's request deadline.
        let catalog = startup::offline_catalog().unwrap();
        let (url, worker) = fixture(
            200,
            r#"{"data":[{"id":"fixture-new-model"},{"id":"fixture-new-model"},{"id":" "}]}"#,
            Duration::ZERO,
        );
        let mut settings = xai_settings();
        settings.base_url = url;
        let models = crate::resolve_model_candidates_with_catalog(
            &settings,
            "fixture",
            Some("fixture-remembered"),
            Some(&catalog),
        )
        .await
        .unwrap();
        assert!(models.iter().any(|model| {
            model.model_id == recommended_xai_model(&catalog, settings.dialect).unwrap()
        }));
        assert!(
            models
                .iter()
                .find(|model| model.model_id == "fixture-remembered")
                .unwrap()
                .is_default
        );
        let new = models
            .iter()
            .filter(|model| model.model_id == "fixture-new-model")
            .collect::<Vec<_>>();
        assert_eq!(new.len(), 1);
        assert!(new[0].pricing.is_none());
        assert!(new[0].context_window.is_none());
        assert!(new[0].capabilities.is_empty());
        assert_eq!(worker.join().unwrap(), 1);
    }

    #[tokio::test]
    async fn explicit_disabled_and_unauthenticated_lists_need_no_fetch() {
        let mut settings = xai_settings();
        settings.base_url = "http://127.0.0.1:1".into();
        settings.live_discovery_disabled = true;
        assert_eq!(
            crate::resolve_model_candidates(&settings, "fixture", None)
                .await
                .unwrap(),
            offline_models(&settings, None)
        );
        settings.live_discovery_disabled = false;
        assert_eq!(
            crate::resolve_model_candidates(&settings, "", None)
                .await
                .unwrap(),
            offline_models(&settings, None)
        );
        settings.model_ids_are_explicit = true;
        settings.model_ids = vec!["fixture-explicit".into()];
        let models = crate::resolve_model_candidates(&settings, "fixture", None)
            .await
            .unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model_id, "fixture-explicit");
    }

    #[tokio::test]
    async fn discovery_future_can_be_cancelled() {
        let (url, worker) = fixture(200, r#"{"data":[]}"#, Duration::from_millis(200));
        let mut settings = xai_settings();
        settings.base_url = url;
        assert!(
            tokio::time::timeout(
                Duration::from_millis(100),
                crate::resolve_model_candidates(&settings, "fixture", None)
            )
            .await
            .is_err()
        );
        assert!(worker.join().unwrap() <= 1);
    }

    #[test]
    fn endpoint_detection_is_not_substring_matching() {
        assert!(is_xai_provider(Some("https://api.x.ai/v1"), None));
        assert!(!is_xai_provider(
            Some("https://api.x.ai.attacker.test/v1"),
            None
        ));
        assert!(!is_xai_provider(Some("https://proxy.test/x.ai"), None));
    }
}
