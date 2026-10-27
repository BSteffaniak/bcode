//! Experimental token packing, explicitly enabled through resolved compaction configuration.
//! Production defaults retain byte-identical legacy chunks.

use super::{
    BTreeSet, COMPACTION_MAX_CARRIED_SUMMARY_CHARS, COMPACTION_MAX_CHUNKS,
    COMPACTION_MAX_SUMMARY_INPUT_CHARS, COMPACTION_SUMMARY_FORMAT, CompactionError,
    CompactionEvidence, ContentBlock, MessageRole, ModelMessage, ModelTurnRequest,
    compaction_capacity_tokens, local_request_estimate, synthesis,
};

use super::budget::ResolvedSummaryPolicy;

/// Persisted packing identity. Changing request-fit semantics requires explicit supersession.
pub(super) const ALGORITHM_ID: &str = "adaptive-complete-request-v2";

#[derive(Clone, Copy)]
pub(super) enum PackingGate {
    Disabled,
    Experimental,
}

/// Merge existing bounded chronological chunks only when the complete original AND repair
/// requests fit. No fragment is clipped or split, and order/coverage remains byte-exact.
/// Carry reservation is token-guided planning headroom, never a substitute for actual validation.
pub(super) fn pack(
    gate: PackingGate,
    source: &[String],
    template: &ModelTurnRequest,
    context_window: u32,
    provider_max_output_tokens: Option<u32>,
) -> Result<Vec<String>, CompactionError> {
    if matches!(gate, PackingGate::Disabled) {
        return Ok(source.to_vec());
    }
    if source.len() > COMPACTION_MAX_CHUNKS
        || source
            .iter()
            .any(|chunk| chunk.len() > COMPACTION_MAX_SUMMARY_INPUT_CHARS)
    {
        return Err(CompactionError::Provider(
            "experimental packing input exceeds bounded source budget".into(),
        ));
    }
    let capacity = compaction_capacity_tokens(
        context_window,
        bcode_config::ProactiveCompactionThreshold::Percent(100),
        template.parameters.max_output_tokens,
        provider_max_output_tokens,
    );
    pack_bounded(
        gate,
        source,
        template,
        capacity.available_input_tokens,
        COMPACTION_MAX_CARRIED_SUMMARY_CHARS,
        None,
    )
}

/// Capacity-aware packing uses the frozen policy for carry and actual complete-request checks.
/// Disabled packing still preserves the original chronological chunks byte-for-byte. The parent
/// must validate every actual request, even when packing is disabled.
pub(super) fn pack_with_policy(
    gate: PackingGate,
    source: &[String],
    template: &ModelTurnRequest,
    policy: &ResolvedSummaryPolicy,
) -> Result<Vec<String>, CompactionError> {
    policy.validate()?;
    if matches!(gate, PackingGate::Disabled) {
        return Ok(source.to_vec());
    }
    pack_bounded(
        gate,
        source,
        template,
        policy.available_input_tokens(),
        policy.maximum_summary_bytes(),
        Some(policy),
    )
}

fn packing_carry_reserve(
    template: &ModelTurnRequest,
    available_input_tokens: u64,
    carry_bytes: usize,
    policy: Option<&ResolvedSummaryPolicy>,
) -> String {
    let carry_tokens = policy.map_or(available_input_tokens / 4, |policy| {
        policy
            .maximum_retained_tokens()
            .min(available_input_tokens / 4)
    });
    let mut low = 0;
    let mut high = carry_bytes;
    let baseline = local_request_estimate(template).tokens;
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        let mut request = template.clone();
        request.messages = vec![ModelMessage {
            role: MessageRole::User,
            content: vec![ContentBlock::Text {
                text: "x".repeat(middle),
            }],
        }];
        if local_request_estimate(&request)
            .tokens
            .saturating_sub(baseline)
            <= carry_tokens
        {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    "x".repeat(low)
}

fn pack_bounded(
    gate: PackingGate,
    source: &[String],
    template: &ModelTurnRequest,
    available_input_tokens: u64,
    carry_bytes: usize,
    policy: Option<&ResolvedSummaryPolicy>,
) -> Result<Vec<String>, CompactionError> {
    if matches!(gate, PackingGate::Disabled) {
        return Ok(source.to_vec());
    }
    if source.len() > COMPACTION_MAX_CHUNKS
        || source
            .iter()
            .any(|chunk| chunk.len() > COMPACTION_MAX_SUMMARY_INPUT_CHARS)
    {
        return Err(CompactionError::Provider(
            "experimental packing input exceeds bounded source budget".into(),
        ));
    }
    // Allocation guards are not retention targets. Reserve only feasible token headroom for
    // carried synthesis, measured through the same complete semantic request estimator. Actual
    // carried evidence is never replaced by this placeholder or truncated to make it fit.
    let carry_reserve =
        packing_carry_reserve(template, available_input_tokens, carry_bytes, policy);
    let limits = synthesis::SynthesisPolicy {
        carry_chars: carry_bytes,
        wire_bytes: policy.map_or(
            COMPACTION_MAX_CARRIED_SUMMARY_CHARS * 4,
            ResolvedSummaryPolicy::maximum_response_bytes,
        ),
        section_chars: carry_bytes,
    };
    let fits = |chunk: &str| {
        let evidence = CompactionEvidence {
            format: COMPACTION_SUMMARY_FORMAT.into(),
            current_scope: Vec::new(),
            continuation: carry_reserve.clone(),
        };
        let Ok(prompt) = synthesis::section_update_prompt(
            &evidence,
            chunk,
            &BTreeSet::new(),
            &BTreeSet::new(),
            limits,
        ) else {
            return false;
        };
        // Eligibility overhead is bounded by the selected source bytes. The optional rejected
        // response is NOT canonical evidence: the repair builder omits it whole when it cannot
        // fit. Packing must reserve the targeted reason + complete original request, not the
        // independent maximum wire allocation (which may exceed the model's entire window).
        let prompt = format!(
            "Host reconciliation eligibility for this chunk (source sequences): {}. Only these unfinished sources may appear in reconciles.\n{}",
            "9".repeat(chunk.len()),
            prompt,
        );
        let repair = synthesis::repair_prompt_with_policy(
            &prompt,
            synthesis::Rejection::InvalidSupersession,
            None,
            limits,
        );
        [prompt, repair].iter().all(|prompt| {
            let mut request = template.clone();
            request.messages = vec![ModelMessage {
                role: MessageRole::User,
                content: vec![ContentBlock::Text {
                    text: prompt.clone(),
                }],
            }];
            policy.map_or_else(
                || local_request_estimate(&request).tokens <= available_input_tokens,
                |policy| policy.validate_request(&request).is_ok(),
            )
        })
    };
    let mut packed = Vec::new();
    let mut pending = String::new();
    for chunk in source {
        let mut candidate = pending.clone();
        candidate.push_str(chunk);
        if fits(&candidate) {
            pending = candidate;
        } else {
            if !pending.is_empty() {
                packed.push(std::mem::take(&mut pending));
            }
            if !fits(chunk) {
                return Err(CompactionError::Provider("indivisible evidence chunk exceeds experimental request capacity; coverage unchanged".into()));
            }
            pending.clone_from(chunk);
        }
    }
    if !pending.is_empty() {
        packed.push(pending);
    }
    Ok(packed)
}

#[cfg(test)]
mod tests {
    use super::super::build_compaction_request;
    use super::*;
    use crate::SessionId;

    fn template() -> ModelTurnRequest {
        build_compaction_request(
            SessionId::new(),
            "test-model".into(),
            bcode_model::ProviderRequestContext::default(),
            "",
            "test-turn".into(),
        )
    }

    fn resolved_policy(request: &ModelTurnRequest, summary_bytes: usize) -> ResolvedSummaryPolicy {
        policy_for_window(request, summary_bytes, 128_000)
    }

    fn policy_for_window(
        request: &ModelTurnRequest,
        summary_bytes: usize,
        window: u32,
    ) -> ResolvedSummaryPolicy {
        use super::super::budget::SummaryBudgetInput;
        let mut before = local_request_estimate(request);
        before.tokens += 80_000;
        ResolvedSummaryPolicy::resolve(SummaryBudgetInput {
            provider_id: "test-provider".into(),
            model_id: request.model_id.clone(),
            context_window: window,
            requested_max_output_tokens: request.parameters.max_output_tokens,
            retained_requested_max_output_tokens: request.parameters.max_output_tokens,
            provider_max_output_tokens: None,
            before,
            retained_without_summary: local_request_estimate(request),
            minimum_reclaim_tokens: 10_000,
            preferred_summary_tokens: 1_500,
            maximum_summary_bytes: summary_bytes,
            maximum_response_bytes: summary_bytes * 2,
        })
        .unwrap()
    }

    #[test]
    fn current_large_guards_do_not_make_64k_unicode_sources_impossible() {
        let request = template();
        let policy = policy_for_window(&request, 65_536, 64_000);
        let source = vec!["漢字🙂e\u{301} authoritative source and final correction\n".repeat(80)];
        let packed =
            pack_with_policy(PackingGate::Experimental, &source, &request, &policy).unwrap();
        assert_eq!(packed.concat(), source.concat());
        assert_eq!(packed, source);
    }

    #[test]
    fn repair_omits_whole_escaped_diagnostic_but_keeps_source_and_reason() {
        let mut request = template();
        let policy = policy_for_window(&request, 65_536, 64_000);
        let limits = synthesis::SynthesisPolicy {
            carry_chars: 65_536,
            section_chars: 65_536,
            wire_bytes: 131_072,
        };
        let base = "Complete canonical directive 漢字🙂; never remove the final correction.";
        let rejected = format!("{}漢字🙂", "\0".repeat(100_000));
        assert!(rejected.len() <= limits.wire_bytes);
        let attached = synthesis::repair_prompt_with_policy(
            base,
            synthesis::Rejection::InvalidSupersession,
            Some(&rejected),
            limits,
        );
        request.messages = vec![ModelMessage {
            role: MessageRole::User,
            content: vec![ContentBlock::Text { text: attached }],
        }];
        assert!(policy.validate_request(&request).is_err());
        let prompt = synthesis::capacity_checked_repair_prompt(
            base,
            synthesis::Rejection::InvalidSupersession,
            Some(&rejected),
            limits,
            &request,
            &policy,
        )
        .unwrap();
        assert_eq!(
            prompt,
            synthesis::repair_prompt_with_policy(
                base,
                synthesis::Rejection::InvalidSupersession,
                None,
                limits
            )
        );
        assert!(prompt.ends_with(base));
        let short = "small rejected 漢字🙂\\\" output";
        let prompt = synthesis::capacity_checked_repair_prompt(
            base,
            synthesis::Rejection::InvalidSupersession,
            Some(short),
            limits,
            &request,
            &policy,
        )
        .unwrap();
        assert_eq!(
            prompt,
            synthesis::repair_prompt_with_policy(
                base,
                synthesis::Rejection::InvalidSupersession,
                Some(short),
                limits
            )
        );
    }

    #[test]
    fn capacity_checked_repair_rejects_oversized_base_without_clipping() {
        let request = template();
        let policy = policy_for_window(&request, 65_536, 8_000);
        let limits = synthesis::SynthesisPolicy {
            carry_chars: 65_536,
            section_chars: 65_536,
            wire_bytes: 131_072,
        };
        let base = "canonical user restriction 漢字🙂\n".repeat(8_000);
        assert!(
            synthesis::capacity_checked_repair_prompt(
                &base,
                synthesis::Rejection::InvalidSupersession,
                None,
                limits,
                &request,
                &policy
            )
            .is_err()
        );
        assert_eq!(base.lines().count(), 8_000);
    }

    #[test]
    fn resolved_policy_packing_preserves_chronology_and_complete_coverage() {
        let request = template();
        let policy = resolved_policy(&request, 8_192);
        let source = vec![
            "first canonical directive\n".into(),
            "independent restriction\n".into(),
            "legitimate correction\n".into(),
        ];
        let packed =
            pack_with_policy(PackingGate::Experimental, &source, &request, &policy).unwrap();
        assert_eq!(packed.concat(), source.concat());
        assert!(packed.len() < source.len());
        assert_eq!(
            pack_with_policy(PackingGate::Disabled, &source, &request, &policy).unwrap(),
            source
        );
    }

    #[test]
    fn allocation_guards_do_not_make_feasible_source_unpackable() {
        let request = template();
        let source = vec!["must preserve this indivisible source\n".into()];
        for bytes in [8_192, 262_144] {
            let policy = resolved_policy(&request, bytes);
            let packed =
                pack_with_policy(PackingGate::Experimental, &source, &request, &policy).unwrap();
            assert_eq!(packed.concat(), source.concat());
        }
    }

    #[test]
    fn disabled_gate_is_exact_legacy_packing_even_for_tiny_models() {
        let source = vec!["first\n".into(), "second\n".into()];
        assert_eq!(
            pack(PackingGate::Disabled, &source, &template(), 1, None).unwrap(),
            source
        );
    }

    #[test]
    fn experimental_merging_preserves_every_source_byte_and_order() {
        let source = vec![
            "漢字🙂e\u{301}".repeat(1000),
            "second source\n".repeat(300),
            "final corrected directive".into(),
        ];
        let packed = pack(
            PackingGate::Experimental,
            &source,
            &template(),
            128_000,
            Some(4096),
        )
        .unwrap();
        assert!(packed.len() < source.len());
        assert_eq!(packed.concat(), source.concat());
        assert!(packed.iter().all(|chunk| !chunk.is_empty()));
    }

    #[test]
    fn output_reserve_and_request_settings_are_not_ignored() {
        let source = vec!["evidence\n".repeat(100)];
        let mut request = template();
        assert!(pack(PackingGate::Experimental, &source, &request, 128_000, None).is_ok());
        request.parameters.max_output_tokens = Some(128_000);
        assert!(pack(PackingGate::Experimental, &source, &request, 128_000, None).is_err());
        request.parameters.max_output_tokens = Some(4096);
        request.provider_context.request.insert(
            "large_request_setting".into(),
            serde_json::json!("x".repeat(600_000)).into(),
        );
        assert!(pack(PackingGate::Experimental, &source, &request, 128_000, None).is_err());
    }

    #[test]
    fn tiny_capacity_fails_without_clipping_and_input_caps_remain_bounded() {
        let source = vec!["directive must retain final correction\n".into()];
        assert!(pack(PackingGate::Experimental, &source, &template(), 100, None).is_err());
        assert!(
            pack(
                PackingGate::Experimental,
                &vec![String::new(); COMPACTION_MAX_CHUNKS + 1],
                &template(),
                128_000,
                None
            )
            .is_err()
        );
        assert!(
            pack(
                PackingGate::Experimental,
                &["x".repeat(COMPACTION_MAX_SUMMARY_INPUT_CHARS + 1)],
                &template(),
                128_000,
                None
            )
            .is_err()
        );
        assert!(
            pack(PackingGate::Experimental, &[], &template(), 128_000, None)
                .unwrap()
                .is_empty()
        );
    }
}
