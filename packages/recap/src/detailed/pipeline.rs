//! Focused pipeline stages; all inputs and prior revisions remain immutable.
use super::stages::{audit, repair};
use super::validation::{check, exact_ids, validate_inventory};
use super::{
    AnalysisResponse, AnalysisStage, Cancellation, CompositionError, DetailedPolicy,
    GenerationIntent, ImageAsset, InformationItem, Operation, RecapDocument, Result, SourceWindow,
    check_cancelled, frames_for, namespace_items, relevant_windows, sources_for, stages,
    validation, window_source,
};
use super::{
    execution::PreparedCall,
    ownership::{Findings, Ownership},
};
use crate::analysis_contracts::{ChapterComposition, Outline};
use serde_json::json;
use std::collections::BTreeMap;

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
    let total_input = policy.allowances.max_input_bytes.saturating_mul(
        if policy.allowances.max_source_windows == 0 {
            usize::MAX
        } else {
            policy.allowances.max_source_windows
        },
    );
    let total_images = policy.allowances.max_image_bytes.saturating_mul(
        if policy.allowances.max_source_windows == 0 {
            usize::MAX
        } else {
            policy.allowances.max_source_windows
        },
    );
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
    let response_limit = op.policy.allowances.max_response_bytes;
    let inspect_visuals = op.policy.inspect_visuals;
    let requests = windows.iter().flat_map(|window| {
        let text = PreparedCall::new(AnalysisStage::ExtractionInventory,
            json!({"window":window,"untrusted_source":window_source(grounded,window)}), vec![],
            "Inventory ALL substantive information in this source window; keep exact details. Return this window_id. Empty inventory requires an honest limitation.", response_limit);
        let frames: Vec<_> = images.iter().filter(|image| window.asset_ids.contains(&image.asset_id)).cloned().collect();
        let visual = (inspect_visuals && !frames.is_empty()).then(|| PreparedCall::new(
            AnalysisStage::VisualInspection,
            json!({"window":window,"untrusted_source":window_source(grounded,window)}), frames,
            "Inspect every supplied frame, especially visual-only information, diagrams, code and labels absent from speech. Return all inspected asset IDs and grounded items.", response_limit));
        std::iter::once(text).chain(visual)
    });
    let mut responses = op.run(Box::new(requests)).await?.into_iter();
    for window in windows {
        let response = responses
            .next()
            .ok_or(CompositionError::Invalid("missing inventory response"))?;
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
            let response = responses
                .next()
                .ok_or(CompositionError::Invalid("missing visual response"))?;
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
    let response_limit = op.policy.allowances.max_response_bytes;
    let requests = outline.chapters.iter().map(|plan| {
        let items: Vec<_> = inventory
            .iter()
            .filter(|item| plan.item_ids.contains(&item.id))
            .cloned()
            .collect();
        let relevant = relevant_windows(windows, &items);
        let frames = frames_for(images, &relevant);
        let allowed = validation::ChapterSources::derive(grounded, &items, &relevant, &frames);
        PreparedCall::new(AnalysisStage::ChapterComposition,
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
                claims from source context. Explain every selected asset in image_choices. \
                Write a faithful readable adaptation, not an inventory recital: connect the source's \
                explanations, worked examples and reasons in source order. Preserve exact quantities, \
                units, conditions, negations, caveats and procedure order. Use the supplied local source \
                to disambiguate assigned items, not to invent bridging facts or import other topics. \
                Any separately labeled preceding context explains boundaries only; do not claim it as \
                newly assigned coverage. Focus changes emphasis, never excludes substantive material.", response_limit)
    });
    let responses = op.run(Box::new(requests)).await?;
    for (plan, response) in outline.chapters.iter().zip(responses) {
        let items: Vec<_> = inventory
            .iter()
            .filter(|item| plan.item_ids.contains(&item.id))
            .cloned()
            .collect();
        let relevant = relevant_windows(windows, &items);
        let frames = frames_for(images, &relevant);
        let allowed = validation::ChapterSources::derive(grounded, &items, &relevant, &frames);
        let AnalysisResponse::ChapterComposition(chapter) = response else {
            unreachable!()
        };
        allowed.validate(grounded, plan, &chapter, &items)?;
        chapters.push(chapter);
    }
    Ok(chapters)
}

/// Full rendered review input for one source window, including fallback targets.
fn coverage_chapters(
    chapters: &[ChapterComposition],
    inventory: &[InformationItem],
    window: &SourceWindow,
    items: &[InformationItem],
) -> Vec<ChapterComposition> {
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
    relevant
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
    // A clean source audit is reusable only while its entire selected chapter
    // input is unchanged. This invalidates every window sharing a changed chapter,
    // including old/new evidence links and nearest-chapter fallback selection.
    // Final independent consistency never consumes these operation-local receipts.
    let ownership = Ownership::new(outline);
    let mut repair_history = Findings::default();
    let mut candidates = vec![chapters.to_vec()];
    let mut clean: BTreeMap<String, (Vec<ChapterComposition>, Vec<String>)> = BTreeMap::new();
    for round in 0..=op.policy.allowances.max_repair_rounds {
        let mut issues = Vec::new();
        for window in windows {
            op.cancelled()?;
            let items: Vec<_> = inventory
                .iter()
                .filter(|item| item.window_id == window.id)
                .cloned()
                .collect();
            let relevant = coverage_chapters(chapters, inventory, window, &items);
            if clean
                .get(&window.id)
                .is_some_and(|(reviewed, _)| *reviewed == relevant)
            {
                continue;
            }
            let (found, limits) = audit(
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
            if found.is_empty() {
                clean.insert(window.id.clone(), (relevant, limits));
            } else {
                clean.remove(&window.id);
            }
            issues.extend(
                found
                    .into_iter()
                    .map(|issue| ownership.finding(issue, [window.id.clone()].into())),
            );
        }
        if issues.is_empty() {
            limitations.extend(clean.into_values().flat_map(|(_, limits)| limits));
            break;
        }
        if round == op.policy.allowances.max_repair_rounds {
            return Err(CompositionError::Invalid("unresolved source coverage gaps"));
        }
        let current = repair_history.record(issues);
        let preservation: Vec<_> = repair_history
            .entries()
            .iter()
            .filter(|prior| !current.iter().any(|open| open.issue.id == prior.issue.id))
            .cloned()
            .collect();
        repair(
            op,
            grounded,
            images,
            windows,
            inventory,
            outline,
            chapters,
            &current,
            &preservation,
        )
        .await?;
        check(
            !candidates.iter().any(|prior| prior.as_slice() == chapters),
            "source repair made no progress or repeated a candidate",
        )?;
        candidates.push(chapters.to_vec());
    }
    Ok(())
}

pub(super) fn finish(
    grounded: &RecapDocument,
    intent: GenerationIntent,
    previous: Option<&RecapDocument>,
    outline: Outline,
    chapters: Vec<ChapterComposition>,
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
    document.validate()?;
    Ok(document)
}
