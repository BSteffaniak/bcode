//! Stable grouping of consecutive units.
//!
//! Grouping is computed once, renderer-neutrally, so every frontend agrees on
//! which units a group contains and on its identity. A group's id derives from
//! its first member, so it stays stable while later members stream in and
//! across resizes. Grouping never reorders units and never crosses turns.
//!
//! Only tool calls are members. Units attached to an invocation (producer
//! contributions such as plugin visuals, including hidden placements) never
//! lead, join, or count toward a group; they sit inside the group of the call
//! they belong to as attachments, so they never split it.
//!
//! An invocation counts once. When the same invocation appears more than once
//! (a historical tool request followed by its result), the projection marks
//! the earlier units [`superseded`](GroupingInput::superseded): the later unit
//! represents the invocation and the superseded ones are attached to the
//! group, including directly before its first member.
//!
//! [`Grouping::compute`] is linear in the number of units supplied (each run
//! looks at most [`MAX_BRIDGED_GAP`] bridge or attached units past its last
//! member).
//! [`Grouping::update`] re-examines only the neighborhood of a changed range
//! and reuses every run that cannot observe the change. Renderers pass their
//! bounded resident window, so work stays bounded by that window.

use std::ops::Range;

use crate::models::{
    GroupBy, GroupSpec, ResolvedPresentation, SizeBucket, UnitFacts, UnitKind, UnitStatus,
};
use crate::unit_failed;

/// Most consecutive bridge or attached units a group may span between two
/// members, or claim after its last member.
///
/// A longer gap ends the group, which keeps each run's look-ahead bounded.
pub const MAX_BRIDGED_GAP: usize = 64;

/// One unit offered for grouping.
#[derive(Debug, Clone, Copy)]
pub struct GroupingInput<'a> {
    /// Stable unit identity.
    pub id: &'a str,
    /// Identity of the turn containing the unit.
    pub turn: u64,
    /// Unit facts.
    pub facts: &'a UnitFacts,
    /// The unit's resolved `group` property.
    pub group: &'a GroupSpec,
    /// Tool invocation the unit is or belongs to, when known.
    pub invocation: Option<&'a str>,
    /// Whether the unit is attached to an invocation rather than being a
    /// tool call itself (for example a producer contribution).
    ///
    /// Attached units never lead, join, or count toward a group. Between two
    /// members of a group in the same turn they are always kept inside it
    /// (whatever the group's bridge kinds, status, or decision state), and
    /// right after the last member they are kept inside it when they belong
    /// to one of its members' invocations.
    pub attached: bool,
    /// Whether a later unit of the same invocation represents it (for
    /// example a historical tool request whose result follows).
    ///
    /// Superseded units are attached units whose owner comes later: they
    /// never lead, join, or count toward a group. Between two members of a
    /// group in the same turn they are always kept inside it as attached
    /// units, and a contiguous span of them directly before a run's first
    /// member, in that member's turn, is claimed by the run as attached units
    /// too, so the invocation renders with its representative.
    pub superseded: bool,
}

/// How a position relates to the group containing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GroupRole {
    /// The first member; renderers attach the group header to it.
    Leader,
    /// A later member.
    Member,
    /// A bridge unit between members; not a member and not counted.
    Bridged,
    /// A unit attached to an invocation (see [`GroupingInput::attached`]);
    /// not a member and not counted.
    Attached,
}

/// A run of members presented as one group unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitGroup {
    /// Stable group identity, `group:<first member id>`.
    pub id: String,
    /// Member positions within the supplied inputs, ascending; never empty.
    pub members: Vec<usize>,
    /// Positions of bridge units between members, ascending.
    pub bridged: Vec<usize>,
    /// Positions of attached units inside the group (between members, right
    /// after the last member when they belong to a member's invocation, or
    /// superseded units directly before the first member), ascending.
    pub attached: Vec<usize>,
    /// Facts describing the group itself, for resolving its presentation.
    pub facts: UnitFacts,
}

impl UnitGroup {
    /// Position of the first member.
    #[must_use]
    pub fn leader(&self) -> usize {
        self.members[0]
    }

    /// Positions from the first member through the last member.
    #[must_use]
    pub fn span(&self) -> Range<usize> {
        self.leader()..self.members[self.members.len() - 1] + 1
    }

    /// First position the group contains: the first superseded unit claimed
    /// before the first member, else the first member.
    ///
    /// Renderers present the group header before this position.
    #[must_use]
    pub fn start(&self) -> usize {
        let leader = self.leader();
        self.attached
            .first()
            .map_or(leader, |&first| first.min(leader))
    }

    /// Role of `position` in this group, if it belongs to the group.
    #[must_use]
    pub fn role(&self, position: usize) -> Option<GroupRole> {
        if position == self.leader() {
            Some(GroupRole::Leader)
        } else if self.members.binary_search(&position).is_ok() {
            Some(GroupRole::Member)
        } else if self.bridged.binary_search(&position).is_ok() {
            Some(GroupRole::Bridged)
        } else if self.attached.binary_search(&position).is_ok() {
            Some(GroupRole::Attached)
        } else {
            None
        }
    }

    /// Member positions shown in place while the group is collapsed or
    /// summarized, given the group's resolved `presentation`.
    ///
    /// A member is revealed when its status is in the group's `reveal` list,
    /// or, whatever that list says, when it awaits a decision or failed
    /// (visibility floor). `inputs` must be the inputs the group was computed
    /// from.
    pub fn revealed_members<'s>(
        &'s self,
        inputs: &'s [GroupingInput<'_>],
        presentation: &'s ResolvedPresentation,
    ) -> impl Iterator<Item = usize> + 's {
        self.members
            .iter()
            .copied()
            .filter(|&position| is_revealed(inputs[position].facts, &presentation.reveal))
    }
}

/// Whether a group member with `facts` is revealed under `reveal`.
///
/// Members awaiting a decision and failed members are always revealed.
#[must_use]
pub fn is_revealed(facts: &UnitFacts, reveal: &[UnitStatus]) -> bool {
    facts.awaiting_decision
        || unit_failed(facts)
        || facts.status.is_some_and(|status| reveal.contains(&status))
}

/// Extent of one scanned run, kept so updates can tell which runs observed a
/// changed position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
    /// First claimed position: the first superseded unit claimed before the
    /// first member, else the first member.
    first: usize,
    /// Last member.
    last: usize,
    /// One past the last examined position; `len + 1` when the run reached
    /// the end of the inputs and would examine units appended later.
    scanned_end: usize,
    /// Largest `scanned_end` of this run and every earlier run.
    reach: usize,
}

/// Groups over a sequence of units, updatable incrementally.
///
/// A run starts at each groupable tool unit not claimed by an earlier run and
/// claims every unit that joins it; runs shorter than their first member's
/// `min` claim their members without forming a group.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grouping {
    groups: Vec<UnitGroup>,
    runs: Vec<Run>,
    len: usize,
}

impl Grouping {
    /// Group `inputs` from scratch.
    #[must_use]
    pub fn compute(inputs: &[GroupingInput<'_>]) -> Self {
        let mut grouping = Self {
            len: inputs.len(),
            ..Self::default()
        };
        scan(
            inputs,
            0,
            |_| false,
            &mut grouping.runs,
            &mut grouping.groups,
        );
        fill_reach(&mut grouping.runs, 0);
        grouping
    }

    /// Update after the inputs changed only within `dirty`.
    ///
    /// `dirty` is a range of positions in the new `inputs`. It replaced the
    /// positions `dirty.start..dirty.end + previous len - inputs.len()` of the
    /// previous inputs; every position before `dirty` is unchanged, and every
    /// position after it is unchanged apart from shifting by the length
    /// difference. Appends, in-place updates, insertions, and removals are all
    /// splices of this form. The result always equals
    /// [`compute`](Self::compute) on `inputs`; an inconsistent range falls
    /// back to a full computation.
    ///
    /// Only runs that examined a replaced position are rescanned; scanning
    /// stops at the first unchanged position where the previous scan also
    /// started fresh, and later groups are reused (shifted).
    pub fn update(&mut self, inputs: &[GroupingInput<'_>], dirty: Range<usize>) {
        let old_len = self.len;
        let new_len = inputs.len();
        let consistent = dirty.start <= dirty.end
            && dirty.end <= new_len
            && dirty.end + old_len >= new_len + dirty.start;
        if !consistent {
            *self = Self::compute(inputs);
            return;
        }
        // Earlier runs examined only positions before `dirty`; keep them.
        let first_affected = self.runs.partition_point(|run| run.reach <= dirty.start);
        // An unclaimed superseded span right before `dirty` looked ahead into
        // it when deciding whether a run starts there, so rescan it too.
        let floor = first_affected
            .checked_sub(1)
            .map_or(0, |kept| self.runs[kept].last + 1);
        let mut back = dirty.start;
        while back > floor && dirty.start - back <= MAX_BRIDGED_GAP && inputs[back - 1].superseded {
            back -= 1;
        }
        let start = self
            .runs
            .get(first_affected)
            .map_or(back, |run| run.first.min(back));
        let old_runs = self.runs.split_off(first_affected);
        let old_groups = self
            .groups
            .split_off(self.groups.partition_point(|group| group.leader() < start));
        // A previous position started a fresh scan unless a run claimed it.
        let is_old_scan_point = |old: usize| {
            let next = old_runs.partition_point(|run| run.first < old);
            next == 0 || old_runs[next - 1].last < old
        };
        let resume = scan(
            inputs,
            start,
            |position| position >= dirty.end && is_old_scan_point(position + old_len - new_len),
            &mut self.runs,
            &mut self.groups,
        );
        let resume_old = resume + old_len - new_len;
        let shift = |position: usize| position + new_len - old_len;
        let tail_runs = old_runs.partition_point(|run| run.first < resume_old);
        self.runs
            .extend(old_runs[tail_runs..].iter().map(|run| Run {
                first: shift(run.first),
                last: shift(run.last),
                scanned_end: shift(run.scanned_end),
                reach: 0,
            }));
        let tail_groups = old_groups.partition_point(|group| group.leader() < resume_old);
        self.groups
            .extend(old_groups.into_iter().skip(tail_groups).map(|mut group| {
                if new_len != old_len {
                    for position in group
                        .members
                        .iter_mut()
                        .chain(&mut group.bridged)
                        .chain(&mut group.attached)
                    {
                        *position = shift(*position);
                    }
                }
                group
            }));
        fill_reach(&mut self.runs, first_affected);
        self.len = new_len;
    }

    /// Groups in transcript order.
    #[must_use]
    pub fn groups(&self) -> &[UnitGroup] {
        &self.groups
    }

    /// Take the groups, in transcript order.
    #[must_use]
    pub fn into_groups(self) -> Vec<UnitGroup> {
        self.groups
    }

    /// Number of inputs the grouping covers.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the grouping covers no inputs.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The group containing `position` (as a member, bridge, or attached
    /// unit) and its role.
    #[must_use]
    pub fn group_at(&self, position: usize) -> Option<(&UnitGroup, GroupRole)> {
        let index = self
            .groups
            .partition_point(|group| group.leader() <= position)
            .checked_sub(1)?;
        let group = &self.groups[index];
        group.role(position).map(|role| (group, role))
    }
}

/// Group units according to each unit's resolved `group` property.
///
/// A tool unit joins the open group when its `group.by` equals the group's,
/// it shares the group's turn, the key matches (same tool for
/// [`GroupBy::SameTool`]; same producer for [`GroupBy::SameProducer`]; any
/// tool for [`GroupBy::Consecutive`] and [`GroupBy::Turn`]), and every unit
/// since the previous member is one of the group's bridge kinds. Failed and
/// awaiting members stay members. Runs shorter than the first member's `min`
/// are not groups.
#[must_use]
pub fn group_units(inputs: &[GroupingInput<'_>]) -> Vec<UnitGroup> {
    Grouping::compute(inputs).into_groups()
}

/// Scan fresh runs from `from` until `stop` accepts a scan point or the
/// inputs end; returns where scanning stopped.
fn scan(
    inputs: &[GroupingInput<'_>],
    from: usize,
    mut stop: impl FnMut(usize) -> bool,
    runs: &mut Vec<Run>,
    groups: &mut Vec<UnitGroup>,
) -> usize {
    let mut position = from;
    while position < inputs.len() && !stop(position) {
        let Some(leader) = run_leader(inputs, position) else {
            position += 1;
            continue;
        };
        let RunScan {
            members,
            bridged,
            mut attached,
            scanned_end,
        } = scan_run(inputs, leader);
        if leader > position {
            // Superseded units directly before the first member.
            attached.splice(0..0, position..leader);
        }
        let last = members[members.len() - 1];
        runs.push(Run {
            first: position,
            last,
            scanned_end,
            reach: 0,
        });
        let min = usize::try_from(inputs[leader].group.min).unwrap_or(usize::MAX);
        if members.len() >= min.max(2) {
            groups.push(build_group(inputs, members, bridged, attached));
        }
        position = last + 1;
    }
    position
}

/// First member of a run claiming `position`, if one starts there.
///
/// A groupable unit leads its own run. A superseded unit starts a run when it
/// begins a contiguous span (at most [`MAX_BRIDGED_GAP`] long) of superseded
/// units in one turn that ends at a groupable unit of the same turn; the run
/// then claims the span as attached units. Looks only at positions from
/// `position` on, so a fresh scan from any unclaimed position agrees with a
/// full one.
fn run_leader(inputs: &[GroupingInput<'_>], position: usize) -> Option<usize> {
    let unit = &inputs[position];
    if groupable(unit) {
        return Some(position);
    }
    if !unit.superseded {
        return None;
    }
    let mut next = position + 1;
    while next < inputs.len() && next - position <= MAX_BRIDGED_GAP {
        let candidate = &inputs[next];
        if candidate.turn != unit.turn {
            return None;
        }
        if !candidate.superseded {
            return groupable(candidate).then_some(next);
        }
        next += 1;
    }
    None
}

/// Positions one run claims, and how far it looked.
struct RunScan {
    members: Vec<usize>,
    bridged: Vec<usize>,
    attached: Vec<usize>,
    /// One past the last examined position; `len + 1` at the end.
    scanned_end: usize,
}

/// Scan the run starting at `first`.
///
/// Positions between members are split into bridged units and attached
/// units owned by a member's invocation or superseded by a later unit; an
/// attached unit of an invocation with no member yet in the run must qualify
/// as an ordinary bridge to be spanned at all, unless it is superseded. When
/// the run ends, the owned attached units directly after its last member are
/// claimed too (they were examined, so `scanned_end` already covers them).
fn scan_run(inputs: &[GroupingInput<'_>], first: usize) -> RunScan {
    let leader = &inputs[first];
    let mut run = RunScan {
        members: vec![first],
        bridged: Vec::new(),
        attached: Vec::new(),
        scanned_end: inputs.len() + 1,
    };
    let mut pending: Vec<usize> = Vec::new();
    let mut position = first + 1;
    while position < inputs.len() {
        let unit = &inputs[position];
        if joins(leader, unit) {
            run.members.push(position);
            for between in std::mem::take(&mut pending) {
                if inputs[between].superseded || owned(inputs, &run.members, &inputs[between]) {
                    run.attached.push(between);
                } else {
                    run.bridged.push(between);
                }
            }
        } else if pending.len() < MAX_BRIDGED_GAP
            && bridges(leader, unit, owned(inputs, &run.members, unit))
        {
            pending.push(position);
        } else {
            run.scanned_end = position + 1;
            break;
        }
        position += 1;
    }
    // Superseded units are left for the run of the invocation they belong to.
    let trailing = pending
        .iter()
        .take_while(|&&after| {
            !inputs[after].superseded && owned(inputs, &run.members, &inputs[after])
        })
        .count();
    run.attached.extend_from_slice(&pending[..trailing]);
    run
}

/// Whether `unit` is attached to the invocation of one of `members`.
fn owned(inputs: &[GroupingInput<'_>], members: &[usize], unit: &GroupingInput<'_>) -> bool {
    unit.attached
        && unit.invocation.is_some_and(|invocation| {
            members
                .iter()
                .any(|&member| inputs[member].invocation == Some(invocation))
        })
}

fn fill_reach(runs: &mut [Run], from: usize) {
    let mut reach = from
        .checked_sub(1)
        .map_or(0, |previous| runs[previous].reach);
    for run in &mut runs[from..] {
        reach = reach.max(run.scanned_end);
        run.reach = reach;
    }
}

fn groupable(input: &GroupingInput<'_>) -> bool {
    !input.attached
        && !input.superseded
        && input.group.is_grouping()
        && input.facts.kind == Some(UnitKind::Tool)
}

fn joins(leader: &GroupingInput<'_>, next: &GroupingInput<'_>) -> bool {
    groupable(next)
        && next.group.by == leader.group.by
        && next.turn == leader.turn
        && match leader.group.by {
            GroupBy::None => false,
            GroupBy::Consecutive | GroupBy::Turn => true,
            GroupBy::SameTool => {
                leader.facts.tool.is_some() && leader.facts.tool == next.facts.tool
            }
            GroupBy::SameProducer => {
                leader.facts.producer.is_some() && leader.facts.producer == next.facts.producer
            }
        }
}

/// Whether `unit` may sit between members of the group led by `leader`.
///
/// Units awaiting a decision and failed units never bridge, so they are never
/// hidden inside a collapsed group they do not belong to. A unit attached to
/// the invocation of a member already in the run (`owned`) sits with that
/// call whatever its kind or status, and so does a superseded unit, whose
/// invocation is represented by a later unit; an attached unit of any other
/// invocation follows the ordinary bridge rules, so a foreign failure still
/// ends the group.
fn bridges(leader: &GroupingInput<'_>, unit: &GroupingInput<'_>, owned: bool) -> bool {
    unit.turn == leader.turn
        && (owned
            || unit.superseded
            || unit
                .facts
                .kind
                .is_some_and(|kind| leader.group.bridge.contains(&kind))
                && !unit.facts.awaiting_decision
                && !unit_failed(unit.facts))
}

fn build_group(
    inputs: &[GroupingInput<'_>],
    members: Vec<usize>,
    bridged: Vec<usize>,
    attached: Vec<usize>,
) -> UnitGroup {
    let first = inputs[members[0]];
    let shared = |value: fn(&UnitFacts) -> &Option<String>| {
        let expected = value(first.facts);
        if members
            .iter()
            .all(|&position| value(inputs[position].facts) == expected)
        {
            expected.clone()
        } else {
            None
        }
    };
    let status = members
        .iter()
        .filter_map(|&position| inputs[position].facts.status)
        .max_by_key(|status| status_rank(*status));
    let facts = UnitFacts {
        kind: Some(UnitKind::Group),
        tool: shared(|facts| &facts.tool),
        producer: shared(|facts| &facts.producer),
        status,
        is_error: members
            .iter()
            .any(|&position| unit_failed(inputs[position].facts)),
        turn: first.facts.turn,
        depth: first.facts.depth,
        items: SizeBucket::of(members.len()),
        ..UnitFacts::default()
    };
    UnitGroup {
        id: format!("group:{}", first.id),
        members,
        bridged,
        attached,
        facts,
    }
}

/// Aggregate status precedence: running > waiting > requested > failed >
/// timed out > cancelled > succeeded.
const fn status_rank(status: UnitStatus) -> u8 {
    match status {
        UnitStatus::Running => 6,
        UnitStatus::Waiting => 5,
        UnitStatus::Requested => 4,
        UnitStatus::Failed => 3,
        UnitStatus::TimedOut => 2,
        UnitStatus::Cancelled => 1,
        UnitStatus::Succeeded => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_defaults;
    use crate::models::TurnRecency;

    fn tool(name: &str, status: UnitStatus) -> UnitFacts {
        UnitFacts {
            kind: Some(UnitKind::Tool),
            tool: Some(name.to_owned()),
            status: Some(status),
            ..UnitFacts::default()
        }
    }

    fn kind(kind: UnitKind) -> UnitFacts {
        UnitFacts {
            kind: Some(kind),
            ..UnitFacts::default()
        }
    }

    /// Owned unit used to build borrowed inputs.
    #[derive(Debug, Clone)]
    struct Unit {
        id: String,
        turn: u64,
        facts: UnitFacts,
        group: GroupSpec,
        invocation: Option<String>,
        attached: bool,
        superseded: bool,
    }

    /// Units with ids `u<index + first_id>`.
    fn units_from(
        first_id: usize,
        facts: Vec<UnitFacts>,
        turns: &[u64],
        group: &GroupSpec,
    ) -> Vec<Unit> {
        facts
            .into_iter()
            .zip(turns)
            .enumerate()
            .map(|(index, (facts, turn))| Unit {
                id: format!("u{}", index + first_id),
                turn: *turn,
                facts,
                group: group.clone(),
                invocation: None,
                attached: false,
                superseded: false,
            })
            .collect()
    }

    fn units(facts: Vec<UnitFacts>, turns: &[u64], group: &GroupSpec) -> Vec<Unit> {
        units_from(0, facts, turns, group)
    }

    fn inputs(units: &[Unit]) -> Vec<GroupingInput<'_>> {
        units
            .iter()
            .map(|unit| GroupingInput {
                id: &unit.id,
                turn: unit.turn,
                facts: &unit.facts,
                group: &unit.group,
                invocation: unit.invocation.as_deref(),
                attached: unit.attached,
                superseded: unit.superseded,
            })
            .collect()
    }

    fn members(groups: &[UnitGroup]) -> Vec<Vec<usize>> {
        groups.iter().map(|group| group.members.clone()).collect()
    }

    fn same_tool_reads(count: usize) -> Vec<Unit> {
        units(
            vec![tool("read", UnitStatus::Succeeded); count],
            &vec![1; count],
            &GroupSpec::new(GroupBy::SameTool),
        )
    }

    #[test]
    fn consecutive_same_tool_runs_group_and_singletons_do_not() {
        let owned = units(
            vec![
                tool("read", UnitStatus::Succeeded),
                tool("read", UnitStatus::Succeeded),
                tool("read", UnitStatus::Running),
                tool("grep", UnitStatus::Succeeded),
                tool("read", UnitStatus::Succeeded),
            ],
            &[1; 5],
            &GroupSpec::new(GroupBy::SameTool),
        );
        let groups = group_units(&inputs(&owned));
        assert_eq!(members(&groups), [vec![0, 1, 2]]);
        assert_eq!(groups[0].id, "group:u0");
        assert_eq!(groups[0].span(), 0..3);
        assert_eq!(groups[0].facts.kind, Some(UnitKind::Group));
        assert_eq!(groups[0].facts.tool.as_deref(), Some("read"));
        assert_eq!(groups[0].facts.status, Some(UnitStatus::Running));
        assert_eq!(groups[0].facts.items, SizeBucket::Few);
    }

    #[test]
    fn any_tool_mode_mixes_tools_but_not_turns() {
        let owned = units(
            vec![
                tool("read", UnitStatus::Succeeded),
                tool("grep", UnitStatus::Succeeded),
                tool("glob", UnitStatus::Succeeded),
                tool("read", UnitStatus::Succeeded),
            ],
            &[1, 1, 2, 2],
            &GroupSpec::new(GroupBy::Consecutive),
        );
        let groups = group_units(&inputs(&owned));
        assert_eq!(members(&groups), [vec![0, 1], vec![2, 3]]);
        assert_eq!(groups[0].facts.tool, None);
    }

    #[test]
    fn failed_and_awaiting_members_stay_members_and_are_revealed() {
        let mut awaiting = tool("bash", UnitStatus::Waiting);
        awaiting.awaiting_decision = true;
        let owned = units(
            vec![
                tool("bash", UnitStatus::Succeeded),
                tool("bash", UnitStatus::Failed),
                tool("bash", UnitStatus::Succeeded),
                awaiting,
                tool("bash", UnitStatus::Succeeded),
                tool("bash", UnitStatus::Running),
            ],
            &[1; 6],
            &GroupSpec::new(GroupBy::SameTool),
        );
        let offered = inputs(&owned);
        let groups = group_units(&offered);
        assert_eq!(members(&groups), [vec![0, 1, 2, 3, 4, 5]]);
        assert!(groups[0].facts.is_error);
        assert_eq!(groups[0].facts.status, Some(UnitStatus::Running));
        assert!(!groups[0].facts.awaiting_decision);

        let mut presentation = host_defaults();
        presentation.reveal = Vec::new();
        assert_eq!(
            groups[0]
                .revealed_members(&offered, &presentation)
                .collect::<Vec<_>>(),
            [1, 3],
            "the floor reveals failed and awaiting members whatever reveal says"
        );
        presentation.reveal = vec![UnitStatus::Running];
        assert_eq!(
            groups[0]
                .revealed_members(&offered, &presentation)
                .collect::<Vec<_>>(),
            [1, 3, 5]
        );
    }

    #[test]
    fn aggregate_status_follows_precedence() {
        let cases = [
            (
                [UnitStatus::Succeeded, UnitStatus::Cancelled],
                UnitStatus::Cancelled,
            ),
            (
                [UnitStatus::Cancelled, UnitStatus::TimedOut],
                UnitStatus::TimedOut,
            ),
            (
                [UnitStatus::TimedOut, UnitStatus::Failed],
                UnitStatus::Failed,
            ),
            (
                [UnitStatus::Failed, UnitStatus::Requested],
                UnitStatus::Requested,
            ),
            (
                [UnitStatus::Requested, UnitStatus::Waiting],
                UnitStatus::Waiting,
            ),
            (
                [UnitStatus::Waiting, UnitStatus::Running],
                UnitStatus::Running,
            ),
        ];
        for (statuses, expected) in cases {
            for ordered in [statuses, [statuses[1], statuses[0]]] {
                let owned = units(
                    ordered.iter().map(|status| tool("read", *status)).collect(),
                    &[1, 1],
                    &GroupSpec::new(GroupBy::SameTool),
                );
                assert_eq!(group_units(&inputs(&owned))[0].facts.status, Some(expected));
            }
        }
    }

    #[test]
    fn group_facts_share_tool_and_producer_only_when_common() {
        let mut first = tool("read", UnitStatus::Succeeded);
        first.producer = Some("bcode.fs".into());
        first.turn = TurnRecency::Previous;
        first.depth = 2;
        let mut second = tool("grep", UnitStatus::Succeeded);
        second.producer = Some("bcode.fs".into());
        let owned = units(
            vec![first, second],
            &[1, 1],
            &GroupSpec::new(GroupBy::SameProducer),
        );
        let facts = &group_units(&inputs(&owned))[0].facts;
        assert_eq!(facts.tool, None);
        assert_eq!(facts.producer.as_deref(), Some("bcode.fs"));
        assert_eq!(facts.turn, TurnRecency::Previous);
        assert_eq!(facts.depth, 2);
        assert_eq!(facts.items, SizeBucket::Few);
        assert!(!facts.is_error);
    }

    #[test]
    fn bridges_sit_between_members_without_counting() {
        let owned = units(
            vec![
                tool("read", UnitStatus::Succeeded),
                kind(UnitKind::Reasoning),
                tool("read", UnitStatus::Succeeded),
                kind(UnitKind::Reasoning),
                kind(UnitKind::Reasoning),
                tool("read", UnitStatus::Succeeded),
                kind(UnitKind::Reasoning),
                kind(UnitKind::AssistantMessage),
            ],
            &[1; 8],
            &GroupSpec::new(GroupBy::SameTool),
        );
        let grouping = Grouping::compute(&inputs(&owned));
        let group = &grouping.groups()[0];
        assert_eq!(group.members, [0, 2, 5]);
        assert_eq!(group.bridged, [1, 3, 4], "trailing bridges are not inside");
        assert_eq!(group.facts.items, SizeBucket::Few);
        let role = |position| grouping.group_at(position).map(|(_, role)| role);
        assert_eq!(role(0), Some(GroupRole::Leader));
        assert_eq!(role(3), Some(GroupRole::Bridged));
        assert_eq!(role(5), Some(GroupRole::Member));
        assert_eq!(role(6), None);
    }

    /// A contribution attached to invocation `call`.
    fn contribution(call: &str, status: UnitStatus) -> Unit {
        let mut unit = units(
            vec![tool("read", status)],
            &[1],
            &GroupSpec::new(GroupBy::SameTool),
        )
        .remove(0);
        unit.invocation = Some(call.to_owned());
        unit.attached = true;
        unit
    }

    /// Same-tool reads with invocations `call<index>`, interleaved with
    /// `attached` contributions inserted at the given positions.
    fn calls_with(count: usize, attached: &[(usize, Unit)]) -> Vec<Unit> {
        let mut owned = same_tool_reads(count);
        for (index, unit) in owned.iter_mut().enumerate() {
            unit.invocation = Some(format!("call{index}"));
        }
        for (offset, (position, unit)) in attached.iter().enumerate() {
            let mut unit = unit.clone();
            unit.id = format!("a{offset}");
            owned.insert(*position, unit);
        }
        owned
    }

    #[test]
    fn attached_units_never_count_lead_or_split_groups() {
        // read, contribution(call0), read, failed contribution(call1),
        // read, contribution(call2).
        let owned = calls_with(
            3,
            &[
                (1, contribution("call0", UnitStatus::Succeeded)),
                (3, contribution("call1", UnitStatus::Failed)),
                (5, contribution("call2", UnitStatus::Succeeded)),
            ],
        );
        let offered = inputs(&owned);
        let grouping = Grouping::compute(&offered);
        assert_eq!(members(grouping.groups()), [vec![0, 2, 4]]);
        let group = &grouping.groups()[0];
        assert_eq!(group.bridged, [] as [usize; 0]);
        assert_eq!(
            group.attached,
            [1, 3, 5],
            "trailing contribution of call2 stays inside"
        );
        assert_eq!(group.facts.items, SizeBucket::Few);
        assert!(
            !group.facts.is_error,
            "an attached failure is not a member failure"
        );
        assert_eq!(group.facts.status, Some(UnitStatus::Succeeded));
        let role = |position| grouping.group_at(position).map(|(_, role)| role);
        assert_eq!(role(3), Some(GroupRole::Attached));
        assert_eq!(role(5), Some(GroupRole::Attached));
    }

    #[test]
    fn attached_units_alone_form_no_group_and_foreign_trailers_stay_outside() {
        let mut lone = vec![
            contribution("x", UnitStatus::Succeeded),
            contribution("y", UnitStatus::Succeeded),
            contribution("z", UnitStatus::Succeeded),
        ];
        lone[1].id = "b".into();
        lone[2].id = "c".into();
        assert_eq!(group_units(&inputs(&lone)), []);

        // One real call between contributions is not a group either.
        let single = calls_with(1, &[(0, contribution("call0", UnitStatus::Succeeded))]);
        assert_eq!(group_units(&inputs(&single)), []);

        // A trailing contribution of another invocation is not claimed, and
        // neither is anything after a foreign one.
        let owned = calls_with(
            2,
            &[
                (2, contribution("other", UnitStatus::Succeeded)),
                (3, contribution("call1", UnitStatus::Succeeded)),
            ],
        );
        let groups = group_units(&inputs(&owned));
        assert_eq!(members(&groups), [vec![0, 1]]);
        assert_eq!(groups[0].attached, [] as [usize; 0]);
    }

    #[test]
    fn foreign_attached_units_follow_bridge_rules_between_members() {
        // [write z (failed), read a, failed contribution of z, read b]: the
        // contribution belongs to no member, so it is not hidden inside the
        // reads' group; as a failure it cannot bridge and ends the run.
        let mut write = units(
            vec![tool("write", UnitStatus::Failed)],
            &[1],
            &GroupSpec::new(GroupBy::SameTool),
        )
        .remove(0);
        write.id = "w".into();
        write.invocation = Some("z".into());
        let owned = calls_with(2, &[(0, write), (2, contribution("z", UnitStatus::Failed))]);
        let grouping = Grouping::compute(&inputs(&owned));
        assert_eq!(grouping.groups(), []);
        assert_eq!(grouping.group_at(2), None);

        // A succeeded foreign contribution is still not a bridge kind here
        // (tool units do not bridge by default), so it also ends the run.
        let owned = calls_with(2, &[(1, contribution("elsewhere", UnitStatus::Succeeded))]);
        assert_eq!(group_units(&inputs(&owned)), []);

        // Where its kind bridges and it is healthy, it is spanned as an
        // ordinary bridge, not as an attachment.
        let mut owned = calls_with(2, &[(1, contribution("elsewhere", UnitStatus::Succeeded))]);
        for unit in &mut owned {
            unit.group.bridge = vec![UnitKind::Tool];
        }
        let groups = group_units(&inputs(&owned));
        assert_eq!(members(&groups), [vec![0, 2]]);
        assert_eq!(groups[0].bridged, [1]);
        assert_eq!(groups[0].attached, [] as [usize; 0]);
    }

    #[test]
    fn attached_units_never_cross_turns() {
        let mut owned = calls_with(2, &[(1, contribution("call0", UnitStatus::Succeeded))]);
        owned[1].turn = 2;
        assert_eq!(group_units(&inputs(&owned)), []);
    }

    #[test]
    fn non_bridge_kinds_and_floor_units_end_groups() {
        let mut awaiting = kind(UnitKind::Permission);
        awaiting.awaiting_decision = true;
        let spec = GroupSpec {
            bridge: vec![UnitKind::Reasoning, UnitKind::Permission],
            ..GroupSpec::new(GroupBy::SameTool)
        };
        let owned = units(
            vec![
                tool("read", UnitStatus::Succeeded),
                kind(UnitKind::AssistantMessage),
                tool("read", UnitStatus::Succeeded),
                tool("read", UnitStatus::Succeeded),
                awaiting,
                tool("read", UnitStatus::Succeeded),
                tool("read", UnitStatus::Succeeded),
            ],
            &[1; 7],
            &spec,
        );
        assert_eq!(
            members(&group_units(&inputs(&owned))),
            [vec![2, 3], vec![5, 6]]
        );

        let no_bridges = GroupSpec {
            bridge: Vec::new(),
            ..GroupSpec::new(GroupBy::SameTool)
        };
        assert_eq!(
            group_units(&inputs(&bridge_between(&no_bridges))),
            Vec::new()
        );
    }

    fn bridge_between(spec: &GroupSpec) -> Vec<Unit> {
        units(
            vec![
                tool("read", UnitStatus::Succeeded),
                kind(UnitKind::Reasoning),
                tool("read", UnitStatus::Succeeded),
            ],
            &[1; 3],
            spec,
        )
    }

    #[test]
    fn bridges_never_cross_turns_and_long_gaps_end_groups() {
        let spec = GroupSpec::new(GroupBy::SameTool);
        let mut crossing = bridge_between(&spec);
        crossing[1].turn = 2;
        crossing[2].turn = 2;
        assert_eq!(group_units(&inputs(&crossing)), Vec::new());

        let mut facts = vec![tool("read", UnitStatus::Succeeded)];
        facts.extend((0..=MAX_BRIDGED_GAP).map(|_| kind(UnitKind::Reasoning)));
        facts.push(tool("read", UnitStatus::Succeeded));
        let count = facts.len();
        let long = units(facts, &vec![1; count], &spec);
        assert_eq!(group_units(&inputs(&long)), Vec::new());
    }

    #[test]
    fn min_controls_group_formation_and_short_runs_claim_members() {
        let spec = GroupSpec {
            min: 3,
            ..GroupSpec::new(GroupBy::SameTool)
        };
        let owned = units(
            vec![
                tool("read", UnitStatus::Succeeded),
                tool("read", UnitStatus::Succeeded),
                tool("grep", UnitStatus::Succeeded),
                tool("grep", UnitStatus::Succeeded),
                tool("grep", UnitStatus::Succeeded),
            ],
            &[1; 5],
            &spec,
        );
        assert_eq!(members(&group_units(&inputs(&owned))), [vec![2, 3, 4]]);
    }

    #[test]
    fn mismatched_group_by_ends_runs() {
        let mut owned = same_tool_reads(3);
        owned[2].group = GroupSpec::new(GroupBy::Consecutive);
        assert_eq!(members(&group_units(&inputs(&owned))), [vec![0, 1]]);
    }

    #[test]
    fn group_identity_is_stable_as_members_stream_in() {
        let mut owned = same_tool_reads(2);
        let mut grouping = Grouping::compute(&inputs(&owned));
        let first = grouping.groups()[0].id.clone();
        owned.extend(units_from(
            2,
            vec![tool("read", UnitStatus::Running)],
            &[1],
            &GroupSpec::new(GroupBy::SameTool),
        ));
        grouping.update(&inputs(&owned), 2..3);
        assert_eq!(grouping.groups()[0].id, first);
        assert_eq!(grouping.groups()[0].members, [0, 1, 2]);
    }

    #[test]
    fn ungrouped_policy_and_non_tools_never_group() {
        let message = kind(UnitKind::AssistantMessage);
        let messages = units(
            vec![message.clone(), message],
            &[1, 1],
            &GroupSpec::new(GroupBy::Consecutive),
        );
        assert_eq!(group_units(&inputs(&messages)), Vec::new());
        let tools = units(
            vec![tool("read", UnitStatus::Succeeded); 2],
            &[1, 1],
            &GroupSpec::none(),
        );
        assert_eq!(group_units(&inputs(&tools)), Vec::new());
        assert_eq!(group_units(&[]), Vec::new());
        assert!(Grouping::compute(&[]).is_empty());
    }

    /// A historical request for `call`, superseded by a later unit.
    fn superseded_request(call: &str) -> Unit {
        let mut unit = units(
            vec![tool("read", UnitStatus::Succeeded)],
            &[1],
            &GroupSpec::new(GroupBy::SameTool),
        )
        .remove(0);
        unit.invocation = Some(call.to_owned());
        unit.superseded = true;
        unit
    }

    #[test]
    fn superseded_requests_count_once_and_attach_to_the_group() {
        // req a, result a, req b, result b: two invocations, not four.
        let owned = calls_with(
            2,
            &[
                (0, superseded_request("call0")),
                (2, superseded_request("call1")),
            ],
        );
        let offered = inputs(&owned);
        let grouping = Grouping::compute(&offered);
        assert_eq!(members(grouping.groups()), [vec![1, 3]]);
        let group = &grouping.groups()[0];
        assert_eq!(group.attached, [0, 2]);
        assert_eq!(group.start(), 0, "the leading request opens the group");
        assert_eq!(group.facts.items, SizeBucket::Few);
        assert_eq!(group.id, "group:u0", "identity follows the first member");
        let role = |position| grouping.group_at(position).map(|(_, role)| role);
        assert_eq!(role(2), Some(GroupRole::Attached));

        // req a, req b, result a, result b: both requests lead in.
        let owned = calls_with(
            2,
            &[
                (0, superseded_request("call0")),
                (1, superseded_request("call1")),
            ],
        );
        let groups = group_units(&inputs(&owned));
        assert_eq!(members(&groups), [vec![2, 3]]);
        assert_eq!(groups[0].attached, [0, 1]);

        // Status aggregation ignores the superseded request.
        let mut owned = calls_with(2, &[(0, superseded_request("call0"))]);
        owned[0].facts.status = Some(UnitStatus::Running);
        let groups = group_units(&inputs(&owned));
        assert_eq!(groups[0].facts.status, Some(UnitStatus::Succeeded));
    }

    #[test]
    fn superseded_units_alone_never_group_or_cross_turns() {
        let mut lone = vec![superseded_request("x"), superseded_request("y")];
        lone[1].id = "b".into();
        assert_eq!(group_units(&inputs(&lone)), []);

        // A request in another turn than the following call stays outside.
        let mut owned = calls_with(2, &[(0, superseded_request("call0"))]);
        owned[0].turn = 0;
        let groups = group_units(&inputs(&owned));
        assert_eq!(members(&groups), [vec![1, 2]]);
        assert_eq!(groups[0].attached, [] as [usize; 0]);

        // A trailing request whose result is elsewhere is not claimed.
        let owned = calls_with(2, &[(2, superseded_request("later"))]);
        let groups = group_units(&inputs(&owned));
        assert_eq!(groups[0].attached, [] as [usize; 0]);
    }

    #[test]
    fn editing_a_superseded_lead_in_updates_like_a_full_computation() {
        let mut owned = calls_with(2, &[(0, superseded_request("call0"))]);
        let mut grouping = Grouping::compute(&inputs(&owned));
        assert_eq!(grouping.groups()[0].attached, [0]);
        // Its call is replaced by an unrelated message: the request no longer
        // leads into a group and the remaining call stands alone.
        owned[1] = units(
            vec![kind(UnitKind::AssistantMessage)],
            &[1],
            &GroupSpec::new(GroupBy::SameTool),
        )
        .remove(0);
        let offered = inputs(&owned);
        grouping.update(&offered, 1..2);
        assert_eq!(grouping, Grouping::compute(&offered));
        assert_eq!(grouping.groups(), []);
        // Restoring a call right after the request regroups from the request.
        owned[1] = calls_with(1, &[]).remove(0);
        owned[1].id = "fresh".into();
        owned[1].invocation = Some("call0".into());
        let offered = inputs(&owned);
        grouping.update(&offered, 1..2);
        assert_eq!(grouping, Grouping::compute(&offered));
        assert_eq!(grouping.groups()[0].attached, [0]);
    }

    /// Small deterministic generator so the comparison tests need no
    /// dependency and reproduce exactly.
    struct Lcg(u64);

    impl Lcg {
        fn below(&mut self, bound: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            usize::try_from(self.0 >> 33).unwrap() % bound
        }
    }

    fn random_unit(rng: &mut Lcg, serial: &mut usize, turn: &mut u64) -> Unit {
        if rng.below(8) == 0 {
            *turn += 1;
        }
        *serial += 1;
        let kinds = [
            UnitKind::Tool,
            UnitKind::Tool,
            UnitKind::Tool,
            UnitKind::Reasoning,
            UnitKind::AssistantMessage,
            UnitKind::Permission,
        ];
        let statuses = [
            UnitStatus::Succeeded,
            UnitStatus::Succeeded,
            UnitStatus::Running,
            UnitStatus::Failed,
            UnitStatus::Waiting,
        ];
        let by = [
            GroupBy::None,
            GroupBy::Consecutive,
            GroupBy::SameTool,
            GroupBy::SameTool,
            GroupBy::SameProducer,
            GroupBy::Turn,
        ][rng.below(6)];
        let bridge = match rng.below(3) {
            0 => vec![UnitKind::Reasoning],
            1 => Vec::new(),
            _ => vec![UnitKind::Reasoning, UnitKind::Tool, UnitKind::Permission],
        };
        Unit {
            id: format!("i{serial}"),
            turn: *turn,
            facts: UnitFacts {
                kind: Some(kinds[rng.below(kinds.len())]),
                tool: Some(["read", "grep", "bash"][rng.below(3)].to_owned()),
                producer: Some(["a", "b"][rng.below(2)].to_owned()),
                status: Some(statuses[rng.below(statuses.len())]),
                awaiting_decision: rng.below(10) == 0,
                ..UnitFacts::default()
            },
            group: GroupSpec {
                by,
                bridge,
                min: u32::try_from(2 + rng.below(3)).unwrap(),
            },
            invocation: Some(format!("call{}", rng.below(4))),
            attached: rng.below(5) == 0,
            superseded: rng.below(5) == 0,
        }
    }

    #[test]
    fn incremental_updates_match_full_recomputation() {
        let mut rng = Lcg(0x5eed);
        for _ in 0..300 {
            let mut serial = 0;
            let mut turn = 0;
            let len = rng.below(40);
            let mut owned: Vec<Unit> = (0..len)
                .map(|_| random_unit(&mut rng, &mut serial, &mut turn))
                .collect();
            let mut grouping = Grouping::compute(&inputs(&owned));
            for _ in 0..20 {
                let start = rng.below(owned.len() + 1);
                let removed = rng.below(owned.len() - start + 1).min(4);
                let inserted = rng.below(5);
                // Keep turn ids non-decreasing around the splice, as in a real
                // transcript.
                let mut splice_turn = start.checked_sub(1).map_or(0, |at| owned[at].turn);
                let next_turn = owned.get(start + removed).map(|unit| unit.turn);
                let replacement: Vec<Unit> = (0..inserted)
                    .map(|_| {
                        let mut unit = random_unit(&mut rng, &mut serial, &mut splice_turn);
                        if let Some(next_turn) = next_turn {
                            unit.turn = unit.turn.min(next_turn);
                        }
                        unit
                    })
                    .collect();
                owned.splice(start..start + removed, replacement);
                let offered = inputs(&owned);
                grouping.update(&offered, start..start + inserted);
                assert_eq!(grouping, Grouping::compute(&offered), "{owned:#?}");
            }
        }
    }

    #[test]
    fn appends_extend_the_open_run_and_keep_earlier_groups() {
        let spec = GroupSpec::new(GroupBy::SameTool);
        let mut owned = units(
            vec![
                tool("read", UnitStatus::Succeeded),
                tool("read", UnitStatus::Succeeded),
                tool("grep", UnitStatus::Succeeded),
                tool("grep", UnitStatus::Succeeded),
            ],
            &[1; 4],
            &spec,
        );
        let mut grouping = Grouping::compute(&inputs(&owned));
        owned.extend(units_from(
            4,
            vec![kind(UnitKind::Reasoning), tool("grep", UnitStatus::Running)],
            &[1, 1],
            &spec,
        ));
        let offered = inputs(&owned);
        grouping.update(&offered, 4..6);
        assert_eq!(grouping, Grouping::compute(&offered));
        assert_eq!(members(grouping.groups()), [vec![0, 1], vec![2, 3, 5]]);
        assert_eq!(grouping.groups()[1].bridged, [4]);
    }

    #[test]
    fn inconsistent_dirty_ranges_fall_back_to_full_computation() {
        let owned = same_tool_reads(2);
        let offered = inputs(&owned);
        let mut grouping = Grouping::default();
        grouping.update(&offered, Range { start: 5, end: 1 });
        assert_eq!(grouping, Grouping::compute(&offered));
        assert_eq!(grouping.len(), 2);
    }
}
