//! Availability union at the central catalog boundary.
use crate::{
    EvidenceCapability, EvidenceDocument, EvidenceError, EvidenceResolution, EvidenceScope,
    ModelCatalog,
    evidence::{attempt_order, validate_document},
    resolve_evidence,
};
use bcode_model::{
    ModelAvailability, ModelAvailabilityReason, ModelAvailabilityStatus, ModelCatalogSupportHint,
    ModelInfo, ModelVisibility,
};
use std::collections::{BTreeMap, BTreeSet};

/// Models and normalized availability decisions for one active serving identity.
pub type EvidenceModelUnion = (Vec<ModelInfo>, BTreeMap<String, ModelAvailability>);

/// Evidence snapshot and exact active scope for one resolution call.
#[derive(Debug, Clone, Copy)]
pub struct ModelEvidenceContext<'a> {
    /// Exact provider/auth/endpoint/account serving identity.
    pub scope: &'a EvidenceScope,
    /// Validated portable observations.
    pub document: &'a EvidenceDocument,
    /// Current Unix epoch milliseconds.
    pub now_ms: u64,
}

impl crate::ModelCatalogResolver {
    /// Resolve a selected/configured view using catalog plus scoped actual observations.
    /// All visibility filtering happens after union so working undeclared models survive.
    /// Apply local ignore policy at the caller after this method.
    ///
    /// # Errors
    /// Returns evidence validation/scope failures without modifying the catalog.
    pub async fn resolve_view_with_evidence(
        &self,
        list: bcode_model::ModelList,
        selected_model_id: Option<&str>,
        configured_model_id: Option<&str>,
        view: crate::ModelListView,
        evidence: ModelEvidenceContext<'_>,
    ) -> Result<bcode_model::ModelList, EvidenceError> {
        let mut resolved = self
            .resolve_selection(list, selected_model_id, configured_model_id)
            .await;
        let catalog = self.catalog_snapshot().await;
        let (models, availability) = catalog.merge_provider_models_with_evidence(
            &evidence.scope.target.provider,
            resolved.models,
            false,
            evidence.scope,
            evidence.document,
            evidence.now_ms,
        )?;
        resolved.models = models;
        resolved.catalog.availability = availability;
        if view == crate::ModelListView::UserVisible {
            resolved
                .models
                .retain(|model| model.visibility == ModelVisibility::Visible);
        }
        let preferred = configured_model_id.or(selected_model_id);
        let default = preferred
            .filter(|id| {
                resolved.models.iter().any(|model| {
                    model.model_id == *id && model.visibility == ModelVisibility::Visible
                })
            })
            .map(str::to_owned)
            .or_else(|| {
                resolved
                    .models
                    .iter()
                    .find(|model| model.is_default && model.visibility == ModelVisibility::Visible)
                    .map(|model| model.model_id.clone())
            })
            .or_else(|| {
                resolved
                    .models
                    .iter()
                    .find(|model| model.visibility == ModelVisibility::Visible)
                    .map(|model| model.model_id.clone())
            });
        for model in &mut resolved.models {
            model.is_default = default.as_deref() == Some(&model.model_id);
        }
        Ok(resolved)
    }
}

impl ModelCatalog {
    /// Union discovered/catalog/evidence models without treating missing declarations
    /// as unsupported. Successful evidence overrides only catalog support filtering;
    /// local ignore policy must be applied afterwards. No new capability is inferred.
    ///
    /// # Errors
    /// Rejects invalid/version-incompatible evidence snapshots.
    pub fn merge_provider_models_with_evidence(
        &self,
        provider_id: &str,
        discovered: Vec<ModelInfo>,
        include_catalog_only: bool,
        scope: &EvidenceScope,
        document: &EvidenceDocument,
        now_ms: u64,
    ) -> Result<EvidenceModelUnion, EvidenceError> {
        validate_document(document)?;
        if provider_id != scope.target.provider {
            return Err(EvidenceError::Invalid);
        }
        let mut models = self.merge_provider_models_for_target(
            provider_id,
            discovered,
            include_catalog_only,
            Some(&scope.target),
        );
        let mut seen: BTreeSet<_> = models.iter().map(|model| model.model_id.clone()).collect();
        for observation in &document.observations {
            if observation.scope == *scope
                && !seen.contains(&observation.model_id)
                && resolve_evidence(
                    document,
                    scope,
                    &observation.model_id,
                    EvidenceCapability::TextGeneration,
                    now_ms,
                ) == EvidenceResolution::Supported
            {
                let entry: bcode_model_catalog_models::ModelCatalogEntry = serde_json::from_value(serde_json::json!({"model_id":observation.model_id,"display_name":observation.model_id,"status":"unknown"})).map_err(|_| EvidenceError::Invalid)?;
                let model = self.enrich_model_for_target(
                    provider_id,
                    crate::model_info_from_catalog_entry_for_target(&entry, &scope.target),
                    &scope.target,
                );
                seen.insert(observation.model_id.clone());
                models.push(model);
            }
        }
        let mut availability = BTreeMap::new();
        for model in &mut models {
            let resolution = resolve_evidence(
                document,
                scope,
                &model.model_id,
                EvidenceCapability::TextGeneration,
                now_ms,
            );
            let declared = self
                .model(provider_id, &model.model_id)
                .is_some_and(|entry| {
                    crate::model_matches_support_target(entry, &scope.target, false)
                });
            let (status, reason) = apply_resolution(model, resolution, declared);
            let latest = document
                .observations
                .iter()
                .filter(|e| {
                    e.model_id == model.model_id
                        && e.scope.target == scope.target
                        && e.scope.endpoint_fingerprint == scope.endpoint_fingerprint
                        && (e.scope.account_fingerprint == scope.account_fingerprint
                            || e.outcome == crate::EvidenceOutcome::ProviderSurfaceUnsupported)
                        && e.capabilities.contains(&EvidenceCapability::TextGeneration)
                        && e.observed_at_ms <= now_ms
                })
                .max_by_key(|e| attempt_order(e));
            let timed = (resolution != EvidenceResolution::Unverified)
                .then_some(latest)
                .flatten();
            availability.insert(
                model.model_id.clone(),
                ModelAvailability {
                    status,
                    reason,
                    target: ModelCatalogSupportHint {
                        provider: scope.target.provider.clone(),
                        auth_mode: scope.target.auth_mode.clone(),
                        api_surface: scope.target.api_surface.clone(),
                        integration: scope.target.integration.clone(),
                    },
                    observed_at_ms: timed.map(|e| e.observed_at_ms),
                    expires_at_ms: timed.map(|e| e.expires_at_ms),
                },
            );
        }
        Ok((models, availability))
    }
}

fn apply_resolution(
    model: &mut ModelInfo,
    resolution: EvidenceResolution,
    declared: bool,
) -> (ModelAvailabilityStatus, ModelAvailabilityReason) {
    match resolution {
        EvidenceResolution::Supported => {
            if matches!(model.visibility, ModelVisibility::Unsupported { .. }) {
                model.visibility = ModelVisibility::Visible;
            }
            (
                ModelAvailabilityStatus::Verified,
                ModelAvailabilityReason::ObservedSuccess,
            )
        }
        EvidenceResolution::Unsupported => {
            model.visibility = ModelVisibility::Unsupported {
                reason: "provider surface does not support this model".to_owned(),
            };
            (
                ModelAvailabilityStatus::Unsupported,
                ModelAvailabilityReason::ProviderSurfaceUnsupported,
            )
        }
        EvidenceResolution::AccountDenied => {
            model.visibility = ModelVisibility::Unsupported {
                reason: "active account is not entitled to this model".to_owned(),
            };
            (
                ModelAvailabilityStatus::AccountDenied,
                ModelAvailabilityReason::AccountDenied,
            )
        }
        EvidenceResolution::Unverified if declared => (
            ModelAvailabilityStatus::CatalogDeclared,
            ModelAvailabilityReason::CatalogDeclaration,
        ),
        EvidenceResolution::Unverified => (
            ModelAvailabilityStatus::Unverified,
            ModelAvailabilityReason::NoEvidence,
        ),
    }
}
