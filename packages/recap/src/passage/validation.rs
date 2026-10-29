//! Referential and transactional checks, not semantic correctness assertions.
use std::collections::BTreeSet;

use crate::{
    contracts::{BlockContent, RecapDocument, Section},
    passage_contracts::{
        Passage, PassageFinding, PassagePatch, PassagePatchKind, PassageReview, PassageTarget,
        PassageTargetKind, SourceUnit,
    },
};

use super::{CompositionError, Result, check};

fn target(passage: &Passage, target: &PassageTarget) -> Result<()> {
    check(
        match target.kind {
            PassageTargetKind::Start => target.block_id.is_empty(),
            PassageTargetKind::Block | PassageTargetKind::AfterBlock => passage
                .blocks
                .iter()
                .any(|block| block.id == target.block_id),
        },
        "unknown passage block target",
    )
}

pub(super) fn passage(document: &RecapDocument, unit: &SourceUnit, value: &Passage) -> Result<()> {
    check(
        value.unit_id == unit.id && value.revision > 0 && !value.blocks.is_empty(),
        "invalid passage identity or empty content",
    )?;
    for block in &value.blocks {
        let mut evidence_ids = BTreeSet::new();
        check(
            !block.evidence_ids.is_empty(),
            "passage block lacks source evidence",
        )?;
        for id in &block.evidence_ids {
            check(
                evidence_ids.insert(id)
                    && unit
                        .primary
                        .evidence
                        .iter()
                        .any(|evidence| evidence.id == *id),
                "foreign or duplicate passage block evidence",
            )?;
        }
        let media = |id: &str| unit.primary.assets.iter().any(|asset| asset.id == id);
        check(
            match &block.content {
                BlockContent::Text { .. } => true,
                BlockContent::Image { asset_id, .. } | BlockContent::Clip { asset_id, .. } => {
                    media(asset_id)
                }
                BlockContent::Comparison { items } => {
                    items.iter().all(|item| media(&item.asset_id))
                }
            },
            "foreign passage media",
        )?;
    }
    let mut candidate = document.clone();
    candidate.sections = vec![Section {
        id: value.id.clone(),
        title: value.title.clone(),
        blocks: value.blocks.clone(),
    }];
    candidate.validate()?;
    Ok(())
}

pub(super) fn review(unit: &SourceUnit, value: &Passage, review: &PassageReview) -> Result<()> {
    check(
        review
            .limitations
            .iter()
            .all(|text| !text.trim().is_empty()),
        "empty passage limitation",
    )?;
    for finding in &review.findings {
        target(value, &finding.target)?;
        check(
            !finding.discrepancy.trim().is_empty() && !finding.correction.trim().is_empty(),
            "empty passage discrepancy",
        )?;
        check(
            finding.range.start_ms < finding.range.end_ms
                && unit.primary.range.start_ms <= finding.range.start_ms
                && finding.range.end_ms <= unit.primary.range.end_ms,
            "passage finding outside primary source",
        )?;
        check(
            !finding.evidence_ids.is_empty(),
            "passage finding lacks evidence",
        )?;
        let mut seen = BTreeSet::new();
        let mut ranges = Vec::new();
        for id in &finding.evidence_ids {
            check(seen.insert(id), "duplicate passage finding evidence")?;
            let evidence = unit
                .primary
                .evidence
                .iter()
                .find(|evidence| evidence.id == *id)
                .ok_or(CompositionError::Invalid(
                    "foreign passage finding evidence",
                ))?;
            check(
                evidence.range.start_ms < finding.range.end_ms
                    && finding.range.start_ms < evidence.range.end_ms,
                "unrelated passage finding evidence",
            )?;
            ranges.push(evidence.range);
        }
        ranges.sort_by_key(|range| (range.start_ms, range.end_ms));
        let mut covered = finding.range.start_ms;
        for range in ranges {
            check(
                range.start_ms <= covered,
                "gap in passage finding provenance",
            )?;
            covered = covered.max(range.end_ms);
        }
        check(
            covered >= finding.range.end_ms,
            "incomplete passage finding provenance",
        )?;
    }
    Ok(())
}

/// Produce a fully checked successor without modifying the supplied base.
pub(super) fn patch(
    document: &RecapDocument,
    unit: &SourceUnit,
    base: &Passage,
    findings: &[PassageFinding],
    patch: &PassagePatch,
) -> Result<Passage> {
    check(
        patch.base_revision == base.revision,
        "stale passage patch revision",
    )?;
    check(!patch.operations.is_empty(), "empty passage patch")?;
    let revision = base
        .revision
        .checked_add(1)
        .ok_or(CompositionError::Invalid("passage revision exhausted"))?;
    let mut touched = BTreeSet::new();
    for operation in &patch.operations {
        target(base, &operation.target)?;
        check(
            findings
                .iter()
                .any(|finding| finding.target == operation.target),
            "patch target unrelated to review findings",
        )?;
        // Reject a replacement/deletion and insertion anchored on the same block,
        // as well as duplicate operations. No edit-order-dependent interpretation.
        check(
            touched.insert(operation.target.block_id.as_str()),
            "conflicting passage patch targets",
        )?;
        let valid = match operation.kind {
            PassagePatchKind::Replace => {
                operation.target.kind == PassageTargetKind::Block && !operation.blocks.is_empty()
            }
            PassagePatchKind::Delete => {
                operation.target.kind == PassageTargetKind::Block && operation.blocks.is_empty()
            }
            PassagePatchKind::Insert => {
                operation.target.kind != PassageTargetKind::Block && !operation.blocks.is_empty()
            }
        };
        check(valid, "invalid passage patch operation target or content")?;
    }
    let mut candidate = base.clone();
    candidate.revision = revision;
    candidate.blocks.clear();
    let fresh = |operation_index: usize, replacement: bool| {
        patch.operations[operation_index]
            .blocks
            .iter()
            .cloned()
            .enumerate()
            .map(move |(index, mut block)| {
                block.id = if replacement && index == 0 {
                    patch.operations[operation_index].target.block_id.clone()
                } else {
                    format!("{}-r{revision}-o{operation_index}-b{index}", base.id)
                };
                block
            })
    };
    for (index, operation) in patch.operations.iter().enumerate() {
        if operation.target.kind == PassageTargetKind::Start {
            candidate.blocks.extend(fresh(index, false));
        }
    }
    for block in &base.blocks {
        let replacement = patch.operations.iter().enumerate().find(|(_, operation)| {
            operation.target.kind == PassageTargetKind::Block
                && operation.target.block_id == block.id
        });
        if let Some((index, operation)) = replacement {
            if operation.kind == PassagePatchKind::Replace {
                candidate.blocks.extend(fresh(index, true));
            }
        } else {
            candidate.blocks.push(block.clone());
        }
        for (index, operation) in patch.operations.iter().enumerate() {
            if operation.target.kind == PassageTargetKind::AfterBlock
                && operation.target.block_id == block.id
            {
                candidate.blocks.extend(fresh(index, false));
            }
        }
    }
    passage(document, unit, &candidate)?;
    check(
        content_key(base)? != content_key(&candidate)?,
        "unchanged passage patch",
    )?;
    Ok(candidate)
}

/// Ignore revision and regenerated identities when detecting repeated prose states.
pub(super) fn content_key(value: &Passage) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&(
        value.title.as_str(),
        value
            .blocks
            .iter()
            .map(|block| (&block.evidence_ids, &block.content))
            .collect::<Vec<_>>(),
    ))?)
}
