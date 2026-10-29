//! Host-owned source selection and admission for the experimental passage path.
use std::collections::BTreeSet;

use crate::{
    analysis_contracts::DetailedPolicy, contracts::RecapDocument, model::ImageAsset,
    passage_contracts::SourceUnit, process::Cancellation,
};

use super::{CompositionError, Result, cancelled, check};

pub(super) fn admit(
    document: &RecapDocument,
    images: &[ImageAsset],
    previous: Option<&RecapDocument>,
    policy: &DetailedPolicy,
    cancel: &Cancellation,
) -> Result<()> {
    cancelled(cancel)?;
    policy.validate()?;
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
    if serde_json::to_vec(document)?.len() > total_input
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
    document.validate()?;
    check(!document.evidence.is_empty(), "no passage source evidence")?;
    if let Some(prior) = previous {
        prior.validate()?;
        check(
            prior.id == document.id
                && prior.source == document.source
                && prior.transcript == document.transcript
                && prior.evidence == document.evidence
                && prior.assets == document.assets,
            "passage refinement changed grounding",
        )?;
    }
    let mut supplied = BTreeSet::new();
    for image in images {
        let unique = supplied.insert(image.asset_id.as_str());
        let registered = document
            .assets
            .iter()
            .find(|asset| asset.id == image.asset_id);
        check(
            unique
                && !image.bytes.is_empty()
                && registered.is_some_and(|asset| {
                    asset.mime_type == image.mime_type && asset.mime_type.starts_with("image/")
                }),
            "invalid passage source image",
        )?;
    }
    if policy.inspect_visuals {
        check(
            document
                .assets
                .iter()
                .filter(|asset| asset.mime_type.starts_with("image/"))
                .all(|asset| supplied.contains(asset.id.as_str())),
            "passage source image bytes missing",
        )?;
    }
    Ok(())
}

pub(super) fn units(
    document: &RecapDocument,
    policy: &DetailedPolicy,
    cancel: &Cancellation,
) -> Result<Vec<SourceUnit>> {
    let units = crate::source_analysis::passage_units(document, policy, cancel)?;
    let mut primary_ids = BTreeSet::new();
    for unit in &units {
        cancelled(cancel)?;
        for cue in &unit.primary.transcript {
            check(
                primary_ids.insert(cue.id.clone()),
                "duplicate passage primary ownership",
            )?;
        }
    }
    check(
        document
            .transcript
            .iter()
            .all(|cue| primary_ids.contains(&cue.id)),
        "unowned primary transcript",
    )?;
    Ok(units)
}

pub(super) fn frames(unit: &SourceUnit, images: &[ImageAsset], inspect: bool) -> Vec<ImageAsset> {
    images
        .iter()
        .filter(|image| {
            inspect
                && unit
                    .primary
                    .assets
                    .iter()
                    .any(|asset| asset.id == image.asset_id)
        })
        .cloned()
        .collect()
}
