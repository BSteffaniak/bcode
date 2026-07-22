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

#[derive(Debug)]
struct UnsupportedVersionAdapter;

impl SessionPersistenceAdapter for UnsupportedVersionAdapter {
    fn load(&self) -> bcode::Result<Option<PersistedSession>> {
        Ok(Some(PersistedSession {
            schema_version: bcode::PERSISTED_SESSION_SCHEMA_VERSION + 1,
            session_id: bcode::SessionId::new(),
            messages: Vec::new(),
            memories: Vec::new(),
        }))
    }

    fn save(&self, _session: &PersistedSession) -> bcode::Result<()> {
        Ok(())
    }
}

#[test]
fn custom_adapter_payload_version_is_validated_before_use() {
    let error = Agent::builder()
        .build()
        .session_with_persistence(Arc::new(UnsupportedVersionAdapter))
        .expect_err("unsupported payload must require migration");
    assert!(matches!(error, BcodeError::SessionState(message) if message.contains("migrate")));
}

#[derive(Debug)]
struct InconsistentMemoryAdapter;

impl SessionPersistenceAdapter for InconsistentMemoryAdapter {
    fn load(&self) -> bcode::Result<Option<PersistedSession>> {
        Ok(Some(PersistedSession {
            schema_version: bcode::PERSISTED_SESSION_SCHEMA_VERSION,
            session_id: bcode::SessionId::new(),
            messages: Vec::new(),
            memories: vec![bcode::MemoryItem {
                id: "memory".to_string(),
                message: bcode::ModelMessage {
                    role: bcode::MessageRole::User,
                    content: vec![bcode::ModelContentBlock::Text {
                        text: "missing transcript message".to_string(),
                    }],
                },
                relevance_millis: 500,
                provenance: bcode::MemoryProvenance {
                    provider_id: "test".to_string(),
                    source_id: "source".to_string(),
                },
                privacy: bcode::MemoryPrivacy::Public,
                retention: bcode::MemoryRetention::SessionTranscript,
            }],
        }))
    }

    fn save(&self, _session: &PersistedSession) -> bcode::Result<()> {
        Ok(())
    }
}

#[test]
fn inconsistent_adapter_payload_is_rejected_without_repair_or_mutation() {
    let error = Agent::builder()
        .build()
        .session_with_persistence(Arc::new(InconsistentMemoryAdapter))
        .expect_err("inconsistent payload must fail");
    assert!(matches!(error, BcodeError::SessionState(message) if message.contains("inconsistent")));
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
    assert_eq!(
        saved.schema_version,
        bcode::PERSISTED_SESSION_SCHEMA_VERSION
    );
    assert_eq!(saved.messages.len(), 2);

    let restored = Agent::builder()
        .model("adapter-model")
        .build()
        .session_with_persistence(adapter)
        .expect("saved adapter should restore");
    assert_eq!(restored.session().messages(), saved.messages);
}
