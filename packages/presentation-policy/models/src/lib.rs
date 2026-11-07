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
/// Maximum plugin-declared facts on one unit.
pub const MAX_PLUGIN_FACTS: usize = 32;
/// Maximum part-visibility or plugin-property entries in one patch.
pub const MAX_PATCH_ENTRIES: usize = 64;
/// Largest accepted preview budget, in source lines.
pub const MAX_PREVIEW_LINES: u32 = 10_000;

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
    /// Plugin-declared facts keyed by namespaced name.
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

/// How consecutive units group.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Grouping {
    /// No grouping.
    #[default]
    None,
    /// Group consecutive invocations of the same tool.
    ConsecutiveSameTool,
    /// Group consecutive tool invocations of any tool.
    ConsecutiveTools,
}

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
    pub group: Option<Grouping>,
    /// Placement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<Placement>,
    /// Chrome container recipe id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// Part visibility keyed by part name (for example `timing`, `arguments`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parts: BTreeMap<String, bool>,
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
            self.group = other.group;
        }
        if other.placement.is_some() {
            self.placement = other.placement;
        }
        if other.container.is_some() {
            self.container.clone_from(&other.container);
        }
        self.parts
            .extend(other.parts.iter().map(|(key, value)| (key.clone(), *value)));
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
    pub group: Grouping,
    /// Placement.
    pub placement: Placement,
    /// Chrome container recipe id, when one was selected.
    pub container: Option<String>,
    /// Part visibility; parts absent here are visible.
    pub parts: BTreeMap<String, bool>,
    /// Plugin-namespaced properties.
    pub plugin: BTreeMap<String, FactValue>,
}

impl ResolvedPresentation {
    /// Whether `part` should be shown.
    #[must_use]
    pub fn part_visible(&self, part: &str) -> bool {
        self.parts.get(part).copied().unwrap_or(true)
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
    /// Plugin-declared facts.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub plugin: BTreeMap<String, OneOrMany<FactValue>>,
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
            && self.plugin.iter().all(|(name, condition)| {
                condition.admits(|expected| facts.plugin.get(name) == Some(expected))
            })
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
        assert_eq!(base.parts.len(), 2);
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
