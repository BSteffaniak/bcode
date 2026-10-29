//! Host-owned finding provenance and conservative operation-local deduplication.
use std::collections::{BTreeMap, BTreeSet};

use crate::analysis_contracts::{CoverageIssue, DispositionKind, Outline};

#[derive(Clone)]
pub(super) struct Finding {
    pub issue: CoverageIssue,
    pub window_ids: BTreeSet<String>,
}

pub(super) struct Ownership {
    chapters: BTreeMap<String, BTreeSet<String>>,
}

impl Ownership {
    pub fn new(outline: &Outline) -> Self {
        let chapters = outline
            .dispositions
            .iter()
            .map(|decision| {
                let owners = if decision.disposition == DispositionKind::Duplicate {
                    outline
                        .dispositions
                        .iter()
                        .filter(|target| decision.duplicate_of.contains(&target.item_id))
                        .flat_map(|target| target.chapter_ids.iter().cloned())
                        .collect()
                } else {
                    decision.chapter_ids.iter().cloned().collect()
                };
                (decision.item_id.clone(), owners)
            })
            .collect();
        Self { chapters }
    }

    pub fn owners(&self, item_id: &str) -> impl Iterator<Item = &String> {
        self.chapters.get(item_id).into_iter().flatten()
    }

    pub fn finding(&self, mut issue: CoverageIssue, window_ids: BTreeSet<String>) -> Finding {
        let mut chapters: BTreeSet<_> = issue.chapter_ids.into_iter().collect();
        for item in &issue.item_ids {
            chapters.extend(self.owners(item).cloned());
        }
        issue.chapter_ids = chapters.into_iter().collect();
        issue.item_ids.sort();
        issue.item_ids.dedup();
        issue.description = issue.description.trim().to_owned();
        Finding { issue, window_ids }
    }
}

#[derive(Default)]
pub(super) struct Findings {
    entries: Vec<Finding>,
}

impl Findings {
    pub fn record(&mut self, findings: Vec<Finding>) -> Vec<Finding> {
        let mut current = Vec::new();
        for mut finding in findings {
            // Model IDs and recursive batch paths are not stable identities. Only
            // exact normalized obligations with the same source/item anchors are
            // deduplicated. Empty item sets cannot distinguish independent missed
            // source facts, even when their descriptions happen to be identical.
            if let Some(prior) = self.entries.iter_mut().find(|prior| {
                !finding.issue.item_ids.is_empty()
                    && prior.window_ids == finding.window_ids
                    && prior.issue.kind == finding.issue.kind
                    && prior.issue.item_ids == finding.issue.item_ids
                    && prior.issue.chapter_ids == finding.issue.chapter_ids
                    && prior.issue.description == finding.issue.description
            }) {
                prior.window_ids.extend(finding.window_ids);
                if !current
                    .iter()
                    .any(|finding: &Finding| finding.issue.id == prior.issue.id)
                {
                    current.push(prior.clone());
                }
            } else {
                finding.issue.id = format!("finding-{}", self.entries.len());
                current.push(finding.clone());
                self.entries.push(finding);
            }
        }
        current
    }

    pub fn entries(&self) -> &[Finding] {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis_contracts::{ChapterPlan, CoverageIssueKind, ItemDisposition};

    fn outline() -> Outline {
        Outline {
            title: "Source".into(),
            chapters: vec![ChapterPlan {
                id: "chapter".into(),
                title: "One".into(),
                item_ids: vec!["retained".into()],
            }],
            dispositions: vec![
                ItemDisposition {
                    item_id: "retained".into(),
                    disposition: DispositionKind::Retained,
                    chapter_ids: vec!["chapter".into()],
                    duplicate_of: vec![],
                    reason: "source".into(),
                },
                ItemDisposition {
                    item_id: "duplicate".into(),
                    disposition: DispositionKind::Duplicate,
                    chapter_ids: vec![],
                    duplicate_of: vec!["retained".into()],
                    reason: "same source fact".into(),
                },
            ],
        }
    }

    fn issue(id: &str, description: &str) -> CoverageIssue {
        CoverageIssue {
            id: id.into(),
            kind: CoverageIssueKind::Missing,
            item_ids: vec!["duplicate".into()],
            chapter_ids: vec![],
            description: description.into(),
        }
    }

    #[test]
    fn renamed_obligations_match_only_with_identical_source_anchors() {
        let ownership = Ownership::new(&outline());
        let mut findings = Findings::default();
        let _ = findings.record(vec![
            ownership.finding(issue("model-1", "retain 19"), ["window-0".into()].into()),
        ]);
        let _ = findings.record(vec![
            ownership.finding(issue("renamed", "retain 19 "), ["window-0".into()].into()),
        ]);
        let _ = findings.record(vec![ownership.finding(
            issue("model-1", "retain caveat"),
            ["window-1".into()].into(),
        )]);
        assert_eq!(findings.entries().len(), 2);
        let first = &findings.entries()[0];
        assert_eq!(first.issue.id, "finding-0");
        assert_eq!(first.issue.chapter_ids, ["chapter"]);
        assert_eq!(first.window_ids, ["window-0".into()].into());
        assert_ne!(first.issue.id, findings.entries()[1].issue.id);
    }
    #[test]
    fn independent_source_facts_and_unanchored_omissions_are_never_collapsed() {
        let ownership = Ownership::new(&outline());
        let mut findings = Findings::default();
        let first = ownership.finding(issue("same-id", "same words"), ["window-0".into()].into());
        let second = ownership.finding(issue("same-id", "same words"), ["window-1".into()].into());
        let _ = findings.record(vec![first, second]);
        let mut unanchored = issue("same-id", "same words");
        unanchored.item_ids.clear();
        unanchored.chapter_ids.push("chapter".into());
        let missing = ownership.finding(unanchored, ["window-0".into()].into());
        let _ = findings.record(vec![missing.clone(), missing]);
        assert_eq!(findings.entries().len(), 4);
        let identities: BTreeSet<_> = findings
            .entries()
            .iter()
            .map(|finding| &finding.issue.id)
            .collect();
        assert_eq!(identities.len(), 4);
    }
    #[test]
    fn active_obligations_do_not_include_historical_corrections() {
        let ownership = Ownership::new(&outline());
        let mut history = Findings::default();
        let first = history.record(vec![
            ownership.finding(issue("old", "retain 19"), ["window-0".into()].into()),
        ]);
        let active = history.record(vec![
            ownership.finding(issue("new", "retain caveat"), ["window-0".into()].into()),
        ]);
        assert_eq!(active.len(), 1);
        assert_eq!(history.entries().len(), 2);
        assert_ne!(active[0].issue.id, first[0].issue.id);
    }
}
