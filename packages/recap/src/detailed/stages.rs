//! Bounded orchestration of outline, audit and targeted replacement stages.
use std::collections::{BTreeMap, BTreeSet};

use serde_json::json;

use super::{
    CompositionError, Operation, Result, frames_for, sources_for,
    validation::{self, check, exact_ids, validate_outline},
};
use crate::{
    analysis_contracts::{
        AnalysisResponse, AnalysisStage, ChapterComposition, CoverageIssue, InformationItem,
        Outline, SourceWindow,
    },
    contracts::{Block, BlockContent, GenerationIntent, RecapDocument},
    model::ImageAsset,
};

/// Leaves have complete dispositions. The root preserves their ordered union:
/// merging is optional and never a reason to omit a leaf or introduce a chapter cap.
pub(super) async fn outline_inventory(
    op: &mut Operation<'_>,
    inventory: &[InformationItem],
    intent: &GenerationIntent,
    previous: Option<&RecapDocument>,
) -> Result<Outline> {
    let previous_sections: Vec<_> = previous
        .into_iter()
        .flat_map(|prior| prior.sections.iter())
        .map(|section| json!({"id":section.id,"title":section.title}))
        .collect();
    let envelope = json!({"intent":intent,"previous_sections":previous_sections});
    let allowance = op.policy.allowances.max_input_bytes / 2;
    let overhead = serde_json::to_vec(&envelope)?.len();
    let mut groups = Vec::new();
    let mut group = Vec::new();
    let mut size = overhead;
    for item in inventory {
        op.cancelled()?;
        let item_size = serde_json::to_vec(item)?.len().saturating_add(1);
        if item_size.saturating_add(overhead) > allowance {
            return Err(CompositionError::Budget);
        }
        if size.saturating_add(item_size) > allowance && !group.is_empty() {
            groups.push(std::mem::take(&mut group));
            size = overhead;
        }
        group.push(item.clone());
        size = size.saturating_add(item_size);
    }
    if !group.is_empty() {
        groups.push(group);
    }
    let hierarchical = groups.len() > 1;
    let mut leaves = Vec::new();
    for (index, items) in groups.iter().enumerate() {
        let response = op.call(AnalysisStage::Outline,
            json!({"items":items,"intent":intent,"previous_sections":previous_sections,
                "outline_level":"source_partition","partition":index}), vec![],
            "Outline this full inventory partition. Account for EVERY item exactly once. \
             Retain all substantive information. Only genuine duplicates or non-substantive filler \
             may be excluded, with source-grounded reasons. Every retained item's chapter assignments \
             must match chapter item_ids. Unsupported substantive information is not success. \
             Retain relevant previous section IDs and use no editorial chapter limit.").await?;
        let AnalysisResponse::Outline(mut outline) = response else {
            unreachable!()
        };
        validate_outline(&outline, items)?;
        // Host namespaces new IDs, never previous identities. This makes every leaf
        // independent without allowing fabricated cross-partition chapter references.
        if hierarchical {
            let old: BTreeMap<_, _> = outline
                .chapters
                .iter()
                .filter(|chapter| {
                    !previous_sections
                        .iter()
                        .any(|section| section["id"] == chapter.id)
                })
                .map(|chapter| (chapter.id.clone(), format!("part-{index}-{}", chapter.id)))
                .collect();
            for chapter in &mut outline.chapters {
                if let Some(new) = old.get(&chapter.id) {
                    chapter.id.clone_from(new);
                }
            }
            for decision in &mut outline.dispositions {
                for id in &mut decision.chapter_ids {
                    if let Some(new) = old.get(id) {
                        id.clone_from(new);
                    }
                }
            }
        }
        leaves.push(outline);
    }
    // A bounded second level can reconcile chronology, duplicate boundaries and
    // grouping from complete leaf plans, without needing the entire raw inventory.
    // When even these plans exceed a request the host root retains them verbatim.
    let merge_input = json!({"outline_level":"hierarchical_root","children":leaves,
        "intent":intent,"previous_sections":previous_sections});
    if hierarchical && serde_json::to_vec(&merge_input)?.len() < allowance {
        let response = op.call(AnalysisStage::Outline, merge_input, vec![],
            "Reconcile the complete child outlines into a source-order root outline. Preserve every \
             item disposition and substantive detail. You may merge related chapters, but may not \
             omit items, invent IDs, or impose a chapter cap. Preserve previous section IDs.").await?;
        let AnalysisResponse::Outline(outline) = response else {
            unreachable!()
        };
        validate_outline(&outline, inventory)?;
        return Ok(outline);
    }
    let title = leaves
        .first()
        .ok_or(CompositionError::Invalid("no outline leaves"))?
        .title
        .clone();
    let mut root = Outline {
        title,
        chapters: vec![],
        dispositions: vec![],
    };
    for leaf in leaves {
        root.chapters.extend(leaf.chapters);
        root.dispositions.extend(leaf.dispositions);
    }
    validate_outline(&root, inventory)?;
    Ok(root)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn audit(
    op: &mut Operation<'_>,
    document: &RecapDocument,
    images: &[ImageAsset],
    windows: &[&SourceWindow],
    items: &[InformationItem],
    outline: &Outline,
    chapters: &[ChapterComposition],
    purpose: &str,
    source_items: &[InformationItem],
) -> Result<(Vec<CoverageIssue>, Vec<String>)> {
    let ledger_items: Vec<_> = source_items
        .iter()
        .filter(|item| windows.iter().any(|window| window.id == item.window_id))
        .cloned()
        .collect();
    let ledger = audit_ledger(&ledger_items, outline);
    audit_scoped(
        op, document, images, windows, items, outline, chapters, purpose, &ledger,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn audit_scoped(
    op: &mut Operation<'_>,
    document: &RecapDocument,
    images: &[ImageAsset],
    windows: &[&SourceWindow],
    items: &[InformationItem],
    outline: &Outline,
    chapters: &[ChapterComposition],
    purpose: &str,
    ledger: &serde_json::Value,
) -> Result<(Vec<CoverageIssue>, Vec<String>)> {
    let item_ids: BTreeSet<_> = items.iter().map(|item| item.id.as_str()).collect();
    let dispositions: Vec<_> = outline
        .dispositions
        .iter()
        .filter(|d| item_ids.contains(d.item_id.as_str()))
        .collect();
    let frames = if op.policy.inspect_visuals {
        frames_for(images, windows)
    } else {
        vec![]
    };
    let input = json!({"purpose":purpose,"items":items,"dispositions":dispositions,"chapters":chapters,
        "source_coverage_ledger":ledger,
        "untrusted_sources":sources_for(document, windows)});
    // A source window can feed many chapters. Partition that fan-out instead of
    // making all of its chapters an accidental indivisible context requirement.
    // Chapter and adjacency consistency checks, in contrast, must remain intact.
    if chapters.len() > 1
        && purpose != "independent_final_consistency"
        && serde_json::to_vec(&input)?
            .len()
            .saturating_add(super::POLICY.len())
            .saturating_add(2000)
            > op.policy.allowances.max_input_bytes
    {
        let middle = chapters.len() / 2;
        let mut all_issues = Vec::new();
        let mut all_limits = Vec::new();
        for (part, selected) in [&chapters[..middle], &chapters[middle..]]
            .into_iter()
            .enumerate()
        {
            let selected_items = batch_items(items, outline, chapters, selected, part == 0);
            let (mut issues, limits) = Box::pin(audit_scoped(
                op,
                document,
                images,
                windows,
                &selected_items,
                outline,
                selected,
                purpose,
                ledger,
            ))
            .await?;
            for issue in &mut issues {
                issue.id = format!("batch-{part}-{}", issue.id);
            }
            all_issues.extend(issues);
            all_limits.extend(limits);
        }
        return Ok((all_issues, all_limits));
    }
    let response = op.call(AnalysisStage::SourceAudit,
        input, frames,
        "Independently compare actual source against actual chapter blocks. Check EVERY supplied \
         inventory item in THIS BATCH's items, including duplicates and exclusions. The complete \
         source_coverage_ledger records source-wide inventory text, dispositions and chapter owners, \
         NOT proof of rendered coverage. Items owned by chapters absent from this batch are checked \
         in those chapters' batches: do NOT report them missing here. Verify batch items against \
         actual blocks, never ledger claims. Independently compare ALL source details against the \
         complete ledger: genuinely substantive details absent from that inventory are still \
         extraction omissions and must be reported with existing batch chapter targets. \
         Find unsupported additions, contradictions, lost specifics and inadequate \
         visuals. A substantive source point missing from the \
         inventory is still an audit failure. If it has no inventory identity, use empty item_ids \
         and name existing chapter_ids that should explain it, with the exact source-grounded \
         correction in description. Never invent an item ID. Non-substantive/duplicate declarations are not evidence. Check excluded substantive \
         claims rather than accepting the outline's rationale. Return all checked IDs, grounded \
         actionable issues and honest limitations. For final consistency also compare all supplied \
         chapters to one another. Do not rewrite or summarize anything.").await?;
    let AnalysisResponse::SourceAudit(audit) = response else {
        unreachable!()
    };
    validation::validate_audit(&audit, items, chapters)?;
    Ok((audit.issues, audit.limitations))
}

fn batch_items(
    items: &[InformationItem],
    outline: &Outline,
    chapters: &[ChapterComposition],
    selected: &[ChapterComposition],
    first: bool,
) -> Vec<InformationItem> {
    items
        .iter()
        .filter(|item| {
            let owners = audit_owners(&item.id, outline);
            owners
                .iter()
                .any(|owner| selected.iter().any(|chapter| chapter.section.id == *owner))
                || (first
                    && !owners
                        .iter()
                        .any(|owner| chapters.iter().any(|chapter| chapter.section.id == *owner)))
        })
        .cloned()
        .collect()
}

fn audit_owners(item_id: &str, outline: &Outline) -> Vec<String> {
    let Some(decision) = outline
        .dispositions
        .iter()
        .find(|decision| decision.item_id == item_id)
    else {
        return vec![];
    };
    if decision.disposition == crate::analysis_contracts::DispositionKind::Duplicate {
        outline
            .dispositions
            .iter()
            .filter(|target| decision.duplicate_of.contains(&target.item_id))
            .flat_map(|target| target.chapter_ids.iter().cloned())
            .collect()
    } else {
        decision.chapter_ids.clone()
    }
}

fn audit_ledger(items: &[InformationItem], outline: &Outline) -> serde_json::Value {
    json!(
        items
            .iter()
            .map(|item| json!({"item":item,
        "disposition":outline.dispositions.iter().find(|decision| decision.item_id == item.id),
        "chapter_ids":audit_owners(&item.id, outline)}))
            .collect::<Vec<_>>()
    )
}

pub(super) fn nearest_chapter<'a>(
    chapters: &'a [ChapterComposition],
    inventory: &[InformationItem],
    window: &SourceWindow,
) -> Option<&'a ChapterComposition> {
    chapters.iter().min_by_key(|chapter| {
        inventory
            .iter()
            .filter(|item| chapter.covered_item_ids.contains(&item.id))
            .map(|item| item.range.start_ms.abs_diff(window.range.start_ms))
            .min()
            .unwrap_or(u64::MAX)
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn consistency(
    op: &mut Operation<'_>,
    document: &RecapDocument,
    images: &[ImageAsset],
    windows: &[SourceWindow],
    inventory: &[InformationItem],
    outline: &Outline,
    chapters: &[ChapterComposition],
    limitations: &mut Vec<String>,
) -> Result<()> {
    // Each source window is inspected independently, including excluded information
    // and windows with no retained inventory. No aggregate document request exists.
    for window in windows {
        let items: Vec<_> = inventory
            .iter()
            .filter(|item| item.window_id == window.id)
            .cloned()
            .collect();
        let mut selected: Vec<_> = chapters
            .iter()
            .filter(|chapter| {
                chapter
                    .covered_item_ids
                    .iter()
                    .any(|id| items.iter().any(|item| &item.id == id))
                    || chapter.section.blocks.iter().any(|block| {
                        block
                            .evidence_ids
                            .iter()
                            .any(|id| window.evidence_ids.contains(id))
                    })
            })
            .cloned()
            .collect();
        if selected.is_empty()
            && let Some(chapter) = nearest_chapter(chapters, inventory, window)
        {
            selected.push(chapter.clone());
        }
        let (issues, limits) = audit(
            op,
            document,
            images,
            &[window],
            &items,
            outline,
            &selected,
            "independent_final_consistency_source",
            inventory,
        )
        .await?;
        check(issues.is_empty(), "final source consistency audit failed")?;
        limitations.extend(limits);
    }
    // Inspect every complete chapter and its direct source, then each adjacency.
    // These are indivisible review units: if one cannot fit, fail rather than
    // truncate text, frames or source. Aggregate length has no single-context cap.
    for index in 0..chapters.len() {
        for width in 1..=2 {
            if index + width > chapters.len() {
                continue;
            }
            let selected = &chapters[index..index + width];
            let items: Vec<_> = inventory
                .iter()
                .filter(|item| {
                    selected
                        .iter()
                        .any(|chapter| chapter.covered_item_ids.contains(&item.id))
                })
                .cloned()
                .collect();
            let relevant: Vec<_> = windows
                .iter()
                .filter(|window| {
                    items.iter().any(|item| item.window_id == window.id)
                        || selected.iter().any(|chapter| {
                            chapter.section.blocks.iter().any(|block| {
                                block
                                    .evidence_ids
                                    .iter()
                                    .any(|id| window.evidence_ids.contains(id))
                            })
                        })
                })
                .collect();
            let (issues, limits) = audit(
                op,
                document,
                images,
                &relevant,
                &items,
                outline,
                selected,
                "independent_final_consistency",
                inventory,
            )
            .await?;
            check(issues.is_empty(), "final chapter consistency audit failed")?;
            limitations.extend(limits);
        }
    }
    Ok(())
}

/// Connected chapter ownership prevents later proposals overwriting an earlier
/// finding's correction in the same chapter.
fn repair_groups(
    outline: &Outline,
    issues: &[CoverageIssue],
) -> Result<Vec<(BTreeSet<String>, Vec<CoverageIssue>)>> {
    let mut groups: Vec<(BTreeSet<String>, Vec<CoverageIssue>)> = Vec::new();
    for issue in issues {
        let mut owners: BTreeSet<_> = outline
            .chapters
            .iter()
            .filter(|plan| {
                issue.chapter_ids.contains(&plan.id)
                    || plan.item_ids.iter().any(|id| issue.item_ids.contains(id))
            })
            .map(|plan| plan.id.clone())
            .collect();
        check(!owners.is_empty(), "audit finding has no repair target")?;
        let mut findings = vec![issue.clone()];
        let mut index = 0;
        while index < groups.len() {
            if owners.is_disjoint(&groups[index].0) {
                index += 1;
            } else {
                let (other_owners, other_findings) = groups.remove(index);
                owners.extend(other_owners);
                findings.extend(other_findings);
                index = 0;
            }
        }
        groups.push((owners, findings));
    }
    Ok(groups)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn repair(
    op: &mut Operation<'_>,
    document: &RecapDocument,
    images: &[ImageAsset],
    windows: &[SourceWindow],
    inventory: &[InformationItem],
    outline: &Outline,
    chapters: &mut [ChapterComposition],
    issues: &[CoverageIssue],
) -> Result<()> {
    // Findings sharing a chapter are one connected repair unit. A replacement
    // sees every accumulated finding for that unit and the latest complete text.
    for (target_ids, findings) in repair_groups(outline, issues)? {
        let targets: Vec<_> = outline
            .chapters
            .iter()
            .filter(|plan| target_ids.contains(&plan.id))
            .collect();
        check(!targets.is_empty(), "audit finding has no repair target")?;
        let items: Vec<_> = inventory
            .iter()
            .filter(|item| targets.iter().any(|plan| plan.item_ids.contains(&item.id)))
            .cloned()
            .collect();
        let relevant: Vec<_> = windows
            .iter()
            .filter(|window| {
                items.iter().any(|item| item.window_id == window.id)
                    || findings
                        .iter()
                        .any(|issue| issue.id.starts_with(&format!("{}-", window.id)))
            })
            .collect();
        let current: Vec<_> = chapters
            .iter()
            .filter(|chapter| targets.iter().any(|plan| plan.id == chapter.section.id))
            .cloned()
            .collect();
        let frames = frames_for(images, &relevant);
        let allowed_assets: Vec<_> = frames.iter().map(|frame| frame.asset_id.clone()).collect();
        let allowed_evidence: Vec<_> = relevant
            .iter()
            .flat_map(|window| window.evidence_ids.iter().cloned())
            .chain(current.iter().flat_map(|chapter| {
                chapter
                    .section
                    .blocks
                    .iter()
                    .flat_map(|block| block.evidence_ids.iter().cloned())
            }))
            .collect();
        let response = op
            .call(
                AnalysisStage::SourceRepair,
                json!({"issues":findings,"chapter_plans":targets,"items":items,"chapters":current,
                "allowed_evidence_ids":allowed_evidence,"allowed_asset_ids":allowed_assets,
                "untrusted_sources":sources_for(document, &relevant)}),
                frames,
                "Repair only these explicit findings by replacing the indicated complete chapters. \
             Preserve section IDs, every assigned item, evidence and unrelated content. The issues \
             include earlier successful corrections: satisfy ALL of them simultaneously and preserve \
             their corrected text in the latest supplied chapters. Never rebuild from inventory alone. No final \
             summary. Use only allowed_evidence_ids and allowed_asset_ids. Every selected asset \
             needs supplied bytes and an image_choices explanation. Do not claim a finding \
             resolved without corrected actual block text.",
            )
            .await?;
        let AnalysisResponse::SourceRepair(repair) = response else {
            unreachable!()
        };
        for chapter in &repair.chapters {
            validation::validate_choices(document, &chapter.image_choices, Some(&allowed_assets))?;
        }
        apply_repair(
            document,
            &targets,
            &items,
            chapters,
            &findings,
            repair,
            &allowed_evidence,
        )?;
    }
    Ok(())
}

pub(super) fn append_limitations(
    document: &mut RecapDocument,
    limitations: Vec<String>,
) -> Result<()> {
    if limitations.is_empty() {
        return Ok(());
    }
    let unique: BTreeSet<_> = limitations.into_iter().collect();
    let text = format!(
        "Coverage limitations (not a claim of complete source coverage):\n{}",
        unique.into_iter().collect::<Vec<_>>().join("\n")
    );
    let existing: BTreeSet<_> = document
        .sections
        .iter()
        .flat_map(|section| section.blocks.iter().map(|block| block.id.as_str()))
        .collect();
    let mut index = 0usize;
    let block_id = loop {
        let candidate = format!("detailed-coverage-limitations-{index}");
        if !existing.contains(candidate.as_str()) {
            break candidate;
        }
        index = index.checked_add(1).ok_or(CompositionError::Budget)?;
    };
    let evidence_ids = document
        .evidence
        .iter()
        .map(|item| item.id.clone())
        .collect();
    let section = document
        .sections
        .last_mut()
        .ok_or(CompositionError::Invalid(
            "no chapters for coverage limitations",
        ))?;
    section.blocks.push(Block {
        id: block_id,
        evidence_ids,
        content: BlockContent::Text { text },
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn apply_repair(
    document: &RecapDocument,
    targets: &[&crate::analysis_contracts::ChapterPlan],
    items: &[InformationItem],
    chapters: &mut [ChapterComposition],
    findings: &[CoverageIssue],
    repair: crate::analysis_contracts::SourceRepair,
    additional: &[String],
) -> Result<()> {
    exact_ids(
        &repair.addressed_issue_ids,
        findings.iter().map(|issue| issue.id.as_str()),
    )?;
    check(
        repair.unresolved_issue_ids.is_empty() && !repair.chapters.is_empty(),
        "unresolved targeted repair",
    )?;
    exact_ids(
        &repair
            .chapters
            .iter()
            .map(|chapter| chapter.section.id.clone())
            .collect::<Vec<_>>(),
        targets.iter().map(|plan| plan.id.as_str()),
    )?;
    for replacement in repair.chapters {
        let plan = targets
            .iter()
            .find(|plan| plan.id == replacement.section.id)
            .ok_or(CompositionError::Invalid(
                "repair changed unrelated chapter",
            ))?;
        let assigned: Vec<_> = items
            .iter()
            .filter(|item| plan.item_ids.contains(&item.id))
            .cloned()
            .collect();
        validation::validate_repaired_chapter(document, plan, &replacement, &assigned, additional)?;
        let target = chapters
            .iter_mut()
            .find(|chapter| chapter.section.id == plan.id)
            .ok_or(CompositionError::Invalid("repair invented chapter"))?;
        *target = replacement;
    }
    Ok(())
}
