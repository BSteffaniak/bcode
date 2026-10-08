//! Workflow-owned terminal actions and effective native-control adaptation.

use bcode_plugin_sdk::tui::{
    PluginTuiHost,
    keymap::{
        KeymapAction,
        controls::{ControlAction, ControlInput},
    },
};
use bmux_keyboard::{KeyStroke, parse_key_stroke};
use bmux_tui::event::Event;
use std::collections::BTreeMap;

pub const RUNS: &str = "workflow.runs";
pub const DISCOVER: &str = "workflow.discover";
pub const AUTHOR: &str = "workflow.author";
pub const SEARCH: &str = "workflow.search";
pub const EDIT: &str = "workflow.edit";
pub const FORM: &str = "workflow.form";
pub const CONFIRM: &str = "workflow.confirm";

// The final column is an internal adapter command, not a user binding fallback.
const RUN_ACTIONS: &[(&str, &str, &str)] = &[
    ("search", "Search catalog", "/"),
    ("filter", "Cycle filter", "f"),
    ("sort", "Cycle sort", "s"),
    ("group", "Cycle group", "g"),
    ("templates", "Browse templates", "shift+f"),
    ("definitions", "Browse definitions", "shift+d"),
    ("new", "Open new authoring", "shift+n"),
    ("scheduling", "Load next scheduling page", "N"),
    ("more", "Load more runs", "m"),
    ("runs", "Focus runs", "1"),
    ("graph", "Focus graph", "2"),
    ("inspector", "Focus inspector", "3"),
    ("actions", "Focus actions", "4"),
    ("descendants", "Toggle descendants", "n"),
    ("section_previous", "Previous section", "["),
    ("section_next", "Next section", "]"),
    ("pause", "Pause or resume", "p"),
    ("cancel", "Cancel run", "c"),
    ("approve", "Approve selected request", "a"),
    ("deny", "Deny selected request", "d"),
    ("input", "Provide input", "i"),
    ("retry", "Retry selected node", "r"),
    ("session", "Open session", "o"),
    ("view", "View definition", "shift+v"),
    ("edit", "Edit draft", "shift+e"),
    ("fork", "Fork definition", "shift+t"),
];
const DISCOVER_ACTIONS: &[(&str, &str, &str)] = &[
    ("search", "Search launch catalog", "/"),
    ("filter", "Cycle source filter", "f"),
    ("readiness", "Cycle readiness filter", "r"),
    ("refresh", "Refresh launch catalog", "u"),
    ("view", "View definition", "v"),
    ("preview", "Preview template", "p"),
    ("apply", "Apply template", "a"),
    ("fork", "Fork definition", "b"),
    ("edit", "Edit draft", "e"),
    ("author", "Open authoring", "o"),
    ("start", "Start workflow", "s"),
    ("more", "Load more launch items", "m"),
];
const AUTHOR_ACTIONS: &[(&str, &str, &str)] = &[
    ("fork", "Fork viewed revision or cycle field", "f"),
    ("duplicate", "Duplicate node", "d"),
    ("remove", "Remove node", "x"),
    ("connect", "Connect nodes", "c"),
    ("control", "Cycle control kind", "m"),
    ("group", "Cycle group kind", "g"),
    ("edge", "Select next edge", "e"),
    ("remove_edge", "Remove edge", "shift+e"),
    ("move_left", "Move node left", "shift+h"),
    ("move_right", "Move node right", "shift+l"),
    ("move_up", "Move node up", "shift+k"),
    ("move_down", "Move node down", "shift+j"),
    ("entry", "Set entry node", "i"),
    ("exit", "Set exit node", "o"),
    ("schema", "Edit selected schema field", "shift+f"),
    ("name", "Edit node name", "n"),
    ("references", "Edit references", "r"),
    ("conditions", "Edit conditions", "t"),
    ("model", "Edit agent model", "shift+m"),
    ("repeat", "Edit repeat bound", "shift+b"),
    ("predicate_edit", "Edit predicate", "shift+p"),
    ("profile", "Cycle agent profile", "a"),
    ("bound", "Increase repeat bound", "+"),
    ("predicate", "Cycle predicate", "v"),
    ("conflict", "Resolve conflict", "shift+r"),
    ("generate", "Generate from prompt", "shift+g"),
    ("accept", "Accept generated draft", "shift+a"),
    ("instantiate", "Instantiate template", "z"),
    ("publish", "Publish draft", "p"),
    ("start", "Start draft", "s"),
];

fn custom(scope: &str) -> &'static [(&'static str, &'static str, &'static str)] {
    match scope {
        RUNS => RUN_ACTIONS,
        DISCOVER => DISCOVER_ACTIONS,
        AUTHOR => AUTHOR_ACTIONS,
        _ => &[],
    }
}

fn controls(scope: &str) -> &'static [ControlAction] {
    match scope {
        FORM => &[
            ControlAction::Up,
            ControlAction::Down,
            ControlAction::Left,
            ControlAction::Right,
            ControlAction::Home,
            ControlAction::End,
            ControlAction::Backspace,
            ControlAction::Delete,
            ControlAction::WordLeft,
            ControlAction::WordRight,
            ControlAction::DeleteWordBackward,
            ControlAction::DeleteWordForward,
            ControlAction::LineStart,
            ControlAction::LineEnd,
            ControlAction::DeleteToStart,
            ControlAction::DeleteToEnd,
            ControlAction::SelectLeft,
            ControlAction::SelectRight,
            ControlAction::SelectUp,
            ControlAction::SelectDown,
            ControlAction::SelectWordLeft,
            ControlAction::SelectWordRight,
            ControlAction::Cancel,
            ControlAction::Submit,
            ControlAction::Newline,
            ControlAction::FocusNext,
            ControlAction::FocusPrevious,
        ],
        EDIT | SEARCH => &[
            ControlAction::Cancel,
            ControlAction::Submit,
            ControlAction::Backspace,
        ],
        CONFIRM => &[ControlAction::Cancel, ControlAction::Submit],
        _ => &[
            ControlAction::Up,
            ControlAction::Down,
            ControlAction::Left,
            ControlAction::Right,
            ControlAction::Home,
            ControlAction::End,
            ControlAction::PageUp,
            ControlAction::PageDown,
            ControlAction::Cancel,
            ControlAction::Submit,
            ControlAction::FocusNext,
            ControlAction::FocusPrevious,
            ControlAction::Toggle,
        ],
    }
}

pub fn declarations() -> Vec<KeymapAction> {
    let mut actions = Vec::new();
    for scope in [RUNS, DISCOVER, AUTHOR, SEARCH, EDIT, FORM, CONFIRM] {
        for control in controls(scope) {
            let mut definition = control.definition(scope);
            if scope == FORM && *control == ControlAction::Submit {
                definition.defaults = vec!["ctrl+enter".into()];
            }
            if scope == FORM && *control == ControlAction::Newline {
                definition.defaults = vec!["enter".into(), "shift+enter".into()];
            }
            if scope == CONFIRM && *control == ControlAction::Submit {
                definition.defaults.push("y".into());
            }
            if scope == CONFIRM && *control == ControlAction::Cancel {
                definition.defaults.push("n".into());
            }
            if matches!(scope, RUNS | DISCOVER | AUTHOR) {
                match control {
                    ControlAction::Cancel => definition.defaults.push("q".into()),
                    ControlAction::Up => definition.defaults.push("k".into()),
                    ControlAction::Down => definition.defaults.push("j".into()),
                    ControlAction::Left => definition.defaults.push("h".into()),
                    ControlAction::Right => definition.defaults.push("l".into()),
                    _ => {}
                }
            }
            actions.push(definition);
        }
        for &(name, description, stroke) in custom(scope) {
            actions.push(KeymapAction {
                id: format!("workflow.{name}"),
                scope: scope.into(),
                description: description.into(),
                defaults: if scope == RUNS && name == "scheduling" {
                    Vec::new()
                } else {
                    vec![stroke.into()]
                },
            });
        }
    }
    actions
}

/// Resolve user input once. Unrecognized keys never reach BMUX shortcuts.
pub fn routed_event(host: &dyn PluginTuiHost, scope: &str, event: &Event) -> Option<Event> {
    let Event::Key(stroke) = event else {
        return Some(event.clone());
    };
    if let Some(action) = host.keymap_action(scope, *stroke) {
        for &(name, _, native) in custom(scope) {
            if action == format!("workflow.{name}") {
                let mut stroke = parse_key_stroke(native).ok()?;
                // Existing semantic handlers distinguish shifted node operations by modifiers.
                if let bmux_keyboard::KeyCode::Char(ch) = stroke.key {
                    stroke.key = bmux_keyboard::KeyCode::Char(ch.to_ascii_lowercase());
                }
                if scope == RUNS && name == "scheduling" {
                    stroke.key = bmux_keyboard::KeyCode::Char('N');
                    stroke.modifiers.shift = false;
                }
                return Some(Event::Key(stroke));
            }
        }
    }
    let input = ControlInput::from_host(
        host,
        scope,
        *stroke,
        controls(scope),
        matches!(scope, SEARCH | EDIT | FORM),
    );
    let mut stroke = if scope == FORM && input == ControlInput::Action(ControlAction::Newline) {
        KeyStroke::simple(bmux_keyboard::KeyCode::Enter)
    } else {
        input.native_stroke()?
    };
    if scope == FORM && input == ControlInput::Action(ControlAction::Submit) {
        stroke.modifiers.ctrl = true;
    }
    Some(Event::Key(stroke))
}

/// Presentation state derived exclusively from effective host bindings.
#[derive(Debug, Default)]
pub struct Hints(BTreeMap<(String, String), String>);
impl Hints {
    pub fn refresh(&mut self, host: &dyn PluginTuiHost) {
        self.0 = declarations()
            .into_iter()
            .map(|action| {
                let keys = host.keymap_keys(&action.scope, &action.id).join("/");
                ((action.scope, action.id), keys)
            })
            .collect();
    }
    pub fn keys(&self, scope: &str, action: &str) -> String {
        self.0
            .get(&(scope.into(), action.into()))
            .cloned()
            .unwrap_or_default()
    }
    pub fn control(&self, scope: &str, action: ControlAction) -> String {
        self.keys(scope, &action.id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_plugin_sdk::tui::keymap::KeymapRegistry;
    use bcode_plugin_sdk::tui::keymap::controls::resolve_registry;

    struct Host(KeymapRegistry);
    impl PluginTuiHost for Host {
        fn spawn(&self, _task: bcode_plugin_sdk::tui::PluginTask) {}
        fn spawn_blocking(&self, _task: Box<dyn FnOnce() + Send + 'static>) {}
        fn request_redraw(&self) {}
        fn keymap_action(&self, scope: &str, stroke: KeyStroke) -> Option<String> {
            self.0.action_for_key(scope, stroke).map(str::to_string)
        }
        fn keymap_is_overridden(&self, scope: &str, stroke: KeyStroke) -> bool {
            self.0.is_overridden_key(scope, stroke)
        }
        fn keymap_keys(&self, scope: &str, action: &str) -> Vec<String> {
            self.0
                .keys_for_action(scope, action)
                .into_iter()
                .map(str::to_string)
                .collect()
        }
    }

    #[test]
    fn remapped_controls_and_hints_agree_without_native_fallback() {
        let host = Host(
            resolve_registry(
                declarations(),
                BTreeMap::from([
                    (
                        RUNS.into(),
                        BTreeMap::from([
                            ("ctrl+j".into(), ControlAction::Down.id()),
                            ("q".into(), String::new()),
                        ]),
                    ),
                    (EDIT.into(), BTreeMap::from([("x".into(), String::new())])),
                ]),
            )
            .unwrap(),
        );
        let event = |key| Event::Key(parse_key_stroke(key).unwrap());
        assert!(routed_event(&host, RUNS, &event("down")).is_none());
        assert!(routed_event(&host, RUNS, &event("j")).is_none());
        assert!(routed_event(&host, RUNS, &event("q")).is_none());
        assert!(
            matches!(routed_event(&host, RUNS, &event("ctrl+j")), Some(Event::Key(stroke)) if stroke == ControlAction::Down.native_stroke())
        );
        assert!(routed_event(&host, EDIT, &event("x")).is_none());
        assert!(
            matches!(routed_event(&host, EDIT, &event("z")), Some(Event::Key(stroke)) if stroke.key == bmux_keyboard::KeyCode::Char('z'))
        );
        let mut hints = Hints::default();
        hints.refresh(&host);
        assert_eq!(hints.control(RUNS, ControlAction::Down), "ctrl+j");
    }

    #[test]
    fn form_submit_and_newline_remain_distinct_native_operations() {
        let host = Host(resolve_registry(declarations(), BTreeMap::new()).unwrap());
        for (key, ctrl) in [("enter", false), ("ctrl+enter", true)] {
            let Some(Event::Key(stroke)) =
                routed_event(&host, FORM, &Event::Key(parse_key_stroke(key).unwrap()))
            else {
                panic!("form operation missing")
            };
            assert_eq!(stroke.key, bmux_keyboard::KeyCode::Enter);
            assert_eq!(stroke.modifiers.ctrl, ctrl);
        }
    }

    #[test]
    fn workflow_scopes_have_valid_conflict_free_defaults() {
        resolve_registry(declarations(), BTreeMap::new()).unwrap();
    }

    #[test]
    fn workflow_action_remap_and_unbind_use_effective_keys() {
        let registry = resolve_registry(
            declarations(),
            BTreeMap::from([(
                RUNS.into(),
                BTreeMap::from([
                    ("ctrl+p".into(), "workflow.pause".into()),
                    ("q".into(), String::new()),
                ]),
            )]),
        )
        .unwrap();
        assert_eq!(registry.keys_for_action(RUNS, "workflow.pause"), ["ctrl+p"]);
        assert_eq!(
            registry.action_for_key(RUNS, parse_key_stroke("p").unwrap()),
            None
        );
        assert_eq!(
            registry.action_for_key(RUNS, parse_key_stroke("q").unwrap()),
            None
        );
    }
}
