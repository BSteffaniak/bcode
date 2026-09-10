//! Live/offline decoder parity across serving dialects.
use super::*;
use std::collections::BTreeMap;

#[test]
fn reports_keep_the_same_semantics_in_each_serving_dialect() {
    for dialect in [
        ResponsesUsageDialect::OpenAi,
        ResponsesUsageDialect::Bedrock,
        ResponsesUsageDialect::ChatCompletions,
    ] {
        let source = dialect.capture_spec().complete_sources[0];
        let mut original=OriginalUsage {provider_id:"test".into(),api_shape:dialect.capture_spec().api_shape.into(),requested:BTreeMap::from([("model".into(),"global.model".into()),("service_tier".into(),"priority".into()),("prompt_cache_retention".into(),"1h".into())]),complete:true,
            reports:vec![bcode_session_models::OriginalUsageReport {source:source.into(),confirmed:BTreeMap::from([("service_tier".into(),"flex".into())]),usage_json:r#"{"input_tokens":100,"output_tokens":5,"input_tokens_details":{"cached_tokens":20,"cache_creation_tokens":30},"output_tokens_details":{"reasoning_tokens":2},"unknown":123456789012345678901234567890}"#.into()}],..Default::default()};
        let live = dialect.observe(None, &original).unwrap();
        assert_eq!(dialect.normalize(&original).unwrap(), live);
        assert_eq!(live.input_tokens, Some(100));
        assert_eq!(live.uncached_input_tokens(), Some(50));
        assert_eq!(live.reasoning_tokens, Some(2));
        assert_eq!(live.pricing_context.service_tier.as_deref(), Some("flex"));
        assert_eq!(original.requested["service_tier"], "priority");
        original.reports[0].usage_json = r#"{"input_tokens":4294967296}"#.into();
        assert!(dialect.normalize(&original).is_err());
    }
}

#[test]
fn mantle_does_not_replace_audio_evidence_with_text() {
    let original=OriginalUsage {api_shape:"responses".into(),complete:true,reports:vec![bcode_session_models::OriginalUsageReport {source:"response.completed".into(),confirmed:BTreeMap::new(),usage_json:r#"{"input_tokens":100,"output_tokens":5,"input_tokens_details":{"cached_tokens":0,"audio_tokens":40}}"#.into()}],..Default::default()};
    let usage = ResponsesUsageDialect::Bedrock.normalize(&original).unwrap();
    assert!(usage.details.iter().any(|detail| detail.modality
        == bcode_model::ModelTokenModality::Audio
        && detail.tokens == 40));
}
