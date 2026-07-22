use bcode::{
    Agent, BcodeError, MessageRole, ModelContentBlock, ModelMessage, ModelProviderInvoker,
    PersistedSession, RuntimeFuture, SessionPersistenceAdapter, ToolCall, ToolDefinition,
    ToolInvocationResponse, ToolPolicyMetadata, ToolSideEffect, ToolUiMetadata,
};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse, StopReason,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[derive(Debug)]
struct ScriptedProvider {
    turns: VecDeque<Vec<ProviderTurnEvent>>,
    requests: Arc<Mutex<Vec<ModelTurnRequest>>>,
    active: Vec<ProviderTurnEvent>,
}

impl ScriptedProvider {
    fn new(requests: Arc<Mutex<Vec<ModelTurnRequest>>>) -> Self {
        Self {
            turns: VecDeque::from([
                vec![
                    ProviderTurnEvent::ReasoningDelta {
                        text: "need lookup".to_string(),
                    },
                    ProviderTurnEvent::ToolCallFinished {
                        call: ToolCall {
                            id: "lookup-1".to_string(),
                            name: "lookup".to_string(),
                            arguments: serde_json::json!({"key": "fruit"}),
                        },
                    },
                    ProviderTurnEvent::TurnFinished {
                        stop_reason: StopReason::ToolCall,
                    },
                ],
                vec![
                    ProviderTurnEvent::TextDelta {
                        text: "first answer".to_string(),
                    },
                    ProviderTurnEvent::TurnFinished {
                        stop_reason: StopReason::EndTurn,
                    },
                ],
                vec![
                    ProviderTurnEvent::TextDelta {
                        text: "second answer".to_string(),
                    },
                    ProviderTurnEvent::TurnFinished {
                        stop_reason: StopReason::EndTurn,
                    },
                ],
                vec![
                    ProviderTurnEvent::TextDelta {
                        text: "regenerated answer".to_string(),
                    },
                    ProviderTurnEvent::TurnFinished {
                        stop_reason: StopReason::EndTurn,
                    },
                ],
            ]),
            requests,
            active: Vec::new(),
        }
    }
}

impl ModelProviderInvoker for ScriptedProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        self.requests
            .lock()
            .expect("request lock")
            .push(request.clone());
        self.active = self.turns.pop_front().expect("scripted provider turn");
        Box::pin(async {
            Ok(StartTurnResponse {
                provider_turn_id: "stateful-turn".to_string(),
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
                events: std::mem::take(&mut self.active),
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

#[derive(Debug, Default)]
struct CapturingPersistence(Mutex<Option<PersistedSession>>);

impl SessionPersistenceAdapter for CapturingPersistence {
    fn load(&self) -> bcode::Result<Option<PersistedSession>> {
        Ok(self.0.lock().expect("persistence lock").clone())
    }

    fn save(&self, session: &PersistedSession) -> bcode::Result<()> {
        *self.0.lock().expect("persistence lock") = Some(session.clone());
        Ok(())
    }
}

fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "lookup".to_string(),
        description: "lookup state".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: ToolPolicyMetadata::default(),
        ui: ToolUiMetadata::default(),
    }
}

fn text_message(text: &str) -> ModelMessage {
    ModelMessage {
        role: MessageRole::User,
        content: vec![ModelContentBlock::Text {
            text: text.to_string(),
        }],
    }
}

#[tokio::test]
async fn stateful_chat_preserves_full_visible_and_model_transcript_semantics() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let persistence = Arc::new(CapturingPersistence::default());
    let mut provider = ScriptedProvider::new(requests.clone());
    let agent = Agent::builder()
        .system("system instruction")
        .inline_tool(definition(), |_| {
            Ok(ToolInvocationResponse {
                output: "apples".to_string(),
                is_error: false,
                content: Vec::new(),
                full_output: None,
                host_action: None,
                result: None,
            })
        })
        .build();
    let mut session = agent.session().with_persistence(persistence.clone());

    session
        .send(&mut provider, "first question")
        .await
        .expect("first turn");
    let first_transcript = session.session().messages().to_vec();
    assert_eq!(
        first_transcript
            .iter()
            .map(|message| message.role)
            .collect::<Vec<_>>(),
        [
            MessageRole::User,
            MessageRole::Assistant,
            MessageRole::Tool,
            MessageRole::Assistant,
        ]
    );
    assert!(first_transcript[1].content.iter().any(|content| matches!(
        content,
        ModelContentBlock::ToolCall { call } if call.id == "lookup-1"
    )));
    assert!(first_transcript[2].content.iter().any(|content| matches!(
        content,
        ModelContentBlock::ToolResult { result } if result.output == "apples"
    )));
    assert!(
        first_transcript
            .iter()
            .all(|message| message.role != MessageRole::System)
    );

    session
        .send(&mut provider, "second question")
        .await
        .expect("second turn");
    let branch = session.branch();
    let fork = session.fork();
    assert!(branch.persistence().is_none());
    assert!(fork.persistence().is_none());
    assert_eq!(branch.session(), session.session());
    assert_eq!(fork.session(), session.session());

    session
        .regenerate_last_with_provider(&mut provider)
        .await
        .expect("regeneration");
    assert!(session.session().messages().iter().any(|message| message.content.iter().any(
        |content| matches!(content, ModelContentBlock::Text { text } if text == "regenerated answer")
    )));
    assert!(session.session().messages().iter().all(|message| {
        message.content.iter().all(
        |content| !matches!(content, ModelContentBlock::Text { text } if text == "second answer")
    )
    }));

    let exported = session.clone().into_messages();
    let imported = Agent::builder()
        .build()
        .session_from_messages(exported.clone());
    assert_eq!(imported.session().messages(), exported);
    let saved = persistence
        .load()
        .expect("persistence load")
        .expect("session persisted");
    assert_eq!(saved.messages, exported);

    let requests = requests.lock().expect("request lock");
    assert!(
        requests
            .iter()
            .all(|request| request.system_prompt.as_deref() == Some("system instruction"))
    );
    let second_request = &requests[2];
    assert!(
        second_request
            .messages
            .iter()
            .any(|message| message.role == MessageRole::Tool)
    );
    assert!(
        second_request
            .messages
            .iter()
            .any(|message| message.content.iter().any(|content| {
                matches!(content, ModelContentBlock::ToolCall { call } if call.id == "lookup-1")
            }))
    );
}

#[derive(Debug)]
struct FailingPersistence;

impl SessionPersistenceAdapter for FailingPersistence {
    fn load(&self) -> bcode::Result<Option<PersistedSession>> {
        Ok(None)
    }

    fn save(&self, _session: &PersistedSession) -> bcode::Result<()> {
        Err(BcodeError::SessionPersistence("save failed".to_string()))
    }
}

#[tokio::test]
async fn persistence_failure_does_not_mutate_visible_transcript() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut provider = ScriptedProvider {
        turns: VecDeque::from([vec![
            ProviderTurnEvent::TextDelta {
                text: "answer".to_string(),
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::EndTurn,
            },
        ]]),
        requests,
        active: Vec::new(),
    };
    let mut session = Agent::builder()
        .build()
        .session_from_messages(vec![text_message("existing")])
        .with_persistence(Arc::new(FailingPersistence));
    let before = session.session().messages().to_vec();

    session
        .send(&mut provider, "new")
        .await
        .expect_err("save failure should fail the turn commit");
    assert_eq!(session.session().messages(), before);
}
