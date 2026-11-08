//! Structural presentation for transcript units.
//!
//! The compiled policy comes from theme, configuration, and user-state rule
//! layers; this module adapts terminal transcript items into renderer-neutral
//! [`UnitFacts`] and keeps per-target disclosure overrides (see
//! [`DisclosureTarget`]) as ephemeral renderer state. The visibility floor
//! applies after overrides, exactly as after rule layers: units awaiting a
//! decision stay expanded and failed units never drop below summary.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use bcode_presentation_policy::models::{
    Disclosure, DurationBucket, LayerSource, ResolvedPresentation, ResolvedSection, RuleLayer,
    SizeBucket, TurnRecency, UnitFacts, UnitKind, UnitStatus,
};
use bcode_presentation_policy::{CompiledPolicy, PolicyDiagnostic, PolicyRegistry};
use bcode_session_view_models::ToolInvocationViewStatus;
use bcode_tui_components::transcript::item::{TranscriptItem, TranscriptItemKind};

#[cfg(test)]
mod bench;
mod groups;

pub use groups::{GroupPlacement, GroupSnapshot, GroupView, MEMBER_INDENT, MemberRendering};

/// Maximum retained per-target overrides; older entries are dropped first.
const MAX_OVERRIDES: usize = 4096;
/// Maximum items scanned from the end when locating the latest turns.
const MAX_TURN_SCAN: usize = 512;
/// Prefix of terminal hit-region ids for disclosure targets.
pub const DISCLOSURE_HIT_PREFIX: &str = "disclosure:";

/// Producer-declared target argument fields: tool name, then declaring
/// plugin id, then ordered argument field names.
///
/// A declaration applies only to units its plugin produced. Tool requests,
/// results, drafts, contributions, and permission requests carry their
/// producer whenever the session knows it, so a unit with a known producer
/// uses only that producer's declaration. Units whose producer is genuinely
/// unknown (locally built items, or a permission requested before its
/// invocation was observed and without its own producer) fall back to a
/// tool's declaration only when exactly one plugin declares that tool, so an
/// ambiguous tool name never borrows another plugin's fields.
pub type DeclaredTargetFields = BTreeMap<String, BTreeMap<String, Vec<String>>>;

/// Something whose disclosure the user can toggle.
///
/// The bare form (`Display`/`FromStr`) is `unit:<item id>`,
/// `group:<first member item id>`, or `section:<item id>:<section name>`.
/// Terminal hit-region ids prefix it with [`DISCLOSURE_HIT_PREFIX`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DisclosureTarget {
    /// One unit, by transcript item id.
    Unit(u64),
    /// A group, by its first member's transcript item id.
    Group(u64),
    /// A named section of a unit, by transcript item id.
    Section(u64, String),
}

impl DisclosureTarget {
    /// Transcript item id the target belongs to (the first member for groups).
    #[must_use]
    pub const fn item_id(&self) -> u64 {
        match self {
            Self::Unit(id) | Self::Group(id) | Self::Section(id, _) => *id,
        }
    }

    /// Terminal hit-region id, `disclosure:<target>`.
    #[must_use]
    pub fn to_hit_id(&self) -> String {
        format!("{DISCLOSURE_HIT_PREFIX}{self}")
    }

    /// Parse a hit-region id; the legacy `disclosure:<item id>` parses as a unit.
    #[must_use]
    pub fn from_hit_id(hit_id: &str) -> Option<Self> {
        let target = hit_id.strip_prefix(DISCLOSURE_HIT_PREFIX)?;
        target
            .parse()
            .ok()
            .or_else(|| target.parse().ok().map(Self::Unit))
    }
}

impl fmt::Display for DisclosureTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unit(id) => write!(f, "unit:{id}"),
            Self::Group(id) => write!(f, "group:{id}"),
            Self::Section(id, name) => write!(f, "section:{id}:{name}"),
        }
    }
}

/// Error parsing a [`DisclosureTarget`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidDisclosureTarget;

impl fmt::Display for InvalidDisclosureTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected unit:<id>, group:<id>, or section:<id>:<name>")
    }
}

impl std::error::Error for InvalidDisclosureTarget {}

impl FromStr for DisclosureTarget {
    type Err = InvalidDisclosureTarget;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (kind, rest) = text.split_once(':').ok_or(InvalidDisclosureTarget)?;
        let id = |text: &str| text.parse::<u64>().map_err(|_| InvalidDisclosureTarget);
        match kind {
            "unit" => Ok(Self::Unit(id(rest)?)),
            "group" => Ok(Self::Group(id(rest)?)),
            "section" => {
                let (item, name) = rest.split_once(':').ok_or(InvalidDisclosureTarget)?;
                if name.is_empty() {
                    return Err(InvalidDisclosureTarget);
                }
                Ok(Self::Section(id(item)?, name.to_owned()))
            }
            _ => Err(InvalidDisclosureTarget),
        }
    }
}

/// Immutable view of structural state used while building transcript rows.
#[derive(Debug, Clone, Default)]
pub struct StructureSnapshot {
    policy: Arc<CompiledPolicy>,
    overrides: Arc<BTreeMap<DisclosureTarget, Override>>,
    turns: TurnIndex,
    groups: Arc<GroupSnapshot>,
    /// Producer-declared target argument fields.
    target_fields: Arc<DeclaredTargetFields>,
    /// Whether any rule conditions on `last_in_turn`.
    uses_last_in_turn: bool,
    /// Whether the legacy reasoning visibility setting (`tui.thinking.show`
    /// or its runtime toggle) is off; see [`Self::suppressed`].
    reasoning_suppressed: bool,
}

/// One manual disclosure choice, stamped so the oldest is dropped first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Override {
    disclosure: Disclosure,
    sequence: u64,
}

/// The latest two turn identities, for the `turn` recency fact, and which
/// recent items end their turn, for the `last_in_turn` fact.
///
/// Equality compares turn recency only (what the `turn` fact observes), so
/// consumers that read only recency do not react to every appended item.
#[derive(Debug, Clone, Default)]
pub struct TurnIndex {
    latest: Option<String>,
    previous: Option<String>,
    /// `last_in_turn` for positioned items among the last [`MAX_TURN_SCAN`]
    /// items; items outside the scan have no `last_in_turn` fact.
    last_in_turn: Arc<BTreeMap<u64, bool>>,
}

impl PartialEq for TurnIndex {
    fn eq(&self, other: &Self) -> bool {
        self.latest == other.latest && self.previous == other.previous
    }
}

impl Eq for TurnIndex {}

impl TurnIndex {
    /// Find the latest two distinct turns, and the items that end their
    /// turn, by scanning at most [`MAX_TURN_SCAN`] `items` from the end.
    ///
    /// Only items with a turn location take part: an item is last in its
    /// turn when the next positioned item belongs to another turn.
    #[must_use]
    pub fn from_items<'a>(items: impl DoubleEndedIterator<Item = &'a TranscriptItem>) -> Self {
        let mut index = Self::default();
        let mut last_in_turn = BTreeMap::new();
        let mut next_turn: Option<&String> = None;
        for item in items.rev().take(MAX_TURN_SCAN) {
            let Some(turn) = item.output_location().map(|location| &location.turn_id) else {
                continue;
            };
            last_in_turn.insert(item.id().get(), next_turn != Some(turn));
            next_turn = Some(turn);
            match &index.latest {
                None => index.latest = Some(turn.clone()),
                Some(latest) if latest != turn && index.previous.is_none() => {
                    index.previous = Some(turn.clone());
                }
                Some(_) => {}
            }
        }
        index.last_in_turn = Arc::new(last_in_turn);
        index
    }

    /// The `last_in_turn` fact for `item`, when it is known.
    #[must_use]
    pub fn last_in_turn(&self, item: &TranscriptItem) -> Option<bool> {
        self.last_in_turn.get(&item.id().get()).copied()
    }

    fn recency(&self, item: &TranscriptItem) -> TurnRecency {
        let Some(turn) = item.output_location().map(|location| &location.turn_id) else {
            return TurnRecency::Latest;
        };
        if self.latest.as_ref() == Some(turn) {
            TurnRecency::Latest
        } else if self.previous.as_ref() == Some(turn) {
            TurnRecency::Previous
        } else {
            TurnRecency::Older
        }
    }

    /// Stable key for layout fingerprints.
    #[must_use]
    pub fn key(&self) -> String {
        format!("{:?}:{:?}", self.latest, self.previous)
    }

    /// Stable key over the items that end their turn, for policies that
    /// read `last_in_turn`.
    fn last_in_turn_key(&self) -> String {
        use std::hash::{Hash as _, Hasher as _};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.last_in_turn.hash(&mut hasher);
        format!("{:x}", hasher.finish())
    }
}

impl StructureSnapshot {
    /// Effective presentation for `item`, or `None` for non-structural items.
    ///
    /// Grouped items are adjusted for their group (see
    /// [`Self::member_rendering`]): members take the member indent, and
    /// members shown as one header row of a summarized group report
    /// `collapsed`, which is also what toggling them starts from.
    #[must_use]
    pub fn presentation(&self, item: &TranscriptItem) -> Option<ResolvedPresentation> {
        let (mut presentation, _) = self.unit_presentation(item)?;
        self.adjust_for_group(item.id().get(), &mut presentation);
        Some(presentation)
    }

    /// The unit's own presentation with its manual override applied, then
    /// the visibility floor, before any group adjustment; with its facts.
    fn unit_presentation(
        &self,
        item: &TranscriptItem,
    ) -> Option<(ResolvedPresentation, UnitFacts)> {
        let facts = self.facts(item)?;
        #[cfg(test)]
        bench::count_resolution();
        let mut presentation = self.policy.resolve(&facts);
        if let Some(disclosure) = self.target_override(&DisclosureTarget::Unit(item.id().get())) {
            presentation.disclosure = disclosure;
            bcode_presentation_policy::apply_visibility_floor(&mut presentation, &facts);
        }
        Some((presentation, facts))
    }

    /// Whether `item` renders nothing at all, not even a hidden-unit count
    /// marker: reasoning hidden while the legacy reasoning visibility setting
    /// is off.
    ///
    /// That setting is the user's explicit choice to omit reasoning, applied
    /// before the visibility floor; any other hidden unit leaves a marker.
    #[must_use]
    pub fn suppressed(&self, facts: &UnitFacts, presentation: &ResolvedPresentation) -> bool {
        self.reasoning_suppressed
            && facts.kind == Some(UnitKind::Reasoning)
            && presentation.disclosure == Disclosure::Hidden
    }

    /// Facts of `item` as rules see them: [`unit_facts`] plus the turn
    /// facts (`turn`, `last_in_turn`) and producer-declared targets.
    #[must_use]
    pub fn facts(&self, item: &TranscriptItem) -> Option<UnitFacts> {
        let mut facts = unit_facts(item)?;
        facts.turn = self.turns.recency(item);
        facts.last_in_turn = self.turns.last_in_turn(item);
        if let Some(fields) = facts
            .tool
            .as_ref()
            .and_then(|tool| self.target_fields.get(tool))
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
        Some(facts)
    }

    /// Effective presentation of section `name` of `item`: the policy's
    /// section with any section override applied, then the visibility floor
    /// (decision sections of units awaiting a decision stay expanded; failure
    /// sections of failed units stay visible).
    ///
    /// Non-structural items get host defaults.
    #[must_use]
    pub fn section(&self, item: &TranscriptItem, name: &str) -> ResolvedSection {
        let Some((mut presentation, facts)) = self.unit_presentation(item) else {
            return ResolvedSection::defaults(bcode_presentation_policy::host_defaults().preview);
        };
        if let Some(disclosure) =
            self.target_override(&DisclosureTarget::Section(item.id().get(), name.to_owned()))
        {
            let mut section = presentation.section(name);
            section.disclosure = disclosure;
            presentation.sections.insert(name.to_owned(), section);
            bcode_presentation_policy::apply_visibility_floor(&mut presentation, &facts);
        }
        presentation.section(name)
    }

    /// Manual override recorded for `target`, before any floor.
    #[must_use]
    pub fn target_override(&self, target: &DisclosureTarget) -> Option<Disclosure> {
        self.overrides.get(target).map(|entry| entry.disclosure)
    }

    /// Targets that currently carry a manual override, in target order.
    pub fn override_targets(&self) -> impl Iterator<Item = &DisclosureTarget> {
        self.overrides.keys()
    }

    /// Section overrides recorded for transcript item `item_id`, host and
    /// plugin-namespaced alike, as `(section name, disclosure)` in name order.
    ///
    /// Reads one ordered range of the override map, so the work is bounded
    /// by that item's overrides (at most [`MAX_OVERRIDES`]) rather than by
    /// every override or a fixed list of section names.
    pub fn section_overrides(&self, item_id: u64) -> impl Iterator<Item = (&str, Disclosure)> {
        let start = DisclosureTarget::Section(item_id, String::new());
        self.overrides
            .range(start..)
            .map_while(move |(target, entry)| match target {
                DisclosureTarget::Section(id, name) if *id == item_id => {
                    Some((name.as_str(), entry.disclosure))
                }
                _ => None,
            })
    }

    /// This snapshot with turn facts from `turns`, for rendering items
    /// outside the resident transcript (for example a picker sample).
    #[must_use]
    pub fn with_turns(mut self, turns: TurnIndex) -> Self {
        self.turns = turns;
        self
    }

    /// Policy presentation for arbitrary `facts`, without overrides.
    ///
    /// Used for units the host synthesizes, such as groups.
    #[must_use]
    pub fn resolve(&self, facts: &UnitFacts) -> ResolvedPresentation {
        self.policy.resolve(facts)
    }

    /// Stable fragment for layout signatures of `item`.
    #[must_use]
    pub fn signature(&self, item: &TranscriptItem) -> String {
        self.presentation(item)
            .map_or_else(String::new, |presentation| {
                format!(
                    "{:?}:{:?}:{}:{:?}:{:?}",
                    presentation.disclosure,
                    presentation.detail,
                    presentation.preview.lines,
                    presentation.preview.keep,
                    presentation.sections
                )
            })
    }
}

/// Mutable structural state owned by the application.
#[derive(Debug, Clone, Default)]
pub struct StructureState {
    snapshot: StructureSnapshot,
    diagnostics: Vec<PolicyDiagnostic>,
    revision: u64,
    override_sequence: u64,
    group_cache: groups::GroupCache,
}

impl StructureState {
    /// Recompile from `layers`; returns whether the effective policy changed.
    pub fn set_layers(&mut self, layers: &[RuleLayer]) -> bool {
        let (policy, diagnostics) =
            CompiledPolicy::compile(layers, &PolicyRegistry::with_host_parts());
        if policy.fingerprint() == self.snapshot.policy.fingerprint() {
            return false;
        }
        for diagnostic in &diagnostics {
            tracing::warn!(
                source = ?diagnostic.source,
                rule = ?diagnostic.rule,
                severity = ?diagnostic.severity,
                message = %diagnostic.message,
                "presentation rule diagnostic"
            );
        }
        self.snapshot.policy = Arc::new(policy);
        self.snapshot.uses_last_in_turn = layers
            .iter()
            .flat_map(|layer| &layer.rules)
            .any(|rule| rule.matcher.last_in_turn.is_some());
        self.diagnostics = diagnostics;
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// Snapshot for row building.
    #[must_use]
    pub fn snapshot(&self) -> StructureSnapshot {
        self.snapshot.clone()
    }

    /// Bring groups over the resident window `items` up to date; returns the
    /// indexes of items whose group role changed and must be laid out again.
    ///
    /// Work is bounded by the resident window: unchanged items are skipped by
    /// identity and revision, and grouping is updated incrementally over the
    /// changed span.
    pub fn sync_groups(&mut self, items: &[&TranscriptItem]) -> BTreeSet<usize> {
        let changed = self
            .group_cache
            .sync(&self.snapshot, self.revision, &groups::sources(items));
        self.snapshot.groups = Arc::clone(&self.group_cache.snapshot);
        changed
    }

    /// Update turn recency; returns whether it changed in a way the policy observes.
    pub fn set_turns(&mut self, turns: TurnIndex) -> bool {
        let recency = turns != self.snapshot.turns;
        let last_in_turn = turns.last_in_turn != self.snapshot.turns.last_in_turn;
        if !recency && !last_in_turn {
            return false;
        }
        self.snapshot.turns = turns;
        (recency && self.snapshot.policy.uses_turn_facts())
            || (last_in_turn && self.snapshot.uses_last_in_turn)
    }

    /// Layout key for turn facts, empty when the policy ignores them.
    #[must_use]
    pub fn turn_key(&self) -> String {
        let mut key = if self.snapshot.policy.uses_turn_facts() {
            self.snapshot.turns.key()
        } else {
            String::new()
        };
        if self.snapshot.uses_last_in_turn {
            key.push_str(":last:");
            key.push_str(&self.snapshot.turns.last_in_turn_key());
        }
        key
    }

    /// Replace producer-declared target argument fields (from plugin
    /// manifests); returns whether they changed. Declared fields are tried
    /// before the host fields.
    pub fn set_target_fields(&mut self, fields: DeclaredTargetFields) -> bool {
        if *self.snapshot.target_fields == fields {
            return false;
        }
        self.snapshot.target_fields = Arc::new(fields);
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// Record whether the legacy reasoning visibility setting is off (see
    /// [`StructureSnapshot::suppressed`]); returns whether it changed.
    pub const fn set_reasoning_suppressed(&mut self, suppressed: bool) -> bool {
        if self.snapshot.reasoning_suppressed == suppressed {
            return false;
        }
        self.snapshot.reasoning_suppressed = suppressed;
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// Diagnostics from the last compilation.
    #[must_use]
    pub fn diagnostics(&self) -> &[PolicyDiagnostic] {
        &self.diagnostics
    }

    /// Policy fingerprint, for structural layout cache keys.
    #[must_use]
    pub fn policy_fingerprint(&self) -> u64 {
        self.snapshot.policy.fingerprint()
    }

    /// Revision bumped by every override or policy change.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Explain how synthesized `facts` (for example a group's) resolve
    /// against the compiled policy, without overrides.
    #[must_use]
    pub fn explain_facts(
        &self,
        facts: &UnitFacts,
    ) -> bcode_presentation_policy::ExplainedPresentation {
        self.snapshot.policy.explain(facts)
    }

    /// Explain how `item` resolved, when it is a structural unit.
    #[must_use]
    pub fn explain(
        &self,
        item: &TranscriptItem,
    ) -> Option<bcode_presentation_policy::ExplainedPresentation> {
        let facts = self.snapshot.facts(item)?;
        let mut explained = self.snapshot.policy.explain(&facts);
        if let Some(disclosure) = self
            .snapshot
            .target_override(&DisclosureTarget::Unit(item.id().get()))
        {
            explained.presentation.disclosure = disclosure;
            explained
                .floor
                .push(format!("manual toggle: disclosure set to {disclosure:?}"));
            // The floor applies after manual toggles, as after rules.
            explained
                .floor
                .extend(bcode_presentation_policy::apply_visibility_floor(
                    &mut explained.presentation,
                    &facts,
                ));
        }
        Some(explained)
    }

    /// Toggle `item` between expanded and collapsed; returns the new disclosure.
    ///
    /// Summarized or hidden items expand first. Units awaiting a decision
    /// cannot be collapsed.
    pub fn toggle(&mut self, item: &TranscriptItem) -> Option<Disclosure> {
        let facts = unit_facts(item)?;
        if facts.awaiting_decision {
            return None;
        }
        let current = self.snapshot.presentation(item)?.disclosure;
        let next = if current == Disclosure::Expanded {
            Disclosure::Collapsed
        } else {
            Disclosure::Expanded
        };
        self.set_override(DisclosureTarget::Unit(item.id().get()), next);
        Some(next)
    }

    /// Toggle `target` from its `current` effective disclosure between
    /// expanded and collapsed; returns the new disclosure.
    ///
    /// Summarized or hidden targets expand first. `facts` are the facts of
    /// the unit owning the target (group facts for group targets); targets of
    /// units awaiting a decision may expand but never collapse.
    pub fn toggle_target(
        &mut self,
        target: &DisclosureTarget,
        current: Disclosure,
        facts: &UnitFacts,
    ) -> Option<Disclosure> {
        let next = if current == Disclosure::Expanded {
            Disclosure::Collapsed
        } else {
            Disclosure::Expanded
        };
        if facts.awaiting_decision && next != Disclosure::Expanded {
            return None;
        }
        self.set_override(target.clone(), next);
        Some(next)
    }

    /// Set `target` to `disclosure`; returns whether the override changed.
    ///
    /// `facts` are the facts of the unit owning the target (group facts for
    /// group targets); targets of units awaiting a decision may expand but
    /// never collapse or hide.
    pub fn set_target(
        &mut self,
        target: &DisclosureTarget,
        disclosure: Disclosure,
        facts: &UnitFacts,
    ) -> bool {
        if facts.awaiting_decision && disclosure != Disclosure::Expanded {
            return false;
        }
        if self.snapshot.target_override(target) == Some(disclosure) {
            return false;
        }
        self.set_override(target.clone(), disclosure);
        true
    }

    /// Drop the overrides of every target matching `remove`; returns whether
    /// any were dropped.
    pub fn clear_targets(&mut self, remove: impl Fn(&DisclosureTarget) -> bool) -> bool {
        if !self.snapshot.overrides.keys().any(&remove) {
            return false;
        }
        Arc::make_mut(&mut self.snapshot.overrides).retain(|target, _| !remove(target));
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// Whether any manual override is recorded.
    #[must_use]
    pub fn has_overrides(&self) -> bool {
        !self.snapshot.overrides.is_empty()
    }

    /// Set every structural unit in `items`, and every group that starts at
    /// one of them (or has a recorded override there), to `disclosure`;
    /// returns changed item indexes.
    ///
    /// Units awaiting a decision are left alone (they stay expanded), and so
    /// are hidden units: bulk actions fold what is shown, and a hidden unit
    /// is revealed individually from its marker.
    pub fn set_all<'a>(
        &mut self,
        items: impl IntoIterator<Item = (usize, &'a TranscriptItem)>,
        disclosure: Disclosure,
    ) -> Vec<usize> {
        let mut changed = Vec::new();
        for (index, item) in items {
            let group = DisclosureTarget::Group(item.id().get());
            if self
                .snapshot
                .group_disclosure(item.id().get())
                .or_else(|| self.snapshot.target_override(&group))
                .is_some_and(|current| current != disclosure)
            {
                self.set_override(group, disclosure);
                changed.push(index);
            }
            let Some(facts) = unit_facts(item) else {
                continue;
            };
            let unit = DisclosureTarget::Unit(item.id().get());
            if facts.awaiting_decision || self.snapshot.target_override(&unit) == Some(disclosure) {
                continue;
            }
            if self
                .snapshot
                .presentation(item)
                .is_some_and(|presentation| {
                    !matches!(presentation.disclosure, Disclosure::Hidden)
                        && presentation.disclosure != disclosure
                })
            {
                self.set_override(unit, disclosure);
                if changed.last() != Some(&index) {
                    changed.push(index);
                }
            }
        }
        changed
    }

    /// Drop all manual overrides; returns whether any existed.
    pub fn clear_overrides(&mut self) -> bool {
        if self.snapshot.overrides.is_empty() {
            return false;
        }
        self.snapshot.overrides = Arc::new(BTreeMap::new());
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// Forget overrides for targets whose items are no longer resident.
    pub fn reconcile(&mut self, resident: &BTreeSet<u64>) {
        if self
            .snapshot
            .overrides
            .keys()
            .any(|target| !resident.contains(&target.item_id()))
        {
            let overrides = Arc::make_mut(&mut self.snapshot.overrides);
            overrides.retain(|target, _| resident.contains(&target.item_id()));
        }
    }

    /// Record `disclosure` for `target`.
    ///
    /// A group choice replaces its members' own unit choices: those were
    /// made relative to the group's previous disclosure (for example by a
    /// bulk expand), and keeping them would render members in full under a
    /// group that was just summarized again.
    fn set_override(&mut self, target: DisclosureTarget, disclosure: Disclosure) {
        let sequence = self.override_sequence;
        self.override_sequence = self.override_sequence.wrapping_add(1);
        let members: Vec<u64> = match &target {
            DisclosureTarget::Group(group) => self
                .snapshot
                .group_view(*group)
                .map(|view| view.members.clone())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let overrides = Arc::make_mut(&mut self.snapshot.overrides);
        for member in members {
            overrides.remove(&DisclosureTarget::Unit(member));
        }
        overrides.insert(
            target,
            Override {
                disclosure,
                sequence,
            },
        );
        // Linear, but only once the bounded map is full.
        while overrides.len() > MAX_OVERRIDES {
            let Some(oldest) = overrides
                .iter()
                .min_by_key(|(_, entry)| entry.sequence)
                .map(|(target, _)| target.clone())
            else {
                break;
            };
            overrides.remove(&oldest);
        }
        self.revision = self.revision.wrapping_add(1);
    }
}

/// Rule layer persisted from interactive "make this the default" actions.
#[must_use]
pub const fn user_state_layer(
    rules: Vec<bcode_presentation_policy::models::PresentationRule>,
) -> RuleLayer {
    RuleLayer {
        source: LayerSource::UserState,
        rules,
    }
}

/// Renderer-neutral facts for a transcript item.
///
/// Every transcript item kind is structural: messages, reasoning, tools,
/// permissions, interactions, compaction, skills, activity, system and
/// plugin status notes. `turn` and `last_in_turn` depend on neighbouring
/// items and are filled by [`StructureSnapshot::facts`]; `target` comes from
/// the host argument fields ([`HOST_TARGET_FIELDS`]).
///
/// The per-item content scan (line count and target) is memoized by item
/// identity and revision, bounded to [`MAX_FACT_MEMO`] items.
#[must_use]
pub fn unit_facts(item: &TranscriptItem) -> Option<UnitFacts> {
    let (lines, target) = content_facts(item);
    let duration = item
        .tool_timing()
        .filter(|_| !item.tool_is_active())
        .and_then(|timing| {
            timing
                .duration_ms
                .or_else(|| Some(timing.finished_at_ms?.saturating_sub(timing.started_at_ms?)))
        })
        .map(DurationBucket::of_millis);
    let base = UnitFacts {
        lines,
        target,
        duration,
        ..UnitFacts::default()
    };
    Some(if is_tool_like(item) {
        tool_facts(item, base)
    } else {
        other_facts(item, base)
    })
}

/// Whether `item` is a tool request, result, contribution, draft, or
/// permission request.
const fn is_tool_like(item: &TranscriptItem) -> bool {
    matches!(
        item.kind(),
        TranscriptItemKind::ToolRequest { .. }
            | TranscriptItemKind::ToolResult { .. }
            | TranscriptItemKind::ToolContribution { .. }
            | TranscriptItemKind::ToolRequestDraft { .. }
            | TranscriptItemKind::PermissionRequest { .. }
    )
}

/// Facts of tool-like items (see [`is_tool_like`]).
fn tool_facts(item: &TranscriptItem, base: UnitFacts) -> UnitFacts {
    match item.kind() {
        TranscriptItemKind::ToolRequest {
            producer_plugin_id,
            tool_name,
            active,
            status,
            timing,
            ..
        } => UnitFacts {
            kind: Some(UnitKind::Tool),
            tool: Some(tool_name.clone()),
            producer: producer_plugin_id.clone(),
            status: Some(if timing.timed_out == Some(true) {
                UnitStatus::TimedOut
            } else {
                status.map_or(
                    if *active {
                        UnitStatus::Running
                    } else {
                        UnitStatus::Requested
                    },
                    unit_status,
                )
            }),
            is_error: matches!(status, Some(ToolInvocationViewStatus::Failed)),
            ..base
        },
        TranscriptItemKind::ToolResult {
            tool_name,
            is_error,
            timing,
            ..
        } => UnitFacts {
            kind: Some(UnitKind::Tool),
            tool: tool_name.clone(),
            producer: item.producer_plugin_id().map(ToOwned::to_owned),
            status: Some(if *is_error {
                UnitStatus::Failed
            } else if timing.timed_out == Some(true) {
                UnitStatus::TimedOut
            } else {
                UnitStatus::Succeeded
            }),
            is_error: *is_error,
            ..base
        },
        TranscriptItemKind::ToolContribution {
            contribution,
            invocation,
            ..
        } => {
            let invocation = invocation.as_deref();
            UnitFacts {
                kind: Some(UnitKind::Tool),
                tool: invocation.and_then(|invocation| invocation.tool_name.clone()),
                producer: invocation
                    .and_then(|invocation| invocation.producer_plugin_id.clone())
                    .or_else(|| Some(contribution.producer_id.clone())),
                schema: Some(format!(
                    "{}/{}",
                    contribution.schema, contribution.schema_version
                )),
                status: invocation.map(|invocation| unit_status(invocation.status)),
                is_error: invocation
                    .and_then(|invocation| invocation.is_error)
                    .unwrap_or(false),
                ..base
            }
        }
        TranscriptItemKind::ToolRequestDraft { draft } => UnitFacts {
            kind: Some(UnitKind::Tool),
            tool: Some(draft.tool_name.clone()),
            producer: draft.producer_plugin_id.clone(),
            schema: Some(format!("{}/{}", draft.schema, draft.schema_version)),
            status: Some(UnitStatus::Requested),
            ..base
        },
        TranscriptItemKind::PermissionRequest { tool_name, .. } => UnitFacts {
            kind: Some(UnitKind::Permission),
            tool: Some(tool_name.clone()),
            producer: item.producer_plugin_id().map(ToOwned::to_owned),
            awaiting_decision: true,
            status: Some(UnitStatus::Waiting),
            ..base
        },
        _ => base,
    }
}

/// Facts of messages, reasoning, decisions and notes.
fn other_facts(item: &TranscriptItem, base: UnitFacts) -> UnitFacts {
    let lifecycle = Some(if item.streaming() {
        UnitStatus::Running
    } else {
        UnitStatus::Succeeded
    });
    match item.kind() {
        TranscriptItemKind::PermissionResult { approved } => UnitFacts {
            kind: Some(UnitKind::Permission),
            status: Some(if *approved {
                UnitStatus::Succeeded
            } else {
                UnitStatus::Cancelled
            }),
            ..base
        },
        TranscriptItemKind::Interaction { interaction } => UnitFacts {
            kind: Some(UnitKind::Interaction),
            producer: interaction.producer_id.clone(),
            schema: interaction.exchange_schema.as_ref().map(|schema| {
                interaction
                    .exchange_schema_version
                    .map_or_else(|| schema.clone(), |version| format!("{schema}/{version}"))
            }),
            awaiting_decision: !interaction.resolved,
            status: Some(if interaction.resolved {
                UnitStatus::Succeeded
            } else {
                UnitStatus::Waiting
            }),
            ..base
        },
        TranscriptItemKind::UserMessage => UnitFacts {
            kind: Some(UnitKind::UserMessage),
            ..base
        },
        TranscriptItemKind::AssistantMessage => UnitFacts {
            kind: Some(UnitKind::AssistantMessage),
            status: lifecycle,
            ..base
        },
        TranscriptItemKind::ReasoningMessage => UnitFacts {
            kind: Some(UnitKind::Reasoning),
            status: lifecycle,
            ..base
        },
        TranscriptItemKind::System => UnitFacts {
            // Plugin-labelled system messages are plugin status notes.
            kind: Some(if item.role() == PLUGIN_ROLE {
                UnitKind::PluginStatus
            } else {
                UnitKind::System
            }),
            ..base
        },
        TranscriptItemKind::Meta => UnitFacts {
            kind: Some(if item.role() == COMPACTION_ROLE {
                UnitKind::Compaction
            } else {
                UnitKind::Activity
            }),
            ..base
        },
        TranscriptItemKind::Skill => UnitFacts {
            kind: Some(UnitKind::Skill),
            ..base
        },
        TranscriptItemKind::SkillError => UnitFacts {
            kind: Some(UnitKind::Skill),
            status: Some(UnitStatus::Failed),
            is_error: true,
            ..base
        },
        TranscriptItemKind::Generic => UnitFacts {
            kind: Some(if item.role() == SKILL_CONTEXT_ROLE {
                UnitKind::Skill
            } else {
                UnitKind::System
            }),
            ..base
        },
        // Tool-like kinds are handled by `tool_facts`.
        _ => base,
    }
}

/// Role of system messages that carry a plugin display label.
const PLUGIN_ROLE: &str = "Plugin";
/// Role of context compaction notes.
const COMPACTION_ROLE: &str = "Compaction";
/// Role of skill context notes.
const SKILL_CONTEXT_ROLE: &str = "Skill context";

/// Host argument fields tried, in order, for the `target` fact.
pub const HOST_TARGET_FIELDS: [&str; 5] = ["path", "command", "pattern", "query", "url"];
/// Largest arguments document scanned for a target; larger ones have none.
const MAX_TARGET_ARGUMENT_BYTES: usize = 1024 * 1024;
/// Items whose content facts are memoized per thread.
const MAX_FACT_MEMO: usize = 4096;

/// Memoized content scan of one item revision.
#[derive(Debug, Clone)]
struct ContentFacts {
    revision: u64,
    text_len: usize,
    arguments_len: usize,
    lines: SizeBucket,
    target: Option<String>,
}

thread_local! {
    static CONTENT_FACTS: std::cell::RefCell<BTreeMap<u64, ContentFacts>> =
        const { std::cell::RefCell::new(BTreeMap::new()) };
}

/// Line bucket and host target of `item`, memoized by identity and revision
/// (and content lengths, which guard against in-place text edits).
fn content_facts(item: &TranscriptItem) -> (SizeBucket, Option<String>) {
    let id = item.id().get();
    let arguments = tool_arguments(item);
    let text_len = item.text().len();
    let arguments_len = arguments.map_or(0, str::len);
    let cached = CONTENT_FACTS.with(|memo| {
        memo.borrow().get(&id).and_then(|entry| {
            (entry.revision == item.revision()
                && entry.text_len == text_len
                && entry.arguments_len == arguments_len)
                .then(|| (entry.lines, entry.target.clone()))
        })
    });
    if let Some(cached) = cached {
        return cached;
    }
    let lines = SizeBucket::of(item.text().lines().count());
    let target = arguments.and_then(|arguments| extract_target(arguments, HOST_TARGET_FIELDS));
    CONTENT_FACTS.with(|memo| {
        let mut memo = memo.borrow_mut();
        memo.insert(
            id,
            ContentFacts {
                revision: item.revision(),
                text_len,
                arguments_len,
                lines,
                target: target.clone(),
            },
        );
        // Item ids grow monotonically, so the smallest is the oldest.
        while memo.len() > MAX_FACT_MEMO {
            memo.pop_first();
        }
    });
    (lines, target)
}

/// Raw tool arguments of `item`, when it carries them.
fn tool_arguments(item: &TranscriptItem) -> Option<&str> {
    match item.kind() {
        // Request text is the (pretty-printed) arguments document.
        TranscriptItemKind::ToolRequest { .. } => Some(item.text()),
        TranscriptItemKind::ToolResult { arguments_json, .. } => arguments_json.as_deref(),
        TranscriptItemKind::PermissionRequest { arguments_json, .. } => Some(arguments_json),
        TranscriptItemKind::ToolContribution { invocation, .. } => invocation
            .as_deref()
            .and_then(|invocation| invocation.arguments_json.as_deref()),
        _ => None,
    }
}

/// First non-blank string value among `fields` (in field order) of the
/// top-level JSON object `arguments`, as one bounded line.
///
/// Other values are skipped without being materialized. Documents over
/// [`MAX_TARGET_ARGUMENT_BYTES`], non-objects, and invalid JSON have no
/// target.
#[must_use]
pub fn extract_target<'f>(
    arguments: &str,
    fields: impl IntoIterator<Item = &'f str>,
) -> Option<String> {
    use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, Visitor};

    struct Fields<'f>(Vec<&'f str>);

    impl<'de> DeserializeSeed<'de> for &Fields<'_> {
        type Value = Vec<(usize, String)>;

        fn deserialize<D: serde::Deserializer<'de>>(
            self,
            deserializer: D,
        ) -> Result<Self::Value, D::Error> {
            deserializer.deserialize_map(self)
        }
    }

    impl<'de> Visitor<'de> for &Fields<'_> {
        type Value = Vec<(usize, String)>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a tool arguments object")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut found = Vec::new();
            while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
                match self.0.iter().position(|field| *field == key) {
                    Some(rank) => {
                        if let serde_json::Value::String(value) =
                            map.next_value::<serde_json::Value>()?
                        {
                            found.push((rank, value));
                        }
                    }
                    None => {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
            }
            Ok(found)
        }
    }

    if arguments.len() > MAX_TARGET_ARGUMENT_BYTES {
        return None;
    }
    let fields = Fields(fields.into_iter().collect());
    if fields.0.is_empty() {
        return None;
    }
    let mut deserializer = serde_json::Deserializer::from_str(arguments);
    let mut found = (&fields).deserialize(&mut deserializer).ok()?;
    found.sort_by_key(|(rank, _)| *rank);
    found.into_iter().find_map(|(_, value)| {
        bcode_plugin_sdk::tui_visual::bounded_optional_line(
            Some(&value),
            bcode_plugin_sdk::tui_visual::MAX_TUI_VISUAL_TARGET_BYTES,
        )
        .filter(|line| !line.is_empty())
    })
}

const fn unit_status(status: ToolInvocationViewStatus) -> UnitStatus {
    match status {
        ToolInvocationViewStatus::Requested => UnitStatus::Requested,
        ToolInvocationViewStatus::Running => UnitStatus::Running,
        ToolInvocationViewStatus::Waiting => UnitStatus::Waiting,
        ToolInvocationViewStatus::Finished => UnitStatus::Succeeded,
        ToolInvocationViewStatus::Failed => UnitStatus::Failed,
        ToolInvocationViewStatus::Cancelled => UnitStatus::Cancelled,
    }
}

#[cfg(test)]
mod tests {
    use bcode_presentation_policy::models::PresentationSection;
    use bcode_tui_components::transcript::item::{permission_request_item, tool_result_item};

    use super::*;

    fn theme_layer(rules: &str) -> RuleLayer {
        RuleLayer {
            source: LayerSource::Theme("test".into()),
            rules: toml::from_str::<PresentationSection>(rules).unwrap().rules,
        }
    }

    fn state(rules: &str) -> StructureState {
        let mut state = StructureState::default();
        state.set_layers(&[theme_layer(rules)]);
        state
    }

    #[test]
    fn tool_results_resolve_through_theme_rules() {
        let state = state(
            "[[rules]]\nmatch = { kind = \"tool\", tool = \"read\" }\nset = { disclosure = \"collapsed\" }",
        );
        let read = tool_result_item("call-1", Some("read"), None, "contents", false);
        let bash = tool_result_item("call-2", Some("bash"), None, "ok", false);
        let snapshot = state.snapshot();
        assert_eq!(
            snapshot.presentation(&read).map(|p| p.disclosure),
            Some(Disclosure::Collapsed)
        );
        assert_eq!(
            snapshot.presentation(&bash).map(|p| p.disclosure),
            Some(Disclosure::Expanded)
        );
        assert_ne!(snapshot.signature(&read), snapshot.signature(&bash));
    }

    #[test]
    fn toggles_override_theme_defaults_and_bump_revision() {
        let mut state = state("[[rules]]\nset = { disclosure = \"collapsed\" }");
        let read = tool_result_item("call-1", Some("read"), None, "contents", false);
        let before = state.revision();
        assert_eq!(state.toggle(&read), Some(Disclosure::Expanded));
        assert!(state.revision() > before);
        assert_eq!(
            state.snapshot().presentation(&read).map(|p| p.disclosure),
            Some(Disclosure::Expanded)
        );
        assert_eq!(state.toggle(&read), Some(Disclosure::Collapsed));
        assert!(state.clear_overrides());
        assert!(!state.clear_overrides());
    }

    #[test]
    fn decisions_cannot_be_collapsed_by_rules_or_toggles() {
        let mut state = state("[[rules]]\nset = { disclosure = \"hidden\" }");
        let permission = permission_request_item("perm-1", "call-1", "bash", "{}", None, None);
        assert_eq!(
            state
                .snapshot()
                .presentation(&permission)
                .map(|p| p.disclosure),
            Some(Disclosure::Expanded)
        );
        assert_eq!(state.toggle(&permission), None);
        assert_eq!(
            state.set_all([(0, &permission)], Disclosure::Collapsed),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn the_failure_floor_applies_after_manual_toggles() {
        let mut state = state("[[rules]]\nset = { disclosure = \"hidden\" }");
        let failed = tool_result_item("call-1", Some("bash"), None, "boom", true);
        let disclosure =
            |state: &StructureState| state.snapshot().presentation(&failed).map(|p| p.disclosure);
        assert_eq!(disclosure(&state), Some(Disclosure::Summary));
        assert_eq!(state.toggle(&failed), Some(Disclosure::Expanded));
        assert_eq!(disclosure(&state), Some(Disclosure::Expanded));
        // Collapsing records the choice, but the floor keeps the summary line.
        assert_eq!(state.toggle(&failed), Some(Disclosure::Collapsed));
        assert_eq!(disclosure(&state), Some(Disclosure::Summary));
        let explained = state.explain(&failed).unwrap();
        assert_eq!(explained.presentation.disclosure, Disclosure::Summary);
        assert!(
            explained
                .floor
                .last()
                .is_some_and(|note| note.contains("failed")),
            "{:?}",
            explained.floor
        );
        // Bulk collapse leaves the same floored result.
        state.clear_overrides();
        state.set_all([(0, &failed)], Disclosure::Collapsed);
        assert_eq!(disclosure(&state), Some(Disclosure::Summary));

        // A failure section a section override hides stays visible.
        let output = DisclosureTarget::Section(failed.id().get(), "error".to_owned());
        let facts = unit_facts(&failed).unwrap();
        assert!(state.set_target(&output, Disclosure::Hidden, &facts));
        let section = state.snapshot().section(&failed, "error");
        assert!(section.visible);
        assert_ne!(section.disclosure, Disclosure::Hidden);
    }

    #[test]
    fn set_all_reports_changed_indexes_and_reconcile_drops_stale_overrides() {
        let mut state = StructureState::default();
        state.set_layers(&[]);
        let a = tool_result_item("a", Some("read"), None, "x", false);
        let b = tool_result_item("b", Some("read"), None, "y", false);
        assert_eq!(
            state.set_all([(3, &a), (7, &b)], Disclosure::Collapsed),
            [3, 7]
        );
        assert_eq!(
            state.set_all([(3, &a), (7, &b)], Disclosure::Collapsed),
            Vec::<usize>::new()
        );
        state.reconcile(&BTreeSet::from([a.id().get()]));
        assert_eq!(
            state.snapshot().presentation(&b).map(|p| p.disclosure),
            Some(Disclosure::Expanded)
        );
    }

    #[test]
    fn disclosure_targets_round_trip_through_bare_and_hit_forms() {
        for (target, bare) in [
            (DisclosureTarget::Unit(7), "unit:7"),
            (DisclosureTarget::Group(8), "group:8"),
            (
                DisclosureTarget::Section(9, "bcode.git:files".to_owned()),
                "section:9:bcode.git:files",
            ),
        ] {
            assert_eq!(target.to_string(), bare);
            assert_eq!(bare.parse::<DisclosureTarget>(), Ok(target.clone()));
            assert_eq!(target.to_hit_id(), format!("disclosure:{bare}"));
            assert_eq!(
                DisclosureTarget::from_hit_id(&target.to_hit_id()),
                Some(target)
            );
        }
        assert_eq!(
            DisclosureTarget::from_hit_id("disclosure:42"),
            Some(DisclosureTarget::Unit(42))
        );
        for invalid in [
            "disclosure:",
            "disclosure:x",
            "disclosure:unit:",
            "disclosure:section:1",
            "disclosure:section:1:",
            "disclosure:tool:1",
            "markdown:unit:1",
        ] {
            assert_eq!(DisclosureTarget::from_hit_id(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn section_targets_override_policy_sections_and_respect_the_floor() {
        let mut state = state(
            "[[rules]]\nset = { preview = { lines = 6 }, sections = { output = { disclosure = \"collapsed\" } } }",
        );
        let read = tool_result_item("call-1", Some("read"), None, "contents", false);
        let output = DisclosureTarget::Section(read.id().get(), "output".to_owned());
        let facts = unit_facts(&read).unwrap();
        let snapshot = state.snapshot();
        assert_eq!(
            snapshot.section(&read, "output").disclosure,
            Disclosure::Collapsed
        );
        assert_eq!(snapshot.section(&read, "result").preview.lines, 6);
        assert_eq!(snapshot.target_override(&output), None);

        let before = state.revision();
        assert_eq!(
            state.toggle_target(&output, Disclosure::Collapsed, &facts),
            Some(Disclosure::Expanded)
        );
        assert!(state.revision() > before);
        let snapshot = state.snapshot();
        assert_eq!(
            snapshot.target_override(&output),
            Some(Disclosure::Expanded)
        );
        assert_eq!(
            snapshot.section(&read, "output").disclosure,
            Disclosure::Expanded
        );
        // A section override leaves the unit itself alone.
        assert_eq!(
            snapshot.presentation(&read).map(|p| p.disclosure),
            Some(Disclosure::Expanded)
        );
        assert_eq!(
            snapshot.target_override(&DisclosureTarget::Unit(read.id().get())),
            None
        );

        let permission = permission_request_item("perm-1", "call-1", "bash", "{}", None, None);
        let decision = unit_facts(&permission).unwrap();
        let arguments = DisclosureTarget::Section(permission.id().get(), "arguments".to_owned());
        assert_eq!(
            state.toggle_target(&arguments, Disclosure::Expanded, &decision),
            None
        );
        assert_eq!(
            state.toggle_target(
                &DisclosureTarget::Unit(permission.id().get()),
                Disclosure::Summary,
                &decision
            ),
            Some(Disclosure::Expanded)
        );
    }

    #[test]
    fn group_and_section_overrides_reconcile_with_their_items() {
        let mut state = StructureState::default();
        let facts = UnitFacts::default();
        let group = DisclosureTarget::Group(1);
        let section = DisclosureTarget::Section(2, "output".to_owned());
        state.toggle_target(&group, Disclosure::Expanded, &facts);
        state.toggle_target(&section, Disclosure::Expanded, &facts);
        state.reconcile(&BTreeSet::from([2]));
        let snapshot = state.snapshot();
        assert_eq!(snapshot.target_override(&group), None);
        assert_eq!(
            snapshot.target_override(&section),
            Some(Disclosure::Collapsed)
        );
    }

    #[test]
    fn section_overrides_list_only_that_items_sections_including_namespaced_ones() {
        let mut state = StructureState::default();
        let facts = UnitFacts::default();
        for target in [
            DisclosureTarget::Unit(2),
            DisclosureTarget::Group(2),
            DisclosureTarget::Section(1, "output".to_owned()),
            DisclosureTarget::Section(2, "output".to_owned()),
            DisclosureTarget::Section(2, "bcode.git.files".to_owned()),
            DisclosureTarget::Section(3, "arguments".to_owned()),
        ] {
            state.toggle_target(&target, Disclosure::Expanded, &facts);
        }
        let snapshot = state.snapshot();
        assert_eq!(
            snapshot.section_overrides(2).collect::<Vec<_>>(),
            [
                ("bcode.git.files", Disclosure::Collapsed),
                ("output", Disclosure::Collapsed)
            ]
        );
        assert_eq!(snapshot.section_overrides(4).count(), 0);
        assert_eq!(snapshot.section_overrides(u64::MAX).count(), 0);
    }

    #[test]
    fn overrides_stay_bounded_and_drop_the_oldest_choices_first() {
        let mut state = StructureState::default();
        let facts = UnitFacts::default();
        let total = u64::try_from(MAX_OVERRIDES).unwrap() + 2;
        for id in (0..total).rev() {
            state.toggle_target(
                &DisclosureTarget::Section(id, "output".to_owned()),
                Disclosure::Expanded,
                &facts,
            );
        }
        let snapshot = state.snapshot();
        assert_eq!(snapshot.overrides.len(), MAX_OVERRIDES);
        // Choices were made newest-item-first, so the highest ids are oldest.
        let output = |id| DisclosureTarget::Section(id, "output".to_owned());
        assert_eq!(snapshot.target_override(&output(total - 1)), None);
        assert_eq!(snapshot.target_override(&output(total - 2)), None);
        assert_eq!(
            snapshot.target_override(&output(total - 3)),
            Some(Disclosure::Collapsed)
        );
        assert_eq!(
            snapshot.target_override(&output(0)),
            Some(Disclosure::Collapsed)
        );
    }

    #[test]
    fn set_target_respects_the_floor_and_reports_only_changes() {
        let mut state = StructureState::default();
        let permission = permission_request_item("perm-1", "call-1", "bash", "{}", None, None);
        let decision = unit_facts(&permission).unwrap();
        let unit = DisclosureTarget::Unit(permission.id().get());
        let arguments = DisclosureTarget::Section(permission.id().get(), "arguments".to_owned());
        for target in [&unit, &arguments] {
            for disclosure in [
                Disclosure::Collapsed,
                Disclosure::Summary,
                Disclosure::Hidden,
            ] {
                assert!(!state.set_target(target, disclosure, &decision));
            }
        }
        assert!(!state.has_overrides());
        assert!(state.set_target(&arguments, Disclosure::Expanded, &decision));
        assert!(!state.set_target(&arguments, Disclosure::Expanded, &decision));

        let facts = UnitFacts::default();
        let group = DisclosureTarget::Group(5);
        let before = state.revision();
        assert!(state.set_target(&group, Disclosure::Summary, &facts));
        assert!(state.revision() > before);
        assert_eq!(
            state.snapshot().target_override(&group),
            Some(Disclosure::Summary)
        );
    }

    #[test]
    fn clear_targets_drops_only_matching_overrides() {
        let mut state = StructureState::default();
        let facts = UnitFacts::default();
        let unit = DisclosureTarget::Unit(1);
        let section = DisclosureTarget::Section(1, "output".to_owned());
        let other = DisclosureTarget::Unit(2);
        for target in [&unit, &section, &other] {
            state.set_target(target, Disclosure::Collapsed, &facts);
        }
        let before = state.revision();
        assert!(state.clear_targets(|target| target.item_id() == 1));
        assert!(state.revision() > before);
        assert!(!state.clear_targets(|target| target.item_id() == 1));
        let snapshot = state.snapshot();
        assert_eq!(snapshot.override_targets().collect::<Vec<_>>(), [&other]);
    }

    #[test]
    fn set_all_updates_recorded_group_overrides() {
        let mut state = StructureState::default();
        state.set_layers(&[]);
        let a = tool_result_item("a", Some("read"), None, "x", false);
        let group = DisclosureTarget::Group(a.id().get());
        state.set_target(&group, Disclosure::Summary, &UnitFacts::default());
        assert_eq!(state.set_all([(4, &a)], Disclosure::Collapsed), [4]);
        assert_eq!(
            state.snapshot().target_override(&group),
            Some(Disclosure::Collapsed)
        );
        assert_eq!(
            state.snapshot().presentation(&a).map(|p| p.disclosure),
            Some(Disclosure::Collapsed)
        );
    }

    fn kind_of(item: &TranscriptItem) -> Option<UnitKind> {
        unit_facts(item).and_then(|facts| facts.kind)
    }

    /// An assistant message positioned in `turn`, as the session view
    /// projects it.
    fn positioned(text: &str, turn: &str, position: u64) -> TranscriptItem {
        bcode_tui_components::transcript::item::terminal_item_from_shared(
            &bcode_session_view_models::TranscriptViewItem {
                id: bcode_session_view_models::TranscriptViewItemId::new(format!(
                    "{turn}:{position}"
                )),
                revision: 1,
                sequence: None,
                timestamp_ms: None,
                output_location: Some(bcode_session_view_models::TurnOutputLocation {
                    turn_id: turn.to_owned(),
                    position: bcode_session_models::TurnOutputPosition::new(position),
                }),
                streaming: false,
                kind: bcode_session_view_models::TranscriptViewItemKind::AssistantMessage {
                    message: bcode_session_view_models::ChatMessageView::markdown(text),
                },
            },
        )
    }

    #[test]
    fn every_transcript_item_kind_is_a_structural_unit() {
        use bcode_tui_components::transcript::item::permission_result_item;
        let interaction = |resolved: bool| {
            let mut summary: bcode_session_view_models::InteractionViewSummary =
                serde_json::from_value(serde_json::json!({
                    "interaction_id": "ask-1",
                    "kind": "prompt",
                    "producer_id": "bcode.ask",
                }))
                .unwrap();
            summary.resolved = resolved;
            TranscriptItem::with_kind(
                "Interaction",
                "Pick one".to_owned(),
                false,
                TranscriptItemKind::Interaction {
                    interaction: Box::new(summary),
                },
            )
        };
        let mut streaming = TranscriptItem::new("Assistant", "partial".to_owned());
        streaming.streaming = true;
        let cases = [
            (
                TranscriptItem::new("You", "hi".to_owned()),
                UnitKind::UserMessage,
            ),
            (
                TranscriptItem::new("Assistant", "hello".to_owned()),
                UnitKind::AssistantMessage,
            ),
            (
                TranscriptItem::new("Reasoning summary", "think".to_owned()),
                UnitKind::Reasoning,
            ),
            (
                TranscriptItem::new("System", "note".to_owned()),
                UnitKind::System,
            ),
            (
                TranscriptItem::with_kind(
                    "Plugin",
                    "ok".to_owned(),
                    false,
                    TranscriptItemKind::System,
                ),
                UnitKind::PluginStatus,
            ),
            (
                TranscriptItem::new("Compaction", "summary".to_owned()),
                UnitKind::Compaction,
            ),
            (
                TranscriptItem::new("Meta", "queued".to_owned()),
                UnitKind::Activity,
            ),
            (
                TranscriptItem::new("Skill", "loaded".to_owned()),
                UnitKind::Skill,
            ),
            (
                TranscriptItem::new("Skill error", "missing".to_owned()),
                UnitKind::Skill,
            ),
            (
                TranscriptItem::new("Skill context", "ctx".to_owned()),
                UnitKind::Skill,
            ),
            (
                TranscriptItem::new("Other", "x".to_owned()),
                UnitKind::System,
            ),
            (permission_result_item("perm-1", true), UnitKind::Permission),
            (
                permission_request_item("perm-1", "call-1", "bash", "{}", None, None),
                UnitKind::Permission,
            ),
            (interaction(false), UnitKind::Interaction),
            (
                tool_result_item("call", Some("read"), None, "x", false),
                UnitKind::Tool,
            ),
        ];
        for (item, kind) in &cases {
            assert_eq!(kind_of(item), Some(*kind), "{}", item.role());
        }
        let facts = |item: &TranscriptItem| unit_facts(item).unwrap();
        assert_eq!(facts(&cases[1].0).status, Some(UnitStatus::Succeeded));
        assert_eq!(facts(&streaming).status, Some(UnitStatus::Running));
        assert!(facts(&cases[8].0).is_error);
        assert_eq!(facts(&cases[8].0).status, Some(UnitStatus::Failed));
        assert_eq!(
            facts(&permission_result_item("perm-2", false)).status,
            Some(UnitStatus::Cancelled)
        );
        let pending = facts(&interaction(false));
        assert!(pending.awaiting_decision);
        assert_eq!(pending.producer.as_deref(), Some("bcode.ask"));
        assert!(!facts(&interaction(true)).awaiting_decision);
    }

    #[test]
    fn messages_resolve_through_rules_and_toggle_like_tools() {
        let mut state = state(
            "[[rules]]\nmatch = { kind = \"assistant_message\" }\nset = { disclosure = \"summary\" }",
        );
        let reply = TranscriptItem::new("Assistant", "hello".to_owned());
        let question = TranscriptItem::new("You", "hi".to_owned());
        let disclosure = |state: &StructureState, item: &TranscriptItem| {
            state.snapshot().presentation(item).map(|p| p.disclosure)
        };
        assert_eq!(disclosure(&state, &reply), Some(Disclosure::Summary));
        assert_eq!(disclosure(&state, &question), Some(Disclosure::Expanded));
        assert_eq!(state.toggle(&reply), Some(Disclosure::Expanded));
        assert_eq!(state.toggle(&question), Some(Disclosure::Collapsed));
    }

    #[test]
    fn bulk_actions_fold_every_shown_unit_kind() {
        let mut state = state(
            "[[rules]]\nmatch = { kind = \"reasoning\" }\nset = { disclosure = \"collapsed\" }\n\
             [[rules]]\nmatch = { kind = \"system\" }\nset = { disclosure = \"hidden\" }",
        );
        let reasoning = TranscriptItem::new("Reasoning summary", "thinking".to_owned());
        let reply = TranscriptItem::new("Assistant", "hello".to_owned());
        let tool = tool_result_item("call", Some("read"), None, "x", false);
        let note = TranscriptItem::new("System", "note".to_owned());
        let permission = permission_request_item("perm-1", "call-1", "bash", "{}", None, None);
        assert_eq!(kind_of(&note), Some(UnitKind::System));
        let items = [&reasoning, &reply, &tool, &note, &permission];
        let disclosures = |state: &StructureState| {
            items
                .iter()
                .map(|item| state.snapshot().presentation(item).map(|p| p.disclosure))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            state.set_all(items.iter().copied().enumerate(), Disclosure::Expanded),
            [0]
        );
        assert_eq!(
            state.set_all(items.iter().copied().enumerate(), Disclosure::Collapsed),
            [0, 1, 2]
        );
        // Hidden units stay hidden and decisions stay expanded.
        assert_eq!(
            disclosures(&state),
            [
                Some(Disclosure::Collapsed),
                Some(Disclosure::Collapsed),
                Some(Disclosure::Collapsed),
                Some(Disclosure::Hidden),
                Some(Disclosure::Expanded)
            ]
        );
        assert_eq!(
            state.set_all(items.iter().copied().enumerate(), Disclosure::Collapsed),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn targets_come_from_host_and_declared_argument_fields() {
        let result =
            |arguments: &str| tool_result_item("call", Some("read"), Some(arguments), "x", false);
        let target = |item: &TranscriptItem| unit_facts(item).unwrap().target;
        assert_eq!(
            target(&result(r#"{"content":"a\nb","path":"src/界.rs"}"#)).as_deref(),
            Some("src/界.rs")
        );
        // Host fields are tried in order, whatever the document order.
        assert_eq!(
            target(&result(r#"{"pattern":"todo","command":"rg todo"}"#)).as_deref(),
            Some("rg todo")
        );
        assert_eq!(
            target(&result(r#"{"query":"  first\nsecond"}"#)).as_deref(),
            Some("first")
        );
        for arguments in ["not json", "[1]", r#"{"path":7}"#, r#"{"path":"  "}"#, "{}"] {
            assert_eq!(target(&result(arguments)), None, "{arguments}");
        }
        let long = format!(r#"{{"path":"{}"}}"#, "界".repeat(400));
        let bounded = target(&result(&long)).unwrap();
        assert!(bounded.len() <= bcode_plugin_sdk::tui_visual::MAX_TUI_VISUAL_TARGET_BYTES);
        assert!(bounded.chars().all(|ch| ch == '界'));
        let huge = format!(
            r#"{{"path":"a","pad":"{}"}}"#,
            "x".repeat(MAX_TARGET_ARGUMENT_BYTES)
        );
        assert_eq!(target(&result(&huge)), None);

        let request = bcode_tui_components::transcript::item::tool_request_item(
            "call",
            None,
            "fetch",
            r#"{"url":"https://example.com","file":"notes.md"}"#,
            None,
        );
        assert_eq!(target(&request).as_deref(), Some("https://example.com"));
        let mut state = StructureState::default();
        state.set_layers(&[]);
        let before = state.revision();
        let declared = |plugins: &[&str]| {
            BTreeMap::from([(
                "fetch".to_owned(),
                plugins
                    .iter()
                    .map(|plugin| ((*plugin).to_owned(), vec!["file".to_owned()]))
                    .collect(),
            )])
        };
        assert!(state.set_target_fields(declared(&["test.fetch"])));
        assert!(state.revision() > before);
        let target_of = |state: &StructureState, item: &TranscriptItem| {
            state.snapshot().facts(item).unwrap().target
        };
        // Without a known producer, the only declaration applies.
        assert_eq!(target_of(&state, &request).as_deref(), Some("notes.md"));

        // A declaration applies only to units its own plugin produced.
        let produced = |producer: &str| {
            bcode_tui_components::transcript::item::tool_request_item(
                "call",
                Some(producer),
                "fetch",
                r#"{"url":"https://example.com","file":"notes.md"}"#,
                None,
            )
        };
        assert_eq!(
            target_of(&state, &produced("test.fetch")).as_deref(),
            Some("notes.md")
        );
        assert_eq!(
            target_of(&state, &produced("test.other")).as_deref(),
            Some("https://example.com")
        );
        // Another plugin declaring the same tool cannot redirect it, and an
        // ambiguous declaration is not applied to units without a producer.
        assert!(state.set_target_fields(declared(&["aaa.helper", "test.fetch"])));
        assert_eq!(
            target_of(&state, &produced("test.fetch")).as_deref(),
            Some("notes.md")
        );
        assert_eq!(
            target_of(&state, &request).as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn results_and_permissions_use_only_their_producers_target_fields() {
        let arguments = r#"{"url":"https://example.com","file":"notes.md"}"#;
        let result = |producer: Option<&str>| {
            tool_result_item("call", Some("fetch"), Some(arguments), "x", false)
                .with_producer_plugin_id(producer)
        };
        let permission = |producer: Option<&str>| {
            bcode_tui_components::transcript::item::permission_request_item(
                "permission",
                "call",
                "fetch",
                arguments,
                None,
                None,
            )
            .with_producer_plugin_id(producer)
        };
        let mut state = StructureState::default();
        state.set_layers(&[]);
        // Exactly one plugin declares `fetch`.
        state.set_target_fields(BTreeMap::from([(
            "fetch".to_owned(),
            BTreeMap::from([("test.fetch".to_owned(), vec!["file".to_owned()])]),
        )]));
        let facts = |item: &TranscriptItem| state.snapshot().facts(item).unwrap();
        for item in [result(Some("test.fetch")), permission(Some("test.fetch"))] {
            let facts = facts(&item);
            assert_eq!(facts.producer.as_deref(), Some("test.fetch"));
            assert_eq!(facts.target.as_deref(), Some("notes.md"));
        }
        // A known producer that declares nothing never borrows the single
        // declaration; host fields apply instead.
        for item in [result(Some("test.other")), permission(Some("test.other"))] {
            let facts = facts(&item);
            assert_eq!(facts.producer.as_deref(), Some("test.other"));
            assert_eq!(facts.target.as_deref(), Some("https://example.com"));
        }
        // Genuinely unknown producers keep the unambiguous fallback.
        for item in [result(None), permission(None)] {
            assert_eq!(facts(&item).target.as_deref(), Some("notes.md"));
        }
    }

    #[test]
    fn finished_tools_carry_duration_buckets() {
        use bcode_presentation_policy::models::DurationBucket;
        let timed = |duration_ms: u64| {
            let item = TranscriptItem::with_kind(
                "Tool",
                "x".to_owned(),
                false,
                TranscriptItemKind::ToolResult {
                    tool_call_id: "call".to_owned(),
                    tool_name: Some("bash".to_owned()),
                    arguments_json: None,
                    working_directory: None,
                    result: "x".to_owned(),
                    artifact: None,
                    is_error: false,
                    timing: bcode_tui_components::transcript::item::ToolTiming {
                        duration_ms: Some(duration_ms),
                        ..Default::default()
                    },
                },
            );
            unit_facts(&item).unwrap().duration
        };
        assert_eq!(timed(999), Some(DurationBucket::UnderOneSecond));
        assert_eq!(timed(12_000), Some(DurationBucket::TenToSixtySeconds));
        assert_eq!(
            unit_facts(&TranscriptItem::new("You", "hi".to_owned()))
                .unwrap()
                .duration,
            None
        );
    }

    #[test]
    fn last_in_turn_marks_the_final_positioned_item_of_each_turn() {
        let items = [
            positioned("a", "t1", 0),
            positioned("b", "t1", 1),
            TranscriptItem::new("You", "next".to_owned()),
            positioned("c", "t2", 0),
        ];
        let mut state = state(
            "[[rules]]\nmatch = { kind = \"assistant_message\", last_in_turn = false }\nset = { disclosure = \"collapsed\" }",
        );
        assert!(state.set_turns(TurnIndex::from_items(items.iter())));
        let snapshot = state.snapshot();
        let facts: Vec<_> = items
            .iter()
            .map(|item| snapshot.facts(item).unwrap())
            .collect();
        assert_eq!(
            facts
                .iter()
                .map(|facts| facts.last_in_turn)
                .collect::<Vec<_>>(),
            [Some(false), Some(true), None, Some(true)]
        );
        assert_eq!(facts[0].turn, TurnRecency::Previous);
        assert_eq!(facts[3].turn, TurnRecency::Latest);
        assert_eq!(
            snapshot.presentation(&items[0]).map(|p| p.disclosure),
            Some(Disclosure::Collapsed)
        );
        assert_ne!(state.turn_key(), "");
        // Appending to the latest turn moves `last_in_turn`, which this
        // policy observes, while turn recency is unchanged.
        let appended: Vec<_> = items
            .iter()
            .cloned()
            .chain([positioned("d", "t2", 1)])
            .collect();
        let key = state.turn_key();
        assert!(state.set_turns(TurnIndex::from_items(appended.iter())));
        assert_ne!(state.turn_key(), key);
    }
}
