use bcode_config::BcodeConfig;

#[test]
fn model_selection_labels_are_scoped_and_presentation_only() {
    let mut config: BcodeConfig = toml::from_str(
        r#"
[model]
profile = "custom"
[model.profiles.custom]
provider_plugin_id = "provider"
model_id = "custom-alias"
display_name = "Custom profile"
[model.aliases.custom-alias]
provider_plugin_id = "provider"
model_id = "native-model"
display_name = "Custom alias"
"#,
    )
    .unwrap();
    let resolve = |config: &BcodeConfig, provider, model, requested| {
        config.model_selection_display_name(Some(provider), Some(model), requested)
    };
    let selection = config.resolved_model_selection();
    assert_eq!(selection.model_id.as_deref(), Some("native-model"));
    assert_eq!(
        resolve(&config, "provider", "native-model", None).as_deref(),
        Some("Custom profile")
    );
    assert_eq!(
        resolve(&config, "provider", "native-model", Some("custom-alias")).as_deref(),
        Some("Custom profile")
    );
    assert_eq!(resolve(&config, "provider", "other-model", None), None);
    assert_eq!(
        resolve(&config, "other-provider", "native-model", None),
        None
    );
    assert_eq!(
        resolve(&config, "provider", "native-model", Some("native-model")),
        None
    );
    config
        .model
        .profiles
        .get_mut("custom")
        .unwrap()
        .display_name = Some(" ".into());
    assert_eq!(
        resolve(&config, "provider", "native-model", None).as_deref(),
        Some("Custom alias")
    );
    config.model.profile = None;
    assert_eq!(
        resolve(&config, "provider", "native-model", Some("custom-alias")).as_deref(),
        Some("Custom alias")
    );
    assert_eq!(resolve(&config, "provider", "native-model", None), None);
    config
        .model
        .aliases
        .get_mut("custom-alias")
        .unwrap()
        .display_name = None;
    assert_eq!(
        resolve(&config, "provider", "native-model", Some("custom-alias")),
        None
    );
}
