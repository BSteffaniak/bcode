//! Key resolution for the V2 transcript.
//!
//! Keys resolve through the shared host keymap: the chat scope binds transcript focus entry
//! (`tui.transcript.focusUnits`, `alt+u`) and the transcript scroll keys (Page Up/Down,
//! `ctrl+up`/`ctrl+down`, `ctrl+home`/`ctrl+end`), and the remappable `transcript_focus` scope
//! binds the focus keys. The meaning of each action is defined here and in [`super::input`].

use bmux_keyboard::KeyStroke;

use crate::keymap::{BmuxAction, BmuxKeyMap, BmuxScope};

/// Keymap scope active while the transcript has keyboard focus.
pub const FOCUS_SCOPE: &str = "transcript_focus";
/// Chat-scope action that enters (or leaves) transcript focus (FO2, FO3).
pub const FOCUS_UNITS_ACTION: &str = "tui.transcript.focusUnits";
/// Default binding of [`FOCUS_UNITS_ACTION`].
pub const FOCUS_UNITS_DEFAULT_KEY: &str = "alt+u";
/// Action that cycles disclosure collapsed → summary → expanded: the focused target in
/// transcript focus, every unit and group otherwise.
pub const CYCLE_DETAIL_ACTION: &str = "tui.transcript.cycleDetail";
/// Chat-scope default binding of [`CYCLE_DETAIL_ACTION`] (outside transcript focus).
pub const CYCLE_DETAIL_DEFAULT_KEY: &str = "alt+d";

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

    /// User-facing description, independent of the binding.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Next => "Focus next disclosure target",
            Self::Previous => "Focus previous disclosure target",
            Self::Toggle => "Toggle focused target",
            Self::Expand => "Expand focused target one level",
            Self::Collapse => "Collapse focused target one level or focus its parent",
            Self::ExpandAll => "Expand everything under the focused target",
            Self::CycleDetail => "Cycle focused target collapsed, summary, expanded",
            Self::Inspect => "Inspect how the focused target's presentation resolved",
            Self::InspectorScrollDown => "Scroll the inspector down",
            Self::InspectorScrollUp => "Scroll the inspector up",
            Self::MakeDefault => "Make the focused tool's disclosure its default",
            Self::Exit => "Close the inspector, or leave transcript focus",
            Self::SessionStart => "Jump to the session start",
            Self::Latest => "Jump to the latest content",
        }
    }

    /// Default bindings.
    #[must_use]
    pub const fn default_keys(self) -> &'static [&'static str] {
        match self {
            Self::Next => &["j", "down"],
            Self::Previous => &["k", "up"],
            Self::Toggle => &["enter", "space"],
            Self::Expand => &["l", "right"],
            Self::Collapse => &["h", "left"],
            Self::ExpandAll => &["e"],
            Self::CycleDetail => &["d"],
            Self::Inspect => &["i"],
            Self::InspectorScrollDown => &["shift+down", "ctrl+e"],
            Self::InspectorScrollUp => &["shift+up", "ctrl+y"],
            Self::MakeDefault => &["m"],
            Self::Exit => &["escape"],
            Self::SessionStart => &["home"],
            Self::Latest => &["end"],
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

/// Register the chat-scope transcript focus entry and detail cycling actions and every
/// `transcript_focus` action (FO2–FO6), including Home/End to the session start / latest
/// content (FO4).
///
/// # Errors
/// Returns a conflicting registration (a contributor default already bound to the key).
pub fn register_actions(
    registry: &mut bcode_plugin_sdk::tui::keymap::KeymapRegistry,
) -> Result<(), bcode_plugin_sdk::tui::keymap::KeymapError> {
    use bcode_plugin_sdk::tui::keymap::KeymapAction;
    registry.register(KeymapAction {
        id: FOCUS_UNITS_ACTION.into(),
        scope: BmuxScope::Chat.id().into(),
        description: "Focus transcript units".into(),
        defaults: vec![FOCUS_UNITS_DEFAULT_KEY.into()],
    })?;
    // Outside transcript focus detail cycling applies to every unit and group; in transcript
    // focus the `transcript_focus` binding cycles the focused target.
    registry.register(KeymapAction {
        id: CYCLE_DETAIL_ACTION.into(),
        scope: BmuxScope::Chat.id().into(),
        description: "Cycle every transcript unit collapsed, summary, expanded".into(),
        defaults: vec![CYCLE_DETAIL_DEFAULT_KEY.into()],
    })?;
    for action in FocusKeyAction::ALL {
        registry.register(KeymapAction {
            id: action.id().into(),
            scope: FOCUS_SCOPE.into(),
            description: action.description().into(),
            defaults: action
                .default_keys()
                .iter()
                .map(|key| (*key).to_owned())
                .collect(),
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
    /// Chat-scope detail cycling of every unit (see [`super::input`]).
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
        BmuxKeyMap::from_config(&bcode_config::TuiConfig::default())
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

    /// Every action registered in the `transcript_focus` scope has a transcript meaning.
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
    fn the_keymap_binds_the_session_start_and_latest_focus_actions() {
        let keymap = keymap();
        assert_eq!(
            keymap.action(FOCUS_SCOPE, key("home")),
            Some(FocusKeyAction::SessionStart.id())
        );
        assert_eq!(
            keymap.action(FOCUS_SCOPE, key("end")),
            Some(FocusKeyAction::Latest.id())
        );
        assert!(keymap.help_markdown().contains("Jump to the session start"));
        assert!(
            keymap
                .help_markdown()
                .contains("Jump to the latest content")
        );
    }

    #[test]
    fn latest_label_follows_the_jump_binding() {
        assert_eq!(super::jump_to_latest_label(keymap().registry()), "ctrl+end");
    }
}
