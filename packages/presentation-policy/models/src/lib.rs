#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Renderer-neutral structural presentation models.
//!
//! Structural presentation decides *what* a renderer shows — whether a unit is
//! expanded or summarized, how much detail and preview it gets, and how
//! consecutive units group — independently of how any renderer draws it.
//!
//! * [`UnitFacts`] describe one presentable unit using facts already present in
//!   renderer-neutral session views.
//! * [`PropertyPatch`] and [`ResolvedPresentation`] carry structural properties.
//! * [`PresentationRule`] pairs a [`FactMatcher`] with a patch. Ordered rule
//!   lists form [`RuleLayer`]s.
//!
//! These types are presentation inputs only. They must never influence
//! authorization, dispatch, persistence, or execution outcomes.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};

/// Maximum rules accepted in one layer.
pub const MAX_RULES_PER_LAYER: usize = 256;
/// Maximum plugin-declared facts on one unit, and fact conditions in one matcher.
pub const MAX_PLUGIN_FACTS: usize = 32;
/// Maximum facts, and maximum properties, one plugin may declare.
pub const MAX_PLUGIN_DECLARATIONS: usize = 32;
/// Maximum UTF-8 bytes of a plugin fact or property name.
pub const MAX_PLUGIN_KEY_BYTES: usize = 64;
/// Maximum UTF-8 bytes of a text fact value.
pub const MAX_PLUGIN_FACT_TEXT_BYTES: usize = 256;
/// Maximum values of an enum property.
pub const MAX_PLUGIN_ENUM_VALUES: usize = 16;
/// Maximum UTF-8 bytes of one enum property value.
pub const MAX_PLUGIN_ENUM_VALUE_BYTES: usize = 64;
/// Maximum part-visibility or plugin-property entries in one patch.
pub const MAX_PATCH_ENTRIES: usize = 64;
/// Largest accepted preview budget, in source lines.
pub const MAX_PREVIEW_LINES: u32 = 10_000;
/// Largest accepted summary template, in bytes.
pub const MAX_SUMMARY_TEMPLATE_BYTES: usize = 256;
/// Largest accepted blank-row spacing before or after a unit.
pub const MAX_SPACING: u8 = 2;
/// Largest accepted extra indent, in columns.
pub const MAX_INDENT: u8 = 8;
/// Smallest run of units that may form a group: a group of one presents a single unit through
/// its group's header.
pub const MIN_GROUP_SIZE: u32 = 1;
/// Smallest run that forms a group when a rule does not set `min`.
pub const DEFAULT_GROUP_SIZE: u32 = 2;
/// Longest display name, in bytes.
pub const MAX_DISPLAY_NAME_BYTES: usize = 64;

/// Registered unit and group header field names.
pub const HEADER_FIELDS: [&str; 13] = [
    "marker", "bullet", "icon", "label", "tool", "target", "status", "timing", "elapsed", "count",
    "plural", "title", "tools",
];
/// Header fields that apply to non-group units.
pub const UNIT_HEADER_FIELDS: [&str; 11] = [
    "marker", "bullet", "icon", "label", "tool", "target", "status", "timing", "elapsed", "count",
    "title",
];
/// Header fields that apply to group units.
pub const GROUP_HEADER_FIELDS: [&str; 9] = [
    "marker", "bullet", "count", "plural", "tool", "tools", "status", "timing", "elapsed",
];
/// Header used for non-group units when no rule sets `header`.
pub const DEFAULT_UNIT_HEADER: [&str; 5] = ["marker", "label", "tool", "status", "timing"];
/// Header used for group units when no rule sets `header`.
pub const DEFAULT_GROUP_HEADER: [&str; 5] = ["marker", "count", "tools", "status", "timing"];
/// Field names that summary templates may substitute.
pub const TEMPLATE_FIELDS: [&str; 9] = [
    "tool",
    "target",
    "status",
    "timing",
    "elapsed",
    "count",
    "plural",
    "title",
    "first_line",
];
/// Host section names.
pub const HOST_SECTIONS: [&str; 9] = [
    "arguments",
    "output",
    "result",
    "diff",
    "artifact",
    "metadata",
    "visual",
    "confirmation",
    "error",
];

/// Kind of presentable unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitKind {
    /// User-authored message.
    UserMessage,
    /// Assistant message.
    AssistantMessage,
    /// Assistant reasoning.
    Reasoning,
    /// Tool invocation.
    Tool,
    /// Permission request.
    Permission,
    /// Interactive request awaiting input.
    Interaction,
    /// Context compaction marker.
    Compaction,
    /// Skill activation.
    Skill,
    /// Activity or progress message.
    Activity,
    /// Plugin status message.
    PluginStatus,
    /// System or notice message.
    System,
    /// Group of consecutive units produced by grouping.
    Group,
}

impl UnitKind {
    /// Every unit kind, in declaration order.
    pub const ALL: [Self; 12] = [
        Self::UserMessage,
        Self::AssistantMessage,
        Self::Reasoning,
        Self::Tool,
        Self::Permission,
        Self::Interaction,
        Self::Compaction,
        Self::Skill,
        Self::Activity,
        Self::PluginStatus,
        Self::System,
        Self::Group,
    ];
}

/// Renderer-neutral lifecycle status of a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitStatus {
    /// Requested but not started.
    Requested,
    /// In progress.
    Running,
    /// Waiting on something external (input, permission, a dependency).
    Waiting,
    /// Finished successfully.
    Succeeded,
    /// Finished with a failure.
    Failed,
    /// Cancelled before finishing.
    Cancelled,
    /// Exceeded its time budget.
    TimedOut,
}

impl UnitStatus {
    /// Whether this status is terminal.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::TimedOut
        )
    }
}

/// Position of a unit relative to the latest turn.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TurnRecency {
    /// In the latest turn.
    #[default]
    Latest,
    /// In the turn immediately before the latest.
    Previous,
    /// In an earlier turn.
    Older,
}

/// Bucketed size, so streaming content changes facts only at boundaries.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SizeBucket {
    /// Zero.
    #[default]
    Empty,
    /// Exactly one.
    One,
    /// Two through ten.
    Few,
    /// Eleven through fifty.
    Some,
    /// Fifty-one through two hundred.
    Many,
    /// More than two hundred.
    Huge,
}

impl SizeBucket {
    /// Bucket an exact count.
    #[must_use]
    pub const fn of(count: usize) -> Self {
        match count {
            0 => Self::Empty,
            1 => Self::One,
            2..=10 => Self::Few,
            11..=50 => Self::Some,
            51..=200 => Self::Many,
            _ => Self::Huge,
        }
    }
}

/// Bucketed wall-clock duration, so running units change facts only at boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum DurationBucket {
    /// Under one second.
    #[serde(rename = "<1s", alias = "lt1s")]
    UnderOneSecond,
    /// One second up to (excluding) ten seconds.
    #[serde(rename = "1s..10s", alias = "s1_10")]
    OneToTenSeconds,
    /// Ten seconds up to (excluding) sixty seconds.
    #[serde(rename = "10s..60s", alias = "s10_60")]
    TenToSixtySeconds,
    /// Sixty seconds or more.
    #[serde(rename = ">60s", alias = "gt60")]
    OverSixtySeconds,
}

impl DurationBucket {
    /// Bucket an exact duration in milliseconds.
    #[must_use]
    pub const fn of_millis(millis: u64) -> Self {
        match millis {
            0..1_000 => Self::UnderOneSecond,
            1_000..10_000 => Self::OneToTenSeconds,
            10_000..60_000 => Self::TenToSixtySeconds,
            _ => Self::OverSixtySeconds,
        }
    }
}

/// Typed value of a plugin-declared fact or property.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FactValue {
    /// Boolean value.
    Bool(bool),
    /// Integer value.
    Int(i64),
    /// Text value.
    Text(String),
}

/// Type of a plugin-declared fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactType {
    /// Boolean.
    Bool,
    /// Signed integer.
    Int,
    /// One line of text, at most [`MAX_PLUGIN_FACT_TEXT_BYTES`].
    String,
}

impl FactType {
    /// Whether `value` has this type.
    #[must_use]
    pub const fn admits(self, value: &FactValue) -> bool {
        matches!(
            (self, value),
            (Self::Bool, FactValue::Bool(_))
                | (Self::Int, FactValue::Int(_))
                | (Self::String, FactValue::Text(_))
        )
    }
}

/// A fact a plugin declared for its own units.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PluginFactSchema {
    /// Declaring plugin id; the fact exists only on that plugin's units.
    pub plugin: String,
    /// Value type.
    #[serde(rename = "type")]
    pub value_type: FactType,
}

/// Value constraints of a plugin-declared property.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum PropertyType {
    /// Boolean.
    Bool,
    /// Integer within `min..=max`.
    Int {
        /// Smallest accepted value.
        min: i64,
        /// Largest accepted value.
        max: i64,
    },
    /// One of the listed strings.
    Enum {
        /// Accepted values, at most [`MAX_PLUGIN_ENUM_VALUES`].
        values: Vec<String>,
    },
}

impl PropertyType {
    /// Why `value` is not acceptable for this type, if it is not.
    #[must_use]
    pub fn reject(&self, value: &FactValue) -> Option<String> {
        match (self, value) {
            (Self::Bool, FactValue::Bool(_)) => None,
            (Self::Int { min, max }, FactValue::Int(value)) => {
                (!(min..=max).contains(&value)).then(|| format!("{value} is outside {min}..={max}"))
            }
            (Self::Enum { values }, FactValue::Text(value)) => {
                (!values.contains(value)).then(|| format!("{value:?} is not one of {values:?}"))
            }
            (Self::Bool, _) => Some("expected a bool".to_owned()),
            (Self::Int { .. }, _) => Some("expected an integer".to_owned()),
            (Self::Enum { .. }, _) => Some("expected a string".to_owned()),
        }
    }
}

/// A presentation property a plugin declared for its own units.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PluginPropertySchema {
    /// Declaring plugin id; the property applies only to that plugin's units.
    pub plugin: String,
    /// Value constraints.
    #[serde(flatten)]
    pub value_type: PropertyType,
    /// Value used when no rule sets the property; satisfies `value_type`.
    pub default: FactValue,
}

/// Facts and properties declared by plugins, by namespaced name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PluginPresentationSchemas {
    /// Declared facts.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub facts: BTreeMap<String, PluginFactSchema>,
    /// Declared properties.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, PluginPropertySchema>,
}

impl PluginPresentationSchemas {
    /// Whether nothing is declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.facts.is_empty() && self.properties.is_empty()
    }
}

/// Whether `name` is a valid fact or property name for plugin `plugin`.
///
/// Names are `<plugin id>.<key>`, at most [`MAX_PLUGIN_KEY_BYTES`] bytes, where
/// the key is one or more dot-separated non-empty segments of lowercase ASCII
/// letters, digits, `_`, and `-` (`bcode.git.files`).
#[must_use]
pub fn valid_plugin_key(name: &str, plugin: &str) -> bool {
    name.len() <= MAX_PLUGIN_KEY_BYTES
        && name
            .strip_prefix(plugin)
            .and_then(|rest| rest.strip_prefix('.'))
            .is_some_and(|key| {
                key.split('.').all(|segment| {
                    !segment.is_empty()
                        && segment.bytes().all(|byte| {
                            byte.is_ascii_lowercase()
                                || byte.is_ascii_digit()
                                || matches!(byte, b'_' | b'-')
                        })
                })
            })
}

/// Facts describing one presentable unit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitFacts {
    /// Unit kind. `None` only for detached test fixtures; matchers on kind fail.
    pub kind: Option<UnitKind>,
    /// Tool name, for tool units.
    pub tool: Option<String>,
    /// Producing plugin id.
    pub producer: Option<String>,
    /// Plugin presentation schema, as `name/version` or `name`.
    pub schema: Option<String>,
    /// Lifecycle status.
    pub status: Option<UnitStatus>,
    /// Whether the unit represents an error.
    pub is_error: bool,
    /// Whether the unit is waiting on a user decision.
    pub awaiting_decision: bool,
    /// Recency relative to the latest turn.
    pub turn: TurnRecency,
    /// Child-session nesting depth.
    pub depth: u16,
    /// Bucketed line count of the unit's primary content.
    pub lines: SizeBucket,
    /// Bucketed item count (for example, members of a group).
    pub items: SizeBucket,
    /// Operation target (path, command, pattern, query, or URL), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Bucketed duration, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<DurationBucket>,
    /// Whether the unit is the last unit of its turn, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_in_turn: Option<bool>,
    /// Plugin-declared facts keyed by namespaced name; only facts declared by
    /// the unit's producer.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugin: BTreeMap<String, FactValue>,
}

/// How much of a unit is shown.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Disclosure {
    /// Header and full body.
    #[default]
    Expanded,
    /// Header and a one-line summary.
    Summary,
    /// Header only.
    Collapsed,
    /// Not shown individually; represented by a count marker.
    Hidden,
}

/// Amount of secondary detail shown.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Detail {
    /// Essential content only.
    Minimal,
    /// Standard detail.
    #[default]
    Normal,
    /// All available detail.
    Verbose,
}

/// Which lines a preview keeps when content exceeds its budget.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum PreviewKeep {
    /// Keep the first lines.
    Head,
    /// Keep the last lines.
    #[default]
    Tail,
    /// Keep lines from both ends.
    Both,
}

/// Preview budget in source lines. Renderers may clamp further to fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preview {
    /// Maximum source lines shown.
    pub lines: u32,
    /// Which lines to keep.
    #[serde(default)]
    pub keep: PreviewKeep,
}

/// Key that decides which consecutive units share a group.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    /// No grouping.
    #[default]
    None,
    /// Consecutive tool units of any tool.
    Consecutive,
    /// Consecutive invocations of the same tool.
    SameTool,
    /// Consecutive units from the same producer.
    SameProducer,
    /// Every tool unit of a turn.
    Turn,
}

/// How consecutive units group.
///
/// Deserializes from a legacy string (`none`, `consecutive_tools`,
/// `consecutive_same_tool`) or a `{ by, bridge, min }` table; serializes as a
/// table. Omitted table fields take the defaults of [`GroupSpec::new`], and
/// `min` defaults to [`DEFAULT_GROUP_SIZE`] and is raised to [`MIN_GROUP_SIZE`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct GroupSpec {
    /// Grouping key.
    pub by: GroupBy,
    /// Unit kinds that may sit between members without ending the group.
    pub bridge: Vec<UnitKind>,
    /// Smallest run that forms a group; at least [`MIN_GROUP_SIZE`]. A run of one presents
    /// its unit through the group header.
    pub min: u32,
}

impl GroupSpec {
    /// Spec for `by` with default bridge (`reasoning` unless `by` is `none`)
    /// and minimum size.
    #[must_use]
    pub fn new(by: GroupBy) -> Self {
        Self {
            by,
            bridge: Self::default_bridge(by),
            min: DEFAULT_GROUP_SIZE,
        }
    }

    /// No grouping.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            by: GroupBy::None,
            bridge: Vec::new(),
            min: DEFAULT_GROUP_SIZE,
        }
    }

    /// Whether this spec groups at all.
    #[must_use]
    pub fn is_grouping(&self) -> bool {
        self.by != GroupBy::None
    }

    fn default_bridge(by: GroupBy) -> Vec<UnitKind> {
        if by == GroupBy::None {
            Vec::new()
        } else {
            vec![UnitKind::Reasoning]
        }
    }
}

impl Default for GroupSpec {
    fn default() -> Self {
        Self::none()
    }
}

impl From<GroupBy> for GroupSpec {
    fn from(by: GroupBy) -> Self {
        Self::new(by)
    }
}

impl<'de> Deserialize<'de> for GroupSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Legacy {
            None,
            ConsecutiveTools,
            ConsecutiveSameTool,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Table {
            #[serde(default)]
            by: GroupBy,
            #[serde(default)]
            bridge: Option<Vec<UnitKind>>,
            #[serde(default)]
            min: Option<u32>,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Shape {
            Legacy(Legacy),
            Table(Table),
        }
        let shape = Shape::deserialize(deserializer).map_err(|_| {
            serde::de::Error::custom(
                "expected none, consecutive_tools, consecutive_same_tool, or { by, bridge, min }",
            )
        })?;
        Ok(match shape {
            Shape::Legacy(Legacy::None) => Self::none(),
            Shape::Legacy(Legacy::ConsecutiveTools) => Self::new(GroupBy::Consecutive),
            Shape::Legacy(Legacy::ConsecutiveSameTool) => Self::new(GroupBy::SameTool),
            Shape::Table(table) => Self {
                bridge: table
                    .bridge
                    .unwrap_or_else(|| Self::default_bridge(table.by)),
                min: table.min.unwrap_or(DEFAULT_GROUP_SIZE).max(MIN_GROUP_SIZE),
                by: table.by,
            },
        })
    }
}

/// Visibility, disclosure, and preview of one named unit section.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SectionPatch {
    /// Whether the section is shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible: Option<bool>,
    /// Section disclosure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disclosure: Option<Disclosure>,
    /// Section preview budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<Preview>,
}

impl SectionPatch {
    /// Overlay `other` on this patch per field; set values in `other` win.
    pub const fn merge(&mut self, other: &Self) {
        if other.visible.is_some() {
            self.visible = other.visible;
        }
        if other.disclosure.is_some() {
            self.disclosure = other.disclosure;
        }
        if other.preview.is_some() {
            self.preview = other.preview;
        }
    }
}

/// Resolved presentation of one named unit section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResolvedSection {
    /// Whether the section is shown.
    pub visible: bool,
    /// Section disclosure.
    pub disclosure: Disclosure,
    /// Section preview budget.
    pub preview: Preview,
}

impl ResolvedSection {
    /// Default section presentation for a unit whose preview is `preview`.
    #[must_use]
    pub const fn defaults(preview: Preview) -> Self {
        Self {
            visible: true,
            disclosure: Disclosure::Expanded,
            preview,
        }
    }

    /// Apply `patch` to this section.
    pub const fn apply(&mut self, patch: &SectionPatch) {
        if let Some(visible) = patch.visible {
            self.visible = visible;
        }
        if let Some(disclosure) = patch.disclosure {
            self.disclosure = disclosure;
        }
        if let Some(preview) = patch.preview {
            self.preview = preview;
        }
    }
}

/// A unit's header: ordered fields, or a template.
///
/// Field names come from [`HEADER_FIELDS`]. A `template` (at most
/// [`MAX_SUMMARY_TEMPLATE_BYTES`]) substitutes the same names in braces and replaces `fields`;
/// renderers style each substituted field by its role and literal text as metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeaderSpec {
    /// Field names, in display order.
    #[serde(default)]
    pub fields: Vec<String>,
    /// Header template; replaces `fields` when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
}

/// How a unit's body sections are introduced.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SectionLabels {
    /// Each section has a label row (`▾ output`).
    #[default]
    Rows,
    /// No label rows: a connector glyph precedes the first body row and the body hangs beside
    /// it.
    Connector,
}

/// Source of a unit's one-line summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummarySource {
    /// Producer-supplied summary.
    Plugin,
    /// First non-blank line of the first visible body section.
    FirstLine,
    /// The summary template.
    Template,
}

/// How a unit's one-line summary is produced.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummarySpec {
    /// Sources tried in order.
    #[serde(default = "SummarySpec::default_source")]
    pub source: Vec<SummarySource>,
    /// Template substituting [`TEMPLATE_FIELDS`] in braces; at most
    /// [`MAX_SUMMARY_TEMPLATE_BYTES`] after resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
}

impl SummarySpec {
    /// Default source order: plugin, first line, then template.
    fn default_source() -> Vec<SummarySource> {
        vec![
            SummarySource::Plugin,
            SummarySource::FirstLine,
            SummarySource::Template,
        ]
    }
}

impl Default for SummarySpec {
    fn default() -> Self {
        Self {
            source: Self::default_source(),
            template: None,
        }
    }
}

/// Blank rows around a unit; each side is clamped to [`MAX_SPACING`] on resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Spacing {
    /// Blank rows before the unit.
    pub before: u8,
    /// Blank rows after the unit.
    pub after: u8,
}

impl Spacing {
    /// Copy with both sides clamped to [`MAX_SPACING`].
    #[must_use]
    pub fn clamped(self) -> Self {
        Self {
            before: self.before.min(MAX_SPACING),
            after: self.after.min(MAX_SPACING),
        }
    }
}

impl Default for Spacing {
    fn default() -> Self {
        Self {
            before: 0,
            after: 1,
        }
    }
}

/// Group member statuses revealed by default when a group is not expanded.
pub const DEFAULT_REVEAL: [UnitStatus; 4] = [
    UnitStatus::Running,
    UnitStatus::Waiting,
    UnitStatus::Failed,
    UnitStatus::TimedOut,
];

/// Where a unit is placed.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    /// In the transcript flow.
    #[default]
    Inline,
    /// Docked outside the transcript flow.
    Docked,
}

/// Sparse set of structural properties applied by a rule.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PropertyPatch {
    /// Disclosure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disclosure: Option<Disclosure>,
    /// Detail level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Detail>,
    /// Preview budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<Preview>,
    /// Grouping of consecutive units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<GroupSpec>,
    /// Group member statuses revealed when a group is not expanded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reveal: Option<Vec<UnitStatus>>,
    /// Placement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<Placement>,
    /// Chrome container recipe id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// Legacy part visibility keyed by part name; an alias for
    /// `sections.<name>.visible`. [`merge`](Self::merge) folds overlaid parts
    /// into [`sections`](Self::sections), and an explicit section value in the
    /// same patch wins over the alias.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parts: BTreeMap<String, bool>,
    /// Section presentation keyed by section name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sections: BTreeMap<String, SectionPatch>,
    /// Header fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<HeaderSpec>,
    /// Summary sources and template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<SummarySpec>,
    /// Blank rows around the unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spacing: Option<Spacing>,
    /// Extra indent, in columns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indent: Option<u8>,
    /// Name presented for the unit's tool (`{tool}`); at most [`MAX_DISPLAY_NAME_BYTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// How body sections are introduced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section_labels: Option<SectionLabels>,
    /// Whether a hidden unit leaves a count marker row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden_marker: Option<bool>,
    /// Plugin-namespaced properties (keys contain a `.`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugin: BTreeMap<String, FactValue>,
}

impl PropertyPatch {
    /// Overlay `other` on this patch; set values in `other` win.
    pub fn merge(&mut self, other: &Self) {
        if other.disclosure.is_some() {
            self.disclosure = other.disclosure;
        }
        if other.detail.is_some() {
            self.detail = other.detail;
        }
        if other.preview.is_some() {
            self.preview = other.preview;
        }
        if other.group.is_some() {
            self.group.clone_from(&other.group);
        }
        if other.reveal.is_some() {
            self.reveal.clone_from(&other.reveal);
        }
        if other.placement.is_some() {
            self.placement = other.placement;
        }
        if other.container.is_some() {
            self.container.clone_from(&other.container);
        }
        for (name, section) in other.section_patches() {
            self.sections
                .entry(name.to_owned())
                .or_default()
                .merge(&section);
        }
        if other.header.is_some() {
            self.header.clone_from(&other.header);
        }
        if other.summary.is_some() {
            self.summary.clone_from(&other.summary);
        }
        if other.spacing.is_some() {
            self.spacing = other.spacing;
        }
        if other.indent.is_some() {
            self.indent = other.indent;
        }
        if other.display_name.is_some() {
            self.display_name.clone_from(&other.display_name);
        }
        if other.section_labels.is_some() {
            self.section_labels = other.section_labels;
        }
        if other.hidden_marker.is_some() {
            self.hidden_marker = other.hidden_marker;
        }
        self.plugin.extend(
            other
                .plugin
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
    }

    /// Whether the patch sets nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Effective section patches, with legacy `parts` folded into
    /// `visible` and explicit `sections` values taking precedence.
    #[must_use]
    pub fn section_patches(&self) -> BTreeMap<&str, SectionPatch> {
        let mut sections: BTreeMap<&str, SectionPatch> = self
            .parts
            .iter()
            .map(|(name, visible)| {
                (
                    name.as_str(),
                    SectionPatch {
                        visible: Some(*visible),
                        ..SectionPatch::default()
                    },
                )
            })
            .collect();
        for (name, section) in &self.sections {
            sections.entry(name.as_str()).or_default().merge(section);
        }
        sections
    }
}

/// Fully resolved structural presentation for one unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedPresentation {
    /// Disclosure.
    pub disclosure: Disclosure,
    /// Detail level.
    pub detail: Detail,
    /// Preview budget.
    pub preview: Preview,
    /// Grouping of consecutive units.
    pub group: GroupSpec,
    /// Group member statuses revealed when a group is not expanded.
    pub reveal: Vec<UnitStatus>,
    /// Placement.
    pub placement: Placement,
    /// Chrome container recipe id, when one was selected.
    pub container: Option<String>,
    /// Explicitly set sections; absent sections take
    /// [`ResolvedSection::defaults`] (see [`section`](Self::section)).
    pub sections: BTreeMap<String, ResolvedSection>,
    /// Header fields, or `None` for the renderer's default header.
    pub header: Option<HeaderSpec>,
    /// Summary sources and template.
    pub summary: SummarySpec,
    /// Blank rows around the unit, each at most [`MAX_SPACING`].
    pub spacing: Spacing,
    /// Extra indent in columns, at most [`MAX_INDENT`].
    pub indent: u8,
    /// Name presented for the unit's tool, when a rule set one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// How body sections are introduced.
    #[serde(default)]
    pub section_labels: SectionLabels,
    /// Whether a hidden unit leaves a count marker row.
    #[serde(default = "default_hidden_marker")]
    pub hidden_marker: bool,
    /// Plugin-namespaced properties.
    pub plugin: BTreeMap<String, FactValue>,
}

const fn default_hidden_marker() -> bool {
    true
}

impl ResolvedPresentation {
    /// Presentation of section `name`, defaulting to visible, expanded, and
    /// the unit's preview.
    #[must_use]
    pub fn section(&self, name: &str) -> ResolvedSection {
        self.sections
            .get(name)
            .copied()
            .unwrap_or_else(|| ResolvedSection::defaults(self.preview))
    }

    /// Whether `part` (a section name) should be shown.
    #[must_use]
    pub fn part_visible(&self, part: &str) -> bool {
        self.section(part).visible
    }
}

/// Values that deserialize from either one value or a list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct OneOrMany<T>(pub Vec<T>);

impl<T> Default for OneOrMany<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for OneOrMany<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Shape<T> {
            One(T),
            Many(Vec<T>),
        }
        Ok(Self(match Shape::deserialize(deserializer)? {
            Shape::One(value) => vec![value],
            Shape::Many(values) => values,
        }))
    }
}

impl<T> OneOrMany<T> {
    /// Whether no values were given (the condition is absent).
    #[must_use]
    pub const fn is_unset(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether the condition is absent or any value satisfies `predicate`.
    pub fn admits(&self, predicate: impl FnMut(&T) -> bool) -> bool {
        self.is_unset() || self.0.iter().any(predicate)
    }
}

/// Bounded integer comparison on a plugin fact; every present bound must hold.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactRange {
    /// Greater than.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gt: Option<i64>,
    /// Greater than or equal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gte: Option<i64>,
    /// Less than.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lt: Option<i64>,
    /// Less than or equal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lte: Option<i64>,
}

impl FactRange {
    /// Whether `value` satisfies every bound.
    #[must_use]
    pub fn contains(&self, value: i64) -> bool {
        self.gt.is_none_or(|bound| value > bound)
            && self.gte.is_none_or(|bound| value >= bound)
            && self.lt.is_none_or(|bound| value < bound)
            && self.lte.is_none_or(|bound| value <= bound)
    }
}

/// Condition on one plugin-declared fact.
///
/// Deserializes from a value or list of values (equality, or one of) or from
/// a `{ gt, gte, lt, lte }` table with at least one bound (an integer range).
/// Units without the fact never match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum FactCondition {
    /// Equal to one of the values; an empty list matches every unit.
    Values(OneOrMany<FactValue>),
    /// An integer within the range.
    Range(FactRange),
}

impl<'de> Deserialize<'de> for FactCondition {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Shape {
            Values(OneOrMany<FactValue>),
            Range(FactRange),
        }
        match Shape::deserialize(deserializer).map_err(|_| {
            serde::de::Error::custom(
                "expected a bool, integer, or string, a list of them, or { gt, gte, lt, lte }",
            )
        })? {
            Shape::Values(values) => Ok(Self::Values(values)),
            Shape::Range(range) if range == FactRange::default() => Err(serde::de::Error::custom(
                "a fact range needs at least one of gt, gte, lt, lte",
            )),
            Shape::Range(range) => Ok(Self::Range(range)),
        }
    }
}

impl From<OneOrMany<FactValue>> for FactCondition {
    fn from(values: OneOrMany<FactValue>) -> Self {
        Self::Values(values)
    }
}

impl FactCondition {
    /// Whether a unit whose fact is `value` matches.
    #[must_use]
    pub fn matches(&self, value: Option<&FactValue>) -> bool {
        match self {
            Self::Values(values) => values.admits(|expected| value == Some(expected)),
            Self::Range(range) => {
                matches!(value, Some(FactValue::Int(value)) if range.contains(*value))
            }
        }
    }
}

/// Conjunction of fact conditions. Absent conditions always match; a list
/// matches when any listed value matches.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactMatcher {
    /// Unit kinds.
    #[serde(default, skip_serializing_if = "OneOrMany::is_unset")]
    pub kind: OneOrMany<UnitKind>,
    /// Tool names.
    #[serde(default, skip_serializing_if = "OneOrMany::is_unset")]
    pub tool: OneOrMany<String>,
    /// Producing plugin ids.
    #[serde(default, skip_serializing_if = "OneOrMany::is_unset")]
    pub producer: OneOrMany<String>,
    /// Presentation schemas; a trailing `/*` matches any version.
    #[serde(default, skip_serializing_if = "OneOrMany::is_unset")]
    pub schema: OneOrMany<String>,
    /// Lifecycle statuses.
    #[serde(default, skip_serializing_if = "OneOrMany::is_unset")]
    pub status: OneOrMany<UnitStatus>,
    /// Error flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    /// Turn recency.
    #[serde(default, skip_serializing_if = "OneOrMany::is_unset")]
    pub turn: OneOrMany<TurnRecency>,
    /// Minimum child-session depth (inclusive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_depth: Option<u16>,
    /// Line-count buckets.
    #[serde(default, skip_serializing_if = "OneOrMany::is_unset")]
    pub lines: OneOrMany<SizeBucket>,
    /// Item-count buckets.
    #[serde(default, skip_serializing_if = "OneOrMany::is_unset")]
    pub items: OneOrMany<SizeBucket>,
    /// Operation targets, matched exactly.
    #[serde(default, skip_serializing_if = "OneOrMany::is_unset")]
    pub target: OneOrMany<String>,
    /// Duration buckets.
    #[serde(default, skip_serializing_if = "OneOrMany::is_unset")]
    pub duration: OneOrMany<DurationBucket>,
    /// Last-in-turn flag; units without the fact never match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_in_turn: Option<bool>,
    /// Plugin-declared facts, by namespaced name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugin: BTreeMap<String, FactCondition>,
}

impl FactMatcher {
    /// Whether every present condition matches `facts`.
    #[must_use]
    pub fn matches(&self, facts: &UnitFacts) -> bool {
        let optional = |condition: &OneOrMany<String>, value: Option<&String>| {
            condition.admits(|expected| value.is_some_and(|value| value == expected))
        };
        self.kind
            .admits(|kind| facts.kind.is_some_and(|actual| actual == *kind))
            && optional(&self.tool, facts.tool.as_ref())
            && optional(&self.producer, facts.producer.as_ref())
            && self.schema.admits(|pattern| {
                facts
                    .schema
                    .as_deref()
                    .is_some_and(|schema| schema_matches(pattern, schema))
            })
            && self
                .status
                .admits(|status| facts.status.is_some_and(|actual| actual == *status))
            && self
                .is_error
                .is_none_or(|expected| expected == facts.is_error)
            && self.turn.admits(|turn| *turn == facts.turn)
            && self.min_depth.is_none_or(|depth| facts.depth >= depth)
            && self.lines.admits(|bucket| *bucket == facts.lines)
            && self.items.admits(|bucket| *bucket == facts.items)
            && optional(&self.target, facts.target.as_ref())
            && self
                .duration
                .admits(|bucket| facts.duration.is_some_and(|actual| actual == *bucket))
            && self
                .last_in_turn
                .is_none_or(|expected| facts.last_in_turn == Some(expected))
            && self
                .plugin
                .iter()
                .all(|(name, condition)| condition.matches(facts.plugin.get(name)))
    }
}

/// Match a schema pattern; `name/*` matches `name` and `name/<anything>`.
#[must_use]
pub fn schema_matches(pattern: &str, schema: &str) -> bool {
    pattern.strip_suffix("/*").map_or_else(
        || pattern == schema,
        |prefix| {
            schema == prefix
                || schema
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with('/'))
        },
    )
}

/// One ordered rule: when facts match, apply the patch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresentationRule {
    /// Conditions; an empty matcher matches every unit.
    #[serde(default, rename = "match")]
    pub matcher: FactMatcher,
    /// Properties set when the rule matches.
    #[serde(default)]
    pub set: PropertyPatch,
}

/// Source of a rule layer, lowest precedence first.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "source", content = "id")]
pub enum LayerSource {
    /// Host built-in defaults.
    Host,
    /// A plugin manifest's defaults for its own units.
    Plugin(String),
    /// A theme or profile `[presentation]` section.
    Theme(String),
    /// Declarative configuration.
    Config,
    /// Interactive defaults persisted to user state.
    UserState,
}

/// Ordered rules from one source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleLayer {
    /// Where the rules came from.
    pub source: LayerSource,
    /// Rules applied in order.
    pub rules: Vec<PresentationRule>,
}

/// `[presentation]` table as written in themes and configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresentationSection {
    /// Ordered rules.
    #[serde(default)]
    pub rules: Vec<PresentationRule>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, status: UnitStatus) -> UnitFacts {
        UnitFacts {
            kind: Some(UnitKind::Tool),
            tool: Some(name.to_owned()),
            status: Some(status),
            ..UnitFacts::default()
        }
    }

    #[test]
    fn rules_parse_from_toml_with_single_values_and_lists() {
        let section: PresentationSection = toml::from_str(
            r#"
            [[rules]]
            match = { kind = "tool", tool = ["read", "glob"], status = "succeeded" }
            set = { disclosure = "collapsed", group = "consecutive_same_tool" }

            [[rules]]
            match = { schema = "bcode.shell.run/*", plugin = { "bcode.shell.exit_code" = [1, 2] } }
            set = { preview = { lines = 12, keep = "head" }, parts = { timing = false }, plugin = { "bcode.shell.show_exit_code" = true } }

            [[rules]]
            set = { detail = "verbose" }
            "#,
        )
        .expect("rules parse");
        assert_eq!(section.rules.len(), 3);
        let first = &section.rules[0];
        assert_eq!(first.matcher.tool.0, ["read", "glob"]);
        assert_eq!(first.set.disclosure, Some(Disclosure::Collapsed));
        assert_eq!(
            section.rules[1].set.preview,
            Some(Preview {
                lines: 12,
                keep: PreviewKeep::Head
            })
        );
        assert_eq!(section.rules[2].matcher, FactMatcher::default());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        for source in [
            "[[rules]]\nmatch = { kinds = \"tool\" }",
            "[[rules]]\nset = { disclosure = \"open\" }",
            "[[rules]]\nset = { colour = 1 }",
            "rule = []",
        ] {
            assert!(
                toml::from_str::<PresentationSection>(source).is_err(),
                "{source}"
            );
        }
    }

    #[test]
    fn matchers_conjoin_conditions_and_lists_disjoin_values() {
        let matcher: FactMatcher =
            toml::from_str("kind = \"tool\"\ntool = [\"read\", \"grep\"]\nstatus = \"succeeded\"")
                .unwrap();
        assert!(matcher.matches(&tool("read", UnitStatus::Succeeded)));
        assert!(matcher.matches(&tool("grep", UnitStatus::Succeeded)));
        assert!(!matcher.matches(&tool("read", UnitStatus::Failed)));
        assert!(!matcher.matches(&tool("edit", UnitStatus::Succeeded)));
        assert!(!matcher.matches(&UnitFacts::default()));
        assert!(FactMatcher::default().matches(&UnitFacts::default()));
    }

    #[test]
    fn schema_wildcards_match_versions_but_not_prefix_collisions() {
        assert!(schema_matches("bcode.shell.run/*", "bcode.shell.run/2"));
        assert!(schema_matches("bcode.shell.run/*", "bcode.shell.run"));
        assert!(!schema_matches("bcode.shell.run/*", "bcode.shell.runner/1"));
        assert!(schema_matches("bcode.shell.run/2", "bcode.shell.run/2"));
        assert!(!schema_matches("bcode.shell.run/2", "bcode.shell.run/3"));
    }

    #[test]
    fn plugin_facts_and_depth_conditions_match() {
        let mut facts = tool("bash", UnitStatus::Failed);
        facts.depth = 2;
        facts
            .plugin
            .insert("bcode.shell.exit_code".to_owned(), FactValue::Int(2));
        let matcher: FactMatcher =
            toml::from_str("min_depth = 1\nplugin = { \"bcode.shell.exit_code\" = [1, 2] }")
                .unwrap();
        assert!(matcher.matches(&facts));
        facts.depth = 0;
        assert!(!matcher.matches(&facts));
    }

    #[test]
    fn plugin_fact_ranges_compare_integers_only() {
        let matcher: FactMatcher = toml::from_str(
            "plugin = { \"bcode.git.files\" = { gt = 10, lte = 50 }, \"bcode.git.dirty\" = true }",
        )
        .unwrap();
        let mut facts = tool("git.status", UnitStatus::Succeeded);
        assert!(
            !matcher.matches(&facts),
            "units without the facts never match"
        );
        facts.plugin = BTreeMap::from([
            ("bcode.git.files".to_owned(), FactValue::Int(11)),
            ("bcode.git.dirty".to_owned(), FactValue::Bool(true)),
        ]);
        assert!(matcher.matches(&facts));
        for (value, expected) in [
            (FactValue::Int(10), false),
            (FactValue::Int(50), true),
            (FactValue::Int(51), false),
            (FactValue::Text("20".to_owned()), false),
        ] {
            facts
                .plugin
                .insert("bcode.git.files".to_owned(), value.clone());
            assert_eq!(matcher.matches(&facts), expected, "{value:?}");
        }
        for invalid in [
            "plugin = { \"a.b\" = {} }",
            "plugin = { \"a.b\" = { above = 1 } }",
            "plugin = { \"a.b\" = { gt = \"x\" } }",
        ] {
            assert!(toml::from_str::<FactMatcher>(invalid).is_err(), "{invalid}");
        }
        let round_trip: FactMatcher =
            serde_json::from_value(serde_json::to_value(&matcher).unwrap()).unwrap();
        assert_eq!(round_trip, matcher);
    }

    #[test]
    fn plugin_keys_are_namespaced_by_their_plugin() {
        assert!(valid_plugin_key("bcode.git.files", "bcode.git"));
        assert!(valid_plugin_key("bcode.git.stats.show-all_2", "bcode.git"));
        for invalid in [
            "bcode.git",
            "bcode.git.",
            "bcode.gitx.files",
            "bcode.shell.files",
            "bcode.git.Files",
            "bcode.git..files",
            "bcode.git.fi les",
        ] {
            assert!(!valid_plugin_key(invalid, "bcode.git"), "{invalid}");
        }
        let long = format!("bcode.git.{}", "a".repeat(MAX_PLUGIN_KEY_BYTES));
        assert!(!valid_plugin_key(&long, "bcode.git"));
    }

    #[test]
    fn property_types_reject_wrong_types_and_out_of_bound_values() {
        let int = PropertyType::Int { min: 0, max: 5 };
        assert_eq!(int.reject(&FactValue::Int(5)), None);
        assert!(int.reject(&FactValue::Int(6)).is_some());
        assert!(int.reject(&FactValue::Bool(true)).is_some());
        let choice = PropertyType::Enum {
            values: vec!["short".to_owned(), "long".to_owned()],
        };
        assert_eq!(choice.reject(&FactValue::Text("long".to_owned())), None);
        assert!(choice.reject(&FactValue::Text("wide".to_owned())).is_some());
        assert_eq!(PropertyType::Bool.reject(&FactValue::Bool(false)), None);
        assert!(PropertyType::Bool.reject(&FactValue::Int(1)).is_some());
        assert!(FactType::Int.admits(&FactValue::Int(1)));
        assert!(!FactType::String.admits(&FactValue::Int(1)));
    }

    #[test]
    fn patches_merge_per_property() {
        let mut base: PropertyPatch = toml::from_str(
            "disclosure = \"collapsed\"\ndetail = \"minimal\"\nparts = { timing = false }",
        )
        .unwrap();
        let overlay: PropertyPatch =
            toml::from_str("disclosure = \"expanded\"\nparts = { arguments = false }").unwrap();
        base.merge(&overlay);
        assert_eq!(base.disclosure, Some(Disclosure::Expanded));
        assert_eq!(base.detail, Some(Detail::Minimal));
        assert_eq!(
            base.section_patches()
                .iter()
                .map(|(name, section)| (*name, section.visible))
                .collect::<Vec<_>>(),
            [("arguments", Some(false)), ("timing", Some(false))]
        );
    }

    #[test]
    fn group_accepts_legacy_strings_and_tables_with_defaults() {
        let parse = |value: &str| toml::from_str::<PropertyPatch>(&format!("group = {value}"));
        assert_eq!(parse("\"none\"").unwrap().group, Some(GroupSpec::none()));
        assert_eq!(
            parse("\"consecutive_tools\"").unwrap().group,
            Some(GroupSpec {
                by: GroupBy::Consecutive,
                bridge: vec![UnitKind::Reasoning],
                min: 2
            })
        );
        assert_eq!(
            parse("\"consecutive_same_tool\"").unwrap().group,
            Some(GroupSpec::new(GroupBy::SameTool))
        );
        assert_eq!(
            parse("{ by = \"same_producer\", bridge = [], min = 0 }")
                .unwrap()
                .group,
            Some(GroupSpec {
                by: GroupBy::SameProducer,
                bridge: Vec::new(),
                min: MIN_GROUP_SIZE
            })
        );
        assert_eq!(
            parse("{ by = \"turn\", min = 4 }").unwrap().group,
            Some(GroupSpec {
                by: GroupBy::Turn,
                bridge: vec![UnitKind::Reasoning],
                min: 4
            })
        );
        assert_eq!(parse("{}").unwrap().group, Some(GroupSpec::none()));
        for invalid in [
            "\"same_tool\"",
            "{ by = \"tool\" }",
            "{ by = \"turn\", size = 2 }",
        ] {
            assert!(parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn group_serializes_as_a_table_that_round_trips() {
        let patch = PropertyPatch {
            group: Some(GroupSpec::new(GroupBy::SameTool)),
            ..PropertyPatch::default()
        };
        let value = serde_json::to_value(&patch).unwrap();
        assert!(value["group"].is_object());
        assert_eq!(
            serde_json::from_value::<PropertyPatch>(value).unwrap(),
            patch
        );
    }

    #[test]
    fn v2_properties_parse_from_the_documented_example() {
        let patch: PropertyPatch = toml::from_str(
            r#"
            group = { by = "consecutive", bridge = ["reasoning"], min = 2 }
            sections = { arguments = { visible = false }, output = { disclosure = "collapsed", preview = { lines = 4 } } }
            header = { fields = ["marker", "tool", "target", "status", "timing"] }
            summary = { source = ["plugin", "template", "first_line"], template = "{tool} {target}" }
            spacing = { before = 0, after = 1 }
            indent = 2
            reveal = ["running", "failed"]
            "#,
        )
        .unwrap();
        assert_eq!(patch.sections["arguments"].visible, Some(false));
        assert_eq!(
            patch.sections["output"].disclosure,
            Some(Disclosure::Collapsed)
        );
        assert_eq!(patch.header.unwrap().fields.len(), 5);
        assert_eq!(
            patch.summary.unwrap().source,
            [
                SummarySource::Plugin,
                SummarySource::Template,
                SummarySource::FirstLine
            ]
        );
        assert_eq!(patch.indent, Some(2));
        assert_eq!(
            patch.reveal,
            Some(vec![UnitStatus::Running, UnitStatus::Failed])
        );
        let partial: PropertyPatch =
            toml::from_str("summary = { template = \"{tool}\" }\nspacing = { before = 2 }")
                .unwrap();
        // A template-only summary keeps the template reachable as the last source.
        assert_eq!(
            partial.summary.unwrap().source,
            [
                SummarySource::Plugin,
                SummarySource::FirstLine,
                SummarySource::Template
            ]
        );
        assert_eq!(
            partial.spacing,
            Some(Spacing {
                before: 2,
                after: 1
            })
        );
        for invalid in [
            "sections = { output = { shown = true } }",
            "header = { field = [] }",
            "summary = { source = [\"magic\"] }",
            "spacing = { left = 1 }",
        ] {
            assert!(
                toml::from_str::<PropertyPatch>(invalid).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn sections_merge_per_section_and_field_and_absorb_parts() {
        let mut base: PropertyPatch = toml::from_str(
            "sections = { output = { disclosure = \"collapsed\", visible = false } }",
        )
        .unwrap();
        base.merge(&toml::from_str("parts = { output = true, timing = false }").unwrap());
        base.merge(&toml::from_str("sections = { output = { preview = { lines = 3 } } }").unwrap());
        let sections = base.section_patches();
        assert_eq!(
            sections["output"],
            SectionPatch {
                visible: Some(true),
                disclosure: Some(Disclosure::Collapsed),
                preview: Some(Preview {
                    lines: 3,
                    keep: PreviewKeep::Tail
                }),
            }
        );
        assert_eq!(sections["timing"].visible, Some(false));

        let same_patch: PropertyPatch = toml::from_str(
            "parts = { output = false }\nsections = { output = { visible = true } }",
        )
        .unwrap();
        assert_eq!(same_patch.section_patches()["output"].visible, Some(true));
    }

    #[test]
    fn resolved_sections_default_to_the_unit_preview() {
        let preview = Preview {
            lines: 9,
            keep: PreviewKeep::Head,
        };
        let presentation = ResolvedPresentation {
            disclosure: Disclosure::Expanded,
            detail: Detail::Normal,
            preview,
            group: GroupSpec::none(),
            reveal: DEFAULT_REVEAL.to_vec(),
            placement: Placement::Inline,
            container: None,
            sections: BTreeMap::from([(
                "arguments".to_owned(),
                ResolvedSection {
                    visible: false,
                    ..ResolvedSection::defaults(preview)
                },
            )]),
            header: None,
            summary: SummarySpec::default(),
            spacing: Spacing::default(),
            indent: 0,
            display_name: None,
            section_labels: SectionLabels::Rows,
            hidden_marker: true,
            plugin: BTreeMap::new(),
        };
        assert_eq!(
            presentation.section("output"),
            ResolvedSection::defaults(preview)
        );
        assert!(presentation.part_visible("output"));
        assert!(!presentation.part_visible("arguments"));
    }

    #[test]
    fn duration_buckets_use_spec_names_and_boundaries() {
        for (millis, bucket, name) in [
            (0, DurationBucket::UnderOneSecond, "<1s"),
            (999, DurationBucket::UnderOneSecond, "<1s"),
            (1_000, DurationBucket::OneToTenSeconds, "1s..10s"),
            (9_999, DurationBucket::OneToTenSeconds, "1s..10s"),
            (10_000, DurationBucket::TenToSixtySeconds, "10s..60s"),
            (60_000, DurationBucket::OverSixtySeconds, ">60s"),
        ] {
            assert_eq!(DurationBucket::of_millis(millis), bucket, "{millis}");
            let matcher: FactMatcher = toml::from_str(&format!("duration = {name:?}")).unwrap();
            assert_eq!(matcher.duration.0, [bucket]);
        }
    }

    #[test]
    fn target_duration_and_last_in_turn_facts_match() {
        let mut facts = tool("read", UnitStatus::Succeeded);
        let matcher: FactMatcher = toml::from_str(
            "target = [\"src/lib.rs\", \"Cargo.toml\"]\nduration = \">60s\"\nlast_in_turn = true",
        )
        .unwrap();
        assert!(!matcher.matches(&facts));
        facts.target = Some("src/lib.rs".to_owned());
        facts.duration = Some(DurationBucket::OverSixtySeconds);
        facts.last_in_turn = Some(true);
        assert!(matcher.matches(&facts));
        facts.target = Some("src/lib".to_owned());
        assert!(!matcher.matches(&facts));
        facts.target = Some("Cargo.toml".to_owned());
        facts.last_in_turn = Some(false);
        assert!(!matcher.matches(&facts));
    }

    #[test]
    fn size_buckets_have_documented_boundaries() {
        let cases = [
            (0, SizeBucket::Empty),
            (1, SizeBucket::One),
            (10, SizeBucket::Few),
            (11, SizeBucket::Some),
            (50, SizeBucket::Some),
            (51, SizeBucket::Many),
            (200, SizeBucket::Many),
            (201, SizeBucket::Huge),
        ];
        for (count, bucket) in cases {
            assert_eq!(SizeBucket::of(count), bucket, "{count}");
        }
    }
}
