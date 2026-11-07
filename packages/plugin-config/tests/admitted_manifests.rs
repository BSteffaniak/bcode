#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

use std::collections::BTreeMap;

#[test]
fn admitted_manifests_are_the_complete_configuration_scope() {
    let manifest: bcode_plugin::PluginManifest = toml::from_str(
        "id = 'example.selected'\nname = 'Selected'\nversion = '1.0.0'\n[runtime]\ntype = 'native'\nabi_version = 1\nlibrary = 'unused'\n",
    ).unwrap();
    let mut config = bcode_config::BcodeConfig::default();
    config.plugins.config.insert(
        "example.selected".into(),
        toml::from_str("value = 'retained'").unwrap(),
    );
    config.plugins.config.insert(
        "example.unloaded".into(),
        toml::from_str("value = 'excluded'").unwrap(),
    );
    let overlays = BTreeMap::from([("example.selected".into(), serde_json::json!({"added":true}))]);
    let base = std::path::Path::new("/private/plugin-state");
    let resolved = bcode_plugin_config::resolve_plugin_configs_for_manifests(
        &config,
        [&manifest],
        base,
        &overlays,
    );
    assert_eq!(resolved.len(), 1);
    let selected = &resolved["example.selected"];
    assert_eq!(
        selected.config,
        serde_json::json!({"value":"retained", "added":true})
    );
    assert_eq!(selected.redacted_config, selected.config);
    assert_eq!(
        selected.state_root.as_deref(),
        Some(base.join("example.selected").as_path())
    );
    let empty = bcode_plugin_config::resolve_plugin_configs_for_manifests(
        &config,
        std::iter::empty(),
        base,
        &overlays,
    );
    assert_eq!(empty.len(), 0);
}
