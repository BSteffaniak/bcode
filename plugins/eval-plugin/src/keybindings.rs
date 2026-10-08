//! Contributor-owned eval shortcuts and terminal-control adaptation.

use bcode_plugin_sdk::tui::{
    PluginTuiHost,
    keymap::{
        KeymapAction,
        controls::{ControlAction, ControlInput},
    },
};
use bmux_keyboard::parse_key_stroke;
use bmux_tui::event::Event;

pub const PICKER: &str = "eval-run-picker";
pub const RUN: &str = "eval-run-viewer";
pub const CAMPAIGN: &str = "eval-campaign";
pub const DETAIL: &str = "eval-generation";
pub const WIZARD: &str = "eval-wizard";

#[derive(serde::Deserialize)]
struct Declarations {
    keymap_actions: Vec<KeymapAction>,
}
pub fn actions_for(scope: &str) -> Vec<KeymapAction> {
    actions()
        .into_iter()
        .filter(|action| action.scope == scope)
        .collect()
}
pub fn actions() -> Vec<KeymapAction> {
    static ACTIONS: std::sync::OnceLock<Vec<KeymapAction>> = std::sync::OnceLock::new();
    ACTIONS
        .get_or_init(|| {
            toml::from_str::<Declarations>(include_str!("../bcode-plugin.toml"))
                .expect("bundled keymap declarations are valid")
                .keymap_actions
        })
        .clone()
}

/// Only resolved semantic actions or literal wizard text reach native controls.
pub fn event(event: &Event, host: &dyn PluginTuiHost, scope: &str) -> Option<Event> {
    let Event::Key(stroke) = event else {
        return Some(event.clone());
    };
    if let Some(id) = host.keymap_action(scope, *stroke) {
        if !actions_for(scope).iter().any(|action| action.id == id) {
            return None;
        }
        if let Some(control) = controls().into_iter().find(|control| control.id() == id) {
            return Some(Event::Key(control.native_stroke()));
        }
        let definition = actions_for(scope)
            .into_iter()
            .find(|action| action.id == id)?;
        return definition
            .defaults
            .first()
            .and_then(|key| parse_key_stroke(key).ok())
            .map(Event::Key);
    }
    if scope == WIZARD {
        return ControlInput::from_host(host, scope, *stroke, &[], true)
            .native_stroke()
            .map(Event::Key);
    }
    None
}

pub fn hints(host: &dyn PluginTuiHost, scope: &str) -> Vec<(String, String)> {
    actions_for(scope)
        .into_iter()
        .filter_map(|action| {
            let keys = host.keymap_keys(scope, &action.id);
            (!keys.is_empty()).then(|| (keys.join("/"), action.description))
        })
        .collect()
}

fn controls() -> Vec<ControlAction> {
    ControlAction::TEXT
        .iter()
        .chain(ControlAction::NAVIGATION)
        .copied()
        .chain([
            ControlAction::Submit,
            ControlAction::Cancel,
            ControlAction::FocusNext,
            ControlAction::FocusPrevious,
            ControlAction::Toggle,
        ])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_plugin_sdk::tui::keymap::{KeymapRegistry, controls::resolve_registry};
    use bcode_plugin_sdk::tui::{PluginTask, PluginTuiHost};
    use bmux_keyboard::KeyStroke;
    use std::collections::BTreeMap;

    struct Host(KeymapRegistry);
    impl PluginTuiHost for Host {
        fn spawn(&self, _task: PluginTask) {}
        fn spawn_blocking(&self, _task: Box<dyn FnOnce() + Send + 'static>) {}
        fn request_redraw(&self) {}
        fn keymap_action(&self, scope: &str, stroke: KeyStroke) -> Option<String> {
            self.0.action_for_key(scope, stroke).map(str::to_owned)
        }
        fn keymap_keys(&self, scope: &str, action: &str) -> Vec<String> {
            self.0
                .keys_for_action(scope, action)
                .into_iter()
                .map(str::to_owned)
                .collect()
        }
        fn keymap_is_overridden(&self, scope: &str, stroke: KeyStroke) -> bool {
            self.0.is_overridden_key(scope, stroke)
        }
    }

    #[test]
    fn contributor_remap_and_unbind_dispatch_match_effective_hints() {
        let scope = PICKER;
        let action = "bcode.eval.picker.refresh";
        let mut registry = resolve_registry(
            actions(),
            BTreeMap::from([(
                scope.into(),
                BTreeMap::from([("ctrl+r".into(), action.into())]),
            )]),
        )
        .unwrap();
        assert_eq!(
            registry.action_for_key(scope, parse_key_stroke("r").unwrap()),
            None
        );
        assert_eq!(registry.keys_for_action(scope, action), ["ctrl+r"]);
        let host = Host(registry.clone());
        assert_eq!(
            event(&Event::Key(parse_key_stroke("r").unwrap()), &host, scope),
            None
        );
        assert_eq!(
            event(
                &Event::Key(parse_key_stroke("ctrl+r").unwrap()),
                &host,
                scope
            ),
            Some(Event::Key(parse_key_stroke("r").unwrap()))
        );
        assert!(hints(&host, scope).iter().any(|(key, _)| key == "ctrl+r"));
        registry
            .configure(BTreeMap::from([(
                scope.into(),
                BTreeMap::from([("r".into(), String::new())]),
            )]))
            .unwrap();
        assert_eq!(
            registry.action_for_key(scope, parse_key_stroke("r").unwrap()),
            None
        );
        assert!(registry.keys_for_action(scope, action).is_empty());
        let host = Host(registry);
        assert_eq!(
            event(&Event::Key(parse_key_stroke("r").unwrap()), &host, scope),
            None
        );
    }

    #[test]
    fn picker_effective_close_binding_routes_real_surface_and_initial_hints() {
        use bcode_plugin_sdk::tui::{PluginTuiAction, PluginTuiSurface};
        use bmux_tui::{buffer::Buffer, frame::Frame, geometry::Rect, paint::PaintCx};
        let registry = resolve_registry(
            actions(),
            BTreeMap::from([(
                PICKER.into(),
                BTreeMap::from([("ctrl+q".into(), "bcode.eval.close".into())]),
            )]),
        )
        .unwrap();
        let host = Host(registry);
        let mut surface = crate::eval_viewer::EvalRunPickerSurface::load(std::path::PathBuf::from(
            "/nonexistent/bcode-keymap-test/runs",
        ));
        let area = Rect::new(0, 0, 512, 18);
        let mut buffer = Buffer::empty(area);
        surface.render_with_host(
            area,
            &mut PaintCx::new(&mut Frame::new(&mut buffer)),
            None,
            &host,
        );
        let text = (0..area.height)
            .filter_map(|row| buffer.row_symbols(row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("ctrl+q"));
        assert!(matches!(
            surface.handle_event(&Event::Key(parse_key_stroke("q").unwrap()), &host),
            PluginTuiAction::None
        ));
        assert!(matches!(
            surface.handle_event(&Event::Key(parse_key_stroke("ctrl+q").unwrap()), &host),
            PluginTuiAction::Close { .. }
        ));
    }

    #[test]
    fn wizard_literal_text_does_not_bypass_explicit_unbind() {
        let registry = resolve_registry(
            actions(),
            BTreeMap::from([(WIZARD.into(), BTreeMap::from([("j".into(), String::new())]))]),
        )
        .unwrap();
        let host = Host(registry);
        assert_eq!(
            event(&Event::Key(parse_key_stroke("j").unwrap()), &host, WIZARD),
            None
        );
        assert_eq!(
            event(&Event::Key(parse_key_stroke("z").unwrap()), &host, WIZARD),
            Some(Event::Key(parse_key_stroke("z").unwrap()))
        );
        assert_eq!(
            event(
                &Event::Key(parse_key_stroke("ctrl+z").unwrap()),
                &host,
                WIZARD
            ),
            None
        );
    }

    #[test]
    fn declared_bindings_are_unique_and_all_modes_register() {
        let registry = resolve_registry(actions(), BTreeMap::new()).unwrap();
        for action in actions() {
            assert!(
                !registry
                    .keys_for_action(&action.scope, &action.id)
                    .is_empty(),
                "{} {}",
                action.scope,
                action.id
            );
        }
    }
}
