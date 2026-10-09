//! Explicitly trusted, deployment-scoped catalog support publication.
//!
//! Ingestion authenticates contributors outside this module. The caller pins exact
//! scopes authorized to publish facts; possession of a local evidence file is NOT
//! trust. A policy maps a reviewed endpoint to a public deployment. Private/custom
//! endpoints must not be allowed as evidence for another serving deployment.
use crate::{
    EvidenceCapability, EvidenceDocument, EvidenceError, EvidenceOutcome, EvidenceScope,
    ModelSupportEvidence,
    evidence::{attempt_order, validate_document},
};
use bcode_model_catalog_models::{
    CatalogDocument, ModelCatalogEntry, ModelDeployment, ModelSupportTarget,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Reviewed publication policy, supplied by the trusted catalog maintainer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupportTrustPolicy {
    /// Exact externally authenticated scopes authorized for shared publication.
    pub trusted_scopes: BTreeSet<EvidenceScope>,
    /// Minimum distinct accounts on the same endpoint for definitive removals.
    /// Must be at least two. Account denials are never eligible.
    pub removal_account_quorum: usize,
}
/// Versioned portable change proposal, always revalidated against policy on apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedSupportPatch {
    /// Patch contract; currently one.
    pub schema_version: u32,
    /// Exact catalog revision this proposal was prepared against.
    pub base_revision: String,
    /// Authenticated observations, not reports or raw provider errors.
    pub evidence: EvidenceDocument,
}
/// Validated changes applied, keyed by provider/model/serving target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedSupportChange {
    /// Provider-native model id.
    pub model_id: String,
    /// Only this deployment is modified.
    pub target: ModelSupportTarget,
    /// True adds support; false removes support on this target only.
    pub supported: bool,
}

/// Build a portable patch from trusted normalized observations.
///
/// # Errors
/// Rejects invalid observations/policy, untrusted scopes, and unknown contracts.
pub fn prepare_support_patch(
    catalog: &CatalogDocument,
    evidence: EvidenceDocument,
    policy: &SupportTrustPolicy,
    now_ms: u64,
) -> Result<SharedSupportPatch, EvidenceError> {
    changes(&evidence, policy, now_ms)?;
    Ok(SharedSupportPatch {
        schema_version: 1,
        base_revision: catalog.catalog_revision.clone(),
        evidence,
    })
}

/// Apply reviewed evidence transactionally, preserving other serving deployments.
///
///
/// Creates minimal entries for previously missing working models, with unknown
/// capability/context/pricing metadata. Does not infer tool/image/cache support.
///
/// # Errors
/// Rejects stale revisions, empty new revisions, untrusted/expired facts, unknown
/// providers, conflicting deployment evidence, or invalid resulting catalogs.
pub fn apply_support_patch(
    catalog: &mut CatalogDocument,
    patch: &SharedSupportPatch,
    policy: &SupportTrustPolicy,
    now_ms: u64,
    new_revision: &str,
) -> Result<Vec<AppliedSupportChange>, EvidenceError> {
    if patch.schema_version != 1 {
        return Err(EvidenceError::UnsupportedVersion);
    }
    if patch.base_revision != catalog.catalog_revision
        || new_revision.is_empty()
        || new_revision == patch.base_revision
    {
        return Err(EvidenceError::Invalid);
    }
    let changes = changes(&patch.evidence, policy, now_ms)?;
    let mut candidate = catalog.clone();
    for change in &changes {
        let provider = candidate
            .providers
            .get_mut(&change.target.provider)
            .ok_or(EvidenceError::Invalid)?;
        if !provider.models.contains_key(&change.model_id) {
            if !change.supported {
                continue;
            }
            let entry: ModelCatalogEntry = serde_json::from_value(serde_json::json!({"model_id":change.model_id,"display_name":change.model_id,"status":"unknown"})).map_err(|_| EvidenceError::Invalid)?;
            provider.models.insert(change.model_id.clone(), entry);
        }
        let entry = provider
            .models
            .get_mut(&change.model_id)
            .ok_or(EvidenceError::Invalid)?;
        if change.supported {
            entry.unsupported_by.remove(&change.target);
            // Preserve all operational metadata when restoring existing support.
            if !entry
                .deployments
                .iter()
                .any(|deployment| deployment.target == change.target)
                && !entry.supported_by.contains(&change.target)
            {
                entry.deployments.push(ModelDeployment {
                    target: change.target.clone(),
                    context_window: None,
                    max_output_tokens: None,
                    capabilities: bcode_model_catalog_models::CatalogCapabilities::default(),
                    reasoning: None,
                    pricing: None,
                });
            }
        } else {
            entry.supported_by.remove(&change.target);
            entry
                .deployments
                .retain(|deployment| deployment.target != change.target);
            entry.unsupported_by.insert(change.target.clone());
        }
    }
    new_revision.clone_into(&mut candidate.catalog_revision);
    crate::validate_catalog(&candidate).map_err(|_| EvidenceError::Invalid)?;
    *catalog = candidate;
    Ok(changes)
}

fn changes(
    document: &EvidenceDocument,
    policy: &SupportTrustPolicy,
    now_ms: u64,
) -> Result<Vec<AppliedSupportChange>, EvidenceError> {
    validate_document(document)?;
    if policy.trusted_scopes.is_empty() || policy.removal_account_quorum < 2 {
        return Err(EvidenceError::Invalid);
    }
    // Collapse repeated account observations before counting quorum. Newer facts
    // (even expired) supersede old facts; account denials contribute no changes.
    let mut latest: BTreeMap<(EvidenceScope, String), &ModelSupportEvidence> = BTreeMap::new();
    for e in &document.observations {
        if !policy.trusted_scopes.contains(&e.scope) || e.observed_at_ms > now_ms {
            return Err(EvidenceError::Invalid);
        }
        if !e.capabilities.contains(&EvidenceCapability::TextGeneration) {
            continue;
        }
        let key = (e.scope.clone(), e.model_id.clone());
        if let Some(old) = latest.get(&key) {
            if attempt_order(old) == attempt_order(e) && old.outcome != e.outcome {
                return Err(EvidenceError::Invalid);
            }
            if attempt_order(old) > attempt_order(e) {
                continue;
            }
        }
        latest.insert(key, e);
    }
    let mut groups: BTreeMap<(ModelSupportTarget, String, String), Vec<&ModelSupportEvidence>> =
        BTreeMap::new();
    for e in latest.into_values() {
        groups
            .entry((
                e.scope.target.clone(),
                e.scope.endpoint_fingerprint.clone(),
                e.model_id.clone(),
            ))
            .or_default()
            .push(e);
    }
    let mut result: BTreeMap<(ModelSupportTarget, String), AppliedSupportChange> = BTreeMap::new();
    for ((target, _, model_id), observations) in groups {
        let newest_success = observations
            .iter()
            .filter(|e| e.outcome == EvidenceOutcome::Success)
            .map(|e| attempt_order(e))
            .max();
        let unsupported = observations
            .iter()
            .filter(|e| {
                e.outcome == EvidenceOutcome::ProviderSurfaceUnsupported
                    && e.expires_at_ms > now_ms
                    && newest_success.is_none_or(|time| attempt_order(e) > time)
            })
            .count();
        let supported = if unsupported >= policy.removal_account_quorum {
            Some(false)
        } else if observations
            .iter()
            .any(|e| e.outcome == EvidenceOutcome::Success && e.expires_at_ms > now_ms)
            && !observations.iter().any(|e| {
                e.outcome == EvidenceOutcome::ProviderSurfaceUnsupported
                    && e.expires_at_ms > now_ms
                    && newest_success.is_some_and(|time| attempt_order(e) >= time)
            })
        {
            Some(true)
        } else {
            None
        };
        if let Some(supported) = supported {
            let change = AppliedSupportChange {
                model_id: model_id.clone(),
                target: target.clone(),
                supported,
            };
            let key = (target, model_id);
            if result
                .get(&key)
                .is_some_and(|old| old.supported != supported)
            {
                return Err(EvidenceError::Invalid);
            }
            result.insert(key, change);
        }
    }
    Ok(result.into_values().collect())
}
