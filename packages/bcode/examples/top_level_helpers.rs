use bcode::{
    AgentEvent, CancellationToken, ModelProviderInvoker, ObjectStreamItem, RuntimeFuture,
    TextStreamItem,
};
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

    let mut builder_provider = ExampleProvider::text("hello from the builder helper");
    let builder_response = bcode::generate_text_builder()
        .model("example-provider:example-model")
        .system("Use concise responses.")
        .metadata("example", "builder")
        .timeout(std::time::Duration::from_secs(30))
        .prompt("Say hello from the builder")
        .run(&mut builder_provider)
        .await?;
    println!("{}", builder_response.text);

    let mut stream = bcode::stream_text_builder()
        .model("example-provider:example-model")
        .metadata("example", "stream-builder")
        .prompt("Stream hello")
        .run(ExampleProvider::text("hello from the streaming builder"));
    while let Some(item) = stream.next().await {
        match item {
            TextStreamItem::Event(AgentEvent::TextDelta(text)) => print!("{text}"),
            TextStreamItem::Finished(response) => {
                println!("\nfinished: {:?}", response.runtime.stop_reason);
                break;
            }
            TextStreamItem::Error(error) => return Err(error),
            TextStreamItem::Event(_) | TextStreamItem::ScopedEvent(_) => {}
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
    let summary: Summary = bcode::generate_object_builder()
        .model("example-provider:example-model")
        .prompt("Return JSON")
        .run(&mut object_provider)
        .await?;
    println!("{}", summary.title);

    let mut streamed_object = bcode::stream_object::<Summary, _>(
        ExampleProvider::text(r#"{"title":"streamed object"}"#),
        "Stream JSON",
    );
    while let Some(item) = streamed_object.next().await {
        match item {
            ObjectStreamItem::RawDelta(delta) => print!("{delta}"),
            ObjectStreamItem::ValidatedPartial(value) => println!("\nvalid partial: {value}"),
            ObjectStreamItem::Finished { object, response } => {
                println!(
                    "\nstreamed object: {} stop={:?}",
                    object.title, response.runtime.stop_reason
                );
                break;
            }
            ObjectStreamItem::Error(error) => return Err(error),
            ObjectStreamItem::Partial(_)
            | ObjectStreamItem::Event(_)
            | ObjectStreamItem::ScopedEvent(_) => {}
        }
    }

    Ok(())
}
