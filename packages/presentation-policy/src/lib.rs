#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Renderer-neutral structural presentation policy.
//!
//! Rule layers from the host, plugins, themes, configuration, and user state
//! compile into a [`CompiledPolicy`]. Resolving a unit's [`UnitFacts`] yields a
//! [`ResolvedPresentation`]; the [visibility floor](apply_visibility_floor)
//! then guarantees that units awaiting a decision and failures stay visible no
//! matter what untrusted theme layers request. [`group_units`] computes stable
//! groups of consecutive units for every renderer alike.
//!
//! Resolution is pure and bounded: compilation indexes rules by unit kind so a
//! unit evaluates only rules that can match it.

pub mod grouping;

use std::collections::{BTreeMap, BTreeSet};

pub use bcode_presentation_policy_models as models;
use models::{
    Detail, Disclosure, Grouping, LayerSource, MAX_PATCH_ENTRIES, MAX_PREVIEW_LINES,
    MAX_RULES_PER_LAYER, Placement, PresentationRule, Preview, PreviewKeep, PropertyPatch,
    ResolvedPresentation, RuleLayer, UnitFacts, UnitKind,
};

pub use grouping::{GroupingInput, UnitGroup, group_units};

/// Default inline preview budget, in source lines.
pub const DEFAULT_PREVIEW_LINES: u32 = 28;

/// Host built-in property values applied before any layer.
#[must_use]
pub const fn host_defaults() -> ResolvedPresentation {
    ResolvedPresentation {
        disclosure: Disclosure::Expanded,
        detail: Detail::Normal,
        preview: Preview {
            lines: DEFAULT_PREVIEW_LINES,
            keep: PreviewKeep::Tail,
        },
        group: Grouping::None,
        placement: Placement::Inline,
        container: None,
        parts: BTreeMap::new(),
        plugin: BTreeMap::new(),
    }
}

/// Severity of a policy diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DiagnosticSeverity {
    /// Accepted, but probably a mistake (for example, an unknown property).
    Warning,
    /// Rejected; the offending rule or value was dropped.
    Error,
}

/// One problem found while compiling layers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDiagnostic {
    /// Severity.
    pub severity: DiagnosticSeverity,
    /// Layer containing the problem.
    pub source: LayerSource,
    /// Zero-based rule index within the layer, when rule-specific.
    pub rule: Option<usize>,
    /// Human-readable description.
    pub message: String,
}

/// Plugin-declared properties and facts that rules may reference.
///
/// Namespaced keys that are not registered produce warnings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicyRegistry {
    /// Registered plugin properties.
    pub properties: BTreeSet<String>,
    /// Registered plugin facts.
    pub facts: BTreeSet<String>,
    /// Known part names; when empty, part names are not checked.
    pub parts: BTreeSet<String>,
}

impl PolicyRegistry {
    /// Registry with the host's built-in part names.
    #[must_use]
    pub fn with_host_parts() -> Self {
        Self {
            parts: [
                "arguments",
                "output",
                "result",
                "timing",
                "metadata",
                "diff",
                "artifact",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            ..Self::default()
        }
    }
}

/// Where a resolved property value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyOrigin {
    /// Property name (`disclosure`, `parts.timing`, `bcode.shell.show_exit_code`, …).
    pub property: String,
    /// Layer that set the final value.
    pub source: LayerSource,
    /// Rule index within that layer.
    pub rule: usize,
}

/// Resolution plus provenance, for `explain` tooling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplainedPresentation {
    /// Final presentation after the visibility floor.
    pub presentation: ResolvedPresentation,
    /// Origin of every property set by a rule; unlisted properties are host defaults.
    pub origins: Vec<PropertyOrigin>,
    /// Floor adjustments, as human-readable notes.
    pub floor: Vec<String>,
}

#[derive(Debug, Clone)]
struct CompiledRule {
    source: LayerSource,
    index: usize,
    rule: PresentationRule,
}

/// Layers compiled for fast, deterministic resolution.
///
/// The default value is identical to compiling no layers.
#[derive(Debug, Clone)]
pub struct CompiledPolicy {
    rules: Vec<CompiledRule>,
    /// Rule positions (into `rules`) that can match each kind, ascending.
    by_kind: BTreeMap<UnitKind, Vec<usize>>,
    /// Positions of rules without a kind condition, ascending.
    unkinded: Vec<usize>,
    fingerprint: u64,
}

impl Default for CompiledPolicy {
    fn default() -> Self {
        Self {
            rules: Vec::new(),
            by_kind: BTreeMap::new(),
            unkinded: Vec::new(),
            fingerprint: fingerprint(&[]),
        }
    }
}

impl CompiledPolicy {
    /// Compile `layers` (lowest precedence first) against `registry`.
    ///
    /// Invalid rules are dropped with an error diagnostic; suspicious but
    /// usable rules are kept with a warning.
    #[must_use]
    pub fn compile(
        layers: &[RuleLayer],
        registry: &PolicyRegistry,
    ) -> (Self, Vec<PolicyDiagnostic>) {
        let mut diagnostics = Vec::new();
        let mut compiled = Self::default();
        debug_assert_eq!(compiled.fingerprint, fingerprint(&[]));
        for layer in layers {
            if layer.rules.len() > MAX_RULES_PER_LAYER {
                diagnostics.push(PolicyDiagnostic {
                    severity: DiagnosticSeverity::Error,
                    source: layer.source.clone(),
                    rule: None,
                    message: format!(
                        "layer has {} rules; only the first {MAX_RULES_PER_LAYER} apply",
                        layer.rules.len()
                    ),
                });
            }
            for (index, rule) in layer.rules.iter().take(MAX_RULES_PER_LAYER).enumerate() {
                let mut report = |severity, message: String| {
                    diagnostics.push(PolicyDiagnostic {
                        severity,
                        source: layer.source.clone(),
                        rule: Some(index),
                        message,
                    });
                };
                if let Some(problem) = invalid_rule(rule, &layer.source) {
                    report(DiagnosticSeverity::Error, problem);
                    continue;
                }
                for warning in suspicious_rule(rule, registry) {
                    report(DiagnosticSeverity::Warning, warning);
                }
                let mut rule = rule.clone();
                if let LayerSource::Plugin(plugin) = &layer.source
                    && rule.matcher.producer.is_unset()
                {
                    // Plugin defaults are scoped to the plugin's own units.
                    rule.matcher.producer.0.push(plugin.clone());
                }
                let position = compiled.rules.len();
                if rule.matcher.kind.is_unset() {
                    compiled.unkinded.push(position);
                } else {
                    let kinds: BTreeSet<UnitKind> = rule.matcher.kind.0.iter().copied().collect();
                    for kind in kinds {
                        compiled.by_kind.entry(kind).or_default().push(position);
                    }
                }
                compiled.rules.push(CompiledRule {
                    source: layer.source.clone(),
                    index,
                    rule,
                });
            }
        }
        compiled.fingerprint = fingerprint(&compiled.rules);
        (compiled, diagnostics)
    }

    /// Stable fingerprint of the compiled rules, for cache keys.
    #[must_use]
    pub const fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    /// Whether any rule conditions on turn recency.
    ///
    /// Renderers use this to re-resolve units when a new turn begins only when
    /// the policy can observe it.
    #[must_use]
    pub fn uses_turn_facts(&self) -> bool {
        self.rules
            .iter()
            .any(|compiled| !compiled.rule.matcher.turn.is_unset())
    }

    /// Number of compiled rules.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.rules.len()
    }

    /// Whether no rules compiled.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Matching rules for `facts`, in application order.
    fn matching<'a>(&'a self, facts: &'a UnitFacts) -> impl Iterator<Item = &'a CompiledRule> {
        let kinded = facts
            .kind
            .and_then(|kind| self.by_kind.get(&kind))
            .map_or(&[][..], Vec::as_slice);
        merge_sorted(kinded, &self.unkinded)
            .map(|position| &self.rules[position])
            .filter(|compiled| compiled.rule.matcher.matches(facts))
    }

    /// Resolve `facts` to a presentation, including the visibility floor.
    #[must_use]
    pub fn resolve(&self, facts: &UnitFacts) -> ResolvedPresentation {
        let mut patch = PropertyPatch::default();
        for compiled in self.matching(facts) {
            patch.merge(&compiled.rule.set);
        }
        let mut presentation = apply_patch(host_defaults(), patch);
        apply_visibility_floor(&mut presentation, facts);
        presentation
    }

    /// Resolve with provenance for every property and floor adjustment.
    #[must_use]
    pub fn explain(&self, facts: &UnitFacts) -> ExplainedPresentation {
        let mut patch = PropertyPatch::default();
        let mut origins: BTreeMap<String, PropertyOrigin> = BTreeMap::new();
        for compiled in self.matching(facts) {
            for property in set_properties(&compiled.rule.set) {
                origins.insert(
                    property.clone(),
                    PropertyOrigin {
                        property,
                        source: compiled.source.clone(),
                        rule: compiled.index,
                    },
                );
            }
            patch.merge(&compiled.rule.set);
        }
        let mut presentation = apply_patch(host_defaults(), patch);
        let floor = apply_visibility_floor(&mut presentation, facts);
        ExplainedPresentation {
            presentation,
            origins: origins.into_values().collect(),
            floor,
        }
    }
}

/// Raise `presentation` to the host visibility floor; returns a note for each adjustment.
///
/// * Units awaiting a user decision are always expanded, with at least normal
///   detail and their arguments and metadata visible.
/// * Failed or errored units are never shown below summary.
///
/// Count markers for hidden units are the renderer's responsibility, driven by
/// [`Disclosure::Hidden`].
pub fn apply_visibility_floor(
    presentation: &mut ResolvedPresentation,
    facts: &UnitFacts,
) -> Vec<String> {
    let mut notes = Vec::new();
    if facts.awaiting_decision {
        if presentation.disclosure != Disclosure::Expanded {
            notes.push(format!(
                "awaiting a decision: disclosure raised from {:?} to Expanded",
                presentation.disclosure
            ));
            presentation.disclosure = Disclosure::Expanded;
        }
        if presentation.detail == Detail::Minimal {
            notes.push("awaiting a decision: detail raised from Minimal to Normal".to_owned());
            presentation.detail = Detail::Normal;
        }
        for part in ["arguments", "metadata"] {
            if presentation.parts.remove(part) == Some(false) {
                notes.push(format!("awaiting a decision: part {part} kept visible"));
            }
        }
    }
    let failed = facts.is_error
        || matches!(
            facts.status,
            Some(models::UnitStatus::Failed | models::UnitStatus::TimedOut)
        );
    if failed
        && matches!(
            presentation.disclosure,
            Disclosure::Collapsed | Disclosure::Hidden
        )
    {
        notes.push(format!(
            "failed: disclosure raised from {:?} to Summary",
            presentation.disclosure
        ));
        presentation.disclosure = Disclosure::Summary;
    }
    notes
}

fn apply_patch(
    mut presentation: ResolvedPresentation,
    patch: PropertyPatch,
) -> ResolvedPresentation {
    if let Some(disclosure) = patch.disclosure {
        presentation.disclosure = disclosure;
    }
    if let Some(detail) = patch.detail {
        presentation.detail = detail;
    }
    if let Some(preview) = patch.preview {
        presentation.preview = preview;
    }
    if let Some(group) = patch.group {
        presentation.group = group;
    }
    if let Some(placement) = patch.placement {
        presentation.placement = placement;
    }
    if patch.container.is_some() {
        presentation.container = patch.container;
    }
    presentation.parts.extend(patch.parts);
    presentation.plugin.extend(patch.plugin);
    presentation
}

fn invalid_rule(rule: &PresentationRule, source: &LayerSource) -> Option<String> {
    let set = &rule.set;
    if let LayerSource::Plugin(plugin) = source
        && rule
            .matcher
            .producer
            .0
            .iter()
            .any(|producer| producer != plugin)
    {
        return Some(format!(
            "plugin {plugin} rules may only target its own units"
        ));
    }
    if set
        .preview
        .is_some_and(|preview| preview.lines > MAX_PREVIEW_LINES)
    {
        return Some(format!("preview exceeds {MAX_PREVIEW_LINES} lines"));
    }
    if set.parts.len() > MAX_PATCH_ENTRIES || set.plugin.len() > MAX_PATCH_ENTRIES {
        return Some(format!("set has more than {MAX_PATCH_ENTRIES} entries"));
    }
    if rule.matcher.plugin.len() > models::MAX_PLUGIN_FACTS {
        return Some(format!(
            "match has more than {} plugin facts",
            models::MAX_PLUGIN_FACTS
        ));
    }
    if let Some(key) = set.plugin.keys().find(|key| !key.contains('.')) {
        return Some(format!("plugin property {key:?} must be namespaced"));
    }
    None
}

fn suspicious_rule(rule: &PresentationRule, registry: &PolicyRegistry) -> Vec<String> {
    let mut warnings = Vec::new();
    for key in rule.set.plugin.keys() {
        if !registry.properties.contains(key) {
            warnings.push(format!("unknown plugin property {key:?}"));
        }
    }
    for key in rule.matcher.plugin.keys() {
        if !registry.facts.contains(key) {
            warnings.push(format!("unknown plugin fact {key:?}"));
        }
    }
    if !registry.parts.is_empty() {
        for key in rule.set.parts.keys() {
            if !registry.parts.contains(key) {
                warnings.push(format!("unknown part {key:?}"));
            }
        }
    }
    if rule.set.is_empty() {
        warnings.push("rule sets no properties".to_owned());
    }
    warnings
}

fn set_properties(patch: &PropertyPatch) -> Vec<String> {
    let mut properties = Vec::new();
    for (set, name) in [
        (patch.disclosure.is_some(), "disclosure"),
        (patch.detail.is_some(), "detail"),
        (patch.preview.is_some(), "preview"),
        (patch.group.is_some(), "group"),
        (patch.placement.is_some(), "placement"),
        (patch.container.is_some(), "container"),
    ] {
        if set {
            properties.push(name.to_owned());
        }
    }
    properties.extend(patch.parts.keys().map(|part| format!("parts.{part}")));
    properties.extend(patch.plugin.keys().cloned());
    properties
}

/// Merge two ascending position lists into one ascending sequence.
fn merge_sorted<'a>(left: &'a [usize], right: &'a [usize]) -> impl Iterator<Item = usize> + 'a {
    let mut left = left.iter().copied().peekable();
    let mut right = right.iter().copied().peekable();
    std::iter::from_fn(move || match (left.peek(), right.peek()) {
        (Some(a), Some(b)) if a <= b => left.next(),
        (_, Some(_)) => right.next(),
        (Some(_), None) => left.next(),
        (None, None) => None,
    })
}

fn fingerprint(rules: &[CompiledRule]) -> u64 {
    // FNV-1a over a canonical debug encoding: stable across runs and platforms.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for compiled in rules {
        for byte in format!("{:?}|{:?}", compiled.source, compiled.rule).bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use models::{PresentationSection, UnitStatus};

    fn layer(source: LayerSource, rules: &str) -> RuleLayer {
        RuleLayer {
            source,
            rules: toml::from_str::<PresentationSection>(rules)
                .expect("rules parse")
                .rules,
        }
    }

    fn tool(name: &str, status: UnitStatus) -> UnitFacts {
        UnitFacts {
            kind: Some(UnitKind::Tool),
            tool: Some(name.to_owned()),
            status: Some(status),
            ..UnitFacts::default()
        }
    }

    const BARE: &str = r#"
        [[rules]]
        match = { kind = "tool" }
        set = { disclosure = "summary", detail = "minimal" }

        [[rules]]
        match = { kind = "tool", status = ["running", "failed"] }
        set = { disclosure = "expanded" }
    "#;

    #[test]
    fn later_rules_and_layers_win_per_property() {
        let (policy, diagnostics) = CompiledPolicy::compile(
            &[
                layer(LayerSource::Theme("bare".into()), BARE),
                layer(
                    LayerSource::Config,
                    "[[rules]]\nmatch = { tool = \"read\" }\nset = { disclosure = \"collapsed\" }",
                ),
            ],
            &PolicyRegistry::with_host_parts(),
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let read = policy.resolve(&tool("read", UnitStatus::Succeeded));
        assert_eq!(read.disclosure, Disclosure::Collapsed);
        assert_eq!(read.detail, Detail::Minimal);
        let bash = policy.resolve(&tool("bash", UnitStatus::Succeeded));
        assert_eq!(bash.disclosure, Disclosure::Summary);
        assert_eq!(
            policy
                .resolve(&tool("bash", UnitStatus::Running))
                .disclosure,
            Disclosure::Expanded
        );
        assert_eq!(bash.preview.lines, DEFAULT_PREVIEW_LINES);
    }

    #[test]
    fn lifecycle_changes_re_resolve_without_special_cases() {
        let (policy, _) = CompiledPolicy::compile(
            &[layer(LayerSource::Theme("bare".into()), BARE)],
            &PolicyRegistry::default(),
        );
        let mut facts = tool("bash", UnitStatus::Running);
        assert_eq!(policy.resolve(&facts).disclosure, Disclosure::Expanded);
        facts.status = Some(UnitStatus::Succeeded);
        assert_eq!(policy.resolve(&facts).disclosure, Disclosure::Summary);
    }

    #[test]
    fn visibility_floor_overrides_untrusted_layers() {
        let hostile = layer(
            LayerSource::Theme("repo".into()),
            r#"
            [[rules]]
            set = { disclosure = "hidden", detail = "minimal", parts = { arguments = false } }
            "#,
        );
        let (policy, _) = CompiledPolicy::compile(&[hostile], &PolicyRegistry::with_host_parts());
        let permission = UnitFacts {
            kind: Some(UnitKind::Permission),
            awaiting_decision: true,
            ..UnitFacts::default()
        };
        let explained = policy.explain(&permission);
        assert_eq!(explained.presentation.disclosure, Disclosure::Expanded);
        assert_eq!(explained.presentation.detail, Detail::Normal);
        assert!(explained.presentation.part_visible("arguments"));
        assert_eq!(explained.floor.len(), 3, "{:?}", explained.floor);

        assert_eq!(
            policy.resolve(&tool("bash", UnitStatus::Failed)).disclosure,
            Disclosure::Summary
        );
        let errored = UnitFacts {
            is_error: true,
            ..tool("bash", UnitStatus::Succeeded)
        };
        assert_eq!(policy.resolve(&errored).disclosure, Disclosure::Summary);
        assert_eq!(
            policy
                .resolve(&tool("read", UnitStatus::Succeeded))
                .disclosure,
            Disclosure::Hidden
        );
    }

    #[test]
    fn plugin_layers_are_scoped_to_their_own_units() {
        let (policy, diagnostics) = CompiledPolicy::compile(
            &[
                layer(
                    LayerSource::Plugin("bcode.shell".into()),
                    "[[rules]]\nset = { disclosure = \"collapsed\" }",
                ),
                layer(
                    LayerSource::Plugin("bcode.shell".into()),
                    "[[rules]]\nmatch = { producer = \"bcode.fs\" }\nset = { detail = \"verbose\" }",
                ),
            ],
            &PolicyRegistry::default(),
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].severity, DiagnosticSeverity::Error);
        let mut shell = tool("bash", UnitStatus::Succeeded);
        shell.producer = Some("bcode.shell".into());
        let mut fs = tool("read", UnitStatus::Succeeded);
        fs.producer = Some("bcode.fs".into());
        assert_eq!(policy.resolve(&shell).disclosure, Disclosure::Collapsed);
        assert_eq!(policy.resolve(&fs).disclosure, Disclosure::Expanded);
        assert_eq!(policy.resolve(&fs).detail, Detail::Normal);
    }

    #[test]
    fn diagnostics_warn_on_unknown_keys_and_drop_invalid_rules() {
        let registry = PolicyRegistry {
            properties: ["bcode.shell.show_exit_code".to_owned()].into(),
            ..PolicyRegistry::with_host_parts()
        };
        let (policy, diagnostics) = CompiledPolicy::compile(
            &[layer(
                LayerSource::Config,
                r#"
                [[rules]]
                set = { parts = { timeing = false }, plugin = { "bcode.shell.show_exit_code" = true, "x.y" = 1 } }

                [[rules]]
                set = { preview = { lines = 999999 } }

                [[rules]]
                set = { plugin = { bare = 1 } }

                [[rules]]
                match = { kind = "tool" }
                "#,
            )],
            &registry,
        );
        let warnings: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == DiagnosticSeverity::Warning)
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();
        assert_eq!(
            warnings,
            [
                "unknown plugin property \"x.y\"",
                "unknown part \"timeing\"",
                "rule sets no properties"
            ]
        );
        let errors = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error)
            .count();
        assert_eq!(errors, 2);
        assert_eq!(policy.len(), 2);
    }

    #[test]
    fn explain_reports_the_winning_layer_and_rule() {
        let (policy, _) = CompiledPolicy::compile(
            &[
                layer(LayerSource::Theme("bare".into()), BARE),
                layer(
                    LayerSource::UserState,
                    "[[rules]]\nmatch = { tool = \"read\" }\nset = { disclosure = \"collapsed\" }",
                ),
            ],
            &PolicyRegistry::default(),
        );
        let explained = policy.explain(&tool("read", UnitStatus::Succeeded));
        let origin = |property: &str| {
            explained
                .origins
                .iter()
                .find(|origin| origin.property == property)
                .cloned()
                .unwrap()
        };
        assert_eq!(origin("disclosure").source, LayerSource::UserState);
        assert_eq!(origin("disclosure").rule, 0);
        assert_eq!(origin("detail").source, LayerSource::Theme("bare".into()));
        assert_eq!(explained.floor, Vec::<String>::new());
    }

    #[test]
    fn kind_index_preserves_global_rule_order() {
        let (policy, _) = CompiledPolicy::compile(
            &[layer(
                LayerSource::Config,
                r#"
                [[rules]]
                match = { kind = "tool" }
                set = { detail = "verbose" }

                [[rules]]
                set = { detail = "minimal" }

                [[rules]]
                match = { kind = ["tool", "reasoning", "tool"] }
                set = { detail = "normal" }
                "#,
            )],
            &PolicyRegistry::default(),
        );
        assert_eq!(
            policy.resolve(&tool("x", UnitStatus::Running)).detail,
            Detail::Normal
        );
        let message = UnitFacts {
            kind: Some(UnitKind::AssistantMessage),
            ..UnitFacts::default()
        };
        assert_eq!(policy.resolve(&message).detail, Detail::Minimal);
    }

    #[test]
    fn default_policy_equals_compiling_no_layers() {
        let (empty, diagnostics) = CompiledPolicy::compile(&[], &PolicyRegistry::default());
        assert_eq!(diagnostics, Vec::new());
        assert_eq!(empty.fingerprint(), CompiledPolicy::default().fingerprint());
    }

    #[test]
    fn fingerprint_tracks_rule_changes() {
        let compile = |rules: &str| {
            CompiledPolicy::compile(
                &[layer(LayerSource::Config, rules)],
                &PolicyRegistry::default(),
            )
            .0
            .fingerprint()
        };
        let a = compile("[[rules]]\nset = { detail = \"minimal\" }");
        assert_eq!(a, compile("[[rules]]\nset = { detail = \"minimal\" }"));
        assert_ne!(a, compile("[[rules]]\nset = { detail = \"verbose\" }"));
    }
}
