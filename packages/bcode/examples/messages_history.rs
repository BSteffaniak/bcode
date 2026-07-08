use bcode::{MessageRole, ModelContentBlock, ModelMessage, ModelProviderInvoker, RuntimeFuture};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse, StopReason,
};
use std::collections::VecDeque;

#[derive(Debug, Default)]
struct HistoryAwareProvider {
    events: VecDeque<ProviderTurnEvent>,
}

impl ModelProviderInvoker for HistoryAwareProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        let message_count = request.messages.len();
        let metadata_topic = request
            .metadata
            .get("topic")
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());
        self.events = VecDeque::from([
            ProviderTurnEvent::TextDelta {
                text: format!(
                    "saw {message_count} prior messages about {metadata_topic}; prompt: {}",
                    last_user_text(request)
                ),
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::EndTurn,
            },
        ]);
        Box::pin(async {
            Ok(StartTurnResponse {
                provider_turn_id: "history-aware-turn".to_string(),
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
    let messages = vec![
        text_message(MessageRole::User, "What is bcode?"),
        text_message(
            MessageRole::Assistant,
            "Bcode is a Rust-native coding agent.",
        ),
    ];

    let agent = bcode::Agent::builder()
        .system("Answer using the prior conversation when useful.")
        .model("history-demo")
        .metadata("topic", "SDK history")
        .timeout(std::time::Duration::from_secs(30))
        .build();

    let mut provider = HistoryAwareProvider::default();
    let response = agent
        .generate_text_with_provider_and_messages(
            &mut provider,
            "Summarize the conversation.",
            messages.clone(),
        )
        .await?;
    println!("{}", response.text);
    println!("latency={}ms", response.runtime.latency_ms);

    let mut helper_provider = HistoryAwareProvider::default();
    let helper_response = bcode::generate_text_with_messages(
        &mut helper_provider,
        messages,
        "Summarize with the top-level helper.",
    )
    .await?;
    println!("{}", helper_response.text);

    Ok(())
}

fn text_message(role: MessageRole, text: impl Into<String>) -> ModelMessage {
    ModelMessage {
        role,
        content: vec![ModelContentBlock::Text { text: text.into() }],
    }
}

fn last_user_text(request: &ModelTurnRequest) -> String {
    request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == MessageRole::User)
        .and_then(|message| {
            message.content.iter().find_map(|block| match block {
                ModelContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
        })
        .unwrap_or_default()
}
