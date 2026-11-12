//! Re-review: what changed since the last recorded review pass.
//!
//! * A durable review whose sources now resolve to other head commits than its
//!   `reviewed_revisions` is compared per changed source (`review.revisions.compare`): a
//!   fast-forward (the old head is an ancestor of the new one) can be reviewed as the diff from
//!   the old head to the new head; anything else (a rebase, or a head that no longer exists) is
//!   reviewed in full.
//! * For a rebased source with both reviewed commits known, the patch ids of its files in the
//!   old and the new diff are compared (`review.patch_ids.get`, bounded chunks); files whose
//!   patch is unchanged are marked viewed with their new identity and shown de-emphasized.
//! * The "changes since last review" view replaces each fast-forwarded source with its
//!   `old head..new head` range, keeps rebased sources whole, and leaves unchanged sources out.
//!   The pass is recorded only when the reviewer marks it complete or publishes.

use crate::services::ReviewServices;
use bcode_code_review_models::{
    REVIEW_PATCH_IDS_MAX_PATHS, ReviewPatchIdsRequest, ReviewPatchIdsResponse,
    ReviewRevisionsCompareRequest, ReviewRevisionsCompareResponse, ReviewSource, ReviewSourceKind,
    ReviewedRevision, RevisionRef,
};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// How one source changed since the last pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedSource {
    /// Workspace source id.
    pub source_id: String,
    /// What the last pass recorded.
    pub reviewed: ReviewedRevision,
    /// Head commit the source resolves to now.
    pub new_head: String,
    /// The comparison, or `None` when it failed.
    pub compare: Option<ReviewRevisionsCompareResponse>,
}

impl ChangedSource {
    /// Whether the source only gained commits on top of the reviewed head.
    #[must_use]
    pub fn fast_forward(&self) -> bool {
        self.compare.is_some_and(|compare| compare.fast_forward)
    }
}

/// Every source whose head moved since the last pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RereviewPlan {
    /// Changed sources, in workspace order.
    pub changed: Vec<ChangedSource>,
}

impl RereviewPlan {
    /// Whether some source moved.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.changed.is_empty()
    }

    /// Commits added on top of fast-forwarded sources.
    #[must_use]
    pub fn new_commits(&self) -> u32 {
        self.changed
            .iter()
            .filter(|source| source.fast_forward())
            .filter_map(|source| source.compare.and_then(|compare| compare.commits_ahead))
            .sum()
    }

    /// Whether some source was rewritten (not a fast-forward).
    #[must_use]
    pub fn rebased(&self) -> bool {
        self.changed.iter().any(|source| !source.fast_forward())
    }

    /// One-line summary for the status line and toasts.
    #[must_use]
    pub fn summary(&self) -> String {
        let commits = self.new_commits();
        match (commits > 0, self.rebased()) {
            (true, true) => format!(
                "{commits} new commit{} and a rebase since your last review",
                if commits == 1 { "" } else { "s" }
            ),
            (true, false) => format!(
                "{commits} new commit{} since your last review",
                if commits == 1 { "" } else { "s" }
            ),
            (false, true) => "rebased since your last review".to_owned(),
            (false, false) => "changed since your last review".to_owned(),
        }
    }

    /// The sources of the "changes since last review" view: fast-forwarded sources as their
    /// `old head..new head` range, rebased sources whole, unchanged sources left out.
    #[must_use]
    pub fn delta_sources(&self, sources: &[ReviewSource]) -> Vec<ReviewSource> {
        sources
            .iter()
            .map(|source| {
                let mut source = source.clone();
                match self
                    .changed
                    .iter()
                    .find(|changed| changed.source_id == source.id)
                {
                    Some(changed) if changed.fast_forward() => {
                        if let Some(old) = changed.reviewed.reviewed_head_oid.clone() {
                            source.label = format!("Since last review · {}", source.label);
                            source.kind = ReviewSourceKind::CommitRange {
                                base: old,
                                head: changed.new_head.clone(),
                                merge_base: false,
                            };
                        }
                    }
                    Some(_) => {}
                    None => source.included = false,
                }
                source
            })
            .collect()
    }
}

/// Compare every changed source's reviewed head with its new head.
pub async fn analyze(
    services: ReviewServices,
    repo_path: PathBuf,
    changed: Vec<(String, ReviewedRevision, String)>,
) -> RereviewPlan {
    let mut plan = RereviewPlan::default();
    for (source_id, reviewed, new_head) in changed {
        let compare = match &reviewed.reviewed_head_oid {
            Some(old) => services
                .call(ReviewRevisionsCompareRequest {
                    repo_path: repo_path.clone(),
                    old_oid: old.clone(),
                    new_oid: new_head.clone(),
                })
                .await
                .ok(),
            None => None,
        };
        plan.changed.push(ChangedSource {
            source_id,
            reviewed,
            new_head,
            compare,
        });
    }
    plan
}

/// One rebased source group whose files' patches are compared.
#[derive(Debug, Clone)]
pub struct PatchComparison {
    /// Opaque group id returned with the results.
    pub group: usize,
    /// Reviewed base and head commits.
    pub old: (String, String),
    /// Current base and head.
    pub new: (RevisionRef, RevisionRef),
    /// Paths of the group's files.
    pub paths: Vec<String>,
}

/// Paths (per group) whose patch is the same in the reviewed and the current diff, with the
/// current patch id.
///
/// # Errors
///
/// Returns the first failing call's message.
pub async fn unchanged_patches(
    services: ReviewServices,
    repo_path: PathBuf,
    comparisons: Vec<PatchComparison>,
) -> Result<Vec<(usize, String, String)>, String> {
    let mut unchanged = Vec::new();
    for comparison in comparisons {
        for chunk in comparison.paths.chunks(REVIEW_PATCH_IDS_MAX_PATHS) {
            let ids = |base: RevisionRef, head: RevisionRef| ReviewPatchIdsRequest {
                repo_path: repo_path.clone(),
                base,
                head,
                paths: chunk.to_vec(),
            };
            let old: ReviewPatchIdsResponse = services
                .call(ids(
                    RevisionRef::commit(&comparison.old.0),
                    RevisionRef::commit(&comparison.old.1),
                ))
                .await
                .map_err(|error| error.message)?;
            let new: ReviewPatchIdsResponse = services
                .call(ids(comparison.new.0.clone(), comparison.new.1.clone()))
                .await
                .map_err(|error| error.message)?;
            let old: BTreeMap<String, String> = old.patch_ids;
            for (path, id) in new.patch_ids {
                if old.get(&path) == Some(&id) {
                    unchanged.push((comparison.group, path, id));
                }
            }
        }
    }
    Ok(unchanged)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(id: &str) -> ReviewSource {
        ReviewSource {
            id: id.into(),
            label: format!("Source {id}"),
            kind: ReviewSourceKind::LastCommit,
            included: true,
        }
    }

    fn changed(id: &str, fast_forward: bool, commits: u32) -> ChangedSource {
        ChangedSource {
            source_id: id.into(),
            reviewed: ReviewedRevision {
                reviewed_base_oid: Some("b".into()),
                reviewed_head_oid: Some("old".into()),
                recorded_at_ms: None,
            },
            new_head: "new".into(),
            compare: Some(ReviewRevisionsCompareResponse {
                old_exists: true,
                fast_forward,
                commits_ahead: Some(commits),
            }),
        }
    }

    #[test]
    fn delta_view_ranges_fast_forwards_keeps_rebases_and_drops_unchanged() {
        let plan = RereviewPlan {
            changed: vec![changed("a", true, 2), changed("b", false, 5)],
        };
        assert_eq!(plan.new_commits(), 2);
        assert!(plan.rebased());
        assert_eq!(
            plan.summary(),
            "2 new commits and a rebase since your last review"
        );
        let sources = plan.delta_sources(&[source("a"), source("b"), source("c")]);
        assert_eq!(
            sources[0].kind,
            ReviewSourceKind::CommitRange {
                base: "old".into(),
                head: "new".into(),
                merge_base: false
            }
        );
        assert_eq!(sources[0].label, "Since last review · Source a");
        assert_eq!(sources[1].kind, ReviewSourceKind::LastCommit);
        assert!(sources[1].included && !sources[2].included);
        let plan = RereviewPlan {
            changed: vec![changed("a", true, 1)],
        };
        assert_eq!(plan.summary(), "1 new commit since your last review");
    }
}
