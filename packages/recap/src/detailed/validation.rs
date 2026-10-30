//! Validation of untrusted stage results; declarations are never provenance.
use std::collections::{BTreeMap, BTreeSet};

use super::{CompositionError, Result};
use crate::{
    analysis_contracts::{
        ChapterComposition, ChapterPlan, DispositionKind, InformationItem, Outline, SourceWindow,
        VisualChoice,
    },
    contracts::{BlockContent, RecapDocument, TimeRange},
    model::ImageAsset,
};

pub(super) const fn check(condition: bool, message: &'static str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(CompositionError::Invalid(message))
    }
}

pub(super) fn id(value: &str) -> Result<()> {
    check(
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
        "invalid analysis identity",
    )
}

pub(super) fn ids(values: &[String]) -> Result<BTreeSet<&str>> {
    let mut seen = BTreeSet::new();
    for value in values {
        id(value)?;
        check(seen.insert(value.as_str()), "duplicate analysis identity")?;
    }
    Ok(seen)
}

pub(super) fn exact_ids<'a>(
    actual: &[String],
    expected: impl Iterator<Item = &'a str>,
) -> Result<()> {
    check(
        ids(actual)? == expected.collect(),
        "incomplete or fabricated identity set",
    )
}

pub(super) const fn contains(outer: TimeRange, inner: TimeRange) -> bool {
    outer.start_ms <= inner.start_ms && inner.end_ms <= outer.end_ms
}

pub(super) fn validate_images(document: &RecapDocument, images: &[ImageAsset]) -> Result<()> {
    let mut supplied = BTreeSet::new();
    for image in images {
        // Record identity independently: validation must not rely on a mutation
        // hidden behind short-circuit boolean evaluation.
        let unique = supplied.insert(image.asset_id.as_str());
        let registered = document
            .assets
            .iter()
            .find(|asset| asset.id == image.asset_id);
        check(
            !image.bytes.is_empty()
                && unique
                && registered.is_some_and(|asset| {
                    asset.mime_type == image.mime_type && asset.mime_type.starts_with("image/")
                }),
            "invalid source image",
        )?;
    }
    check(
        document
            .assets
            .iter()
            .filter(|asset| asset.mime_type.starts_with("image/"))
            .all(|asset| supplied.contains(asset.id.as_str())),
        "source image bytes missing",
    )
}

pub(super) fn validate_inventory(
    document: &RecapDocument,
    window: &SourceWindow,
    items: &[InformationItem],
    limitations: &[String],
) -> Result<()> {
    check(
        !items.is_empty() || !limitations.is_empty(),
        "empty unexplained inventory",
    )?;
    check(
        limitations.iter().all(|text| !text.trim().is_empty()),
        "empty limitation",
    )?;
    ids(&items.iter().map(|item| item.id.clone()).collect::<Vec<_>>())?;
    for item in items {
        item.range.validate(document.source.duration_ms)?;
        check(
            item.window_id == window.id
                && contains(window.range, item.range)
                && !item.text.trim().is_empty(),
            "invalid inventory source window",
        )?;
        ids(&item.evidence_ids)?;
        inventory_evidence(document, window, item)?;
    }
    Ok(())
}

/// Every reference must be relevant, and their timestamp union must cover the
/// entire item. Overlapping/adjacent cues may jointly support a multi-cue fact;
/// neither an enclosing bounding box nor any single overlap proves coverage.
fn inventory_evidence(
    document: &RecapDocument,
    window: &SourceWindow,
    item: &InformationItem,
) -> Result<()> {
    check(!item.evidence_ids.is_empty(), "inventory evidence missing")?;
    let mut ranges = Vec::with_capacity(item.evidence_ids.len());
    for id in &item.evidence_ids {
        check(
            window.evidence_ids.contains(id),
            "inventory evidence outside window",
        )?;
        let evidence = document
            .evidence
            .iter()
            .find(|evidence| evidence.id == *id)
            .ok_or(CompositionError::Invalid("invented inventory evidence"))?;
        check(
            super::overlaps(evidence.range, item.range),
            "unrelated inventory evidence",
        )?;
        ranges.push(evidence.range);
    }
    ranges.sort_unstable_by_key(|range| (range.start_ms, range.end_ms));
    let mut covered_until = item.range.start_ms;
    for range in ranges {
        check(
            range.start_ms <= covered_until,
            "inventory evidence coverage gap",
        )?;
        covered_until = covered_until.max(range.end_ms);
    }
    check(
        covered_until >= item.range.end_ms,
        "inventory evidence incomplete coverage",
    )
}

pub(super) fn validate_outline(outline: &Outline, items: &[InformationItem]) -> Result<()> {
    check(
        !outline.title.trim().is_empty() && !outline.chapters.is_empty(),
        "empty outline",
    )?;
    let chapter_ids: BTreeSet<_> = ids(&outline
        .chapters
        .iter()
        .map(|chapter| chapter.id.clone())
        .collect::<Vec<_>>())?
    .into_iter()
    .map(str::to_owned)
    .collect();
    let inventory: BTreeSet<_> = items.iter().map(|item| item.id.as_str()).collect();
    exact_ids(
        &outline
            .dispositions
            .iter()
            .map(|d| d.item_id.clone())
            .collect::<Vec<_>>(),
        inventory.iter().copied(),
    )?;
    let decisions: BTreeMap<_, _> = outline
        .dispositions
        .iter()
        .map(|d| (d.item_id.as_str(), d))
        .collect();
    for decision in &outline.dispositions {
        check(
            !decision.reason.trim().is_empty(),
            "unjustified item disposition",
        )?;
        ids(&decision.chapter_ids)?;
        ids(&decision.duplicate_of)?;
        match decision.disposition {
            DispositionKind::Retained => {
                check(
                    !decision.chapter_ids.is_empty()
                        && decision.duplicate_of.is_empty()
                        && decision
                            .chapter_ids
                            .iter()
                            .all(|id| chapter_ids.contains(id)),
                    "invalid retained disposition",
                )?;
            }
            DispositionKind::Duplicate => {
                check(
                    decision.chapter_ids.is_empty()
                        && !decision.duplicate_of.is_empty()
                        && decision.duplicate_of.iter().all(|target| {
                            target != &decision.item_id
                                && decisions.get(target.as_str()).is_some_and(|other| {
                                    other.disposition == DispositionKind::Retained
                                })
                        }),
                    "invalid duplicate disposition",
                )?;
            }
            DispositionKind::NonSubstantive => {
                check(
                    decision.chapter_ids.is_empty() && decision.duplicate_of.is_empty(),
                    "invalid exclusion",
                )?;
            }
            DispositionKind::Unsupported => {
                return Err(CompositionError::Invalid(
                    "unsupported substantive source item",
                ));
            }
        }
    }
    for chapter in &outline.chapters {
        check(
            !chapter.title.trim().is_empty() && !chapter.item_ids.is_empty(),
            "empty planned chapter",
        )?;
        exact_ids(
            &chapter.item_ids,
            outline
                .dispositions
                .iter()
                .filter(|d| {
                    d.disposition == DispositionKind::Retained
                        && d.chapter_ids.contains(&chapter.id)
                })
                .map(|d| d.item_id.as_str()),
        )?;
    }
    Ok(())
}

pub(super) fn validate_choices(
    document: &RecapDocument,
    choices: &[VisualChoice],
    allowed: Option<&[String]>,
) -> Result<()> {
    ids(&choices
        .iter()
        .map(|choice| choice.asset_id.clone())
        .collect::<Vec<_>>())?;
    for choice in choices {
        check(
            !choice.reason.trim().is_empty()
                && document
                    .assets
                    .iter()
                    .any(|asset| asset.id == choice.asset_id)
                && allowed.is_none_or(|ids| ids.contains(&choice.asset_id)),
            "invalid visual choice",
        )?;
    }
    Ok(())
}

/// Host-selected chapter scope: assigned claims plus illustration evidence for
/// frames actually supplied in the same source windows. Other text evidence is
/// context only, not permission to add cross-subject claims.
pub(super) struct ChapterSources {
    pub evidence_ids: Vec<String>,
    pub asset_ids: Vec<String>,
}

impl ChapterSources {
    #[must_use]
    pub fn derive(
        document: &RecapDocument,
        items: &[InformationItem],
        windows: &[&SourceWindow],
        frames: &[ImageAsset],
    ) -> Self {
        let asset_ids: Vec<_> = frames.iter().map(|frame| frame.asset_id.clone()).collect();
        let assigned: BTreeSet<_> = items
            .iter()
            .flat_map(|item| item.evidence_ids.iter().map(String::as_str))
            .collect();
        let evidence_ids = document
            .evidence
            .iter()
            .filter(|evidence| {
                assigned.contains(evidence.id.as_str())
                    || (windows
                        .iter()
                        .any(|window| window.evidence_ids.contains(&evidence.id))
                        && document.assets.iter().any(|asset| {
                            asset_ids.contains(&asset.id) && contains(evidence.range, asset.range)
                        }))
            })
            .map(|evidence| evidence.id.clone())
            .collect();
        Self {
            evidence_ids,
            asset_ids,
        }
    }

    pub fn validate(
        &self,
        document: &RecapDocument,
        plan: &ChapterPlan,
        chapter: &ChapterComposition,
        items: &[InformationItem],
    ) -> Result<()> {
        validate_repaired_chapter(document, plan, chapter, items, &self.evidence_ids)?;
        validate_choices(document, &chapter.image_choices, Some(&self.asset_ids))
    }
}

pub(super) fn validate_repaired_chapter(
    document: &RecapDocument,
    plan: &ChapterPlan,
    chapter: &ChapterComposition,
    items: &[InformationItem],
    additional_evidence: &[String],
) -> Result<()> {
    check(
        chapter.section.id == plan.id && !chapter.section.blocks.is_empty(),
        "chapter identity mismatch",
    )?;
    exact_ids(
        &chapter.covered_item_ids,
        plan.item_ids.iter().map(String::as_str),
    )?;
    validate_choices(document, &chapter.image_choices, None)?;
    let mut selected = BTreeSet::new();
    let mut text = false;
    for block in &chapter.section.blocks {
        check(!block.evidence_ids.is_empty(), "ungrounded chapter block")?;
        for evidence_id in &block.evidence_ids {
            check(
                items
                    .iter()
                    .any(|item| item.evidence_ids.contains(evidence_id))
                    || additional_evidence.contains(evidence_id),
                "chapter uses unassigned evidence",
            )?;
        }
        let assets: Vec<&str> = match &block.content {
            BlockContent::Heading { .. }
            | BlockContent::List { .. }
            | BlockContent::Diagram { .. } => {
                text = true;
                vec![]
            }
            BlockContent::Text { text: content } => {
                text |= !content.trim().is_empty();
                vec![]
            }
            BlockContent::Image { asset_id, .. } | BlockContent::Clip { asset_id, .. } => {
                vec![asset_id]
            }
            BlockContent::Comparison { items } => {
                items.iter().map(|item| item.asset_id.as_str()).collect()
            }
        };
        for asset_id in assets {
            let asset = document
                .assets
                .iter()
                .find(|asset| asset.id == asset_id)
                .ok_or(CompositionError::Invalid("invented chapter asset"))?;
            check(
                block.evidence_ids.iter().any(|id| {
                    document
                        .evidence
                        .iter()
                        .any(|evidence| evidence.id == *id && contains(evidence.range, asset.range))
                }),
                "visual outside evidence",
            )?;
            selected.insert(asset_id);
        }
    }
    check(text, "chapter has no readable text")?;
    check(
        selected
            == chapter
                .image_choices
                .iter()
                .map(|choice| choice.asset_id.as_str())
                .collect(),
        "unexplained visual selection",
    )?;
    // Existing document validation is authoritative for block shape, all IDs,
    // media types and source registries, even for an injected typed executor.
    let mut candidate = document.clone();
    candidate.sections = vec![chapter.section.clone()];
    candidate.validate()?;
    Ok(())
}

/// Count-only classification: never retains or renders rejected identity strings.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct AuditIdentityCounts {
    pub missing: usize,
    pub unknown: usize,
    pub duplicate: usize,
    pub malformed: usize,
}

impl AuditIdentityCounts {
    pub fn inspect(actual: &[String], expected: &BTreeSet<&str>) -> Self {
        let mut seen = BTreeSet::new();
        let mut counts = Self::default();
        for value in actual {
            counts.malformed += usize::from(id(value).is_err());
            if !seen.insert(value.as_str()) {
                counts.duplicate += 1;
            }
        }
        counts.missing = expected.difference(&seen).count();
        counts.unknown = seen.difference(expected).count();
        counts
    }

    fn validate(&self) -> Result<()> {
        check(self.malformed == 0, "audit checked identities malformed")?;
        check(self.duplicate == 0, "audit checked identities duplicate")?;
        check(self.unknown == 0, "audit checked identities unknown")?;
        check(self.missing == 0, "audit checked identities missing")
    }
}

/// Findings with no inventory identity still need a validated chapter target.
pub(super) fn validate_audit(
    audit: &crate::analysis_contracts::SourceAudit,
    items: &[InformationItem],
    chapters: &[ChapterComposition],
) -> Result<()> {
    use crate::analysis_contracts::CoverageIssueKind;
    let item_ids: BTreeSet<_> = items.iter().map(|item| item.id.as_str()).collect();
    AuditIdentityCounts::inspect(&audit.checked_item_ids, &item_ids).validate()?;
    ids(&audit
        .issues
        .iter()
        .map(|issue| issue.id.clone())
        .collect::<Vec<_>>())?;
    let chapter_ids: BTreeSet<_> = chapters
        .iter()
        .map(|chapter| chapter.section.id.as_str())
        .collect();
    for issue in &audit.issues {
        ids(&issue.item_ids)?;
        ids(&issue.chapter_ids)?;
        check(
            !issue.description.trim().is_empty()
                && (!issue.item_ids.is_empty()
                    || !issue.chapter_ids.is_empty()
                    || issue.kind == CoverageIssueKind::UnsupportedClaim)
                && issue
                    .item_ids
                    .iter()
                    .all(|id| item_ids.contains(id.as_str()))
                && issue
                    .chapter_ids
                    .iter()
                    .all(|id| chapter_ids.contains(id.as_str())),
            "invalid source audit finding",
        )?;
    }
    check(
        audit.limitations.iter().all(|text| !text.trim().is_empty()),
        "empty audit limitation",
    )?;
    Ok(())
}

#[cfg(test)]
mod audit_identity_tests {
    use super::*;

    #[test]
    fn checked_identity_failures_are_counted_without_exposing_payloads() {
        let expected = ["required"].into();
        for (actual, counts, category) in [
            (
                vec![],
                AuditIdentityCounts {
                    missing: 1,
                    ..AuditIdentityCounts::default()
                },
                "audit checked identities missing",
            ),
            (
                vec!["required".into(), "private-ledger-id".into()],
                AuditIdentityCounts {
                    unknown: 1,
                    ..AuditIdentityCounts::default()
                },
                "audit checked identities unknown",
            ),
            (
                vec!["required".into(), "required".into()],
                AuditIdentityCounts {
                    duplicate: 1,
                    ..AuditIdentityCounts::default()
                },
                "audit checked identities duplicate",
            ),
        ] {
            let result = AuditIdentityCounts::inspect(&actual, &expected);
            assert_eq!(result, counts);
            assert!(
                matches!(result.validate(), Err(CompositionError::Invalid(message)) if message == category)
            );
        }
        assert!(
            AuditIdentityCounts::inspect(&[], &BTreeSet::new())
                .validate()
                .is_ok()
        );
        let counts = AuditIdentityCounts::inspect(&["private-ledger-id".into()], &expected);
        assert_eq!(counts.missing, 1);
        assert_eq!(counts.unknown, 1);
        assert!(!format!("{counts:?}").contains("private-ledger-id"));
    }
}
