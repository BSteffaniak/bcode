#![cfg(all(feature = "config", feature = "embedded-plugins"))]

use std::collections::BTreeMap;

use bcode::Bcode;
use bcode_config::{
    BcodeConfig, ModelConfig, ModelProfileConfig, RuntimeAuthBinding, RuntimeAuthProfile,
    RuntimeAuthSubscriptions,
};
use bcode_plugin::{PluginRuntimeHost, PluginSelection};

fn configured_bcode() -> Bcode {
    let plugins = PluginRuntimeHost::load_defaults_with_static_bundled(
        &PluginSelection::all_enabled(),
        &bcode_bundled_plugins::static_bundled_plugins(),
    )
    .expect("bundled provider should load");
    Bcode::builder().plugin_runtime(plugins).build()
}

#[test]
fn setup_report_uses_plugin_metadata_and_runtime_auth_without_secrets() {
    let mut config = BcodeConfig::default();
    config.model = ModelConfig {
        provider_plugin_id: Some("bcode.openai-compatible".to_owned()),
        model_id: Some("gpt-test".to_owned()),
        profiles: BTreeMap::from([(
            "alternate".to_owned(),
            ModelProfileConfig {
                provider_plugin_id: "bcode.openai-compatible".to_owned(),
                model_id: Some("gpt-other".to_owned()),
                ..ModelProfileConfig::default()
            },
        )]),
        ..ModelConfig::default()
    };
    let runtime = RuntimeAuthSubscriptions {
        bindings: BTreeMap::from([(
            "openai".to_owned(),
            RuntimeAuthBinding {
                profile: "openai-work".to_owned(),
                owner_plugin_id: "bcode.openai-compatible".to_owned(),
            },
        )]),
        profiles: BTreeMap::from([(
            "openai-work".to_owned(),
            RuntimeAuthProfile {
                provider_id: "openai".to_owned(),
                owner_plugin_id: "bcode.openai-compatible".to_owned(),
                backend: "sshenv".to_owned(),
                scheme: "api_key".to_owned(),
                storage_profile: "openai-work".to_owned(),
                ..RuntimeAuthProfile::default()
            },
        )]),
        ..RuntimeAuthSubscriptions::default()
    };

    let report = configured_bcode().provider_setup_report_from_config(&config, &runtime);

    assert_eq!(report.candidates.len(), 2);
    let default = &report.candidates[0];
    assert_eq!(default.auth.provider_id.as_deref(), Some("openai"));
    assert_eq!(default.auth.display_name.as_deref(), Some("OpenAI"));
    assert_eq!(default.auth.profile.as_deref(), Some("openai-work"));
    assert!(default.auth.ready);
    assert!(
        default
            .auth
            .methods
            .iter()
            .any(|method| method.method_id == "api_key")
    );
    let serialized = serde_json::to_string(&report).expect("report should serialize");
    assert!(!serialized.contains("vault"));
    assert!(!serialized.contains("BCODE_OPENAI_API_KEY"));
}
