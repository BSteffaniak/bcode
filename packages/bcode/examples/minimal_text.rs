use bcode::{Agent, AgentEvent, AgentStreamItem};
use bcode_agent_runtime::{ModelProviderInvoker, RuntimeFuture};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse, StopReason,
};
use std::collections::VecDeque;

struct ExampleProvider {
    events: VecDeque<ProviderTurnEvent>,
}

impl ExampleProvider {
    fn new() -> Self {
        Self {
            events: VecDeque::from([
                ProviderTurnEvent::TextDelta {
                    text: "hello".to_string(),
                },
                ProviderTurnEvent::TextDelta {
                    text: " from Bcode".to_string(),
                },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::EndTurn,
                },
            ]),
        }
    }
}

impl ModelProviderInvoker for ExampleProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        Box::pin(async {
            Ok(StartTurnResponse {
                provider_turn_id: "example-turn".to_string(),
            })
        })
    }

    fn poll_turn_events<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a PollTurnEventsRequest,
    ) -> RuntimeFuture<'a, PollTurnEventsResponse> {
        Box::pin(async move {
            Ok(PollTurnEventsResponse {
                events: self.events.pop_front().into_iter().collect(),
            })
        })
    }

    fn cancel_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a CancelTurnRequest,
    ) -> RuntimeFuture<'a, AckResponse> {
        Box::pin(async { Ok(AckResponse::default()) })
    }

    fn finish_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a FinishTurnRequest,
    ) -> RuntimeFuture<'a, AckResponse> {
        Box::pin(async { Ok(AckResponse::default()) })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> bcode::Result<()> {
    let agent = Agent::builder()
        .name("example")
        .model("example-model")
        .build();

    let mut provider = ExampleProvider::new();
    let response = agent
        .generate_text_with_provider(&mut provider, "Say hello")
        .await?;
    println!("{}", response.text);

    let mut stream = agent.stream_text_with_provider(ExampleProvider::new(), "Say hello again");
    while let Some(item) = stream.next().await {
        match item {
            AgentStreamItem::Event(AgentEvent::TextDelta(text)) => print!("{text}"),
            AgentStreamItem::Finished(response) => {
                println!("\nfinished: {:?}", response.stop_reason);
                break;
            }
            AgentStreamItem::Error(error) => return Err(error.into()),
            AgentStreamItem::Event(_) => {}
        }
    }

    Ok(())
}
