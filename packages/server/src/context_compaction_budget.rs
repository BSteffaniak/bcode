//! Frozen capacity policy for local compaction. This module neither publishes history nor
//! authorizes requests: durable attempt reservations and ownership remain orchestration concerns.

use super::{
    CompactionError, ModelTurnRequest, compaction_capacity_tokens, local_request_estimate,
};
use bcode_session_models::LocalContextEstimate;

/// Independently versioned checkpoint policy contract; unsupported versions fail closed.
pub(super) const SUMMARY_POLICY_VERSION: u16 = 1;

/// Accounting of complete requests, including the actual retained tail, tools and overhead.
/// Both estimates must come from the same canonical semantic estimator, not text heuristics.
pub(super) struct SummaryBudgetInput {
    pub provider_id: String,
    pub model_id: String,
    pub context_window: u32,
    /// Output setting for synthesis original/repair requests.
    pub requested_max_output_tokens: Option<u32>,
    /// Output setting for the actual resumed main request; may differ from synthesis.
    pub retained_requested_max_output_tokens: Option<u32>,
    pub provider_max_output_tokens: Option<u32>,
    pub before: LocalContextEstimate,
    /// Complete post-compaction request with an empty summary but the real retained tail.
    pub retained_without_summary: LocalContextEstimate,
    /// Zero selects capacity-only reconstruction; positive values require net reclaim.
    pub minimum_reclaim_tokens: u64,
    /// Presentation preference, never an acceptance ceiling.
    pub preferred_summary_tokens: u64,
    /// Independent allocation/serialization limits, in UTF-8 bytes, not characters or tokens.
    pub maximum_summary_bytes: usize,
    pub maximum_response_bytes: usize,
}

/// Persist this entire value in the checkpoint identity. Resolve once per operation, not per
/// attempt. Changing model, accounting, retention or limits invalidates reuse rather than
/// reinterpreting an accepted carry. Historical classification belongs to session migration.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ResolvedSummaryPolicy {
    version: u16,
    provider_id: String,
    model_id: String,
    estimator_version: u16,
    context_window: u32,
    requested_max_output_tokens: Option<u32>,
    retained_requested_max_output_tokens: Option<u32>,
    provider_max_output_tokens: Option<u32>,
    before_tokens: u64,
    retained_without_summary_tokens: u64,
    minimum_reclaim_tokens: u64,
    preferred_summary_tokens: u64,
    maximum_retained_tokens: u64,
    available_input_tokens: u64,
    retained_available_input_tokens: u64,
    output_reserve_tokens: u64,
    maximum_summary_bytes: usize,
    maximum_response_bytes: usize,
}

fn invalid(message: &str) -> CompactionError {
    CompactionError::Provider(format!("compaction summary policy: {message}"))
}

impl ResolvedSummaryPolicy {
    /// Resolve from the same model capacity and complete-request estimates used by the parent.
    ///
    /// # Errors
    /// Rejects missing identity, mismatched estimators, invalid bounds, and budgets that cannot
    /// retain any summary while meeting both safe capacity and the requested net reclaim.
    pub(super) fn resolve(input: SummaryBudgetInput) -> Result<Self, CompactionError> {
        if input.provider_id.is_empty()
            || input.model_id.is_empty()
            || input.context_window == 0
            || input.before.algorithm_version != input.retained_without_summary.algorithm_version
            || input.before.algorithm_version
                != crate::context_accounting::LOCAL_CONTEXT_ESTIMATOR_VERSION
            || input.before.tokens == u64::MAX
            || input.retained_without_summary.tokens == u64::MAX
            || input.maximum_summary_bytes == 0
            || input.maximum_response_bytes == 0
            || input.maximum_summary_bytes > 1_048_576
            || input.maximum_response_bytes > 4_194_304
            || input.provider_id.len() > 1_024
            || input.model_id.len() > 1_024
            || input.preferred_summary_tokens == 0
        {
            return Err(invalid("invalid accounting, identity or safety bounds"));
        }
        let capacity = compaction_capacity_tokens(
            input.context_window,
            bcode_config::ProactiveCompactionThreshold::Percent(100),
            input.requested_max_output_tokens,
            input.provider_max_output_tokens,
        );
        let retained_capacity = compaction_capacity_tokens(
            input.context_window,
            bcode_config::ProactiveCompactionThreshold::Percent(100),
            input.retained_requested_max_output_tokens,
            input.provider_max_output_tokens,
        );
        let maximum_after_tokens = if input.minimum_reclaim_tokens == 0 {
            retained_capacity.available_input_tokens
        } else {
            input
                .before
                .tokens
                .checked_sub(input.minimum_reclaim_tokens)
                .ok_or_else(|| invalid("required reclaim exceeds original request"))?
                .min(retained_capacity.available_input_tokens)
        };
        let maximum_retained_tokens = maximum_after_tokens
            .checked_sub(input.retained_without_summary.tokens)
            .ok_or_else(|| invalid(&format!(
                "retained tail and overhead leave no summary budget (before={}, retained={}, capacity={}, required_reclaim={})",
                input.before.tokens, input.retained_without_summary.tokens,
                retained_capacity.available_input_tokens, input.minimum_reclaim_tokens,
            )))?;
        if maximum_retained_tokens == 0 {
            return Err(invalid("no summary capacity remains"));
        }
        Ok(Self {
            version: SUMMARY_POLICY_VERSION,
            provider_id: input.provider_id,
            model_id: input.model_id,
            estimator_version: input.before.algorithm_version,
            context_window: input.context_window,
            requested_max_output_tokens: input.requested_max_output_tokens,
            retained_requested_max_output_tokens: input.retained_requested_max_output_tokens,
            provider_max_output_tokens: input.provider_max_output_tokens,
            before_tokens: input.before.tokens,
            retained_without_summary_tokens: input.retained_without_summary.tokens,
            minimum_reclaim_tokens: input.minimum_reclaim_tokens,
            preferred_summary_tokens: input.preferred_summary_tokens.min(maximum_retained_tokens),
            maximum_retained_tokens,
            available_input_tokens: capacity.available_input_tokens,
            retained_available_input_tokens: retained_capacity.available_input_tokens,
            output_reserve_tokens: capacity.output_reserve_tokens,
            maximum_summary_bytes: input.maximum_summary_bytes,
            maximum_response_bytes: input.maximum_response_bytes,
        })
    }

    /// Validate the current contract and every derived field after checkpoint decoding.
    /// # Errors
    /// Unsupported or inconsistent policy data is rejected without mutation or fallback.
    pub(super) fn validate(&self) -> Result<(), CompactionError> {
        if self.version != SUMMARY_POLICY_VERSION {
            return Err(invalid("unsupported policy version"));
        }
        let rebuilt = Self::resolve(SummaryBudgetInput {
            provider_id: self.provider_id.clone(),
            model_id: self.model_id.clone(),
            context_window: self.context_window,
            requested_max_output_tokens: self.requested_max_output_tokens,
            retained_requested_max_output_tokens: self.retained_requested_max_output_tokens,
            provider_max_output_tokens: self.provider_max_output_tokens,
            before: LocalContextEstimate {
                tokens: self.before_tokens,
                algorithm_version: self.estimator_version,
            },
            retained_without_summary: LocalContextEstimate {
                tokens: self.retained_without_summary_tokens,
                algorithm_version: self.estimator_version,
            },
            minimum_reclaim_tokens: self.minimum_reclaim_tokens,
            preferred_summary_tokens: self.preferred_summary_tokens,
            maximum_summary_bytes: self.maximum_summary_bytes,
            maximum_response_bytes: self.maximum_response_bytes,
        })?;
        if rebuilt != *self {
            return Err(invalid("inconsistent resolved policy"));
        }
        Ok(())
    }

    #[must_use]
    pub(super) const fn preferred_summary_tokens(&self) -> u64 {
        self.preferred_summary_tokens
    }

    #[must_use]
    pub(super) const fn maximum_retained_tokens(&self) -> u64 {
        self.maximum_retained_tokens
    }

    #[must_use]
    pub(super) const fn maximum_summary_bytes(&self) -> usize {
        self.maximum_summary_bytes
    }

    #[must_use]
    pub(super) const fn maximum_response_bytes(&self) -> usize {
        self.maximum_response_bytes
    }

    #[must_use]
    pub(super) const fn available_input_tokens(&self) -> u64 {
        self.available_input_tokens
    }

    /// Check the actual assembled original OR repair request immediately before reservation/send.
    /// # Errors
    /// Rejects invalid policy, changed output settings, or unsafe complete input requests.
    pub(super) fn validate_request(
        &self,
        request: &ModelTurnRequest,
    ) -> Result<(), CompactionError> {
        self.validate()?;
        if request.model_id != self.model_id
            || request.parameters.max_output_tokens != self.requested_max_output_tokens
        {
            return Err(invalid("request output settings changed"));
        }
        if local_request_estimate(request).tokens > self.available_input_tokens {
            return Err(invalid(
                "complete synthesis request exceeds safe input capacity",
            ));
        }
        Ok(())
    }

    /// Validate wire safety for an intermediate response, without requiring final net reclaim.
    /// Semantic validity and merged-carry safety are separate synthesis/capacity checks.
    /// # Errors
    /// Rejects invalid frozen policy, excessive UTF-8 wire bytes, or estimated output tokens.
    pub(super) fn validate_response(&self, response: &str) -> Result<(), CompactionError> {
        self.validate()?;
        if response.len() > self.maximum_response_bytes {
            return Err(invalid("wire response exceeds byte safety bound"));
        }
        if crate::context_accounting::estimated_tokens_from_chars(response.chars().count())
            > self.output_reserve_tokens
        {
            return Err(invalid("wire response exceeds output reserve"));
        }
        Ok(())
    }

    /// Validate the actual retained request, including a compatible opaque native snapshot.
    /// Native projection need not resemble the portable-summary baseline. The caller separately
    /// validates snapshot compatibility, portable fallback semantics and publication ownership.
    /// # Errors
    /// Rejects byte/output overflow, changed identity, unsafe actual capacity or failed reclaim.
    pub(super) fn validate_retained_request(
        &self,
        summary: &str,
        response: &str,
        retained_request: &ModelTurnRequest,
    ) -> Result<(), CompactionError> {
        self.validate_response(response)?;
        if summary.is_empty() || summary.len() > self.maximum_summary_bytes {
            return Err(invalid("summary exceeds byte safety bound or is empty"));
        }
        let after = local_request_estimate(retained_request);
        if retained_request.model_id != self.model_id
            || retained_request.parameters.max_output_tokens
                != self.retained_requested_max_output_tokens
            || after.algorithm_version != self.estimator_version
            || after.tokens > self.retained_available_input_tokens
            || (self.minimum_reclaim_tokens > 0
                && self.before_tokens.saturating_sub(after.tokens) < self.minimum_reclaim_tokens)
        {
            return Err(invalid(
                "retained request exceeds capacity or fails required net reclaim",
            ));
        }
        Ok(())
    }

    /// Validate a host-merged carry and its complete actual post-compaction request. This is a
    /// capacity gate only: provenance, chronology, scope reconciliation and coverage MUST already
    /// have passed the synthesis validator. No input is shortened or rewritten here.
    /// # Errors
    /// Rejects oversized UTF-8 data, output, retained request, or insufficient net reclaim.
    pub(super) fn validate_candidate(
        &self,
        summary: &str,
        response: &str,
        retained_request: &ModelTurnRequest,
    ) -> Result<(), CompactionError> {
        self.validate_retained_request(summary, response, retained_request)?;
        let after = local_request_estimate(retained_request);
        if after.tokens < self.retained_without_summary_tokens
            || after.tokens - self.retained_without_summary_tokens > self.maximum_retained_tokens
        {
            return Err(invalid(
                "portable retained request exceeds resolved summary budget",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_accounting::LOCAL_CONTEXT_ESTIMATOR_VERSION;

    fn input() -> SummaryBudgetInput {
        let estimate = |tokens| LocalContextEstimate {
            tokens,
            algorithm_version: LOCAL_CONTEXT_ESTIMATOR_VERSION,
        };
        SummaryBudgetInput {
            provider_id: "bcode.openai-compatible".into(),
            model_id: "gpt-6.1-sol".into(),
            context_window: 128_000,
            requested_max_output_tokens: Some(16_000),
            retained_requested_max_output_tokens: None,
            provider_max_output_tokens: Some(32_000),
            before: estimate(80_000),
            retained_without_summary: estimate(20_000),
            minimum_reclaim_tokens: 10_000,
            preferred_summary_tokens: 1_500,
            maximum_summary_bytes: 65_536,
            maximum_response_bytes: 131_072,
        }
    }

    #[test]
    fn native_projection_below_portable_baseline_uses_actual_capacity_and_reclaim() {
        let policy = ResolvedSummaryPolicy::resolve(input()).unwrap();
        let mut request = super::super::build_compaction_request(
            crate::SessionId::new(),
            "gpt-6.1-sol".into(),
            bcode_model::ProviderRequestContext::default(),
            "compatible native snapshot retained tail",
            "native-budget-test".into(),
        );
        // Compatibility is established by the provider-context projector before this gate;
        // accounting consumes only its actual semantic request, not opaque snapshot bytes.
        assert!(local_request_estimate(&request).tokens < policy.retained_without_summary_tokens);
        policy
            .validate_retained_request("portable fallback", "", &request)
            .unwrap();
        assert!(
            policy
                .validate_candidate("portable fallback", "", &request)
                .is_err()
        );
        request.model_id = "changed-model".into();
        assert!(
            policy
                .validate_retained_request("portable fallback", "", &request)
                .is_err()
        );
        request.model_id = "gpt-6.1-sol".into();
        request.parameters.max_output_tokens = Some(1);
        assert!(
            policy
                .validate_retained_request("portable fallback", "", &request)
                .is_err()
        );
        request.parameters.max_output_tokens = None;
        request.provider_context.request.insert(
            "semantic_overhead".into(),
            serde_json::json!("x".repeat(600_000)).into(),
        );
        assert!(
            policy
                .validate_retained_request("portable fallback", "", &request)
                .is_err()
        );
        request.provider_context.request.clear();
        let mut value = input();
        value.minimum_reclaim_tokens = value.before.tokens - 1;
        value.retained_without_summary.tokens = 0;
        let strict = ResolvedSummaryPolicy::resolve(value).unwrap();
        assert!(local_request_estimate(&request).tokens > 1);
        assert!(
            strict
                .validate_retained_request("portable fallback", "", &request)
                .is_err()
        );
        assert!(
            policy
                .validate_retained_request(
                    "portable fallback",
                    &"x".repeat(policy.maximum_response_bytes() + 1),
                    &request,
                )
                .is_err()
        );
    }

    #[test]
    fn zero_reclaim_reconstruction_can_grow_but_still_checks_actual_capacity() {
        let mut retained = super::super::build_compaction_request(
            crate::SessionId::new(),
            "gpt-6.1-sol".into(),
            bcode_model::ProviderRequestContext::default(),
            "",
            "reconstruction-budget-test".into(),
        );
        let mut value = input();
        value.minimum_reclaim_tokens = 0;
        value.retained_without_summary = local_request_estimate(&retained);
        value.before.tokens = 1;
        let policy = ResolvedSummaryPolicy::resolve(value).unwrap();
        policy.validate().unwrap();
        assert!(policy.maximum_retained_tokens() > 16_000);
        let summary = "legitimate reconstruction evidence ".repeat(200);
        retained.messages[0].content = vec![super::super::ContentBlock::Text {
            text: summary.clone(),
        }];
        assert!(local_request_estimate(&retained).tokens > policy.before_tokens);
        policy.validate_candidate(&summary, "", &retained).unwrap();
        retained.provider_context.request.insert(
            "semantic_overhead".into(),
            serde_json::json!("x".repeat(600_000)).into(),
        );
        assert!(policy.validate_candidate(&summary, "", &retained).is_err());
    }

    #[test]
    fn positive_reclaim_still_caps_after_tokens_below_before() {
        let mut value = input();
        value.before.tokens = 20_100;
        value.minimum_reclaim_tokens = 50;
        let policy = ResolvedSummaryPolicy::resolve(value).unwrap();
        assert_eq!(policy.maximum_retained_tokens(), 50);
        policy.validate().unwrap();
        let mut value = input();
        value.before.tokens = 1;
        value.minimum_reclaim_tokens = 2;
        assert!(ResolvedSummaryPolicy::resolve(value).is_err());
    }

    #[test]
    fn partial_response_checks_only_frozen_wire_bytes_and_output_reserve() {
        let mut value = input();
        // Final reclaim leaves just one retained token; this does not restrict an intermediate
        // response that will be merged/replaced by subsequent chronological synthesis steps.
        value.retained_without_summary.tokens = 69_999;
        let policy = ResolvedSummaryPolicy::resolve(value).unwrap();
        assert_eq!(policy.maximum_retained_tokens(), 1);
        policy.validate_response(&"x".repeat(8_000)).unwrap();
        policy.validate_response("").unwrap();
        assert!(policy.validate_response(&"x".repeat(64_001)).is_err());

        let mut value = input();
        value.maximum_response_bytes = 8;
        let policy = ResolvedSummaryPolicy::resolve(value).unwrap();
        policy.validate_response("😀😀").unwrap();
        // Three Unicode scalars are far below the token reserve, but exceed eight UTF-8 bytes.
        assert!(policy.validate_response("😀😀😀").is_err());
    }

    #[test]
    fn tiny_capacity_invalid_estimates_and_unbounded_bytes_fail_closed() {
        let mut value = input();
        value.context_window = 100;
        assert!(ResolvedSummaryPolicy::resolve(value).is_err());
        let mut value = input();
        value.before.algorithm_version += 1;
        assert!(ResolvedSummaryPolicy::resolve(value).is_err());
        let mut value = input();
        value.before.tokens = u64::MAX;
        assert!(ResolvedSummaryPolicy::resolve(value).is_err());
        let mut value = input();
        value.maximum_response_bytes = usize::MAX;
        assert!(ResolvedSummaryPolicy::resolve(value).is_err());
        let mut value = input();
        value.minimum_reclaim_tokens = value.before.tokens + 1;
        assert!(ResolvedSummaryPolicy::resolve(value).is_err());
    }

    #[test]
    fn preferred_target_is_not_the_retention_ceiling() {
        let policy = ResolvedSummaryPolicy::resolve(input()).unwrap();
        assert_eq!(policy.preferred_summary_tokens(), 1_500);
        assert_eq!(policy.maximum_retained_tokens(), 50_000);
        assert_eq!(policy.maximum_summary_bytes(), 65_536);
        policy.validate().unwrap();
    }

    #[test]
    fn actual_tail_and_reclaim_constrain_retention_independently_of_wire_output() {
        let mut value = input();
        value.retained_without_summary.tokens = 68_000;
        assert_eq!(
            ResolvedSummaryPolicy::resolve(value)
                .unwrap()
                .maximum_retained_tokens(),
            2_000
        );
        let mut value = input();
        value.provider_max_output_tokens = Some(3_000);
        assert_eq!(
            ResolvedSummaryPolicy::resolve(value)
                .unwrap()
                .maximum_retained_tokens(),
            50_000
        );
        let mut value = input();
        value.retained_without_summary.tokens = 70_000;
        assert!(ResolvedSummaryPolicy::resolve(value).is_err());
    }

    #[test]
    fn summaries_above_old_character_ceiling_are_accepted_when_actual_request_fits() {
        use super::super::build_compaction_request;
        let summary = "retain independent restriction; unfinished directive; ".repeat(180);
        assert!(summary.chars().count() > 6_000);
        let mut retained = build_compaction_request(
            crate::SessionId::new(),
            "gpt-6.1-sol".into(),
            bcode_model::ProviderRequestContext::default(),
            "",
            "budget-test".into(),
        );
        let mut value = input();
        value.retained_without_summary = local_request_estimate(&retained);
        let policy = ResolvedSummaryPolicy::resolve(value).unwrap();
        retained.messages[0].content = vec![super::super::ContentBlock::Text {
            text: summary.clone(),
        }];
        let response = serde_json::to_string(&summary).unwrap();
        assert!(
            policy
                .validate_candidate(&summary, &response, &retained)
                .is_ok()
        );
        assert!(
            policy
                .validate_candidate(
                    &summary,
                    &"x".repeat(policy.maximum_response_bytes() + 1),
                    &retained,
                )
                .is_err()
        );
        retained.messages[0].content = vec![super::super::ContentBlock::Text {
            text: "large retained overhead".repeat(30_000),
        }];
        assert!(
            policy
                .validate_candidate(&summary, &response, &retained)
                .is_err()
        );
    }

    #[test]
    fn unsafe_actual_generation_request_and_changed_model_are_rejected() {
        let policy = ResolvedSummaryPolicy::resolve(input()).unwrap();
        let mut request = super::super::build_compaction_request(
            crate::SessionId::new(),
            "gpt-6.1-sol".into(),
            bcode_model::ProviderRequestContext::default(),
            "source and carried scope",
            "budget-test".into(),
        );
        request.parameters.max_output_tokens = Some(16_000);
        policy.validate_request(&request).unwrap();
        request.provider_context.request.insert(
            "semantic_setting".into(),
            serde_json::json!("x".repeat(600_000)).into(),
        );
        assert!(policy.validate_request(&request).is_err());
        request.provider_context.request.clear();
        request.model_id = "other-model".into();
        assert!(policy.validate_request(&request).is_err());
    }

    #[test]
    fn unsupported_or_tampered_checkpoint_policy_fails_closed() {
        let policy = ResolvedSummaryPolicy::resolve(input()).unwrap();
        let json = serde_json::to_string(&policy).unwrap();
        let decoded: ResolvedSummaryPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(policy, decoded);
        let mut changed = decoded;
        changed.maximum_retained_tokens += 1;
        assert!(changed.validate().is_err());
        changed.version += 1;
        assert!(changed.validate().is_err());
    }
}
