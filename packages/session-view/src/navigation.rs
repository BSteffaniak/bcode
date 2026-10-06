//! Bounded, renderer-neutral navigation over already loaded session summaries.
//!
//! This projection never discovers sessions or reads history. Hosts load child pages through the
//! application boundary; missing parents and incomplete pages remain visible rather than guessed.

#[cfg(test)]
#[path = "navigation_tests.rs"]
mod tests;

use std::collections::{BTreeMap, BTreeSet};

use bcode_session_models::{SessionId, SessionSummary};

/// Location-scoped identity. Discovery in another location does not grant open authority.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SessionNavigationKey {
    /// Canonical session identity.
    pub session_id: SessionId,
    /// Owning location, when supplied by discovery.
    pub location_id: Option<String>,
}

impl SessionNavigationKey {
    /// Derive an identity from a portable summary.
    #[must_use]
    pub fn from_summary(summary: &SessionSummary) -> Self {
        Self {
            session_id: summary.id,
            location_id: summary
                .location
                .as_ref()
                .map(|location| location.location_id.clone()),
        }
    }

    /// Return the parent identity within the same location, if known.
    // Repository policy avoids redundant #[must_use] on Option-returning APIs;
    // Option already carries the result-use contract.
    #[allow(clippy::must_use_candidate)]
    pub fn parent(summary: &SessionSummary) -> Option<Self> {
        summary.parent_id.map(|session_id| Self {
            session_id,
            location_id: summary
                .location
                .as_ref()
                .map(|location| location.location_id.clone()),
        })
    }
}

/// One semantic row. Depth is not constrained by a renderer's geometry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTreeRow {
    /// Index into the caller's bounded loaded summary slice.
    pub source_index: usize,
    /// Semantic nesting depth; terminal indentation may degrade independently.
    pub depth: usize,
    /// Whether the row is shown as an ancestor of a matching descendant.
    pub ancestor_context: bool,
    /// Parent unavailable in the loaded slice, or relationship cycle detected.
    pub detached: bool,
}

/// A bounded projection, not a claim that all descendants have been discovered.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionTreeProjection {
    /// Visible rows in preorder.
    pub rows: Vec<SessionTreeRow>,
    /// More loaded visible rows exist beyond the requested presentation window.
    pub truncated: bool,
}

/// Project only supplied summaries, without recursively fetching descendants.
///
/// `matches` contains source indices matching a local search. `None` means ordinary collapsed
/// navigation; a search reveals matching rows and loaded ancestors regardless of expansion.
/// Duplicate location-scoped identities are not treated as authoritative ancestry.
#[must_use]
pub fn project_session_tree(
    summaries: &[SessionSummary],
    expanded: &BTreeSet<SessionNavigationKey>,
    matches: Option<&BTreeSet<usize>>,
    row_limit: usize,
) -> SessionTreeProjection {
    let mut identities = BTreeMap::new();
    let mut ambiguous = BTreeSet::new();
    for (index, summary) in summaries.iter().enumerate() {
        let key = SessionNavigationKey::from_summary(summary);
        if identities.insert(key.clone(), index).is_some() {
            ambiguous.insert(key);
        }
    }
    for key in ambiguous {
        identities.remove(&key);
    }
    let parents: Vec<_> = summaries
        .iter()
        .map(|summary| {
            SessionNavigationKey::parent(summary).and_then(|key| identities.get(&key).copied())
        })
        .collect();
    let mut children = vec![Vec::new(); summaries.len()];
    let mut roots = Vec::new();
    for (index, parent) in parents.iter().enumerate() {
        if let Some(parent) = parent {
            children[*parent].push(index);
        } else {
            roots.push(index);
        }
    }
    let mut included = BTreeSet::new();
    if let Some(matches) = matches {
        let mut pending: Vec<_> = matches
            .iter()
            .copied()
            .filter(|index| *index < summaries.len())
            .collect();
        while let Some(index) = pending.pop() {
            if included.insert(index)
                && let Some(parent) = parents[index]
            {
                pending.push(parent);
            }
        }
    }
    let mut result = SessionTreeProjection::default();
    let mut visited = BTreeSet::new();
    let mut pending: Vec<_> = roots
        .into_iter()
        .rev()
        .map(|index| (index, 0, false))
        .collect();
    // A cycle has no root. The second pass surfaces otherwise unreachable loaded entries.
    for fallback in 0..=summaries.len() {
        while let Some((index, depth, cycle)) = pending.pop() {
            if !visited.insert(index) {
                continue;
            }
            if matches.is_some() && !included.contains(&index) {
                continue;
            }
            if result.rows.len() == row_limit {
                result.truncated = true;
                return result;
            }
            result.rows.push(SessionTreeRow {
                source_index: index,
                depth,
                ancestor_context: matches.is_some_and(|matches| !matches.contains(&index)),
                detached: cycle
                    || (summaries[index].parent_id.is_some() && parents[index].is_none()),
            });
            if matches.is_some()
                || expanded.contains(&SessionNavigationKey::from_summary(&summaries[index]))
            {
                pending.extend(
                    children[index]
                        .iter()
                        .rev()
                        .map(|child| (*child, depth.saturating_add(1), cycle)),
                );
            } else {
                // Mark collapsed loaded descendants without exposing them as detached roots.
                let mut hidden = children[index].clone();
                while let Some(child) = hidden.pop() {
                    if visited.insert(child) {
                        hidden.extend(children[child].iter().copied());
                    }
                }
            }
        }
        if fallback < summaries.len() && !visited.contains(&fallback) {
            pending.push((fallback, 0, true));
        }
    }
    result
}
