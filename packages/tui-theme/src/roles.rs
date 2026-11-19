//! Registry of semantic style roles.
//!
//! The registry is the single inventory of roles Bcode understands. It drives
//! unknown-role diagnostics, `bcode theme roles`, documentation, and — for
//! schema-v2 themes — derivation of every unset role from the seed palette.

/// Role category, used for documentation and listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RoleCategory {
    /// Canvas, surfaces, text, and borders.
    Base,
    /// Interactive controls, focus, and selection.
    Interaction,
    /// Semantic states.
    State,
    /// Transcript speakers and metadata.
    Transcript,
    /// Tool lifecycle presentation.
    Tool,
    /// Markdown presentation.
    Markdown,
    /// Syntax highlighting.
    Syntax,
    /// Source cards and diffs.
    Source,
}

impl RoleCategory {
    /// Display name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Interaction => "interaction",
            Self::State => "state",
            Self::Transcript => "transcript",
            Self::Tool => "tool",
            Self::Markdown => "markdown",
            Self::Syntax => "syntax",
            Self::Source => "source",
        }
    }
}

/// How a schema-v2 theme derives a role it leaves unset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DerivedStyle {
    /// Foreground expression.
    pub fg: Option<&'static str>,
    /// Background expression.
    pub bg: Option<&'static str>,
    /// Modifier names.
    pub modifiers: &'static [&'static str],
}

impl DerivedStyle {
    const fn fg(expr: &'static str) -> Self {
        Self {
            fg: Some(expr),
            bg: None,
            modifiers: &[],
        }
    }

    const fn bg(expr: &'static str) -> Self {
        Self {
            fg: None,
            bg: Some(expr),
            modifiers: &[],
        }
    }

    const fn with(mut self, modifiers: &'static [&'static str]) -> Self {
        self.modifiers = modifiers;
        self
    }

    const fn and_bg(mut self, expr: &'static str) -> Self {
        self.bg = Some(expr);
        self
    }
}

/// One registered role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleSpec {
    /// Role key as written in `[styles."key"]`.
    pub key: &'static str,
    /// Category.
    pub category: RoleCategory,
    /// One-line description.
    pub description: &'static str,
    /// Derivation from seeds for schema-v2 themes.
    pub derive: Option<DerivedStyle>,
}

const fn role(
    key: &'static str,
    category: RoleCategory,
    description: &'static str,
    derive: Option<DerivedStyle>,
) -> RoleSpec {
    RoleSpec {
        key,
        category,
        description,
        derive,
    }
}

/// Palette names schema-v2 themes use as seeds.
///
/// `background`, `text`, and `accent` anchor derivation; the hue slots feed
/// states, syntax, and Markdown. Optional slots fall back through `or(…)`.
pub const SEED_PALETTE: &[(&str, &str)] = &[
    ("background", "canvas background"),
    ("text", "primary text"),
    ("accent", "interactive accent"),
    ("info", "informational state (defaults to blue/cyan)"),
    ("success", "success state (defaults to green)"),
    ("warning", "warning state (defaults to yellow)"),
    ("error", "error state (defaults to red)"),
    ("red", "hue slot"),
    ("green", "hue slot"),
    ("yellow", "hue slot"),
    ("blue", "hue slot"),
    ("magenta", "hue slot"),
    ("cyan", "hue slot"),
    ("orange", "optional hue slot"),
    ("violet", "optional hue slot"),
];

/// Optional palette slots for colors a source scheme declares exactly.
///
/// Each slot feeds the roles listed beside it unchanged. A palette that omits
/// a slot keeps the seed derivation for those roles, so slots only ever make
/// a palette more exact.
pub const EXACT_PALETTE: &[(&str, &str)] = &[
    (
        "muted",
        "secondary text: text.muted, metadata, labels, diff context",
    ),
    ("comment", "code comments: syntax.comment"),
    (
        "border",
        "window and panel borders: border.default, source.border",
    ),
    ("border_focused", "focused borders: border.focused"),
    ("surface", "raised panels and popups: surface.raised"),
    (
        "surface_overlay",
        "modal panels: surface.overlay (defaults to surface)",
    ),
    ("selection", "selection fill: selection.active background"),
    (
        "line_highlight",
        "current-line fill: selection.inactive background",
    ),
    ("gutter", "line numbers: source.gutter"),
    ("diff_add", "added diff lines: diff.added"),
    ("diff_remove", "removed diff lines: diff.removed"),
    ("diff_add_bg", "added row fill: diff.added_row"),
    ("diff_remove_bg", "removed row fill: diff.removed_row"),
    (
        "diff_add_emphasis_bg",
        "intraline added fill: diff.added_emphasis",
    ),
    (
        "diff_remove_emphasis_bg",
        "intraline removed fill: diff.removed_emphasis",
    ),
    ("syntax_keyword", "keywords: syntax.keyword"),
    ("syntax_function", "functions: syntax.function"),
    ("syntax_variable", "variables: syntax.variable"),
    ("syntax_string", "strings: syntax.string"),
    ("syntax_number", "numbers and constants: syntax.number"),
    ("syntax_type", "types: syntax.type"),
    ("syntax_operator", "operators: syntax.operator"),
    ("syntax_punctuation", "punctuation: syntax.punctuation"),
    (
        "markup_heading",
        "headings: markdown.heading, syntax.heading",
    ),
    ("markup_link", "links: link, markdown.link, syntax.link"),
    ("markup_raw", "markup literals: syntax.raw"),
    ("markup_code", "inline code text: markdown.inline_code"),
    ("markup_code_bg", "inline code fill: markdown.inline_code"),
    ("markup_list", "list markers: markdown.list_marker"),
];

// Derivation building blocks. Every expression resolves only from the
// palette, so a v2 theme with RGB seeds derives a complete presentation.
// `readable(…)` keeps a color taken directly from the palette exactly and
// raises only computed colors to the ratio.
const INFO: &str = "readable(or($info, $blue, $cyan, $accent), $background, 4.5)";
const SUCCESS: &str = "readable(or($success, $green, $accent), $background, 4.5)";
const WARNING: &str = "readable(or($warning, $yellow, $orange, $accent), $background, 4.5)";
const ERROR: &str = "readable(or($error, $red, $accent), $background, 4.5)";
const ACCENT: &str = "readable($accent, $background, 4.5)";
const TEXT: &str = "readable($text, $background, 4.5)";
const MUTED: &str = "readable(or($muted, mix($text, $background, 0.6)), $background, 3.0)";
const BORDER: &str = "or($border, mix($text, $background, 0.25))";
/// Markdown rules and frames: lines inside content rather than window borders.
const RULE: &str = "mix($text, $background, 0.25)";
const VIOLET: &str = "readable(or($violet, $magenta, $accent), $background, 4.5)";
const CYAN: &str = "readable(or($cyan, $blue, $accent), $background, 4.5)";
const BLUE: &str = "readable(or($blue, $cyan, $accent), $background, 4.5)";
const FOCUSED: &str = "readable(or($border_focused, $accent), $background, 4.5)";
const HEADING: &str = "readable(or($markup_heading, $accent), $background, 4.5)";
const LINK: &str = "readable(or($markup_link, $info, $blue, $cyan, $accent), $background, 4.5)";
const COMMENT: &str =
    "readable(or($comment, $muted, mix($text, $background, 0.6)), $background, 3.0)";
const GUTTER: &str =
    "readable(or($gutter, $muted, mix($text, $background, 0.6)), $background, 3.0)";
const PUNCTUATION: &str = "readable(or($syntax_punctuation, $muted, \
     mix($text, $background, 0.6)), $background, 3.0)";
const KEYWORD: &str = "readable(or($syntax_keyword, $violet, $magenta, $accent), $background, 4.5)";
const FUNCTION: &str = "readable(or($syntax_function, $blue, $cyan, $accent), $background, 4.5)";
const VARIABLE: &str = "readable(or($syntax_variable, $text), $background, 4.5)";
const STRING: &str = "readable(or($syntax_string, $green, $success, $accent), $background, 4.5)";
const NUMBER: &str = "readable(or($syntax_number, $orange, $yellow, $accent), $background, 4.5)";
const TYPE: &str = "readable(or($syntax_type, $cyan, $blue, $accent), $background, 4.5)";
const OPERATOR: &str = "readable(or($syntax_operator, $cyan, $blue, $accent), $background, 4.5)";
const RAW: &str = "readable(or($markup_raw, $green, $success, $accent), $background, 4.5)";
const INLINE_CODE: &str = "readable(or($markup_code, $orange, $yellow, $accent), $background, 4.5)";
const LIST_MARKER: &str = "readable(or($markup_list, $accent), $background, 4.5)";
const DIFF_ADDED: &str = "readable(or($diff_add, $success, $green, $accent), $background, 4.5)";
const DIFF_REMOVED: &str = "readable(or($diff_remove, $error, $red, $accent), $background, 4.5)";

/// Every registered role, in category order.
pub const ROLES: &[RoleSpec] = &[
    // Base
    role(
        "canvas",
        RoleCategory::Base,
        "complete frame fill",
        Some(DerivedStyle::fg(TEXT).and_bg("$background")),
    ),
    role(
        "text.primary",
        RoleCategory::Base,
        "primary text",
        Some(DerivedStyle::fg(TEXT)),
    ),
    role(
        "text.muted",
        RoleCategory::Base,
        "secondary text and metadata",
        Some(DerivedStyle::fg(MUTED)),
    ),
    role(
        "border.default",
        RoleCategory::Base,
        "borders and separators",
        Some(DerivedStyle::fg(BORDER)),
    ),
    role(
        "border.focused",
        RoleCategory::Base,
        "focused borders and accent chrome",
        Some(DerivedStyle::fg(FOCUSED).with(&["bold"])),
    ),
    role(
        "border.focused.panel",
        RoleCategory::Base,
        "border of the focused panel or dialog",
        None,
    ),
    role(
        "surface.raised",
        RoleCategory::Base,
        "pickers, palettes, the composer",
        Some(DerivedStyle::bg(
            "or($surface, mix($text, $background, 0.05))",
        )),
    ),
    role(
        "activity.spinner",
        RoleCategory::Base,
        "the activity spinner; unset uses state.info",
        None,
    ),
    role(
        "activity.text",
        RoleCategory::Base,
        "the activity verb or label; unset uses state.info",
        None,
    ),
    role(
        "activity.shimmer",
        RoleCategory::Base,
        "the highlight sweeping across activity text; unset uses bold activity text",
        None,
    ),
    role(
        "composer.border",
        RoleCategory::Base,
        "the composer's border; unset uses the focused accent",
        None,
    ),
    role(
        "composer.surface",
        RoleCategory::Base,
        "the composer's fill; unset uses surface.raised",
        None,
    ),
    role(
        "surface.overlay",
        RoleCategory::Base,
        "opaque modal and dialog panels",
        Some(DerivedStyle::bg(
            "or($surface_overlay, $surface, mix($text, $background, 0.09))",
        )),
    ),
    role(
        "surface.scrim",
        RoleCategory::Base,
        "optional full-frame modal scrim",
        None,
    ),
    role(
        "title",
        RoleCategory::Base,
        "panel, dialog, and column titles",
        Some(DerivedStyle::fg(ACCENT).with(&["bold"])),
    ),
    role(
        "accent",
        RoleCategory::Base,
        "current/active items",
        Some(DerivedStyle::fg(ACCENT)),
    ),
    role(
        "link",
        RoleCategory::Base,
        "hyperlinks",
        Some(DerivedStyle::fg(LINK).with(&["underline"])),
    ),
    // Interaction
    role(
        "control.focused",
        RoleCategory::Interaction,
        "focused inputs and controls",
        Some(DerivedStyle::fg(ACCENT).with(&["bold"])),
    ),
    role(
        "control.hover",
        RoleCategory::Interaction,
        "hovered controls",
        Some(DerivedStyle::fg(ACCENT)),
    ),
    role(
        "selection.active",
        RoleCategory::Interaction,
        "selection in the focused control",
        Some(
            DerivedStyle::fg(TEXT)
                .and_bg("or($selection, mix($accent, $background, 0.3))")
                .with(&["bold"]),
        ),
    ),
    role(
        "selection.inactive",
        RoleCategory::Interaction,
        "selection in an unfocused control",
        Some(DerivedStyle::bg(
            "or($line_highlight, mix($text, $background, 0.12))",
        )),
    ),
    // State
    role(
        "state.info",
        RoleCategory::State,
        "informational state",
        Some(DerivedStyle::fg(INFO)),
    ),
    role(
        "state.success",
        RoleCategory::State,
        "success state",
        Some(DerivedStyle::fg(SUCCESS)),
    ),
    role(
        "state.warning",
        RoleCategory::State,
        "warning state",
        Some(DerivedStyle::fg(WARNING)),
    ),
    role(
        "state.error",
        RoleCategory::State,
        "error state",
        Some(DerivedStyle::fg(ERROR)),
    ),
    role(
        "state.info.surface",
        RoleCategory::State,
        "informational badge/toast background",
        Some(DerivedStyle::bg(
            "mix(or($info, $blue, $accent), $background, 0.16)",
        )),
    ),
    role(
        "state.success.surface",
        RoleCategory::State,
        "success badge/toast background",
        Some(DerivedStyle::bg(
            "mix(or($success, $green, $accent), $background, 0.16)",
        )),
    ),
    role(
        "state.warning.surface",
        RoleCategory::State,
        "warning badge/toast background",
        Some(DerivedStyle::bg(
            "mix(or($warning, $yellow, $accent), $background, 0.16)",
        )),
    ),
    role(
        "state.error.surface",
        RoleCategory::State,
        "error badge/toast background",
        Some(DerivedStyle::bg(
            "mix(or($error, $red, $accent), $background, 0.16)",
        )),
    ),
    // Transcript
    role(
        "transcript.user.label",
        RoleCategory::Transcript,
        "user speaker label",
        Some(DerivedStyle::fg(ACCENT).with(&["bold"])),
    ),
    role(
        "transcript.assistant.label",
        RoleCategory::Transcript,
        "assistant speaker label",
        Some(DerivedStyle::fg(VIOLET).with(&["bold"])),
    ),
    role(
        "transcript.reasoning.label",
        RoleCategory::Transcript,
        "reasoning label",
        Some(DerivedStyle::fg(MUTED).with(&["italic"])),
    ),
    role(
        "transcript.system.label",
        RoleCategory::Transcript,
        "system label",
        Some(DerivedStyle::fg(MUTED)),
    ),
    role(
        "transcript.meta",
        RoleCategory::Transcript,
        "timestamps and metadata",
        Some(DerivedStyle::fg(MUTED)),
    ),
    role(
        "transcript.skill.label",
        RoleCategory::Transcript,
        "skill label",
        Some(DerivedStyle::fg(CYAN)),
    ),
    role(
        "transcript.skill_error.label",
        RoleCategory::Transcript,
        "failed skill label",
        Some(DerivedStyle::fg(ERROR)),
    ),
    role(
        "transcript.pending.label",
        RoleCategory::Transcript,
        "pending submission label",
        Some(DerivedStyle::fg(ACCENT)),
    ),
    role(
        "transcript.stream_status.label",
        RoleCategory::Transcript,
        "stream status label",
        Some(DerivedStyle::fg(WARNING)),
    ),
    role(
        "transcript.detail.label",
        RoleCategory::Transcript,
        "detail label",
        Some(DerivedStyle::fg(MUTED)),
    ),
    role(
        "transcript.detail.body",
        RoleCategory::Transcript,
        "detail body",
        Some(DerivedStyle::fg(MUTED)),
    ),
    role(
        "transcript.user.container",
        RoleCategory::Transcript,
        "user message container",
        None,
    ),
    role(
        "transcript.assistant.container",
        RoleCategory::Transcript,
        "assistant message container",
        None,
    ),
    // Tool
    role(
        "tool.requested.title",
        RoleCategory::Tool,
        "requested tool title",
        Some(DerivedStyle::fg(MUTED)),
    ),
    role(
        "tool.running.title",
        RoleCategory::Tool,
        "running tool title",
        Some(DerivedStyle::fg(INFO).with(&["bold"])),
    ),
    role(
        "tool.waiting.title",
        RoleCategory::Tool,
        "waiting tool title",
        Some(DerivedStyle::fg(WARNING).with(&["bold"])),
    ),
    role(
        "tool.succeeded.title",
        RoleCategory::Tool,
        "succeeded tool title",
        Some(DerivedStyle::fg(SUCCESS)),
    ),
    role(
        "tool.failed.title",
        RoleCategory::Tool,
        "failed tool title",
        Some(DerivedStyle::fg(ERROR).with(&["bold"])),
    ),
    role(
        "tool.cancelled.title",
        RoleCategory::Tool,
        "cancelled tool title",
        Some(DerivedStyle::fg(MUTED).with(&["crossed_out"])),
    ),
    role(
        "tool.timed_out.title",
        RoleCategory::Tool,
        "timed-out tool title",
        Some(DerivedStyle::fg(WARNING)),
    ),
    role(
        "tool.requested.container",
        RoleCategory::Tool,
        "requested tool container",
        None,
    ),
    role(
        "tool.running.container",
        RoleCategory::Tool,
        "running tool container",
        None,
    ),
    role(
        "tool.waiting.container",
        RoleCategory::Tool,
        "waiting tool container",
        None,
    ),
    role(
        "tool.succeeded.container",
        RoleCategory::Tool,
        "succeeded tool container",
        None,
    ),
    role(
        "tool.failed.container",
        RoleCategory::Tool,
        "failed tool container",
        None,
    ),
    role(
        "tool.cancelled.container",
        RoleCategory::Tool,
        "cancelled tool container",
        None,
    ),
    role(
        "tool.timed_out.container",
        RoleCategory::Tool,
        "timed-out tool container",
        None,
    ),
    role(
        "tool.count",
        RoleCategory::Tool,
        "counts in header templates; unset uses tool.metadata in bold",
        None,
    ),
    role(
        "tool.metadata",
        RoleCategory::Tool,
        "tool timing and metadata",
        Some(DerivedStyle::fg(MUTED)),
    ),
    role(
        "tool.label",
        RoleCategory::Tool,
        "tool field labels",
        Some(DerivedStyle::fg(MUTED).with(&["bold"])),
    ),
    role(
        "tool.argument",
        RoleCategory::Tool,
        "tool argument values",
        Some(DerivedStyle::fg(TEXT)),
    ),
    role(
        "tool.output",
        RoleCategory::Tool,
        "tool output",
        Some(DerivedStyle::fg(TEXT)),
    ),
    role(
        "tool.truncation",
        RoleCategory::Tool,
        "tool truncation notices",
        Some(DerivedStyle::fg(MUTED).with(&["italic"])),
    ),
    // Markdown
    role(
        "markdown.text",
        RoleCategory::Markdown,
        "body text",
        Some(DerivedStyle::fg(TEXT)),
    ),
    role(
        "markdown.heading",
        RoleCategory::Markdown,
        "headings",
        Some(DerivedStyle::fg(HEADING).with(&["bold"])),
    ),
    role(
        "markdown.link",
        RoleCategory::Markdown,
        "links",
        Some(DerivedStyle::fg(LINK).with(&["underline"])),
    ),
    role(
        "markdown.strong",
        RoleCategory::Markdown,
        "strong emphasis",
        Some(DerivedStyle::fg(TEXT).with(&["bold"])),
    ),
    role(
        "markdown.emphasis",
        RoleCategory::Markdown,
        "emphasis",
        Some(DerivedStyle::fg(TEXT).with(&["italic"])),
    ),
    role(
        "markdown.strikethrough",
        RoleCategory::Markdown,
        "strikethrough",
        Some(DerivedStyle::fg(MUTED).with(&["crossed_out"])),
    ),
    role(
        "markdown.inline_code",
        RoleCategory::Markdown,
        "inline code",
        Some(
            DerivedStyle::fg(INLINE_CODE)
                .and_bg("or($markup_code_bg, mix($text, $background, 0.07))"),
        ),
    ),
    role(
        "markdown.code_block_text",
        RoleCategory::Markdown,
        "fenced code text",
        Some(DerivedStyle::fg(TEXT)),
    ),
    role(
        "markdown.code_block_border",
        RoleCategory::Markdown,
        "fenced code border",
        Some(DerivedStyle::fg(RULE)),
    ),
    role(
        "markdown.blockquote_bar",
        RoleCategory::Markdown,
        "blockquote bar",
        Some(DerivedStyle::fg(RULE)),
    ),
    role(
        "markdown.alert_note",
        RoleCategory::Markdown,
        "note alerts",
        Some(DerivedStyle::fg(BLUE)),
    ),
    role(
        "markdown.alert_tip",
        RoleCategory::Markdown,
        "tip alerts",
        Some(DerivedStyle::fg(SUCCESS)),
    ),
    role(
        "markdown.alert_important",
        RoleCategory::Markdown,
        "important alerts",
        Some(DerivedStyle::fg(VIOLET)),
    ),
    role(
        "markdown.alert_warning",
        RoleCategory::Markdown,
        "warning alerts",
        Some(DerivedStyle::fg(WARNING)),
    ),
    role(
        "markdown.alert_caution",
        RoleCategory::Markdown,
        "caution alerts",
        Some(DerivedStyle::fg(ERROR)),
    ),
    role(
        "markdown.list_marker",
        RoleCategory::Markdown,
        "list markers",
        Some(DerivedStyle::fg(LIST_MARKER)),
    ),
    role(
        "markdown.task_checked",
        RoleCategory::Markdown,
        "checked tasks",
        Some(DerivedStyle::fg(SUCCESS)),
    ),
    role(
        "markdown.task_unchecked",
        RoleCategory::Markdown,
        "unchecked tasks",
        Some(DerivedStyle::fg(MUTED)),
    ),
    role(
        "markdown.table_border",
        RoleCategory::Markdown,
        "table borders",
        Some(DerivedStyle::fg(RULE)),
    ),
    role(
        "markdown.horizontal_rule",
        RoleCategory::Markdown,
        "horizontal rules",
        Some(DerivedStyle::fg(RULE)),
    ),
    role(
        "markdown.keyboard",
        RoleCategory::Markdown,
        "keyboard keys (HTML <kbd>)",
        Some(DerivedStyle {
            fg: Some(TEXT),
            bg: Some(RULE),
            modifiers: &[],
        }),
    ),
    role(
        "markdown.highlight",
        RoleCategory::Markdown,
        "highlighted text (HTML <mark>)",
        Some(DerivedStyle {
            fg: Some(TEXT),
            bg: Some("mix(or($warning, $yellow, $accent), $background, 0.4)"),
            modifiers: &[],
        }),
    ),
    role(
        "markdown.code.text",
        RoleCategory::Markdown,
        "plain code in Markdown code blocks; unset uses syntax.text",
        None,
    ),
    role(
        "markdown.code.comment",
        RoleCategory::Markdown,
        "comments in Markdown code blocks; unset uses syntax.comment",
        None,
    ),
    role(
        "markdown.code.keyword",
        RoleCategory::Markdown,
        "keywords in Markdown code blocks; unset uses syntax.keyword",
        None,
    ),
    role(
        "markdown.code.function",
        RoleCategory::Markdown,
        "functions in Markdown code blocks; unset uses syntax.function",
        None,
    ),
    role(
        "markdown.code.variable",
        RoleCategory::Markdown,
        "variables in Markdown code blocks; unset uses syntax.variable",
        None,
    ),
    role(
        "markdown.code.string",
        RoleCategory::Markdown,
        "strings in Markdown code blocks; unset uses syntax.string",
        None,
    ),
    role(
        "markdown.code.number",
        RoleCategory::Markdown,
        "numbers in Markdown code blocks; unset uses syntax.number",
        None,
    ),
    role(
        "markdown.code.type",
        RoleCategory::Markdown,
        "types in Markdown code blocks; unset uses syntax.type",
        None,
    ),
    role(
        "markdown.code.operator",
        RoleCategory::Markdown,
        "operators in Markdown code blocks; unset uses syntax.operator",
        None,
    ),
    role(
        "markdown.code.punctuation",
        RoleCategory::Markdown,
        "punctuation in Markdown code blocks; unset uses syntax.punctuation",
        None,
    ),
    role(
        "markdown.code.heading",
        RoleCategory::Markdown,
        "headings in code in Markdown code blocks; unset uses syntax.heading",
        None,
    ),
    role(
        "markdown.code.link",
        RoleCategory::Markdown,
        "links in code in Markdown code blocks; unset uses syntax.link",
        None,
    ),
    role(
        "markdown.code.raw",
        RoleCategory::Markdown,
        "raw text in code in Markdown code blocks; unset uses syntax.raw",
        None,
    ),
    // Syntax
    role(
        "syntax.text",
        RoleCategory::Syntax,
        "plain code",
        Some(DerivedStyle::fg(TEXT)),
    ),
    role(
        "syntax.comment",
        RoleCategory::Syntax,
        "comments",
        Some(DerivedStyle::fg(COMMENT).with(&["italic"])),
    ),
    role(
        "syntax.keyword",
        RoleCategory::Syntax,
        "keywords",
        Some(DerivedStyle::fg(KEYWORD)),
    ),
    role(
        "syntax.function",
        RoleCategory::Syntax,
        "functions",
        Some(DerivedStyle::fg(FUNCTION)),
    ),
    role(
        "syntax.variable",
        RoleCategory::Syntax,
        "variables",
        Some(DerivedStyle::fg(VARIABLE)),
    ),
    role(
        "syntax.string",
        RoleCategory::Syntax,
        "strings",
        Some(DerivedStyle::fg(STRING)),
    ),
    role(
        "syntax.number",
        RoleCategory::Syntax,
        "numbers",
        Some(DerivedStyle::fg(NUMBER)),
    ),
    role(
        "syntax.type",
        RoleCategory::Syntax,
        "types",
        Some(DerivedStyle::fg(TYPE)),
    ),
    role(
        "syntax.operator",
        RoleCategory::Syntax,
        "operators",
        Some(DerivedStyle::fg(OPERATOR)),
    ),
    role(
        "syntax.punctuation",
        RoleCategory::Syntax,
        "punctuation",
        Some(DerivedStyle::fg(PUNCTUATION)),
    ),
    role(
        "syntax.heading",
        RoleCategory::Syntax,
        "markup headings",
        Some(DerivedStyle::fg(HEADING).with(&["bold"])),
    ),
    role(
        "syntax.link",
        RoleCategory::Syntax,
        "markup links",
        Some(DerivedStyle::fg(LINK)),
    ),
    role(
        "syntax.raw",
        RoleCategory::Syntax,
        "markup literals",
        Some(DerivedStyle::fg(RAW)),
    ),
    // Source and diff
    role(
        "source.text",
        RoleCategory::Source,
        "source card text",
        Some(DerivedStyle::fg(TEXT)),
    ),
    role(
        "source.border",
        RoleCategory::Source,
        "source card border",
        Some(DerivedStyle::fg(BORDER)),
    ),
    role(
        "source.gutter",
        RoleCategory::Source,
        "line-number gutter",
        Some(DerivedStyle::fg(GUTTER)),
    ),
    role(
        "source.truncated",
        RoleCategory::Source,
        "truncation notice",
        Some(DerivedStyle::fg(WARNING)),
    ),
    role(
        "source.file.header",
        RoleCategory::Source,
        "a review file's header row; unset mixes the accent into the canvas",
        None,
    ),
    role(
        "source.file.desk",
        RoleCategory::Source,
        "the space between a review's files; unset steps the canvas by contrast",
        None,
    ),
    role(
        "source.file.edge",
        RoleCategory::Source,
        "a review file's card edge and rule (foreground); unset is the page over the desk",
        None,
    ),
    role(
        "source.file.label",
        RoleCategory::Source,
        "the label on the rule between a review's files; unset uses text.muted",
        None,
    ),
    role(
        "source.thread.background",
        RoleCategory::Source,
        "a review thread card's fill (background); unset steps the canvas toward the text",
        None,
    ),
    role(
        "source.thread.border",
        RoleCategory::Source,
        "a review thread card's frame once viewed; unset uses border.default",
        None,
    ),
    role(
        "source.thread.unviewed",
        RoleCategory::Source,
        "a review thread card's frame while unviewed; unset uses the accent",
        None,
    ),
    role(
        "source.thread.waiting",
        RoleCategory::Source,
        "a review thread card's frame when it waits on you; unset uses state.warning",
        None,
    ),
    role(
        "source.thread.resolved",
        RoleCategory::Source,
        "a resolved review thread card's frame; unset uses state.success, dimmed",
        None,
    ),
    role(
        "diff.text",
        RoleCategory::Source,
        "diff text",
        Some(DerivedStyle::fg(TEXT)),
    ),
    role(
        "diff.muted",
        RoleCategory::Source,
        "diff context",
        Some(DerivedStyle::fg(MUTED)),
    ),
    role(
        "diff.title",
        RoleCategory::Source,
        "diff title",
        Some(DerivedStyle::fg(ACCENT).with(&["bold"])),
    ),
    role(
        "diff.label",
        RoleCategory::Source,
        "diff labels",
        Some(DerivedStyle::fg(TEXT).with(&["bold"])),
    ),
    role(
        "diff.added",
        RoleCategory::Source,
        "added lines",
        Some(DerivedStyle::fg(DIFF_ADDED)),
    ),
    role(
        "diff.removed",
        RoleCategory::Source,
        "removed lines",
        Some(DerivedStyle::fg(DIFF_REMOVED)),
    ),
    role(
        "diff.hunk",
        RoleCategory::Source,
        "hunk headers",
        Some(DerivedStyle::fg(INFO)),
    ),
    role(
        "diff.added_row",
        RoleCategory::Source,
        "added row background",
        Some(DerivedStyle::bg(
            "or($diff_add_bg, mix(or($success, $green), $background, 0.14))",
        )),
    ),
    role(
        "diff.removed_row",
        RoleCategory::Source,
        "removed row background",
        Some(DerivedStyle::bg(
            "or($diff_remove_bg, mix(or($error, $red), $background, 0.14))",
        )),
    ),
    role(
        "diff.added_emphasis",
        RoleCategory::Source,
        "intraline added emphasis",
        Some(
            DerivedStyle::bg(
                "or($diff_add_emphasis_bg, mix(or($success, $green), $background, 0.32))",
            )
            .with(&["bold"]),
        ),
    ),
    role(
        "diff.removed_emphasis",
        RoleCategory::Source,
        "intraline removed emphasis",
        Some(
            DerivedStyle::bg(
                "or($diff_remove_emphasis_bg, mix(or($error, $red), $background, 0.32))",
            )
            .with(&["bold"]),
        ),
    ),
];

/// Role namespaces owned by domains outside this registry (for example the
/// git client's `git.*` roles). Keys under these namespaces are not reported
/// as unknown.
pub const DOMAIN_ROLE_NAMESPACES: &[&str] = &["git"];

/// Look up a registered role.
#[must_use]
pub fn role_spec(key: &str) -> Option<&'static RoleSpec> {
    ROLES.iter().find(|spec| spec.key == key)
}

/// Whether `key` is a registered role or lies in a known domain namespace.
///
/// `extra_namespaces` lets hosts register plugin-owned namespaces.
#[must_use]
pub fn is_known_role(key: &str, extra_namespaces: &[String]) -> bool {
    let namespace = key.split('.').next().unwrap_or(key);
    role_spec(key).is_some()
        || DOMAIN_ROLE_NAMESPACES.contains(&namespace)
        || extra_namespaces.iter().any(|extra| extra == namespace)
}

/// Closest registered role to an unknown key, for "did you mean" hints.
#[must_use]
pub fn suggestion(key: &str) -> Option<&'static str> {
    ROLES
        .iter()
        .map(|spec| (edit_distance(key, spec.key), spec.key))
        .filter(|(distance, _)| *distance <= 3)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, key)| key)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    for (i, left) in a.chars().enumerate() {
        let mut current = vec![i + 1];
        for (j, right) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(left != *right);
            current.push(substitution.min(previous[j + 1] + 1).min(current[j] + 1));
        }
        previous = current;
    }
    previous[b.len()]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn role_keys_are_unique() {
        let keys: BTreeSet<_> = ROLES.iter().map(|spec| spec.key).collect();
        assert_eq!(keys.len(), ROLES.len());
    }

    #[test]
    fn required_roles_are_registered_and_derivable() {
        for key in crate::definition::REQUIRED_SEMANTIC_STYLE_ROLES {
            let spec = role_spec(key).unwrap_or_else(|| panic!("{key} not registered"));
            assert!(spec.derive.is_some(), "{key} lacks derivation");
        }
    }

    #[test]
    fn derivation_expressions_parse() {
        for spec in ROLES {
            if let Some(derive) = spec.derive {
                for expr in [derive.fg, derive.bg].into_iter().flatten() {
                    crate::expr::parse(expr)
                        .unwrap_or_else(|error| panic!("{}: {error}", spec.key));
                }
            }
        }
    }

    #[test]
    fn unknown_roles_get_close_suggestions_and_namespaces_are_known() {
        assert!(is_known_role("border.focused", &[]));
        assert!(is_known_role("git.branch.head", &[]));
        assert!(!is_known_role("bordr.focussed", &[]));
        assert!(is_known_role("shell.exit", &["shell".to_owned()]));
        assert_eq!(suggestion("bordr.focussed"), Some("border.focused"));
        assert_eq!(suggestion("completely.different.thing"), None);
    }
}
