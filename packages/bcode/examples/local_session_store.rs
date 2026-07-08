use bcode::{Agent, LocalSessionStore, ModelProviderInvoker, RuntimeFuture};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse, StopReason,
};
use std::collections::VecDeque;
use std::path::PathBuf;

struct EchoProvider {
    turns: VecDeque<String>,
}

impl EchoProvider {
    fn new(turns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            turns: turns.into_iter().map(Into::into).collect(),
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
            println!("provider received {} messages", request.messages.len());
            Ok(StartTurnResponse {
                provider_turn_id: "local-session-turn".to_string(),
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

fn store_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "bcode-local-session-example-{}.json",
        std::process::id()
    ))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> bcode::Result<()> {
    let path = store_path();
    let _ = std::fs::remove_file(&path);
    let store = LocalSessionStore::new(&path);

    let agent = Agent::builder().model("echo-provider").build();
    let mut session = agent.session_with_store(store.clone())?;
    let mut provider = EchoProvider::new(["stored response"]);
    session
        .generate_text_with_provider(&mut provider, "persist this turn")
        .await?;

    let reloaded = Agent::builder()
        .model("echo-provider")
        .build()
        .session_with_store(store)?;

    println!("reloaded messages: {}", reloaded.session().messages().len());
    let _ = std::fs::remove_file(path);
    Ok(())
}
