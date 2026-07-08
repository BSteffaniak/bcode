use bcode::{Agent, ModelProviderInvoker, RuntimeFuture};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse, StopReason,
};
use std::collections::VecDeque;

struct EchoProvider {
    turns: VecDeque<String>,
}

impl EchoProvider {
    fn new() -> Self {
        Self {
            turns: VecDeque::from(["first response".to_string(), "second response".to_string()]),
        }
    }
}

impl ModelProviderInvoker for EchoProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        Box::pin(async move {
            println!(
                "provider received {} prior messages",
                request.messages.len() - 1
            );
            Ok(StartTurnResponse {
                provider_turn_id: "session-turn".to_string(),
            })
        })
    }

    fn poll_turn_events<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a PollTurnEventsRequest,
    ) -> RuntimeFuture<'a, PollTurnEventsResponse> {
        Box::pin(async move {
            let text = self.turns.pop_front().unwrap_or_default();
            Ok(PollTurnEventsResponse {
                events: vec![
                    ProviderTurnEvent::TextDelta { text },
                    ProviderTurnEvent::TurnFinished {
                        stop_reason: StopReason::EndTurn,
                    },
                ],
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
    let agent = Agent::builder().model("echo-provider").build();
    let mut session = agent.session();
    let mut provider = EchoProvider::new();

    let first = session
        .generate_text_with_provider(&mut provider, "remember apples")
        .await?;
    let second = session
        .generate_text_with_provider(&mut provider, "what did I ask you to remember?")
        .await?;

    println!("first: {}", first.text);
    println!("second: {}", second.text);
    println!(
        "persistable messages: {}",
        session.session().messages().len()
    );
    Ok(())
}
