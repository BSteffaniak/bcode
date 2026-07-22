use bcode::{Agent, ProviderTurnEvent, TokenUsage, testing::*};
use std::time::Duration;

#[tokio::main(flavor = "current_thread")]
async fn main() -> bcode::Result<()> {
    let provider = ScriptedProvider::new([ScriptedProviderTurn::new()
        .events([
            ProviderTurnEvent::TurnStarted,
            ProviderTurnEvent::Warning {
                message: "deterministic warning".to_string(),
            },
            ProviderTurnEvent::Usage {
                usage: TokenUsage {
                    input_tokens: Some(2),
                    output_tokens: Some(1),
                    total_tokens: Some(3),
                    ..TokenUsage::default()
                },
            },
        ])
        .delay(Duration::from_millis(1))
        .events([
            ProviderTurnEvent::TextDelta {
                text: "scripted answer".to_string(),
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: bcode::StopReason::EndTurn,
            },
        ])]);
    let probe = provider.probe();
    let agent = Agent::builder()
        .provider_plugin("test-provider")
        .model("test-model")
        .build();

    let response = agent.run(&mut provider.clone(), "hello").await?;
    assert_eq!(response.text, "scripted answer");
    probe
        .assert_requests(&[ScriptedRequestExpectation::new()
            .provider_plugin_id("test-provider")
            .model_id("test-model")])
        .expect("captured request");
    probe.assert_finish_count(1).expect("provider cleanup");
    Ok(())
}
