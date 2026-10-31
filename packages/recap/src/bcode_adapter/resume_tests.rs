use super::*;

use bcode_recap::model_configuration::ResumeModelConfiguration;

fn model_options(path: std::path::PathBuf) -> BcodeOptions {
    BcodeOptions {
        config_path: path,
        profile: None,
        context: None,
        model: None,
        provider: None,
        endpoint: None,
        auth_profile: None,
        max_output_tokens: 8192,
        timeout: Duration::from_secs(180),
    }
}

#[tokio::test]
async fn saved_settings_fail_closed_before_provider_resolution_when_configuration_changes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(
        &path,
        "[model]\nprovider_plugin_id = 'bcode.openai-compatible'\nmodel_id = 'test-model'\n",
    )
    .unwrap();
    let options = model_options(path.clone());
    let saved = ResumeModelConfiguration {
        version: 1,
        identity: "original-generation".into(),
        options: options.clone(),
        selection_fingerprint: "unused".into(),
        config_fingerprint: configuration_fingerprint(&options).unwrap(),
    };
    std::fs::write(
        &path,
        "[model]\nprovider_plugin_id = 'bcode.openai-compatible'\nmodel_id = 'different-model'\n",
    )
    .unwrap();
    assert!(matches!(
        BcodeExecutor::configured_resume(saved.clone(), options.clone(), false).await,
        Err(ModelFailure::Configuration)
    ));
    let mut future = saved;
    future.version = 2;
    assert!(matches!(
        BcodeExecutor::configured_resume(future, options.clone(), true).await,
        Err(ModelFailure::Configuration)
    ));
    // The legacy path must also fail before inference when reconstruction is impossible.
    let missing = model_options(directory.path().join("missing.toml"));
    assert!(
        BcodeExecutor::configured_legacy(missing, "original")
            .await
            .is_err()
    );
}

#[test]
fn saved_options_preserve_effective_references_and_absolute_paths() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(
        &path,
        "[model]\nprovider_plugin_id = 'bcode.openai-compatible'\nmodel_id = 'test-model'\n",
    )
    .unwrap();
    let selection = crate::connections::Selection {
        config_path: Some(path.clone()),
        profile: None,
        model: Some("original-model".into()),
        ..Default::default()
    };
    let retained = retained_options(&selection, 10000, Duration::from_secs(300)).unwrap();
    assert_eq!(retained.config_path, path.canonicalize().unwrap());
    assert_eq!(retained.model, selection.model);
    assert_eq!(retained.max_output_tokens, 10000);
    assert_eq!(retained.timeout, Duration::from_secs(300));
    let restored: BcodeOptions =
        serde_json::from_slice(&serde_json::to_vec(&retained).unwrap()).unwrap();
    assert_eq!(retained, restored);
}
