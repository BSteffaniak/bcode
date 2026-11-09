//! Canonical accounting facts for usage reported by a managed native harness.
//!
//! A managed harness (for example Claude Code or Codex) reports its own per-result token counters
//! and, when its billing model has one, its own cost. These helpers turn those canonical
//! observations into request-keyed [`SessionTokenUsage`] contributions that the session usage
//! projection and every session view share. Native costs are vendor-reported amounts: they are
//! carried as-is and are never repriced by a Bcode pricing catalog.

use crate::{
    ExternalBackendEnvelope, ExternalBackendRecord, ExternalHarnessOperationRecord,
    SessionCostComponent, SessionCostEstimate, SessionEventKind, SessionTokenUsage,
};
use bcode_harness_models::{HarnessObservation, HarnessObservationKind};

/// Request-key prefix of every native harness usage contribution.
///
/// Keys are `external:{generation}:{observation_id}`, scoped to the managed ownership generation
/// so a reused native observation identity in a later generation remains distinct.
pub const NATIVE_HARNESS_USAGE_KEY_PREFIX: &str = "external:";

/// Normalized cost source label for vendor-reported native harness costs.
pub const NATIVE_HARNESS_COST_SOURCE: &str = "native harness report";

/// Pricing bucket of the single component that carries a vendor-reported native total.
///
/// The component reports no token count or per-token rate because the native harness reports
/// only the total amount.
pub const NATIVE_HARNESS_COST_BUCKET: &str = "native_reported_total";

/// Return whether a usage request key identifies a native harness contribution.
#[must_use]
pub fn is_native_harness_usage_key(key: &str) -> bool {
    key.starts_with(NATIVE_HARNESS_USAGE_KEY_PREFIX)
}

/// Return the request key of one native harness usage observation.
#[must_use]
pub fn native_harness_usage_key(generation: u64, observation_id: &str) -> String {
    format!("{NATIVE_HARNESS_USAGE_KEY_PREFIX}{generation}:{observation_id}")
}

/// Return the cost estimate carrying one vendor-reported native result total, in USD micros.
///
/// A harness without a monetary cost (for example a subscription) reports no total; its usage is
/// then counted without any cost estimate rather than as unavailable pricing.
#[must_use]
pub fn native_harness_cost(total_micros: u64) -> SessionCostEstimate {
    SessionCostEstimate::Estimated {
        currency: "USD".to_owned(),
        total_micros,
        components: vec![SessionCostComponent {
            bucket: NATIVE_HARNESS_COST_BUCKET.to_owned(),
            modality: None,
            tokens: 0,
            price_micros: 0,
            cost_micros: total_micros,
        }],
        source: NATIVE_HARNESS_COST_SOURCE.to_owned(),
        revision: None,
    }
}

fn saturating_u32(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Convert one native usage observation into its terminal request contribution.
///
/// Returns `None` for every observation kind other than
/// [`HarnessObservationKind::Usage`]. The returned usage carries its vendor-reported cost, if
/// any, in [`SessionTokenUsage::cost`].
#[must_use]
pub fn native_harness_usage(observation: &HarnessObservation) -> Option<SessionTokenUsage> {
    let HarnessObservationKind::Usage {
        input_tokens,
        output_tokens,
        cache_read_input_tokens,
        cache_creation_input_tokens,
        total_cost_micros,
        ..
    } = &observation.kind
    else {
        return None;
    };
    // Bcode input totals include cache subsets; native counters report them separately.
    let input = input_tokens
        .saturating_add(*cache_read_input_tokens)
        .saturating_add(*cache_creation_input_tokens);
    Some(SessionTokenUsage {
        request_id: Some(native_harness_usage_key(
            observation.generation,
            &observation.observation_id,
        )),
        observation_id: Some(observation.observation_id.clone()),
        terminal: true,
        input_tokens: Some(saturating_u32(input)),
        output_tokens: Some(saturating_u32(*output_tokens)),
        cached_input_tokens: Some(saturating_u32(*cache_read_input_tokens)),
        cache_write_input_tokens: Some(saturating_u32(*cache_creation_input_tokens)),
        cost: total_cost_micros.map(native_harness_cost),
        ..SessionTokenUsage::default()
    })
}

/// Return every native harness usage contribution carried by one canonical event, in order.
///
/// Managed harness observation pages and managed external-backend observations are both
/// recognized. Malformed external-backend envelopes contribute nothing; canonical append
/// validation owns their rejection.
#[must_use]
pub fn native_harness_usage_in_event(kind: &SessionEventKind) -> Vec<SessionTokenUsage> {
    if let SessionEventKind::ExternalHarnessOperation { operation } = kind {
        return match &operation.record {
            ExternalHarnessOperationRecord::ObservationPage { page, .. } => page
                .observations
                .iter()
                .filter_map(native_harness_usage)
                .collect(),
            _ => Vec::new(),
        };
    }
    match ExternalBackendEnvelope::from_event_kind(kind) {
        Ok(Some(ExternalBackendEnvelope {
            record: ExternalBackendRecord::Observation { observation },
            ..
        })) => native_harness_usage(&observation).into_iter().collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_harness_models::HarnessCompleteness;

    fn usage_observation(total_cost_micros: Option<u64>) -> HarnessObservation {
        HarnessObservation {
            observation_id: "result-1".into(),
            session_id: "session".into(),
            generation: 3,
            native_correlations: Vec::new(),
            completeness: HarnessCompleteness::Complete,
            kind: HarnessObservationKind::Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_input_tokens: 100,
                cache_creation_input_tokens: 20,
                total_cost_micros,
                duration_ms: None,
                model: None,
            },
        }
    }

    #[test]
    fn reported_native_cost_is_a_valid_complete_estimate() {
        let usage = native_harness_usage(&usage_observation(Some(1_234))).unwrap();
        assert_eq!(usage.request_id.as_deref(), Some("external:3:result-1"));
        assert!(is_native_harness_usage_key(
            usage.request_id.as_deref().unwrap()
        ));
        assert_eq!(usage.input_tokens, Some(130));
        assert_eq!(usage.metered_total_tokens(), Some(135));
        assert!(usage.terminal);
        usage.validate().unwrap();
        assert!(matches!(
            usage.cost,
            Some(SessionCostEstimate::Estimated {
                total_micros: 1_234,
                ref source,
                ..
            }) if source == NATIVE_HARNESS_COST_SOURCE
        ));
    }

    #[test]
    fn unpriced_native_usage_has_no_cost_estimate() {
        let usage = native_harness_usage(&usage_observation(None)).unwrap();
        assert_eq!(usage.cost, None);
        usage.validate().unwrap();
    }

    #[test]
    fn non_usage_observations_contribute_nothing() {
        let mut observation = usage_observation(None);
        observation.kind = HarnessObservationKind::Result {
            text: "done".into(),
        };
        assert!(native_harness_usage(&observation).is_none());
    }
}
