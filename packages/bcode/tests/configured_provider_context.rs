#![cfg(feature = "config")]

use std::collections::BTreeMap;

use bcode::Bcode;
use bcode_config::{
    AuthCredentialMapping, AuthProfileConfig, BcodeConfig, ModelConfig, ModelProfileConfig,
};

#[test]
fn configured_builder_materializes_selected_provider_context() {
    let mut config = BcodeConfig::default();
    config.model = ModelConfig {
        provider_plugin_id: Some("provider".to_owned()),
        model_id: Some("model".to_owned()),
        profile: Some("default".to_owned()),
        profiles: BTreeMap::from([(
            "default".to_owned(),
            ModelProfileConfig {
                provider_plugin_id: "provider".to_owned(),
                model_id: Some("model".to_owned()),
                auth_profile: Some("work".to_owned()),
                ..ModelProfileConfig::default()
            },
        )]),
        ..ModelConfig::default()
    };
    config.auth.profiles.insert(
        "work".to_owned(),
        AuthProfileConfig {
            backend: "env".to_owned(),
            provider_id: None,
            owner_plugin_id: None,
            scheme: Some("bearer".to_owned()),
            map: BTreeMap::from([(
                "token".to_owned(),
                AuthCredentialMapping {
                    env: Some("BCODE_TEST_TOKEN".to_owned()),
                    key: None,
                },
            )]),
            settings: BTreeMap::new(),
        },
    );

    let bcode = Bcode::builder()
        .provider_defaults_from_config(&config)
        .build();
    let report = bcode
        .provider_registry()
        .default_selection_report()
        .expect("configured model selection");

    assert_eq!(report.selector.provider_plugin_id(), Some("provider"));
    assert_eq!(report.selector.model_id(), "model");
    assert_eq!(
        bcode.provider_context().auth_profile.as_deref(),
        Some("work")
    );
    assert_eq!(
        bcode
            .agent()
            .build()
            .provider_context()
            .auth_profile
            .as_deref(),
        Some("work")
    );
}
