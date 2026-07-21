use bcode::{
    Agent, MessageRole, ModelContentBlock, ModelMessage, ModelProviderInvoker, RuntimeFuture,
    SessionContextProvider,
};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse, StopReason,
};
use std::sync::{Arc, Mutex};

#[derive(Debug)]
struct MemoryContext;

impl SessionContextProvider for MemoryContext {
    fn context_messages(
        &self,
        _session: &bcode::InMemorySession,
    ) -> bcode::Result<Vec<ModelMessage>> {
        Ok(vec![ModelMessage {
            role: MessageRole::User,
            content: vec![ModelContentBlock::Text {
                text: "remembered profile".to_string(),
            }],
        }])
    }
}

#[derive(Debug)]
struct RecordingProvider {
    messages: Arc<Mutex<Vec<ModelMessage>>>,
    events: Vec<ProviderTurnEvent>,
}

impl ModelProviderInvoker for RecordingProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        *self
            .messages
            .lock()
            .expect("message lock should be available") = request.messages.clone();
        self.events = vec![
            ProviderTurnEvent::TextDelta {
                text: "assistant response".to_string(),
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::EndTurn,
            },
        ];
        Box::pin(async {
            Ok(StartTurnResponse {
                provider_turn_id: "chat-turn".to_string(),
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
                events: std::mem::take(&mut self.events),
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

#[tokio::test]
async fn chat_injects_memory_context_without_persisting_it() {
    let messages = Arc::new(Mutex::new(Vec::new()));
    let mut provider = RecordingProvider {
        messages: Arc::clone(&messages),
        events: Vec::new(),
    };
    let mut chat = Agent::builder()
        .model("chat-model")
        .build()
        .chat()
        .with_context_provider(MemoryContext);

    let response = chat
        .send(&mut provider, "hello")
        .await
        .expect("chat turn should succeed");

    assert_eq!(response.text, "assistant response");
    let request_messages = messages.lock().expect("message lock should be available");
    assert_eq!(request_messages.len(), 2);
    assert_eq!(chat.session().messages().len(), 2);
    assert!(chat.session().messages().iter().all(|message| {
        !message.content.iter().any(|content| {
            matches!(content, ModelContentBlock::Text { text } if text == "remembered profile")
        })
    }));
}
