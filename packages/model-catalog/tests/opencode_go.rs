#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use bcode_model::ModelCapability;
use bcode_model_catalog::{ModelCatalog, load_embedded_catalog};
use bcode_model_catalog_models::ModelSupportTarget;

#[test]
fn go_baseline_has_exact_api_key_chat_deployments_without_cache_claims() {
    let catalog = ModelCatalog::new(load_embedded_catalog().unwrap());
    let provider = catalog.provider("opencode-go").unwrap();
    assert_eq!(provider.default_model_id.as_deref(), Some("glm-5.3-flash"));
    let target =
        ModelSupportTarget::new("opencode-go", "api_key", "chat_completions", Some("bcode"));
    let models = catalog.provider_models_for_support_target("opencode-go", &target, false);
    assert_eq!(models.len(), 5);
    for (id, context, output) in [
        ("glm-5.3-flash", 1_000_000, 131_072),
        ("glm-5.3", 1_000_000, 131_072),
        ("kimi-k2.7-code", 262_144, 262_144),
        ("gemini-4-argon", 1_048_576, 65_536),
        ("kimi-k3.1-pro", 1_048_576, 131_072),
    ] {
        let model = models.iter().find(|model| model.model_id == id).unwrap();
        assert_eq!(model.context_window, Some(context));
        assert_eq!(model.max_output_tokens, Some(output));
        assert!(model.capabilities.contains(&ModelCapability::ToolCalls));
        let entry = catalog.model("opencode-go", id).unwrap();
        assert!(!entry.capabilities.prompt_cache);
        assert_eq!(
            entry.source.metadata.as_deref(),
            Some("https://opencode.ai/inference/go/v1/models")
        );
    }
    for target in [
        ModelSupportTarget::new("opencode-go", "api_key", "responses_api", Some("bcode")),
        ModelSupportTarget::new(
            "opencode-go",
            "chatgpt_subscription",
            "chat_completions",
            Some("bcode"),
        ),
    ] {
        assert_eq!(
            catalog.provider_models_for_support_target("opencode-go", &target, false),
            []
        );
    }
    assert!(catalog.model("opencode-go", "beam-free").is_none());
    assert!(catalog.model("opencode-go", "unlisted-model").is_none());
}
