//! Experimental token packing, explicitly enabled through resolved compaction configuration.
//! Production defaults retain byte-identical legacy chunks.

use super::{
    COMPACTION_MAX_CARRIED_SUMMARY_CHARS, COMPACTION_MAX_CHUNKS,
    COMPACTION_MAX_SUMMARY_INPUT_CHARS, COMPACTION_SYSTEM_PROMPT, CompactionError, ContentBlock,
    MessageRole, ModelMessage, ModelTurnRequest, compaction_capacity_tokens,
    local_request_estimate, synthesis, synthesis_prompt,
};

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
    // Synthesis accepts at most this many encoded characters. Six-byte JSON escaping is an
    // intentionally conservative reserve, not a second context-token accounting algorithm.
    let carry_reserve = "\\u0000".repeat(COMPACTION_MAX_CARRIED_SUMMARY_CHARS);
    let fits = |chunk: &str| {
        let prompt = synthesis_prompt(
            COMPACTION_MAX_CHUNKS - 1,
            COMPACTION_MAX_CHUNKS,
            &carry_reserve,
            chunk,
        );
        // Bound the eligibility list by the canonical chunk bytes: each eligible source
        // must have a source record in that chunk. Reserve worst-case JSON escaping for
        // the bounded rejected response using the actual repair formatter.
        let prompt = format!(
            "Host reconciliation eligibility for this chunk (source sequences): {}. Only these unfinished sources may appear in reconciles.\n{}",
            "9".repeat(chunk.len()),
            prompt,
        );
        let rejected = "\0".repeat(COMPACTION_MAX_CARRIED_SUMMARY_CHARS);
        [
            prompt.clone(),
            synthesis::repair_prompt(
                &prompt,
                synthesis::Rejection::InvalidSupersession,
                Some(&rejected),
            ),
        ]
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
            local_request_estimate(&request).tokens <= capacity.available_input_tokens
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
