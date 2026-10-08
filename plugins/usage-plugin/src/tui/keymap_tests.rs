use super::{Dashboard, keymap_actions};
use bcode_plugin_sdk::tui::keymap::{KeymapRegistry, controls::resolve_registry};
use bcode_plugin_sdk::tui::{PluginTuiAction, PluginTuiHost, PluginTuiSurface};
use bmux_keyboard::{KeyCode, KeyStroke, parse_key_stroke};
use bmux_tui::event::Event;
use std::collections::BTreeMap;

struct Host(KeymapRegistry);
impl PluginTuiHost for Host {
    fn spawn(&self, _: bcode_plugin_sdk::tui::PluginTask) {}
    fn spawn_blocking(&self, _: Box<dyn FnOnce() + Send + 'static>) {}
    fn request_redraw(&self) {}
    fn keymap_action(&self, scope: &str, key: KeyStroke) -> Option<String> {
        self.0.action_for_key(scope, key).map(str::to_owned)
    }
    fn keymap_is_overridden(&self, scope: &str, key: KeyStroke) -> bool {
        self.0.is_overridden_key(scope, key)
    }
    fn keymap_keys(&self, scope: &str, action: &str) -> Vec<String> {
        self.0
            .keys_for_action(scope, action)
            .into_iter()
            .map(str::to_owned)
            .collect()
    }
}

#[test]
fn literal_input_and_explicit_unbind_are_distinct() {
    use bcode_plugin_sdk::tui::keymap::controls::{ControlAction, ControlInput, LiteralText};
    let host = Host(
        resolve_registry(
            keymap_actions(),
            BTreeMap::from([(
                "usage-model-picker".into(),
                BTreeMap::from([("z".into(), String::new())]),
            )]),
        )
        .unwrap(),
    );
    assert_eq!(
        ControlInput::from_host(
            &host,
            "usage-model-picker",
            parse_key_stroke("z").unwrap(),
            ControlAction::TEXT,
            true
        ),
        ControlInput::Ignored
    );
    assert_eq!(
        ControlInput::from_host(
            &host,
            "usage-model-picker",
            KeyStroke::simple(KeyCode::Char('λ')),
            ControlAction::TEXT,
            true
        ),
        ControlInput::Text(LiteralText('λ'))
    );
}

#[test]
fn remapping_and_unbinding_do_not_reach_generic_controls() {
    let host = Host(
        resolve_registry(
            keymap_actions(),
            BTreeMap::from([(
                "usage".into(),
                BTreeMap::from([
                    ("ctrl+x".into(), "bcode.usage.close".into()),
                    ("d".into(), String::new()),
                ]),
            )]),
        )
        .unwrap(),
    );
    let mut surface = Dashboard::new(None);
    surface.prepare_keymap(&host);
    assert!(surface.shortcuts.contains("ctrl+x Close"));
    surface.handle_event(&Event::Key(parse_key_stroke("d").unwrap()), &host);
    assert!(!surface.details);
    assert!(!matches!(
        surface.handle_event(&Event::Key(parse_key_stroke("q").unwrap()), &host),
        PluginTuiAction::Close { .. }
    ));
    assert!(matches!(
        surface.handle_event(&Event::Key(parse_key_stroke("ctrl+x").unwrap()), &host),
        PluginTuiAction::Close { .. }
    ));
}
