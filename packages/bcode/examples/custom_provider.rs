use bcode::{
    Agent, InProcessModelProvider, InProcessModelProviderAdapter, InProcessProviderContext,
    InProcessProviderFuture, InProcessProviderOutcome, ModelTurnRequest, ProviderTurnEvent,
    TokenUsage,
};

#[derive(Debug)]
struct CustomProvider;

impl InProcessModelProvider for CustomProvider {
    fn run_turn(
        &self,
        request: ModelTurnRequest,
        context: InProcessProviderContext,
    ) -> InProcessProviderFuture<'_> {
        Box::pin(async move {
            let prompt = request
                .messages
                .last()
                .and_then(|message| message.content.first())
                .and_then(|block| match block {
                    bcode::ModelContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or_default();
            context
                .events()
                .emit(ProviderTurnEvent::TextDelta {
                    text: format!("custom provider received: {prompt}"),
                })
                .expect("turn is active");
            context
                .events()
                .emit(ProviderTurnEvent::Usage {
                    usage: TokenUsage {
                        input_tokens: Some(1),
                        output_tokens: Some(4),
                        total_tokens: Some(5),
                        ..TokenUsage::default()
                    },
                })
                .expect("turn is active");
            Ok(InProcessProviderOutcome::EndTurn)
        })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> bcode::Result<()> {
    let agent = Agent::builder().model("custom-model").build();
    let mut provider = InProcessModelProviderAdapter::new(CustomProvider);
    let response = agent.run(&mut provider, "hello").await?;
    println!("{}", response.text);
    Ok(())
}
