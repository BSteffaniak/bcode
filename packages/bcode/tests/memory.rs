use bcode::{
    Agent, BcodeError, MemoryFailurePolicy, MemoryItem, MemoryPolicy, MemoryPrivacy,
    MemoryProvenance, MemoryProvider, MemoryRetention, MemoryRetrievalRequest, MessageRole,
    ModelContentBlock, ModelMessage, ModelProviderInvoker, PersistedSession, RuntimeFuture,
    SessionPersistenceAdapter,
};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse, StopReason,
};
use std::sync::{Arc, Mutex};

fn message(text: impl Into<String>) -> ModelMessage {
    ModelMessage {
        role: MessageRole::User,
        content: vec![ModelContentBlock::Text { text: text.into() }],
    }
}

fn memory(id: &str, text: &str, relevance_millis: u16, privacy: MemoryPrivacy) -> MemoryItem {
    MemoryItem {
        id: id.to_string(),
        message: message(text),
        relevance_millis,
        provenance: MemoryProvenance {
            provider_id: "profile-memory".to_string(),
            source_id: format!("source-{id}"),
        },
        privacy,
        retention: MemoryRetention::RequestOnly,
    }
}

#[derive(Debug)]
struct QueryMemory;

impl MemoryProvider for QueryMemory {
    fn retrieve(&self, request: &MemoryRetrievalRequest) -> bcode::Result<Vec<MemoryItem>> {
        assert_eq!(request.query, "what fruit?");
        assert!(request.transcript.is_empty());
        Ok(vec![
            memory("low", "low relevance", 100, MemoryPrivacy::Public),
            memory("high", "remember apples", 900, MemoryPrivacy::Private),
            memory(
                "sensitive",
                "private token",
                1_000,
                MemoryPrivacy::Sensitive,
            ),
        ])
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
        *self.messages.lock().expect("messages lock") = request.messages.clone();
        self.events = vec![
            ProviderTurnEvent::TextDelta {
                text: "apples".to_string(),
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::EndTurn,
            },
        ];
        Box::pin(async {
            Ok(StartTurnResponse {
                provider_turn_id: "memory-turn".to_string(),
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
async fn retrieval_is_query_aware_ranked_bounded_private_and_request_only() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut provider = RecordingProvider {
        messages: Arc::clone(&requests),
        events: Vec::new(),
    };
    let mut session = Agent::builder()
        .build()
        .session()
        .with_memory_provider(QueryMemory)
        .with_memory_policy(MemoryPolicy {
            max_items: 1,
            max_item_bytes: 1_024,
            max_total_bytes: 1_024,
            max_privacy: MemoryPrivacy::Private,
            failure_policy: MemoryFailurePolicy::ContinueWithoutMemory,
        });

    session
        .send(&mut provider, "what fruit?")
        .await
        .expect("memory turn should succeed");

    let request = requests.lock().expect("messages lock");
    assert_eq!(request.len(), 2);
    assert!(matches!(
        &request[0].content[..],
        [ModelContentBlock::Text { text }] if text == "remember apples"
    ));
    assert_eq!(session.session().messages().len(), 2);
    assert!(session.session().messages().iter().all(|message| {
        !message.content.iter().any(|content| {
            matches!(content, ModelContentBlock::Text { text } if text == "remember apples")
        })
    }));
    let report = session.memory_report();
    assert_eq!(report.accepted.len(), 1);
    assert_eq!(report.accepted[0].id, "high");
    assert_eq!(report.accepted[0].provenance.source_id, "source-high");
    assert!(report.accepted_bytes > 0);
    assert!(
        report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("privacy_not_allowed"))
    );
}

#[derive(Debug)]
struct OversizedMemory;

impl MemoryProvider for OversizedMemory {
    fn retrieve(&self, _request: &MemoryRetrievalRequest) -> bcode::Result<Vec<MemoryItem>> {
        Ok(vec![
            memory(
                "large",
                "this item is too large",
                1_000,
                MemoryPrivacy::Public,
            ),
            memory("small", "ok", 900, MemoryPrivacy::Public),
        ])
    }
}

#[tokio::test]
async fn memory_item_and_total_size_bounds_are_enforced_before_model_context() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut provider = RecordingProvider {
        messages: Arc::clone(&requests),
        events: Vec::new(),
    };
    let small_bytes = serde_json::to_vec(&message("ok"))
        .expect("message should encode")
        .len();
    let mut session = Agent::builder()
        .build()
        .session()
        .with_memory_provider(OversizedMemory)
        .with_memory_policy(MemoryPolicy {
            max_items: 4,
            max_item_bytes: small_bytes,
            max_total_bytes: small_bytes,
            max_privacy: MemoryPrivacy::Public,
            failure_policy: MemoryFailurePolicy::ContinueWithoutMemory,
        });

    session
        .send(&mut provider, "bounded")
        .await
        .expect("oversized memory should be skipped");

    let request = requests.lock().expect("messages lock");
    assert_eq!(request.len(), 2);
    assert!(matches!(
        &request[0].content[..],
        [ModelContentBlock::Text { text }] if text == "ok"
    ));
    assert_eq!(session.memory_report().accepted_bytes, small_bytes);
    assert!(
        session
            .memory_report()
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("item_too_large"))
    );
}

#[derive(Debug)]
struct FailingMemory;

impl MemoryProvider for FailingMemory {
    fn retrieve(&self, _request: &MemoryRetrievalRequest) -> bcode::Result<Vec<MemoryItem>> {
        Err(BcodeError::Hook("secret backend detail".to_string()))
    }
}

#[tokio::test]
async fn memory_failure_policy_is_explicit_and_secret_safe() {
    let mut provider = RecordingProvider {
        messages: Arc::new(Mutex::new(Vec::new())),
        events: Vec::new(),
    };
    let mut strict = Agent::builder()
        .build()
        .session()
        .with_memory_provider(FailingMemory);
    let error = strict
        .send(&mut provider, "strict")
        .await
        .expect_err("strict policy should fail");
    assert!(matches!(
        error,
        BcodeError::Memory {
            provider_index: 0,
            code: "retrieval_failed"
        }
    ));
    assert!(!error.to_string().contains("secret backend detail"));

    let mut tolerant = Agent::builder()
        .build()
        .session()
        .with_memory_provider(FailingMemory)
        .with_memory_policy(MemoryPolicy {
            failure_policy: MemoryFailurePolicy::ContinueWithoutMemory,
            ..MemoryPolicy::default()
        });
    tolerant
        .send(&mut provider, "tolerant")
        .await
        .expect("tolerant policy should continue");
    assert_eq!(
        tolerant.memory_report().diagnostics,
        ["provider:0:retrieval_failed"]
    );
}

#[derive(Debug, Default)]
struct MemoryPersistence(Mutex<Option<PersistedSession>>);

impl SessionPersistenceAdapter for MemoryPersistence {
    fn load(&self) -> bcode::Result<Option<PersistedSession>> {
        Ok(self.0.lock().expect("persistence lock").clone())
    }

    fn save(&self, session: &PersistedSession) -> bcode::Result<()> {
        *self.0.lock().expect("persistence lock") = Some(session.clone());
        Ok(())
    }
}

#[test]
fn older_persisted_session_payload_defaults_memory_records() {
    let value = serde_json::json!({
        "session_id": bcode::SessionId::new(),
        "messages": []
    });
    let persisted: PersistedSession =
        serde_json::from_value(value).expect("older session payload should decode");
    assert!(persisted.memories.is_empty());
}

#[test]
fn persistence_requires_an_explicit_session_transcript_memory_item() {
    let persistence = Arc::new(MemoryPersistence::default());
    let mut session = Agent::builder()
        .build()
        .session()
        .with_persistence(persistence.clone());
    let error = session
        .remember(memory(
            "request",
            "request only",
            500,
            MemoryPrivacy::Public,
        ))
        .expect_err("request-only memory must not persist");
    assert!(matches!(
        error,
        BcodeError::MemoryValidation {
            code: "request_only_cannot_be_persisted"
        }
    ));

    let mut persisted = memory("persisted", "visible memory", 500, MemoryPrivacy::Private);
    persisted.retention = MemoryRetention::SessionTranscript;
    session
        .remember(persisted)
        .expect("explicit persisted memory should append");
    assert_eq!(session.session().messages(), [message("visible memory")]);
    let saved = persistence
        .load()
        .expect("persistence should load")
        .expect("persisted memory should save");
    assert_eq!(saved.messages, [message("visible memory")]);
    assert_eq!(saved.memories.len(), 1);
    assert_eq!(saved.memories[0].id, "persisted");
    assert_eq!(saved.memories[0].provenance.source_id, "source-persisted");
    assert_eq!(saved.memories[0].privacy, MemoryPrivacy::Private);
    let restored = Agent::builder()
        .build()
        .session_with_persistence(persistence)
        .expect("persisted memory should restore");
    assert_eq!(restored.session().persisted_memories(), saved.memories);
}
