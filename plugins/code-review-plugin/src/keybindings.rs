//! Contributor-owned review action registry and native control adaptation.

use crate::code_review_tui::{ReviewApp, ReviewUxMode};
use bcode_plugin_sdk::tui::{
    PluginTuiHost,
    keymap::{
        KeymapAction, KeymapRegistry,
        controls::{ControlAction, ControlInput},
    },
};
use bmux_keyboard::{KeyStroke, parse_key_stroke};

pub const HOME: &str = "code-review-home";
pub const REVIEW: &str = "code-review";
pub const BUILD: &str = "code-review-build";
pub const PROMPT: &str = "code-review-prompt";
pub const IMPORT: &str = "code-review-import";
pub const IMPORT_OPTIONS: &str = "code-review-import-options";
pub const PUBLISH_OPTIONS: &str = "code-review-publish-options";
pub const PUBLISH: &str = "code-review-publish";
pub const COMMENT: &str = "code-review-comment";
pub const SEARCH: &str = "code-review-home-search";
pub const NEW: &str = "code-review-home-new";
pub const RENAME: &str = "code-review-home-rename";

thread_local! {
    static HINTS: std::cell::RefCell<Option<Vec<String>>> = const { std::cell::RefCell::new(None) };
}

pub fn set_hints(hints: Vec<String>) {
    HINTS.with(|current| *current.borrow_mut() = Some(hints));
}

pub fn clear_hints() {
    HINTS.with(|current| *current.borrow_mut() = None);
}

pub fn hints() -> Option<Vec<String>> {
    HINTS.with(|current| current.borrow().clone())
}

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

pub fn review_scope(app: &ReviewApp) -> &'static str {
    if app.prompt_state.is_some() {
        PROMPT
    } else if app.import_options_active() {
        IMPORT_OPTIONS
    } else if app.import_state.is_some() {
        IMPORT
    } else if app.publish_options_active() {
        PUBLISH_OPTIONS
    } else if app.publish_state.is_some() {
        PUBLISH
    } else if app.comment_editor.is_some() {
        COMMENT
    } else if app.ux_mode == ReviewUxMode::Build {
        BUILD
    } else {
        REVIEW
    }
}

pub fn native(
    scope: &str,
    _stroke: KeyStroke,
    action: Option<String>,
    literal: impl FnOnce() -> Option<KeyStroke>,
) -> Option<KeyStroke> {
    if let Some(action) = action {
        if !actions_for(scope)
            .iter()
            .any(|definition| definition.id == action)
        {
            return None;
        }
        if let Some(control) = controls()
            .into_iter()
            .find(|control| control.id() == action)
        {
            return Some(control.native_stroke());
        }
        return actions_for(scope)
            .into_iter()
            .find(|definition| definition.id == action)?
            .defaults
            .first()
            .and_then(|key| parse_key_stroke(key).ok());
    }
    literal()
}

pub fn from_host(
    host: &dyn PluginTuiHost,
    scope: &str,
    stroke: KeyStroke,
    text: bool,
) -> Option<KeyStroke> {
    native(scope, stroke, host.keymap_action(scope, stroke), || {
        ControlInput::from_host(host, scope, stroke, &[], text).native_stroke()
    })
}

pub fn from_registry(
    registry: &KeymapRegistry,
    scope: &str,
    stroke: KeyStroke,
    text: bool,
) -> Option<KeyStroke> {
    native(
        scope,
        stroke,
        registry.action_for_key(scope, stroke).map(str::to_owned),
        || ControlInput::from_registry(registry, scope, stroke, &[], text).native_stroke(),
    )
}

pub fn load_registry() -> Result<KeymapRegistry, crate::tui_host_types::TuiError> {
    let config =
        bcode_config::load_config().map_err(|error| std::io::Error::other(error.to_string()))?;
    bcode_plugin_sdk::tui::keymap::controls::resolve_registry(
        actions(),
        config.tui.keybindings.scoped_overrides(),
    )
    .map_err(|error| std::io::Error::other(error.to_string()).into())
}

pub fn effective_hints(scope: &str, keys: impl Fn(&str) -> Vec<String>) -> Vec<String> {
    actions_for(scope)
        .into_iter()
        .filter_map(|action| {
            let keys = keys(&action.id);
            (!keys.is_empty()).then(|| format!("{} {}", keys.join("/"), action.description))
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
            ControlAction::Newline,
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
        let scope = REVIEW;
        let action = "bcode.code_review.sidebar";
        let mut registry = resolve_registry(
            actions(),
            BTreeMap::from([(
                scope.into(),
                BTreeMap::from([("ctrl+b".into(), action.into())]),
            )]),
        )
        .unwrap();
        assert_eq!(
            registry.action_for_key(scope, parse_key_stroke("b").unwrap()),
            None
        );
        assert_eq!(registry.keys_for_action(scope, action), ["ctrl+b"]);
        let host = Host(registry.clone());
        assert_eq!(
            from_host(&host, scope, parse_key_stroke("b").unwrap(), false),
            None
        );
        assert_eq!(
            from_host(&host, scope, parse_key_stroke("ctrl+b").unwrap(), false),
            Some(parse_key_stroke("b").unwrap())
        );
        assert!(
            effective_hints(scope, |action| registry
                .keys_for_action(scope, action)
                .into_iter()
                .map(str::to_owned)
                .collect())
            .iter()
            .any(|hint| hint.starts_with("ctrl+b "))
        );
        registry
            .configure(BTreeMap::from([(
                scope.into(),
                BTreeMap::from([("b".into(), String::new())]),
            )]))
            .unwrap();
        assert_eq!(
            registry.action_for_key(scope, parse_key_stroke("b").unwrap()),
            None
        );
        assert!(registry.keys_for_action(scope, action).is_empty());
        assert_eq!(
            from_registry(&registry, scope, parse_key_stroke("b").unwrap(), false),
            None
        );
    }

    #[test]
    fn option_navigation_remap_leaves_old_key_as_literal_not_navigation() {
        let registry = resolve_registry(
            actions(),
            BTreeMap::from([(
                IMPORT_OPTIONS.into(),
                BTreeMap::from([
                    ("ctrl+n".into(), ControlAction::Down.id()),
                    ("x".into(), String::new()),
                ]),
            )]),
        )
        .unwrap();
        assert_eq!(
            from_registry(
                &registry,
                IMPORT_OPTIONS,
                parse_key_stroke("ctrl+n").unwrap(),
                true
            ),
            Some(ControlAction::Down.native_stroke())
        );
        assert_eq!(
            from_registry(
                &registry,
                IMPORT_OPTIONS,
                parse_key_stroke("j").unwrap(),
                true
            ),
            Some(parse_key_stroke("j").unwrap())
        );
        assert_eq!(
            from_registry(
                &registry,
                IMPORT_OPTIONS,
                parse_key_stroke("x").unwrap(),
                true
            ),
            None
        );
        assert_eq!(
            from_registry(
                &registry,
                IMPORT_OPTIONS,
                parse_key_stroke("ctrl+z").unwrap(),
                true
            ),
            None
        );
    }

    #[test]
    fn shifted_actions_remain_distinct_from_lowercase_actions() {
        let registry = resolve_registry(actions(), BTreeMap::new()).unwrap();
        assert_eq!(
            registry.action_for_key(REVIEW, parse_key_stroke("a").unwrap()),
            Some("bcode.code_review.ask")
        );
        assert_eq!(
            registry.action_for_key(REVIEW, parse_key_stroke("shift+a").unwrap()),
            Some("bcode.code_review.add_source")
        );
        assert_eq!(
            registry.action_for_key(HOME, parse_key_stroke("shift+d").unwrap()),
            Some("bcode.code_review.draft_filter")
        );
        assert_eq!(
            from_registry(&registry, HOME, parse_key_stroke("shift+d").unwrap(), false),
            Some(parse_key_stroke("shift+d").unwrap())
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
