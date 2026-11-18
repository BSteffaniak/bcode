//! Choosing what a review compares with: the comparison menu and the version picker of one pull
//! request. Both are BMUX picker frames over menus ([`Picker`]); see
//! `docs/review-stack-history.md`.
//!
//! The comparison menu is a live control: `space` (or a click) picks a baseline or flips a
//! view option in place and the review behind the menu follows at once; `enter` keeps the
//! state and closes; `esc` puts back the state the menu opened on. The baseline in effect is
//! marked like a radio button (with what it resolves to, such as `4m ago`), view options like
//! check boxes, and the cursor opens on the baseline in effect. How what is new since the
//! baseline is marked (the mark style, changed words, hunks seen before, code removed since)
//! is chosen the same way, and a key to the tab badges closes the menu.

use super::config::compare::{MarkOptions, MarkStyle, SeenHunks};
use super::history::ago;
use super::picker::{Picker, PickerItem, PickerOutcome};
use super::theme::ReviewTheme;
use bcode_code_review_models::{ChangeKind, LayerLook, LayerTimeline, LookKind};
use bmux_keyboard::{KeyCode, KeyStroke};
use bmux_tui::event::{Event, MouseButton, MouseEventKind};
use bmux_tui::geometry::Rect;
use bmux_tui::paint::PaintCx;
use bmux_tui::text::{Line, Span};

/// One hour, in milliseconds.
const HOUR_MS: u64 = 3_600_000;

/// The points in time the menu offers, with their labels.
pub const AGO_PRESETS: [(u64, &str); 3] = [
    (HOUR_MS, "1 hour ago"),
    (24 * HOUR_MS, "yesterday"),
    (7 * 24 * HOUR_MS, "a week ago"),
];

/// A row of the comparison menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuChoice {
    /// Compare with what the reviewer last looked at.
    LastLooked,
    /// Compare with each layer's version before the latest push.
    PreviousPush,
    /// Compare between pushes picked on the timeline.
    Push,
    /// Compare two versions of the current pull request, picked in the version picker.
    Version,
    /// Compare with each layer as it was this long ago.
    Ago(u64),
    /// The whole stack as it was at a time (chosen in the stack history); listed while in
    /// effect, or while the menu opened on it.
    StackAt,
    /// No comparison.
    Off,
    /// View option: only pull requests with real changes.
    OnlyChanged,
    /// View option: only what changed.
    View,
    /// How lines new since the baseline are marked.
    MarkStyle(MarkStyle),
    /// Mark option: the changed words of lines modified since the baseline.
    Words,
    /// How hunks with nothing new since the baseline show.
    Seen(SeenHunks),
    /// Mark option: code removed since the baseline shows inline.
    RemovedInline,
    /// Ask the review assistant for a guide of the changes since the baseline in effect.
    Guide,
}

const fn mark_style_id(style: MarkStyle) -> &'static str {
    match style {
        MarkStyle::Both => "mark:both",
        MarkStyle::Bar => "mark:bar",
        MarkStyle::Tint => "mark:tint",
        MarkStyle::Strong => "mark:strong",
    }
}

const fn seen_id(seen: SeenHunks) -> &'static str {
    match seen {
        SeenHunks::Show => "seen:show",
        SeenHunks::Dim => "seen:dim",
        SeenHunks::Fold => "seen:fold",
    }
}

impl MenuChoice {
    fn id(self) -> String {
        match self {
            Self::LastLooked => "last".to_owned(),
            Self::PreviousPush => "previous".to_owned(),
            Self::Push => "push".to_owned(),
            Self::Version => "version".to_owned(),
            Self::Ago(ms) => format!("ago:{ms}"),
            Self::StackAt => "stack_at".to_owned(),
            Self::Off => "off".to_owned(),
            Self::OnlyChanged => "only_changed".to_owned(),
            Self::View => "view".to_owned(),
            Self::MarkStyle(style) => mark_style_id(style).to_owned(),
            Self::Words => "words".to_owned(),
            Self::Seen(seen) => seen_id(seen).to_owned(),
            Self::RemovedInline => "removed_inline".to_owned(),
            Self::Guide => "guide".to_owned(),
        }
    }

    fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            "last" => Self::LastLooked,
            "previous" => Self::PreviousPush,
            "push" => Self::Push,
            "version" => Self::Version,
            "stack_at" => Self::StackAt,
            "off" => Self::Off,
            "only_changed" => Self::OnlyChanged,
            "view" => Self::View,
            "words" => Self::Words,
            "removed_inline" => Self::RemovedInline,
            "guide" => Self::Guide,
            other => {
                if let Some(style) = MarkStyle::ALL
                    .into_iter()
                    .find(|style| mark_style_id(*style) == other)
                {
                    Self::MarkStyle(style)
                } else if let Some(seen) = SeenHunks::ALL
                    .into_iter()
                    .find(|seen| seen_id(*seen) == other)
                {
                    Self::Seen(seen)
                } else {
                    Self::Ago(other.strip_prefix("ago:")?.parse().ok()?)
                }
            }
        })
    }

    /// Whether the row is a view or mark option rather than a baseline.
    #[must_use]
    pub const fn is_option(self) -> bool {
        matches!(
            self,
            Self::OnlyChanged
                | Self::View
                | Self::MarkStyle(_)
                | Self::Words
                | Self::Seen(_)
                | Self::RemovedInline
        )
    }

    /// Whether the row is an action that closes the menu rather than a choice.
    #[must_use]
    pub const fn is_action(self) -> bool {
        matches!(self, Self::Guide)
    }

    /// Whether picking the row opens another picker (the push timeline or the version picker)
    /// instead of comparing at once.
    #[must_use]
    pub const fn opens_picker(self) -> bool {
        matches!(self, Self::Push | Self::Version)
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

/// What a key or click in the comparison menu led to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareMenuOutcome {
    /// Put back the state the menu opened on, and close it.
    Cancel,
    /// Keep the state, and close it.
    Done,
    /// Apply the choice and keep the menu open (a preview).
    Apply(MenuChoice),
    /// Apply the choice, keep the state, and close the menu.
    ApplyAndClose(MenuChoice),
    /// Draw again.
    Redraw,
    /// Not its input.
    Ignored,
}

/// What the comparison menu offers and how things stand.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MenuState {
    /// The baseline in effect: marked, and the row the cursor opens on.
    pub current: Option<MenuChoice>,
    /// What the baseline in effect resolves to (`4m ago`, `push 3 → now`, `v2 → now`).
    pub detail: Option<String>,
    /// When the reviewer last looked, when known (`4m ago`).
    pub last_looked: Option<String>,
    /// When the stack was last pushed, when known (`2h ago`).
    pub last_push: Option<String>,
    /// The pull request the version picker would open on.
    pub version_of: Option<u64>,
    /// Only pull requests with real changes are shown.
    pub only_changed: bool,
    /// Only what changed is shown.
    pub changes_view: bool,
    /// What there is to pick from.
    pub offers: MenuOffers,
    /// The key of the view switch, for its row.
    pub view_key: Option<String>,
    /// How far the comparison is (`comparing 2 of 3…`), while some source is still computing.
    pub progress: Option<String>,
    /// When the whole stack is shown as it was (`3h ago`), while that is in effect or the
    /// menu opened on it: its row is offered.
    pub stack_at: Option<String>,
    /// How what is new since the baseline is marked.
    pub marks: MarkOptions,
    /// What each tab badge glyph means.
    pub legend: String,
    /// Some pull request is compared: the menu offers a guide of the changes.
    pub guide: bool,
}

/// What the menu can offer to pick from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MenuOffers {
    /// The stack has pushes.
    pub pushes: bool,
    /// The current pull request has several versions.
    pub versions: bool,
}

/// The comparison menu.
#[derive(Debug)]
pub struct CompareMenu {
    picker: Picker,
    state: MenuState,
}

/// The radio mark of a baseline row.
const fn radio(on: bool, ascii: bool) -> &'static str {
    match (on, ascii) {
        (true, false) => "◉ ",
        (false, false) => "○ ",
        (true, true) => "(*) ",
        (false, true) => "( ) ",
    }
}

/// The check box of a view option row.
const fn check(on: bool) -> &'static str {
    if on { "[x] " } else { "[ ] " }
}

const FOOTER: &str = "space pick · enter done · esc cancel";

impl CompareMenu {
    /// The menu for `state`, the cursor on the baseline in effect.
    #[must_use]
    pub fn new(state: &MenuState, theme: &ReviewTheme) -> Self {
        let picker = Picker::new("review.compare_menu", "Compare", None);
        let mut menu = Self {
            picker,
            state: MenuState::default(),
        };
        menu.update(state, theme);
        if let Some(current) = state.current {
            menu.picker.focus_id(&current.id());
        }
        menu
    }

    /// Show `state` (after a pick took effect), keeping the cursor where it is.
    pub fn update(&mut self, state: &MenuState, theme: &ReviewTheme) {
        if self.state == *state && !self.picker.items().is_empty() {
            return;
        }
        self.state = state.clone();
        let footer = state.progress.as_ref().map_or_else(
            || FOOTER.to_owned(),
            |progress| format!("{progress} · {FOOTER}"),
        );
        self.picker.set_footer(footer);
        self.picker.set_items(Self::items(state, theme));
    }

    /// The state shown.
    #[must_use]
    pub const fn state(&self) -> &MenuState {
        &self.state
    }

    fn items(state: &MenuState, theme: &ReviewTheme) -> Vec<PickerItem> {
        let ascii = theme.icons.menu.is_ascii();
        let separator = theme.icons.separator;
        let ellipsis = theme.icons.ellipsis;
        let target = |choice: MenuChoice, label: String, known: Option<&str>, hint: &str| {
            let on = state.current == Some(choice);
            let mut spans = vec![
                Span::styled(
                    radio(on, ascii),
                    if on { theme.accent } else { theme.muted },
                ),
                Span::styled(label, if on { theme.title } else { theme.text }),
            ];
            // The baseline in effect says what it resolves to; the others say when they are,
            // when known, else what they mean.
            let detail = if on {
                state.detail.as_deref().or(known)
            } else {
                known
            };
            match detail {
                Some(detail) if !detail.is_empty() => spans.push(Span::styled(
                    format!("{separator}{detail}"),
                    if on { theme.accent } else { theme.muted },
                )),
                _ => spans.push(Span::styled(format!("  {hint}"), theme.muted)),
            }
            PickerItem::new(choice.id(), Line::from_spans(spans))
        };
        let section = |id: &str, label: &str| {
            PickerItem::section(
                id,
                Line::from_spans(vec![Span::styled(label.to_owned(), theme.title)]),
            )
        };
        let mut items: Vec<PickerItem> = Self::guide_item(state, theme).into_iter().collect();
        items.extend([
            section("s.since", "Show changes since"),
            target(
                MenuChoice::LastLooked,
                "you last looked".to_owned(),
                state.last_looked.as_deref(),
                "each pull request as you last saw it",
            ),
        ]);
        if state.offers.pushes {
            items.push(target(
                MenuChoice::PreviousPush,
                "the previous push".to_owned(),
                state.last_push.as_deref(),
                "each pull request before the latest push",
            ));
        }
        for (ms, label) in AGO_PRESETS {
            items.push(target(
                MenuChoice::Ago(ms),
                label.to_owned(),
                None,
                "each pull request as it was then",
            ));
        }
        if state.offers.pushes {
            items.push(target(
                MenuChoice::Push,
                format!("pushes picked on the timeline{ellipsis}"),
                None,
                "from one push to another",
            ));
        }
        if state.offers.versions || state.current == Some(MenuChoice::Version) {
            let of = state.version_of.map_or_else(
                || "this pull request".to_owned(),
                |number| format!("#{number}"),
            );
            items.push(target(
                MenuChoice::Version,
                format!("two versions of {of}{ellipsis}"),
                None,
                "picked in its version list",
            ));
        }
        if let Some(when) = state.stack_at.as_deref() {
            items.push(target(
                MenuChoice::StackAt,
                "the whole stack as it was".to_owned(),
                Some(when),
                "picked in the stack history",
            ));
        }
        items.push(target(
            MenuChoice::Off,
            "nothing: show the full change".to_owned(),
            None,
            "no comparison",
        ));
        items.push(section("s.view", "View"));
        items.extend(Self::option_items(state, theme));
        items.extend(Self::mark_items(state, theme));
        items.push(PickerItem::section(
            "s.legend",
            Line::from_spans(vec![Span::styled(state.legend.clone(), theme.muted)]),
        ));
        items
    }

    /// The row asking for a guide of the changes, while some pull request is compared.
    fn guide_item(state: &MenuState, theme: &ReviewTheme) -> Option<PickerItem> {
        state.guide.then(|| {
            PickerItem::new(
                MenuChoice::Guide.id(),
                Line::from_spans(vec![
                    Span::styled(theme.icons.guide_marker, theme.accent),
                    Span::raw(" "),
                    Span::styled("Guide these changes", theme.text),
                    Span::styled(
                        "  the review assistant walks you through what changed",
                        theme.muted,
                    ),
                ]),
            )
        })
    }

    /// The rows choosing how what is new since the baseline is marked: radio buttons for the
    /// mark style and for hunks seen before, check boxes for changed words and removed code.
    fn mark_items(state: &MenuState, theme: &ReviewTheme) -> Vec<PickerItem> {
        let ascii = theme.icons.menu.is_ascii();
        let marks = state.marks;
        let row = |choice: MenuChoice, mark: &'static str, on: bool, label: &str, hint: &str| {
            PickerItem::new(
                choice.id(),
                Line::from_spans(vec![
                    Span::styled(mark, if on { theme.accent } else { theme.muted }),
                    Span::styled(label.to_owned(), if on { theme.title } else { theme.text }),
                    Span::styled(format!("  {hint}"), theme.muted),
                ]),
            )
        };
        let radio_row = |choice: MenuChoice, on: bool, label: &str, hint: &str| {
            row(choice, radio(on, ascii), on, label, hint)
        };
        let check_row = |choice: MenuChoice, on: bool, label: &str, hint: &str| {
            row(choice, check(on), on, label, hint)
        };
        let section = |id: &str, label: &str| {
            PickerItem::section(
                id,
                Line::from_spans(vec![Span::styled(label.to_owned(), theme.title)]),
            )
        };
        let mut items = vec![section("s.marks", "Mark new lines")];
        for style in MarkStyle::ALL {
            let (label, hint) = match style {
                MarkStyle::Both => ("bar and tint", "a gutter bar and a tinted row"),
                MarkStyle::Bar => ("gutter bar only", "rows keep their colors"),
                MarkStyle::Tint => ("tint only", "no bar in the gutter"),
                MarkStyle::Strong => ("strong", "accent bar, deeper tint, and a new label"),
            };
            items.push(radio_row(
                MenuChoice::MarkStyle(style),
                marks.style == style,
                label,
                hint,
            ));
        }
        items.push(check_row(
            MenuChoice::Words,
            marks.words,
            "Mark changed words",
            "in a line changed since, what differs from then",
        ));
        items.push(check_row(
            MenuChoice::RemovedInline,
            marks.removed_inline,
            "Show removed code inline",
            "struck through, instead of behind a marker",
        ));
        items.push(section("s.seen", "Hunks with nothing new"));
        for seen in SeenHunks::ALL {
            let (label, hint) = match seen {
                SeenHunks::Show => ("show", "like the others"),
                SeenHunks::Dim => ("dim", "faint, to read past"),
                SeenHunks::Fold => ("fold", "to their header; a click shows one"),
            };
            items.push(radio_row(
                MenuChoice::Seen(seen),
                marks.seen == seen,
                label,
                hint,
            ));
        }
        items
    }

    /// The view option rows: check boxes.
    fn option_items(state: &MenuState, theme: &ReviewTheme) -> [PickerItem; 2] {
        let option = |choice: MenuChoice, on: bool, label: &str, hint: String| {
            PickerItem::new(
                choice.id(),
                Line::from_spans(vec![
                    Span::styled(check(on), if on { theme.accent } else { theme.muted }),
                    Span::styled(label.to_owned(), if on { theme.title } else { theme.text }),
                    Span::styled(format!("  {hint}"), theme.muted),
                ]),
            )
        };
        let key = state
            .view_key
            .as_deref()
            .map_or_else(String::new, |key| format!(" ({key})"));
        [
            option(
                MenuChoice::View,
                state.changes_view,
                "Only what changed",
                format!("hide what is the same as then{key}"),
            ),
            option(
                MenuChoice::OnlyChanged,
                state.only_changed,
                "Only pull requests with real changes",
                "hide unchanged and rebased-only ones".to_owned(),
            ),
        ]
    }

    /// Answer a key.
    pub fn key(&mut self, stroke: KeyStroke) -> CompareMenuOutcome {
        let plain = stroke.modifiers.is_empty();
        match stroke.key {
            KeyCode::Space | KeyCode::Char(' ') if plain => {
                return self
                    .focused()
                    .map_or(CompareMenuOutcome::Ignored, |choice| {
                        if choice.opens_picker() || choice.is_action() {
                            CompareMenuOutcome::ApplyAndClose(choice)
                        } else {
                            CompareMenuOutcome::Apply(choice)
                        }
                    });
            }
            KeyCode::Enter if plain => {
                return match self.focused() {
                    // A view option is flipped with space or a click; enter keeps what is set.
                    Some(choice) if choice.is_option() => CompareMenuOutcome::Done,
                    Some(choice)
                        if self.state.current == Some(choice) && !choice.opens_picker() =>
                    {
                        CompareMenuOutcome::Done
                    }
                    Some(choice) => CompareMenuOutcome::ApplyAndClose(choice),
                    None => CompareMenuOutcome::Done,
                };
            }
            _ => {}
        }
        match self.picker.key(stroke) {
            PickerOutcome::Cancel => CompareMenuOutcome::Cancel,
            PickerOutcome::Ignored => CompareMenuOutcome::Ignored,
            _ => CompareMenuOutcome::Redraw,
        }
    }

    /// Answer a pointer event: a click on a row picks it in place (a row that opens another
    /// picker opens it); a click outside the panel keeps the state and closes the menu.
    pub fn mouse(&mut self, event: &Event, theme: &ReviewTheme) -> CompareMenuOutcome {
        if let Event::Mouse(mouse) = event
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self.picker.panel_contains(mouse.position) == Some(false)
        {
            return CompareMenuOutcome::Done;
        }
        match self.picker.mouse(event, theme) {
            PickerOutcome::Cancel => CompareMenuOutcome::Cancel,
            PickerOutcome::Picked(id) => match MenuChoice::from_id(&id) {
                Some(choice) if choice.opens_picker() || choice.is_action() => {
                    CompareMenuOutcome::ApplyAndClose(choice)
                }
                Some(choice) => CompareMenuOutcome::Apply(choice),
                None => CompareMenuOutcome::Ignored,
            },
            PickerOutcome::Ignored => CompareMenuOutcome::Ignored,
            _ => CompareMenuOutcome::Redraw,
        }
    }

    /// The focused row.
    #[must_use]
    pub fn focused(&self) -> Option<MenuChoice> {
        self.picker.focused().and_then(MenuChoice::from_id)
    }

    /// The rows' text, in order (section headings included).
    #[must_use]
    pub fn rows(&self) -> Vec<String> {
        self.picker
            .items()
            .iter()
            .map(|item| item.line.plain_text())
            .collect()
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

    fn menu_state() -> MenuState {
        MenuState {
            current: Some(MenuChoice::LastLooked),
            detail: Some("4m ago".into()),
            last_looked: Some("4m ago".into()),
            last_push: Some("2h ago".into()),
            version_of: Some(7),
            only_changed: true,
            changes_view: false,
            offers: MenuOffers {
                pushes: false,
                versions: true,
            },
            view_key: Some("shift+l".into()),
            progress: None,
            stack_at: None,
            marks: MarkOptions::default(),
            legend: crate::history::badge_legend(&crate::theme::Icons::UNICODE),
            guide: false,
        }
    }

    #[test]
    fn the_menu_marks_what_is_in_effect_and_opens_on_it() {
        let theme = ReviewTheme::derive(None, IconStyle::Unicode);
        let menu = CompareMenu::new(&menu_state(), &theme);
        assert_eq!(menu.focused(), Some(MenuChoice::LastLooked));
        let rows = menu.rows();
        // Baselines and view options under their own headings.
        assert_eq!(rows[0], "Show changes since");
        assert!(rows.iter().any(|row| row == "View"), "{rows:?}");
        assert!(
            rows.contains(&"◉ you last looked · 4m ago".to_owned()),
            "{rows:?}"
        );
        assert!(rows.iter().any(|row| row.starts_with("○ 1 hour ago")));
        assert!(
            rows.iter()
                .any(|row| row.starts_with("○ two versions of #7"))
        );
        assert!(!rows.iter().any(|row| row.contains("previous push")));
        assert!(
            rows.iter()
                .any(|row| row.starts_with("[x] Only pull requests with real changes"))
        );
        assert!(
            rows.iter()
                .any(|row| row.starts_with("[ ] Only what changed") && row.contains("shift+l"))
        );
        assert_eq!(
            MenuChoice::from_id("ago:3600000"),
            Some(MenuChoice::Ago(3_600_000))
        );
        // ASCII marks.
        let ascii = ReviewTheme::derive(None, IconStyle::Ascii);
        let rows = CompareMenu::new(&menu_state(), &ascii).rows();
        assert!(
            rows.iter()
                .any(|row| row.starts_with("(*) you last looked"))
        );
    }

    #[test]
    fn mark_options_are_radio_buttons_and_check_boxes_under_their_headings() {
        let theme = ReviewTheme::derive(None, IconStyle::Unicode);
        let mut menu = CompareMenu::new(&menu_state(), &theme);
        let rows = menu.rows();
        let at = |row: &str| rows.iter().position(|text| text == row);
        let marks = at("Mark new lines").expect("mark heading");
        let seen = at("Hunks with nothing new").expect("seen heading");
        assert!(
            at("View").is_some_and(|view| view < marks) && marks < seen,
            "{rows:?}"
        );
        assert!(rows[marks + 1].starts_with("◉ bar and tint"), "{rows:?}");
        assert!(rows.iter().any(|row| row.starts_with("○ strong")));
        assert!(
            rows.iter()
                .any(|row| row.starts_with("[x] Mark changed words"))
        );
        assert!(
            rows.iter()
                .any(|row| row.starts_with("[ ] Show removed code inline"))
        );
        assert!(rows[seen + 1].starts_with("◉ show"), "{rows:?}");
        // The key to the tab badges closes the menu, telling unchanged from approved.
        let legend = rows.last().expect("legend");
        assert!(legend.contains("= unchanged since") && legend.contains("✓ approved"));
        // Every mark row picks in place, and enter on one keeps what is set.
        for choice in [
            MenuChoice::MarkStyle(MarkStyle::Strong),
            MenuChoice::Words,
            MenuChoice::RemovedInline,
            MenuChoice::Seen(SeenHunks::Fold),
        ] {
            assert_eq!(MenuChoice::from_id(&choice.id()), Some(choice));
            assert!(choice.is_option());
            while menu.focused() != Some(choice) {
                assert_eq!(menu.key(key(KeyCode::Down)), CompareMenuOutcome::Redraw);
            }
            assert_eq!(
                menu.key(key(KeyCode::Space)),
                CompareMenuOutcome::Apply(choice)
            );
            assert_eq!(menu.key(key(KeyCode::Enter)), CompareMenuOutcome::Done);
            menu.key(key(KeyCode::Home));
        }
        // The owner's new state moves the marks, keeping the cursor.
        let mut state = menu_state();
        state.marks.style = MarkStyle::Strong;
        state.marks.words = false;
        state.marks.seen = SeenHunks::Dim;
        menu.update(&state, &theme);
        let rows = menu.rows();
        assert!(rows.iter().any(|row| row.starts_with("◉ strong")));
        assert!(rows.iter().any(|row| row.starts_with("○ bar and tint")));
        assert!(
            rows.iter()
                .any(|row| row.starts_with("[ ] Mark changed words"))
        );
        assert!(rows.iter().any(|row| row.starts_with("◉ dim")));
        let ascii = ReviewTheme::derive(None, IconStyle::Ascii);
        let rows = CompareMenu::new(&state, &ascii).rows();
        assert!(rows.iter().any(|row| row.starts_with("(*) strong")));
    }

    #[test]
    fn space_picks_in_place_enter_keeps_and_esc_cancels() {
        let theme = ReviewTheme::derive(None, IconStyle::Unicode);
        let mut menu = CompareMenu::new(&menu_state(), &theme);
        // Space on a baseline previews it; the menu stays.
        assert_eq!(menu.key(key(KeyCode::Down)), CompareMenuOutcome::Redraw);
        assert_eq!(menu.focused(), Some(MenuChoice::Ago(HOUR_MS)));
        assert_eq!(
            menu.key(key(KeyCode::Space)),
            CompareMenuOutcome::Apply(MenuChoice::Ago(HOUR_MS))
        );
        // The owner shows the new state; the cursor stays where it is and the mark moves.
        let mut state = menu_state();
        state.current = Some(MenuChoice::Ago(HOUR_MS));
        state.detail = Some("1h ago".into());
        menu.update(&state, &theme);
        assert_eq!(menu.focused(), Some(MenuChoice::Ago(HOUR_MS)));
        let rows = menu.rows();
        assert!(
            rows.contains(&"◉ 1 hour ago · 1h ago".to_owned()),
            "{rows:?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.starts_with("○ you last looked · 4m ago"))
        );
        // Space on a view option flips it in place; enter on it keeps what is set.
        while menu.focused() != Some(MenuChoice::OnlyChanged) {
            menu.key(key(KeyCode::Down));
        }
        assert_eq!(
            menu.key(key(KeyCode::Space)),
            CompareMenuOutcome::Apply(MenuChoice::OnlyChanged)
        );
        assert_eq!(menu.key(key(KeyCode::Enter)), CompareMenuOutcome::Done);
        // Enter on another baseline picks it and closes; on the one in effect it just closes.
        menu.key(key(KeyCode::Home));
        assert_eq!(
            menu.key(key(KeyCode::Enter)),
            CompareMenuOutcome::ApplyAndClose(MenuChoice::LastLooked)
        );
        menu.key(key(KeyCode::Down));
        assert_eq!(menu.key(key(KeyCode::Enter)), CompareMenuOutcome::Done);
        // A row that opens another picker opens it with space too.
        while menu.focused() != Some(MenuChoice::Version) {
            menu.key(key(KeyCode::Down));
        }
        assert_eq!(
            menu.key(key(KeyCode::Space)),
            CompareMenuOutcome::ApplyAndClose(MenuChoice::Version)
        );
        assert_eq!(menu.key(key(KeyCode::Escape)), CompareMenuOutcome::Cancel);
    }

    #[test]
    fn a_comparison_offers_a_guide_of_its_changes_that_closes_the_menu() {
        let theme = ReviewTheme::derive(None, IconStyle::Unicode);
        let plain = CompareMenu::new(&menu_state(), &theme).rows();
        assert!(!plain.iter().any(|row| row.contains("Guide these changes")));
        let mut state = menu_state();
        state.guide = true;
        let mut menu = CompareMenu::new(&state, &theme);
        assert!(
            menu.rows()[0].contains("Guide these changes"),
            "{:?}",
            menu.rows()
        );
        menu.key(key(KeyCode::Home));
        assert_eq!(menu.focused(), Some(MenuChoice::Guide));
        assert_eq!(
            menu.key(key(KeyCode::Space)),
            CompareMenuOutcome::ApplyAndClose(MenuChoice::Guide)
        );
        assert_eq!(
            menu.key(key(KeyCode::Enter)),
            CompareMenuOutcome::ApplyAndClose(MenuChoice::Guide)
        );
    }

    #[test]
    fn clicks_pick_in_place_and_a_click_outside_keeps_and_closes() {
        use crate::testing::harness::paint;
        use bmux_tui::event::MouseEvent;
        use bmux_tui::geometry::Point;
        let theme = ReviewTheme::derive(None, IconStyle::Unicode);
        let mut menu = CompareMenu::new(&menu_state(), &theme);
        let screen = paint(90, 30, |area, cx| menu.paint(&theme, area, cx));
        let event = |kind: MouseEventKind, x: u16, y: u16| {
            Event::Mouse(MouseEvent::new(kind, Point::new(x, y)))
        };
        let mut click = |x: u16, y: u16| {
            let down = menu.mouse(
                &event(MouseEventKind::Down(MouseButton::Left), x, y),
                &theme,
            );
            if down == CompareMenuOutcome::Done {
                return down;
            }
            menu.mouse(&event(MouseEventKind::Up(MouseButton::Left), x, y), &theme)
        };
        let row = (0..screen.height())
            .find(|y| screen.row_text(*y).contains("[x] Only pull requests"))
            .expect("the option row");
        let x = screen.column_of(row, "[x]").expect("its column");
        assert_eq!(
            click(x + 1, row),
            CompareMenuOutcome::Apply(MenuChoice::OnlyChanged)
        );
        let row = (0..screen.height())
            .find(|y| screen.row_text(*y).contains("a week ago"))
            .expect("a baseline row");
        let x = screen.column_of(row, "a week ago").expect("its column");
        assert_eq!(
            click(x, row),
            CompareMenuOutcome::Apply(MenuChoice::Ago(7 * 24 * HOUR_MS))
        );
        assert_eq!(click(0, screen.height() - 1), CompareMenuOutcome::Done);
    }

    #[test]
    fn the_menu_fits_tiny_areas() {
        use crate::testing::harness::paint;
        let theme = ReviewTheme::derive(None, IconStyle::Unicode);
        let mut menu = CompareMenu::new(&menu_state(), &theme);
        for (width, height) in [(0_u16, 0_u16), (1, 1), (8, 4), (20, 6), (40, 12)] {
            let screen = paint(width, height, |area, cx| menu.paint(&theme, area, cx));
            for hit in screen.hits() {
                assert!(
                    hit.area.right() <= width && hit.area.bottom() <= height,
                    "{width}x{height}: {hit:?}"
                );
            }
        }
    }
}
