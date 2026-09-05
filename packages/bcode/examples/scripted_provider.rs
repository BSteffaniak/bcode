use bcode::{
    AgentBuilder, AgentRuntime, ProviderRequestIdentity, ProviderTurnEvent, TokenUsage, testing::*,
};
use std::sync::Arc;
use std::time::Duration;

#[cfg(not(feature = "simulation-example"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = switchy::unsync::Builder::new().build()?;
    let result = runtime.block_on(run());
    runtime.wait()?;
    result?;
    Ok(())
}

#[cfg(feature = "simulation-example")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The harness, not application code, advances simulated time. One poll per step
    // is an explicit exploration policy, not a claim of exhaustive schedule coverage.
    switchy::time::simulator::reset_step();
    let runtime = switchy::unsync::Builder::new().build()?;
    let mut task = runtime.spawn(run());
    for _ in 0..10_000 {
        runtime.tick();
        if task.is_finished() {
            let result = runtime.block_on(task)?;
            // Do not call unbounded `wait`: runtime-wide bounded draining is not
            // exposed upstream yet. This diagnostic example runs once per process
            // and does not certify cleanup or in-process run isolation.
            result?;
            return Ok(());
        }
        let _ = switchy::time::simulator::next_step();
    }
    task.abort();
    // Request cancellation; a single poll is not proof of runtime-wide cleanup.
    runtime.tick();
    Err("simulation harness step budget exhausted (not a product timeout)".into())
}

async fn run() -> bcode::Result<()> {
    let provider = ScriptedProvider::new([ScriptedProviderTurn::new()
        .events([
            ProviderTurnEvent::TurnStarted,
            ProviderTurnEvent::Warning {
                message: "deterministic warning".to_string(),
            },
            ProviderTurnEvent::Usage {
                usage: TokenUsage {
                    input_tokens: Some(2),
                    output_tokens: Some(1),
                    total_tokens: Some(3),
                    ..TokenUsage::default()
                },
            },
        ])
        .delay(Duration::from_millis(1))
        .events([
            ProviderTurnEvent::TextDelta {
                text: "scripted answer".to_string(),
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: bcode::StopReason::EndTurn,
            },
        ])]);
    let probe = provider.probe();
    let session_id = "00000000-0000-4000-8000-000000000001"
        .parse()
        .expect("fixture ID");
    let identities = ScriptedRequestIdentities::new([ProviderRequestIdentity {
        session_id,
        turn_id: "scripted-turn-0".to_string(),
    }])?;
    let agent = AgentBuilder::from_context(session_id, "/".into())
        .runtime(AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)))
        .provider_plugin("test-provider")
        .model("test-model")
        .build();

    let response = agent.run(&mut provider.clone(), "hello").await?;
    assert_eq!(response.text, "scripted answer");
    probe
        .assert_requests(&[ScriptedRequestExpectation::new()
            .provider_plugin_id("test-provider")
            .model_id("test-model")])
        .expect("captured request");
    probe.assert_finish_count(1).expect("provider cleanup");
    run_terminal_scenarios().await?;
    run_tool_scenarios().await?;
    Ok(())
}

async fn run_tool_scenarios() -> bcode::Result<()> {
    for allowed in [true, false] {
        let session_id = "00000000-0000-4000-8000-000000000003"
            .parse()
            .expect("fixture ID");
        let identities =
            ScriptedRequestIdentities::new((0..2).map(|index| ProviderRequestIdentity {
                session_id,
                turn_id: format!("tool-{allowed}-{index}"),
            }))?;
        let permissions = ScriptedPermissionPolicy::new([if allowed {
            bcode::PermissionDecision::Allow
        } else {
            bcode::PermissionDecision::Deny("fixture denial".into())
        }]);
        let permission_probe = permissions.clone();
        let tool = ScriptedTool::new([ScriptedToolOutcome::text("tool output")]);
        let tool_probe = tool.probe();
        let agent = tool
            .register(
                AgentBuilder::from_context(session_id, "/".into())
                    .runtime(
                        AgentRuntime::new()
                            .with_provider_request_identity_source(Arc::new(identities)),
                    )
                    .provider_plugin("test-provider")
                    .model("test-model"),
                bcode::ToolDefinition {
                    name: "scripted".into(),
                    description: "Fixture tool".into(),
                    input_schema: serde_json::json!({"type": "object"}),
                },
            )
            .custom_permission_policy(permissions)
            .build();
        let mut provider = ScriptedProvider::new([
            ScriptedProviderTurn::new().events([
                ProviderTurnEvent::ToolCallFinished {
                    call: bcode::ToolCall {
                        id: "call-1".into(),
                        name: "scripted".into(),
                        arguments: serde_json::json!({"input": 1}),
                    },
                },
                ProviderTurnEvent::TurnFinished {
                    stop_reason: bcode::StopReason::ToolCall,
                },
            ]),
            ScriptedProviderTurn::complete_text("after tool"),
        ]);
        let probe = provider.probe();
        let response = agent.run(&mut provider, "use tool").await?;
        assert_eq!(response.text, "after tool");
        assert_eq!(tool_probe.invocation_count(), usize::from(allowed));
        let requests = permission_probe.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].context.session_id, session_id);
        let expected = if allowed {
            "tool output"
        } else {
            "tool execution denied: fixture denial"
        };
        assert!(response.steps.iter().any(|step| matches!(
            step,
            bcode::GenerationStep::ToolResult { result, .. }
                if result.is_error != allowed && result.output == expected
        )));
        if allowed {
            assert_eq!(tool_probe.invocations()[0].request.arguments["input"], 1);
        }
        probe
            .assert_finish_count(2)
            .expect("both provider requests finished");
    }
    Ok(())
}

async fn run_terminal_scenarios() -> bcode::Result<()> {
    for cancelled in [true, false] {
        let session_id = "00000000-0000-4000-8000-000000000002"
            .parse()
            .expect("fixture ID");
        let identities = ScriptedRequestIdentities::new([ProviderRequestIdentity {
            session_id,
            turn_id: format!("terminal-{cancelled}"),
        }])?;
        let timeout = Duration::from_millis(100);
        let agent = AgentBuilder::from_context(session_id, "/".into())
            .runtime(
                AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)),
            )
            .provider_plugin("test-provider")
            .model("test-model")
            .timeout(timeout)
            .build();
        let provider = ScriptedProvider::new([ScriptedProviderTurn::new()
            .events([ProviderTurnEvent::TextDelta {
                text: "before terminal".into(),
            }])
            .pending()]);
        let probe = provider.probe();
        let cancellation = bcode::CancellationToken::new();
        let stream = agent.stream_text_with_provider_and_cancellation(
            provider,
            "hello",
            cancellation.clone(),
        );
        let mut recorder = TextStreamRecorder::new(stream);
        // Consume the runtime start and provider delta before cancelling, so the
        // scenario exercises active provider work rather than pre-start rejection.
        assert_eq!(recorder.consume_up_to(2).await, 2);
        assert!(matches!(
            recorder.items(),
            [bcode::TextStreamItem::Event(bcode::AgentEvent::TurnStarted),
             bcode::TextStreamItem::Event(bcode::AgentEvent::TextDelta(text))]
                if text == "before terminal"
        ));
        let transcript = if cancelled {
            recorder.cancel_and_finish(&cancellation).await
        } else {
            recorder.finish().await
        };
        transcript
            .assert_terminal_coherence()
            .expect("one stable terminal followed by stream exhaustion");
        let expected_events = [
            bcode::AgentEvent::TurnStarted,
            bcode::AgentEvent::TextDelta("before terminal".into()),
        ];
        // This SDK surface reports cancellation through the typed terminal error,
        // not an additional AgentEvent::Cancelled notification.
        transcript
            .assert_event_order(&expected_events)
            .expect("exact terminal event sequence");
        if cancelled {
            transcript.assert_cancelled().expect("typed cancellation");
        } else {
            assert!(matches!(
                transcript.assert_runtime_error().expect("typed timeout"),
                bcode::RuntimeError::Timeout { timeout: actual } if *actual == timeout
            ));
        }
        probe
            .assert_cancellation_count(1)
            .expect("provider cancelled");
        probe
            .assert_finish_count(1)
            .expect("provider finished once");
    }
    Ok(())
}
