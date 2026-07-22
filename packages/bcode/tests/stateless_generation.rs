use bcode::{ModelProviderInvoker, RuntimeFuture, generate_text};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse, StopReason,
};

#[derive(Debug, Default)]
struct StatelessProvider {
    requests: Vec<ModelTurnRequest>,
    events: Vec<ProviderTurnEvent>,
    next_turn: usize,
}

impl ModelProviderInvoker for StatelessProvider {
    fn start_turn<'a>(
        &'a mut self,
        provider_plugin_id: Option<&'a str>,
        request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        assert!(provider_plugin_id.is_none());
        self.requests.push(request.clone());
        self.next_turn += 1;
        self.events = vec![
            ProviderTurnEvent::TextDelta {
                text: format!("response {}", self.next_turn),
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::EndTurn,
            },
        ];
        let provider_turn_id = format!("stateless-turn-{}", self.next_turn);
        Box::pin(async move { Ok(StartTurnResponse { provider_turn_id }) })
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
async fn generation_helper_has_no_hidden_transcript_or_session_state() {
    let mut provider = StatelessProvider::default();

    let first = generate_text(&mut provider, "first prompt")
        .await
        .expect("first stateless request");
    let second = generate_text(&mut provider, "second prompt")
        .await
        .expect("second stateless request");

    assert_eq!(first.text, "response 1");
    assert_eq!(second.text, "response 2");
    assert_eq!(provider.requests.len(), 2);
    assert_eq!(provider.requests[0].messages.len(), 1);
    assert_eq!(provider.requests[1].messages.len(), 1);
    assert!(matches!(
        &provider.requests[0].messages[0].content[..],
        [bcode::ModelContentBlock::Text { text }] if text == "first prompt"
    ));
    assert!(matches!(
        &provider.requests[1].messages[0].content[..],
        [bcode::ModelContentBlock::Text { text }] if text == "second prompt"
    ));
}
