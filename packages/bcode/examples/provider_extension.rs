#[cfg(feature = "openai-compatible-provider")]
use bcode::{
    Agent,
    openai::{
        OpenAiPromptCacheRetention, OpenAiResponsesRequestOptions, OpenAiServiceTier,
        OpenAiTruncation,
    },
};

#[cfg(feature = "openai-compatible-provider")]
fn main() -> bcode::Result<()> {
    let agent = Agent::builder()
        .model("gpt-5")
        .provider_extension(&OpenAiResponsesRequestOptions {
            service_tier: Some(OpenAiServiceTier::Priority),
            truncation: Some(OpenAiTruncation::Disabled),
            safety_identifier: Some("stable-user-id".to_string()),
            prompt_cache_retention: Some(OpenAiPromptCacheRetention::TwentyFourHours),
        })?
        .build();

    let options = agent
        .provider_extension::<OpenAiResponsesRequestOptions>()?
        .expect("configured extension");
    assert_eq!(options.service_tier, Some(OpenAiServiceTier::Priority));
    Ok(())
}

#[cfg(not(feature = "openai-compatible-provider"))]
fn main() {
    eprintln!("enable the openai-compatible-provider feature");
}
