use bcode::{
    AgentTurnRequest, GenerateTextResponse, ModelProviderInvoker, ModelResponseCache,
    RuntimeFuture, StopReason, generate_text_builder,
};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse,
};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
struct MemoryCache {
    response: Mutex<Option<GenerateTextResponse>>,
    puts: Mutex<u32>,
}

impl ModelResponseCache for MemoryCache {
    fn get(&self, _request: &AgentTurnRequest) -> bcode::Result<Option<GenerateTextResponse>> {
        Ok(self
            .response
            .lock()
            .expect("cache lock should be available")
            .clone())
    }

    fn put(
        &self,
        _request: &AgentTurnRequest,
        response: &GenerateTextResponse,
    ) -> bcode::Result<()> {
        *self
            .response
            .lock()
            .expect("cache lock should be available") = Some(response.clone());
        *self.puts.lock().expect("put lock should be available") += 1;
        Ok(())
    }
}

#[derive(Debug, Default)]
struct CountingProvider {
    starts: u32,
    events: Vec<ProviderTurnEvent>,
}

impl ModelProviderInvoker for CountingProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        self.starts += 1;
        self.events = vec![
            ProviderTurnEvent::TextDelta {
                text: "cached response".to_string(),
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::EndTurn,
            },
        ];
        Box::pin(async {
            Ok(StartTurnResponse {
                provider_turn_id: "cache-turn".to_string(),
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
async fn response_cache_short_circuits_provider_after_first_response() {
    let cache = Arc::new(MemoryCache::default());
    let mut provider = CountingProvider::default();

    let first = generate_text_builder()
        .prompt("cache me")
        .response_cache(cache.clone())
        .run(&mut provider)
        .await
        .expect("cache miss should invoke provider");
    let second = generate_text_builder()
        .prompt("cache me")
        .response_cache(cache.clone())
        .run(&mut provider)
        .await
        .expect("cache hit should return response");

    assert_eq!(provider.starts, 1);
    assert_eq!(first.text, second.text);
    assert_eq!(*cache.puts.lock().expect("put lock should be available"), 1);
}
