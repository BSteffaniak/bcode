//! Group projection over the resident transcript window.
//!
//! Grouping itself is renderer-neutral ([`bcode_presentation_policy::Grouping`]);
//! this module feeds it the resident window, resolves each group's own
//! presentation, and records how every grouped item renders so the terminal
//! keeps one layout entry per transcript item. Only tool calls are members:
//! supplemental and hidden tool contribution items are offered as attached
//! units, so they never count toward a group and render with the member whose
//! invocation they belong to. The projection is updated
//! incrementally: unchanged items are recognized by identity and revision, so
//! a streaming update re-examines only the changed item's neighborhood.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use bcode_presentation_policy::models::{
    Disclosure, GroupSpec, ResolvedPresentation, Spacing, UnitFacts,
};
use bcode_presentation_policy::{GroupRole, Grouping, GroupingInput, UnitGroup, is_revealed};
use bcode_tui_components::transcript::item::{TranscriptItem, TranscriptItemKind};

use super::{DisclosureTarget, StructureSnapshot, TurnIndex};

/// Columns members are indented past their group's own indent.
pub const MEMBER_INDENT: u8 = 2;

/// Turn identity used for items without an output location.
const UNKNOWN_TURN: u64 = u64::MAX;

/// How one grouped item renders under its group's current disclosure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MemberRendering {
    /// No rows: the group hides this item.
    Nothing,
    /// One header row (the member's header fields), as in a summarized group.
    HeaderRow,
    /// The item's own presentation, indented under the group.
    Full,
}

/// How a transcript item takes part in a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupPlacement {
    /// The group, by its first member's item id.
    pub group: u64,
    /// Leader, member, bridge, or attached unit.
    pub role: GroupRole,
    /// Whether the member is shown in place while the group is collapsed or
    /// summarized (its status is in the group's `reveal` list, or the
    /// visibility floor reveals it). Always `false` for bridges and attached
    /// units.
    pub revealed: bool,
    /// For an attached unit, the member item whose invocation it belongs
    /// to, when that member is in the group; it renders with that member.
    pub attached_to: Option<u64>,
    /// Whether this is the last item of the group that renders rows, so it
    /// carries the group's trailing spacing.
    pub closes: bool,
}

/// One group unit resolved for presentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupView {
    /// First member item id; the group target is `group:<id>`.
    pub id: u64,
    /// Group facts (kind `group`), as rules see them.
    pub facts: UnitFacts,
    /// Group presentation from the policy, visibility floor applied, before
    /// manual overrides.
    pub presentation: ResolvedPresentation,
    /// Member item ids in transcript order.
    pub members: Vec<u64>,
    /// Bridged item ids in transcript order.
    pub bridged: Vec<u64>,
    /// Attached item ids (supplemental and hidden tool contributions) in
    /// transcript order; never counted as members.
    pub attached: Vec<u64>,
    /// Member tool names with their counts, in first-appearance order.
    pub tools: Vec<(String, usize)>,
    /// Sum of the finished members' durations, when any is known.
    pub duration_ms: Option<u64>,
}

/// Groups over the resident window and every grouped item's placement.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupSnapshot {
    views: BTreeMap<u64, Arc<GroupView>>,
    placements: BTreeMap<u64, GroupPlacement>,
    /// Resident index of every grouped item when the snapshot was built.
    positions: BTreeMap<u64, usize>,
}

impl GroupSnapshot {
    /// Whether no group exists.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.views.is_empty()
    }

    /// Number of groups.
    #[must_use]
    pub fn len(&self) -> usize {
        self.views.len()
    }
}

/// Pure member rendering rule shared by layout and toggling.
///
/// * Expanded groups render every member and bridge fully.
/// * Summarized groups render one header row per member; revealed members,
///   and members the user opened, render fully; bridges render nothing.
/// * Collapsed groups render only revealed members.
///
/// Attached units are resolved by [`StructureSnapshot::member_rendering`]
/// from the member they belong to; this rule treats them as bridges.
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

/// Groups are never hidden; a hidden choice collapses to the header.
const fn group_floor(disclosure: Disclosure) -> Disclosure {
    match disclosure {
        Disclosure::Hidden => Disclosure::Collapsed,
        other => other,
    }
}

impl StructureSnapshot {
    /// Placement of the item with `item_id` in its group, if it is grouped.
    #[must_use]
    pub fn group_placement(&self, item_id: u64) -> Option<&GroupPlacement> {
        self.groups.placements.get(&item_id)
    }

    /// The group whose first member is `group`.
    #[must_use]
    pub fn group_view(&self, group: u64) -> Option<&GroupView> {
        self.groups.views.get(&group).map(AsRef::as_ref)
    }

    /// First member id of the group containing `item_id` (as a member, a
    /// bridge, or an attached unit).
    #[must_use]
    pub fn group_of(&self, item_id: u64) -> Option<u64> {
        self.group_placement(item_id)
            .map(|placement| placement.group)
    }

    /// Effective disclosure of `group`: a manual override, else the policy.
    ///
    /// Groups never resolve to hidden.
    #[must_use]
    pub fn group_disclosure(&self, group: u64) -> Option<Disclosure> {
        let view = self.group_view(group)?;
        Some(group_floor(
            self.target_override(&DisclosureTarget::Group(group))
                .unwrap_or(view.presentation.disclosure),
        ))
    }

    /// Effective presentation of `group`, overrides applied.
    #[must_use]
    pub fn group_presentation(&self, group: u64) -> Option<ResolvedPresentation> {
        let mut presentation = self.group_view(group)?.presentation.clone();
        presentation.disclosure = self.group_disclosure(group)?;
        Some(presentation)
    }

    /// Indent of members of `group`: the group's indent plus
    /// [`MEMBER_INDENT`].
    #[must_use]
    pub fn member_indent(&self, group: u64) -> u8 {
        self.group_view(group).map_or(0, |view| {
            view.presentation
                .indent
                .min(bcode_presentation_policy::models::MAX_INDENT)
                .saturating_add(MEMBER_INDENT)
        })
    }

    /// How the item with `item_id` renders under its group, or `None` when
    /// it is not grouped.
    ///
    /// An attached unit renders fully when the group is expanded or when the
    /// member it belongs to renders fully, and renders nothing otherwise (a
    /// member's header row stands for its whole invocation).
    #[must_use]
    pub fn member_rendering(&self, item_id: u64) -> Option<MemberRendering> {
        let placement = self.group_placement(item_id)?;
        let disclosure = self.group_disclosure(placement.group)?;
        if placement.role == GroupRole::Attached {
            let owner_full = placement
                .attached_to
                .filter(|owner| {
                    self.group_placement(*owner)
                        .is_some_and(|owner| owner.role != GroupRole::Attached)
                })
                .and_then(|owner| self.member_rendering(owner))
                == Some(MemberRendering::Full);
            return Some(if disclosure == Disclosure::Expanded || owner_full {
                MemberRendering::Full
            } else {
                MemberRendering::Nothing
            });
        }
        // A member the user opened from its header row renders in full; a
        // collapsed choice keeps the compact row.
        let overridden = matches!(
            self.target_override(&DisclosureTarget::Unit(item_id)),
            Some(Disclosure::Expanded | Disclosure::Summary)
        );
        Some(rendering_for(
            placement.role,
            disclosure,
            placement.revealed,
            overridden,
        ))
    }

    /// Resident items of `group` (members, bridges, and attached units) in
    /// transcript order,
    /// with the resident index each had when groups were last synchronized.
    #[must_use]
    pub fn group_items(&self, group: u64) -> Vec<(u64, Option<usize>)> {
        let Some(view) = self.group_view(group) else {
            return Vec::new();
        };
        let mut ids: Vec<u64> = view
            .members
            .iter()
            .chain(&view.bridged)
            .chain(&view.attached)
            .copied()
            .collect();
        ids.sort_by_key(|id| self.groups.positions.get(id).copied());
        ids.into_iter()
            .map(|id| (id, self.groups.positions.get(&id).copied()))
            .collect()
    }

    /// Adjust an item's own presentation for its place in a group.
    ///
    /// Members rendered fully take the member indent; members rendered as a
    /// header row collapse, drop their own spacing, and carry the group's
    /// trailing spacing when they close it.
    pub(super) fn adjust_for_group(&self, item_id: u64, presentation: &mut ResolvedPresentation) {
        let Some(placement) = self.group_placement(item_id).copied() else {
            return;
        };
        let Some(rendering) = self.member_rendering(item_id) else {
            return;
        };
        let indent = self.member_indent(placement.group);
        match rendering {
            MemberRendering::Nothing => {}
            MemberRendering::Full => presentation.indent = indent,
            MemberRendering::HeaderRow => {
                presentation.disclosure = Disclosure::Collapsed;
                presentation.indent = indent;
                presentation.spacing = Spacing {
                    before: 0,
                    after: if placement.closes {
                        self.group_closing_spacing(placement.group)
                    } else {
                        0
                    },
                };
            }
        }
    }

    /// Blank rows after a group's last rendered row when that row is
    /// compact (the group header or a member header row).
    #[must_use]
    pub fn group_closing_spacing(&self, group: u64) -> u8 {
        self.group_view(group)
            .map_or(0, |view| view.presentation.spacing.clamped().after)
    }

    /// Stable layout fragment for the item's group role: everything about
    /// the group that changes how this item's entry is laid out.
    #[must_use]
    pub fn group_signature(&self, item_id: u64) -> String {
        let Some(placement) = self.group_placement(item_id) else {
            return String::new();
        };
        let rendering = self.member_rendering(item_id);
        let mut signature = format!(
            "{}:{:?}:{:?}:{}:{}:{}",
            placement.group,
            placement.role,
            rendering,
            placement.revealed,
            placement.closes,
            self.member_indent(placement.group),
        );
        if placement.closes {
            let _ = write!(
                signature,
                ":close={}",
                self.group_closing_spacing(placement.group)
            );
        }
        if placement.role == GroupRole::Leader
            && let (Some(view), Some(presentation)) = (
                self.group_view(placement.group),
                self.group_presentation(placement.group),
            )
        {
            let _ = write!(
                signature,
                ":{:?}:{:?}:{:?}:{:?}:{}:{:?}:{:?}:{}:{:?}:{:?}:{}",
                presentation.disclosure,
                presentation.header,
                presentation.spacing,
                presentation.sections.get("timing"),
                presentation.indent,
                view.facts.status,
                view.facts.tool,
                view.members.len(),
                view.tools,
                view.duration_ms,
                view.facts.is_error,
            );
        }
        signature
    }

    /// This snapshot with groups computed from scratch over `items`.
    ///
    /// Used for standalone renders (previews) that keep no group cache.
    #[must_use]
    pub fn with_groups_for(mut self, items: &[&TranscriptItem]) -> Self {
        let mut cache = GroupCache::default();
        cache.sync(&self, 0, &sources(items));
        self.groups = Arc::clone(&cache.snapshot);
        self
    }
}

/// One resident item offered to grouping, with its turn identity.
#[derive(Debug, Clone, Copy)]
pub(super) struct GroupSource<'a> {
    pub item: &'a TranscriptItem,
    pub turn: Option<&'a str>,
}

/// Group sources for `items`, keyed by their output location's turn.
pub(super) fn sources<'a>(items: &[&'a TranscriptItem]) -> Vec<GroupSource<'a>> {
    items
        .iter()
        .map(|item| GroupSource {
            item,
            turn: item
                .output_location()
                .map(|location| location.turn_id.as_str()),
        })
        .collect()
}

/// One resident item as grouping last saw it.
#[derive(Debug, Clone)]
struct CachedUnit {
    item_id: u64,
    revision: u64,
    id: String,
    turn: u64,
    facts: UnitFacts,
    group: GroupSpec,
    duration_ms: Option<u64>,
    /// Invocation the item is or belongs to.
    invocation: Option<String>,
    /// Whether the item is a tool contribution attached to an invocation
    /// rather than a tool call (see [`is_attached_contribution`]).
    attached: bool,
}

impl CachedUnit {
    fn new(snapshot: &StructureSnapshot, source: GroupSource<'_>) -> Self {
        let item = source.item;
        // The same facts rendering resolves with (turn recency,
        // `last_in_turn`, declared targets), so `group` rules on any fact
        // agree with the presentation of the unit.
        let (facts, group) = snapshot.facts(item).map_or_else(
            || (UnitFacts::default(), GroupSpec::none()),
            |facts| {
                let group = snapshot.policy.resolve(&facts).group;
                (facts, group)
            },
        );
        let duration_ms = item
            .tool_timing()
            .filter(|_| !item.tool_is_active())
            .and_then(|timing| {
                timing
                    .duration_ms
                    .or_else(|| Some(timing.finished_at_ms?.saturating_sub(timing.started_at_ms?)))
            });
        Self {
            item_id: item.id().get(),
            revision: item.revision(),
            id: item.id().get().to_string(),
            turn: source.turn.map_or(UNKNOWN_TURN, |turn| {
                let mut hasher = DefaultHasher::new();
                turn.hash(&mut hasher);
                hasher.finish()
            }),
            facts,
            group,
            duration_ms,
            invocation: item.visual_invocation_id().map(str::to_owned),
            attached: is_attached_contribution(item),
        }
    }

    const fn same_item(&self, source: GroupSource<'_>) -> bool {
        self.item_id == source.item.id().get() && self.revision == source.item.revision()
    }

    /// Whether grouping and group views would see the same inputs.
    fn same_inputs(&self, other: &Self) -> bool {
        self.item_id == other.item_id
            && self.turn == other.turn
            && self.facts == other.facts
            && self.group == other.group
            && self.duration_ms == other.duration_ms
            && self.invocation == other.invocation
            && self.attached == other.attached
    }
}

/// Incrementally maintained group projection.
#[derive(Debug, Clone, Default)]
pub(super) struct GroupCache {
    units: Vec<CachedUnit>,
    grouping: Grouping,
    /// Policy fingerprint the units were resolved under.
    policy: Option<u64>,
    /// Turn recency the units were resolved under, when the policy reads it.
    turns: Option<TurnIndex>,
    /// `last_in_turn` positions the units were resolved under, when a rule
    /// reads them.
    last_in_turn: Option<String>,
    /// Producer-declared target fields the units were resolved under.
    target_fields: Arc<super::DeclaredTargetFields>,
    /// Structure revision the views were built under.
    revision: Option<u64>,
    /// Layout fragment of every grouped item, for change detection.
    signatures: BTreeMap<u64, String>,
    pub(super) snapshot: Arc<GroupSnapshot>,
}

impl GroupCache {
    /// Bring the projection up to date with `sources`; returns the resident
    /// indexes whose group role changed and must be laid out again.
    ///
    /// Unchanged items are skipped by identity and revision. Grouping is
    /// updated only over the changed span, and group views are rebuilt only
    /// when grouping inputs, policy, or overrides changed.
    pub(super) fn sync(
        &mut self,
        snapshot: &StructureSnapshot,
        revision: u64,
        sources: &[GroupSource<'_>],
    ) -> BTreeSet<usize> {
        let policy = snapshot.policy.fingerprint();
        let turns = snapshot
            .policy
            .uses_turn_facts()
            .then(|| snapshot.turns.clone());
        let last_in_turn = snapshot
            .uses_last_in_turn
            .then(|| snapshot.turns.last_in_turn_key());
        let regrouped = if self.policy != Some(policy)
            || self.turns != turns
            || self.last_in_turn != last_in_turn
            || self.target_fields != snapshot.target_fields
        {
            self.units = sources
                .iter()
                .map(|source| CachedUnit::new(snapshot, *source))
                .collect();
            self.grouping = Grouping::compute(&inputs(&self.units));
            self.policy = Some(policy);
            self.turns = turns;
            self.last_in_turn = last_in_turn;
            self.target_fields = Arc::clone(&snapshot.target_fields);
            true
        } else {
            self.splice(snapshot, sources)
        };
        if !regrouped && self.revision == Some(revision) {
            return BTreeSet::new();
        }
        self.revision = Some(revision);
        self.rebuild_views(snapshot)
    }

    /// Refresh changed units and regroup the changed span; returns whether
    /// grouping inputs changed.
    fn splice(&mut self, snapshot: &StructureSnapshot, sources: &[GroupSource<'_>]) -> bool {
        let old_len = self.units.len();
        let new_len = sources.len();
        let common = old_len.min(new_len);
        let prefix = (0..common)
            .take_while(|&index| self.units[index].same_item(sources[index]))
            .count();
        let suffix = (0..common - prefix)
            .take_while(|&back| {
                self.units[old_len - 1 - back].same_item(sources[new_len - 1 - back])
            })
            .count();
        if prefix + suffix == old_len && old_len == new_len {
            return false;
        }
        let dirty = prefix..new_len - suffix;
        let fresh: Vec<CachedUnit> = sources[dirty.clone()]
            .iter()
            .map(|source| CachedUnit::new(snapshot, *source))
            .collect();
        let replaced = prefix..old_len - suffix;
        let unchanged = old_len == new_len
            && self.units[replaced.clone()]
                .iter()
                .zip(&fresh)
                .all(|(old, new)| old.same_inputs(new));
        self.units.splice(replaced, fresh);
        if unchanged {
            return false;
        }
        self.grouping.update(&inputs(&self.units), dirty);
        true
    }

    /// Rebuild group views and placements; returns resident indexes whose
    /// layout fragment changed.
    fn rebuild_views(&mut self, snapshot: &StructureSnapshot) -> BTreeSet<usize> {
        let mut built = GroupSnapshot::default();
        for group in self.grouping.groups() {
            let view = self.view(snapshot, group);
            let id = view.id;
            // Member item id by invocation, so attached units find the
            // call they belong to.
            let owners: BTreeMap<&str, u64> = group
                .members
                .iter()
                .filter_map(|&position| {
                    let unit = &self.units[position];
                    Some((unit.invocation.as_deref()?, unit.item_id))
                })
                .collect();
            for (position, role) in group
                .members
                .iter()
                .map(|&position| (position, group.role(position)))
                .chain(
                    group
                        .bridged
                        .iter()
                        .map(|&position| (position, Some(GroupRole::Bridged))),
                )
                .chain(
                    group
                        .attached
                        .iter()
                        .map(|&position| (position, Some(GroupRole::Attached))),
                )
            {
                let Some(role) = role else { continue };
                let unit = &self.units[position];
                let member = matches!(role, GroupRole::Leader | GroupRole::Member);
                built.positions.insert(unit.item_id, position);
                built.placements.insert(
                    unit.item_id,
                    GroupPlacement {
                        group: id,
                        role,
                        revealed: member && is_revealed(&unit.facts, &view.presentation.reveal),
                        attached_to: (role == GroupRole::Attached)
                            .then(|| owners.get(unit.invocation.as_deref()?).copied())
                            .flatten(),
                        closes: false,
                    },
                );
            }
            built.views.insert(id, Arc::new(view));
        }
        mark_closers(snapshot, &mut built);
        let mut probe = snapshot.clone();
        probe.groups = Arc::new(built);
        let signatures: BTreeMap<u64, String> = probe
            .groups
            .placements
            .keys()
            .map(|&id| (id, probe.group_signature(id)))
            .collect();
        let resident: BTreeMap<u64, usize> = self
            .units
            .iter()
            .enumerate()
            .filter(|(_, unit)| {
                signatures.contains_key(&unit.item_id)
                    || self.signatures.contains_key(&unit.item_id)
            })
            .map(|(index, unit)| (unit.item_id, index))
            .collect();
        let changed = resident
            .iter()
            .filter(|(id, _)| signatures.get(id) != self.signatures.get(id))
            .map(|(_, &index)| index)
            .collect();
        self.signatures = signatures;
        self.snapshot = Arc::clone(&probe.groups);
        changed
    }

    fn view(&self, snapshot: &StructureSnapshot, group: &UnitGroup) -> GroupView {
        let leader = &self.units[group.leader()];
        let presentation = snapshot.policy.resolve(&group.facts);
        let mut tools: Vec<(String, usize)> = Vec::new();
        let mut duration_ms: Option<u64> = None;
        for &position in &group.members {
            let unit = &self.units[position];
            let tool = unit.facts.tool.as_deref().unwrap_or("tool");
            match tools.iter_mut().find(|(name, _)| name == tool) {
                Some((_, count)) => *count += 1,
                None => tools.push((tool.to_owned(), 1)),
            }
            if let Some(duration) = unit.duration_ms {
                duration_ms = Some(duration_ms.unwrap_or(0).saturating_add(duration));
            }
        }
        GroupView {
            id: leader.item_id,
            facts: group.facts.clone(),
            presentation,
            members: group
                .members
                .iter()
                .map(|&position| self.units[position].item_id)
                .collect(),
            bridged: group
                .bridged
                .iter()
                .map(|&position| self.units[position].item_id)
                .collect(),
            attached: group
                .attached
                .iter()
                .map(|&position| self.units[position].item_id)
                .collect(),
            tools,
            duration_ms,
        }
    }
}

/// Mark the last row-rendering item of every group; the leader always
/// renders the group header, so it closes when nothing after it renders.
fn mark_closers(snapshot: &StructureSnapshot, built: &mut GroupSnapshot) {
    let mut probe = snapshot.clone();
    probe.groups = Arc::new(std::mem::take(built));
    let mut closers = Vec::new();
    for view in probe.groups.views.values() {
        let mut items: Vec<u64> = view
            .members
            .iter()
            .chain(&view.bridged)
            .chain(&view.attached)
            .copied()
            .collect();
        items.sort_by_key(|id| probe.groups.positions.get(id).copied());
        let closer = items
            .iter()
            .rev()
            .find(|&&id| {
                probe
                    .member_rendering(id)
                    .is_some_and(|rendering| rendering != MemberRendering::Nothing)
            })
            .copied()
            .unwrap_or(view.id);
        closers.push(closer);
    }
    *built = Arc::try_unwrap(probe.groups).unwrap_or_else(|shared| (*shared).clone());
    for closer in closers {
        if let Some(placement) = built.placements.get_mut(&closer) {
            placement.closes = true;
        }
    }
}

/// Whether `item` is a tool contribution attached to an invocation rather
/// than the presentation of a tool call itself.
///
/// Supplemental and hidden placements are extra contributions and never
/// count as calls. Request, progress, and result placements occupy the
/// invocation's primary slot (they are how a plugin-presented call renders,
/// with the call's own item identity), so they stay group members.
const fn is_attached_contribution(item: &TranscriptItem) -> bool {
    matches!(
        item.kind(),
        TranscriptItemKind::ToolContribution {
            placement: bcode_session_models::ToolContributionPlacement::Supplemental
                | bcode_session_models::ToolContributionPlacement::Hidden,
            ..
        }
    )
}

fn inputs(units: &[CachedUnit]) -> Vec<GroupingInput<'_>> {
    units
        .iter()
        .map(|unit| GroupingInput {
            id: &unit.id,
            turn: unit.turn,
            facts: &unit.facts,
            group: &unit.group,
            invocation: unit.invocation.as_deref(),
            attached: unit.attached,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use bcode_presentation_policy::models::{
        LayerSource, PresentationSection, RuleLayer, UnitStatus,
    };
    use bcode_session_view_models::ToolInvocationViewStatus;
    use bcode_tui_components::transcript::item::{
        ToolTiming, TranscriptItemKind, tool_result_item,
    };

    use super::super::{StructureState, unit_facts};
    use super::*;

    const GROUP_READS: &str = "[[rules]]\nmatch = { kind = \"tool\" }\nset = { group = { by = \"consecutive\" } }\n[[rules]]\nmatch = { kind = \"group\" }\nset = { disclosure = \"collapsed\" }";

    fn state(rules: &str) -> StructureState {
        let mut state = StructureState::default();
        state.set_layers(&[RuleLayer {
            source: LayerSource::Theme("test".into()),
            rules: toml::from_str::<PresentationSection>(rules).unwrap().rules,
        }]);
        state
    }

    fn tool(call: &str, name: &str, is_error: bool) -> TranscriptItem {
        tool_result_item(call, Some(name), None, "line", is_error)
    }

    fn running(call: &str, name: &str) -> TranscriptItem {
        TranscriptItem::with_kind(
            "Tool",
            "{}".to_owned(),
            false,
            TranscriptItemKind::ToolRequest {
                tool_call_id: call.to_owned(),
                producer_plugin_id: None,
                tool_name: name.to_owned(),
                working_directory: None,
                active: true,
                status: Some(ToolInvocationViewStatus::Running),
                timing: ToolTiming::default(),
            },
        )
    }

    fn reasoning() -> TranscriptItem {
        TranscriptItem::with_kind(
            "Reasoning",
            "thinking".to_owned(),
            false,
            TranscriptItemKind::ReasoningMessage,
        )
    }

    fn sync(state: &mut StructureState, items: &[TranscriptItem]) -> BTreeSet<usize> {
        state.sync_groups(&items.iter().collect::<Vec<_>>())
    }

    fn renderings(
        state: &StructureState,
        items: &[TranscriptItem],
    ) -> Vec<Option<MemberRendering>> {
        let snapshot = state.snapshot();
        items
            .iter()
            .map(|item| snapshot.member_rendering(item.id().get()))
            .collect()
    }

    fn request(call: &str, name: &str, arguments: &str) -> TranscriptItem {
        TranscriptItem::with_kind(
            "Tool",
            arguments.to_owned(),
            false,
            TranscriptItemKind::ToolRequest {
                tool_call_id: call.to_owned(),
                producer_plugin_id: None,
                tool_name: name.to_owned(),
                working_directory: None,
                active: false,
                status: Some(ToolInvocationViewStatus::Finished),
                timing: ToolTiming::default(),
            },
        )
    }

    fn contribution(
        call: &str,
        placement: bcode_session_models::ToolContributionPlacement,
    ) -> TranscriptItem {
        TranscriptItem::with_kind(
            "Tool presentation",
            "visual".to_owned(),
            false,
            TranscriptItemKind::ToolContribution {
                contribution: Box::new(bcode_session_models::ToolContributionEvent {
                    invocation_id: call.to_owned(),
                    contribution_id: format!("{placement:?}"),
                    sequence: 1,
                    producer_id: "test.plugin".to_owned(),
                    schema: "test.visual".to_owned(),
                    schema_version: 1,
                    operation: bcode_session_models::ToolContributionOperation::Upsert,
                    persistence: bcode_session_models::ToolContributionPersistence::Durable,
                    artifact: None,
                    payload: serde_json::Value::Null,
                }),
                placement,
                invocation: None,
            },
        )
    }

    #[test]
    fn contributions_attach_to_their_calls_without_counting_or_splitting_groups() {
        use MemberRendering::{Full, HeaderRow, Nothing};
        use bcode_session_models::ToolContributionPlacement::{Hidden, Supplemental};
        let mut state = state(
            "[[rules]]\nmatch = { kind = \"tool\" }\nset = { group = { by = \"consecutive\" } }\n\
             [[rules]]\nmatch = { kind = \"group\" }\nset = { disclosure = \"summary\" }",
        );
        let items = vec![
            tool("a", "read", false),
            contribution("a", Supplemental),
            contribution("a", Hidden),
            tool("b", "read", false),
            contribution("b", Supplemental),
            TranscriptItem::new("Assistant", "done".to_owned()),
        ];
        sync(&mut state, &items);
        let snapshot = state.snapshot();
        let leader = items[0].id().get();
        let view = snapshot.group_view(leader).expect("one group");
        let id = |index: usize| items[index].id().get();
        assert_eq!(view.members, [id(0), id(3)]);
        assert_eq!(view.tools, [("read".to_owned(), 2)]);
        assert_eq!(view.attached, [id(1), id(2), id(4)]);
        assert_eq!(
            view.facts.items,
            bcode_presentation_policy::models::SizeBucket::of(2)
        );
        assert_eq!(snapshot.group_of(id(5)), None);
        for index in [1, 2, 4] {
            assert_eq!(
                snapshot
                    .group_placement(id(index))
                    .map(|placement| placement.role),
                Some(GroupRole::Attached)
            );
        }
        assert_eq!(
            snapshot
                .group_placement(id(4))
                .and_then(|placement| placement.attached_to),
            Some(id(3))
        );
        assert_eq!(
            snapshot
                .group_items(leader)
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
            [id(0), id(1), id(2), id(3), id(4)]
        );

        // Summarized: one header row per call; contributions render with it.
        assert_eq!(
            renderings(&state, &items),
            [
                Some(HeaderRow),
                Some(Nothing),
                Some(Nothing),
                Some(HeaderRow),
                Some(Nothing),
                None
            ]
        );
        // Opening call `a` shows its contributions; `b`'s stay folded.
        let member = DisclosureTarget::Unit(id(0));
        assert!(state.set_target(
            &member,
            Disclosure::Expanded,
            &unit_facts(&items[0]).unwrap()
        ));
        sync(&mut state, &items);
        assert_eq!(
            renderings(&state, &items),
            [
                Some(Full),
                Some(Full),
                Some(Full),
                Some(HeaderRow),
                Some(Nothing),
                None
            ]
        );
        // The last row-rendering item closes the group: `b`'s header row.
        assert!(state.snapshot().group_placement(id(3)).unwrap().closes);
        // Expanding the group shows everything.
        let group = DisclosureTarget::Group(leader);
        let facts = state.snapshot().group_view(leader).unwrap().facts.clone();
        assert!(state.set_target(&group, Disclosure::Expanded, &facts));
        sync(&mut state, &items);
        assert_eq!(renderings(&state, &items)[..5], [Some(Full); 5]);
        assert!(state.snapshot().group_placement(id(4)).unwrap().closes);
    }

    #[test]
    fn contributions_alone_never_form_groups_but_primary_presentations_are_calls() {
        use bcode_session_models::ToolContributionPlacement::{Result, Supplemental};
        let mut state = state(GROUP_READS);
        let lone = vec![
            tool("a", "read", false),
            contribution("a", Supplemental),
            contribution("x", Supplemental),
        ];
        sync(&mut state, &lone);
        assert!(state.snapshot().groups.is_empty());

        // A primary-slot presentation is how a plugin-presented call renders,
        // so it is a member.
        let items = vec![tool("a", "read", false), contribution("b", Result)];
        sync(&mut state, &items);
        let snapshot = state.snapshot();
        let view = snapshot.group_view(items[0].id().get()).expect("group");
        assert_eq!(view.members.len(), 2);
        assert_eq!(view.attached, [] as [u64; 0]);
    }

    #[test]
    fn group_rules_see_declared_targets_and_regroup_when_fields_change() {
        let mut state = state(
            "[[rules]]\nmatch = { kind = \"tool\", target = \"notes\" }\nset = { group = { by = \"consecutive\" } }",
        );
        let items = vec![
            request("a", "read", r#"{"file":"notes"}"#),
            request("b", "read", r#"{"file":"notes"}"#),
        ];
        sync(&mut state, &items);
        let leader = items[0].id().get();
        assert_eq!(state.snapshot().group_of(leader), None);

        assert!(state.set_target_fields(BTreeMap::from([(
            "read".to_owned(),
            BTreeMap::from([("test.reader".to_owned(), vec!["file".to_owned()])])
        )])));
        let changed = sync(&mut state, &items);
        assert_eq!(changed, BTreeSet::from([0, 1]));
        let snapshot = state.snapshot();
        assert_eq!(snapshot.group_of(leader), Some(leader));
        assert_eq!(snapshot.group_of(items[1].id().get()), Some(leader));
    }

    #[test]
    fn regrouping_drops_member_choices_made_under_the_previous_group_disclosure() {
        let mut state = state(
            "[[rules]]\nmatch = { kind = \"tool\" }\nset = { disclosure = \"collapsed\", group = { by = \"consecutive\" } }\n\
             [[rules]]\nmatch = { kind = \"group\" }\nset = { disclosure = \"summary\" }",
        );
        let items = vec![tool("a", "read", false), tool("b", "read", false)];
        sync(&mut state, &items);
        let leader = items[0].id().get();
        let rows = || [Some(MemberRendering::HeaderRow); 2];
        assert_eq!(renderings(&state, &items), rows());

        // A bulk expand opens the group and records each member as expanded.
        assert_eq!(
            state.set_all(items.iter().enumerate(), Disclosure::Expanded),
            [0, 1]
        );
        assert_eq!(renderings(&state, &items), [Some(MemberRendering::Full); 2]);
        // Summarizing the group again shows one header row per member.
        let group = DisclosureTarget::Group(leader);
        let facts = state.snapshot().group_view(leader).unwrap().facts.clone();
        assert!(state.set_target(&group, Disclosure::Summary, &facts));
        assert_eq!(renderings(&state, &items), rows());

        // Opening a member from its row renders it in full; closing it
        // returns to the row.
        let member = DisclosureTarget::Unit(items[1].id().get());
        let member_facts = unit_facts(&items[1]).unwrap();
        assert!(state.set_target(&member, Disclosure::Expanded, &member_facts));
        assert_eq!(
            renderings(&state, &items),
            [
                Some(MemberRendering::HeaderRow),
                Some(MemberRendering::Full)
            ]
        );
        assert!(state.set_target(&member, Disclosure::Collapsed, &member_facts));
        assert_eq!(renderings(&state, &items), rows());
    }

    #[test]
    fn consecutive_tools_form_one_group_led_by_the_first_member() {
        let mut state = state(GROUP_READS);
        let items = vec![
            TranscriptItem::new("You", "hi".to_owned()),
            tool("a", "read", false),
            tool("b", "read", false),
            tool("c", "grep", false),
        ];
        let changed = sync(&mut state, &items);
        assert_eq!(changed, BTreeSet::from([1, 2, 3]));
        let snapshot = state.snapshot();
        let leader = items[1].id().get();
        let view = snapshot.group_view(leader).expect("group");
        assert_eq!(view.members.len(), 3);
        assert_eq!(view.tools, [("read".to_owned(), 2), ("grep".to_owned(), 1)]);
        assert_eq!(view.facts.status, Some(UnitStatus::Succeeded));
        assert_eq!(
            snapshot.group_disclosure(leader),
            Some(Disclosure::Collapsed)
        );
        assert_eq!(
            snapshot
                .group_placement(leader)
                .map(|placement| placement.role),
            Some(GroupRole::Leader)
        );
        assert_eq!(snapshot.group_of(items[3].id().get()), Some(leader));
        assert_eq!(snapshot.group_of(items[0].id().get()), None);
        // Collapsed: no member renders; the leader closes with the header.
        assert_eq!(
            renderings(&state, &items),
            [
                None,
                Some(MemberRendering::Nothing),
                Some(MemberRendering::Nothing),
                Some(MemberRendering::Nothing)
            ]
        );
        assert!(snapshot.group_placement(leader).unwrap().closes);
        // Nothing changed, nothing to lay out again.
        assert_eq!(sync(&mut state, &items), BTreeSet::new());
    }

    #[test]
    fn collapsed_groups_reveal_running_and_failed_members_without_splitting() {
        let mut state = state(GROUP_READS);
        let items = vec![
            tool("a", "read", false),
            running("b", "read"),
            tool("c", "read", true),
            tool("d", "read", false),
        ];
        sync(&mut state, &items);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.groups.len(), 1, "failed members stay members");
        let view = snapshot.group_view(items[0].id().get()).unwrap();
        assert_eq!(view.members.len(), 4);
        assert_eq!(view.facts.status, Some(UnitStatus::Running));
        assert!(view.facts.is_error);
        assert_eq!(
            renderings(&state, &items),
            [
                Some(MemberRendering::Nothing),
                Some(MemberRendering::Full),
                Some(MemberRendering::Full),
                Some(MemberRendering::Nothing)
            ]
        );
        // The failed member is the last item that renders rows.
        assert!(
            snapshot
                .group_placement(items[2].id().get())
                .unwrap()
                .closes
        );
        assert!(
            !snapshot
                .group_placement(items[0].id().get())
                .unwrap()
                .closes
        );
        // Revealed members render indented under the group header.
        let presentation = snapshot.presentation(&items[1]).unwrap();
        assert_eq!(presentation.indent, MEMBER_INDENT);
    }

    #[test]
    fn summarized_groups_show_member_header_rows_that_toggle_open() {
        let mut state = state(
            "[[rules]]\nmatch = { kind = \"tool\" }\nset = { group = \"consecutive_tools\" }\n[[rules]]\nmatch = { kind = \"group\" }\nset = { disclosure = \"summary\", spacing = { after = 2 } }",
        );
        let items = vec![tool("a", "read", false), tool("b", "read", false)];
        sync(&mut state, &items);
        let snapshot = state.snapshot();
        assert_eq!(
            renderings(&state, &items),
            [
                Some(MemberRendering::HeaderRow),
                Some(MemberRendering::HeaderRow)
            ]
        );
        let first = snapshot.presentation(&items[0]).unwrap();
        assert_eq!(first.disclosure, Disclosure::Collapsed);
        assert_eq!((first.spacing.before, first.spacing.after), (0, 0));
        let last = snapshot.presentation(&items[1]).unwrap();
        assert_eq!(
            last.spacing.after, 2,
            "the closer carries the group's spacing"
        );

        // Toggling a member header row opens that member in place.
        let facts = unit_facts(&items[1]).unwrap();
        let target = DisclosureTarget::Unit(items[1].id().get());
        assert_eq!(
            state.toggle_target(&target, last.disclosure, &facts),
            Some(Disclosure::Expanded)
        );
        let changed = sync(&mut state, &items);
        assert!(changed.contains(&1), "{changed:?}");
        assert_eq!(renderings(&state, &items)[1], Some(MemberRendering::Full));
        assert_eq!(
            state.snapshot().presentation(&items[1]).unwrap().disclosure,
            Disclosure::Expanded
        );
    }

    #[test]
    fn bridged_reasoning_renders_only_inside_expanded_groups() {
        let mut state = state(GROUP_READS);
        let items = vec![
            tool("a", "read", false),
            reasoning(),
            tool("b", "grep", false),
        ];
        sync(&mut state, &items);
        let leader = items[0].id().get();
        let snapshot = state.snapshot();
        let view = snapshot.group_view(leader).unwrap();
        assert_eq!(view.members.len(), 2, "bridges do not count");
        assert_eq!(view.bridged, [items[1].id().get()]);
        assert_eq!(
            snapshot
                .group_placement(items[1].id().get())
                .map(|p| p.role),
            Some(GroupRole::Bridged)
        );
        assert_eq!(
            renderings(&state, &items)[1],
            Some(MemberRendering::Nothing)
        );

        state.set_target(
            &DisclosureTarget::Group(leader),
            Disclosure::Expanded,
            &view.facts.clone(),
        );
        let changed = sync(&mut state, &items);
        assert_eq!(changed, BTreeSet::from([0, 1, 2]));
        assert_eq!(
            renderings(&state, &items),
            [
                Some(MemberRendering::Full),
                Some(MemberRendering::Full),
                Some(MemberRendering::Full)
            ]
        );
    }

    #[test]
    fn runs_shorter_than_min_do_not_group() {
        let mut state = state(
            "[[rules]]\nmatch = { kind = \"tool\" }\nset = { group = { by = \"same_tool\", min = 3 } }",
        );
        let mut items = vec![tool("a", "read", false), tool("b", "read", false)];
        sync(&mut state, &items);
        assert!(state.snapshot().groups.is_empty());
        items.push(tool("c", "read", false));
        assert_eq!(sync(&mut state, &items), BTreeSet::from([0, 1, 2]));
        assert_eq!(state.snapshot().groups.len(), 1);
    }

    #[test]
    fn groups_never_cross_turns() {
        let state = state(GROUP_READS);
        let items = [
            tool("a", "read", false),
            tool("b", "read", false),
            tool("c", "read", false),
            tool("d", "read", false),
        ];
        let turns = [Some("t1"), Some("t1"), Some("t2"), Some("t2")];
        let sources: Vec<GroupSource<'_>> = items
            .iter()
            .zip(turns)
            .map(|(item, turn)| GroupSource { item, turn })
            .collect();
        let snapshot = state.snapshot();
        let mut cache = GroupCache::default();
        cache.sync(&snapshot, 0, &sources);
        let leaders: Vec<u64> = cache.snapshot.views.keys().copied().collect();
        assert_eq!(leaders, [items[0].id().get(), items[2].id().get()]);
        assert!(
            cache
                .snapshot
                .views
                .values()
                .all(|view| view.members.len() == 2)
        );
    }

    #[test]
    fn streaming_a_new_member_updates_incrementally_and_dirties_only_its_group() {
        let mut state = state(GROUP_READS);
        let mut items = vec![
            TranscriptItem::new("You", "hi".to_owned()),
            tool("x", "bash", false),
            TranscriptItem::new("Assistant", "ok".to_owned()),
            tool("a", "read", false),
            tool("b", "read", false),
        ];
        sync(&mut state, &items);
        let leader = items[1].id().get();
        assert_eq!(state.snapshot().group_view(leader), None);
        let leader = items[3].id().get();

        // A running member streams in: the group grows and reveals it, the
        // previous closer and the new member re-lay out, earlier items do not.
        items.push(running("c", "read"));
        let changed = sync(&mut state, &items);
        assert_eq!(changed, BTreeSet::from([3, 5]), "{changed:?}");
        let snapshot = state.snapshot();
        assert_eq!(snapshot.group_view(leader).unwrap().members.len(), 3);
        assert_eq!(renderings(&state, &items)[5], Some(MemberRendering::Full));

        // The incremental result equals a fresh computation.
        let fresh = state
            .snapshot()
            .with_groups_for(&items.iter().collect::<Vec<_>>());
        assert_eq!(*fresh.groups, *snapshot.groups);

        // Unrelated streaming text leaves the groups alone.
        items[2].append_text(" more");
        assert_eq!(sync(&mut state, &items), BTreeSet::new());
    }

    #[test]
    fn group_signatures_follow_disclosure_and_membership() {
        let mut state = state(GROUP_READS);
        let mut items = vec![tool("a", "read", false), tool("b", "read", false)];
        sync(&mut state, &items);
        let leader = items[0].id().get();
        let snapshot = state.snapshot();
        let before = [
            snapshot.group_signature(leader),
            snapshot.group_signature(items[1].id().get()),
        ];
        assert!(before.iter().all(|signature| !signature.is_empty()));
        let facts = snapshot.group_view(leader).unwrap().facts.clone();
        state.toggle_target(
            &DisclosureTarget::Group(leader),
            Disclosure::Collapsed,
            &facts,
        );
        sync(&mut state, &items);
        let snapshot = state.snapshot();
        assert_ne!(snapshot.group_signature(leader), before[0]);
        assert_ne!(snapshot.group_signature(items[1].id().get()), before[1]);

        // Breaking the group clears the roles.
        items.remove(1);
        let changed = sync(&mut state, &items);
        assert_eq!(changed, BTreeSet::from([0]));
        assert_eq!(state.snapshot().group_signature(leader), "");
    }
}
