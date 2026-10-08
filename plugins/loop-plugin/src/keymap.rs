//! Loop-owned semantic actions and BMUX adaptation.
pub use bcode_plugin_sdk::tui::keymap::controls::ControlAction;
use bcode_plugin_sdk::tui::keymap::{KeymapAction, controls::ControlInput};
use bcode_plugin_sdk::tui::{PluginTuiHost, PluginTuiRegistry};
use bmux_keyboard::{KeyCode, KeyStroke, Modifiers};
use bmux_tui::event::Event;

pub const SETUP: &str = "loop-setup";
pub const GOAL: &str = "goal-setup";
pub const SWARM: &str = "goal-swarm";
pub const LIVE: &str = "goal-generation";
pub const DOCUMENT: &str = "goal-document";
pub const SUPERVISION: &str = "goal-supervision";
pub const JUDGEMENT: &str = "loop-judgement";

fn custom(scope: &str, name: &str, keys: &[&str]) -> KeymapAction {
    KeymapAction {
        id: format!("bcode.loop.{name}"),
        scope: scope.into(),
        description: name.replace('_', " "),
        defaults: keys.iter().map(|key| (*key).into()).collect(),
    }
}

pub fn actions() -> Vec<KeymapAction> {
    let mut actions = Vec::new();
    for scope in [SETUP, GOAL, SWARM, LIVE, DOCUMENT, SUPERVISION, JUDGEMENT] {
        let controls = if matches!(scope, DOCUMENT | SUPERVISION | LIVE) {
            ControlAction::NAVIGATION
        } else {
            ControlAction::TEXT
        };
        actions.extend(controls.iter().map(|action| action.definition(scope)));
        if !matches!(scope, DOCUMENT | SUPERVISION | LIVE) {
            for action in [ControlAction::PageUp, ControlAction::PageDown] {
                actions.push(action.definition(scope));
            }
        }
        for action in [
            ControlAction::Cancel,
            ControlAction::FocusNext,
            ControlAction::FocusPrevious,
            ControlAction::Submit,
            ControlAction::Toggle,
            ControlAction::Newline,
        ] {
            actions.push(action.definition(scope));
        }
    }
    for scope in [SETUP, GOAL] {
        actions.push(custom(scope, "start", &["ctrl+enter"]));
    }
    for (name, keys) in [
        ("swarm", &["ctrl+s"][..]),
        ("delegation_check", &["ctrl+d"]),
        ("progress", &["ctrl+p"]),
        ("generate", &["ctrl+r"]),
    ] {
        actions.push(custom(GOAL, name, keys));
    }
    actions.push(custom(LIVE, "open_session", &["h"]));
    for (name, keys) in [
        ("refresh", &["r", "shift+r"][..]),
        ("threshold_up", &["+"]),
        ("threshold_down", &["-"]),
        ("fallback", &["f", "shift+f"]),
    ] {
        actions.push(custom(JUDGEMENT, name, keys));
    }
    for index in 1..=9 {
        actions.push(custom(
            SUPERVISION,
            &format!("child_{index}"),
            &[&index.to_string()],
        ));
    }
    actions
}

pub fn register(registry: &mut PluginTuiRegistry) {
    for action in actions() {
        registry.register_keymap_action(action);
    }
}

pub fn hint(host: &dyn PluginTuiHost, scope: &str, name: &str) -> String {
    let keys = host.keymap_keys(scope, &format!("bcode.loop.{name}"));
    if keys.is_empty() {
        "unbound".into()
    } else {
        keys.join("/")
    }
}

/// Never feed unresolved shortcut keys into components. Text is admitted separately.
pub fn event(host: &dyn PluginTuiHost, scope: &str, event: &Event, text: bool) -> Option<Event> {
    let Event::Key(stroke) = event else {
        return Some(event.clone());
    };
    if let Some(action) = host.keymap_action(scope, *stroke) {
        let native = match action.as_str() {
            "bcode.loop.start" => Some((KeyCode::Enter, true)),
            "bcode.loop.swarm" => Some((KeyCode::Char('s'), true)),
            "bcode.loop.delegation_check" => Some((KeyCode::Char('d'), true)),
            "bcode.loop.progress" => Some((KeyCode::Char('p'), true)),
            "bcode.loop.generate" => Some((KeyCode::Char('r'), true)),
            "bcode.loop.open_session" => Some((KeyCode::Char('h'), false)),
            "bcode.loop.refresh" => Some((KeyCode::Char('r'), false)),
            "bcode.loop.threshold_up" => Some((KeyCode::Char('+'), false)),
            "bcode.loop.threshold_down" => Some((KeyCode::Char('-'), false)),
            "bcode.loop.fallback" => Some((KeyCode::Char('f'), false)),
            _ => None,
        };
        if let Some((key, ctrl)) = native {
            return Some(Event::Key(KeyStroke {
                key,
                modifiers: Modifiers {
                    ctrl,
                    ..Modifiers::NONE
                },
            }));
        }
    }
    let mut supported = ControlAction::TEXT.to_vec();
    supported.extend_from_slice(ControlAction::NAVIGATION);
    supported.extend([
        ControlAction::Cancel,
        ControlAction::Submit,
        ControlAction::Toggle,
        ControlAction::Newline,
        ControlAction::FocusNext,
        ControlAction::FocusPrevious,
    ]);
    let input = ControlInput::from_host(host, scope, *stroke, &supported, text);
    if text && input == ControlInput::Action(ControlAction::Toggle) {
        return Some(Event::Key(KeyStroke::simple(KeyCode::Char(' '))));
    }
    input.native_stroke().map(Event::Key)
}

pub fn control_hint(host: &dyn PluginTuiHost, scope: &str, action: ControlAction) -> String {
    let keys = host.keymap_keys(scope, &action.id());
    if keys.is_empty() {
        "unbound".into()
    } else {
        keys.join("/")
    }
}
pub fn live_hint(host: &dyn PluginTuiHost) -> String {
    format!(
        "Draft output · {} opens live session · {} folds output · {} cancels",
        hint(host, LIVE, "open_session"),
        control_hint(host, LIVE, ControlAction::FocusNext),
        control_hint(host, LIVE, ControlAction::Cancel)
    )
}

#[cfg(test)]
pub fn test_registry() -> bcode_plugin_sdk::tui::keymap::KeymapRegistry {
    bcode_plugin_sdk::tui::keymap::controls::resolve_registry(
        actions(),
        std::collections::BTreeMap::new(),
    )
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PluginTuiSurface;
    use bcode_plugin_sdk::tui::keymap::{KeymapRegistry, controls::resolve_registry};
    use std::collections::BTreeMap;

    struct Host(KeymapRegistry);
    impl PluginTuiHost for Host {
        fn spawn(&self, _: bcode_plugin_sdk::tui::PluginTask) {}
        fn spawn_blocking(&self, _: Box<dyn FnOnce() + Send + 'static>) {}
        fn request_redraw(&self) {}
        fn keymap_action(&self, scope: &str, stroke: KeyStroke) -> Option<String> {
            self.0.action_for_key(scope, stroke).map(str::to_owned)
        }
        fn keymap_is_overridden(&self, scope: &str, stroke: KeyStroke) -> bool {
            self.0.is_overridden_key(scope, stroke)
        }
        fn keymap_keys(&self, scope: &str, action: &str) -> Vec<String> {
            self.0
                .keys_for_action(scope, action)
                .into_iter()
                .map(str::to_owned)
                .collect()
        }
    }
    fn host(scope: &str, overrides: &[(&str, &str)]) -> Host {
        Host(
            resolve_registry(
                actions(),
                BTreeMap::from([(
                    scope.into(),
                    overrides
                        .iter()
                        .map(|(key, action)| ((*key).into(), (*action).into()))
                        .collect(),
                )]),
            )
            .unwrap(),
        )
    }
    fn key(code: KeyCode) -> Event {
        Event::Key(KeyStroke::simple(code))
    }

    #[test]
    fn remapped_cancel_controls_surface_and_does_not_forward_old_escape() {
        let host = host(SETUP, &[("ctrl+x", "tui.control.cancel")]);
        assert!(event(&host, SETUP, &key(KeyCode::Escape), false).is_none());
        let remap = Event::Key(KeyStroke {
            key: KeyCode::Char('x'),
            modifiers: Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
        });
        let mut surface = crate::LoopSurface::new(None);
        assert!(matches!(
            surface.handle_event(&key(KeyCode::Escape), &host),
            crate::PluginTuiAction::None
        ));
        assert!(matches!(
            surface.handle_event(&remap, &host),
            crate::PluginTuiAction::Close { .. }
        ));
    }

    #[test]
    fn explicit_unbind_blocks_text_and_navigation_but_literal_text_survives() {
        let host = host(SETUP, &[("x", ""), ("up", "")]);
        assert!(event(&host, SETUP, &key(KeyCode::Char('x')), true).is_none());
        assert!(event(&host, SETUP, &key(KeyCode::Up), true).is_none());
        assert!(
            matches!(event(&host, SETUP, &key(KeyCode::Char('λ')), true), Some(Event::Key(stroke)) if stroke.key == KeyCode::Char('λ'))
        );
    }

    #[test]
    fn initial_render_uses_effective_remap_and_unbind_without_input_or_poll() {
        let host = host(SETUP, &[("ctrl+x", "bcode.loop.start"), ("escape", "")]);
        let mut surface = crate::LoopSurface::new(None);
        let area = crate::Rect::new(0, 0, 100, 40);
        let mut buffer = bmux_tui::buffer::Buffer::empty(area);
        surface.render_with_host(
            area,
            &mut crate::PaintCx::new(&mut crate::Frame::new(&mut buffer)),
            None,
            &host,
        );
        assert_eq!(surface.shortcut("start"), "ctrl+x");
        assert_eq!(surface.shortcut("tui.control.cancel"), "unbound");
        let text = (0..area.height)
            .flat_map(|y| (0..area.width).map(move |x| (x, y)))
            .map(|(x, y)| {
                buffer
                    .get(bmux_tui::geometry::Point::new(x, y))
                    .map_or("", |cell| cell.symbol.as_str())
            })
            .collect::<String>();
        assert!(text.contains("ctrl+x"));
        assert!(text.contains("unbound"));
        assert!(!text.contains("Ctrl-Enter"));
    }

    #[test]
    fn slice_hints_refresh_after_effective_binding_changes() {
        let first = host(GOAL, &[("ctrl+x", "bcode.loop.generate")]);
        let second = host(GOAL, &[("ctrl+y", "bcode.loop.generate"), ("ctrl+s", "")]);
        let mut surface = crate::LoopSurface::new(None);
        surface.origin = crate::SetupKind::Goal;
        let area = crate::Rect::new(0, 0, 100, 40);
        let mut buffer = bmux_tui::buffer::Buffer::empty(area);
        surface.render_with_host(
            area,
            &mut crate::PaintCx::new(&mut crate::Frame::new(&mut buffer)),
            None,
            &first,
        );
        assert_eq!(surface.shortcut("generate"), "ctrl+x");
        surface.render_slice_with_host(
            40,
            0,
            area,
            &mut crate::PaintCx::new(&mut crate::Frame::new(&mut buffer)),
            &second,
        );
        assert_eq!(surface.shortcut("generate"), "ctrl+y");
        assert_eq!(surface.shortcut("swarm"), "unbound");
    }

    #[test]
    fn child_option_digits_are_independent_registered_actions() {
        let host = host(SUPERVISION, &[("z", "bcode.loop.child_2"), ("1", "")]);
        assert_eq!(
            host.keymap_action(SUPERVISION, KeyStroke::simple(KeyCode::Char('z')))
                .as_deref(),
            Some("bcode.loop.child_2")
        );
        assert!(
            host.keymap_action(SUPERVISION, KeyStroke::simple(KeyCode::Char('2')))
                .is_none()
        );
        assert_eq!(hint(&host, SUPERVISION, "child_2"), "z");
        assert_eq!(hint(&host, SUPERVISION, "child_1"), "unbound");
    }
}
