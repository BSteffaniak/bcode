//! Key resolution for the V2 transcript (spec section 6, FO2–FO6, FO10).
//!
//! Keys resolve through the shared host keymap: the chat scope binds transcript focus entry
//! (`tui.transcript.focusUnits`, `alt+u`) and the transcript scroll keys (Page Up/Down,
//! `ctrl+up`/`ctrl+down`, `ctrl+home`/`ctrl+end`), and the remappable `transcript_focus` scope
//! binds the focus keys. The action identities are the keymap contract shared by both engines;
//! the V2 meaning of each action is defined here and in [`super::input`].

use bmux_keyboard::KeyStroke;

use crate::keymap::{BmuxAction, BmuxKeyMap, BmuxScope};

/// Keymap scope active while the transcript has keyboard focus.
pub const FOCUS_SCOPE: &str = "transcript_focus";
/// Chat-scope action that enters (or leaves) transcript focus (FO2, FO3).
pub const FOCUS_UNITS_ACTION: &str = "tui.transcript.focusUnits";
/// Chat-scope action that cycles detail outside transcript focus.
pub const CYCLE_DETAIL_ACTION: &str = "tui.transcript.cycleDetail";

/// An action of the [`FOCUS_SCOPE`] scope (FO6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FocusKeyAction {
    /// Focus the next target (`j`, Down).
    Next,
    /// Focus the previous target (`k`, Up).
    Previous,
    /// Toggle the focused disclosure (Enter, Space).
    Toggle,
    /// Expand the focused disclosure (`l`, Right).
    Expand,
    /// Collapse the focused disclosure, or focus its parent (`h`, Left).
    Collapse,
    /// Expand everything under the focused target (`e`).
    ExpandAll,
    /// Cycle the focused disclosure collapsed, summary, expanded (`d`).
    CycleDetail,
    /// Toggle the structure inspector (`i`).
    Inspect,
    /// Scroll the inspector down (shift+Down).
    InspectorScrollDown,
    /// Scroll the inspector up (shift+Up).
    InspectorScrollUp,
    /// Make the focused tool's disclosure its default (`m`).
    MakeDefault,
    /// Return focus to the composer (Escape).
    Exit,
    /// Jump to the true session start and focus its first target (Home, FO4).
    SessionStart,
    /// Jump to the latest content and focus its last target (End, FO4).
    Latest,
}

impl FocusKeyAction {
    /// Every action.
    pub const ALL: [Self; 14] = [
        Self::Next,
        Self::Previous,
        Self::Toggle,
        Self::Expand,
        Self::Collapse,
        Self::ExpandAll,
        Self::CycleDetail,
        Self::Inspect,
        Self::InspectorScrollDown,
        Self::InspectorScrollUp,
        Self::MakeDefault,
        Self::Exit,
        Self::SessionStart,
        Self::Latest,
    ];

    /// Keymap registry identity.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Next => "tui.transcriptFocus.next",
            Self::Previous => "tui.transcriptFocus.previous",
            Self::Toggle => "tui.transcriptFocus.toggle",
            Self::Expand => "tui.transcriptFocus.expand",
            Self::Collapse => "tui.transcriptFocus.collapse",
            Self::ExpandAll => "tui.transcriptFocus.expandAll",
            Self::CycleDetail => CYCLE_DETAIL_ACTION,
            Self::Inspect => "tui.transcript.inspect",
            Self::InspectorScrollDown => "tui.transcriptFocus.inspectorScrollDown",
            Self::InspectorScrollUp => "tui.transcriptFocus.inspectorScrollUp",
            Self::MakeDefault => "tui.transcript.makeDefault",
            Self::Exit => "tui.transcriptFocus.exit",
            Self::SessionStart => SESSION_START_ACTION,
            Self::Latest => LATEST_ACTION,
        }
    }

    /// Parse a registry identity.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.id() == id)
    }
}

/// `transcript_focus` action: jump to the session start (FO4, default Home).
pub const SESSION_START_ACTION: &str = "tui.transcriptFocus.sessionStart";
/// `transcript_focus` action: jump to the latest content (FO4, default End).
pub const LATEST_ACTION: &str = "tui.transcriptFocus.latest";

/// Register the `transcript_focus` actions only the V2 engine has: Home/End jump to the
/// session start / latest content (FO4). The keymap of the V1 engine never contains them.
///
/// # Errors
/// Returns a conflicting registration (a contributor default already bound to the key).
pub fn register_actions(
    registry: &mut bcode_plugin_sdk::tui::keymap::KeymapRegistry,
) -> Result<(), bcode_plugin_sdk::tui::keymap::KeymapError> {
    for (id, description, key) in [
        (SESSION_START_ACTION, "Jump to the session start", "home"),
        (LATEST_ACTION, "Jump to the latest content", "end"),
    ] {
        registry.register(bcode_plugin_sdk::tui::keymap::KeymapAction {
            id: id.into(),
            scope: FOCUS_SCOPE.into(),
            description: description.into(),
            defaults: vec![key.into()],
        })?;
    }
    Ok(())
}

/// Transcript scroll and jump keys that work from either focus (FO5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScrollKey {
    /// Page Up / Page Down (`PageBy`).
    Page(i32),
    /// `ctrl+up` / `ctrl+down` (`ScrollBy`).
    Line(i64),
    /// `ctrl+home`: `JumpToSessionStart` with a direct `Head` load.
    SessionStart,
    /// `ctrl+end`: `JumpToLatest` with a direct `Tail` load.
    Latest,
}

/// Rows one `ctrl+up` / `ctrl+down` scrolls.
pub const LINE_SCROLL_ROWS: i64 = 3;

/// What a key means for the V2 transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TranscriptKey {
    /// `alt+u`: enter transcript focus, or leave it (FO2, FO3).
    ToggleFocus,
    /// Chat-scope detail cycling (V1 cycles every unit; V2 consumes it, see [`super::input`]).
    CycleDetailAll,
    /// A transcript scroll key (FO5).
    Scroll(ScrollKey),
    /// A `transcript_focus` key; only while the transcript has focus (FO6).
    Focus(FocusKeyAction),
}

/// Resolve `stroke` for the V2 transcript. With transcript focus the `transcript_focus` scope
/// wins over chat bindings; `None` means the key is not a transcript key (with transcript
/// focus it returns focus to the composer, FO3).
#[must_use]
pub fn resolve_key(
    keymap: &BmuxKeyMap,
    stroke: KeyStroke,
    transcript_focused: bool,
) -> Option<TranscriptKey> {
    let chat = keymap.action(BmuxScope::Chat.id(), stroke);
    if chat == Some(FOCUS_UNITS_ACTION) {
        return Some(TranscriptKey::ToggleFocus);
    }
    if transcript_focused
        && let Some(action) = keymap
            .action(FOCUS_SCOPE, stroke)
            .and_then(FocusKeyAction::from_id)
    {
        return Some(TranscriptKey::Focus(action));
    }
    if chat == Some(CYCLE_DETAIL_ACTION) {
        return Some(TranscriptKey::CycleDetailAll);
    }
    let scroll = match keymap.action_for_key(BmuxScope::Chat, stroke)? {
        BmuxAction::TranscriptPageUp => ScrollKey::Page(-1),
        BmuxAction::TranscriptPageDown => ScrollKey::Page(1),
        BmuxAction::TranscriptLineUp => ScrollKey::Line(-LINE_SCROLL_ROWS),
        BmuxAction::TranscriptLineDown => ScrollKey::Line(LINE_SCROLL_ROWS),
        BmuxAction::TranscriptTop => ScrollKey::SessionStart,
        BmuxAction::TranscriptBottom => ScrollKey::Latest,
        _ => return None,
    };
    Some(TranscriptKey::Scroll(scroll))
}

impl super::TranscriptV2Host {
    /// Run a user-bound tool-disclosure key (`tui.tools.expandAll`, `tui.tools.collapseAll`,
    /// `tui.tools.resetDisclosure`; chat scope, unbound by default) as its command-palette
    /// route, so the key does exactly what the palette command does on the V2 transcript.
    /// With transcript focus a `transcript_focus` binding of the same key wins, as for every
    /// other chat key (FO6). Returns whether the transcript changed, or `None` when `stroke`
    /// is not such a key.
    pub fn handle_tools_key(
        &mut self,
        keymap: &BmuxKeyMap,
        stroke: KeyStroke,
        at: std::time::Instant,
    ) -> Option<bool> {
        let route = tools_route(keymap, stroke)?;
        if self.transcript_focus_active()
            && keymap
                .action(FOCUS_SCOPE, stroke)
                .and_then(FocusKeyAction::from_id)
                .is_some()
        {
            return None;
        }
        match self.run_transcript_route(route, at)? {
            super::input::TranscriptRouteOutcome::Ran(changed) => Some(changed),
        }
    }
}

/// The command-palette route of the user-bound `tui.tools.*` key `stroke` (chat scope), if it
/// is one.
pub(super) fn tools_route(keymap: &BmuxKeyMap, stroke: KeyStroke) -> Option<&'static str> {
    keymap
        .action_for_key(BmuxScope::Chat, stroke)?
        .tools_route()
}

/// Label of the jump-to-latest key for the latest bar ("New messages below · KEY to jump").
#[must_use]
pub fn jump_to_latest_label(registry: &bcode_plugin_sdk::tui::keymap::KeymapRegistry) -> String {
    registry
        .keys_for_action(BmuxScope::Chat.id(), BmuxAction::TranscriptBottom.id())
        .first()
        .map_or_else(|| "ctrl+end".to_owned(), |key| (*key).to_owned())
}

#[cfg(test)]
mod tests {
    use bmux_keyboard::parse_key_stroke;

    use super::{FOCUS_SCOPE, FocusKeyAction, ScrollKey, TranscriptKey, resolve_key};
    use crate::keymap::BmuxKeyMap;

    fn keymap() -> BmuxKeyMap {
        BmuxKeyMap::from_config_for_engine(
            &bcode_config::TuiConfig::default(),
            bcode_config::TuiEngine::V2,
        )
    }

    fn key(input: &str) -> bmux_keyboard::KeyStroke {
        parse_key_stroke(input).expect("key")
    }

    #[test]
    fn every_focus_action_has_a_default_binding_in_the_focus_scope() {
        let keymap = keymap();
        for action in FocusKeyAction::ALL {
            let keys = keymap.registry().keys_for_action(FOCUS_SCOPE, action.id());
            assert!(!keys.is_empty(), "{action:?} is bound");
            for binding in keys {
                assert_eq!(
                    resolve_key(&keymap, key(binding), true),
                    Some(TranscriptKey::Focus(action)),
                    "{binding}"
                );
            }
        }
    }

    #[test]
    fn focus_keys_need_transcript_focus_and_scroll_keys_work_from_either_focus() {
        let keymap = keymap();
        assert_eq!(resolve_key(&keymap, key("j"), false), None);
        assert_eq!(resolve_key(&keymap, key("home"), false), None);
        assert_eq!(
            resolve_key(&keymap, key("home"), true),
            Some(TranscriptKey::Focus(FocusKeyAction::SessionStart))
        );
        assert_eq!(
            resolve_key(&keymap, key("end"), true),
            Some(TranscriptKey::Focus(FocusKeyAction::Latest))
        );
        for focused in [false, true] {
            assert_eq!(
                resolve_key(&keymap, key("alt+u"), focused),
                Some(TranscriptKey::ToggleFocus)
            );
            assert_eq!(
                resolve_key(&keymap, key("pageUp"), focused),
                Some(TranscriptKey::Scroll(ScrollKey::Page(-1)))
            );
            assert_eq!(
                resolve_key(&keymap, key("ctrl+end"), focused),
                Some(TranscriptKey::Scroll(ScrollKey::Latest))
            );
            assert_eq!(
                resolve_key(&keymap, key("ctrl+home"), focused),
                Some(TranscriptKey::Scroll(ScrollKey::SessionStart))
            );
            assert_eq!(
                resolve_key(&keymap, key("ctrl+down"), focused),
                Some(TranscriptKey::Scroll(ScrollKey::Line(3)))
            );
        }
        assert_eq!(resolve_key(&keymap, key("a"), true), None);
    }

    /// The `transcript_focus` scope is the keymap contract both engines share: every action
    /// registered there (including any the V1 engine adds) has a V2 meaning.
    #[test]
    fn every_registered_focus_scope_action_has_a_v2_meaning() {
        let keymap = keymap();
        let unknown: Vec<&str> = keymap
            .registry()
            .actions()
            .filter(|action| action.scope == FOCUS_SCOPE)
            .map(|action| action.id.as_str())
            .filter(|id| FocusKeyAction::from_id(id).is_none())
            .collect();
        assert!(unknown.is_empty(), "no V2 meaning: {unknown:?}");
    }

    #[test]
    fn only_the_v2_keymap_binds_the_v2_focus_actions() {
        let v1 = BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());
        for action in [FocusKeyAction::SessionStart, FocusKeyAction::Latest] {
            assert!(
                v1.registry()
                    .keys_for_action(FOCUS_SCOPE, action.id())
                    .is_empty(),
                "{action:?} is not a V1 action"
            );
        }
        assert!(!v1.help_markdown().contains("Jump to the session start"));
        assert_eq!(v1.action(FOCUS_SCOPE, key("home")), None);
        assert_eq!(v1.action(FOCUS_SCOPE, key("end")), None);
        let v2 = keymap();
        assert!(v2.help_markdown().contains("Jump to the session start"));
        assert!(v2.help_markdown().contains("Jump to the latest content"));
    }

    #[test]
    fn latest_label_follows_the_jump_binding() {
        assert_eq!(super::jump_to_latest_label(keymap().registry()), "ctrl+end");
    }
}
