//! Layer versions from a forge's push history.

use bcode_code_review_models::{LayerVersion, PullHistory, PullHistoryEvent, VersionSource};
use std::collections::{BTreeMap, BTreeSet};

/// Commits pushed within this many milliseconds of the next one count as one push (a forge lists
/// every commit of a push; only the last was ever the head).
const PUSH_GAP_MS: u64 = 60 * 1000;

/// The versions `histories` record.
///
/// Each force push's heads before and after (with who pushed), each push's newest commit, and the
/// base branch each was on (`current_bases` for the time after the last base change the history
/// knows).
#[must_use]
pub fn forge_versions(
    provider: &str,
    repository: &str,
    histories: &[PullHistory],
    current_bases: &BTreeMap<u64, String>,
) -> Vec<LayerVersion> {
    let mut versions = Vec::new();
    for history in histories {
        // The base branch at the start: the first change's old branch, else the current one.
        let mut base = history
            .events
            .iter()
            .find_map(|event| match event {
                PullHistoryEvent::BaseChanged { from, .. } => from.clone(),
                _ => None,
            })
            .or_else(|| current_bases.get(&history.number).cloned());
        let version =
            |head: &str, at_ms: u64, actor: Option<String>, base: Option<String>| LayerVersion {
                provider: provider.to_owned(),
                repository: repository.to_owned(),
                number: history.number,
                head_oid: head.to_owned(),
                base_oid: None,
                base_branch: base,
                seen_at_ms: at_ms,
                actor,
                sources: BTreeSet::from([VersionSource::Forge]),
            };
        for (index, event) in history.events.iter().enumerate() {
            match event {
                PullHistoryEvent::ForcePushed {
                    at_ms,
                    actor,
                    before_oid,
                    after_oid,
                } => {
                    // The head before was there until this push.
                    if let Some(before) = before_oid {
                        versions.push(version(before, at_ms.saturating_sub(1), None, base.clone()));
                    }
                    if let Some(after) = after_oid {
                        versions.push(version(after, *at_ms, actor.clone(), base.clone()));
                    }
                }
                PullHistoryEvent::CommitsPushed { at_ms, head_oid } => {
                    let pushed_with_next = matches!(
                        history.events.get(index + 1),
                        Some(PullHistoryEvent::CommitsPushed { at_ms: next, .. })
                            if next.saturating_sub(*at_ms) <= PUSH_GAP_MS
                    );
                    if !pushed_with_next {
                        versions.push(version(head_oid, *at_ms, None, base.clone()));
                    }
                }
                PullHistoryEvent::BaseChanged { to, .. } => {
                    if to.is_some() {
                        base.clone_from(to);
                    }
                }
            }
        }
    }
    versions
}
