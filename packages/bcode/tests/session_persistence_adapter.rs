use bcode::{
    Agent, BcodeError, ModelProviderInvoker, PersistedSession, RuntimeFuture,
    SessionPersistenceAdapter,
};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse, StopReason,
};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
struct MemoryAdapter {
    session: Mutex<Option<PersistedSession>>,
}

impl SessionPersistenceAdapter for MemoryAdapter {
    fn load(&self) -> bcode::Result<Option<PersistedSession>> {
        Ok(self
            .session
            .lock()
            .map_err(|error| BcodeError::SessionPersistence(error.to_string()))?
            .clone())
    }

    fn save(&self, session: &PersistedSession) -> bcode::Result<()> {
        *self
            .session
            .lock()
            .map_err(|error| BcodeError::SessionPersistence(error.to_string()))? =
            Some(session.clone());
        Ok(())
    }
}

#[derive(Debug, Default)]
struct TextProvider;

impl ModelProviderInvoker for TextProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        Box::pin(async {
            Ok(StartTurnResponse {
                provider_turn_id: "adapter-turn".to_string(),
            })
        })
    }

    fn poll_turn_events<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a PollTurnEventsRequest,
    ) -> RuntimeFuture<'a, PollTurnEventsResponse> {
        Box::pin(async {
            Ok(PollTurnEventsResponse {
                events: vec![
                    ProviderTurnEvent::TextDelta {
                        text: "persisted response".to_string(),
                    },
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

#[tokio::test]
async fn custom_adapter_loads_and_saves_successful_turns() {
    let adapter = Arc::new(MemoryAdapter::default());
    let persistence: Arc<dyn SessionPersistenceAdapter> = adapter.clone();
    let agent = Agent::builder().model("adapter-model").build();
    let mut session = agent
        .session_with_persistence(persistence)
        .expect("empty adapter should load");

    session
        .generate_text_with_provider(&mut TextProvider, "persist this")
        .await
        .expect("turn should be persisted");

    let saved = adapter
        .load()
        .expect("adapter should remain readable")
        .expect("successful turn should save");
    assert_eq!(saved.messages.len(), 2);

    let restored = Agent::builder()
        .model("adapter-model")
        .build()
        .session_with_persistence(adapter)
        .expect("saved adapter should restore");
    assert_eq!(restored.session().messages(), saved.messages);
}
