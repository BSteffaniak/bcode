//! Stable grouping of consecutive units.
//!
//! Grouping is computed once, renderer-neutrally, so every frontend agrees on
//! which units a group contains and on its identity. A group's id derives from
//! its first member, so it stays stable while later members stream in and
//! across resizes. Grouping never reorders units and never crosses turns.
//!
//! The computation is linear in the number of units supplied. Renderers pass
//! their bounded resident window, so work stays bounded by that window.

use std::ops::Range;

use crate::models::{Grouping, SizeBucket, UnitFacts, UnitKind, UnitStatus};

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
    pub group: Grouping,
}

/// A run of two or more consecutive units presented as one group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitGroup {
    /// Stable group identity derived from the first member.
    pub id: String,
    /// Member positions within the supplied inputs.
    pub members: Range<usize>,
    /// Facts describing the group itself, for resolving its presentation.
    pub facts: UnitFacts,
}

/// Group consecutive units according to each unit's resolved `group` property.
///
/// A unit joins the current run when both it and the run use the same
/// grouping mode, they share a turn, and the mode's key matches (same tool for
/// [`Grouping::ConsecutiveSameTool`]; any tool for
/// [`Grouping::ConsecutiveTools`]). Units awaiting a decision or representing
/// failures are never grouped, so they always remain individually visible.
/// Runs of one unit are not groups.
#[must_use]
pub fn group_units(inputs: &[GroupingInput<'_>]) -> Vec<UnitGroup> {
    let mut groups = Vec::new();
    let mut start = 0;
    while start < inputs.len() {
        let mut end = start + 1;
        if groupable(&inputs[start]) {
            while end < inputs.len() && joins(&inputs[start], &inputs[end]) {
                end += 1;
            }
        }
        if end - start >= 2 {
            groups.push(build_group(inputs, start..end));
        }
        start = end;
    }
    groups
}

fn groupable(input: &GroupingInput<'_>) -> bool {
    input.group != Grouping::None
        && input.facts.kind == Some(UnitKind::Tool)
        && !input.facts.awaiting_decision
        && !input.facts.is_error
        && !matches!(
            input.facts.status,
            Some(UnitStatus::Failed | UnitStatus::TimedOut)
        )
}

fn joins(first: &GroupingInput<'_>, next: &GroupingInput<'_>) -> bool {
    groupable(next)
        && next.group == first.group
        && next.turn == first.turn
        && match first.group {
            Grouping::None => false,
            Grouping::ConsecutiveTools => true,
            Grouping::ConsecutiveSameTool => {
                first.facts.tool.is_some() && first.facts.tool == next.facts.tool
            }
        }
}

fn build_group(inputs: &[GroupingInput<'_>], members: Range<usize>) -> UnitGroup {
    let slice = &inputs[members.clone()];
    let first = slice[0];
    let same_tool = slice
        .iter()
        .all(|input| input.facts.tool == first.facts.tool);
    let status = aggregate_status(slice.iter().filter_map(|input| input.facts.status));
    UnitGroup {
        id: format!("group:{}", first.id),
        members: members.clone(),
        facts: UnitFacts {
            kind: Some(UnitKind::Group),
            tool: if same_tool {
                first.facts.tool.clone()
            } else {
                None
            },
            producer: if slice
                .iter()
                .all(|input| input.facts.producer == first.facts.producer)
            {
                first.facts.producer.clone()
            } else {
                None
            },
            schema: None,
            status,
            is_error: false,
            awaiting_decision: false,
            turn: first.facts.turn,
            depth: first.facts.depth,
            lines: SizeBucket::Empty,
            items: SizeBucket::of(members.len()),
            plugin: std::collections::BTreeMap::new(),
        },
    }
}

/// In-progress members dominate, then cancellations, then success.
fn aggregate_status(statuses: impl Iterator<Item = UnitStatus>) -> Option<UnitStatus> {
    statuses.fold(None, |aggregate, status| {
        let rank = |status: UnitStatus| match status {
            UnitStatus::Running => 4,
            UnitStatus::Waiting => 3,
            UnitStatus::Requested => 2,
            UnitStatus::Cancelled => 1,
            _ => 0,
        };
        match aggregate {
            Some(current) if rank(current) >= rank(status) => Some(current),
            _ => Some(status),
        }
    })
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

    fn inputs<'a>(
        facts: &'a [UnitFacts],
        ids: &'a [String],
        turns: &[u64],
        group: Grouping,
    ) -> Vec<GroupingInput<'a>> {
        facts
            .iter()
            .zip(ids)
            .zip(turns)
            .map(|((facts, id), turn)| GroupingInput {
                id,
                turn: *turn,
                facts,
                group,
            })
            .collect()
    }

    fn ids(count: usize) -> Vec<String> {
        (0..count).map(|index| format!("u{index}")).collect()
    }

    #[test]
    fn consecutive_same_tool_runs_group_and_singletons_do_not() {
        let facts = [
            tool("read", UnitStatus::Succeeded),
            tool("read", UnitStatus::Succeeded),
            tool("read", UnitStatus::Running),
            tool("grep", UnitStatus::Succeeded),
            tool("read", UnitStatus::Succeeded),
        ];
        let ids = ids(facts.len());
        let groups = group_units(&inputs(
            &facts,
            &ids,
            &[1; 5],
            Grouping::ConsecutiveSameTool,
        ));
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].members, 0..3);
        assert_eq!(groups[0].id, "group:u0");
        assert_eq!(groups[0].facts.kind, Some(UnitKind::Group));
        assert_eq!(groups[0].facts.tool.as_deref(), Some("read"));
        assert_eq!(groups[0].facts.status, Some(UnitStatus::Running));
        assert_eq!(groups[0].facts.items, SizeBucket::Few);
    }

    #[test]
    fn any_tool_mode_mixes_tools_but_not_turns() {
        let facts = [
            tool("read", UnitStatus::Succeeded),
            tool("grep", UnitStatus::Succeeded),
            tool("glob", UnitStatus::Succeeded),
            tool("read", UnitStatus::Succeeded),
        ];
        let ids = ids(facts.len());
        let groups = group_units(&inputs(
            &facts,
            &ids,
            &[1, 1, 2, 2],
            Grouping::ConsecutiveTools,
        ));
        assert_eq!(
            groups
                .iter()
                .map(|group| group.members.clone())
                .collect::<Vec<_>>(),
            [0..2, 2..4]
        );
        assert_eq!(groups[0].facts.tool, None);
    }

    #[test]
    fn failures_and_decisions_break_groups_and_stay_standalone() {
        let mut awaiting = tool("bash", UnitStatus::Waiting);
        awaiting.awaiting_decision = true;
        let facts = [
            tool("bash", UnitStatus::Succeeded),
            tool("bash", UnitStatus::Failed),
            tool("bash", UnitStatus::Succeeded),
            awaiting,
            tool("bash", UnitStatus::Succeeded),
            tool("bash", UnitStatus::Succeeded),
        ];
        let ids = ids(facts.len());
        let groups = group_units(&inputs(
            &facts,
            &ids,
            &[1; 6],
            Grouping::ConsecutiveSameTool,
        ));
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].members, 4..6);
    }

    #[test]
    fn group_identity_is_stable_as_members_stream_in() {
        let mut facts = vec![
            tool("read", UnitStatus::Succeeded),
            tool("read", UnitStatus::Succeeded),
        ];
        let first = {
            let ids = ids(facts.len());
            group_units(&inputs(
                &facts,
                &ids,
                &[1, 1],
                Grouping::ConsecutiveSameTool,
            ))[0]
                .id
                .clone()
        };
        facts.push(tool("read", UnitStatus::Running));
        let ids = ids(facts.len());
        let grown = group_units(&inputs(
            &facts,
            &ids,
            &[1, 1, 1],
            Grouping::ConsecutiveSameTool,
        ));
        assert_eq!(grown[0].id, first);
        assert_eq!(grown[0].members, 0..3);
    }

    #[test]
    fn ungrouped_policy_and_non_tools_never_group() {
        let message = UnitFacts {
            kind: Some(UnitKind::AssistantMessage),
            ..UnitFacts::default()
        };
        let facts = [message.clone(), message];
        let ids = ids(2);
        assert_eq!(
            group_units(&inputs(&facts, &ids, &[1, 1], Grouping::ConsecutiveTools)),
            Vec::new()
        );
        let tools = [
            tool("read", UnitStatus::Succeeded),
            tool("read", UnitStatus::Succeeded),
        ];
        assert_eq!(
            group_units(&inputs(&tools, &ids, &[1, 1], Grouping::None)),
            Vec::new()
        );
        assert_eq!(group_units(&[]), Vec::new());
    }
}
