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

#[derive(Clone)]
struct RankingMemory;

impl MemoryProvider for RankingMemory {
    fn retrieve(&self, _: &MemoryRetrievalRequest) -> bcode::Result<Vec<MemoryItem>> {
        Ok(vec![
            memory("baseline", "irrelevant", 900, MemoryPrivacy::Public),
            memory("useful", "remember apples", 100, MemoryPrivacy::Public),
        ])
    }
}

struct AdvisoryRanker {
    calls: Arc<Mutex<usize>>,
    outcome: u8,
}

impl bcode::MemoryContextRanker for AdvisoryRanker {
    fn rank<'a>(
        &'a self,
        task: &'a str,
        candidates: &'a [bcode_session_models::JudgementContextCandidate],
        _budget_bytes: usize,
    ) -> bcode::MemoryRankingFuture<'a> {
        *self.calls.lock().expect("calls") += 1;
        assert_eq!(task, "what fruit?");
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[1].snippet, "remember apples");
        assert!(candidates[1].provenance.contains("source-useful"));
        Box::pin(async move {
            match self.outcome {
                0 => Ok(Some(
                    candidates
                        .iter()
                        .rev()
                        .map(|item| item.id.clone())
                        .collect(),
                )),
                1 => Ok(None),
                2 => Err(BcodeError::MemoryValidation {
                    code: "advisory_unavailable",
                }),
                _ => Ok(Some(vec![candidates[0].id.clone(); 2])),
            }
        })
    }
}

#[tokio::test]
async fn normal_sdk_memory_incorporation_applies_advice_and_preserves_fallback() {
    // The application adapter resolves no-key/off as None and errors as failures. This test
    // exercises SDK incorporation, not central credential discovery or provider inference.
    for outcome in 0..4 {
        let calls = Arc::new(Mutex::new(0));
        let mut session = Agent::builder()
            .build()
            .session()
            .with_memory_provider(RankingMemory)
            .with_memory_policy(MemoryPolicy {
                max_items: 1,
                ..MemoryPolicy::default()
            })
            .with_memory_context_ranker(
                Arc::new(AdvisoryRanker {
                    calls: Arc::clone(&calls),
                    outcome,
                }),
                MemoryPrivacy::Public,
            );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut provider = RecordingProvider {
            messages: Arc::clone(&requests),
            events: Vec::new(),
        };
        session
            .send(&mut provider, "what fruit?")
            .await
            .expect("turn");
        let expected = if outcome == 0 { "useful" } else { "baseline" };
        assert_eq!(session.memory_report().accepted[0].id, expected);
        assert_eq!(
            session.memory_report().accepted[0].provenance.source_id,
            format!("source-{expected}")
        );
        assert_eq!(*calls.lock().expect("calls"), 1);
        let requests = requests.lock().expect("requests");
        assert!(
            matches!(&requests[0].content[..], [ModelContentBlock::Text { text }]
            if text == if outcome == 0 { "remember apples" } else { "irrelevant" })
        );
        assert_eq!(session.session().messages().len(), 2);
    }
}

#[derive(Clone)]
struct ClassifiedMemory(MemoryPrivacy);

impl MemoryProvider for ClassifiedMemory {
    fn retrieve(&self, _: &MemoryRetrievalRequest) -> bcode::Result<Vec<MemoryItem>> {
        Ok(vec![
            memory("baseline", "irrelevant", 900, self.0),
            memory("useful", "remember apples", 100, self.0),
        ])
    }
}

#[tokio::test]
async fn sdk_ranking_does_not_transfer_private_memory_or_judge_without_pressure() {
    for (privacy, max_items) in [
        (MemoryPrivacy::Private, 1),
        (MemoryPrivacy::Sensitive, 1),
        (MemoryPrivacy::Public, 2),
    ] {
        let calls = Arc::new(Mutex::new(0));
        let mut session = Agent::builder()
            .build()
            .session()
            .with_memory_provider(ClassifiedMemory(privacy))
            .with_memory_policy(MemoryPolicy {
                max_items,
                max_privacy: privacy,
                ..MemoryPolicy::default()
            })
            .with_memory_context_ranker(
                Arc::new(AdvisoryRanker {
                    calls: Arc::clone(&calls),
                    outcome: 0,
                }),
                MemoryPrivacy::Public,
            );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut provider = RecordingProvider {
            messages: Arc::clone(&requests),
            events: Vec::new(),
        };
        session
            .send(&mut provider, "what fruit?")
            .await
            .expect("turn");
        assert_eq!(*calls.lock().expect("calls"), 0);
        assert_eq!(session.memory_report().accepted[0].id, "baseline");
        assert_eq!(session.memory_report().accepted.len(), max_items);
        assert_eq!(requests.lock().expect("requests").len(), max_items + 1);
    }
}

struct ExplicitContext;

impl bcode::SessionContextProvider for ExplicitContext {
    fn context_messages(&self, _: &bcode::InMemorySession) -> bcode::Result<Vec<ModelMessage>> {
        Ok(vec![message("mandatory explicit context")])
    }
}

#[tokio::test]
async fn sdk_ranking_obeys_serialized_byte_budget_and_retains_explicit_context() {
    let calls = Arc::new(Mutex::new(0));
    let useful = memory("useful", "remember apples", 100, MemoryPrivacy::Public);
    let budget = serde_json::to_vec(&useful.message)
        .expect("serialized message")
        .len();
    let mut session = Agent::builder()
        .build()
        .session()
        .with_context_provider(ExplicitContext)
        .with_memory_provider(RankingMemory)
        .with_memory_policy(MemoryPolicy {
            max_total_bytes: budget,
            ..MemoryPolicy::default()
        })
        .with_memory_context_ranker(
            Arc::new(AdvisoryRanker {
                calls: Arc::clone(&calls),
                outcome: 0,
            }),
            MemoryPrivacy::Public,
        );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut provider = RecordingProvider {
        messages: Arc::clone(&requests),
        events: Vec::new(),
    };
    session
        .send(&mut provider, "what fruit?")
        .await
        .expect("turn");
    assert_eq!(*calls.lock().expect("calls"), 1);
    assert_eq!(session.memory_report().accepted_bytes, budget);
    assert_eq!(session.memory_report().accepted.len(), 1);
    assert_eq!(session.memory_report().accepted[0].id, "useful");
    let requests = requests.lock().expect("requests");
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0], message("mandatory explicit context"));
    assert_eq!(requests[1], useful.message);
    assert_eq!(requests[2], message("what fruit?"));
    assert_eq!(session.session().messages().len(), 2);
}

#[cfg(feature = "daemon-client")]
#[tokio::test]
async fn sdk_history_search_and_ranking_capture_real_request_only_context() {
    use bcode_ipc::{Request, Response, ResponsePayload};
    use bcode_session_search::{SessionSearchQuery, TextMatchMode};
    for arm in 0..5u8 {
        let session_id = bcode::SessionId::new();
        // macOS's environment temp root can exceed the Unix socket path limit.
        // Keep only this fixture's unique socket directory short; daemon identity is unchanged.
        let directory = std::path::PathBuf::from(format!("/tmp/bch-{session_id}"));
        std::fs::create_dir(&directory).expect("fixture directory");
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(directory.join("test.sock"));
        let listener = bcode_ipc::LocalIpcListener::bind(&endpoint).expect("listener");
        let (_, digest) = bcode_daemon_lifecycle::current_executable_identity().expect("identity");
        let daemon = bcode_ipc::DaemonStatus {
            namespace: bcode_ipc::daemon_namespace(),
            protocol_version: u32::from(bcode_ipc::CURRENT_PROTOCOL_VERSION),
            artifact_id: Some(bcode_ipc::ArtifactId::current()),
            build_fingerprint: bcode_ipc::BUILD_FINGERPRINT.to_owned(),
            executable_digest: Some(digest),
            storage_writer_epoch: Some(bcode_ipc::CURRENT_SESSION_STORAGE_WRITER_EPOCH),
            session_event_schema_version: Some(
                bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            ),
            state_location_id: Some(bcode_ipc::state_location_id()),
            ..bcode_ipc::DaemonStatus::default()
        };
        let calls = Arc::new(Mutex::new((0usize, 0usize)));
        let observed = Arc::clone(&calls);
        let server = tokio::spawn(async move {
            loop {
                let mut stream = listener.accept().await.expect("accept");
                let hello = bcode_ipc::recv_envelope(&mut stream).await.expect("hello");
                bcode_ipc::send_envelope(
                    &mut stream,
                    &bcode_ipc::response_envelope(
                        hello.request_id,
                        &Response::Ok(ResponsePayload::Hello {
                            protocol_version: bcode_ipc::ProtocolVersion::current(),
                            client_id: bcode_session_models::ClientId::new(),
                            daemon: daemon.clone(),
                        }),
                    )
                    .expect("hello envelope"),
                )
                .await
                .expect("hello send");
                let envelope = bcode_ipc::recv_envelope(&mut stream)
                    .await
                    .expect("request");
                let payload = match bcode_ipc::decode_request(&envelope.payload).expect("decode") {
                    Request::SessionSearch {
                        request,
                        hydrate,
                        routes,
                        policy,
                    } => {
                        observed.lock().expect("calls").0 += 1;
                        assert!(!hydrate);
                        assert!(routes.is_empty());
                        assert!(!policy.allow_remote);
                        assert_eq!(request.limit, 2);
                        assert_eq!(
                            request.filters.session_ids,
                            [session_id].into_iter().collect()
                        );
                        let hits = ["irrelevant", "apples"]
                            .into_iter()
                            .enumerate()
                            .map(|(index, text)| bcode_session_search::SessionSearchHit {
                                locator: bcode_session_search::SessionSearchLocator {
                                    session_id,
                                    sequence: u64::try_from(index).expect("bounded index") + 1,
                                    record_id: Some(format!("record-{index}")),
                                },
                                content_kind: bcode_session_search::SearchContentKind::UserMessage,
                                matched_field: bcode_session_search::SearchField::Text,
                                provider_id: "indexed".into(),
                                provider_rank: u32::try_from(index).expect("bounded index") + 1,
                                provider_score: None,
                                preview: Some(text.into()),
                                preview_truncated: false,
                            })
                            .collect();
                        ResponsePayload::SessionSearch {
                            response: bcode_session_search::FederatedSessionSearchResponse {
                                hits,
                                query_complete: true,
                                coverage_complete: false,
                                providers: Vec::new(),
                                failures: Vec::new(),
                            },
                            hydrated_hits: Vec::new(),
                        }
                    }
                    Request::SelectOptionalContext { request_json } => {
                        observed.lock().expect("calls").1 += 1;
                        if arm == 2 {
                            // Simulate advisory transport failure, not a retrieval failure.
                            continue;
                        }
                        let request: bcode_session_models::ContextSelectionRequest =
                            serde_json::from_str(&request_json).expect("selection");
                        assert_eq!(request.task, "what fruit?");
                        assert!(request.candidates.iter().all(|candidate| candidate.source
                            == bcode_session_models::JudgementContextSource::History));
                        let mut ids = request
                            .candidates
                            .iter()
                            .map(|candidate| candidate.id.clone())
                            .collect::<Vec<_>>();
                        if arm == 0 {
                            ids.reverse();
                        }
                        if arm == 3 {
                            ids[1] = ids[0].clone();
                        }
                        ResponsePayload::OptionalContextSelection {
                            result_json: serde_json::to_string(
                                &bcode_session_models::ContextSelectionResponse {
                                    ordered_ids: ids.clone(),
                                    included_ids: ids,
                                    applied: arm == 0 || arm == 3,
                                },
                            )
                            .expect("result"),
                        }
                    }
                    request => panic!("unexpected request: {request:?}"),
                };
                bcode_ipc::send_envelope(
                    &mut stream,
                    &bcode_ipc::response_envelope(envelope.request_id, &Response::Ok(payload))
                        .expect("envelope"),
                )
                .await
                .expect("send");
            }
        });
        let request = bcode_session_search::SessionSearchRequest {
            query: SessionSearchQuery::Text {
                text: "fruit".into(),
                mode: TextMatchMode::Terms,
                fields: Default::default(),
            },
            filters: bcode_session_search::SessionSearchFilters {
                session_ids: [session_id].into_iter().collect(),
                ..Default::default()
            },
            sort: Default::default(),
            limit: 2,
            cursor: None,
            deadline_ms: None,
        };
        let mut history = bcode::DaemonHistoryContextProvider::new(
            bcode::BcodeClient::new(endpoint),
            request,
            Default::default(),
            Vec::new(),
            10,
            MemoryPrivacy::Private,
        );
        if arm != 4 {
            history = history.with_judgement_transfer(MemoryPrivacy::Private);
        }
        let mut session = Agent::builder()
            .build()
            .session()
            .with_context_provider(ExplicitContext)
            .with_history_context_provider(history.clone());
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut provider = RecordingProvider {
            messages: Arc::clone(&captured),
            events: Vec::new(),
        };
        session
            .send(&mut provider, "what fruit?")
            .await
            .expect("turn");
        assert_eq!(*calls.lock().expect("calls"), (1, usize::from(arm != 4)));
        let report = history.report().expect("coverage report");
        assert!(!report.coverage_complete);
        assert_eq!(report.hits.len(), 2);
        {
            let captured = captured.lock().expect("captured");
            assert_eq!(captured.len(), 3);
            assert_eq!(captured[0], message("mandatory explicit context"));
            let expected = if arm == 0 { "apples" } else { "irrelevant" };
            assert!(
                matches!(&captured[1].content[..], [ModelContentBlock::Text { text }] if text.ends_with(expected) && text.contains("record-"))
            );
            assert_eq!(captured[2], message("what fruit?"));
        }
        assert_eq!(session.session().messages().len(), 2);
        server.abort();
        let _ = server.await;
        // Awaited task teardown drops the listener, which owns socket unlinking.
        assert!(
            !directory.join("test.sock").exists(),
            "listener must release its socket"
        );
        std::fs::remove_dir(directory).expect("remove fixture directory");
    }
}

#[tokio::test]
async fn absent_sdk_advisory_adapter_keeps_memory_baseline() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut provider = RecordingProvider {
        messages: Arc::clone(&requests),
        events: Vec::new(),
    };
    let mut session = Agent::builder()
        .build()
        .session()
        .with_memory_provider(RankingMemory)
        .with_memory_policy(MemoryPolicy {
            max_items: 1,
            ..MemoryPolicy::default()
        });
    session
        .send(&mut provider, "what fruit?")
        .await
        .expect("turn");
    assert_eq!(session.memory_report().accepted[0].id, "baseline");
}
