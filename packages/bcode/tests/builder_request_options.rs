use bcode::{
    AgentStreamItem, CancellationToken, MessageRole, ModelContentBlock, ModelMessage,
    ModelProviderInvoker, ObjectStreamItem, RuntimeFuture, StopReason, generate_object_builder,
    stream_object_builder, stream_text_builder,
};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelParameters, ModelTurnRequest,
    PollTurnEventsRequest, PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Deserialize, JsonSchema)]
struct Output {
    value: String,
}

#[derive(Debug)]
struct RecordingProvider {
    requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
    output: String,
    emitted: bool,
}

impl RecordingProvider {
    fn new(requests: Arc<Mutex<Vec<ModelTurnRequest>>>, output: impl Into<String>) -> Self {
        Self {
            requests,
            output: output.into(),
            emitted: false,
        }
    }
}

impl ModelProviderInvoker for RecordingProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        self.requests
            .lock()
            .expect("requests lock should be available")
            .push(request.clone());
        Box::pin(async {
            Ok(StartTurnResponse {
                provider_turn_id: "builder-options".to_string(),
            })
        })
    }

    fn poll_turn_events<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a PollTurnEventsRequest,
    ) -> RuntimeFuture<'a, PollTurnEventsResponse> {
        Box::pin(async move {
            let events = if self.emitted {
                Vec::new()
            } else {
                self.emitted = true;
                vec![
                    ProviderTurnEvent::TextDelta {
                        text: self.output.clone(),
                    },
                    ProviderTurnEvent::TurnFinished {
                        stop_reason: StopReason::EndTurn,
                    },
                ]
            };
            Ok(PollTurnEventsResponse { events })
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

fn history() -> ModelMessage {
    ModelMessage {
        role: MessageRole::Assistant,
        content: vec![ModelContentBlock::Text {
            text: "prior".to_string(),
        }],
    }
}

fn assert_request(request: &ModelTurnRequest) {
    assert_eq!(request.model_id, "model");
    assert_eq!(request.system_prompt.as_deref(), Some("system"));
    assert_eq!(request.messages.len(), 2);
    assert_eq!(request.parameters.temperature, Some(0.25));
    assert_eq!(
        request.metadata.get("trace").map(String::as_str),
        Some("yes")
    );
}

#[tokio::test]
async fn stream_and_object_builders_forward_relevant_request_options() {
    let parameters = ModelParameters {
        temperature: Some(0.25),
        ..ModelParameters::default()
    };

    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut text_stream = stream_text_builder()
        .model("provider:model")
        .system("system")
        .messages(vec![history()])
        .parameters(parameters.clone())
        .metadata("trace", "yes")
        .timeout(Duration::from_secs(5))
        .cancellation(CancellationToken::new())
        .prompt("text")
        .run(RecordingProvider::new(Arc::clone(&requests), "text"));
    while let Some(item) = text_stream.next().await {
        if matches!(item, AgentStreamItem::Finished(_)) {
            break;
        }
    }
    assert_request(&requests.lock().expect("requests lock")[0]);

    requests.lock().expect("requests lock").clear();
    let object: Output = generate_object_builder()
        .model("provider:model")
        .system("system")
        .messages(vec![history()])
        .parameters(parameters.clone())
        .metadata("trace", "yes")
        .timeout(Duration::from_secs(5))
        .cancellation(CancellationToken::new())
        .prompt("object")
        .run(&mut RecordingProvider::new(
            Arc::clone(&requests),
            r#"{"value":"generated"}"#,
        ))
        .await
        .expect("object should decode");
    assert_eq!(object.value, "generated");
    assert_request(&requests.lock().expect("requests lock")[0]);

    requests.lock().expect("requests lock").clear();
    let mut object_stream = stream_object_builder::<Output>()
        .model("provider:model")
        .system("system")
        .messages(vec![history()])
        .parameters(parameters)
        .metadata("trace", "yes")
        .timeout(Duration::from_secs(5))
        .cancellation(CancellationToken::new())
        .prompt("object stream")
        .run(RecordingProvider::new(
            Arc::clone(&requests),
            r#"{"value":"streamed"}"#,
        ));
    while let Some(item) = object_stream.next().await {
        if matches!(item, ObjectStreamItem::Finished { .. }) {
            break;
        }
    }
    assert_request(&requests.lock().expect("requests lock")[0]);
}
