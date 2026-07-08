use bcode::{AgentEvent, AgentStreamItem, CancellationToken, ModelProviderInvoker, RuntimeFuture};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse, StopReason,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::collections::VecDeque;

#[derive(Debug, Deserialize, JsonSchema)]
struct Summary {
    title: String,
}

struct ExampleProvider {
    events: VecDeque<ProviderTurnEvent>,
}

impl ExampleProvider {
    fn text(text: impl Into<String>) -> Self {
        Self {
            events: VecDeque::from([
                ProviderTurnEvent::TextDelta { text: text.into() },
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
    let mut text_provider = ExampleProvider::text("hello from the top-level helper");
    let response = bcode::generate_text(&mut text_provider, "Say hello").await?;
    println!("{}", response.text);

    let mut selected_provider = ExampleProvider::text("hello from the selected helper");
    let selected = bcode::generate_text_with_model(
        &mut selected_provider,
        "example-provider:example-model",
        "Say hello with an explicit selector",
    )
    .await?;
    println!("{}", selected.text);

    let mut stream = bcode::stream_text(
        ExampleProvider::text("hello from the streaming helper"),
        "Stream hello",
    );
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

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let mut cancelled_provider = ExampleProvider::text("this should not be generated");
    if let Err(error) = bcode::generate_text_with_cancellation(
        &mut cancelled_provider,
        "Cancel before generation",
        cancellation,
    )
    .await
    {
        println!("cancelled: {error}");
    }

    let mut object_provider = ExampleProvider::text(r#"{"title":"top-level object"}"#);
    let summary: Summary = bcode::generate_object(&mut object_provider, "Return JSON").await?;
    println!("{}", summary.title);

    Ok(())
}
