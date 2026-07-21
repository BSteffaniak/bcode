use bcode::{ModelProviderInvoker, RetryPolicy, RuntimeFuture, StopReason, generate_text_builder};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse,
};
use std::time::Duration;

#[derive(Debug, Default)]
struct FlakyProvider {
    starts: u32,
    events: Vec<ProviderTurnEvent>,
}

impl ModelProviderInvoker for FlakyProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        self.starts += 1;
        Box::pin(async move {
            if self.starts == 1 {
                Err(bcode::RuntimeError::ProviderInvocation(
                    "temporary failure".to_string(),
                ))
            } else {
                self.events = vec![
                    ProviderTurnEvent::TextDelta {
                        text: "recovered".to_string(),
                    },
                    ProviderTurnEvent::TurnFinished {
                        stop_reason: StopReason::EndTurn,
                    },
                ];
                Ok(StartTurnResponse {
                    provider_turn_id: format!("retry-turn-{}", self.starts),
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
async fn retry_policy_retries_provider_failures_within_bound() {
    let mut provider = FlakyProvider::default();

    let response = generate_text_builder()
        .prompt("retry")
        .retry_policy(RetryPolicy::new(1, Duration::ZERO))
        .run(&mut provider)
        .await
        .expect("one configured retry should recover");

    assert_eq!(provider.starts, 2);
    assert_eq!(response.text, "recovered");
}

#[tokio::test]
async fn retry_policy_preserves_failure_when_bound_is_zero() {
    let mut provider = FlakyProvider::default();

    let error = generate_text_builder()
        .prompt("do not retry")
        .retry_policy(RetryPolicy::new(0, Duration::ZERO))
        .run(&mut provider)
        .await
        .expect_err("zero retries should preserve the first failure");

    assert_eq!(provider.starts, 1);
    assert!(error.to_string().contains("temporary failure"));
}
