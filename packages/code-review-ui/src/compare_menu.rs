//! Choosing what a review compares with: the "Changes since…" menu and the version picker of
//! one pull request. Both are BMUX picker frames over menus ([`Picker`]); see
//! `docs/review-stack-history.md`.

use super::history::ago;
use super::picker::{Picker, PickerItem, PickerOutcome};
use super::theme::ReviewTheme;
use bcode_code_review_models::{ChangeKind, LayerLook, LayerTimeline, LookKind};
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::Event;
use bmux_tui::geometry::Rect;
use bmux_tui::paint::PaintCx;
use bmux_tui::text::{Line, Span};

/// One hour, in milliseconds.
const HOUR_MS: u64 = 3_600_000;

/// What the menu chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuChoice {
    /// What the reviewer last looked at.
    LastLooked,
    /// Each layer's version before the latest push.
    PreviousPush,
    /// A push, picked on the timeline.
    Push,
    /// A version of the current pull request, picked in the version picker.
    Version,
    /// Each layer as it was this long ago.
    Ago(u64),
    /// No comparison.
    Off,
    /// Only pull requests with real changes.
    OnlyChanged,
    /// Only what changed / the full change.
    View,
}

impl MenuChoice {
    fn id(self) -> String {
        match self {
            Self::LastLooked => "last".to_owned(),
            Self::PreviousPush => "previous".to_owned(),
            Self::Push => "push".to_owned(),
            Self::Version => "version".to_owned(),
            Self::Ago(ms) => format!("ago:{ms}"),
            Self::Off => "off".to_owned(),
            Self::OnlyChanged => "only_changed".to_owned(),
            Self::View => "view".to_owned(),
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            "last" => Self::LastLooked,
            "previous" => Self::PreviousPush,
            "push" => Self::Push,
            "version" => Self::Version,
            "off" => Self::Off,
            "only_changed" => Self::OnlyChanged,
            "view" => Self::View,
            other => Self::Ago(other.strip_prefix("ago:")?.parse().ok()?),
        })
    }
}

/// What a key or click in a menu led to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuOutcome<T> {
    /// Close it.
    Close,
    /// Draw again.
    Redraw,
    /// Not its input.
    Ignored,
    /// A choice.
    Chose(T),
}

/// What the "Changes since…" menu offers and how things stand.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MenuState {
    /// The choice in effect, to focus.
    pub current: Option<MenuChoice>,
    /// Only pull requests with real changes are shown.
    pub only_changed: bool,
    /// Only what changed is shown.
    pub changes_view: bool,
    /// What there is to pick from.
    pub offers: MenuOffers,
    /// The keys of the view switch and of this menu, for the labels.
    pub view_key: Option<String>,
}

/// What the menu can offer to pick from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MenuOffers {
    /// The stack has pushes.
    pub pushes: bool,
    /// The current pull request has several versions.
    pub versions: bool,
}

/// The "Changes since…" menu.
#[derive(Debug)]
pub struct CompareMenu {
    picker: Picker,
}

const fn check(on: bool) -> &'static str {
    if on { "[x] " } else { "[ ] " }
}

impl CompareMenu {
    /// The menu for `state`.
    #[must_use]
    pub fn new(state: &MenuState, theme: &ReviewTheme) -> Self {
        let mut picker = Picker::new("review.compare_menu", "Changes since…", None);
        picker.set_footer("enter choose · esc close");
        let row = |choice: MenuChoice, label: &str, hint: &str| {
            PickerItem::new(
                choice.id(),
                Line::from_spans(vec![
                    Span::styled(label.to_owned(), theme.text),
                    Span::styled(format!("  {hint}"), theme.muted),
                ]),
            )
        };
        let section = |id: &str, label: &str| {
            PickerItem::section(
                id,
                Line::from_spans(vec![Span::styled(label.to_owned(), theme.title)]),
            )
        };
        let mut items = vec![
            section("s.since", "Compare with"),
            row(
                MenuChoice::LastLooked,
                "You last looked",
                "each pull request as you last saw it",
            ),
        ];
        if state.offers.pushes {
            items.push(row(
                MenuChoice::PreviousPush,
                "Previous push",
                "each pull request before the stack's latest push",
            ));
            items.push(row(
                MenuChoice::Push,
                "A push…",
                "pick pushes on the timeline",
            ));
        }
        if state.offers.versions {
            items.push(row(
                MenuChoice::Version,
                "A version of this pull request…",
                "pick two versions",
            ));
        }
        items.push(section("s.time", "As it was"));
        for (ms, label) in [
            (HOUR_MS, "1 hour ago"),
            (24 * HOUR_MS, "Yesterday"),
            (7 * 24 * HOUR_MS, "A week ago"),
        ] {
            items.push(row(
                MenuChoice::Ago(ms),
                label,
                "every pull request since then",
            ));
        }
        items.push(row(
            MenuChoice::Off,
            "Full change",
            "no comparison: the review as without history",
        ));
        items.push(section("s.show", "Show"));
        items.push(row(
            MenuChoice::OnlyChanged,
            &format!(
                "{}Only pull requests with real changes",
                check(state.only_changed)
            ),
            "hide unchanged and rebased-only ones",
        ));
        let key = state
            .view_key
            .as_deref()
            .map_or_else(String::new, |key| format!("({key})"));
        items.push(row(
            MenuChoice::View,
            &format!("{}Show only what changed", check(state.changes_view)),
            &key,
        ));
        picker.set_items(items);
        if let Some(current) = state.current {
            picker.focus_id(&current.id());
        }
        Self { picker }
    }

    /// Answer a key.
    pub fn key(&mut self, stroke: KeyStroke) -> MenuOutcome<MenuChoice> {
        Self::outcome(self.picker.key(stroke))
    }

    /// Answer a pointer event.
    pub fn mouse(&mut self, event: &Event, theme: &ReviewTheme) -> MenuOutcome<MenuChoice> {
        Self::outcome(self.picker.mouse(event, theme))
    }

    fn outcome(outcome: PickerOutcome) -> MenuOutcome<MenuChoice> {
        match outcome {
            PickerOutcome::Cancel => MenuOutcome::Close,
            PickerOutcome::Picked(id) => {
                MenuChoice::from_id(&id).map_or(MenuOutcome::Ignored, MenuOutcome::Chose)
            }
            PickerOutcome::Ignored => MenuOutcome::Ignored,
            _ => MenuOutcome::Redraw,
        }
    }

    /// The focused choice.
    #[must_use]
    pub fn focused(&self) -> Option<MenuChoice> {
        self.picker.focused().and_then(MenuChoice::from_id)
    }

    /// Paint over `area`.
    pub fn paint(&mut self, theme: &ReviewTheme, area: Rect, cx: &mut PaintCx<'_, '_>) {
        self.picker.paint(theme, area, cx);
    }
}

/// Two versions picked in the version picker (indices into the layer's versions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionPick {
    /// The older version.
    pub from: usize,
    /// The newer version.
    pub to: usize,
}

/// The version picker of one pull request: one row per version, newest first.
#[derive(Debug)]
pub struct VersionPicker {
    layer: LayerTimeline,
    looks: Vec<LayerLook>,
    from: Option<usize>,
    to: usize,
    now_ms: u64,
    picker: Picker,
}

/// A version's kind of change, said briefly.
const fn kind_text(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::First => "first",
        ChangeKind::Content => "changed",
        ChangeKind::CommitsAdded => "commits added",
        ChangeKind::RebaseOnly => "rebased",
        ChangeKind::Retargeted => "retargeted",
        ChangeKind::Unavailable => "unavailable",
    }
}

impl VersionPicker {
    /// The picker for `layer`, `from` and `to` preselected (indices into its versions; `to`
    /// defaults to the newest), with the reviewer's `looks` marked.
    #[must_use]
    pub fn new(
        layer: LayerTimeline,
        looks: Vec<LayerLook>,
        from: Option<usize>,
        to: Option<usize>,
        now_ms: u64,
        theme: &ReviewTheme,
    ) -> Self {
        let newest = layer.versions.len().saturating_sub(1);
        let mut picker = Picker::new(
            "review.versions",
            format!("Versions of #{} {}", layer.number, layer.title),
            None,
        );
        picker.set_footer("f from · t to · enter compare · esc close");
        let mut this = Self {
            from: from.filter(|from| *from < layer.versions.len()),
            to: to.unwrap_or(newest).min(newest),
            layer,
            looks,
            now_ms,
            picker,
        };
        this.rebuild(theme);
        let focus = this.from.unwrap_or(this.to);
        this.picker.focus_id(&focus.to_string());
        this
    }

    /// The picked pair so far: `from` (when set) and `to`.
    #[must_use]
    pub const fn picked(&self) -> (Option<usize>, usize) {
        (self.from, self.to)
    }

    /// The pull request.
    #[must_use]
    pub const fn number(&self) -> u64 {
        self.layer.number
    }

    /// The pull request's versions.
    #[must_use]
    pub const fn layer(&self) -> &LayerTimeline {
        &self.layer
    }

    fn marks(&self, head: &str) -> Option<(&'static str, &'static str)> {
        let looks = self.looks.iter().filter(|look| look.head_oid == head);
        let kinds: Vec<LookKind> = looks.map(|look| look.kind).collect();
        if kinds.contains(&LookKind::Reviewed) {
            Some(("✓", " you reviewed"))
        } else if kinds.iter().any(|kind| kind.engaged()) {
            Some(("◆", " you looked"))
        } else if kinds.contains(&LookKind::Opened) {
            Some(("○", " you opened"))
        } else {
            None
        }
    }

    fn rebuild(&mut self, theme: &ReviewTheme) {
        let newest = self.layer.versions.len().saturating_sub(1);
        let mut items = Vec::new();
        for (index, version) in self.layer.versions.iter().enumerate().rev() {
            let available = version.kind != ChangeKind::Unavailable;
            let mark = if index == self.to {
                "to ▸   "
            } else if Some(index) == self.from {
                "from ▸ "
            } else {
                "       "
            };
            let when = if index == newest {
                "now".to_owned()
            } else {
                ago(version.version.seen_at_ms, self.now_ms)
            };
            let number = if available {
                format!("v{}", index + 1)
            } else {
                "·".to_owned()
            };
            let style = if available { theme.text } else { theme.muted };
            let mut spans = vec![
                Span::styled(mark, theme.accent),
                Span::styled(number, theme.title),
                Span::styled("  ", theme.text),
                Span::styled(when, theme.muted),
                Span::styled("  ", theme.text),
                Span::styled(kind_text(version.kind), style),
            ];
            if let Some((glyph, label)) = self.marks(&version.version.head_oid) {
                spans.push(Span::styled(format!("  {glyph}{label}"), theme.viewed));
            }
            if let Some(actor) = &version.version.actor {
                spans.push(Span::styled(format!("  {actor}"), theme.muted));
            }
            let line = Line::from_spans(spans);
            items.push(if available {
                PickerItem::new(index.to_string(), line)
            } else {
                // Its commits are gone: it cannot be compared.
                PickerItem::section(format!("gone.{index}"), line)
            });
        }
        self.picker.set_items(items);
    }

    /// Answer a key.
    pub fn key(&mut self, stroke: KeyStroke, theme: &ReviewTheme) -> MenuOutcome<VersionPick> {
        let plain = stroke.modifiers.is_empty();
        let focused = self
            .picker
            .focused()
            .and_then(|id| id.parse::<usize>().ok());
        match stroke.key {
            KeyCode::Char('f') if plain => {
                if let Some(focused) = focused {
                    self.from = (self.from != Some(focused)).then_some(focused);
                    self.rebuild(theme);
                }
                return MenuOutcome::Redraw;
            }
            KeyCode::Char('t') if plain => {
                if let Some(focused) = focused {
                    self.to = focused;
                    self.rebuild(theme);
                }
                return MenuOutcome::Redraw;
            }
            KeyCode::Enter if plain => return self.apply(focused),
            _ => {}
        }
        match self.picker.key(stroke) {
            PickerOutcome::Cancel => MenuOutcome::Close,
            PickerOutcome::Ignored => MenuOutcome::Ignored,
            _ => MenuOutcome::Redraw,
        }
    }

    /// Compare: from the `from` picked (else the focused version) to `to`.
    fn apply(&self, focused: Option<usize>) -> MenuOutcome<VersionPick> {
        let Some(from) = self.from.or(focused) else {
            return MenuOutcome::Ignored;
        };
        if from == self.to {
            return MenuOutcome::Ignored;
        }
        MenuOutcome::Chose(VersionPick {
            from: from.min(self.to),
            to: from.max(self.to),
        })
    }

    /// Answer a pointer event: a click on a row sets `from`; with a modifier, `to`.
    pub fn mouse(&mut self, event: &Event, theme: &ReviewTheme) -> MenuOutcome<VersionPick> {
        let modified = matches!(event, Event::Mouse(mouse)
            if mouse.modifiers.shift || mouse.modifiers.ctrl || mouse.modifiers.alt);
        match self.picker.mouse(event, theme) {
            PickerOutcome::Picked(id) => {
                if let Ok(index) = id.parse::<usize>() {
                    if modified {
                        self.to = index;
                    } else {
                        self.from = Some(index);
                    }
                    self.rebuild(theme);
                }
                MenuOutcome::Redraw
            }
            PickerOutcome::Cancel => MenuOutcome::Close,
            PickerOutcome::Ignored => MenuOutcome::Ignored,
            _ => MenuOutcome::Redraw,
        }
    }

    /// Paint over `area`.
    pub fn paint(&mut self, theme: &ReviewTheme, area: Rect, cx: &mut PaintCx<'_, '_>) {
        self.picker.paint(theme, area, cx);
    }

    /// The rows' text, newest first.
    #[must_use]
    pub fn rows(&self) -> Vec<String> {
        self.picker
            .items()
            .iter()
            .map(|item| item.line.plain_text())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::IconStyle;
    use bcode_code_review_models::{LayerVersion, TimelineVersion, VersionSource};
    use std::collections::BTreeSet;

    fn layer() -> LayerTimeline {
        let version = |head: &str, at: u64, kind: ChangeKind| TimelineVersion {
            version: LayerVersion {
                provider: "p".into(),
                repository: "o/r".into(),
                number: 7,
                head_oid: head.into(),
                base_oid: None,
                base_branch: None,
                seen_at_ms: at,
                actor: Some("ana".into()),
                sources: BTreeSet::from([VersionSource::Observed]),
            },
            kind,
            fork_oid: Some("fork".into()),
            reviewed: false,
        };
        LayerTimeline {
            number: 7,
            title: "Layer".into(),
            head_branch: "l".into(),
            versions: vec![
                version("a", 1_000, ChangeKind::First),
                version("b", 2_000, ChangeKind::Content),
                version("gone", 2_500, ChangeKind::Unavailable),
                version("c", 3_000, ChangeKind::RebaseOnly),
            ],
        }
    }

    fn key(code: KeyCode) -> KeyStroke {
        KeyStroke {
            key: code,
            modifiers: bmux_keyboard::Modifiers::default(),
        }
    }

    #[test]
    fn versions_are_picked_from_and_to_and_marked_with_looks() {
        let theme = ReviewTheme::derive(None, IconStyle::Unicode);
        let look = LayerLook {
            provider: "p".into(),
            repository: "o/r".into(),
            number: 7,
            head_oid: "a".into(),
            fork_oid: None,
            kind: LookKind::Viewed,
            looked_at_ms: 1_500,
            review_id: None,
        };
        let mut picker = VersionPicker::new(layer(), vec![look], None, None, 4_000, &theme);
        let rows = picker.rows();
        // Newest first; the version whose commits are gone has no number.
        assert!(rows[0].starts_with("to ▸") && rows[0].contains("v4") && rows[0].contains("now"));
        assert!(rows[1].contains('·') && rows[1].contains("unavailable"));
        assert!(
            rows[3].contains("v1") && rows[3].contains("◆ you looked"),
            "{rows:?}"
        );
        // Down past the unavailable row to v2, pin it as from, apply.
        assert_eq!(picker.key(key(KeyCode::Down), &theme), MenuOutcome::Redraw);
        assert_eq!(
            picker.key(key(KeyCode::Char('f')), &theme),
            MenuOutcome::Redraw
        );
        assert_eq!(picker.picked(), (Some(1), 3));
        assert!(picker.rows()[2].starts_with("from ▸"));
        assert_eq!(
            picker.key(key(KeyCode::Enter), &theme),
            MenuOutcome::Chose(VersionPick { from: 1, to: 3 })
        );
        // `to` can be an older version too.
        assert_eq!(picker.key(key(KeyCode::Down), &theme), MenuOutcome::Redraw);
        picker.key(key(KeyCode::Char('t')), &theme);
        assert_eq!(
            picker.key(key(KeyCode::Enter), &theme),
            MenuOutcome::Chose(VersionPick { from: 0, to: 1 })
        );
        assert_eq!(picker.key(key(KeyCode::Escape), &theme), MenuOutcome::Close);
    }

    #[test]
    fn the_menu_offers_what_the_review_has_and_says_what_is_on() {
        let theme = ReviewTheme::derive(None, IconStyle::Unicode);
        let mut menu = CompareMenu::new(
            &MenuState {
                current: Some(MenuChoice::Off),
                only_changed: true,
                changes_view: false,
                offers: MenuOffers {
                    pushes: false,
                    versions: true,
                },
                view_key: Some("shift+l".into()),
            },
            &theme,
        );
        assert_eq!(menu.focused(), Some(MenuChoice::Off));
        let ids: Vec<String> = menu
            .picker
            .items()
            .iter()
            .map(|item| item.line.plain_text())
            .collect();
        assert!(!ids.iter().any(|text| text.contains("Previous push")));
        assert!(
            ids.iter()
                .any(|text| text.contains("[x] Only pull requests"))
        );
        assert!(ids.iter().any(|text| text.contains("(shift+l)")));
        assert_eq!(
            menu.key(key(KeyCode::Enter)),
            MenuOutcome::Chose(MenuChoice::Off)
        );
        assert_eq!(
            MenuChoice::from_id("ago:3600000"),
            Some(MenuChoice::Ago(3_600_000))
        );
    }
}
