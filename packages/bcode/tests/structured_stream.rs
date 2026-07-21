use bcode::{
    BcodeError, ModelProviderInvoker, ObjectStreamItem, RuntimeFuture, StopReason,
    StructuredOutputOptions, stream_object_builder,
};
use bcode_model::{
    AckResponse, CancelTurnRequest, FinishTurnRequest, ModelTurnRequest, PollTurnEventsRequest,
    PollTurnEventsResponse, ProviderTurnEvent, StartTurnResponse,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::collections::VecDeque;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Debug, Deserialize, JsonSchema, PartialEq, Eq)]
struct Output {
    name: String,
    count: u32,
}

#[derive(Debug)]
struct ScriptedProvider {
    events: VecDeque<ProviderTurnEvent>,
    starts: Arc<AtomicUsize>,
}

impl ScriptedProvider {
    fn new(chunks: &[&str], starts: Arc<AtomicUsize>) -> Self {
        let mut events = chunks
            .iter()
            .map(|text| ProviderTurnEvent::TextDelta {
                text: (*text).to_string(),
            })
            .collect::<VecDeque<_>>();
        events.push_back(ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::EndTurn,
        });
        Self { events, starts }
    }
}

impl ModelProviderInvoker for ScriptedProvider {
    fn start_turn<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a ModelTurnRequest,
    ) -> RuntimeFuture<'a, StartTurnResponse> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(StartTurnResponse {
                provider_turn_id: "structured-stream".to_string(),
            })
        })
    }

    fn poll_turn_events<'a>(
        &'a mut self,
        _provider_plugin_id: Option<&'a str>,
        _request: &'a PollTurnEventsRequest,
    ) -> RuntimeFuture<'a, PollTurnEventsResponse> {
        let events = self.events.drain(..).collect();
        Box::pin(async move { Ok(PollTurnEventsResponse { events }) })
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
async fn structured_stream_emits_changed_partial_and_validated_states() {
    let starts = Arc::new(AtomicUsize::new(0));
    let provider = ScriptedProvider::new(
        &[r#"{"name":"Al"#, r#"ice","count":"#, "2}"],
        Arc::clone(&starts),
    );
    let mut stream = stream_object_builder::<Output>()
        .prompt("produce output")
        .run(provider);
    let mut raw = Vec::new();
    let mut partials = Vec::new();
    let mut validated = Vec::new();
    let mut finished = None;

    while let Some(item) = stream.next().await {
        match item {
            ObjectStreamItem::RawDelta(delta) => raw.push(delta),
            ObjectStreamItem::Partial(value) => partials.push(value),
            ObjectStreamItem::ValidatedPartial(value) => validated.push(value),
            ObjectStreamItem::Finished { object, .. } => finished = Some(object),
            ObjectStreamItem::Error(error) => panic!("stream failed: {error}"),
            ObjectStreamItem::Event(_) | ObjectStreamItem::ScopedEvent(_) => {}
        }
    }

    assert_eq!(raw.len(), 3);
    assert!(
        partials.len() >= 2,
        "incomplete prefixes should produce useful partials"
    );
    assert_eq!(
        validated.last(),
        Some(&serde_json::json!({"name": "Alice", "count": 2}))
    );
    assert_eq!(
        finished,
        Some(Output {
            name: "Alice".to_string(),
            count: 2,
        })
    );
    assert_eq!(starts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn structured_stream_suppresses_unchanged_partial_states() {
    let starts = Arc::new(AtomicUsize::new(0));
    let provider = ScriptedProvider::new(&[r#"{"name":"Alice","count":2}"#, "   "], starts);
    let mut stream = stream_object_builder::<Output>()
        .prompt("produce output")
        .run(provider);
    let mut partial_count = 0;
    let mut validated_count = 0;

    while let Some(item) = stream.next().await {
        match item {
            ObjectStreamItem::Partial(_) => partial_count += 1,
            ObjectStreamItem::ValidatedPartial(_) => validated_count += 1,
            ObjectStreamItem::Error(error) => panic!("stream failed: {error}"),
            _ => {}
        }
    }

    assert_eq!(partial_count, 1);
    assert_eq!(validated_count, 1);
}

#[tokio::test]
async fn syntactically_invalid_prefix_is_not_repaired_into_partial_state() {
    let starts = Arc::new(AtomicUsize::new(0));
    let provider = ScriptedProvider::new(&["{name:"], starts);
    let mut stream = stream_object_builder::<Output>()
        .prompt("produce output")
        .run(provider);
    let mut saw_partial = false;
    let mut terminal_error = None;

    while let Some(item) = stream.next().await {
        match item {
            ObjectStreamItem::Partial(_) | ObjectStreamItem::ValidatedPartial(_) => {
                saw_partial = true;
            }
            ObjectStreamItem::Error(error) => terminal_error = Some(error),
            _ => {}
        }
    }

    assert!(!saw_partial);
    assert!(matches!(
        terminal_error,
        Some(BcodeError::StructuredInvalidJson(_))
    ));
}

#[tokio::test]
async fn unsatisfiable_stream_schema_never_emits_validated_partial_and_fails_final_value() {
    let starts = Arc::new(AtomicUsize::new(0));
    let provider = ScriptedProvider::new(&[r#"{"name":"Alice","count":2}"#], starts);
    let options =
        StructuredOutputOptions::json_schema("Impossible", serde_json::json!({"not": {}}));
    let mut stream = stream_object_builder::<Output>()
        .prompt("produce output")
        .run_with_options(provider, options);
    let mut saw_partial = false;
    let mut saw_validated = false;
    let mut terminal_error = None;

    while let Some(item) = stream.next().await {
        match item {
            ObjectStreamItem::Partial(_) => saw_partial = true,
            ObjectStreamItem::ValidatedPartial(_) => saw_validated = true,
            ObjectStreamItem::Error(error) => terminal_error = Some(error),
            _ => {}
        }
    }

    assert!(saw_partial);
    assert!(!saw_validated);
    assert!(matches!(
        terminal_error,
        Some(BcodeError::StructuredSchemaValidation(_))
    ));
}

#[tokio::test]
async fn streaming_repairs_are_rejected_before_provider_invocation() {
    let starts = Arc::new(AtomicUsize::new(0));
    let provider = ScriptedProvider::new(&[r#"{"name":"Alice","count":2}"#], Arc::clone(&starts));
    let options = StructuredOutputOptions::for_type::<Output>().with_max_repairs(1);
    let mut stream = stream_object_builder::<Output>()
        .prompt("produce output")
        .run_with_options(provider, options);

    assert!(matches!(
        stream.next().await,
        Some(ObjectStreamItem::Error(
            BcodeError::StructuredStreamingRepairsUnsupported { max_repairs: 1 }
        ))
    ));
    assert!(stream.next().await.is_none());
    assert_eq!(starts.load(Ordering::SeqCst), 0);
}
