use bcode::{
    FallbackPolicy, ModelProviderInvoker, RuntimeFuture, StopReason, generate_text_builder,
};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse,
};

#[derive(Debug, Default)]
struct RoutedProvider {
    starts: Vec<(Option<String>, String)>,
    events: Vec<ProviderTurnEvent>,
}

impl ModelProviderInvoker for RoutedProvider {
    fn start_turn<'a>(
        &'a mut self,
        provider_plugin_id: Option<&'a str>,
        request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        self.starts.push((
            provider_plugin_id.map(str::to_string),
            request.model_id.clone(),
        ));
        Box::pin(async move {
            if provider_plugin_id == Some("primary") {
                Err(bcode::RuntimeError::ProviderInvocation(
                    "primary unavailable".to_string(),
                ))
            } else {
                self.events = vec![
                    ProviderTurnEvent::TextDelta {
                        text: "fallback response".to_string(),
                    },
                    ProviderTurnEvent::TurnFinished {
                        stop_reason: StopReason::EndTurn,
                    },
                ];
                Ok(StartTurnResponse {
                    provider_turn_id: "fallback-turn".to_string(),
                })
            }
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
async fn fallback_policy_switches_provider_and_model_in_order() {
    let mut provider = RoutedProvider::default();

    let response = generate_text_builder()
        .model("primary:primary-model")
        .prompt("route")
        .fallback_policy(FallbackPolicy::new().fallback("secondary:fallback-model"))
        .run(&mut provider)
        .await
        .expect("configured fallback should recover");

    assert_eq!(
        provider.starts,
        [
            (Some("primary".to_string()), "primary-model".to_string()),
            (Some("secondary".to_string()), "fallback-model".to_string())
        ]
    );
    assert_eq!(response.text, "fallback response");
}
