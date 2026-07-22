#![cfg(feature = "testing")]

use bcode::{
    Agent, CancellationToken, ModelPricingInfo, ModelPricingSource, ModelPricingUnit,
    ModelResponseCache, ModelTokenPrice, PermissionDecision, ProviderError, ProviderErrorCategory,
    ProviderTurnEvent, RetryPolicy, StopReason, TokenUsage, ToolCall, ToolDefinition,
    ToolInvocationResponse, ToolPolicyMetadata, ToolSideEffect, ToolUiMetadata,
    generate_text_builder,
    testing::{
        ScriptedModelResponseCache, ScriptedPermissionPolicy, ScriptedProvider,
        ScriptedProviderTurn,
    },
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{Layer, Registry};

#[derive(Debug, Clone)]
struct Record {
    kind: &'static str,
    name: String,
    fields: BTreeMap<String, String>,
    scope: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct CaptureLayer {
    records: Arc<Mutex<Vec<Record>>>,
}

#[derive(Default)]
struct FieldVisitor(BTreeMap<String, String>);

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

impl<S> Layer<S> for CaptureLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(
        &self,
        attributes: &tracing::span::Attributes<'_>,
        _id: &tracing::Id,
        _context: Context<'_, S>,
    ) {
        if attributes.metadata().target() != "bcode::sdk" {
            return;
        }
        let mut visitor = FieldVisitor::default();
        attributes.record(&mut visitor);
        self.records.lock().expect("records lock").push(Record {
            kind: "span",
            name: attributes.metadata().name().to_string(),
            fields: visitor.0,
            scope: Vec::new(),
        });
    }

    fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
        if event.metadata().target() != "bcode::sdk" {
            return;
        }
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let scope = context.event_scope(event).map_or_else(Vec::new, |scope| {
            scope
                .from_root()
                .map(|span| span.metadata().name().to_string())
                .collect()
        });
        self.records.lock().expect("records lock").push(Record {
            kind: "event",
            name: event.metadata().name().to_string(),
            fields: visitor.0,
            scope,
        });
    }
}

fn provider_error() -> ProviderError {
    ProviderError {
        code: "temporary".to_string(),
        category: ProviderErrorCategory::Network,
        message: "temporary".to_string(),
        retryable: true,
        provider_message: None,
        failure: None,
        request_id: None,
        diagnostic_context: Box::default(),
        sources: Box::default(),
        retry: None,
    }
}

fn tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "trace_tool".to_string(),
        description: "Trace one tool".to_string(),
        input_schema: serde_json::json!({"type":"object"}),
        side_effect: ToolSideEffect::ReadOnly,
        requires_permission: true,
        policy: ToolPolicyMetadata::default(),
        ui: ToolUiMetadata::default(),
    }
}

fn response(output: &str) -> ToolInvocationResponse {
    ToolInvocationResponse {
        output: output.to_string(),
        is_error: false,
        content: Vec::new(),
        full_output: None,
        host_action: None,
        result: None,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn stable_tracing_contract_covers_request_provider_tool_session_retry_cache_and_cancellation()
{
    let capture = CaptureLayer::default();
    let records = Arc::clone(&capture.records);
    let subscriber = Registry::default().with(capture);
    let _guard = tracing::subscriber::set_default(subscriber);

    let provider = ScriptedProvider::new([
        ScriptedProviderTurn::start_error(provider_error()),
        ScriptedProviderTurn::new().events([
            ProviderTurnEvent::ToolCallFinished {
                call: ToolCall {
                    id: "trace-call".to_string(),
                    name: "trace_tool".to_string(),
                    arguments: serde_json::json!({}),
                },
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::ToolCall,
            },
        ]),
        ScriptedProviderTurn::new().events([
            ProviderTurnEvent::TextDelta {
                text: "traced answer".to_string(),
            },
            ProviderTurnEvent::Usage {
                usage: TokenUsage {
                    input_tokens: Some(3),
                    output_tokens: Some(5),
                    total_tokens: Some(8),
                    ..TokenUsage::default()
                },
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::EndTurn,
            },
        ]),
    ]);
    let cache = Arc::new(ScriptedModelResponseCache::new().with_tool_responses(true));
    let agent = Agent::builder()
        .provider_plugin("trace-provider")
        .model("trace-model")
        .model_pricing(ModelPricingInfo {
            currency: "USD".to_string(),
            unit: ModelPricingUnit::PerMillionTokens,
            input: Some(ModelTokenPrice::from_micros(1_000_000)),
            cached_input: None,
            cache_write_input: None,
            output: Some(ModelTokenPrice::from_micros(1_000_000)),
            source: ModelPricingSource::UserOverride,
        })
        .retry_policy(RetryPolicy::new(1, Duration::ZERO))
        .custom_permission_policy(ScriptedPermissionPolicy::new([PermissionDecision::Allow]))
        .inline_tool(tool_definition(), |_| Ok(response("tool output")))
        .response_cache(cache.clone())
        .build();
    let first = agent
        .run(&mut provider.clone(), "trace request")
        .await
        .expect("traced turn");
    assert_eq!(first.text, "traced answer");
    let second = generate_text_builder()
        .prompt("trace request")
        .configure_agent(|builder| {
            builder
                .provider_plugin("trace-provider")
                .model("trace-model")
                .model_pricing(ModelPricingInfo {
                    currency: "USD".to_string(),
                    unit: ModelPricingUnit::PerMillionTokens,
                    input: Some(ModelTokenPrice::from_micros(1_000_000)),
                    cached_input: None,
                    cache_write_input: None,
                    output: Some(ModelTokenPrice::from_micros(1_000_000)),
                    source: ModelPricingSource::UserOverride,
                })
                .retry_policy(RetryPolicy::new(1, Duration::ZERO))
                .custom_permission_policy(ScriptedPermissionPolicy::new([
                    PermissionDecision::Allow,
                ]))
                .inline_tool(tool_definition(), |_| Ok(response("tool output")))
        })
        .response_cache(cache as Arc<dyn ModelResponseCache>)
        .run(&mut ScriptedProvider::new([]))
        .await
        .expect("cache hit");
    assert_eq!(second.text, "traced answer");

    let mut session = Agent::builder()
        .provider_plugin("trace-provider")
        .model("trace-model")
        .build()
        .session();
    session
        .generate_text_with_provider(
            &mut ScriptedProvider::new([ScriptedProviderTurn::complete_text("session answer")]),
            "session request",
        )
        .await
        .expect("session turn");

    let cancellation = CancellationToken::new();
    let mut stream = agent.stream_text_with_provider_and_cancellation(
        ScriptedProvider::new([ScriptedProviderTurn::new()
            .events([ProviderTurnEvent::TurnStarted])
            .pending()]),
        "cancel request",
        cancellation.clone(),
    );
    let _ = stream.next().await;
    cancellation.cancel();
    while stream.next().await.is_some() {}

    let records = records.lock().expect("records lock");
    let span_names = records
        .iter()
        .filter(|record| record.kind == "span")
        .map(|record| record.name.as_str())
        .collect::<Vec<_>>();
    for expected in [
        "bcode.model_request",
        "bcode.agent_turn",
        "bcode.provider_round",
        "bcode.provider_operation",
        "bcode.tool_batch",
        "bcode.tool_call",
        "bcode.session_turn",
    ] {
        assert!(span_names.contains(&expected), "missing span {expected}");
    }
    assert!(records.iter().any(|record| {
        record.name == "bcode.model_request"
            && record
                .fields
                .get("provider_id")
                .is_some_and(|value| value.contains("trace-provider"))
            && record
                .fields
                .get("model_id")
                .is_some_and(|value| value.contains("trace-model"))
            && record.fields.contains_key("session_id")
    }));
    assert!(records.iter().any(|record| {
        record.name == "bcode.tool_call"
            && record
                .fields
                .get("tool_call_id")
                .is_some_and(|value| value.contains("trace-call"))
            && record
                .fields
                .get("tool_name")
                .is_some_and(|value| value.contains("trace_tool"))
    }));
    for event in [
        "bcode.retry_scheduled",
        "bcode.cache_lookup",
        "bcode.cache_store",
        "bcode.usage",
        "bcode.cost_estimate",
        "bcode.error",
        "bcode.cancellation",
    ] {
        assert!(
            records.iter().any(|record| {
                record.kind == "event"
                    && record
                        .fields
                        .get("event")
                        .is_some_and(|value| value.contains(event))
            }),
            "missing event {event}"
        );
    }
    assert!(records.iter().any(|record| {
        record.kind == "event"
            && record
                .fields
                .get("event")
                .is_some_and(|value| value.contains("bcode.cancellation"))
            && record
                .scope
                .iter()
                .any(|span| span == "bcode.model_request")
    }));
}
