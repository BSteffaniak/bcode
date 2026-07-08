use bcode::{Agent, ModelProviderInvoker, RuntimeFuture};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse, StopReason,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::collections::VecDeque;

#[derive(Debug, Deserialize, JsonSchema)]
struct ReviewSummary {
    risk: String,
    findings: Vec<String>,
}

struct JsonProvider {
    events: VecDeque<ProviderTurnEvent>,
}

impl JsonProvider {
    fn new() -> Self {
        Self {
            events: VecDeque::from([
                ProviderTurnEvent::TextDelta {
                    text: r#"{"risk":"low","findings":["No blocking issues found"]}"#.to_string(),
                },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::EndTurn,
                },
            ]),
        }
    }
}

impl ModelProviderInvoker for JsonProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        Box::pin(async move {
            assert!(request.structured_output.is_some());
            Ok(StartTurnResponse {
                provider_turn_id: "structured-turn".to_string(),
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
    let agent = Agent::builder().model("json-provider").build();
    let mut provider = JsonProvider::new();
    let summary: ReviewSummary = agent
        .generate_object_with_provider(&mut provider, "Review this patch")
        .await?;

    println!("risk: {}", summary.risk);
    println!("findings: {}", summary.findings.join("; "));
    Ok(())
}
