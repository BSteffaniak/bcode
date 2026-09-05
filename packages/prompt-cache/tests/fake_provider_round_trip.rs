//! Round-trip verification: the host planner, the scenario suite, the analyzer, and the
//! reference cache simulator must agree with each other through a real plugin invocation path.

use bcode_fake_provider_plugin::FakeProviderPlugin;
use bcode_fake_provider_plugin::prompt_cache::{
    FAKE_CACHE_EXPLICIT_MODEL_ID, FAKE_CACHE_PREFIX_MODEL_ID, reset,
};
use bcode_model_provider_runtime::BlockingModelProviderInvoker;
use bcode_plugin_sdk::{
    ConcurrentRustPlugin, NativeServiceContext, PluginConfigContext, ServiceBridge,
    ServiceCancellation, ServiceEventEmitter, ServiceRequest,
};
use bcode_prompt_cache::scenarios::{
    PromptCacheScenarioOptions, run_prompt_cache_scenarios, scenario,
};
use bcode_prompt_cache::{PromptCacheMechanism, PromptCacheScenarioOutcome, measurement};
use std::sync::{Mutex, MutexGuard};

/// The fake cache simulator is process-wide; serialize tests that reset it.
static SIMULATOR_LOCK: Mutex<()> = Mutex::new(());

fn simulator_guard() -> MutexGuard<'static, ()> {
    let guard = SIMULATOR_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    reset();
    guard
}

#[derive(Default)]
struct FakePluginInvoker {
    plugin: FakeProviderPlugin,
    starting_prepared_turn: bool,
}

impl BlockingModelProviderInvoker for FakePluginInvoker {
    fn start_turn(
        &mut self,
        provider_plugin_id: Option<&str>,
        request: &bcode_model::ModelTurnRequest,
    ) -> Result<bcode_model::StartTurnResponse, String> {
        self.starting_prepared_turn = true;
        let result = self.invoke_json(provider_plugin_id, bcode_model::OP_START_TURN, request);
        self.starting_prepared_turn = false;
        result
    }

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
        assert!(
            operation != bcode_model::OP_START_TURN || self.starting_prepared_turn,
            "every scenario turn must pass through host preparation"
        );
        let response = self.plugin.invoke_service_concurrent(NativeServiceContext {
            plugin_id: "bcode.fake-provider".to_string(),
            request: ServiceRequest {
                interface_id: bcode_model::MODEL_PROVIDER_INTERFACE_ID_V2.to_string(),
                operation: operation.to_string(),
                payload: serde_json::to_vec(request).map_err(|error| error.to_string())?,
            },
            config: PluginConfigContext::default(),
            events: ServiceEventEmitter::default(),
            cancellation: ServiceCancellation::default(),
            bridge: ServiceBridge::default(),
            transient_progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
        });
        if let Some(error) = response.error {
            return Err(format!("{}: {}", error.code, error.message));
        }
        serde_json::from_slice(&response.payload).map_err(|error| error.to_string())
    }
}

fn options(model_id: &str) -> PromptCacheScenarioOptions {
    PromptCacheScenarioOptions {
        model_id: Some(model_id.to_string()),
        ..PromptCacheScenarioOptions::default()
    }
}

fn outcome<'a>(
    report: &'a bcode_prompt_cache::PromptCacheVerificationReport,
    name: &str,
) -> &'a PromptCacheScenarioOutcome {
    &report
        .scenarios
        .iter()
        .find(|result| result.scenario == name)
        .unwrap_or_else(|| panic!("scenario {name} missing from report"))
        .outcome
}

#[test]
fn explicit_cache_model_passes_every_scenario() {
    let _guard = simulator_guard();
    let report = run_prompt_cache_scenarios(
        &mut FakePluginInvoker::default(),
        &options(FAKE_CACHE_EXPLICIT_MODEL_ID),
    )
    .expect("scenario suite runs against the explicit fake cache model");

    assert_eq!(report.model_id, FAKE_CACHE_EXPLICIT_MODEL_ID);
    let expectations = report.expectations.as_ref().expect("caching advertised");
    assert_eq!(expectations.mechanism, PromptCacheMechanism::ExplicitPoints);
    assert!(expectations.min_prefix_declared);
    for scenario in [
        scenario::COLD_REQUEST,
        scenario::WARM_SAME_PREFIX,
        scenario::GROWING_CONVERSATION,
        scenario::TOOL_LOOP,
        scenario::TTL_MATRIX,
        scenario::MODE_OFF,
        scenario::BUDGET_OVERFLOW,
    ] {
        assert_eq!(
            outcome(&report, scenario),
            &PromptCacheScenarioOutcome::Passed,
            "{scenario}: {report:#?}"
        );
    }
    assert!(report.is_success());
    assert!(report.verified_any_behavior());

    let tool_loop = report
        .scenarios
        .iter()
        .find(|result| result.scenario == scenario::TOOL_LOOP)
        .expect("tool loop scenario");
    assert!(tool_loop.measurements[measurement::HIT_ROUND_RATIO] >= 0.9);
    assert!(tool_loop.measurements[measurement::CACHED_INPUT_INCREASE_COUNT] >= 3.0);
    assert!(tool_loop.rounds.iter().all(|round| round.tool_round));
    let warm = report
        .scenarios
        .iter()
        .find(|result| result.scenario == scenario::WARM_SAME_PREFIX)
        .expect("warm scenario");
    assert!(warm.measurements[measurement::WARM_READ_RATIO] >= 0.9);
    assert!(warm.rounds[0].has_cache_write());
}

#[test]
fn automatic_prefix_model_passes_applicable_scenarios_and_skips_explicit_ones() {
    let _guard = simulator_guard();
    let report = run_prompt_cache_scenarios(
        &mut FakePluginInvoker::default(),
        &options(FAKE_CACHE_PREFIX_MODEL_ID),
    )
    .expect("scenario suite runs against the automatic fake cache model");

    let expectations = report.expectations.as_ref().expect("caching advertised");
    assert_eq!(
        expectations.mechanism,
        PromptCacheMechanism::AutomaticPrefix
    );
    assert!(expectations.ttl_seconds.is_empty());
    for scenario in [
        scenario::COLD_REQUEST,
        scenario::WARM_SAME_PREFIX,
        scenario::GROWING_CONVERSATION,
        scenario::TOOL_LOOP,
        scenario::MODE_OFF,
    ] {
        assert_eq!(
            outcome(&report, scenario),
            &PromptCacheScenarioOutcome::Passed,
            "{scenario}: {report:#?}"
        );
    }
    for scenario in [scenario::TTL_MATRIX, scenario::BUDGET_OVERFLOW] {
        assert!(
            matches!(
                outcome(&report, scenario),
                PromptCacheScenarioOutcome::Skipped { .. }
            ),
            "{scenario}: {report:#?}"
        );
    }
    let warm = report
        .scenarios
        .iter()
        .find(|result| result.scenario == scenario::WARM_SAME_PREFIX)
        .expect("warm scenario");
    assert!(
        !warm.rounds[0].has_cache_write(),
        "automatic-prefix model must not report cache writes"
    );
    assert!(warm.rounds[1].has_cache_read());
}

#[test]
fn non_caching_model_skips_every_scenario() {
    let _guard = simulator_guard();
    let report =
        run_prompt_cache_scenarios(&mut FakePluginInvoker::default(), &options("fake-echo"))
            .expect("scenario suite runs against a non-caching model");

    assert!(report.expectations.is_none());
    assert_eq!(report.scenarios.len(), 7);
    assert!(
        report
            .scenarios
            .iter()
            .all(|result| matches!(result.outcome, PromptCacheScenarioOutcome::Skipped { .. }))
    );
    assert!(report.is_success());
    assert!(!report.verified_any_behavior());
}
