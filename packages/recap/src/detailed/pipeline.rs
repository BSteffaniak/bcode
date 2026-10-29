//! Focused pipeline stages; all inputs and prior revisions remain immutable.
use super::stages::{append_limitations, audit, repair};
use super::validation::{check, exact_ids, validate_inventory};
use super::{
    AnalysisResponse, AnalysisStage, Cancellation, CompositionError, DetailedPolicy,
    GenerationIntent, ImageAsset, InformationItem, Operation, RecapDocument, Result, SourceWindow,
    check_cancelled, frames_for, namespace_items, relevant_windows, sources_for, stages,
    validation, window_source,
};
use crate::analysis_contracts::{ChapterComposition, Outline};
use serde_json::json;

pub(super) fn admit(
    grounded: &RecapDocument,
    images: &[ImageAsset],
    previous: Option<&RecapDocument>,
    policy: &DetailedPolicy,
    cancel: &Cancellation,
) -> Result<()> {
    check_cancelled(cancel)?;
    grounded.validate()?;
    policy.validate()?;
    // Operation admission is finite as well as each request. These totals are
    // derived from the caller's explicit request/window allowances, not editorial
    // limits, and are checked before constructing model-visible source windows.
    let total_input = policy
        .allowances
        .max_input_bytes
        .checked_mul(policy.allowances.max_source_windows)
        .ok_or(CompositionError::Budget)?;
    let total_images = policy
        .allowances
        .max_image_bytes
        .checked_mul(policy.allowances.max_source_windows)
        .ok_or(CompositionError::Budget)?;
    if serde_json::to_vec(grounded)?.len() > total_input
        || previous.is_some_and(|prior| {
            serde_json::to_vec(prior).map_or(true, |bytes| bytes.len() > total_input)
        })
        || images
            .iter()
            .try_fold(0usize, |size, image| size.checked_add(image.bytes.len()))
            .is_none_or(|size| size > total_images)
    {
        return Err(CompositionError::Budget);
    }
    check(!grounded.evidence.is_empty(), "no source evidence")?;
    if let Some(prior) = previous {
        prior.validate()?;
        check(
            prior.id == grounded.id
                && prior.source == grounded.source
                && prior.transcript == grounded.transcript
                && prior.evidence == grounded.evidence
                && prior.assets == grounded.assets,
            "refinement changed grounding",
        )?;
    }
    validation::validate_images(grounded, images)?;
    Ok(())
}

pub(super) async fn inventory(
    op: &mut Operation<'_>,
    grounded: &RecapDocument,
    images: &[ImageAsset],
    windows: &[SourceWindow],
) -> Result<(Vec<InformationItem>, Vec<String>)> {
    let mut inventory = Vec::new();
    let mut limitations = Vec::new();
    for window in windows {
        let source = window_source(grounded, window);
        let response = op
            .call(
                AnalysisStage::ExtractionInventory,
                json!({"window": window, "untrusted_source": source}),
                vec![],
                "Inventory ALL substantive information in this source window; keep exact details. \
             Return this window_id. Empty inventory requires an honest limitation.",
            )
            .await?;
        let AnalysisResponse::ExtractionInventory(mut extracted) = response else {
            unreachable!()
        };
        check(
            extracted.window_id == window.id,
            "inventory window mismatch",
        )?;
        validate_inventory(grounded, window, &extracted.items, &extracted.limitations)?;
        namespace_items(&mut extracted.items, &format!("{}-text", window.id));
        inventory.extend(extracted.items);
        limitations.extend(extracted.limitations);
        let frames: Vec<_> = images
            .iter()
            .filter(|image| window.asset_ids.contains(&image.asset_id))
            .cloned()
            .collect();
        if op.policy.inspect_visuals && !frames.is_empty() {
            let response = op.call(AnalysisStage::VisualInspection,
                json!({"window":window, "untrusted_source":source}), frames.clone(),
                "Inspect every supplied frame, especially visual-only information, diagrams, code \
                 and labels absent from speech. Return all inspected asset IDs and grounded items.").await?;
            let AnalysisResponse::VisualInspection(mut visual) = response else {
                unreachable!()
            };
            check(visual.window_id == window.id, "visual window mismatch")?;
            exact_ids(
                &visual.inspected_asset_ids,
                frames.iter().map(|image| image.asset_id.as_str()),
            )?;
            validate_inventory(grounded, window, &visual.items, &visual.limitations)?;
            validation::validate_choices(
                grounded,
                &visual.visual_choices,
                Some(&window.asset_ids),
            )?;
            namespace_items(&mut visual.items, &format!("{}-visual", window.id));
            inventory.extend(visual.items);
            limitations.extend(visual.limitations);
        } else if !window.asset_ids.is_empty() {
            limitations.push(format!(
                "Visual source information in {} was not inspected.",
                window.id
            ));
        }
    }
    check(!inventory.is_empty(), "no substantive source inventory")?;
    exact_ids(
        &inventory
            .iter()
            .map(|item| item.id.clone())
            .collect::<Vec<_>>(),
        inventory.iter().map(|item| item.id.as_str()),
    )?;
    Ok((inventory, limitations))
}

/// Immutable context shared by chapter writing and direct-source audit.
pub(super) struct SourcePlan<'a> {
    pub grounded: &'a RecapDocument,
    pub images: &'a [ImageAsset],
    pub windows: &'a [SourceWindow],
    pub inventory: &'a [InformationItem],
    pub outline: &'a Outline,
}

pub(super) async fn chapters(
    op: &mut Operation<'_>,
    source: &SourcePlan<'_>,
    intent: &GenerationIntent,
    previous: Option<&RecapDocument>,
) -> Result<Vec<ChapterComposition>> {
    let SourcePlan {
        grounded,
        images,
        windows,
        inventory,
        outline,
    } = *source;
    let mut chapters = Vec::new();
    for plan in &outline.chapters {
        let items: Vec<_> = inventory
            .iter()
            .filter(|item| plan.item_ids.contains(&item.id))
            .cloned()
            .collect();
        let relevant = relevant_windows(windows, &items);
        let frames = frames_for(images, &relevant);
        let allowed = validation::ChapterSources::derive(grounded, &items, &relevant, &frames);
        let response = op.call(AnalysisStage::ChapterComposition,
            json!({"chapter":plan, "items":items, "intent":intent,
                "allowed_evidence_ids":allowed.evidence_ids, "allowed_asset_ids":allowed.asset_ids,
                "untrusted_sources":sources_for(grounded, &relevant),
                "untrusted_previous_section":previous.and_then(|prior| prior.sections.iter().find(|s| s.id == plan.id))}),
            frames, "Compose exactly this chapter with its assigned item IDs. Preserve all details \
                in existing Section blocks. Use a unique block identity prefixed by chapter ID. \
                Explain each chosen source image; include readable source-grounded text. \
                Every evidence reference MUST be in allowed_evidence_ids. Only allowed_asset_ids \
                have supplied frame bytes and may be selected. Assigned item evidence supports \
                the text; additional allowed evidence supports supplied illustrations, not unrelated \
                claims from source context. Explain every selected asset in image_choices.").await?;
        let AnalysisResponse::ChapterComposition(chapter) = response else {
            unreachable!()
        };
        allowed.validate(grounded, plan, &chapter, &items)?;
        chapters.push(chapter);
    }
    Ok(chapters)
}

pub(super) async fn coverage(
    op: &mut Operation<'_>,
    source: &SourcePlan<'_>,
    chapters: &mut [ChapterComposition],
    limitations: &mut Vec<String>,
) -> Result<()> {
    let SourcePlan {
        grounded,
        images,
        windows,
        inventory,
        outline,
    } = *source;
    // First compare each entire source window directly, including excluded items.
    // Repairs are only proposals: every changed chapter is validated and every source
    // window is checked again after a repair. No model declaration resolves a finding.
    let mut repair_history: Vec<crate::analysis_contracts::CoverageIssue> = Vec::new();
    for round in 0..=op.policy.allowances.max_repair_rounds {
        let mut issues = Vec::new();
        let mut audit_limits = Vec::new();
        for window in windows {
            let items: Vec<_> = inventory
                .iter()
                .filter(|item| item.window_id == window.id)
                .cloned()
                .collect();
            let mut relevant: Vec<_> = chapters
                .iter()
                .filter(|chapter| {
                    chapter.section.blocks.iter().any(|block| {
                        block
                            .evidence_ids
                            .iter()
                            .any(|id| window.evidence_ids.contains(id))
                    }) || chapter
                        .covered_item_ids
                        .iter()
                        .any(|id| items.iter().any(|item| &item.id == id))
                })
                .cloned()
                .collect();
            if relevant.is_empty() {
                // Even a window completely missed by extraction needs an existing
                // repair target. The nearest chapter is context, not invented coverage.
                if let Some(chapter) = stages::nearest_chapter(chapters, inventory, window) {
                    relevant.push(chapter.clone());
                }
            }
            let (mut found, limits) = audit(
                op,
                grounded,
                images,
                &[window],
                &items,
                outline,
                &relevant,
                "direct_source_coverage",
                inventory,
            )
            .await?;
            for issue in &mut found {
                issue.id = format!("{}-{}", window.id, issue.id);
            }
            issues.extend(found);
            audit_limits.extend(limits);
        }
        if issues.is_empty() {
            limitations.extend(audit_limits);
            break;
        }
        if round == op.policy.allowances.max_repair_rounds {
            return Err(CompositionError::Invalid("unresolved source coverage gaps"));
        }
        for issue in issues {
            if let Some(prior) = repair_history.iter_mut().find(|prior| prior.id == issue.id) {
                *prior = issue;
            } else {
                repair_history.push(issue);
            }
        }
        repair(
            op,
            grounded,
            images,
            windows,
            inventory,
            outline,
            chapters,
            &repair_history,
        )
        .await?;
    }
    Ok(())
}

pub(super) fn finish(
    grounded: &RecapDocument,
    intent: GenerationIntent,
    previous: Option<&RecapDocument>,
    outline: Outline,
    chapters: Vec<ChapterComposition>,
    limitations: Vec<String>,
) -> Result<RecapDocument> {
    let mut document = grounded.clone();
    document.intent = intent;
    document.title = outline.title;
    document.sections = chapters
        .into_iter()
        .map(|mut chapter| {
            for block in &mut chapter.section.blocks {
                match &mut block.content {
                    crate::contracts::BlockContent::Image { asset_id, caption }
                    | crate::contracts::BlockContent::Clip { asset_id, caption } => {
                        if let Some(choice) = chapter
                            .image_choices
                            .iter()
                            .find(|choice| choice.asset_id == *asset_id)
                        {
                            caption.push_str(" — Why selected: ");
                            caption.push_str(&choice.reason);
                        }
                    }
                    crate::contracts::BlockContent::Comparison { items } => {
                        for item in items {
                            if let Some(choice) = chapter
                                .image_choices
                                .iter()
                                .find(|choice| choice.asset_id == item.asset_id)
                            {
                                item.label.push_str(" — Why selected: ");
                                item.label.push_str(&choice.reason);
                            }
                        }
                    }
                    crate::contracts::BlockContent::Text { .. } => {}
                }
            }
            chapter.section
        })
        .collect();
    document.revision = previous.map_or(Ok(1), |prior| {
        prior
            .revision
            .checked_add(1)
            .ok_or(CompositionError::Invalid("revision exhausted"))
    })?;
    append_limitations(&mut document, limitations)?;
    document.validate()?;
    Ok(document)
}
