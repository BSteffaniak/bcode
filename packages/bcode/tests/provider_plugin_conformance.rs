#![cfg(all(
    feature = "embedded-plugins",
    feature = "static-bundled-fake-provider-plugin"
))]

use bcode::{Bcode, ModelSelector, ToolExecutionOptions};
use bcode_model::{ModelCapability, ProviderCapability, StopReason};
use bcode_plugin::{PluginRuntimeHost, PluginSelection};
use std::num::NonZeroUsize;

fn fake_bcode() -> Bcode {
    let plugins = PluginRuntimeHost::load_defaults_with_static_bundled(
        &PluginSelection::all_enabled(),
        &bcode_bundled_plugins::static_bundled_plugins(),
    )
    .expect("load static fake provider");
    Bcode::builder()
        .plugin_runtime(plugins)
        .provider("bcode.fake-provider")
        .default_model(ModelSelector::with_provider(
            "bcode.fake-provider",
            "fake-echo",
        ))
        .build()
}

#[tokio::test]
async fn static_provider_adapter_conforms_for_multiple_calls_and_sequential_fallback() {
    let bcode = fake_bcode();
    let capabilities = bcode
        .provider_capabilities("bcode.fake-provider")
        .await
        .expect("provider capabilities");
    assert!(
        capabilities
            .capabilities
            .contains(&ProviderCapability::ParallelToolCalls)
    );
    let models = bcode
        .provider_models("bcode.fake-provider")
        .await
        .expect("provider models");
    assert!(
        models.models[0]
            .capabilities
            .contains(&ModelCapability::ParallelToolCalls)
    );

    let parallel = bcode
        .agent()
        .provider_context(bcode_model::ProviderRequestContext {
            settings: [("fake_parallel_tool_calls".to_owned(), "2".to_owned())]
                .into_iter()
                .collect(),
            ..Default::default()
        })
        .execution_options(ToolExecutionOptions {
            max_concurrency: NonZeroUsize::new(2),
            ..ToolExecutionOptions::default()
        })
        .inline_tool(bcode_tool_definition("first"), |_| {
            Ok(bcode::ToolInvocationResponse {
                output: "first".to_owned(),
                is_error: false,
                content: Vec::new(),
                full_output: None,
                host_action: None,
                result: None,
            })
        })
        .inline_tool(bcode_tool_definition("second"), |_| {
            Ok(bcode::ToolInvocationResponse {
                output: "second".to_owned(),
                is_error: false,
                content: Vec::new(),
                full_output: None,
                host_action: None,
                result: None,
            })
        })
        .build();
    bcode_fake_provider_plugin::reset_fake_compaction_started();
    let response = parallel
        .generate_text("multiple")
        .await
        .expect("parallel fake provider round");
    let ids = response
        .runtime
        .events
        .iter()
        .filter_map(|event| match event {
            bcode::AgentEvent::ToolCallFinished(call) => Some(call.id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(ids, ["fake-call-0", "fake-call-1"]);
    assert!(bcode_fake_provider_plugin::fake_last_parallel_tool_policy());

    let sequential = fake_bcode()
        .agent()
        .provider_context(bcode_model::ProviderRequestContext {
            settings: [("fake_parallel_tool_calls".to_owned(), "2".to_owned())]
                .into_iter()
                .collect(),
            ..Default::default()
        })
        .execution_options(ToolExecutionOptions {
            parallel: false,
            ..ToolExecutionOptions::default()
        })
        .inline_tool(bcode_tool_definition("first"), |_| {
            Ok(bcode::ToolInvocationResponse {
                output: "first".to_owned(),
                is_error: false,
                content: Vec::new(),
                full_output: None,
                host_action: None,
                result: None,
            })
        })
        .inline_tool(bcode_tool_definition("second"), |_| {
            Ok(bcode::ToolInvocationResponse {
                output: "second".to_owned(),
                is_error: false,
                content: Vec::new(),
                full_output: None,
                host_action: None,
                result: None,
            })
        })
        .build();
    bcode_fake_provider_plugin::reset_fake_compaction_started();
    assert_eq!(
        sequential
            .generate_text("sequential")
            .await
            .expect("sequential fallback")
            .runtime
            .stop_reason,
        Some(StopReason::EndTurn)
    );
    assert!(!bcode_fake_provider_plugin::fake_last_parallel_tool_policy());
}

#[tokio::test]
async fn static_provider_adapter_conforms_for_malformed_calls_and_cancellation() {
    let malformed = fake_bcode()
        .agent()
        .provider_context(bcode_model::ProviderRequestContext {
            settings: [("fake_malformed_tool_call".to_owned(), "true".to_owned())]
                .into_iter()
                .collect(),
            ..Default::default()
        })
        .build()
        .generate_text("malformed")
        .await
        .expect_err("malformed provider call must fail");
    assert!(malformed.to_string().contains("malformed tool call"));

    let agent = fake_bcode()
        .agent()
        .provider_context(bcode_model::ProviderRequestContext {
            settings: [("fake_turn_delay_ms".to_owned(), "1000".to_owned())]
                .into_iter()
                .collect(),
            ..Default::default()
        })
        .build();
    let cancellation = bcode::CancellationToken::new();
    let mut stream = agent
        .stream_text_with_cancellation("cancel", cancellation.clone())
        .expect("start cancellable stream");
    cancellation.cancel();
    let mut cancelled = false;
    while let Some(item) = stream.next().await {
        if matches!(
            item,
            bcode::TextStreamItem::Error(bcode::BcodeError::Runtime(
                bcode::RuntimeError::Cancelled
            ))
        ) {
            cancelled = true;
            break;
        }
    }
    assert!(cancelled);
}

fn bcode_tool_definition(name: &str) -> bcode::ToolDefinition {
    bcode::ToolDefinition {
        name: name.to_owned(),
        description: name.to_owned(),
        input_schema: serde_json::json!({"type": "object"}),
        side_effect: bcode::ToolSideEffect::ReadOnly,
        requires_permission: false,
        policy: bcode::ToolPolicyMetadata::default(),
        ui: bcode::ToolUiMetadata::default(),
    }
}
