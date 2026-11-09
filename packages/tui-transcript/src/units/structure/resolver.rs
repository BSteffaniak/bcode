//! Presentation-policy structure resolution over the resident window (U2, U5, A14, A16).
//!
//! [`StructureResolver`] keeps one resolved entry per resident item (facts and policy
//! presentation), re-resolving only items whose revision changed, and only all items when the
//! policy, plugin facts, declared targets or observed turn facts change. Policy resolution is
//! memoized by [`CompiledPolicy`] (at most 256 distinct resolutions, D11). Groups come from the
//! renderer-neutral incremental [`Grouping`], updated over the changed span only. Manual
//! disclosure overrides apply on top, followed by the visibility floor (A16).
//!
//! The result is exposed as item placements (consulted by unit derivation through
//! [`UnitStructure`]), per-unit [`ResolvedUnitPresentation`]s (the `structure` input of
//! `ItemRenderContext`), and deterministic signatures that unit derivation folds into layout
//! revisions.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use bcode_presentation_policy::models::{
    Disclosure, GroupSpec, MAX_INDENT, ResolvedPresentation, Spacing, UnitFacts, UnitKind,
};
use bcode_presentation_policy::{
    CompiledPolicy, GroupRole, Grouping, GroupingInput, UnitGroup, apply_visibility_floor,
    host_defaults, is_revealed,
};
use bcode_session_view_models::{
    TranscriptChange, TranscriptChanges, TranscriptViewItem, TranscriptViewItemId, ViewRevision,
};

use super::disclosure::{DisclosureOverrides, DisclosureTarget};
use super::item_facts::{
    DeclaredTargetFields, NoPluginFacts, PluginFactSource, TurnFacts, extract_target,
    invocation_id, is_attached_contribution, is_historical_request, item_facts, tool_arguments,
    tool_duration_ms,
};
use super::{ItemPlacement, MemberRendering, UnitStructure, starts_turn};
use crate::layout::context::{StableHasher, hash_presentation};
use crate::units::UnitKey;

/// Columns members are indented past their group's own indent.
pub const MEMBER_INDENT: u8 = 2;

/// Inputs of structure resolution supplied by the host.
#[derive(Clone)]
pub struct StructureConfig {
    /// Compiled rule layers (host, plugins, theme, configuration, user state).
    pub policy: Arc<CompiledPolicy>,
    /// Plugin-declared facts.
    pub plugin_facts: Arc<dyn PluginFactSource>,
    /// Producer-declared target argument fields.
    pub target_fields: Arc<DeclaredTargetFields>,
    /// Whether any rule conditions on `last_in_turn`.
    pub uses_last_in_turn: bool,
    /// Whether hidden reasoning renders nothing at all (an explicit user choice applied
    /// before the visibility floor); other hidden units leave a count marker.
    pub reasoning_suppressed: bool,
}

impl Default for StructureConfig {
    fn default() -> Self {
        Self {
            policy: Arc::new(CompiledPolicy::default()),
            plugin_facts: Arc::new(NoPluginFacts),
            target_fields: Arc::new(DeclaredTargetFields::new()),
            uses_last_in_turn: false,
            reasoning_suppressed: false,
        }
    }
}

impl std::fmt::Debug for StructureConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StructureConfig")
            .field("policy", &self.policy.fingerprint())
            .field("plugin_facts", &self.plugin_facts.generation())
            .field("target_fields", &self.target_fields.len())
            .field("uses_last_in_turn", &self.uses_last_in_turn)
            .field("reasoning_suppressed", &self.reasoning_suppressed)
            .finish()
    }
}

impl StructureConfig {
    /// Deterministic identity of the configuration (no pointers): a change re-resolves every
    /// item.
    #[must_use]
    pub fn key(&self) -> u64 {
        let mut hasher = StableHasher::new();
        self.policy.fingerprint().hash(&mut hasher);
        self.plugin_facts.generation().hash(&mut hasher);
        self.target_fields.hash(&mut hasher);
        self.uses_last_in_turn.hash(&mut hasher);
        self.reasoning_suppressed.hash(&mut hasher);
        hasher.finish()
    }
}

/// Resolved structure of one item unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemStructure {
    /// Facts as rules saw them.
    pub facts: UnitFacts,
    /// Effective presentation: policy, manual overrides (unit and sections), visibility
    /// floor, then group adjustments (member indent; header-row members collapse).
    pub presentation: ResolvedPresentation,
    /// How the item renders inside its group, when grouped.
    pub rendering: Option<MemberRendering>,
    /// First member of the item's group, when grouped.
    pub group: Option<TranscriptViewItemId>,
    /// The unit's disclosure target.
    pub target: DisclosureTarget,
    /// Whether the unit may be collapsed (units awaiting a decision may not).
    pub collapsible: bool,
}

/// Resolved structure of one group header unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupStructure {
    /// First member; the group's disclosure target is `Group(leader)`.
    pub leader: TranscriptViewItemId,
    /// Group facts (kind `group`), as rules see them.
    pub facts: UnitFacts,
    /// Effective group presentation (manual override applied; never hidden).
    pub presentation: ResolvedPresentation,
    /// Member item ids in transcript order.
    pub members: Vec<TranscriptViewItemId>,
    /// Member tool names with their counts, in first-appearance order.
    pub tools: Vec<(String, u64)>,
    /// Sum of the finished members' durations, when any is known.
    pub duration_ms: Option<u64>,
    /// The group's disclosure target.
    pub target: DisclosureTarget,
    /// Whether the header is the group's last rendered row: no member, bridged or attached
    /// item renders a unit, so the header carries the group's closing spacing.
    pub closes: bool,
}

/// Resolved structure of one unit; the `structure` input of `ItemRenderContext`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedUnitPresentation {
    /// A transcript item unit.
    Item(ItemStructure),
    /// A group header unit.
    Group(GroupStructure),
    /// A hidden-items count marker.
    HiddenMarker,
    /// A resident-window boundary row.
    Boundary,
}

impl ResolvedUnitPresentation {
    /// Deterministic hash of everything layout-affecting in this structure (L3).
    #[must_use]
    pub fn layout_hash(&self) -> u64 {
        let mut hasher = StableHasher::new();
        match self {
            Self::Item(item) => {
                1u8.hash(&mut hasher);
                hash_presentation(&item.presentation, &mut hasher);
                hash_rendered_facts(&item.facts, &mut hasher);
                item.rendering.hash(&mut hasher);
                item.collapsible.hash(&mut hasher);
            }
            Self::Group(group) => {
                2u8.hash(&mut hasher);
                hash_presentation(&group.presentation, &mut hasher);
                hash_group_facts(&group.facts, &mut hasher);
                group.members.len().hash(&mut hasher);
                group.tools.hash(&mut hasher);
                group.duration_ms.hash(&mut hasher);
                group.closes.hash(&mut hasher);
            }
            Self::HiddenMarker => 3u8.hash(&mut hasher),
            Self::Boundary => 4u8.hash(&mut hasher),
        }
        hasher.finish()
    }

    /// Effective disclosure of the unit (hidden markers and boundaries are expanded).
    #[must_use]
    pub const fn disclosure(&self) -> Disclosure {
        match self {
            Self::Item(item) => item.presentation.disclosure,
            Self::Group(group) => group.presentation.disclosure,
            Self::HiddenMarker | Self::Boundary => Disclosure::Expanded,
        }
    }

    /// Disclosure target of the unit, if it has one.
    #[must_use]
    pub const fn target(&self) -> Option<&DisclosureTarget> {
        match self {
            Self::Item(item) => Some(&item.target),
            Self::Group(group) => Some(&group.target),
            Self::HiddenMarker | Self::Boundary => None,
        }
    }
}

/// Facts producers render for an item (header and summary fields, status styling). Facts
/// that only steer rules (turn recency, size buckets, plugin facts) reach layout through the
/// presentation they resolve to, so they stay out of the hash.
fn hash_rendered_facts(facts: &UnitFacts, hasher: &mut StableHasher) {
    facts.tool.hash(hasher);
    facts.target.hash(hasher);
    facts.status.hash(hasher);
    facts.is_error.hash(hasher);
    facts.awaiting_decision.hash(hasher);
}

fn hash_group_facts(facts: &UnitFacts, hasher: &mut StableHasher) {
    facts.status.hash(hasher);
    facts.is_error.hash(hasher);
    facts.awaiting_decision.hash(hasher);
    facts.tool.hash(hasher);
}

/// Items whose structure changed in one update.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StructureUpdate {
    /// Items whose placement or effective presentation changed (for a group header, its first
    /// item).
    pub changed: BTreeSet<TranscriptViewItemId>,
    /// Group membership, group presentation or placements outside the changed items moved, so
    /// unit derivation must run in full.
    pub full: bool,
}

impl StructureUpdate {
    /// Whether nothing changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && !self.full
    }

    /// `changes` extended with the structure changes, for unit derivation: every changed item
    /// as a revision (so its unit takes a new layout revision), plus a reset marker when
    /// derivation must run in full.
    #[must_use]
    pub fn augment(
        &self,
        changes: &TranscriptChanges,
        revision_of: &dyn Fn(&TranscriptViewItemId) -> Option<ViewRevision>,
    ) -> TranscriptChanges {
        if self.is_empty() {
            return changes.clone();
        }
        let mut augmented = changes.clone();
        let revised = changes
            .changes
            .iter()
            .filter_map(|change| match change {
                TranscriptChange::Revised { id, .. } => Some(id),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        for id in &self.changed {
            if revised.contains(id) {
                continue;
            }
            if let Some(revision) = revision_of(id) {
                augmented.changes.push(TranscriptChange::Revised {
                    id: id.clone(),
                    revision,
                });
            }
        }
        if self.full {
            augmented.changes.push(TranscriptChange::Reset);
        }
        augmented
    }
}

/// One resident item as resolution last saw it.
#[derive(Debug, Clone)]
struct Entry {
    id: TranscriptViewItemId,
    revision: ViewRevision,
    /// Stable string identity for grouping.
    key: String,
    turn: u64,
    facts: UnitFacts,
    /// Policy presentation, visibility floor applied, before overrides.
    base: ResolvedPresentation,
    duration_ms: Option<u64>,
    invocation: Option<String>,
    attached: bool,
    request: bool,
    superseded: bool,
}

impl Entry {
    fn represents_invocation(&self) -> bool {
        !self.attached && !self.request && self.facts.kind == Some(UnitKind::Tool)
    }

    fn same_inputs(&self, other: &Self) -> bool {
        self.turn == other.turn
            && self.facts == other.facts
            && self.base == other.base
            && self.invocation == other.invocation
            && self.attached == other.attached
            && self.duration_ms == other.duration_ms
    }
}

/// Turn identity of items without an output location before any user message.
const UNKNOWN_TURN: u64 = u64::MAX;

fn stable_str_hash(text: &str) -> u64 {
    let mut hasher = StableHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Incremental structure resolution over the resident window.
#[derive(Debug)]
pub struct StructureResolver {
    config: StructureConfig,
    config_key: Option<u64>,
    overrides: DisclosureOverrides,
    placed_overrides: Option<u64>,
    turns: TurnFacts,
    entries: Vec<Entry>,
    positions: BTreeMap<TranscriptViewItemId, usize>,
    grouping: Grouping,
    placements: BTreeMap<TranscriptViewItemId, ItemPlacement>,
    items: BTreeMap<TranscriptViewItemId, ResolvedUnitPresentation>,
    groups: BTreeMap<TranscriptViewItemId, ResolvedUnitPresentation>,
    /// Group header key (first item of the group run) by group leader.
    group_heads: BTreeMap<TranscriptViewItemId, TranscriptViewItemId>,
    signatures: BTreeMap<TranscriptViewItemId, u64>,
    group_signatures: BTreeMap<TranscriptViewItemId, u64>,
    hidden: ResolvedUnitPresentation,
    boundary: ResolvedUnitPresentation,
    fallback: ResolvedUnitPresentation,
}

impl Default for StructureResolver {
    fn default() -> Self {
        Self::new(StructureConfig::default())
    }
}

impl StructureResolver {
    /// A resolver with `config` and no resident items.
    #[must_use]
    pub fn new(config: StructureConfig) -> Self {
        Self {
            config,
            config_key: None,
            overrides: DisclosureOverrides::new(),
            placed_overrides: None,
            turns: TurnFacts::default(),
            entries: Vec::new(),
            positions: BTreeMap::new(),
            grouping: Grouping::default(),
            placements: BTreeMap::new(),
            items: BTreeMap::new(),
            groups: BTreeMap::new(),
            group_heads: BTreeMap::new(),
            signatures: BTreeMap::new(),
            group_signatures: BTreeMap::new(),
            hidden: ResolvedUnitPresentation::HiddenMarker,
            boundary: ResolvedUnitPresentation::Boundary,
            fallback: ResolvedUnitPresentation::Item(ItemStructure {
                facts: UnitFacts::default(),
                presentation: host_defaults(),
                rendering: None,
                group: None,
                target: DisclosureTarget::Unit(TranscriptViewItemId::new("")),
                collapsible: true,
            }),
        }
    }

    /// Current configuration.
    #[must_use]
    pub const fn config(&self) -> &StructureConfig {
        &self.config
    }

    /// Replace the configuration; every item is re-resolved by the next [`Self::update`].
    pub fn set_config(&mut self, config: StructureConfig) {
        self.config = config;
    }

    /// Manual disclosure overrides.
    #[must_use]
    pub const fn overrides(&self) -> &DisclosureOverrides {
        &self.overrides
    }

    /// Bring resolution up to date with the document after a transcript operation.
    ///
    /// `changes` is the operation's item-level change set and `items` the document after it.
    /// Revision-only changes re-resolve only the revised items, and regroup and re-place only
    /// when their facts, presentation or grouping inputs changed. Membership changes splice
    /// the resolved entries (unchanged items keep their resolution) and regroup over the
    /// changed span. Overrides whose items left the window are forgotten (D12).
    pub fn update(
        &mut self,
        changes: &TranscriptChanges,
        items: &[TranscriptViewItem],
    ) -> StructureUpdate {
        let key = self.config.key();
        if self.config_key != Some(key) {
            self.config_key = Some(key);
            return self.rebuild(items);
        }
        if changes.changes.is_empty()
            && items.len() == self.entries.len()
            && self.placed_overrides == Some(self.overrides.revision())
        {
            return StructureUpdate::default();
        }
        let revision_only = !changes.changes.is_empty()
            && changes
                .changes
                .iter()
                .all(|change| matches!(change, TranscriptChange::Revised { .. }))
            && items.len() == self.entries.len();
        let mut regroup = None;
        let mut touched = BTreeSet::new();
        if revision_only {
            for change in &changes.changes {
                let TranscriptChange::Revised { id, .. } = change else {
                    continue;
                };
                let Some(&position) = self.positions.get(id) else {
                    return self.rebuild(items);
                };
                let Some(item) = items.get(position).filter(|item| &item.id == id) else {
                    return self.rebuild(items);
                };
                let mut fresh = self.entry(item, self.entries[position].turn);
                fresh.superseded = self.entries[position].superseded;
                let inputs_changed = !self.entries[position].same_inputs(&fresh);
                self.entries[position] = fresh;
                if inputs_changed {
                    touched.insert(position);
                }
            }
            if touched.is_empty() && self.placed_overrides == Some(self.overrides.revision()) {
                return StructureUpdate::default();
            }
            if !touched.is_empty() {
                let reclassified = mark_superseded(&mut self.entries);
                let start = touched.first().copied().unwrap_or(0);
                let end = touched.last().map_or(0, |last| last + 1);
                let start = reclassified.map_or(start, |first| first.min(start));
                let end = reclassified.map_or(end, |first| end.max(first + 1));
                regroup = Some(start..end);
            }
        } else if !changes.changes.is_empty() || items.len() != self.entries.len() {
            let turns = TurnFacts::from_items(items);
            let observed = (self.config.policy.uses_turn_facts()
                && !turns.same_recency(&self.turns))
                || (self.config.uses_last_in_turn && !turns.same_last_in_turn(&self.turns));
            self.turns = turns;
            if observed {
                return self.rebuild(items);
            }
            match self.splice(items) {
                Some(dirty) => regroup = Some(dirty),
                None if self.placed_overrides == Some(self.overrides.revision()) => {
                    return StructureUpdate::default();
                }
                None => {}
            }
        }
        if let Some(dirty) = regroup {
            let inputs = inputs(&self.entries);
            self.grouping.update(&inputs, dirty);
        }
        self.place()
    }

    /// Re-resolve and regroup every item from scratch.
    fn rebuild(&mut self, items: &[TranscriptViewItem]) -> StructureUpdate {
        self.turns = TurnFacts::from_items(items);
        let turns = turn_identities(items);
        self.entries = items
            .iter()
            .zip(turns)
            .map(|(item, turn)| self.entry(item, turn))
            .collect();
        mark_superseded(&mut self.entries);
        self.positions = positions(&self.entries);
        self.grouping = Grouping::compute(&inputs(&self.entries));
        let mut update = self.place();
        update.full = true;
        update
    }

    /// Splice changed entries by identity and revision; returns the dirty range of the new
    /// entries, or `None` when nothing changed.
    fn splice(&mut self, items: &[TranscriptViewItem]) -> Option<std::ops::Range<usize>> {
        let old_len = self.entries.len();
        let new_len = items.len();
        let common = old_len.min(new_len);
        let same = |entry: &Entry, item: &TranscriptViewItem| {
            entry.id == item.id && entry.revision == item.revision
        };
        let prefix = (0..common)
            .take_while(|&index| same(&self.entries[index], &items[index]))
            .count();
        let suffix = (0..common - prefix)
            .take_while(|&back| {
                same(
                    &self.entries[old_len - 1 - back],
                    &items[new_len - 1 - back],
                )
            })
            .count();
        if prefix + suffix == old_len && old_len == new_len {
            return None;
        }
        let dirty = prefix..new_len - suffix;
        let turns = turn_identities(items);
        let mut fresh = dirty
            .clone()
            .map(|index| self.entry(&items[index], turns[index]))
            .collect::<Vec<_>>();
        let replaced = prefix..old_len - suffix;
        for (new, old) in fresh.iter_mut().zip(&self.entries[replaced.clone()]) {
            if new.id == old.id {
                new.superseded = old.superseded;
            }
        }
        self.entries.splice(replaced, fresh);
        // Turn identities of surviving items can change when a user message is inserted
        // before them (a prepend completing a partial turn).
        let mut start = dirty.start;
        let mut end = dirty.end;
        for (index, turn) in turns.iter().enumerate() {
            if self.entries[index].turn != *turn {
                self.entries[index].turn = *turn;
                start = start.min(index);
                end = end.max(index + 1);
            }
        }
        if let Some(first) = mark_superseded(&mut self.entries) {
            start = start.min(first);
            end = end.max(first + 1);
        }
        self.positions = positions(&self.entries);
        Some(start..end)
    }

    fn entry(&self, item: &TranscriptViewItem, turn: u64) -> Entry {
        let mut facts = item_facts(item);
        facts.turn = self.turns.recency(item);
        facts.last_in_turn = self.turns.last_in_turn(item);
        if let Some(fields) = facts
            .tool
            .as_ref()
            .and_then(|tool| self.config.target_fields.get(tool))
            .and_then(|declared| match &facts.producer {
                Some(producer) => declared.get(producer),
                None if declared.len() == 1 => declared.values().next(),
                None => None,
            })
            && let Some(target) = tool_arguments(item)
                .and_then(|arguments| extract_target(arguments, fields.iter().map(String::as_str)))
        {
            facts.target = Some(target);
        }
        if let Some(producer) = facts.producer.clone() {
            facts.plugin = self
                .config
                .plugin_facts
                .facts(item, &producer, facts.tool.as_deref());
        }
        let base = self.config.policy.resolve(&facts);
        Entry {
            id: item.id.clone(),
            revision: item.revision,
            key: item.id.get().to_owned(),
            turn,
            duration_ms: match &item.kind {
                bcode_session_view_models::TranscriptViewItemKind::ToolInvocation { tool }
                | bcode_session_view_models::TranscriptViewItemKind::ToolRequest { tool } => {
                    tool_duration_ms(tool)
                }
                _ => None,
            },
            invocation: invocation_id(item).map(ToOwned::to_owned),
            attached: is_attached_contribution(item),
            request: is_historical_request(item),
            superseded: false,
            facts,
            base,
        }
    }

    /// Recompute placements, effective presentations and signatures; returns the items whose
    /// placement or signature changed.
    fn place(&mut self) -> StructureUpdate {
        let resident = self.entries.iter().map(|entry| &entry.id).collect();
        self.overrides.reconcile(&resident);
        self.placed_overrides = Some(self.overrides.revision());

        let mut placements = BTreeMap::new();
        let mut structures = BTreeMap::new();
        let mut groups = BTreeMap::new();
        let mut group_heads = BTreeMap::new();
        let mut group_signatures = BTreeMap::new();
        for group in self.grouping.groups() {
            self.place_group(
                group,
                &mut placements,
                &mut structures,
                &mut groups,
                &mut group_heads,
                &mut group_signatures,
            );
        }
        for entry in &self.entries {
            if placements.contains_key(&entry.id) {
                continue;
            }
            let presentation = self.effective(entry);
            let placement = if presentation.disclosure == Disclosure::Hidden {
                if self.config.reasoning_suppressed && entry.facts.kind == Some(UnitKind::Reasoning)
                {
                    ItemPlacement::Suppressed
                } else {
                    ItemPlacement::Hidden
                }
            } else {
                ItemPlacement::Visible
            };
            placements.insert(entry.id.clone(), placement);
            structures.insert(
                entry.id.clone(),
                ResolvedUnitPresentation::Item(ItemStructure {
                    facts: entry.facts.clone(),
                    presentation,
                    rendering: None,
                    group: None,
                    target: DisclosureTarget::Unit(entry.id.clone()),
                    collapsible: !entry.facts.awaiting_decision,
                }),
            );
        }
        let signatures = structures
            .iter()
            .map(|(id, structure)| (id.clone(), structure.layout_hash()))
            .collect::<BTreeMap<_, _>>();

        let mut update = StructureUpdate::default();
        for entry in &self.entries {
            let id = &entry.id;
            if placements.get(id) != self.placements.get(id) {
                update.changed.insert(id.clone());
                if matches!(placements.get(id), Some(ItemPlacement::Grouped { .. }))
                    || matches!(self.placements.get(id), Some(ItemPlacement::Grouped { .. }))
                {
                    update.full = true;
                }
            }
            if signatures.get(id) != self.signatures.get(id) {
                update.changed.insert(id.clone());
            }
            if group_signatures.get(id) != self.group_signatures.get(id) {
                update.changed.insert(id.clone());
                update.full = true;
            }
        }
        self.placements = placements;
        self.items = structures;
        self.groups = groups;
        self.group_heads = group_heads;
        self.signatures = signatures;
        self.group_signatures = group_signatures;
        update
    }

    /// The entry's own presentation with its unit and section overrides applied, then the
    /// visibility floor (the floor applies after manual toggles, A16).
    fn effective(&self, entry: &Entry) -> ResolvedPresentation {
        let mut presentation = entry.base.clone();
        let unit_override = self
            .overrides
            .get(&DisclosureTarget::Unit(entry.id.clone()));
        if let Some(disclosure) = unit_override {
            presentation.disclosure = disclosure;
        }
        let mut overridden = unit_override.is_some();
        for (name, disclosure) in self.overrides.sections(&entry.id) {
            let mut section = presentation.section(name);
            section.disclosure = disclosure;
            presentation.sections.insert(name.to_owned(), section);
            overridden = true;
        }
        if overridden {
            apply_visibility_floor(&mut presentation, &entry.facts);
        }
        presentation
    }

    #[allow(clippy::too_many_lines)]
    fn place_group(
        &self,
        group: &UnitGroup,
        placements: &mut BTreeMap<TranscriptViewItemId, ItemPlacement>,
        structures: &mut BTreeMap<TranscriptViewItemId, ResolvedUnitPresentation>,
        groups: &mut BTreeMap<TranscriptViewItemId, ResolvedUnitPresentation>,
        group_heads: &mut BTreeMap<TranscriptViewItemId, TranscriptViewItemId>,
        group_signatures: &mut BTreeMap<TranscriptViewItemId, u64>,
    ) {
        let leader = self.entries[group.leader()].id.clone();
        let head = self.entries[group.start()].id.clone();
        let mut presentation = self.config.policy.resolve(&group.facts);
        let target = DisclosureTarget::Group(leader.clone());
        // Groups are never hidden; a hidden choice collapses to the header.
        let disclosure = match self
            .overrides
            .get(&target)
            .unwrap_or(presentation.disclosure)
        {
            Disclosure::Hidden => Disclosure::Collapsed,
            other => other,
        };
        presentation.disclosure = disclosure;
        let member_indent = presentation
            .indent
            .min(MAX_INDENT)
            .saturating_add(MEMBER_INDENT);
        let mut tools: Vec<(String, u64)> = Vec::new();
        let mut duration_ms: Option<u64> = None;
        for &position in &group.members {
            let entry = &self.entries[position];
            let tool = entry.facts.tool.as_deref().unwrap_or("tool");
            match tools.iter_mut().find(|(name, _)| name == tool) {
                Some((_, count)) => *count += 1,
                None => tools.push((tool.to_owned(), 1)),
            }
            if let Some(duration) = entry.duration_ms {
                duration_ms = Some(duration_ms.unwrap_or(0).saturating_add(duration));
            }
        }
        let owners = group
            .members
            .iter()
            .filter_map(|&position| {
                let entry = &self.entries[position];
                Some((entry.invocation.as_deref()?, position))
            })
            .collect::<BTreeMap<_, _>>();
        let group_id = stable_str_hash(head.get());
        let expanded = disclosure == Disclosure::Expanded;
        let member_rendering = |position: usize, role: GroupRole| {
            let entry = &self.entries[position];
            let revealed = matches!(role, GroupRole::Leader | GroupRole::Member)
                && is_revealed(&entry.facts, &presentation.reveal);
            let overridden = matches!(
                self.overrides
                    .get(&DisclosureTarget::Unit(entry.id.clone())),
                Some(Disclosure::Expanded | Disclosure::Summary)
            );
            rendering_for(role, disclosure, revealed, overridden)
        };
        let mut positions = group
            .members
            .iter()
            .chain(&group.bridged)
            .chain(&group.attached)
            .copied()
            .collect::<Vec<_>>();
        positions.sort_unstable();
        positions.dedup();
        let renderings = positions
            .iter()
            .map(|&position| {
                let role = group.role(position).unwrap_or(GroupRole::Bridged);
                let rendering = if role == GroupRole::Attached {
                    let owner_full = self.entries[position]
                        .invocation
                        .as_deref()
                        .and_then(|invocation| owners.get(invocation))
                        .is_some_and(|&owner| {
                            member_rendering(owner, group.role(owner).unwrap_or(GroupRole::Member))
                                == MemberRendering::Full
                        });
                    if expanded || owner_full {
                        MemberRendering::Full
                    } else {
                        MemberRendering::Nothing
                    }
                } else {
                    member_rendering(position, role)
                };
                (position, role, rendering)
            })
            .collect::<Vec<_>>();
        let closer = renderings
            .iter()
            .rev()
            .find(|(_, _, rendering)| rendering.has_unit())
            .map(|(position, _, _)| *position);
        for &(position, role, rendering) in &renderings {
            let entry = &self.entries[position];
            placements.insert(
                entry.id.clone(),
                ItemPlacement::Grouped {
                    group: group_id,
                    expanded,
                    member: matches!(role, GroupRole::Leader | GroupRole::Member),
                    rendering,
                },
            );
            let mut own = self.effective(entry);
            match rendering {
                MemberRendering::Nothing => {}
                MemberRendering::Full => own.indent = member_indent,
                MemberRendering::HeaderRow => {
                    own.disclosure = Disclosure::Collapsed;
                    own.indent = member_indent;
                    own.spacing = Spacing {
                        before: 0,
                        after: if closer == Some(position) {
                            presentation.spacing.clamped().after
                        } else {
                            0
                        },
                    };
                }
            }
            structures.insert(
                entry.id.clone(),
                ResolvedUnitPresentation::Item(ItemStructure {
                    facts: entry.facts.clone(),
                    presentation: own,
                    rendering: Some(rendering),
                    group: Some(leader.clone()),
                    target: DisclosureTarget::Unit(entry.id.clone()),
                    collapsible: !entry.facts.awaiting_decision,
                }),
            );
        }
        let structure = ResolvedUnitPresentation::Group(GroupStructure {
            leader: leader.clone(),
            facts: group.facts.clone(),
            presentation,
            members: group
                .members
                .iter()
                .map(|&position| self.entries[position].id.clone())
                .collect(),
            tools,
            duration_ms,
            target,
            closes: closer.is_none(),
        });
        group_signatures.insert(head.clone(), structure.layout_hash());
        groups.insert(head.clone(), structure);
        group_heads.insert(leader, head);
    }

    /// Placement of an item (visible when unknown).
    #[must_use]
    pub fn placement_of(&self, id: &TranscriptViewItemId) -> ItemPlacement {
        self.placements
            .get(id)
            .copied()
            .unwrap_or(ItemPlacement::Visible)
    }

    /// Resolved structure of a unit. Items not (yet) resolved get host defaults.
    #[must_use]
    pub fn unit_presentation(&self, unit: &UnitKey) -> &ResolvedUnitPresentation {
        match unit {
            UnitKey::Item(id) => self.items.get(id).unwrap_or(&self.fallback),
            UnitKey::GroupHeader(id) => self.groups.get(id).unwrap_or(&self.fallback),
            UnitKey::HiddenMarker(_) => &self.hidden,
            UnitKey::Boundary(_) => &self.boundary,
        }
    }

    /// The unit presenting a disclosure target: the item unit, or the group header.
    #[must_use]
    pub fn target_unit(&self, target: &DisclosureTarget) -> Option<UnitKey> {
        match target {
            DisclosureTarget::Unit(id) | DisclosureTarget::Section(id, _) => self
                .items
                .contains_key(id)
                .then(|| UnitKey::Item(id.clone())),
            DisclosureTarget::Group(leader) => self
                .group_heads
                .get(leader)
                .map(|head| UnitKey::GroupHeader(head.clone())),
        }
    }

    /// Effective disclosure of `target`, if it exists.
    #[must_use]
    pub fn effective_disclosure(&self, target: &DisclosureTarget) -> Option<Disclosure> {
        match target {
            DisclosureTarget::Unit(id) => match self.items.get(id)? {
                ResolvedUnitPresentation::Item(item) => Some(item.presentation.disclosure),
                _ => None,
            },
            DisclosureTarget::Section(id, name) => match self.items.get(id)? {
                ResolvedUnitPresentation::Item(item) => {
                    Some(item.presentation.section(name).disclosure)
                }
                _ => None,
            },
            DisclosureTarget::Group(leader) => {
                match self.groups.get(self.group_heads.get(leader)?)? {
                    ResolvedUnitPresentation::Group(group) => Some(group.presentation.disclosure),
                    _ => None,
                }
            }
        }
    }

    fn awaiting_decision(&self, target: &DisclosureTarget) -> bool {
        match target {
            DisclosureTarget::Unit(id) | DisclosureTarget::Section(id, _) => self
                .positions
                .get(id)
                .is_some_and(|&position| self.entries[position].facts.awaiting_decision),
            DisclosureTarget::Group(_) => false,
        }
    }

    /// Set `target` to `disclosure`; returns whether the override changed. Targets of units
    /// awaiting a decision may expand but never collapse or hide (A16). A group choice
    /// replaces its members' own unit choices. Takes effect at the next [`Self::update`].
    pub fn set_disclosure(&mut self, target: &DisclosureTarget, disclosure: Disclosure) -> bool {
        if self.awaiting_decision(target) && disclosure != Disclosure::Expanded {
            return false;
        }
        let replaced = match target {
            DisclosureTarget::Group(leader) => self
                .group_heads
                .get(leader)
                .and_then(|head| self.groups.get(head))
                .map(|group| match group {
                    ResolvedUnitPresentation::Group(group) => group
                        .members
                        .iter()
                        .map(|id| DisclosureTarget::Unit(id.clone()))
                        .collect(),
                    _ => Vec::new(),
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        self.overrides.set(target.clone(), disclosure, &replaced)
    }

    /// Toggle `target` between expanded and collapsed from its effective disclosure
    /// (summarized or hidden targets expand first); returns the new disclosure, or `None`
    /// when the target is unknown or may not collapse.
    pub fn toggle(&mut self, target: &DisclosureTarget) -> Option<Disclosure> {
        let current = self.effective_disclosure(target)?;
        let next = if current == Disclosure::Expanded {
            Disclosure::Collapsed
        } else {
            Disclosure::Expanded
        };
        if self.awaiting_decision(target) && next != Disclosure::Expanded {
            return None;
        }
        self.set_disclosure(target, next);
        Some(next)
    }

    /// Drop every manual override; returns whether any existed.
    pub fn clear_overrides(&mut self) -> bool {
        self.overrides.clear()
    }
}

impl UnitStructure for StructureResolver {
    fn placement(&self, item: &TranscriptViewItem) -> ItemPlacement {
        self.placement_of(&item.id)
    }

    fn signature(&self, item: &TranscriptViewItem) -> u64 {
        self.signatures.get(&item.id).copied().unwrap_or(0)
    }

    fn group_signature(&self, first: &TranscriptViewItem) -> u64 {
        self.group_signatures.get(&first.id).copied().unwrap_or(0)
    }
}

/// Pure member rendering rule.
///
/// * Expanded groups render every member and bridge fully.
/// * Summarized groups render one header row per member; revealed members and members the
///   user opened render fully; bridges render nothing.
/// * Collapsed groups render only revealed members.
const fn rendering_for(
    role: GroupRole,
    disclosure: Disclosure,
    revealed: bool,
    overridden: bool,
) -> MemberRendering {
    match (role, disclosure) {
        (_, Disclosure::Expanded) => MemberRendering::Full,
        (GroupRole::Bridged | GroupRole::Attached, _) => MemberRendering::Nothing,
        (_, Disclosure::Summary) if revealed || overridden => MemberRendering::Full,
        (_, Disclosure::Summary) => MemberRendering::HeaderRow,
        (_, Disclosure::Collapsed | Disclosure::Hidden) if revealed => MemberRendering::Full,
        (_, Disclosure::Collapsed | Disclosure::Hidden) => MemberRendering::Nothing,
    }
}

/// Turn identity per item: the output location's turn, else the last user message seen.
fn turn_identities(items: &[TranscriptViewItem]) -> Vec<u64> {
    let mut current = UNKNOWN_TURN;
    items
        .iter()
        .map(|item| {
            if starts_turn(item) {
                current = stable_str_hash(item.id.get());
            }
            item.output_location
                .as_ref()
                .map_or(current, |location| stable_str_hash(&location.turn_id))
        })
        .collect()
}

fn positions(entries: &[Entry]) -> BTreeMap<TranscriptViewItemId, usize> {
    entries
        .iter()
        .enumerate()
        .map(|(position, entry)| (entry.id.clone(), position))
        .collect()
}

fn inputs(entries: &[Entry]) -> Vec<GroupingInput<'_>> {
    entries
        .iter()
        .map(|entry| GroupingInput {
            id: &entry.key,
            turn: entry.turn,
            facts: &entry.facts,
            group: group_spec(&entry.base),
            invocation: entry.invocation.as_deref(),
            attached: entry.attached,
            superseded: entry.superseded,
        })
        .collect()
}

const fn group_spec(presentation: &ResolvedPresentation) -> &GroupSpec {
    &presentation.group
}

/// Mark every historical request whose invocation a later entry represents as superseded;
/// returns the first position whose mark changed. One backward pass.
fn mark_superseded(entries: &mut [Entry]) -> Option<usize> {
    let mut represented: BTreeSet<String> = BTreeSet::new();
    let mut marks = vec![false; entries.len()];
    for (position, entry) in entries.iter().enumerate().rev() {
        let Some(invocation) = entry.invocation.as_deref() else {
            continue;
        };
        if entry.request {
            marks[position] = represented.contains(invocation);
        } else if entry.represents_invocation() {
            represented.insert(invocation.to_owned());
        }
    }
    let mut first_changed = None;
    for (position, (entry, mark)) in entries.iter_mut().zip(marks).enumerate() {
        if entry.superseded != mark {
            entry.superseded = mark;
            first_changed.get_or_insert(position);
        }
    }
    first_changed
}

#[cfg(test)]
mod tests;
