#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Renderer-neutral structural presentation policy.
//!
//! Rule layers from the host, plugins, themes, configuration, and user state
//! compile into a [`CompiledPolicy`]. Resolving a unit's [`UnitFacts`] yields a
//! [`ResolvedPresentation`]; the [visibility floor](apply_visibility_floor)
//! then guarantees that units awaiting a decision and failures stay visible no
//! matter what untrusted theme layers request. [`Grouping`] computes stable
//! groups of consecutive units for every renderer alike, and
//! [`render_summary_template`] renders summary templates the same way
//! everywhere.
//!
//! Resolution is pure and bounded: compilation indexes rules by unit kind, then
//! tool, so a unit evaluates only rules that can match it.

pub mod grouping;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

pub use bcode_presentation_policy_models as models;
use models::{
    DEFAULT_REVEAL, Detail, Disclosure, FactCondition, FactMatcher, FactType, FactValue,
    GROUP_HEADER_FIELDS, GroupSpec, HEADER_FIELDS, HOST_SECTIONS, LayerSource, MAX_INDENT,
    MAX_PATCH_ENTRIES, MAX_PREVIEW_LINES, MAX_RULES_PER_LAYER, MAX_SUMMARY_TEMPLATE_BYTES,
    Placement, PluginFactSchema, PluginPresentationSchemas, PluginPropertySchema, PresentationRule,
    Preview, PreviewKeep, PropertyPatch, ResolvedPresentation, ResolvedSection, RuleLayer, Spacing,
    SummarySpec, TEMPLATE_FIELDS, UNIT_HEADER_FIELDS, UnitFacts, UnitKind, UnitStatus,
};

pub use grouping::{
    GroupRole, Grouping, GroupingInput, MAX_BRIDGED_GAP, UnitGroup, group_units, is_revealed,
};

/// Largest rendered summary, in bytes; longer output is cut on a character
/// boundary.
pub const MAX_RENDERED_SUMMARY_BYTES: usize = 1024;

/// Sections that describe the operation a pending decision is about; the
/// floor keeps them visible and expanded on units awaiting a decision.
pub const DECISION_SECTIONS: [&str; 3] = ["arguments", "metadata", "confirmation"];

/// Sections the floor never hides on failed units.
pub const FAILURE_SECTIONS: [&str; 2] = ["error", "output"];

/// Default inline preview budget, in source lines.
pub const DEFAULT_PREVIEW_LINES: u32 = 28;

/// Most distinct resolutions a [`CompiledPolicy`] memoizes; the memo is
/// cleared when it would grow past this.
///
/// Entries are keyed by the set of matching rules, not by unit, so a
/// transcript of any length typically needs only a handful.
pub const MAX_RESOLUTION_MEMO: usize = 256;

/// Host built-in property values applied before any layer.
#[must_use]
pub fn host_defaults() -> ResolvedPresentation {
    ResolvedPresentation {
        disclosure: Disclosure::Expanded,
        detail: Detail::Normal,
        preview: Preview {
            lines: DEFAULT_PREVIEW_LINES,
            keep: PreviewKeep::Tail,
        },
        group: GroupSpec::none(),
        reveal: DEFAULT_REVEAL.to_vec(),
        placement: Placement::Inline,
        container: None,
        sections: BTreeMap::new(),
        header: None,
        summary: SummarySpec::default(),
        spacing: Spacing {
            before: 0,
            after: 1,
        },
        indent: 0,
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
/// Namespaced keys that are not registered produce warnings. Values of
/// registered properties that violate their schema are dropped with a
/// warning, and registered properties apply only to their plugin's units.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicyRegistry {
    /// Registered plugin properties, by namespaced name.
    pub properties: BTreeMap<String, PluginPropertySchema>,
    /// Registered plugin facts, by namespaced name.
    pub facts: BTreeMap<String, PluginFactSchema>,
    /// Known part and section names; when empty, names are not checked.
    /// Namespaced names (containing a `.`) are never checked.
    pub parts: BTreeSet<String>,
}

impl PolicyRegistry {
    /// Registry with the host's built-in section names, plus the legacy
    /// `timing` part.
    #[must_use]
    pub fn with_host_parts() -> Self {
        Self {
            parts: HOST_SECTIONS
                .into_iter()
                .chain(["timing"])
                .map(str::to_owned)
                .collect(),
            ..Self::default()
        }
    }

    /// This registry with the plugin-declared `schemas` registered.
    #[must_use]
    pub fn with_plugin_schemas(mut self, schemas: &PluginPresentationSchemas) -> Self {
        self.facts.extend(
            schemas
                .facts
                .iter()
                .map(|(name, schema)| (name.clone(), schema.clone())),
        );
        self.properties.extend(
            schemas
                .properties
                .iter()
                .map(|(name, schema)| (name.clone(), schema.clone())),
        );
        self
    }
}

/// Where a resolved property value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyOrigin {
    /// Property name (`disclosure`, `sections.output.visible`,
    /// `bcode.shell.show_exit_code`, …).
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

/// Rule positions (into `CompiledPolicy::rules`) bucketed by tool condition.
#[derive(Debug, Clone, Default)]
struct ToolIndex {
    /// Rules naming each tool, ascending.
    by_tool: BTreeMap<String, Vec<usize>>,
    /// Rules without a tool condition, ascending.
    untooled: Vec<usize>,
}

impl ToolIndex {
    fn insert(&mut self, position: usize, matcher: &FactMatcher) {
        if matcher.tool.is_unset() {
            self.untooled.push(position);
        } else {
            let tools: BTreeSet<&String> = matcher.tool.0.iter().collect();
            for tool in tools {
                self.by_tool.entry(tool.clone()).or_default().push(position);
            }
        }
    }

    /// Positions that can match a unit with `tool`, ascending.
    fn candidates<'a>(&'a self, tool: Option<&str>) -> impl Iterator<Item = usize> + 'a {
        let tooled = tool
            .and_then(|tool| self.by_tool.get(tool))
            .map_or(&[][..], Vec::as_slice);
        merge_sorted(tooled.iter().copied(), self.untooled.iter().copied())
    }
}

/// What a resolution depends on besides the unit's facts matching rules:
/// the matching rule positions, in application order, and the facts the
/// visibility floor reads.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ResolutionKey {
    rules: Vec<usize>,
    /// The unit's producer, when plugin properties are scoped by it.
    producer: Option<String>,
    group: bool,
    awaiting_decision: bool,
    failed: bool,
}

/// Layers compiled for fast, deterministic resolution.
///
/// The default value is identical to compiling no layers.
///
/// [`resolve`](Self::resolve) memoizes its results by matching rule set and
/// floor facts (at most [`MAX_RESOLUTION_MEMO`] entries), so units that match
/// the same rules share one patch application. The memo belongs to this
/// compilation: recompiling yields a new, empty one, and clones start empty.
#[derive(Debug)]
pub struct CompiledPolicy {
    rules: Vec<CompiledRule>,
    /// Rules that can match each kind, by tool.
    by_kind: BTreeMap<UnitKind, ToolIndex>,
    /// Rules without a kind condition, by tool.
    unkinded: ToolIndex,
    /// Registered plugin properties, for scoping values to their plugin.
    properties: BTreeMap<String, PluginPropertySchema>,
    /// Whether any rule sets a plugin property, so resolution depends on the
    /// unit's producer.
    scopes_properties: bool,
    fingerprint: u64,
    memo: Mutex<BTreeMap<ResolutionKey, ResolvedPresentation>>,
}

impl Default for CompiledPolicy {
    fn default() -> Self {
        Self {
            rules: Vec::new(),
            by_kind: BTreeMap::new(),
            unkinded: ToolIndex::default(),
            properties: BTreeMap::new(),
            scopes_properties: false,
            fingerprint: fingerprint(&[], &BTreeMap::new()),
            memo: Mutex::default(),
        }
    }
}

impl Clone for CompiledPolicy {
    fn clone(&self) -> Self {
        Self {
            rules: self.rules.clone(),
            by_kind: self.by_kind.clone(),
            unkinded: self.unkinded.clone(),
            properties: self.properties.clone(),
            scopes_properties: self.scopes_properties,
            fingerprint: self.fingerprint,
            memo: Mutex::default(),
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
        let mut compiled = Self {
            properties: registry.properties.clone(),
            ..Self::default()
        };
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
                rule.set.plugin.retain(|key, value| {
                    let rejection = registry
                        .properties
                        .get(key)
                        .and_then(|schema| schema.value_type.reject(value));
                    if let Some(why) = &rejection {
                        report(
                            DiagnosticSeverity::Warning,
                            format!("plugin property {key:?} ignored: {why}"),
                        );
                    }
                    rejection.is_none()
                });
                compiled.scopes_properties |= !rule.set.plugin.is_empty();
                if let LayerSource::Plugin(plugin) = &layer.source
                    && rule.matcher.producer.is_unset()
                {
                    // Plugin defaults are scoped to the plugin's own units.
                    rule.matcher.producer.0.push(plugin.clone());
                }
                let position = compiled.rules.len();
                if rule.matcher.kind.is_unset() {
                    compiled.unkinded.insert(position, &rule.matcher);
                } else {
                    let kinds: BTreeSet<UnitKind> = rule.matcher.kind.0.iter().copied().collect();
                    for kind in kinds {
                        compiled
                            .by_kind
                            .entry(kind)
                            .or_default()
                            .insert(position, &rule.matcher);
                    }
                }
                compiled.rules.push(CompiledRule {
                    source: layer.source.clone(),
                    index,
                    rule,
                });
            }
        }
        compiled.fingerprint = fingerprint(&compiled.rules, &compiled.properties);
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

    /// Positions of the rules matching `facts`, in application order.
    fn matching_positions<'a>(&'a self, facts: &'a UnitFacts) -> impl Iterator<Item = usize> + 'a {
        let tool = facts.tool.as_deref();
        let kinded = facts
            .kind
            .and_then(|kind| self.by_kind.get(&kind))
            .into_iter()
            .flat_map(move |index| index.candidates(tool));
        merge_sorted(kinded, self.unkinded.candidates(tool))
            .filter(|&position| self.rules[position].rule.matcher.matches(facts))
    }

    /// Matching rules for `facts`, in application order.
    fn matching<'a>(&'a self, facts: &'a UnitFacts) -> impl Iterator<Item = &'a CompiledRule> {
        self.matching_positions(facts)
            .map(|position| &self.rules[position])
    }

    /// Resolve `facts` to a presentation, including the visibility floor.
    ///
    /// Results are memoized per compilation (see [`CompiledPolicy`]); a
    /// memoized result is identical to resolving afresh.
    #[must_use]
    pub fn resolve(&self, facts: &UnitFacts) -> ResolvedPresentation {
        let key = ResolutionKey {
            rules: self.matching_positions(facts).collect(),
            producer: self
                .scopes_properties
                .then(|| facts.producer.clone())
                .flatten(),
            group: facts.kind == Some(UnitKind::Group),
            awaiting_decision: facts.awaiting_decision,
            failed: unit_failed(facts),
        };
        if let Some(hit) = self
            .memo
            .lock()
            .ok()
            .and_then(|memo| memo.get(&key).cloned())
        {
            return hit;
        }
        let mut patch = PropertyPatch::default();
        for &position in &key.rules {
            patch.merge(&self.rules[position].rule.set);
        }
        let mut presentation = apply_patch(host_defaults(), patch);
        self.scope_plugin_properties(&mut presentation.plugin, facts.producer.as_deref());
        apply_visibility_floor(&mut presentation, facts);
        if let Ok(mut memo) = self.memo.lock() {
            if memo.len() >= MAX_RESOLUTION_MEMO {
                memo.clear();
            }
            memo.insert(key, presentation.clone());
        }
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
        let set_plugin: Vec<String> = patch.plugin.keys().cloned().collect();
        let mut presentation = apply_patch(host_defaults(), patch);
        self.scope_plugin_properties(&mut presentation.plugin, facts.producer.as_deref());
        for key in set_plugin {
            if !presentation.plugin.contains_key(&key) {
                origins.remove(&key);
            }
        }
        let floor = floor_adjustments(&mut presentation, facts)
            .into_iter()
            .map(|adjustment| match origins.get(&adjustment.property) {
                Some(origin) => format!(
                    "{} (overrides rule {} in layer {:?})",
                    adjustment.note, origin.rule, origin.source
                ),
                None => adjustment.note,
            })
            .collect();
        ExplainedPresentation {
            presentation,
            origins: origins.into_values().collect(),
            floor,
        }
    }
}

impl CompiledPolicy {
    /// Keep only the plugin properties that apply to a unit produced by
    /// `producer`.
    ///
    /// A registered property applies only to its declaring plugin's units.
    /// Unregistered properties (already reported as unknown) apply to no unit,
    /// so a rule can never hand one plugin's adapter a property its name
    /// merely resembles, including under nested plugin ids.
    fn scope_plugin_properties(
        &self,
        properties: &mut BTreeMap<String, FactValue>,
        producer: Option<&str>,
    ) {
        if properties.is_empty() {
            return;
        }
        let Some(producer) = producer else {
            properties.clear();
            return;
        };
        properties.retain(|key, _| {
            self.properties
                .get(key)
                .is_some_and(|schema| schema.plugin == producer)
        });
    }
}

/// Whether `facts` describe a failed, timed-out, or errored unit.
#[must_use]
pub const fn unit_failed(facts: &UnitFacts) -> bool {
    facts.is_error
        || matches!(
            facts.status,
            Some(UnitStatus::Failed | UnitStatus::TimedOut)
        )
}

/// Raise `presentation` to the host visibility floor; returns a note for each adjustment.
///
/// * Units awaiting a user decision are always expanded, with at least normal
///   detail and their [`DECISION_SECTIONS`] visible and expanded.
/// * Failed or errored units are never shown below summary, and their
///   [`FAILURE_SECTIONS`] are never hidden.
/// * Group units are never hidden; their failed and awaiting members are
///   revealed instead (see [`UnitGroup::revealed_members`]), so a failed member
///   does not raise the group itself.
///
/// Count markers for hidden units are the renderer's responsibility, driven by
/// [`Disclosure::Hidden`].
pub fn apply_visibility_floor(
    presentation: &mut ResolvedPresentation,
    facts: &UnitFacts,
) -> Vec<String> {
    floor_adjustments(presentation, facts)
        .into_iter()
        .map(|adjustment| adjustment.note)
        .collect()
}

/// One visibility-floor adjustment and the property it overrode.
struct FloorAdjustment {
    /// Property name as reported by `explain` origins.
    property: String,
    note: String,
}

fn floor_adjustments(
    presentation: &mut ResolvedPresentation,
    facts: &UnitFacts,
) -> Vec<FloorAdjustment> {
    let mut adjustments = Vec::new();
    let mut adjust = |property: String, note: String| {
        adjustments.push(FloorAdjustment { property, note });
    };
    if facts.kind == Some(UnitKind::Group) {
        if presentation.disclosure == Disclosure::Hidden {
            adjust(
                "disclosure".to_owned(),
                "group: disclosure raised from Hidden to Collapsed".to_owned(),
            );
            presentation.disclosure = Disclosure::Collapsed;
        }
        return adjustments;
    }
    if facts.awaiting_decision {
        if presentation.disclosure != Disclosure::Expanded {
            adjust(
                "disclosure".to_owned(),
                format!(
                    "awaiting a decision: disclosure raised from {:?} to Expanded",
                    presentation.disclosure
                ),
            );
            presentation.disclosure = Disclosure::Expanded;
        }
        if presentation.detail == Detail::Minimal {
            adjust(
                "detail".to_owned(),
                "awaiting a decision: detail raised from Minimal to Normal".to_owned(),
            );
            presentation.detail = Detail::Normal;
        }
        for name in DECISION_SECTIONS {
            let Some(section) = presentation.sections.get_mut(name) else {
                continue;
            };
            if !section.visible || section.disclosure != Disclosure::Expanded {
                let field = if section.visible {
                    "disclosure"
                } else {
                    "visible"
                };
                adjust(
                    format!("sections.{name}.{field}"),
                    format!("awaiting a decision: section {name} kept visible and expanded"),
                );
                section.visible = true;
                section.disclosure = Disclosure::Expanded;
            }
        }
    }
    if unit_failed(facts) {
        if matches!(
            presentation.disclosure,
            Disclosure::Collapsed | Disclosure::Hidden
        ) {
            adjust(
                "disclosure".to_owned(),
                format!(
                    "failed: disclosure raised from {:?} to Summary",
                    presentation.disclosure
                ),
            );
            presentation.disclosure = Disclosure::Summary;
        }
        for name in FAILURE_SECTIONS {
            let Some(section) = presentation.sections.get_mut(name) else {
                continue;
            };
            if !section.visible || section.disclosure == Disclosure::Hidden {
                let field = if section.visible {
                    "disclosure"
                } else {
                    "visible"
                };
                adjust(
                    format!("sections.{name}.{field}"),
                    format!("failed: section {name} kept visible"),
                );
                section.visible = true;
                if section.disclosure == Disclosure::Hidden {
                    section.disclosure = Disclosure::Collapsed;
                }
            }
        }
    }
    adjustments
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
    for (name, section) in patch.section_patches() {
        presentation
            .sections
            .entry(name.to_owned())
            .or_insert_with(|| ResolvedSection::defaults(presentation.preview))
            .apply(&section);
    }
    if let Some(group) = patch.group {
        presentation.group = group;
    }
    if let Some(reveal) = patch.reveal {
        presentation.reveal = reveal;
    }
    if let Some(placement) = patch.placement {
        presentation.placement = placement;
    }
    if patch.container.is_some() {
        presentation.container = patch.container;
    }
    if patch.header.is_some() {
        presentation.header = patch.header;
    }
    if let Some(mut summary) = patch.summary {
        if let Some(template) = &mut summary.template {
            truncate_to_boundary(template, MAX_SUMMARY_TEMPLATE_BYTES);
        }
        presentation.summary = summary;
    }
    if let Some(spacing) = patch.spacing {
        presentation.spacing = spacing.clamped();
    }
    if let Some(indent) = patch.indent {
        presentation.indent = indent.min(MAX_INDENT);
    }
    presentation.plugin.extend(patch.plugin);
    presentation
}

/// Truncate `text` to at most `max` bytes on a character boundary.
fn truncate_to_boundary(text: &mut String, max: usize) {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
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
    if set
        .sections
        .values()
        .filter_map(|section| section.preview)
        .any(|preview| preview.lines > MAX_PREVIEW_LINES)
    {
        return Some(format!("section preview exceeds {MAX_PREVIEW_LINES} lines"));
    }
    let lists = [
        set.parts.len(),
        set.sections.len(),
        set.plugin.len(),
        set.header.as_ref().map_or(0, |header| header.fields.len()),
        set.summary
            .as_ref()
            .map_or(0, |summary| summary.source.len()),
        set.group.as_ref().map_or(0, |group| group.bridge.len()),
        set.reveal.as_ref().map_or(0, Vec::len),
    ];
    if lists.into_iter().any(|len| len > MAX_PATCH_ENTRIES) {
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
        if !registry.properties.contains_key(key) {
            warnings.push(format!("unknown plugin property {key:?}"));
        }
    }
    for (key, condition) in &rule.matcher.plugin {
        match registry.facts.get(key) {
            None => warnings.push(format!("unknown plugin fact {key:?}")),
            Some(schema) => warnings.extend(fact_condition_warning(key, schema, condition)),
        }
    }
    if !registry.parts.is_empty() {
        for key in rule.set.parts.keys() {
            if !key.contains('.') && !registry.parts.contains(key) {
                warnings.push(format!("unknown part {key:?}"));
            }
        }
        for key in rule.set.sections.keys() {
            if !key.contains('.') && !registry.parts.contains(key) {
                warnings.push(format!("unknown section {key:?}"));
            }
        }
    }
    if let Some(header) = &rule.set.header {
        let kinds = &rule.matcher.kind.0;
        let groups_only = !kinds.is_empty() && kinds.iter().all(|kind| *kind == UnitKind::Group);
        let units_only = !kinds.is_empty() && !kinds.contains(&UnitKind::Group);
        for field in &header.fields {
            let field = field.as_str();
            if !HEADER_FIELDS.contains(&field) {
                warnings.push(format!("unknown header field {field:?}"));
            } else if groups_only && !GROUP_HEADER_FIELDS.contains(&field) {
                warnings.push(format!(
                    "header field {field:?} does not apply to group units"
                ));
            } else if units_only && !UNIT_HEADER_FIELDS.contains(&field) {
                warnings.push(format!(
                    "header field {field:?} applies only to group units"
                ));
            }
        }
    }
    if let Some(template) = rule
        .set
        .summary
        .as_ref()
        .and_then(|summary| summary.template.as_deref())
    {
        if template.len() > MAX_SUMMARY_TEMPLATE_BYTES {
            warnings.push(format!(
                "summary template exceeds {MAX_SUMMARY_TEMPLATE_BYTES} bytes and is truncated"
            ));
        }
        for token in template_tokens(template) {
            if let TemplateToken::Field(name) = token
                && !TEMPLATE_FIELDS.contains(&name)
            {
                warnings.push(format!("unknown summary template field {name:?}"));
            }
        }
    }
    warnings.extend(floor_crossings(rule));
    if rule
        .set
        .spacing
        .is_some_and(|spacing| spacing != spacing.clamped())
    {
        warnings.push(format!("spacing is clamped to {}", models::MAX_SPACING));
    }
    if rule.set.indent.is_some_and(|indent| indent > MAX_INDENT) {
        warnings.push(format!("indent is clamped to {MAX_INDENT}"));
    }
    if rule.set.is_empty() {
        warnings.push("rule sets no properties".to_owned());
    }
    warnings
}

/// Warning for a condition that can never match fact `key` of `schema`'s type.
fn fact_condition_warning(
    key: &str,
    schema: &PluginFactSchema,
    condition: &FactCondition,
) -> Option<String> {
    let type_name = match schema.value_type {
        FactType::Bool => "bool",
        FactType::Int => "int",
        FactType::String => "string",
    };
    match condition {
        FactCondition::Range(_) if schema.value_type != FactType::Int => Some(format!(
            "plugin fact {key:?} has type {type_name}; a range never matches it"
        )),
        FactCondition::Values(values) => values
            .0
            .iter()
            .find(|value| !schema.value_type.admits(value))
            .map(|value| {
                format!("plugin fact {key:?} has type {type_name}; {value:?} never matches it")
            }),
        FactCondition::Range(_) => None,
    }
}

/// Warnings for patches the visibility floor will partly undo.
///
/// Only rules that explicitly target the protected units are reported; a rule
/// that matches every unit is not, since the floor affects only some of them.
fn floor_crossings(rule: &PresentationRule) -> Vec<String> {
    let mut warnings = Vec::new();
    let matcher = &rule.matcher;
    let set = &rule.set;
    if matcher.kind.0.contains(&UnitKind::Group) && set.disclosure == Some(Disclosure::Hidden) {
        warnings
            .push("rule hides group units; the visibility floor keeps groups collapsed".to_owned());
    }
    let targets_failures = matcher.is_error == Some(true)
        || (!matcher.status.is_unset()
            && matcher
                .status
                .0
                .iter()
                .all(|status| matches!(status, UnitStatus::Failed | UnitStatus::TimedOut)));
    if !targets_failures {
        return warnings;
    }
    if matches!(
        set.disclosure,
        Some(Disclosure::Collapsed | Disclosure::Hidden)
    ) {
        warnings.push(
            "rule collapses failed units; the visibility floor keeps them at summary".to_owned(),
        );
    }
    for (name, section) in set.section_patches() {
        if FAILURE_SECTIONS.contains(&name)
            && (section.visible == Some(false) || section.disclosure == Some(Disclosure::Hidden))
        {
            warnings.push(format!(
                "rule hides section {name} of failed units; the visibility floor keeps it visible"
            ));
        }
    }
    warnings
}

/// Longest placeholder name recognized in a summary template.
const MAX_TEMPLATE_FIELD_NAME: usize = 32;

/// One piece of a summary template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TemplateToken<'a> {
    /// Text copied as is.
    Literal(&'a str),
    /// A `{name}` placeholder.
    Field(&'a str),
}

/// Split a summary template into literals and `{name}` placeholders.
///
/// A placeholder name is 1 to 32 ASCII letters, digits, or underscores; any
/// other brace is literal text.
fn template_tokens(template: &str) -> impl Iterator<Item = TemplateToken<'_>> {
    let mut rest = template;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        if let Some(after) = rest.strip_prefix('{') {
            if let Some(end) = after.find('}') {
                let name = &after[..end];
                let valid = (1..=MAX_TEMPLATE_FIELD_NAME).contains(&name.len())
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
                if valid {
                    rest = &after[end + 1..];
                    return Some(TemplateToken::Field(name));
                }
            }
            let (brace, after) = rest.split_at(1);
            rest = after;
            return Some(TemplateToken::Literal(brace));
        }
        let end = rest.find('{').unwrap_or(rest.len());
        let (literal, after) = rest.split_at(end);
        rest = after;
        Some(TemplateToken::Literal(literal))
    })
}

/// Append `text` to `out` with control characters as spaces; returns `false`
/// once `out` reached [`MAX_RENDERED_SUMMARY_BYTES`].
fn push_single_line(out: &mut String, text: &str) -> bool {
    for character in text.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        if out.len() + character.len_utf8() > MAX_RENDERED_SUMMARY_BYTES {
            return false;
        }
        out.push(character);
    }
    true
}

/// Render a summary `template`, substituting `{name}` placeholders.
///
/// Only registered names ([`TEMPLATE_FIELDS`]) substitute; unregistered names
/// and names for which `lookup` returns `None` render empty. The result is a
/// single line: line breaks and other control characters become spaces, and
/// surrounding whitespace is trimmed. Work is bounded: at most
/// [`MAX_SUMMARY_TEMPLATE_BYTES`] of the template are read and the output is
/// cut to [`MAX_RENDERED_SUMMARY_BYTES`] on a character boundary.
#[must_use]
pub fn render_summary_template(template: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut end = template.len().min(MAX_SUMMARY_TEMPLATE_BYTES);
    while !template.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::new();
    for token in template_tokens(&template[..end]) {
        let complete = match token {
            TemplateToken::Literal(text) => push_single_line(&mut out, text),
            TemplateToken::Field(name) => {
                !TEMPLATE_FIELDS.contains(&name)
                    || lookup(name).is_none_or(|value| push_single_line(&mut out, &value))
            }
        };
        if !complete {
            break;
        }
    }
    let trimmed = out.trim();
    if trimmed.len() == out.len() {
        out
    } else {
        trimmed.to_owned()
    }
}

fn set_properties(patch: &PropertyPatch) -> Vec<String> {
    let mut properties = Vec::new();
    for (set, name) in [
        (patch.disclosure.is_some(), "disclosure"),
        (patch.detail.is_some(), "detail"),
        (patch.preview.is_some(), "preview"),
        (patch.group.is_some(), "group"),
        (patch.reveal.is_some(), "reveal"),
        (patch.placement.is_some(), "placement"),
        (patch.container.is_some(), "container"),
        (patch.header.is_some(), "header"),
        (patch.summary.is_some(), "summary"),
        (patch.spacing.is_some(), "spacing"),
        (patch.indent.is_some(), "indent"),
    ] {
        if set {
            properties.push(name.to_owned());
        }
    }
    for (name, section) in patch.section_patches() {
        for (set, field) in [
            (section.visible.is_some(), "visible"),
            (section.disclosure.is_some(), "disclosure"),
            (section.preview.is_some(), "preview"),
        ] {
            if set {
                properties.push(format!("sections.{name}.{field}"));
            }
        }
    }
    properties.extend(patch.plugin.keys().cloned());
    properties
}

/// Merge two ascending, disjoint position sequences into one ascending sequence.
fn merge_sorted(
    left: impl Iterator<Item = usize>,
    right: impl Iterator<Item = usize>,
) -> impl Iterator<Item = usize> {
    let mut left = left.peekable();
    let mut right = right.peekable();
    std::iter::from_fn(move || match (left.peek(), right.peek()) {
        (Some(a), Some(b)) if a <= b => left.next(),
        (_, Some(_)) => right.next(),
        (Some(_), None) => left.next(),
        (None, None) => None,
    })
}

fn fingerprint(rules: &[CompiledRule], properties: &BTreeMap<String, PluginPropertySchema>) -> u64 {
    // FNV-1a over a canonical debug encoding: stable across runs and platforms.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |text: String| {
        for byte in text.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for compiled in rules {
        feed(format!("{:?}|{:?}", compiled.source, compiled.rule));
    }
    if !properties.is_empty() {
        feed(format!("{properties:?}"));
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
            properties: BTreeMap::from([(
                "bcode.shell.show_exit_code".to_owned(),
                PluginPropertySchema {
                    plugin: "bcode.shell".to_owned(),
                    value_type: models::PropertyType::Bool,
                    default: FactValue::Bool(true),
                },
            )]),
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

    fn git_registry() -> PolicyRegistry {
        PolicyRegistry::with_host_parts().with_plugin_schemas(&PluginPresentationSchemas {
            facts: BTreeMap::from([(
                "bcode.git.files".to_owned(),
                PluginFactSchema {
                    plugin: "bcode.git".to_owned(),
                    value_type: FactType::Int,
                },
            )]),
            properties: BTreeMap::from([
                (
                    "bcode.git.show_stats".to_owned(),
                    PluginPropertySchema {
                        plugin: "bcode.git".to_owned(),
                        value_type: models::PropertyType::Bool,
                        default: FactValue::Bool(true),
                    },
                ),
                (
                    "bcode.git.max_files".to_owned(),
                    PluginPropertySchema {
                        plugin: "bcode.git".to_owned(),
                        value_type: models::PropertyType::Int { min: 1, max: 20 },
                        default: FactValue::Int(5),
                    },
                ),
            ]),
        })
    }

    fn produced(name: &str, producer: &str) -> UnitFacts {
        UnitFacts {
            producer: Some(producer.to_owned()),
            ..tool(name, UnitStatus::Succeeded)
        }
    }

    #[test]
    fn plugin_properties_apply_only_to_their_plugins_units() {
        let (policy, diagnostics) = CompiledPolicy::compile(
            &[
                layer(
                    LayerSource::Plugin("bcode.git".into()),
                    "[[rules]]\nset = { plugin = { \"bcode.git.show_stats\" = true, \"bcode.git.max_files\" = 5 } }",
                ),
                layer(
                    LayerSource::Theme("compact".into()),
                    r#"
                    [[rules]]
                    set = { plugin = { "bcode.git.show_stats" = false, "bcode.shell.wrap" = true } }
                    "#,
                ),
            ],
            &git_registry(),
        );
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["unknown plugin property \"bcode.shell.wrap\""]
        );
        let git = policy.resolve(&produced("git.clone", "bcode.git"));
        assert_eq!(
            git.plugin,
            BTreeMap::from([
                ("bcode.git.max_files".to_owned(), FactValue::Int(5)),
                ("bcode.git.show_stats".to_owned(), FactValue::Bool(false)),
            ])
        );
        // Neither another plugin's units nor units without a producer get
        // git properties, and the undeclared shell property reaches no unit.
        let shell = policy.resolve(&produced("bash", "bcode.shell"));
        assert_eq!(shell.plugin, BTreeMap::new());
        assert_eq!(
            policy.resolve(&tool("read", UnitStatus::Succeeded)).plugin,
            BTreeMap::new()
        );
        // The memo never hands one producer's properties to another.
        assert_eq!(policy.resolve(&produced("git.clone", "bcode.git")), git);

        let explained = policy.explain(&produced("git.clone", "bcode.git"));
        let origin = |property: &str| {
            explained
                .origins
                .iter()
                .find(|origin| origin.property == property)
                .map(|origin| origin.source.clone())
        };
        assert_eq!(
            origin("bcode.git.show_stats"),
            Some(LayerSource::Theme("compact".into()))
        );
        assert_eq!(
            origin("bcode.git.max_files"),
            Some(LayerSource::Plugin("bcode.git".into()))
        );
        assert_eq!(
            origin("bcode.shell.wrap"),
            None,
            "dropped values have no origin"
        );
    }

    #[test]
    fn invalid_plugin_property_values_warn_and_are_ignored() {
        let (policy, diagnostics) = CompiledPolicy::compile(
            &[
                layer(
                    LayerSource::Plugin("bcode.git".into()),
                    "[[rules]]\nset = { plugin = { \"bcode.git.max_files\" = 5 } }",
                ),
                layer(
                    LayerSource::Config,
                    r#"
                    [[rules]]
                    set = { plugin = { "bcode.git.max_files" = 99, "bcode.git.show_stats" = "yes" } }
                    "#,
                ),
            ],
            &git_registry(),
        );
        let warnings: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| (diagnostic.source.clone(), diagnostic.message.as_str()))
            .collect();
        assert_eq!(
            warnings,
            [
                (
                    LayerSource::Config,
                    "plugin property \"bcode.git.max_files\" ignored: 99 is outside 1..=20"
                ),
                (
                    LayerSource::Config,
                    "plugin property \"bcode.git.show_stats\" ignored: expected a bool"
                ),
            ]
        );
        let git = policy.resolve(&produced("git.clone", "bcode.git"));
        assert_eq!(
            git.plugin.get("bcode.git.max_files"),
            Some(&FactValue::Int(5))
        );
        assert_eq!(git.plugin.get("bcode.git.show_stats"), None);
    }

    #[test]
    fn plugin_fact_conditions_are_checked_against_declared_types() {
        let (policy, diagnostics) = CompiledPolicy::compile(
            &[layer(
                LayerSource::Theme("t".into()),
                r#"
                [[rules]]
                match = { plugin = { "bcode.git.files" = { gt = 10 } } }
                set = { disclosure = "collapsed" }

                [[rules]]
                match = { plugin = { "bcode.git.files" = "many" } }
                set = { detail = "minimal" }

                [[rules]]
                match = { plugin = { "bcode.git.lines" = 1 } }
                set = { detail = "verbose" }
                "#,
            )],
            &git_registry(),
        );
        let messages: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();
        assert_eq!(
            messages,
            [
                "plugin fact \"bcode.git.files\" has type int; Text(\"many\") never matches it",
                "unknown plugin fact \"bcode.git.lines\"",
            ]
        );
        let mut facts = produced("git.status", "bcode.git");
        assert_eq!(policy.resolve(&facts).disclosure, Disclosure::Expanded);
        facts
            .plugin
            .insert("bcode.git.files".to_owned(), FactValue::Int(11));
        assert_eq!(policy.resolve(&facts).disclosure, Disclosure::Collapsed);
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
    fn v2_properties_resolve_with_defaults_clamps_and_per_field_sections() {
        let (policy, diagnostics) = CompiledPolicy::compile(
            &[layer(
                LayerSource::Theme("v2".into()),
                r#"
                    [[rules]]
                    match = { kind = "tool" }
                    set = { preview = { lines = 5 }, sections = { output = { disclosure = "collapsed" }, arguments = { visible = false } }, spacing = { before = 9, after = 0 }, indent = 30, summary = { template = "{tool}" } }

                    [[rules]]
                    match = { tool = "read" }
                    set = { parts = { arguments = true }, sections = { output = { preview = { lines = 2 } } }, header = { fields = ["marker", "tool"] }, group = "consecutive_same_tool", reveal = ["failed"] }
                    "#,
            )],
            &PolicyRegistry::with_host_parts(),
        );
        let warnings: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();
        assert_eq!(
            warnings,
            ["spacing is clamped to 2", "indent is clamped to 8"]
        );

        let defaults = policy.resolve(&UnitFacts {
            kind: Some(UnitKind::Reasoning),
            ..UnitFacts::default()
        });
        assert_eq!(defaults, host_defaults());
        assert_eq!(defaults.spacing, Spacing::default());
        assert_eq!(defaults.reveal, models::DEFAULT_REVEAL);
        assert_eq!(
            defaults.section("output"),
            ResolvedSection::defaults(defaults.preview)
        );

        let read = policy.resolve(&tool("read", UnitStatus::Succeeded));
        let output = read.section("output");
        assert_eq!(output.disclosure, Disclosure::Collapsed);
        assert_eq!(output.preview.lines, 2);
        assert!(output.visible);
        assert!(read.part_visible("arguments"));
        assert_eq!(read.section("result").preview.lines, 5);
        assert_eq!(
            read.spacing,
            Spacing {
                before: 2,
                after: 0
            }
        );
        assert_eq!(read.indent, 8);
        assert_eq!(read.header.as_ref().map(|h| h.fields.len()), Some(2));
        assert_eq!(read.group, GroupSpec::new(models::GroupBy::SameTool));
        assert_eq!(read.reveal, [UnitStatus::Failed]);

        let bash = policy.resolve(&tool("bash", UnitStatus::Succeeded));
        assert!(!bash.part_visible("arguments"));
        assert_eq!(bash.section("output").preview.lines, 5);
        assert_eq!(bash.header, None);
        assert_eq!(bash.summary.template.as_deref(), Some("{tool}"));
    }

    #[test]
    fn summary_templates_are_truncated_on_character_boundaries() {
        let template = "é".repeat(200);
        let (policy, diagnostics) = CompiledPolicy::compile(
            &[layer(
                LayerSource::Config,
                &format!("[[rules]]\nset = {{ summary = {{ template = {template:?} }} }}"),
            )],
            &PolicyRegistry::default(),
        );
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        let resolved = policy.resolve(&tool("x", UnitStatus::Running));
        let truncated = resolved.summary.template.unwrap();
        assert_eq!(truncated.len(), MAX_SUMMARY_TEMPLATE_BYTES);
        assert!(template.starts_with(&truncated));
    }

    #[test]
    fn v2_diagnostics_warn_on_unknown_names_and_drop_oversized_sections() {
        let (policy, diagnostics) = CompiledPolicy::compile(
            &[layer(
                LayerSource::Config,
                r#"
                [[rules]]
                set = { sections = { ouptut = { visible = false }, "bcode.git.files" = { visible = false } }, header = { fields = ["marker", "colour"] }, summary = { template = "{tool} {nope}" } }

                [[rules]]
                set = { sections = { output = { preview = { lines = 999999 } } } }
                "#,
            )],
            &PolicyRegistry::with_host_parts(),
        );
        let messages: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| (diagnostic.severity, diagnostic.message.as_str()))
            .collect();
        assert_eq!(
            messages,
            [
                (DiagnosticSeverity::Warning, "unknown section \"ouptut\""),
                (
                    DiagnosticSeverity::Warning,
                    "unknown header field \"colour\""
                ),
                (
                    DiagnosticSeverity::Warning,
                    "unknown summary template field \"nope\""
                ),
                (
                    DiagnosticSeverity::Error,
                    "section preview exceeds 10000 lines"
                ),
            ]
        );
        assert_eq!(policy.len(), 1);
    }

    #[test]
    fn floor_keeps_decision_sections_visible_and_expanded() {
        let (policy, _) = CompiledPolicy::compile(
            &[layer(
                LayerSource::Theme("repo".into()),
                "[[rules]]\nset = { sections = { arguments = { disclosure = \"collapsed\" }, metadata = { visible = false }, output = { visible = false } } }",
            )],
            &PolicyRegistry::with_host_parts(),
        );
        let permission = UnitFacts {
            kind: Some(UnitKind::Permission),
            awaiting_decision: true,
            ..UnitFacts::default()
        };
        let explained = policy.explain(&permission);
        for name in ["arguments", "metadata"] {
            let section = explained.presentation.section(name);
            assert!(section.visible, "{name}");
            assert_eq!(section.disclosure, Disclosure::Expanded, "{name}");
        }
        assert!(!explained.presentation.part_visible("output"));
        assert_eq!(explained.floor.len(), 2, "{:?}", explained.floor);
    }

    #[test]
    fn explain_reports_v2_properties_per_section_field() {
        let (policy, _) = CompiledPolicy::compile(
            &[
                layer(
                    LayerSource::Theme("t".into()),
                    "[[rules]]\nset = { parts = { output = false }, indent = 2, reveal = [\"running\"] }",
                ),
                layer(
                    LayerSource::Config,
                    "[[rules]]\nset = { sections = { output = { disclosure = \"summary\" } }, header = { fields = [] }, summary = {}, spacing = {}, group = \"consecutive_tools\" }",
                ),
            ],
            &PolicyRegistry::default(),
        );
        let explained = policy.explain(&tool("read", UnitStatus::Succeeded));
        let sources: BTreeMap<_, _> = explained
            .origins
            .iter()
            .map(|origin| (origin.property.as_str(), origin.source.clone()))
            .collect();
        let theme = LayerSource::Theme("t".into());
        assert_eq!(
            sources,
            BTreeMap::from([
                ("group", LayerSource::Config),
                ("header", LayerSource::Config),
                ("indent", theme.clone()),
                ("reveal", theme.clone()),
                ("sections.output.disclosure", LayerSource::Config),
                ("sections.output.visible", theme),
                ("spacing", LayerSource::Config),
                ("summary", LayerSource::Config),
            ])
        );
    }

    #[test]
    fn memoized_resolution_matches_fresh_resolution_and_stays_bounded() {
        let (policy, _) = CompiledPolicy::compile(
            &[layer(
                LayerSource::Theme("memo".into()),
                r#"
                [[rules]]
                match = { kind = "tool" }
                set = { disclosure = "hidden", sections = { error = { visible = false }, arguments = { disclosure = "collapsed" } } }

                [[rules]]
                match = { tool = "read" }
                set = { detail = "minimal" }
                "#,
            )],
            &PolicyRegistry::with_host_parts(),
        );
        let mut cases = Vec::new();
        for name in ["read", "bash"] {
            for status in [
                UnitStatus::Succeeded,
                UnitStatus::Failed,
                UnitStatus::Waiting,
            ] {
                for awaiting_decision in [false, true] {
                    cases.push(UnitFacts {
                        awaiting_decision,
                        ..tool(name, status)
                    });
                }
            }
        }
        cases.push(group(Some("read")));
        // Resolve every case twice, interleaved, so later calls hit the memo
        // populated by units that share a rule set but differ in floor facts.
        for _ in 0..2 {
            for facts in &cases {
                assert_eq!(
                    policy.resolve(facts),
                    policy.explain(facts).presentation,
                    "{facts:?}"
                );
            }
        }
        // Distinct (rule set, floor facts) combinations, not units, fill the memo.
        let distinct = policy.memo.lock().unwrap().len();
        assert!(distinct < cases.len());
        // Many distinct rule sets never grow the memo past its bound.
        let layers: Vec<RuleLayer> = (0..3)
            .map(|chunk| {
                let mut rules = String::new();
                for index in 0..MAX_RULES_PER_LAYER / 2 {
                    use std::fmt::Write as _;
                    let indent = index % 4;
                    let _ = writeln!(
                        rules,
                        "[[rules]]\nmatch = {{ tool = \"tool-{chunk}-{index}\" }}\nset = {{ indent = {indent} }}"
                    );
                }
                layer(LayerSource::Config, &rules)
            })
            .collect();
        let (wide, _) = CompiledPolicy::compile(&layers, &PolicyRegistry::with_host_parts());
        for chunk in 0..3 {
            for index in 0..MAX_RULES_PER_LAYER / 2 {
                let facts = tool(&format!("tool-{chunk}-{index}"), UnitStatus::Succeeded);
                assert_eq!(wide.resolve(&facts), wide.explain(&facts).presentation);
                assert!(wide.memo.lock().unwrap().len() <= MAX_RESOLUTION_MEMO);
            }
        }
        // A clone starts with an empty memo and resolves identically.
        let clone = policy.clone();
        assert!(clone.memo.lock().unwrap().is_empty());
        assert_eq!(clone.resolve(&cases[0]), policy.resolve(&cases[0]));
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
        let variants = [
            "set = { group = { by = \"turn\" } }",
            "set = { group = { by = \"turn\", min = 3 } }",
            "set = { group = { by = \"turn\", bridge = [] } }",
            "set = { reveal = [\"failed\"] }",
            "set = { sections = { output = { visible = false } } }",
            "set = { sections = { output = { disclosure = \"collapsed\" } } }",
            "set = { header = { fields = [\"tool\"] } }",
            "set = { summary = { template = \"{tool}\" } }",
            "set = { spacing = { before = 1 } }",
            "set = { indent = 1 }",
            "match = { target = \"a\" }\nset = { indent = 1 }",
            "match = { duration = \"<1s\" }\nset = { indent = 1 }",
            "match = { last_in_turn = true }\nset = { indent = 1 }",
        ];
        let fingerprints: BTreeSet<u64> = variants
            .iter()
            .map(|rule| compile(&format!("[[rules]]\n{rule}")))
            .chain([a])
            .collect();
        assert_eq!(fingerprints.len(), variants.len() + 1);
    }

    fn group(tool: Option<&str>) -> UnitFacts {
        UnitFacts {
            kind: Some(UnitKind::Group),
            tool: tool.map(str::to_owned),
            ..UnitFacts::default()
        }
    }

    #[test]
    fn kind_and_tool_index_matches_only_applicable_rules_in_order() {
        let (policy, _) = CompiledPolicy::compile(
            &[layer(
                LayerSource::Config,
                r#"
                [[rules]]
                match = { kind = "group" }
                set = { disclosure = "collapsed", indent = 1 }

                [[rules]]
                match = { kind = "group", tool = ["read", "read"] }
                set = { disclosure = "summary" }

                [[rules]]
                match = { tool = "read" }
                set = { indent = 3 }

                [[rules]]
                match = { kind = ["tool", "group"], tool = "grep" }
                set = { detail = "verbose" }

                [[rules]]
                match = { kind = "tool" }
                set = { indent = 4 }
                "#,
            )],
            &PolicyRegistry::default(),
        );
        let read_group = policy.resolve(&group(Some("read")));
        assert_eq!(read_group.disclosure, Disclosure::Summary);
        assert_eq!(read_group.indent, 3);
        let mixed_group = policy.resolve(&group(None));
        assert_eq!(mixed_group.disclosure, Disclosure::Collapsed);
        assert_eq!(mixed_group.indent, 1);
        assert_eq!(policy.resolve(&group(Some("grep"))).detail, Detail::Verbose);
        let read_tool = policy.resolve(&tool("read", UnitStatus::Succeeded));
        assert_eq!(read_tool.disclosure, Disclosure::Expanded);
        assert_eq!(read_tool.indent, 4);
        assert_eq!(
            policy.resolve(&tool("grep", UnitStatus::Succeeded)).detail,
            Detail::Verbose
        );
        let explained = policy.explain(&group(Some("read")));
        let rules: Vec<_> = explained.origins.iter().map(|origin| origin.rule).collect();
        assert_eq!(rules, [1, 2], "disclosure from rule 1, indent from rule 2");
    }

    #[test]
    fn groups_are_never_hidden_and_failed_members_do_not_raise_them() {
        let (policy, diagnostics) = CompiledPolicy::compile(
            &[layer(
                LayerSource::Theme("repo".into()),
                "[[rules]]\nmatch = { kind = \"group\" }\nset = { disclosure = \"hidden\" }",
            )],
            &PolicyRegistry::default(),
        );
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["rule hides group units; the visibility floor keeps groups collapsed"]
        );
        let mut failed_group = group(None);
        failed_group.is_error = true;
        failed_group.status = Some(UnitStatus::Failed);
        let explained = policy.explain(&failed_group);
        assert_eq!(explained.presentation.disclosure, Disclosure::Collapsed);
        assert_eq!(
            explained.floor,
            [
                "group: disclosure raised from Hidden to Collapsed (overrides rule 0 in layer Theme(\"repo\"))"
            ]
        );
    }

    #[test]
    fn floor_keeps_failure_sections_visible_and_names_the_rule() {
        let (policy, diagnostics) = CompiledPolicy::compile(
            &[
                layer(
                    LayerSource::Theme("repo".into()),
                    "[[rules]]\nset = { sections = { error = { visible = false }, output = { disclosure = \"hidden\" }, diff = { visible = false } } }",
                ),
                layer(
                    LayerSource::Config,
                    "[[rules]]\nmatch = { status = [\"failed\", \"timed_out\"] }\nset = { disclosure = \"collapsed\", parts = { output = false } }",
                ),
            ],
            &PolicyRegistry::with_host_parts(),
        );
        let warnings: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| (diagnostic.source.clone(), diagnostic.message.as_str()))
            .collect();
        assert_eq!(
            warnings,
            [
                (
                    LayerSource::Config,
                    "rule collapses failed units; the visibility floor keeps them at summary"
                ),
                (
                    LayerSource::Config,
                    "rule hides section output of failed units; the visibility floor keeps it visible"
                ),
            ]
        );
        let explained = policy.explain(&tool("bash", UnitStatus::TimedOut));
        let presentation = &explained.presentation;
        assert_eq!(presentation.disclosure, Disclosure::Summary);
        assert!(presentation.section("error").visible);
        let output = presentation.section("output");
        assert!(output.visible);
        assert_eq!(output.disclosure, Disclosure::Collapsed);
        assert!(
            !presentation.part_visible("diff"),
            "other sections may hide"
        );
        assert_eq!(
            explained.floor,
            [
                "failed: disclosure raised from Collapsed to Summary (overrides rule 0 in layer Config)",
                "failed: section error kept visible (overrides rule 0 in layer Theme(\"repo\"))",
                "failed: section output kept visible (overrides rule 0 in layer Config)",
            ]
        );

        let succeeded = policy.resolve(&tool("bash", UnitStatus::Succeeded));
        assert!(!succeeded.part_visible("error"));
        assert_eq!(succeeded.section("output").disclosure, Disclosure::Hidden);
    }

    #[test]
    fn floor_keeps_confirmation_expanded_while_awaiting() {
        let (policy, _) = CompiledPolicy::compile(
            &[layer(
                LayerSource::Theme("repo".into()),
                "[[rules]]\nset = { sections = { confirmation = { disclosure = \"collapsed\" } } }",
            )],
            &PolicyRegistry::with_host_parts(),
        );
        let interaction = UnitFacts {
            kind: Some(UnitKind::Interaction),
            awaiting_decision: true,
            ..UnitFacts::default()
        };
        let explained = policy.explain(&interaction);
        assert_eq!(
            explained.presentation.section("confirmation").disclosure,
            Disclosure::Expanded
        );
        assert_eq!(
            explained.floor,
            [
                "awaiting a decision: section confirmation kept visible and expanded (overrides rule 0 in layer Theme(\"repo\"))"
            ]
        );
    }

    #[test]
    fn header_fields_are_checked_against_the_targeted_kinds() {
        let (_, diagnostics) = CompiledPolicy::compile(
            &[layer(
                LayerSource::Config,
                r#"
                [[rules]]
                match = { kind = "group" }
                set = { header = { fields = ["marker", "count", "tools", "status", "timing", "icon"] } }

                [[rules]]
                match = { kind = "tool" }
                set = { header = { fields = ["marker", "icon", "label", "tool", "target", "status", "timing", "count", "title", "tools"] } }

                [[rules]]
                match = { kind = ["tool", "group"] }
                set = { header = { fields = ["icon", "tools", "bogus"] } }

                [[rules]]
                set = { header = { fields = ["tools", "title"] } }
                "#,
            )],
            &PolicyRegistry::default(),
        );
        let warnings: Vec<_> = diagnostics
            .iter()
            .map(|diagnostic| (diagnostic.rule, diagnostic.message.as_str()))
            .collect();
        assert_eq!(
            warnings,
            [
                (
                    Some(0),
                    "header field \"icon\" does not apply to group units"
                ),
                (
                    Some(1),
                    "header field \"tools\" applies only to group units"
                ),
                (Some(2), "unknown header field \"bogus\""),
            ]
        );
    }

    #[test]
    fn summary_templates_substitute_only_registered_fields() {
        let lookup = |name: &str| match name {
            "tool" => Some("read".to_owned()),
            "target" => Some("src/lib.rs".to_owned()),
            "first_line" => Some("line one\nline two\ttabbed".to_owned()),
            "nope" => Some("leaked".to_owned()),
            _ => None,
        };
        let render = |template: &str| render_summary_template(template, &lookup);
        assert_eq!(render("{tool} {target}"), "read src/lib.rs");
        assert_eq!(
            render("{tool} {status}"),
            "read",
            "missing values render empty"
        );
        assert_eq!(
            render("{nope}|{tool}"),
            "|read",
            "unregistered names render empty"
        );
        assert_eq!(render("{first_line}"), "line one line two tabbed");
        assert_eq!(render("{ tool } {tool"), "{ tool } {tool");
        assert_eq!(render("{{tool}}"), "{read}");
        assert_eq!(render("é{tool}✓"), "éread✓");
        assert_eq!(render(""), "");

        let wide = |_: &str| Some("界".repeat(1000));
        let long = render_summary_template("{first_line}", &wide);
        assert!(long.len() <= MAX_RENDERED_SUMMARY_BYTES);
        assert!(long.len() > MAX_RENDERED_SUMMARY_BYTES - 3);
        assert!(long.chars().all(|character| character == '界'));

        let template = format!("{}{{tool}}", "x".repeat(MAX_SUMMARY_TEMPLATE_BYTES));
        assert_eq!(
            render(&template),
            "x".repeat(MAX_SUMMARY_TEMPLATE_BYTES),
            "template bytes past the bound are ignored"
        );
    }

    #[test]
    fn template_warnings_agree_with_rendering() {
        let (_, diagnostics) = CompiledPolicy::compile(
            &[layer(
                LayerSource::Config,
                "[[rules]]\nset = { summary = { template = \"{tool} { spaced } {first_line} {colour}\" } }",
            )],
            &PolicyRegistry::default(),
        );
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            ["unknown summary template field \"colour\""]
        );
    }
}
