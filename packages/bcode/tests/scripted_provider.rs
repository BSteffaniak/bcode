#![cfg(feature = "testing")]

use bcode::{
    Agent, BcodeError, CancellationToken, FallbackPolicy, ModelMiddleware, ModelSelector,
    ProviderError, ProviderErrorCategory, ProviderTurnEvent, RetryPolicy, StopReason, ToolCall,
    generate_text_builder, testing::*,
};
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Debug)]
struct MetadataMiddleware;

impl ModelMiddleware for MetadataMiddleware {
    fn before_request(
        &self,
        mut request: bcode::AgentTurnRequest,
    ) -> bcode::Result<bcode::AgentTurnRequest> {
        request
            .metadata
            .insert("middleware".to_string(), "applied".to_string());
        Ok(request)
    }
}

fn error(code: &str, category: ProviderErrorCategory, retryable: bool) -> ProviderError {
    ProviderError {
        code: code.to_string(),
        category,
        message: code.to_string(),
        retryable,
        provider_message: None,
        failure: None,
        request_id: None,
        diagnostic_context: Box::default(),
        sources: Box::default(),
        retry: None,
    }
}

#[tokio::test]
async fn scripted_provider_captures_middleware_output_and_continuation_rounds() {
    let provider = ScriptedProvider::new([
        ScriptedProviderTurn::new().events([
            ProviderTurnEvent::TurnStarted,
            ProviderTurnEvent::ToolCallFinished {
                call: ToolCall {
                    id: "call-1".to_string(),
                    name: "missing".to_string(),
                    arguments: serde_json::json!({"value": 1}),
                },
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: StopReason::ToolCall,
            },
        ]),
        ScriptedProviderTurn::complete_text("complete"),
    ]);
    let probe = provider.probe();
    let agent = Agent::builder()
        .provider_plugin("scripted")
        .model("model-a")
        .middleware_layer(MetadataMiddleware)
        .build();

    let response = agent
        .run(&mut provider.clone(), "hello")
        .await
        .expect("script completes despite model-visible missing-tool result");
    assert_eq!(response.text, "complete");

    let requests = probe.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].request.model_id, "model-a");
    assert_eq!(requests[0].provider_plugin_id.as_deref(), Some("scripted"));
    assert!(requests[1].request.messages.len() > requests[0].request.messages.len());
    let mut expected_metadata = BTreeMap::new();
    expected_metadata.insert("middleware".to_string(), "applied".to_string());
    probe
        .assert_requests(&[
            ScriptedRequestExpectation::new()
                .provider_plugin_id("scripted")
                .model_id("model-a")
                .metadata(expected_metadata.clone()),
            ScriptedRequestExpectation::new()
                .provider_plugin_id("scripted")
                .model_id("model-a")
                .metadata(expected_metadata),
        ])
        .expect("selected request fields match");
    probe
        .assert_finish_count(2)
        .expect("each turn is cleaned up");
}

#[tokio::test]
async fn scripted_delay_uses_tokio_sleep() {
    let provider = ScriptedProvider::new([ScriptedProviderTurn::new()
        .events([ProviderTurnEvent::TurnStarted])
        .delay(Duration::from_millis(1))
        .events([ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::EndTurn,
        }])]);
    let agent = Agent::builder()
        .model("model-a")
        .timeout(Duration::from_secs(60))
        .build();

    let response = agent
        .run(&mut provider.clone(), "hello")
        .await
        .expect("paused Tokio clock advances scripted sleep");
    assert_eq!(response.runtime.stop_reason, Some(StopReason::EndTurn));
}

#[tokio::test]
async fn pending_script_observes_runtime_timeout_cleanup() {
    let provider = ScriptedProvider::new([ScriptedProviderTurn::new()
        .events([ProviderTurnEvent::TurnStarted])
        .pending()]);
    let probe = provider.probe();
    let agent = Agent::builder()
        .model("model-a")
        .timeout(Duration::from_millis(1))
        .build();

    let result = agent.run(&mut provider.clone(), "hello").await;
    assert!(matches!(result, Err(BcodeError::Runtime(_))));
    probe
        .assert_cancellation_count(1)
        .expect("timeout cancels provider turn");
    probe
        .assert_finish_count(1)
        .expect("timeout finishes provider turn");
}

#[tokio::test]
async fn start_and_poll_failures_are_scriptable() {
    let start_provider = ScriptedProvider::new([ScriptedProviderTurn::start_error(error(
        "start_failed",
        ProviderErrorCategory::Network,
        false,
    ))]);
    let agent = Agent::builder().model("model-a").build();
    let start_result = agent.run(&mut start_provider.clone(), "hello").await;
    assert!(start_result.is_err());

    let poll_provider = ScriptedProvider::new([ScriptedProviderTurn::new().poll_error(error(
        "poll_failed",
        ProviderErrorCategory::ProviderInternal,
        false,
    ))]);
    let poll_result = agent.run(&mut poll_provider.clone(), "hello").await;
    assert!(poll_result.is_err());
}

#[tokio::test]
async fn malformed_event_scripts_reach_canonical_validation() {
    let provider = ScriptedProvider::new([ScriptedProviderTurn::new().events([
        ProviderTurnEvent::TurnStarted,
        ProviderTurnEvent::TurnFinished {
            stop_reason: StopReason::ToolCall,
        },
    ])]);
    let agent = Agent::builder().model("model-a").build();

    let result = agent.run(&mut provider.clone(), "hello").await;
    assert!(matches!(result, Err(BcodeError::Runtime(_))));
}

#[test]
fn request_assertions_report_the_exact_field() {
    let provider = ScriptedProvider::new([ScriptedProviderTurn::provider_error(error(
        "terminal",
        ProviderErrorCategory::InvalidRequest,
        false,
    ))]);
    let probe = provider.probe();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let agent = Agent::builder().model("actual").build();
    let _ = runtime.block_on(agent.run(&mut provider.clone(), "hello"));

    let assertion = probe
        .assert_requests(&[ScriptedRequestExpectation::new().model_id("expected")])
        .expect_err("model should differ");
    assert!(matches!(
        assertion,
        ScriptedProviderAssertionError::FieldMismatch {
            field: "model_id",
            ..
        }
    ));
}

#[tokio::test]
async fn scripted_provider_supports_retry_scenarios() {
    let provider = ScriptedProvider::new([
        ScriptedProviderTurn::start_error(error("temporary", ProviderErrorCategory::Network, true)),
        ScriptedProviderTurn::complete_text("recovered"),
    ]);
    let probe = provider.probe();
    let agent = Agent::builder()
        .model("model-a")
        .retry_policy(RetryPolicy::new(1, Duration::ZERO))
        .build();

    let response = agent
        .run(&mut provider.clone(), "hello")
        .await
        .expect("retry consumes the second scripted turn");
    assert_eq!(response.text, "recovered");
    probe
        .assert_requests(&[
            ScriptedRequestExpectation::new().model_id("model-a"),
            ScriptedRequestExpectation::new().model_id("model-a"),
        ])
        .expect("both attempts captured");
    probe
        .assert_finish_count(1)
        .expect("only the successfully started turn needs finish cleanup");
}

#[tokio::test]
async fn scripted_provider_supports_fallback_scenarios_and_selection_assertions() {
    let provider = ScriptedProvider::new([
        ScriptedProviderTurn::start_error(error(
            "primary_down",
            ProviderErrorCategory::Network,
            false,
        )),
        ScriptedProviderTurn::complete_text("fallback"),
    ]);
    let probe = provider.probe();
    let agent = Agent::builder()
        .provider_plugin("primary")
        .model("model-a")
        .fallback_policy(
            FallbackPolicy::new().fallback(ModelSelector::with_provider("secondary", "model-b")),
        )
        .build();

    let response = agent
        .run(&mut provider.clone(), "hello")
        .await
        .expect("fallback consumes the second scripted turn");
    assert_eq!(response.text, "fallback");
    probe
        .assert_requests(&[
            ScriptedRequestExpectation::new()
                .provider_plugin_id("primary")
                .model_id("model-a"),
            ScriptedRequestExpectation::new()
                .provider_plugin_id("secondary")
                .model_id("model-b"),
        ])
        .expect("fallback provider/model selection is captured");
}

#[tokio::test]
async fn pending_script_observes_explicit_cancellation_cleanup() {
    let provider = ScriptedProvider::new([ScriptedProviderTurn::new()
        .events([ProviderTurnEvent::TurnStarted])
        .pending()]);
    let probe = provider.probe();
    let cancellation = CancellationToken::new();
    let cancellation_for_task = cancellation.clone();
    let mut provider_for_task = provider.clone();
    let task = tokio::spawn(async move {
        generate_text_builder()
            .prompt("hello")
            .model("model-a")
            .cancellation(cancellation_for_task)
            .run(&mut provider_for_task)
            .await
    });

    tokio::task::yield_now().await;
    cancellation.cancel();
    let result = task.await.expect("generation task joins");
    assert!(matches!(result, Err(BcodeError::Runtime(_))));
    probe
        .assert_cancellation_count(1)
        .expect("explicit cancellation reaches provider cleanup");
    probe
        .assert_finish_count(1)
        .expect("explicit cancellation finishes provider turn");
}
