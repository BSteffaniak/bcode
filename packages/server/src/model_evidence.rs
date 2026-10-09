//! Application-owned conversion of normalized provider observations into optional evidence.
use super::{BTreeSet, ServerState};
use bcode_model::{ModelAvailabilityObservation, ModelCheckStatus};
use bcode_model_catalog::{
    EvidenceCapability, EvidenceOutcome, EvidenceScope, EvidenceStore, ModelSupportEvidence,
};

pub fn persist(state: &ServerState, observation: &ModelAvailabilityObservation) -> bool {
    let outcome = match observation.status {
        ModelCheckStatus::Working => EvidenceOutcome::Success,
        ModelCheckStatus::Unsupported => EvidenceOutcome::ProviderSurfaceUnsupported,
        ModelCheckStatus::AccountDenied => EvidenceOutcome::AccountDenied,
        _ => return false,
    };
    let target = &observation.scope.target;
    let scope = EvidenceScope {
        target: bcode_model_catalog_models::ModelSupportTarget {
            provider: target.provider.clone(),
            auth_mode: target.auth_mode.clone(),
            api_surface: target.api_surface.clone(),
            integration: target.integration.clone(),
        },
        endpoint_fingerprint: observation.scope.endpoint_fingerprint.clone(),
        account_fingerprint: observation.scope.account_fingerprint.clone(),
    };
    let evidence = ModelSupportEvidence {
        scope,
        model_id: observation.model_id.clone(),
        outcome,
        capabilities: BTreeSet::from([EvidenceCapability::TextGeneration]),
        attempt_started_at_ms: observation.attempt_started_at_ms,
        attempt_ordinal: observation.attempt_ordinal,
        observed_at_ms: observation.observed_at_ms,
        expires_at_ms: observation
            .observed_at_ms
            .saturating_add(24 * 60 * 60 * 1000),
    };
    EvidenceStore::new(&state.state_root)
        .record(&evidence, observation.observed_at_ms)
        .is_ok()
}
