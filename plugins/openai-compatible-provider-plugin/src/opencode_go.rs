//! `OpenCode` Go routing and conversation identification.

use super::{ModelTurnRequest, Settings};

pub const BASE_URL: &str = "https://opencode.ai/inference/go/openai/v1";
const MODELS_URL: &str = "https://opencode.ai/inference/go/v1/models";

pub fn is_go_endpoint(base_url: &str) -> bool {
    base_url.trim_end_matches('/') == BASE_URL
}

pub fn models_url(settings: &Settings) -> String {
    if is_go_endpoint(&settings.base_url) {
        MODELS_URL.to_owned()
    } else {
        format!("{}/models", settings.base_url.trim_end_matches('/'))
    }
}

pub fn identify_request(
    builder: reqwest::RequestBuilder,
    settings: &Settings,
    request: &ModelTurnRequest,
) -> reqwest::RequestBuilder {
    if is_go_endpoint(&settings.base_url) {
        builder
            .header(
                "x-opencode-session",
                format!("ses_bcode_{}", request.session_id),
            )
            .header("User-Agent", concat!("bcode/", env!("CARGO_PKG_VERSION")))
    } else {
        builder
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_credentials_never_fall_back_to_other_accounts_and_grok_keeps_go_route() {
        use bcode_model::ProviderRequestContext;
        let mut context = ProviderRequestContext::default();
        context
            .settings
            .insert("base_url".to_owned(), BASE_URL.to_owned());
        context
            .settings
            .insert("dialect".to_owned(), "chat_completions".to_owned());
        context.settings.insert(
            bcode_model::CATALOG_PROVIDER_ID_SETTING.to_owned(),
            "opencode-go".to_owned(),
        );
        context.env.insert(
            "OPENAI_API_KEY".to_owned(),
            "other-provider-secret".to_owned(),
        );
        let missing = super::super::settings_for_context(&context);
        assert!(matches!(missing.auth, super::super::AuthSettings::Missing));
        context
            .env
            .insert("OPENCODE_GO_API_KEY".to_owned(), "test-go-key".to_owned());
        context
            .env
            .insert("BCODE_OPENAI_MODEL".to_owned(), "grok-4.7".to_owned());
        let settings = super::super::settings_for_context(&context);
        assert!(
            matches!(settings.auth, super::super::AuthSettings::ApiKey(ref key) if key == "test-go-key")
        );
        assert_eq!(settings.base_url, BASE_URL);
        assert_eq!(
            settings.dialect,
            super::super::OpenAiCompatibleDialect::ChatCompletions
        );
    }

    #[test]
    fn go_endpoint_precedence_fails_closed_and_environment_route_is_recognized() {
        let mut context = bcode_model::ProviderRequestContext::default();
        context.settings.insert("base_url".into(), BASE_URL.into());
        context
            .settings
            .insert("openai.base_url".into(), "https://example.com/v1".into());
        context.settings.insert(
            bcode_model::CATALOG_PROVIDER_ID_SETTING.into(),
            "opencode-go".into(),
        );
        context
            .env
            .insert("OPENCODE_GO_API_KEY".into(), "test-go-key".into());
        assert!(matches!(
            super::super::settings_for_context(&context).auth,
            super::super::AuthSettings::Missing
        ));
        context.settings.clear();
        context
            .env
            .insert("BCODE_OPENAI_BASE_URL".into(), BASE_URL.into());
        context
            .env
            .insert("BCODE_OPENAI_MODEL".into(), "grok-4.7".into());
        let settings = super::super::settings_for_context(&context);
        assert_eq!(settings.base_url, BASE_URL);
        assert!(
            matches!(settings.auth, super::super::AuthSettings::ApiKey(ref key) if key == "test-go-key")
        );
    }

    #[test]
    fn go_model_list_keeps_its_support_target_and_unknown_cache_metadata() {
        let mut request = bcode_model::ModelListRequest::default();
        request.provider_context.settings = std::collections::BTreeMap::from([
            ("base_url".into(), BASE_URL.into()),
            ("dialect".into(), "chat_completions".into()),
            ("live_discovery".into(), "false".into()),
            (
                bcode_model::CATALOG_PROVIDER_ID_SETTING.into(),
                "opencode-go".into(),
            ),
        ]);
        let plugin = super::super::OpenAiCompatibleProviderPlugin::default();
        let result = plugin.models(&request);
        assert_ne!(result.models, []);
        for model in result.models {
            assert_eq!(model.cache, bcode_model::ModelCacheInfo::default());
        }
        let bcode_model::ModelCatalogPolicy::ExpandSupported {
            provider_id,
            target,
            ..
        } = result.catalog.policy
        else {
            panic!("Go catalog expansion expected")
        };
        assert_eq!(provider_id, "opencode-go");
        assert_eq!(target.provider, "opencode-go");
        assert_eq!(target.api_surface, "chat_completions");
        assert_eq!(target.auth_mode, "api_key");
    }

    #[test]
    fn go_headers_are_stable_per_conversation_and_not_sent_elsewhere() {
        let mut settings = super::super::settings();
        settings.base_url = BASE_URL.to_owned();
        let request = super::super::tests::test_request(Vec::new());
        let client = reqwest::Client::new();
        let build = |settings: &Settings, request: &ModelTurnRequest| {
            identify_request(client.post(BASE_URL), settings, request)
                .build()
                .unwrap()
        };
        let first = build(&settings, &request);
        let retry = build(&settings, &request);
        assert_eq!(
            first.headers()["x-opencode-session"],
            retry.headers()["x-opencode-session"]
        );
        assert_eq!(
            first.headers()["User-Agent"],
            concat!("bcode/", env!("CARGO_PKG_VERSION"))
        );
        let mut another = request.clone();
        another.session_id = bcode_session_models::SessionId::new();
        assert_ne!(
            first.headers()["x-opencode-session"],
            build(&settings, &another).headers()["x-opencode-session"]
        );
        settings.base_url = "https://example.com/v1".to_owned();
        assert!(
            !build(&settings, &request)
                .headers()
                .contains_key("x-opencode-session")
        );
    }

    #[test]
    fn go_route_is_exact_and_discovery_uses_documented_endpoint() {
        assert!(is_go_endpoint(BASE_URL));
        assert!(is_go_endpoint(&format!("{BASE_URL}/")));
        for other in [
            "https://opencode.ai/inference/openai/v1",
            "https://opencode.ai.evil.example/inference/go/openai/v1",
            "https://example.com/inference/go/openai/v1",
            "http://opencode.ai/inference/go/openai/v1",
        ] {
            assert!(!is_go_endpoint(other));
        }
        let mut settings = super::super::settings();
        settings.base_url = BASE_URL.to_owned();
        assert_eq!(models_url(&settings), MODELS_URL);
        settings.base_url = "https://example.com/v1/".to_owned();
        assert_eq!(models_url(&settings), "https://example.com/v1/models");
    }
}
