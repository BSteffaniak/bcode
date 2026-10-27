//! Experimental token packing, explicitly enabled through resolved compaction configuration.
//! Production defaults retain byte-identical legacy chunks.

use super::{
    BTreeSet, COMPACTION_MAX_CARRIED_SUMMARY_CHARS, COMPACTION_MAX_CHUNKS,
    COMPACTION_MAX_SUMMARY_INPUT_CHARS, COMPACTION_SUMMARY_FORMAT, COMPACTION_SYSTEM_PROMPT,
    CompactionError, CompactionEvidence, ContentBlock, MessageRole, ModelMessage, ModelTurnRequest,
    compaction_capacity_tokens, local_request_estimate, synthesis,
};

use super::budget::ResolvedSummaryPolicy;

#[derive(Clone, Copy)]
pub(super) enum PackingGate {
    Disabled,
    Experimental,
}

/// Merge existing bounded chronological chunks only when the complete original AND repair
/// requests fit. No fragment is clipped or split, and order/coverage remains byte-exact.
/// A conservative carry placeholder reserves the full possible escaped carry for future steps.
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
    // Each UTF-8 input byte needs at most six encoded JSON bytes. Reserve actual serialized
    // characters, then use the canonical complete-request estimator rather than token arithmetic.
    let carry_reserve = "\\u0000".repeat(carry_bytes);
    let fits = |chunk: &str| {
        let limits = synthesis::SynthesisPolicy {
            carry_chars: carry_bytes,
            wire_bytes: policy.map_or(
                COMPACTION_MAX_CARRIED_SUMMARY_CHARS * 4,
                ResolvedSummaryPolicy::maximum_response_bytes,
            ),
            section_chars: carry_bytes,
        };
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
        // Bound the eligibility list by the canonical chunk bytes: each eligible source
        // must have a source record in that chunk. Reserve worst-case JSON escaping for
        // the bounded rejected response using the actual repair formatter.
        let prompt = format!(
            "Host reconciliation eligibility for this chunk (source sequences): {}. Only these unfinished sources may appear in reconciles.\n{}",
            "9".repeat(chunk.len()),
            prompt,
        );
        let rejected_bytes = policy.map_or(COMPACTION_MAX_CARRIED_SUMMARY_CHARS, |policy| {
            policy.maximum_response_bytes()
        });
        // Independently reserve the worst diagnostic escaping even if the formatter elects not
        // to attach this placeholder. Actual attachment policy may be stricter, never looser.
        let rejected_diagnostic = serde_json::to_string(&"\0".repeat(rejected_bytes))
            .expect("serializing a string cannot fail");
        let repair = synthesis::repair_prompt_with_policy(
            &prompt,
            synthesis::Rejection::InvalidSupersession,
            None,
            limits,
        );
        [prompt, format!("{repair}\n{rejected_diagnostic}")]
            .iter()
            .all(|prompt| {
                let mut request = template.clone();
                request.system_prompt = Some(COMPACTION_SYSTEM_PROMPT.into());
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
        use super::super::budget::SummaryBudgetInput;
        let mut before = local_request_estimate(request);
        before.tokens += 80_000;
        ResolvedSummaryPolicy::resolve(SummaryBudgetInput {
            provider_id: "test-provider".into(),
            model_id: request.model_id.clone(),
            context_window: 128_000,
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
    fn frozen_carry_byte_reserve_is_not_the_old_character_constant() {
        let request = template();
        let source = vec!["must preserve this indivisible source\n".into()];
        let small = resolved_policy(&request, 8_192);
        let large = resolved_policy(&request, 262_144);
        assert!(pack_with_policy(PackingGate::Experimental, &source, &request, &small).is_ok());
        assert!(pack_with_policy(PackingGate::Experimental, &source, &request, &large).is_err());
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
