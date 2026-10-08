//! Behavioral configuration merging coverage without terminal dependencies.

use std::collections::BTreeMap;

use bcode_config::TuiKeyBindingConfig;

#[test]
fn contributor_scopes_and_compatibility_sections_share_one_override_map() {
    let config = TuiKeyBindingConfig {
        chat: BTreeMap::from([("ctrl+x".into(), "legacy.exit".into())]),
        scopes: BTreeMap::from([
            (
                "chat".into(),
                BTreeMap::from([
                    ("ctrl+x".into(), "plugin.other".into()),
                    ("x".into(), String::new()),
                ]),
            ),
            (
                "viewer".into(),
                BTreeMap::from([("y".into(), "plugin.unknown".into())]),
            ),
        ]),
        ..TuiKeyBindingConfig::default()
    };
    let scopes = config.scoped_overrides_with(|id| {
        if id == "legacy.exit" {
            "tui.app.exit".into()
        } else {
            id.into()
        }
    });
    assert_eq!(scopes["chat"]["ctrl+x"], "tui.app.exit");
    assert_eq!(scopes["chat"]["x"], "");
    assert_eq!(scopes["viewer"]["y"], "plugin.unknown");
    assert_eq!(config.scoped_overrides()["chat"]["ctrl+x"], "legacy.exit");
}
