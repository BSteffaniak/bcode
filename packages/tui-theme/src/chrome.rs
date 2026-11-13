//! Renderer chrome: the shape of the interface, independent of color.
//!
//! `[chrome.tui]` selects border glyphs, focus effects, icon sets, density,
//! separators, speaker-label treatment, surface layering, and motion. Every
//! value is a closed enumeration mapped onto BMUX primitives by the terminal
//! renderer; themes never supply layout code. Sections for other renderers
//! (`[chrome.<renderer>]`) are preserved as opaque data for those renderers.

use std::collections::BTreeMap;

use bmux_tui::style::Color;
use serde::{Deserialize, Serialize};

/// Border glyph family.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BorderStyle {
    /// No border.
    None,
    /// ASCII-safe `+-|`.
    Ascii,
    /// Single line.
    #[default]
    Single,
    /// Rounded corners.
    Rounded,
    /// Heavy lines.
    Thick,
    /// Double lines.
    Double,
    /// A single leading bar only (transcript containers).
    Left,
    /// Full-width horizontal rules above and below, no sides (the composer).
    Rules,
}

/// Focus effect for focused borders.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusEffect {
    /// Focus uses the focused border style only.
    #[default]
    None,
    /// Static gradient across the focused border.
    Gradient,
    /// Gradient that sweeps around the border (respects reduced motion).
    Sweep,
}

/// Icon set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IconSet {
    /// No icons; text labels only.
    None,
    /// Portable Unicode symbols.
    #[default]
    Unicode,
    /// Nerd Font glyphs (requires a patched font; falls back to Unicode width rules).
    Nerd,
}

/// Spacing density.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Density {
    /// No padding; for small terminals.
    Compact,
    /// Balanced spacing.
    #[default]
    Cozy,
    /// Generous padding.
    Comfortable,
}

/// Separator glyph style between status segments.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeparatorStyle {
    /// Middle dot / spaces.
    #[default]
    Plain,
    /// Vertical bar.
    Bar,
    /// Slanted wedges.
    Slant,
    /// Powerline arrows (requires a powerline-capable font).
    Powerline,
}

/// Speaker-label treatment in the transcript.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeakerStyle {
    /// Plain text prefix.
    #[default]
    Prefix,
    /// Filled badge.
    Badge,
    /// Colored bar along the message.
    Bar,
    /// Label in a left gutter.
    Gutter,
    /// No heading row: the speaker's marker glyph (`markers`) precedes the first body row and
    /// the body hangs beside it.
    Marker,
}

/// Surface layering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceStyle {
    /// Raised and overlay surfaces share the canvas.
    #[default]
    Flat,
    /// Raised and overlay surfaces use their own fills.
    Layered,
}

/// Motion level for animated chrome.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Motion {
    /// No animation.
    None,
    /// Subtle, low-frequency animation.
    #[default]
    Subtle,
    /// Full animation.
    Full,
}

/// Whether a piece of screen chrome is drawn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// Drawn.
    #[default]
    Shown,
    /// Not drawn.
    Hidden,
}

impl Visibility {
    /// Whether the chrome is drawn.
    #[must_use]
    pub const fn is_shown(self) -> bool {
        matches!(self, Self::Shown)
    }
}

/// Where the status line sits relative to the composer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusPlacement {
    /// Between the transcript and the composer.
    #[default]
    AboveComposer,
    /// Below the composer, as a footer.
    BelowComposer,
}

/// One segment of the status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusSegment {
    /// Connection state, readiness, and (unless it has its own row) what the agent is doing.
    Activity,
    /// Context window use.
    Context,
    /// Token and cost accounting.
    Usage,
    /// Child session activity.
    Children,
    /// Long-running background progress.
    Progress,
    /// Plugin status contributions.
    Plugins,
    /// The latest status message and execution mode.
    Message,
    /// The reading position in the transcript.
    Position,
    /// Key hints.
    Hints,
}

impl StatusSegment {
    /// Every segment, in the default order.
    pub const ALL: [Self; 9] = [
        Self::Activity,
        Self::Context,
        Self::Usage,
        Self::Children,
        Self::Progress,
        Self::Plugins,
        Self::Message,
        Self::Position,
        Self::Hints,
    ];
}

/// The status line's segments, in display order; stored inline so chrome stays `Copy`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct StatusSegments {
    len: u8,
    items: [StatusSegment; StatusSegment::ALL.len()],
}

impl Default for StatusSegments {
    fn default() -> Self {
        Self {
            len: u8::try_from(StatusSegment::ALL.len()).unwrap_or(u8::MAX),
            items: StatusSegment::ALL,
        }
    }
}

impl StatusSegments {
    /// `segments` in order, each at most once; `None` when one repeats.
    #[must_use]
    pub fn new(segments: &[StatusSegment]) -> Option<Self> {
        let mut items = StatusSegment::ALL;
        for (index, segment) in segments.iter().enumerate() {
            if segments[..index].contains(segment) {
                return None;
            }
            items[index] = *segment;
        }
        Some(Self {
            len: u8::try_from(segments.len()).ok()?,
            items,
        })
    }

    /// The segments, in display order.
    pub fn iter(&self) -> impl Iterator<Item = StatusSegment> + '_ {
        self.items[..usize::from(self.len)].iter().copied()
    }
}

impl std::fmt::Debug for StatusSegments {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl Serialize for StatusSegments {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter())
    }
}

impl<'de> Deserialize<'de> for StatusSegments {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let segments = Vec::<StatusSegment>::deserialize(deserializer)?;
        Self::new(&segments)
            .ok_or_else(|| serde::de::Error::custom("status segments must not repeat"))
    }
}

/// Status line placement, indentation, and segments.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StatusChrome {
    /// Placement relative to the composer.
    pub placement: StatusPlacement,
    /// Leading blank columns, at most [`MAX_STATUS_INDENT`].
    pub indent: u8,
    /// Shown segments in display order; segments left out are not shown.
    pub segments: StatusSegments,
}

/// Widest status line indent.
pub const MAX_STATUS_INDENT: u8 = 8;

/// Longest [`ChromeText`], in bytes.
pub const MAX_CHROME_TEXT_BYTES: usize = 48;

/// Short single-line chrome text: a glyph, prompt, or placeholder.
///
/// At most [`MAX_CHROME_TEXT_BYTES`] bytes and no control characters. Stored inline so resolved
/// chrome stays `Copy`; renderers measure it with their text-layout primitives.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChromeText {
    len: u8,
    bytes: [u8; MAX_CHROME_TEXT_BYTES],
}

impl ChromeText {
    /// `text` when it fits and contains no control characters.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        if text.len() > MAX_CHROME_TEXT_BYTES || text.chars().any(char::is_control) {
            return None;
        }
        let mut bytes = [0; MAX_CHROME_TEXT_BYTES];
        bytes[..text.len()].copy_from_slice(text.as_bytes());
        Some(Self {
            len: u8::try_from(text.len()).ok()?,
            bytes,
        })
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or_default()
    }
}

impl std::fmt::Debug for ChromeText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_str().fmt(f)
    }
}

impl Serialize for ChromeText {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ChromeText {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::new(&text).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "chrome text must be at most {MAX_CHROME_TEXT_BYTES} bytes without control characters"
            ))
        })
    }
}

/// What the transcript keeps in view while a reply streams in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FollowPolicy {
    /// Hold a new reply's (or submission's) first row at the viewport top, then follow the
    /// latest content once it overflows.
    #[default]
    Reply,
    /// Keep the latest content at the viewport bottom.
    Latest,
}

/// Marker glyphs; `None` uses the renderer's default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MarkerGlyphs {
    /// Before user messages (the `marker` speaker treatment).
    pub user: Option<ChromeText>,
    /// Before assistant messages (the `marker` speaker treatment).
    pub assistant: Option<ChromeText>,
    /// The `bullet` header field, styled by the unit's status.
    pub bullet: Option<ChromeText>,
    /// Before the first body row of units whose `section_labels` is `connector`.
    pub connector: Option<ChromeText>,
}

/// Most items in a [`ChromeList`].
pub const MAX_CHROME_LIST_ITEMS: usize = 32;

/// Most bytes of all items of a [`ChromeList`].
pub const MAX_CHROME_LIST_BYTES: usize = 384;

/// A short list of chrome texts (spinner verbs, for example), stored inline so resolved chrome
/// stays `Copy`.
///
/// At most [`MAX_CHROME_LIST_ITEMS`] items of at most [`MAX_CHROME_TEXT_BYTES`] bytes each,
/// [`MAX_CHROME_LIST_BYTES`] in all, none empty or containing control characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChromeList {
    len: u16,
    bytes: [u8; MAX_CHROME_LIST_BYTES],
}

impl Default for ChromeList {
    fn default() -> Self {
        Self {
            len: 0,
            bytes: [0; MAX_CHROME_LIST_BYTES],
        }
    }
}

impl ChromeList {
    /// `items` when they fit the bounds.
    #[must_use]
    pub fn new<S: AsRef<str>>(items: &[S]) -> Option<Self> {
        if items.len() > MAX_CHROME_LIST_ITEMS {
            return None;
        }
        let mut list = Self::default();
        let mut len = 0;
        for (index, item) in items.iter().enumerate() {
            let item = item.as_ref();
            if item.is_empty()
                || item.len() > MAX_CHROME_TEXT_BYTES
                || item.chars().any(char::is_control)
            {
                return None;
            }
            // Items are joined by a line feed, which no item may contain.
            let separator = usize::from(index > 0);
            if len + separator + item.len() > MAX_CHROME_LIST_BYTES {
                return None;
            }
            if separator == 1 {
                list.bytes[len] = b'\n';
                len += 1;
            }
            list.bytes[len..len + item.len()].copy_from_slice(item.as_bytes());
            len += item.len();
        }
        list.len = u16::try_from(len).ok()?;
        Some(list)
    }

    /// The items, in order.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        let text = std::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or_default();
        text.split('\n').filter(|item| !item.is_empty())
    }

    /// Whether the list has no items.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The `index`th item, wrapping around the list; `None` when empty.
    #[must_use]
    pub fn cycled(&self, index: usize) -> Option<&str> {
        let count = self.iter().count();
        (count > 0)
            .then(|| self.iter().nth(index % count))
            .flatten()
    }
}

impl std::fmt::Debug for ChromeList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl Serialize for ChromeList {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter())
    }
}

impl<'de> Deserialize<'de> for ChromeList {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let items = Vec::<String>::deserialize(deserializer)?;
        Self::new(&items).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "chrome lists hold at most {MAX_CHROME_LIST_ITEMS} non-empty single-line items of \
                 at most {MAX_CHROME_TEXT_BYTES} bytes, {MAX_CHROME_LIST_BYTES} bytes in all"
            ))
        })
    }
}

/// Where activity (the spinner and what the agent is doing) is shown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityPlacement {
    /// As the status line's first segment.
    #[default]
    Status,
    /// On its own row above the composer, present only while busy.
    Line,
}

/// Activity presentation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ActivityChrome {
    /// Where activity is shown.
    pub placement: ActivityPlacement,
    /// Spinner frames, one grapheme each; `None` keeps the renderer's spinner.
    pub spinner: Option<ChromeText>,
    /// Verbs describing the agent at work; one is chosen per busy period and shown in place of
    /// the detailed activity label. Empty shows the label.
    pub verbs: ChromeList,
    /// Sweep a highlight (`activity.shimmer`) across the activity text, as motion allows.
    pub shimmer: bool,
}

/// How fenced code blocks are framed in Markdown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeBlockFrame {
    /// A header with the language, a bar before each row, and a footer.
    #[default]
    Framed,
    /// Highlighted rows only.
    Plain,
}

/// How Markdown table header cells align.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TableHeaders {
    /// Like their column.
    #[default]
    Column,
    /// Centered.
    Center,
}

/// Markdown structure glyphs and options; `None` keeps the renderer's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MarkdownChrome {
    /// Unordered list bullet.
    pub bullet: Option<ChromeText>,
    /// Spaces after a list marker, at most [`MAX_MARKER_GAP`].
    pub marker_gap: Option<u8>,
    /// Fenced code block framing.
    pub code_blocks: CodeBlockFrame,
    /// Block quote bar.
    pub quote_bar: Option<ChromeText>,
    /// Table header alignment.
    pub table_headers: TableHeaders,
    /// Typographic quotes, dashes, and ellipses; `false` keeps text as written.
    pub smart_punctuation: bool,
}

impl Default for MarkdownChrome {
    fn default() -> Self {
        Self {
            bullet: None,
            marker_gap: None,
            code_blocks: CodeBlockFrame::Framed,
            quote_bar: None,
            table_headers: TableHeaders::Column,
            smart_punctuation: true,
        }
    }
}

/// Widest Markdown list marker gap.
pub const MAX_MARKER_GAP: u8 = 4;

/// Where decision dialogs (permission requests) appear.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DialogPlacement {
    /// A centered modal over a dimmed screen.
    #[default]
    Modal,
    /// Docked at the bottom in place of the composer and status line; the transcript stays
    /// visible above it.
    Docked,
}

/// How a dialog presents its actions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DialogActions {
    /// A row of buttons.
    #[default]
    Buttons,
    /// A numbered list, one action per row; digits choose an action.
    Numbered,
}

/// Decision dialog presentation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DialogChrome {
    /// Where dialogs appear.
    pub placement: DialogPlacement,
    /// How actions are presented.
    pub actions: DialogActions,
}

/// The input composer's chrome.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ComposerChrome {
    /// Glyph drawn before the first input row; `None` draws none.
    pub prompt: Option<ChromeText>,
    /// The frame title, drawn by full frames only.
    pub title: Visibility,
    /// Placeholder of an empty input; `None` keeps the host's.
    pub placeholder: Option<ChromeText>,
    /// The send and send-options controls.
    pub controls: Visibility,
}

/// Border style per surface; `None` keeps each component's own default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TuiBorders {
    /// Panels and panes.
    pub panel: Option<BorderStyle>,
    /// Dialogs and modals.
    pub dialog: Option<BorderStyle>,
    /// Tool cards.
    pub tool: Option<BorderStyle>,
    /// The composer.
    pub composer: Option<BorderStyle>,
}

/// Resolved terminal chrome.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct TuiChrome {
    /// Border style per surface.
    pub borders: TuiBorders,
    /// Focus effect.
    pub focus: FocusEffect,
    /// Gradient colors for focus effects; empty uses the focused border color.
    pub focus_colors: Vec<Color>,
    /// Icon set.
    pub icons: IconSet,
    /// Spacing density.
    pub density: Density,
    /// Status separators.
    pub separators: SeparatorStyle,
    /// Speaker-label treatment.
    pub speaker: SpeakerStyle,
    /// Surface layering.
    pub surfaces: SurfaceStyle,
    /// Motion level.
    pub motion: Motion,
    /// The header row at the top of the screen.
    pub header: Visibility,
    /// The marker at the start of a session's transcript.
    pub session_start: Visibility,
    /// Status line placement.
    pub status: StatusChrome,
    /// The input composer.
    pub composer: ComposerChrome,
    /// Speaker marker glyphs.
    pub markers: MarkerGlyphs,
    /// What the transcript keeps in view as replies arrive.
    pub follow: FollowPolicy,
    /// Activity presentation.
    pub activity: ActivityChrome,
    /// Markdown structure.
    pub markdown: MarkdownChrome,
    /// Decision dialogs.
    pub dialogs: DialogChrome,
}

/// Maximum focus gradient colors.
pub const MAX_FOCUS_COLORS: usize = 4;

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawBorders {
    #[serde(default)]
    pub panel: Option<BorderStyle>,
    #[serde(default)]
    pub dialog: Option<BorderStyle>,
    #[serde(default)]
    pub tool: Option<BorderStyle>,
    #[serde(default)]
    pub composer: Option<BorderStyle>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawFocus {
    #[serde(default)]
    pub effect: Option<FocusEffect>,
    #[serde(default)]
    pub colors: Option<Vec<crate::definition::RawColor>>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawSeparators {
    #[serde(default)]
    pub status: Option<SeparatorStyle>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawStatus {
    #[serde(default)]
    pub placement: Option<StatusPlacement>,
    #[serde(default)]
    pub indent: Option<u8>,
    #[serde(default)]
    pub segments: Option<StatusSegments>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawComposer {
    #[serde(default)]
    pub prompt: Option<ChromeText>,
    #[serde(default)]
    pub title: Option<Visibility>,
    #[serde(default)]
    pub placeholder: Option<ChromeText>,
    #[serde(default)]
    pub controls: Option<Visibility>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawDialogs {
    #[serde(default)]
    pub placement: Option<DialogPlacement>,
    #[serde(default)]
    pub actions: Option<DialogActions>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawMarkdown {
    #[serde(default)]
    pub bullet: Option<ChromeText>,
    #[serde(default)]
    pub marker_gap: Option<u8>,
    #[serde(default)]
    pub code_blocks: Option<CodeBlockFrame>,
    #[serde(default)]
    pub quote_bar: Option<ChromeText>,
    #[serde(default)]
    pub table_headers: Option<TableHeaders>,
    #[serde(default)]
    pub smart_punctuation: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawActivity {
    #[serde(default)]
    pub placement: Option<ActivityPlacement>,
    #[serde(default)]
    pub spinner: Option<ChromeText>,
    #[serde(default)]
    pub verbs: Option<ChromeList>,
    #[serde(default)]
    pub shimmer: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawMarkers {
    #[serde(default)]
    pub user: Option<ChromeText>,
    #[serde(default)]
    pub assistant: Option<ChromeText>,
    #[serde(default)]
    pub bullet: Option<ChromeText>,
    #[serde(default)]
    pub connector: Option<ChromeText>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawTuiChrome {
    #[serde(default)]
    pub borders: Option<RawBorders>,
    #[serde(default)]
    pub focus: Option<RawFocus>,
    #[serde(default)]
    pub icons: Option<IconSet>,
    #[serde(default)]
    pub density: Option<Density>,
    #[serde(default)]
    pub separators: Option<RawSeparators>,
    #[serde(default)]
    pub speaker: Option<SpeakerStyle>,
    #[serde(default)]
    pub surfaces: Option<SurfaceStyle>,
    #[serde(default)]
    pub motion: Option<Motion>,
    #[serde(default)]
    pub header: Option<Visibility>,
    #[serde(default)]
    pub session_start: Option<Visibility>,
    #[serde(default)]
    pub status: Option<RawStatus>,
    #[serde(default)]
    pub composer: Option<RawComposer>,
    #[serde(default)]
    pub markers: Option<RawMarkers>,
    #[serde(default)]
    pub follow: Option<FollowPolicy>,
    #[serde(default)]
    pub activity: Option<RawActivity>,
    #[serde(default)]
    pub markdown: Option<RawMarkdown>,
    #[serde(default)]
    pub dialogs: Option<RawDialogs>,
}

/// `[chrome]` table: the terminal section plus opaque sections for other renderers.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub(crate) struct RawChrome {
    #[serde(default)]
    pub tui: Option<RawTuiChrome>,
    #[serde(flatten)]
    pub other: BTreeMap<String, toml::Value>,
}

impl RawTuiChrome {
    /// Overlay `other`; set fields in `other` win, nested tables merge per field.
    pub(crate) fn merge(&mut self, other: &Self) {
        if let Some(borders) = &other.borders {
            let target = self.borders.get_or_insert_with(RawBorders::default);
            for (slot, value) in [
                (&mut target.panel, borders.panel),
                (&mut target.dialog, borders.dialog),
                (&mut target.tool, borders.tool),
                (&mut target.composer, borders.composer),
            ] {
                if value.is_some() {
                    *slot = value;
                }
            }
        }
        if let Some(focus) = &other.focus {
            let target = self.focus.get_or_insert_with(RawFocus::default);
            if focus.effect.is_some() {
                target.effect = focus.effect;
            }
            if focus.colors.is_some() {
                target.colors.clone_from(&focus.colors);
            }
        }
        if let Some(separators) = &other.separators
            && separators.status.is_some()
        {
            self.separators
                .get_or_insert_with(RawSeparators::default)
                .status = separators.status;
        }
        macro_rules! take {
            ($($field:ident),*) => {$(
                if other.$field.is_some() {
                    self.$field = other.$field;
                }
            )*};
        }
        take!(
            icons,
            density,
            speaker,
            surfaces,
            motion,
            header,
            session_start,
            follow
        );
        // Nested tables merge per field: set fields in `other` win.
        macro_rules! overlay {
            ($table:ident, $raw:ident: $($field:ident),*) => {
                if let Some(source) = &other.$table {
                    let target = self.$table.get_or_insert_with($raw::default);
                    $(if source.$field.is_some() {
                        target.$field = source.$field;
                    })*
                }
            };
        }
        overlay!(activity, RawActivity: placement, spinner, verbs, shimmer);
        overlay!(markers, RawMarkers: user, assistant, bullet, connector);
        overlay!(composer, RawComposer: prompt, title, placeholder, controls);
        overlay!(status, RawStatus: placement, indent, segments);
        overlay!(dialogs, RawDialogs: placement, actions);
        overlay!(
            markdown,
            RawMarkdown: bullet,
            marker_gap,
            code_blocks,
            quote_bar,
            table_headers,
            smart_punctuation
        );
    }
}

impl RawChrome {
    pub(crate) fn merge(&mut self, other: &Self) {
        if let Some(tui) = &other.tui {
            self.tui
                .get_or_insert_with(RawTuiChrome::default)
                .merge(tui);
        }
        crate::definition::deep_merge_extensions(&mut self.other, &other.other);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_merges_field_by_field() {
        let mut base: RawChrome = toml::from_str(
            r#"
            [tui]
            borders = { panel = "rounded", dialog = "thick" }
            icons = "nerd"
            "#,
        )
        .unwrap();
        let overlay: RawChrome = toml::from_str(
            r#"
            [tui]
            borders = { panel = "ascii" }
            motion = "none"

            [web]
            radius = 6
            "#,
        )
        .unwrap();
        base.merge(&overlay);
        let tui = base.tui.unwrap();
        let borders = tui.borders.unwrap();
        assert_eq!(borders.panel, Some(BorderStyle::Ascii));
        assert_eq!(borders.dialog, Some(BorderStyle::Thick));
        assert_eq!(tui.icons, Some(IconSet::Nerd));
        assert_eq!(tui.motion, Some(Motion::None));
        assert!(base.other.contains_key("web"));
    }

    #[test]
    fn unknown_tui_chrome_keys_are_rejected() {
        assert!(toml::from_str::<RawChrome>("[tui]\nborder = \"rounded\"").is_err());
        assert!(toml::from_str::<RawChrome>("[tui]\nicons = \"emoji\"").is_err());
    }

    #[test]
    fn screen_chrome_merges_per_field_and_validates_text() {
        let mut base: RawChrome = toml::from_str(
            r#"
            [tui]
            header = "hidden"
            status = { placement = "below_composer", indent = 2 }
            composer = { prompt = "> ", controls = "hidden" }
            "#,
        )
        .unwrap();
        let overlay: RawChrome = toml::from_str(
            r#"
            [tui]
            status = { indent = 4 }
            composer = { prompt = "❯ ", title = "hidden" }
            "#,
        )
        .unwrap();
        base.merge(&overlay);
        let tui = base.tui.unwrap();
        assert_eq!(tui.header, Some(Visibility::Hidden));
        let status = tui.status.unwrap();
        assert_eq!(status.placement, Some(StatusPlacement::BelowComposer));
        assert_eq!(status.indent, Some(4));
        let composer = tui.composer.unwrap();
        assert_eq!(
            composer.prompt.map(|text| text.as_str().to_owned()),
            Some("❯ ".to_owned())
        );
        assert_eq!(composer.title, Some(Visibility::Hidden));
        assert_eq!(composer.controls, Some(Visibility::Hidden));
        // Chrome text is one bounded line.
        assert!(toml::from_str::<RawChrome>("[tui]\ncomposer = { prompt = \"a\\nb\" }").is_err());
        let long = "x".repeat(MAX_CHROME_TEXT_BYTES + 1);
        assert!(
            toml::from_str::<RawChrome>(&format!("[tui]\ncomposer = {{ prompt = \"{long}\" }}"))
                .is_err()
        );
        assert!(toml::from_str::<RawChrome>("[tui]\nheader = \"faded\"").is_err());
    }

    #[test]
    fn chrome_lists_are_bounded_single_line_items() {
        let list = ChromeList::new(&["Mulling", "Pondering", "中文"]).expect("fits");
        assert_eq!(
            list.iter().collect::<Vec<_>>(),
            ["Mulling", "Pondering", "中文"]
        );
        assert_eq!(list.cycled(4), Some("Pondering"));
        assert_eq!(ChromeList::default().cycled(0), None);
        assert!(ChromeList::new(&[""]).is_none());
        assert!(ChromeList::new(&["a\nb"]).is_none());
        assert!(ChromeList::new(&["x".repeat(MAX_CHROME_TEXT_BYTES + 1)]).is_none());
        assert!(ChromeList::new(&vec!["x"; MAX_CHROME_LIST_ITEMS + 1]).is_none());
        let parsed: RawChrome = toml::from_str(
            "[tui]\nactivity = { placement = \"line\", spinner = \"✢✳\", verbs = [\"Mulling\"], shimmer = true }",
        )
        .unwrap();
        let activity = parsed.tui.unwrap().activity.unwrap();
        assert_eq!(activity.placement, Some(ActivityPlacement::Line));
        assert_eq!(activity.verbs.map(|verbs| verbs.iter().count()), Some(1));
        assert!(toml::from_str::<RawChrome>("[tui]\nactivity = { verbs = [\"\"] }").is_err());
    }
}
