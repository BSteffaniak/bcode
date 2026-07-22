use bcode_fake_provider_plugin::FakeProviderPlugin;
use bcode_model_provider_runtime::{
    BlockingModelProviderInvoker, ProviderConformanceError, ProviderConformanceOptions,
    ProviderConformanceOutcome, run_provider_conformance_suite,
};
use bcode_plugin_sdk::{
    ConcurrentRustPlugin, NativeServiceContext, PluginConfigContext, ServiceBridge,
    ServiceCancellation, ServiceEventEmitter, ServiceRequest,
};
use std::time::Duration;

#[derive(Debug, Clone, Copy, Default)]
enum PollBehavior {
    #[default]
    Normal,
    Empty,
    OmitTurnStarted,
}

#[derive(Default)]
struct FakePluginInvoker {
    plugin: FakeProviderPlugin,
    poll_behavior: PollBehavior,
    cancel_calls: usize,
    finish_calls: usize,
}

impl FakePluginInvoker {
    fn with_poll_behavior(poll_behavior: PollBehavior) -> Self {
        Self {
            poll_behavior,
            ..Self::default()
        }
    }
}

impl BlockingModelProviderInvoker for FakePluginInvoker {
    fn invoke_json<Q, R>(
        &mut self,
        _provider_plugin_id: Option<&str>,
        operation: &'static str,
        request: &Q,
    ) -> Result<R, String>
    where
        Q: serde::Serialize,
        R: serde::de::DeserializeOwned,
    {
        if operation == bcode_model::OP_CANCEL_TURN {
            self.cancel_calls += 1;
        } else if operation == bcode_model::OP_FINISH_TURN {
            self.finish_calls += 1;
        }
        let response = self.plugin.invoke_service_concurrent(NativeServiceContext {
            plugin_id: "bcode.fake-provider".to_string(),
            request: ServiceRequest {
                interface_id: bcode_model::MODEL_PROVIDER_INTERFACE_ID.to_string(),
                operation: operation.to_string(),
                payload: serde_json::to_vec(request).map_err(|error| error.to_string())?,
            },
            config: PluginConfigContext::default(),
            events: ServiceEventEmitter::default(),
            cancellation: ServiceCancellation::default(),
            bridge: ServiceBridge::default(),
        });
        if let Some(error) = response.error {
            return Err(format!("{}: {}", error.code, error.message));
        }
        let mut payload = response.payload;
        if operation == bcode_model::OP_POLL_TURN_EVENTS {
            payload = rewrite_poll_payload(&payload, self.poll_behavior)?;
        }
        serde_json::from_slice(&payload).map_err(|error| error.to_string())
    }
}

fn rewrite_poll_payload(payload: &[u8], behavior: PollBehavior) -> Result<Vec<u8>, String> {
    let mut response: bcode_model::PollTurnEventsResponse =
        serde_json::from_slice(payload).map_err(|error| error.to_string())?;
    match behavior {
        PollBehavior::Normal => {}
        PollBehavior::Empty => response.events.clear(),
        PollBehavior::OmitTurnStarted => response
            .events
            .retain(|event| !matches!(event, bcode_model::ProviderTurnEvent::TurnStarted)),
    }
    serde_json::to_vec(&response).map_err(|error| error.to_string())
}

#[test]
fn bundled_fake_provider_passes_reusable_public_conformance_suite() {
    let report = run_provider_conformance_suite(
        &mut FakePluginInvoker::default(),
        &ProviderConformanceOptions::default(),
    )
    .expect("fake provider should satisfy the public provider contract");

    assert_eq!(report.provider.provider_id, "bcode.fake-provider");
    assert_eq!(report.model.model_id, "fake-echo");
    assert!(report.cases.iter().all(|case| {
        case.outcome == ProviderConformanceOutcome::Passed
            || matches!(case.outcome, ProviderConformanceOutcome::Skipped { .. })
    }));
    for required_case in [
        "baseline turn",
        "tool calling",
        "parallel tool calling",
        "structured output",
        "cancellation",
    ] {
        assert!(report.cases.iter().any(|case| {
            case.name == required_case && case.outcome == ProviderConformanceOutcome::Passed
        }));
    }
}

#[test]
fn suite_reports_actionable_case_and_cleans_up_after_event_violation() {
    let mut invoker = FakePluginInvoker::with_poll_behavior(PollBehavior::OmitTurnStarted);
    let error =
        run_provider_conformance_suite(&mut invoker, &ProviderConformanceOptions::default())
            .expect_err("missing TurnStarted must violate the contract");

    assert!(matches!(
        error,
        ProviderConformanceError::Violation {
            case: "baseline turn",
            ref message,
        } if message.contains("TurnStarted must be the first event")
    ));
    assert_eq!(invoker.cancel_calls, 1);
    assert_eq!(invoker.finish_calls, 1);
}

#[test]
fn suite_times_out_actionably_and_runs_cancel_then_finish_cleanup() {
    let mut invoker = FakePluginInvoker::with_poll_behavior(PollBehavior::Empty);
    let options = ProviderConformanceOptions {
        turn_timeout: Duration::from_millis(5),
        poll_interval: Duration::from_millis(1),
        ..ProviderConformanceOptions::default()
    };
    let error = run_provider_conformance_suite(&mut invoker, &options)
        .expect_err("an endlessly empty provider stream must time out");

    assert!(matches!(
        error,
        ProviderConformanceError::Timeout {
            case: "baseline turn",
            ..
        }
    ));
    assert_eq!(invoker.cancel_calls, 1);
    assert_eq!(invoker.finish_calls, 1);
}
