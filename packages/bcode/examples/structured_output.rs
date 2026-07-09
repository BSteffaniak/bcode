use bcode::{Agent, ModelProviderInvoker, RuntimeFuture, StructuredOutputOptions};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse, StopReason,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use std::collections::VecDeque;

#[derive(Debug, Deserialize, JsonSchema)]
struct ReviewSummary {
    risk: String,
    findings: Vec<String>,
}

struct JsonProvider {
    outputs: VecDeque<String>,
    events: VecDeque<ProviderTurnEvent>,
}

impl JsonProvider {
    fn with_outputs(outputs: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            outputs: outputs.into_iter().map(Into::into).collect(),
            events: VecDeque::new(),
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
            let text = self.outputs.pop_front().unwrap_or_default();
            self.events = VecDeque::from([
                ProviderTurnEvent::TextDelta { text },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: StopReason::EndTurn,
                },
            ]);
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

    let mut typed_provider =
        JsonProvider::with_outputs([r#"{"risk":"low","findings":["No blocking issues found"]}"#]);
    let summary: ReviewSummary = agent
        .generate_object_with_provider(&mut typed_provider, "Review this patch")
        .await?;
    println!("risk: {}", summary.risk);
    println!("findings: {}", summary.findings.join("; "));

    let schema = json!({
        "type": "object",
        "properties": {
            "risk": { "type": "string" },
            "findings": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["risk", "findings"]
    });
    let options = StructuredOutputOptions::json_schema("ReviewSummary", schema.clone());
    let mut explicit_provider =
        JsonProvider::with_outputs([r#"{"risk":"medium","findings":["Needs a second look"]}"#]);
    let explicit: ReviewSummary = agent
        .generate_object_with_provider_and_options(
            &mut explicit_provider,
            "Review with an explicit schema",
            options,
        )
        .await?;
    println!("explicit risk: {}", explicit.risk);

    let repair_options =
        StructuredOutputOptions::json_schema("ReviewSummary", schema).with_max_repairs(1);
    let mut repair_provider = JsonProvider::with_outputs([
        r#"{"risk":"high"}"#,
        r#"{"risk":"low","findings":["Repaired after schema feedback"]}"#,
    ]);
    let repaired: ReviewSummary = agent
        .generate_object_with_provider_and_options(
            &mut repair_provider,
            "Review and repair if needed",
            repair_options,
        )
        .await?;
    println!("repaired finding: {}", repaired.findings.join("; "));

    Ok(())
}
