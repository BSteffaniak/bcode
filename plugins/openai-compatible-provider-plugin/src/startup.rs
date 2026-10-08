//! Offline startup selection. This path does not construct a provider client or read credentials.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use bcode_model::{
    ModelCapability, StartupModelReadiness, StartupReadinessRequest, StartupReadinessResponse,
    StartupRecommendation, StartupRecommendationRequest, StartupRecommendationResponse,
};
use bcode_model_catalog::ModelCatalog;
use bcode_model_catalog_models::{BcodeSupportStatus, CatalogModelStatus, ModelSupportTarget};
use bcode_plugin_sdk::{ServiceRequest, ServiceResponse};

use super::{DEFAULT_BASE_URL, DEFAULT_XAI_BASE_URL, PROVIDER_ID, invalid_request, json_response};

struct Routing {
    base_url: &'static str,
    dialect: &'static str,
    auth_mode: &'static str,
    support_auth_mode: &'static str,
}

fn routing(request: &StartupRecommendationRequest) -> Option<Routing> {
    match (
        request.auth_provider_id.as_str(),
        request.method_id.as_str(),
    ) {
        ("openai", "api_key") => Some(Routing {
            base_url: DEFAULT_BASE_URL,
            dialect: "responses_api",
            auth_mode: "api_key",
            support_auth_mode: "api_key",
        }),
        ("openai", "chatgpt" | "device") => Some(Routing {
            base_url: DEFAULT_BASE_URL,
            dialect: "chatgpt_codex",
            auth_mode: "chatgpt",
            support_auth_mode: "chatgpt_subscription",
        }),
        ("openrouter", "api_key") => Some(Routing {
            base_url: "https://openrouter.ai/api/v1",
            dialect: "chat_completions",
            auth_mode: "api_key",
            support_auth_mode: "api_key",
        }),
        ("xai", "api_key") => Some(Routing {
            base_url: DEFAULT_XAI_BASE_URL,
            dialect: "responses_api",
            auth_mode: "api_key",
            support_auth_mode: "api_key",
        }),
        _ => None,
    }
}

pub fn response(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<StartupRecommendationRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    let recommendation = routing(&request).and_then(|routing| {
        embedded_catalog().and_then(|catalog| recommend(catalog, &request, &routing))
    });
    json_response(&StartupRecommendationResponse { recommendation })
}

fn embedded_catalog() -> Option<&'static ModelCatalog> {
    // Cache both success and failure. No environment, credentials, overlays or live snapshots.
    static CATALOG: OnceLock<Option<ModelCatalog>> = OnceLock::new();
    CATALOG
        .get_or_init(|| ModelCatalog::load_bundled().ok())
        .as_ref()
}

pub fn readiness_response(request: &ServiceRequest) -> ServiceResponse {
    let request = match request.payload_json::<StartupReadinessRequest>() {
        Ok(request) => request,
        Err(error) => return invalid_request(&error),
    };
    let model = embedded_catalog().map_or(StartupModelReadiness::Unknown, |catalog| {
        readiness(catalog, &request)
    });
    json_response(&StartupReadinessResponse { model })
}

fn readiness(catalog: &ModelCatalog, request: &StartupReadinessRequest) -> StartupModelReadiness {
    use StartupModelReadiness::{Available, Unavailable, Unknown};

    let settings = &request.settings;
    let endpoint = settings
        .get("openai.base_url")
        .or_else(|| settings.get("base_url"))
        .map_or(DEFAULT_BASE_URL, String::as_str);
    let provider_id = settings
        .get(bcode_model::CATALOG_PROVIDER_ID_SETTING)
        .map_or("openai", String::as_str);
    // Only known provider-owned routing is authoritative. A custom proxy might serve models
    // absent from the upstream catalog, even when it uses an upstream catalog mapping.
    let known_endpoint = match provider_id {
        "openai" => DEFAULT_BASE_URL,
        "openrouter" => "https://openrouter.ai/api/v1",
        "xai" => DEFAULT_XAI_BASE_URL,
        _ => return Unknown,
    };
    if endpoint.trim_end_matches('/') != known_endpoint {
        return Unknown;
    }
    let auth_mode = settings.get("auth_mode").map_or("api_key", String::as_str);
    let configured_dialect = settings
        .get("openai.dialect")
        .or_else(|| settings.get("dialect"));
    let dialect = match (auth_mode, configured_dialect) {
        (_, Some(value)) if super::parse_dialect(value).is_none() => return Unknown,
        ("chatgpt" | "chatgpt_subscription", _) if provider_id == "openai" => "chatgpt_codex",
        ("api_key", Some(value)) => match super::parse_dialect(value) {
            Some(super::OpenAiCompatibleDialect::ResponsesApi) => "responses_api",
            Some(super::OpenAiCompatibleDialect::ChatCompletions) => "chat_completions",
            _ => return Unknown,
        },
        ("api_key", None) => "chat_completions",
        _ => return Unknown,
    };
    let Some(provider) = catalog.provider(provider_id) else {
        return Unknown;
    };
    // Exact native identity only: never resolve an alias or suggest a replacement here.
    let Some(entry) = provider
        .models
        .values()
        .find(|entry| entry.model_id == request.model_id)
    else {
        return Unavailable;
    };
    if entry.status == CatalogModelStatus::Unavailable
        || entry.bcode_support == BcodeSupportStatus::Unsupported
    {
        return Unavailable;
    }
    let target = ModelSupportTarget::new(
        provider_id,
        if dialect == "chatgpt_codex" {
            "chatgpt_subscription"
        } else {
            "api_key"
        },
        dialect,
        Some("bcode"),
    );
    let model = catalog
        .provider_models_for_support_target(provider_id, &target, false)
        .into_iter()
        .find(|model| model.model_id == request.model_id);
    match model {
        Some(model) if model.capabilities.contains(&ModelCapability::ToolCalls) => Available,
        None if entry.supported_by.is_empty() && entry.deployments.is_empty() => Unknown,
        Some(_) | None => Unavailable,
    }
}

fn recommend(
    catalog: &ModelCatalog,
    request: &StartupRecommendationRequest,
    routing: &Routing,
) -> Option<StartupRecommendation> {
    let provider_id = &request.auth_provider_id;
    let provider = catalog.provider(provider_id)?;
    let target = ModelSupportTarget::new(
        provider_id.as_str(),
        routing.support_auth_mode,
        routing.dialect,
        Some("bcode"),
    );
    let models = catalog.provider_models_for_support_target(provider_id, &target, false);
    let preferred = if routing.auth_mode == "chatgpt" {
        provider.default_codex_model_id.as_ref()
    } else {
        provider.default_model_id.as_ref()
    };
    let model = preferred
        .into_iter()
        .chain(provider.fallback_model_ids.iter())
        .filter_map(|id| {
            let entry = catalog.model(provider_id, id)?;
            models.iter().find(|model| model.model_id == entry.model_id)
        })
        .chain(models.iter())
        .find(|model| model.capabilities.contains(&ModelCapability::ToolCalls))?;
    Some(StartupRecommendation {
        provider_plugin_id: PROVIDER_ID.to_owned(),
        model_id: model.model_id.clone(),
        settings: BTreeMap::from([
            ("base_url".to_owned(), routing.base_url.to_owned()),
            ("dialect".to_owned(), routing.dialect.to_owned()),
            ("auth_mode".to_owned(), routing.auth_mode.to_owned()),
            (
                bcode_model::CATALOG_PROVIDER_ID_SETTING.to_owned(),
                provider_id.clone(),
            ),
        ]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(provider: &str, method: &str) -> StartupRecommendationRequest {
        StartupRecommendationRequest {
            auth_provider_id: provider.to_owned(),
            method_id: method.to_owned(),
        }
    }

    #[test]
    fn openai_selection_uses_supported_tool_models_and_safe_routing() {
        let catalog = ModelCatalog::load_bundled().unwrap();
        for method in ["api_key", "chatgpt", "device"] {
            let request = request("openai", method);
            let routing = routing(&request).unwrap();
            let recommendation = recommend(&catalog, &request, &routing).unwrap();
            let provider = catalog.provider("openai").unwrap();
            if method != "api_key" {
                assert_eq!(
                    Some(recommendation.model_id.as_str()),
                    provider.default_codex_model_id.as_deref()
                );
            }
            assert_eq!(recommendation.provider_plugin_id, PROVIDER_ID);
            assert_eq!(recommendation.settings.len(), 4);
            assert_eq!(recommendation.settings["catalog_provider_id"], "openai");
            assert_eq!(recommendation.settings["base_url"], DEFAULT_BASE_URL);
            assert_eq!(recommendation.settings["dialect"], routing.dialect);
            assert_eq!(recommendation.settings["auth_mode"], routing.auth_mode);
            let model = catalog
                .provider_models_as_model_info("openai")
                .into_iter()
                .find(|model| model.model_id == recommendation.model_id)
                .unwrap();
            assert!(model.capabilities.contains(&ModelCapability::ToolCalls));
        }
    }

    #[test]
    fn unsupported_registration_or_method_fails_closed() {
        for (provider, method) in [
            ("unknown", "api_key"),
            ("openai", "browser"),
            ("openai", "unknown"),
            ("openrouter", "chatgpt"),
            ("xai", "device"),
            ("OPENAI", "api_key"),
        ] {
            assert!(routing(&request(provider, method)).is_none());
        }
    }

    #[test]
    fn typed_service_is_credential_free_and_rejects_malformed_requests() {
        let service_request = ServiceRequest {
            interface_id: bcode_model::MODEL_PROVIDER_INTERFACE_ID.to_owned(),
            operation: bcode_model::OP_STARTUP_RECOMMENDATION.to_owned(),
            payload: serde_json::to_vec(&request("openai", "device")).unwrap(),
        };
        let result = response(&service_request);
        assert!(result.error.is_none());
        let decoded: StartupRecommendationResponse =
            serde_json::from_slice(&result.payload).unwrap();
        assert!(decoded.recommendation.is_some());
        let malformed = ServiceRequest {
            payload: b"{}".to_vec(),
            ..service_request
        };
        assert!(response(&malformed).error.is_some());
    }

    #[test]
    fn unsupported_service_request_serializes_none() {
        let result = response(&ServiceRequest {
            interface_id: bcode_model::MODEL_PROVIDER_INTERFACE_ID.to_owned(),
            operation: bcode_model::OP_STARTUP_RECOMMENDATION.to_owned(),
            payload: serde_json::to_vec(&request("unsupported", "api_key")).unwrap(),
        });
        assert!(result.error.is_none());
        let decoded: StartupRecommendationResponse =
            serde_json::from_slice(&result.payload).unwrap();
        assert!(decoded.recommendation.is_none());
    }

    fn selected(catalog: &ModelCatalog, method: &str) -> StartupReadinessRequest {
        let request = request("openai", method);
        let recommendation = recommend(catalog, &request, &routing(&request).unwrap()).unwrap();
        StartupReadinessRequest {
            model_id: recommendation.model_id,
            settings: recommendation.settings,
        }
    }

    #[test]
    fn exact_readiness_requires_supported_coding_tools() {
        let catalog = ModelCatalog::load_bundled().unwrap();
        for method in ["api_key", "chatgpt", "device"] {
            let mut selection = selected(&catalog, method);
            assert_eq!(
                readiness(&catalog, &selection),
                StartupModelReadiness::Available
            );
            selection.model_id = "not-an-openai-model".to_owned();
            assert_eq!(
                readiness(&catalog, &selection),
                StartupModelReadiness::Unavailable
            );
        }
        let selection = selected(&catalog, "device");
        let mut document = catalog.document().clone();
        let entry = document
            .providers
            .get_mut("openai")
            .unwrap()
            .models
            .values_mut()
            .find(|entry| entry.model_id == selection.model_id)
            .unwrap();
        entry.capabilities.tool_use = false;
        for deployment in &mut entry.deployments {
            deployment.capabilities.tool_use = false;
        }
        assert_eq!(
            readiness(&ModelCatalog::new(document), &selection),
            StartupModelReadiness::Unavailable
        );
    }

    #[test]
    fn readiness_does_not_resolve_aliases_or_replace_unavailable_models() {
        let catalog = ModelCatalog::load_bundled().unwrap();
        let selection = selected(&catalog, "device");
        let mut document = catalog.document().clone();
        let entry = document
            .providers
            .get_mut("openai")
            .unwrap()
            .models
            .values_mut()
            .find(|entry| entry.model_id == selection.model_id)
            .unwrap();
        entry.aliases.insert("startup-test-alias".to_owned());
        entry.status = CatalogModelStatus::Unavailable;
        let catalog = ModelCatalog::new(document);
        assert_eq!(
            readiness(&catalog, &selection),
            StartupModelReadiness::Unavailable
        );
        let alias = StartupReadinessRequest {
            model_id: "startup-test-alias".to_owned(),
            ..selection
        };
        assert_eq!(
            readiness(&catalog, &alias),
            StartupModelReadiness::Unavailable
        );
    }

    #[test]
    fn custom_routes_and_missing_provider_catalog_are_unknown() {
        let catalog = ModelCatalog::load_bundled().unwrap();
        let selection = selected(&catalog, "api_key");
        for (setting, value) in [
            ("base_url", "https://custom.invalid/v1"),
            ("dialect", "unknown_surface"),
            ("catalog_provider_id", "bedrock"),
            ("auth_mode", "unknown_auth"),
        ] {
            let mut custom = selection.clone();
            custom.settings.insert(setting.to_owned(), value.to_owned());
            assert_eq!(readiness(&catalog, &custom), StartupModelReadiness::Unknown);
        }
        for (provider, endpoint) in [
            ("openrouter", "https://openrouter.ai/api/v1"),
            ("xai", DEFAULT_XAI_BASE_URL),
        ] {
            let custom = StartupReadinessRequest {
                model_id: "explicit-custom-model".to_owned(),
                settings: BTreeMap::from([
                    ("catalog_provider_id".to_owned(), provider.to_owned()),
                    ("base_url".to_owned(), endpoint.to_owned()),
                ]),
            };
            if catalog.provider(provider).is_none() {
                assert_eq!(readiness(&catalog, &custom), StartupModelReadiness::Unknown);
            }
        }
    }

    #[test]
    fn readiness_service_serializes_without_credentials_or_configuration() {
        let catalog = ModelCatalog::load_bundled().unwrap();
        let request = ServiceRequest {
            interface_id: bcode_model::MODEL_PROVIDER_INTERFACE_ID.to_owned(),
            operation: bcode_model::OP_STARTUP_READINESS.to_owned(),
            payload: serde_json::to_vec(&selected(&catalog, "device")).unwrap(),
        };
        let response = readiness_response(&request);
        assert!(response.error.is_none());
        let result: StartupReadinessResponse = serde_json::from_slice(&response.payload).unwrap();
        assert_eq!(result.model, StartupModelReadiness::Available);
        assert!(
            readiness_response(&ServiceRequest {
                payload: b"{}".to_vec(),
                ..request
            })
            .error
            .is_some()
        );
    }

    #[test]
    fn compatible_providers_do_not_borrow_an_openai_model() {
        let catalog = ModelCatalog::load_bundled().unwrap();
        for (provider, endpoint, dialect) in [
            (
                "openrouter",
                "https://openrouter.ai/api/v1",
                "chat_completions",
            ),
            ("xai", DEFAULT_XAI_BASE_URL, "responses_api"),
        ] {
            let request = request(provider, "api_key");
            let routing = routing(&request).unwrap();
            assert_eq!(routing.base_url, endpoint);
            assert_eq!(routing.dialect, dialect);
            // A catalog without this provider cannot produce an arbitrary OpenAI fallback.
            if catalog.provider(provider).is_none() {
                assert!(recommend(&catalog, &request, &routing).is_none());
            }
        }
    }
}
