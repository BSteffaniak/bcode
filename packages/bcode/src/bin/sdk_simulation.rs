use bcode::{
    AgentBuilder, AgentRuntime, ModelProviderInvoker, ProviderRequestIdentity, ProviderTurnEvent,
    TokenUsage, testing::*,
};
use std::sync::Arc;
use std::time::Duration;

#[cfg(not(feature = "simulation-example"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    install_runner_panic_hook();
    if !validate_native_arguments(std::env::args().skip(1))? {
        print_runner_help();
        return Ok(());
    }
    run_native(run())
}

#[cfg(not(feature = "simulation-example"))]
fn run_native(
    scenario: impl std::future::Future<Output = bcode::Result<()>>,
) -> Result<(), Box<dyn std::error::Error>> {
    use futures::FutureExt as _;
    let runtime = switchy::unsync::Builder::new().build()?;
    let result = runtime.block_on(std::panic::AssertUnwindSafe(scenario).catch_unwind());
    let result: Result<(), Box<dyn std::error::Error>> = match result {
        Ok(result) => result.map_err(Into::into),
        Err(_) => Err("native SDK scenario panicked".into()),
    };
    finish_native_run(result, runtime.wait().is_ok())
}

#[cfg(not(feature = "simulation-example"))]
fn finish_native_run(
    outcome: Result<(), Box<dyn std::error::Error>>,
    runtime_stopped: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if runtime_stopped {
        return outcome;
    }
    Err(Box::new(ShutdownFailure {
        message: "native runtime shutdown failed; resource release is unverified",
        scenario: outcome.err(),
    }))
}

struct ShutdownFailure {
    message: &'static str,
    scenario: Option<Box<dyn std::error::Error>>,
}

impl std::fmt::Debug for ShutdownFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

impl std::fmt::Display for ShutdownFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message)
    }
}

impl std::error::Error for ShutdownFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.scenario.as_deref()
    }
}

#[cfg(feature = "simulation-example")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    install_runner_panic_hook();
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--help"] {
        print_runner_help();
        return Ok(());
    }
    let budgets = SimulationBudgets::parse(args)?;
    // Validate before initializing any simulator globals or admitting application work.
    let seed = simulation_input("SIMULATOR_SEED")?;
    let epoch = simulation_input("SIMULATOR_EPOCH_OFFSET")?;
    let step_ms = simulation_input("SIMULATOR_STEP_MULTIPLIER")?;
    if step_ms == 0 {
        return Err("SIMULATOR_STEP_MULTIPLIER must be positive so deadlines can advance".into());
    }
    eprintln!(
        "diagnostic simulation: seed={seed} epoch_ms={epoch} step_ms={step_ms}; not a replay artifact"
    );
    // The harness, not application code, advances simulated time. One poll per step
    // is an explicit exploration policy, not a claim of exhaustive schedule coverage.
    switchy::time::simulator::reset_step();
    run_simulated(run(), budgets.execution, budgets.drain)
}

// Only the standalone executable owns this process-wide policy. Caught fixture
// panics still invoke Rust's hook, so never print their potentially private payloads.
fn install_runner_panic_hook() {
    std::panic::set_hook(Box::new(|_| {
        eprintln!(
            "SDK runner panic observed; payload omitted (the scenario determines the outcome)"
        );
    }));
}

fn print_runner_help() {
    println!("bcode-sdk-simulation: diagnostic SDK runner, not a certified DST profile");
    #[cfg(not(feature = "simulation-example"))]
    println!(
        "Native build: no execution options. Build with simulation-example for step controls."
    );
    #[cfg(feature = "simulation-example")]
    println!(
        "Simulator build: [--execution-steps N] [--drain-steps N] (positive; default 10000 each)\nRequired environment: SIMULATOR_SEED, SIMULATOR_EPOCH_OFFSET, SIMULATOR_STEP_MULTIPLIER (positive)\nStep budgets do not bound wall-clock time or constitute replay artifacts."
    );
}

#[cfg(any(test, not(feature = "simulation-example")))]
fn validate_native_arguments(args: impl IntoIterator<Item = String>) -> Result<bool, &'static str> {
    let mut args = args.into_iter();
    match (args.next(), args.next()) {
        (None, None) => Ok(true),
        (Some(arg), None) if arg == "--help" => Ok(false),
        _ => {
            Err("native runner accepts only --help; simulation options require simulation-example")
        }
    }
}

#[cfg(any(test, feature = "simulation-example"))]
#[derive(Debug, PartialEq, Eq)]
struct SimulationBudgets {
    execution: usize,
    drain: usize,
}

#[cfg(any(test, feature = "simulation-example"))]
impl SimulationBudgets {
    fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, &'static str> {
        let mut execution = None;
        let mut drain = None;
        let mut args = args.into_iter();
        while let Some(flag) = args.next() {
            let slot = match flag.as_str() {
                "--execution-steps" => &mut execution,
                "--drain-steps" => &mut drain,
                _ => return Err("expected --execution-steps or --drain-steps"),
            };
            if slot.is_some() {
                return Err("duplicate simulation budget option");
            }
            let value = args.next().ok_or("missing simulation budget value")?;
            let value = value
                .parse::<usize>()
                .map_err(|_| "invalid simulation step budget")?;
            if value == 0 {
                return Err("simulation step budgets must be positive");
            }
            *slot = Some(value);
        }
        Ok(Self {
            execution: execution.unwrap_or(10_000),
            drain: drain.unwrap_or(10_000),
        })
    }
}

#[cfg(test)]
mod shutdown_diagnostic_tests {
    use super::ShutdownFailure;
    use std::error::Error as _;

    #[test]
    fn simulation_drain_failure_preserves_source_without_exposing_it() {
        for scenario in [None, Some("private scenario detail".into())] {
            let error = ShutdownFailure {
                message: "simulation drain step budget exhausted; cleanup is unverified",
                scenario,
            };
            assert_eq!(error.to_string(), error.message);
            assert_eq!(format!("{error:?}"), error.message);
            assert_eq!(format!("{error:#?}"), error.message);
            assert_eq!(
                error.source().map(ToString::to_string),
                error.scenario.as_ref().map(ToString::to_string),
            );
        }
    }
}

#[cfg(all(test, not(feature = "simulation-example")))]
mod native_lifecycle_tests {
    use super::*;

    #[test]
    fn workflow_cancellation_and_deadlines_release_native_resources() {
        run_native(async {
            run_workflow_repeat_cancellation().await;
            run_workflow_cancellation(false).await?;
            run_workflow_cancellation(true).await?;
            Ok(())
        })
        .expect("native workflow termination acknowledges resource release");
    }

    #[test]
    fn shutdown_failure_preserves_scenario_source_without_displaying_it() {
        let error = finish_native_run(Err("private scenario detail".into()), false).unwrap_err();
        assert_eq!(format!("{error:?}"), error.to_string());
        assert_eq!(format!("{error:#?}"), error.to_string());
        assert_eq!(
            error.to_string(),
            "native runtime shutdown failed; resource release is unverified"
        );
        assert_eq!(
            error.source().unwrap().to_string(),
            "private scenario detail"
        );
        assert!(
            finish_native_run(Ok(()), false)
                .unwrap_err()
                .source()
                .is_none()
        );
        assert_eq!(
            finish_native_run(Err("original".into()), true)
                .unwrap_err()
                .to_string(),
            "original"
        );
        assert!(finish_native_run(Ok(()), true).is_ok());
    }

    #[test]
    fn root_panic_releases_scenario_state() {
        struct Release(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Release {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::Release);
            }
        }
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let release = Release(Arc::clone(&completed));
        let result = run_native(async move {
            let _release = release;
            panic!("private root panic");
        });
        assert_eq!(
            result.unwrap_err().to_string(),
            "native SDK scenario panicked"
        );
        assert!(completed.load(std::sync::atomic::Ordering::Acquire));
    }
}

#[cfg(test)]
mod budget_tests {
    use super::SimulationBudgets;

    #[test]
    fn native_arguments_do_not_silently_ignore_simulator_controls() {
        assert_eq!(super::validate_native_arguments([]), Ok(true));
        assert_eq!(
            super::validate_native_arguments(["--help".into()]),
            Ok(false)
        );
        for args in [
            vec!["--execution-steps", "3"],
            vec!["--help", "extra"],
            vec!["unknown"],
        ] {
            assert!(super::validate_native_arguments(args.into_iter().map(str::to_owned)).is_err());
        }
    }

    #[test]
    fn budgets_accept_defaults_and_explicit_values() {
        assert_eq!(
            SimulationBudgets::parse([]).unwrap(),
            SimulationBudgets {
                execution: 10_000,
                drain: 10_000
            }
        );
        assert_eq!(
            SimulationBudgets::parse(
                ["--drain-steps", "2", "--execution-steps", "3"].map(str::to_owned)
            )
            .unwrap(),
            SimulationBudgets {
                execution: 3,
                drain: 2
            }
        );
    }

    #[test]
    fn budgets_reject_ambiguous_or_invalid_inputs() {
        for args in [
            vec!["--unknown"],
            vec!["--drain-steps"],
            vec!["--execution-steps", "0"],
            vec!["--drain-steps", "-1"],
            vec!["--drain-steps", "secret"],
            vec!["--drain-steps", "1", "--drain-steps", "2"],
            vec!["--execution-steps", "99999999999999999999999999999999"],
        ] {
            assert!(SimulationBudgets::parse(args.into_iter().map(str::to_owned)).is_err());
        }
    }
}

#[cfg(feature = "simulation-example")]
fn run_simulated(
    scenario: impl std::future::Future<Output = bcode::Result<()>> + Send + 'static,
    execution_steps: usize,
    drain_steps: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    use futures::FutureExt as _;
    // Simulator clocks and scheduler state are process-global. Keep test runs
    // exclusive even when libtest uses its default parallel execution.
    #[cfg(test)]
    let _simulation_guard = {
        static SIMULATION: std::sync::Mutex<()> = std::sync::Mutex::new(());
        SIMULATION.lock().expect("previous simulator test panicked")
    };
    let runtime = switchy::unsync::Builder::new().build()?;
    // Catch root panics inside the task so scenario-owned work is released first.
    let mut task = runtime.spawn(std::panic::AssertUnwindSafe(scenario).catch_unwind());
    let mut outcome: Result<(), Box<dyn std::error::Error>> =
        Err("simulation harness step budget exhausted (not a product timeout)".into());
    for _ in 0..execution_steps {
        runtime.tick();
        if task.is_finished() {
            outcome = match runtime.block_on(&mut task) {
                Ok(Ok(result)) => result.map_err(Into::into),
                Ok(Err(_)) => Err("simulation scenario panicked".into()),
                Err(_) => Err("simulation root task failed".into()),
            };
            break;
        }
        let _ = switchy::time::simulator::next_step();
    }
    task.abort();
    drop(task);
    // Task polls may block: this bounds scheduling steps, not wall-clock shutdown.
    for step in 0..=drain_steps {
        if runtime.try_finish() {
            return outcome;
        }
        if step != drain_steps {
            runtime.tick();
            let _ = switchy::time::simulator::next_step();
        }
    }
    Err(Box::new(ShutdownFailure {
        message: "simulation drain step budget exhausted; cleanup is unverified",
        scenario: outcome.err(),
    }))
}

#[cfg(feature = "simulation-example")]
fn simulation_input(name: &str) -> Result<u64, Box<dyn std::error::Error>> {
    let value = std::env::var(name).map_err(|_| {
        format!("{name} must be explicitly set; example: SIMULATOR_SEED=1 SIMULATOR_EPOCH_OFFSET=1700000000000 SIMULATOR_STEP_MULTIPLIER=1")
    })?;
    value.parse().map_err(|_| {
        // Do not echo arbitrary environment values into diagnostics.
        format!("{name} must be an unsigned 64-bit integer").into()
    })
}

#[cfg(all(test, feature = "simulation-example"))]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn sdk_scenario_completes_and_drains_under_simulation() {
        run_simulated(run(), 10_000, 10_000)
            .expect("SDK scenario completes with acknowledged simulator drain");
    }

    #[test]
    fn sequential_sdk_runs_complete_after_acknowledged_drain() {
        for _ in 0..2 {
            // Each run constructs fresh SDK services and request identity sources.
            // Time remains monotonic across runs; no process-global reset escapes
            // the runner's exclusive test lifetime.
            run_simulated(run(), 10_000, 10_000)
                .expect("fresh SDK scenario completes after prior runtime drain");
        }
    }

    #[test]
    fn exhausted_sdk_run_drains_before_fresh_run() {
        let error = run_simulated(run(), 10, 10_000)
            .expect_err("the full SDK scenario exceeds ten scheduling steps");
        assert_eq!(
            error.to_string(),
            "simulation harness step budget exhausted (not a product timeout)"
        );
        // A shutdown failure would replace this error, so the next run follows
        // acknowledged drain rather than merely dropping the root task handle.
        run_simulated(run(), 10_000, 10_000)
            .expect("fresh SDK run completes after budget-exhausted predecessor");
    }

    #[test]
    fn budget_exhaustion_releases_root_captures() {
        let released = bcode::CancellationToken::new();
        let release = WorkerRelease(released.clone());
        let result = run_simulated(
            async move {
                let _release = release;
                std::future::pending().await
            },
            0,
            100,
        );
        assert_eq!(
            result.unwrap_err().to_string(),
            "simulation harness step budget exhausted (not a product timeout)"
        );
        assert!(released.is_cancelled());
    }

    struct PanickingProvider(bool);

    impl bcode::InProcessModelProvider for PanickingProvider {
        fn run_turn(
            &self,
            _: bcode::ModelTurnRequest,
            _: bcode::InProcessProviderContext,
        ) -> bcode::InProcessProviderFuture<'_> {
            assert!(!self.0, "fixture construction panic");
            Box::pin(async { panic!("fixture polling panic") })
        }
    }

    #[test]
    fn provider_panics_are_normalized_without_unwinding_scheduler() {
        for construction in [false, true] {
            run_simulated(
                async move {
                    let mut provider =
                        bcode::InProcessModelProviderAdapter::new(PanickingProvider(construction));
                    let session_id = "00000000-0000-4000-8000-000000000024"
                        .parse()
                        .expect("fixture ID");
                    let identities = ScriptedRequestIdentities::new([ProviderRequestIdentity {
                        session_id,
                        turn_id: "provider-panic".into(),
                    }])?;
                    let error = AgentBuilder::from_context(session_id, "/".into())
                        .runtime(
                            AgentRuntime::new()
                                .with_provider_request_identity_source(Arc::new(identities)),
                        )
                        .build()
                        .generate_text_with_provider(&mut provider, "panic")
                        .await
                        .expect_err("provider failure");
                    assert!(matches!(error,
                    bcode::BcodeError::Runtime(bcode::RuntimeError::Provider { code, message, .. })
                    if code == "in_process_worker_stopped"
                    && message == "in-process provider worker stopped before completing its turn"));
                    Ok(())
                },
                1000,
                100,
            )
            .expect("scheduler survives provider panic and drains");
        }
    }

    #[test]
    fn sdk_shutdown_drains_abandoned_provider_and_fences_admission() {
        run_simulated(
            run_in_process_cleanup(InProcessCleanup::Shutdown),
            1000,
            100,
        )
        .expect("SDK shutdown releases worker under simulation");
    }

    #[test]
    fn sdk_host_deadline_shutdown_wait_drains_provider() {
        run_simulated(
            run_in_process_cleanup(InProcessCleanup::ShutdownWait),
            1000,
            100,
        )
        .expect("host-bounded shutdown wait releases provider worker");
    }

    #[test]
    fn scenario_error_drains_admitted_work_before_returning() {
        let released = bcode::CancellationToken::new();
        let release = WorkerRelease(released.clone());
        let result = run_simulated(
            async move {
                switchy::unsync::task::spawn(async move {
                    switchy::unsync::time::sleep(Duration::from_millis(10)).await;
                    drop(release);
                });
                Err(bcode::BcodeError::ToolExecution(
                    "fixture scenario error".to_owned(),
                ))
            },
            100,
            100,
        );
        assert!(matches!(
            result.expect_err("scenario failure must survive successful drain")
                .downcast_ref::<bcode::BcodeError>(),
            Some(bcode::BcodeError::ToolExecution(message)) if message == "fixture scenario error"
        ));
        assert!(
            released.is_cancelled(),
            "admitted worker must release before return"
        );
    }

    #[test]
    fn root_panic_drains_spawned_work_and_preserves_failure() {
        let released = bcode::CancellationToken::new();
        let release = WorkerRelease(released.clone());
        let result = run_simulated(
            async move {
                switchy::unsync::task::spawn(async move {
                    drop(release);
                });
                panic!("fixture root panic");
            },
            100,
            100,
        );
        assert_eq!(
            result.unwrap_err().to_string(),
            "simulation scenario panicked"
        );
        assert!(released.is_cancelled());
    }
}

struct InProcessEcho;

impl bcode::InProcessModelProvider for InProcessEcho {
    fn run_turn(
        &self,
        _request: bcode::ModelTurnRequest,
        context: bcode::InProcessProviderContext,
    ) -> bcode::InProcessProviderFuture<'_> {
        Box::pin(async move {
            switchy::unsync::time::sleep(Duration::from_millis(1)).await;
            context
                .events()
                .emit(ProviderTurnEvent::TextDelta {
                    text: "in-process answer".into(),
                })
                .expect("active in-process turn accepts text");
            Ok(bcode::InProcessProviderOutcome::EndTurn)
        })
    }
}

struct PendingInProcess {
    started: bcode::CancellationToken,
    released: bcode::CancellationToken,
    context: Arc<std::sync::Mutex<Option<bcode::InProcessProviderContext>>>,
}

struct WorkerRelease(bcode::CancellationToken);

impl Drop for WorkerRelease {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl bcode::InProcessModelProvider for PendingInProcess {
    fn run_turn(
        &self,
        _request: bcode::ModelTurnRequest,
        context: bcode::InProcessProviderContext,
    ) -> bcode::InProcessProviderFuture<'_> {
        Box::pin(async move {
            if self.started.is_cancelled() {
                assert!(
                    !context.cancellation().is_cancelled(),
                    "fresh turn is active"
                );
                let old_context = self
                    .context
                    .lock()
                    .expect("context lock")
                    .clone()
                    .expect("previous worker context");
                assert_eq!(
                    old_context.events().emit(ProviderTurnEvent::TextDelta {
                        text: "stale during replacement".into(),
                    }),
                    Err(bcode::InProcessProviderEmitError::TurnFinished),
                    "old context must not inject into an active replacement"
                );
                context
                    .events()
                    .emit(ProviderTurnEvent::TextDelta {
                        text: "recovered worker".into(),
                    })
                    .expect("fresh turn accepts output");
                return Ok(bcode::InProcessProviderOutcome::EndTurn);
            }
            let _release = WorkerRelease(self.released.clone());
            *self.context.lock().expect("context lock") = Some(context);
            self.started.cancel();
            std::future::pending().await
        })
    }
}

#[derive(Clone, Copy, Debug)]
enum InProcessCleanup {
    Shutdown,
    ShutdownWait,
    AdapterDrop,
    Cancel,
    Deadline,
}

async fn run_in_process_cleanup(mode: InProcessCleanup) -> bcode::Result<()> {
    let started = bcode::CancellationToken::new();
    let released = bcode::CancellationToken::new();
    let context = Arc::new(std::sync::Mutex::new(None));
    let mut provider = bcode::InProcessModelProviderAdapter::new(PendingInProcess {
        started: started.clone(),
        released: released.clone(),
        context: context.clone(),
    });
    let timeout = Duration::from_secs(3);
    let session_id = "00000000-0000-4000-8000-000000000022"
        .parse()
        .expect("fixture ID");
    let identities = ScriptedRequestIdentities::new(
        ["cleanup-active", "cleanup-next", "cleanup-after-shutdown"].map(|turn_id| {
            ProviderRequestIdentity {
                session_id,
                turn_id: turn_id.into(),
            }
        }),
    )?;
    let agent = AgentBuilder::from_context(session_id, "/".into())
        .runtime(AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)))
        .timeout(timeout)
        .build();
    let cancellation = bcode::CancellationToken::new();
    let generation_started = switchy::time::instant_now();
    let mut generation = Box::pin(agent.generate_text_with_provider_and_cancellation(
        &mut provider,
        "worker cleanup",
        cancellation.clone(),
    ));
    switchy::unsync::select! {
        result = &mut generation => panic!("pending provider completed: {result:?}"),
        () = started.cancelled() => {},
        () = switchy::unsync::time::sleep(Duration::from_secs(2)) => panic!("worker did not start"),
    }
    if matches!(mode, InProcessCleanup::Cancel) {
        cancellation.cancel();
        switchy::unsync::select! {
            result = &mut generation => assert!(matches!(result,
                Err(bcode::BcodeError::Runtime(bcode::RuntimeError::Cancelled)))),
            () = switchy::unsync::time::sleep(Duration::from_secs(2)) => panic!("SDK cancellation did not complete"),
        }
    }
    if matches!(mode, InProcessCleanup::Deadline) {
        switchy::unsync::select! {
            result = &mut generation => assert!(matches!(result,
                Err(bcode::BcodeError::Runtime(bcode::RuntimeError::Timeout { timeout: actual })) if actual == timeout)),
            () = switchy::unsync::time::sleep(Duration::from_secs(5)) => panic!("SDK deadline did not complete"),
        }
        assert!(
            switchy::time::instant_now().duration_since(generation_started) >= timeout,
            "in-process SDK deadline fired early"
        );
    }
    drop(generation);
    if matches!(
        mode,
        InProcessCleanup::Shutdown | InProcessCleanup::ShutdownWait
    ) {
        if matches!(mode, InProcessCleanup::ShutdownWait) {
            {
                let invoker: &mut dyn bcode::ModelProviderInvoker = &mut provider;
                drop(invoker.shutdown_wait());
            }
            let error = agent
                .generate_text_with_provider(&mut provider, "unpolled shutdown")
                .await
                .expect_err("unpolled trait shutdown fences SDK admission");
            assert!(matches!(error, bcode::BcodeError::Runtime(
                bcode::RuntimeError::Provider { code, error, .. }
            ) if code == "in_process_admission_closed" && !error.retryable));
            let invoker: &mut dyn bcode::ModelProviderInvoker = &mut provider;
            switchy::unsync::select! {
                result = invoker.shutdown_wait() => result?,
                () = switchy::unsync::time::sleep(Duration::from_secs(2)) => panic!("host shutdown deadline expired"),
            }
        } else {
            provider.shutdown(Duration::from_secs(2)).await?;
        }
        assert!(
            released.is_cancelled(),
            "shutdown acknowledges worker release"
        );
    }
    // Returned terminal outcomes must release the worker without adapter destruction.
    let provider = (!matches!(mode, InProcessCleanup::AdapterDrop)).then_some(provider);
    switchy::unsync::select! {
        () = released.cancelled() => {},
        () = switchy::unsync::time::sleep(Duration::from_secs(2)) => panic!("cleanup did not release worker future ({mode:?})"),
    }
    let context = context
        .lock()
        .expect("context lock")
        .clone()
        .expect("worker context");
    assert!(
        context.cancellation().is_cancelled(),
        "{mode:?}: cancellation visible"
    );
    assert_eq!(
        context.events().emit(ProviderTurnEvent::TextDelta {
            text: "late output".into(),
        }),
        Err(bcode::InProcessProviderEmitError::TurnFinished),
        "{mode:?}: terminal worker rejects late output"
    );
    if let Some(mut provider) = provider {
        if matches!(
            mode,
            InProcessCleanup::Shutdown | InProcessCleanup::ShutdownWait
        ) {
            let error = agent
                .generate_text_with_provider(&mut provider, "closed adapter")
                .await
                .expect_err("shutdown fences admission");
            assert!(matches!(error, bcode::BcodeError::Runtime(
                bcode::RuntimeError::Provider { code, error, .. }
            ) if code == "in_process_admission_closed" && !error.retryable));
            provider.shutdown(Duration::from_secs(2)).await?;
            return Ok(());
        }
        let response = agent
            .generate_text_with_provider(&mut provider, "recover worker")
            .await?;
        assert_eq!(response.text, "recovered worker");
        assert_eq!(
            response.runtime.stop_reason,
            Some(bcode::StopReason::EndTurn)
        );
        assert_eq!(
            context.events().emit(ProviderTurnEvent::TextDelta {
                text: "stale after reuse".into(),
            }),
            Err(bcode::InProcessProviderEmitError::TurnFinished),
            "old context stays terminal after adapter reuse"
        );
    }
    Ok(())
}

async fn run_explicit_workflow() -> bcode::Result<()> {
    let session_id = "00000000-0000-4000-8000-000000000020"
        .parse()
        .expect("fixture ID");
    let identities = ScriptedRequestIdentities::new([ProviderRequestIdentity {
        session_id,
        turn_id: "workflow-review-0".into(),
    }])?;
    let builder = AgentBuilder::from_context(session_id, "/".into())
        .runtime(AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)))
        .model("workflow-model");
    let provider =
        ScriptedProvider::new([ScriptedProviderTurn::complete_text(r#"{"approved":true}"#)]);
    let probe = provider.probe();
    let mut owner = provider.clone();
    let step =
        bcode::workflow::AgentStep::<serde_json::Value, serde_json::Value>::with_agent_builder(
            "review",
            move || provider.clone(),
            builder,
        )
        .read_only()
        .build();
    let workflow = bcode::workflow::WorkflowBuilder::new("sdk-controlled-review", step)
        .build()
        .expect("workflow definition");
    let result = workflow.run(serde_json::json!({"diff": "+ safe"})).await;
    owner.shutdown_wait().await?;
    assert_eq!(
        result.expect("workflow succeeds"),
        serde_json::json!({"approved": true})
    );
    probe
        .assert_requests(&[ScriptedRequestExpectation::new().model_id("workflow-model")])
        .expect("workflow uses configured model exactly once");
    probe
        .assert_finish_count(1)
        .expect("workflow releases provider turn");
    Ok(())
}

async fn run_workflow_cancellation(timeout: bool) -> bcode::Result<()> {
    let session_id = "00000000-0000-4000-8000-000000000021"
        .parse()
        .expect("fixture ID");
    let identities = ScriptedRequestIdentities::new([ProviderRequestIdentity {
        session_id,
        turn_id: "workflow-cancel-0".into(),
    }])?;
    let builder = AgentBuilder::from_context(session_id, "/".into())
        .runtime(AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)));
    let provider = ScriptedProvider::new([ScriptedProviderTurn::new().pending()]);
    let probe = provider.probe();
    let mut owner = provider.clone();
    let step =
        bcode::workflow::AgentStep::<serde_json::Value, serde_json::Value>::with_agent_builder(
            "cancel-review",
            move || provider.clone(),
            builder,
        )
        .read_only()
        .build()
        .resources([bcode::workflow::ResourceClaim::write("review-slot")]);
    let step = if timeout {
        step.timeout(Duration::from_millis(20))
    } else {
        step
    };
    let workflow = bcode::workflow::WorkflowBuilder::new("sdk-cancel-review", step)
        .build()
        .expect("workflow definition");
    let cancellation = bcode::workflow::WorkflowCancellation::new();
    let observer = workflow.observer();
    let execution = workflow.run_with_observer(
        serde_json::json!({}),
        cancellation.clone(),
        None,
        observer.clone(),
    );
    let cancel_after_start = async {
        while probe.requests().is_empty() {
            switchy::unsync::time::sleep(Duration::from_millis(1)).await;
        }
        if !timeout {
            cancellation.cancel();
        }
        std::future::pending::<()>().await;
    };
    let result = switchy::unsync::select! {
        result = execution => result,
        () = cancel_after_start => unreachable!("cancellation driver stays pending"),
    };
    if timeout {
        assert!(matches!(
            result,
            Err(bcode::workflow::WorkflowError::TimedOut { .. })
        ));
    } else {
        assert!(matches!(
            result,
            Err(bcode::workflow::WorkflowError::Cancelled { .. })
        ));
    }
    probe
        .assert_cancellation_count(1)
        .expect("provider cancelled once");
    probe
        .assert_finish_count(1)
        .expect("provider released once");
    let snapshot = observer.snapshot();
    assert_eq!(
        snapshot.nodes["cancel-review"],
        if timeout {
            bcode::workflow::NodeRunState::TimedOut
        } else {
            bcode::workflow::NodeRunState::Cancelled
        }
    );
    assert!(snapshot.running.is_empty());
    assert!(snapshot.resource_holders.is_empty());
    owner.shutdown_wait().await?;
    Ok(())
}

async fn run_workflow_retry() {
    for delay in [Duration::ZERO, Duration::from_millis(5)] {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&attempts);
        let step = bcode::workflow::Step::map("retry-operation", move |input: u32| {
            if observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                Err(bcode::workflow::WorkflowError::step(
                    "retry-operation",
                    "transient fixture failure",
                ))
            } else {
                Ok(input + 1)
            }
        })
        .retry_with_policy(
            "retry-controller",
            bcode::workflow::RetryPolicy::new(2).backoff(delay),
        );
        let workflow = bcode::workflow::WorkflowBuilder::new("sdk-retry", step)
            .build()
            .expect("retry workflow");
        assert_eq!(workflow.run(41).await.expect("retry succeeds"), 42);
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}

async fn run_retry_cancellation() {
    let cancellation = bcode::workflow::WorkflowCancellation::new();
    let signal = cancellation.clone();
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = Arc::clone(&attempts);
    let step = bcode::workflow::Step::map("cancel-retry", move |_: u32| {
        observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        signal.cancel();
        Err::<u32, _>(bcode::workflow::WorkflowError::step(
            "cancel-retry",
            "fixture failure",
        ))
    })
    .retry_with_policy(
        "retry",
        bcode::workflow::RetryPolicy::new(2).backoff(Duration::from_secs(3600)),
    );
    let workflow = bcode::workflow::WorkflowBuilder::new("cancel-backoff", step)
        .build()
        .expect("workflow");
    let result = switchy::unsync::time::timeout(
        Duration::from_secs(1),
        workflow.run_with_cancellation(0, cancellation),
    )
    .await
    .expect("cancellation does not wait for backoff");
    assert!(matches!(
        result,
        Err(bcode::workflow::WorkflowError::Cancelled { .. })
    ));
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
}

async fn run_workflow_repeat_cancellation() {
    let started = bcode::workflow::WorkflowCancellation::new();
    let signal = started.clone();
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = Arc::clone(&attempts);
    let step = bcode::workflow::Step::map("work", move |input: serde_json::Value| {
        observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        signal.cancel();
        Ok(input)
    })
    .resources([bcode::workflow::ResourceClaim::write("repository")])
    .repeat_while(
        "repeat",
        bcode::workflow::field::<serde_json::Value>("again").eq(true),
        100,
    )
    .then(bcode::workflow::Step::map(
        "after-repeat",
        |_: serde_json::Value| -> Result<serde_json::Value, bcode::workflow::WorkflowError> {
            panic!("downstream work must not execute after repeat cancellation")
        },
    ));
    let workflow = bcode::workflow::WorkflowBuilder::new("repeat-cancellation", step)
        .build()
        .expect("workflow");
    let cancellation = bcode::workflow::WorkflowCancellation::new();
    let cancel_signal = cancellation.clone();
    let cancel_task = switchy::unsync::task::spawn(async move {
        started.cancelled().await;
        cancel_signal.cancel();
    });
    let observer = workflow.observer();
    let result = workflow
        .run_with_observer(
            serde_json::json!({"again": true}),
            cancellation,
            None,
            observer.clone(),
        )
        .await;
    cancel_task.await.expect("cancellation task joins");
    let snapshot = observer.snapshot();
    assert_eq!(
        snapshot.nodes["repeat"],
        bcode::workflow::NodeRunState::Cancelled
    );
    assert_eq!(
        snapshot.nodes["after-repeat"],
        bcode::workflow::NodeRunState::Cancelled
    );
    assert!(snapshot.running.is_empty());
    assert!(snapshot.resource_holders.is_empty());
    assert!(matches!(
        result,
        Err(bcode::workflow::WorkflowError::Cancelled { .. })
    ));
    let count = attempts.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        count > 0 && count < 100,
        "cancellation must run before iteration exhaustion"
    );
}

async fn run_workflow_panic() {
    for asynchronous in [false, true] {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = Arc::clone(&attempts);
        let step: bcode::workflow::Step<u32, u32> = if asynchronous {
            bcode::workflow::Step::task("panic", move |_: u32, _| {
                let observed = Arc::clone(&observed);
                async move {
                    observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    switchy::unsync::task::yield_now().await;
                    panic!("fixture-private-workflow-panic");
                }
            })
        } else {
            bcode::workflow::Step::map("panic", move |_: u32| {
                observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                panic!("fixture-private-workflow-panic");
            })
        };
        let workflow = bcode::workflow::WorkflowBuilder::new(
            "sdk-panic",
            step.resources([bcode::workflow::ResourceClaim::write("repository")])
                .retry("retry-panic", 3)
                .retry("outer-retry", 2)
                .then(bcode::workflow::Step::<u32, u32>::map(
                    "unreachable",
                    |_: u32| {
                        panic!("downstream step must not execute after panic");
                    },
                )),
        )
        .build()
        .expect("workflow");
        let observer = workflow.observer();
        let result = workflow
            .run_with_observer(
                0,
                bcode::workflow::WorkflowCancellation::new(),
                None,
                observer.clone(),
            )
            .await;
        assert!(matches!(result,
            Err(bcode::workflow::WorkflowError::Panicked { step })
                if step == "panic"
        ));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
        let snapshot = observer.snapshot();
        assert_eq!(
            snapshot.nodes["outer-retry"],
            bcode::workflow::NodeRunState::Failed
        );
        assert_eq!(
            snapshot.nodes["unreachable"],
            bcode::workflow::NodeRunState::Skipped
        );
        assert_eq!(
            snapshot.nodes["retry-panic"],
            bcode::workflow::NodeRunState::Failed
        );
        assert_eq!(
            snapshot.nodes["panic"],
            bcode::workflow::NodeRunState::Failed
        );
        assert!(snapshot.running.is_empty());
        assert!(snapshot.resource_holders.is_empty());
    }
}

async fn run_workflow_contention(resource_claim: bool, cancel_waiter: bool) {
    let cancellation = bcode::workflow::WorkflowCancellation::new();
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let make_step = |name| {
        let active = Arc::clone(&active);
        let completed = Arc::clone(&completed);
        let signal = cancellation.clone();
        let step = bcode::workflow::Step::task(name, move |input: u32, _context| {
            let signal = signal.clone();
            let active = Arc::clone(&active);
            let completed = Arc::clone(&completed);
            async move {
                assert_eq!(active.fetch_add(1, std::sync::atomic::Ordering::SeqCst), 0);
                switchy::unsync::time::sleep(Duration::from_millis(5)).await;
                if cancel_waiter {
                    signal.cancel();
                }
                assert_eq!(active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst), 1);
                completed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(input + 1)
            }
        });
        if resource_claim {
            step.resources([bcode::workflow::ResourceClaim::write("repository")])
        } else {
            step
        }
    };
    let step = bcode::workflow::parallel(make_step("left"), make_step("right"));
    let workflow = bcode::workflow::WorkflowBuilder::new("workflow-contention", step)
        .build()
        .expect("workflow");
    let execution = async {
        if resource_claim {
            workflow.run_with_cancellation(41, cancellation).await
        } else {
            workflow
                .run_with_concurrency_limit(41, cancellation, 1)
                .await
        }
    };
    let result = switchy::unsync::time::timeout(Duration::from_secs(1), execution)
        .await
        .expect("waiter wakes after release");
    if cancel_waiter {
        assert!(matches!(
            result,
            Err(bcode::workflow::WorkflowError::Cancelled { .. })
        ));
        assert_eq!(completed.load(std::sync::atomic::Ordering::SeqCst), 1);
    } else {
        assert_eq!(result.expect("workflow succeeds"), (42, 42));
        assert_eq!(completed.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
    assert_eq!(active.load(std::sync::atomic::Ordering::SeqCst), 0);
}

async fn run_parallel_wait_all() {
    for failing_left in [false, true] {
        let completed = bcode::workflow::WorkflowCancellation::new();
        let signal = completed.clone();
        let delayed = bcode::workflow::Step::task("delayed", move |(): (), _context| {
            let signal = signal.clone();
            async move {
                switchy::unsync::time::sleep(Duration::from_millis(5)).await;
                signal.cancel();
                Ok(())
            }
        });
        let failure = bcode::workflow::Step::map("failure", |(): ()| {
            Err::<(), _>(bcode::workflow::WorkflowError::step(
                "failure",
                "fixture failure",
            ))
        });
        let (left, right) = if failing_left {
            (failure, delayed)
        } else {
            (delayed, failure)
        };
        let step = bcode::workflow::parallel_named_with_policy(
            "join",
            bcode::workflow::ParallelFailurePolicy::WaitAll,
            left,
            right,
        );
        let workflow = bcode::workflow::WorkflowBuilder::new("parallel-wait-all", step)
            .build()
            .expect("workflow");
        let result = switchy::unsync::time::timeout(Duration::from_secs(1), workflow.run(()))
            .await
            .expect("both branches finish");
        assert!(matches!(result,
            Err(bcode::workflow::WorkflowError::Step { step, message })
                if step == "failure" && message == "fixture failure"
        ));
        assert!(
            completed.is_cancelled(),
            "wait-all must not drop the delayed sibling"
        );
    }
}

async fn run_fan_out_cancellation(drop_execution: bool) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Active(Arc<AtomicUsize>);
    impl Drop for Active {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    let parent = bcode::workflow::WorkflowCancellation::new();
    let signal = parent.clone();
    let active = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(AtomicUsize::new(0));
    let ready = bcode::workflow::WorkflowCancellation::new();
    let member_ready = ready.clone();
    let member_active = Arc::clone(&active);
    let member_started = Arc::clone(&started);
    let step = bcode::workflow::Step::task("member", move |input: u32, _| {
        let active = Arc::clone(&member_active);
        let started = Arc::clone(&member_started);
        let signal = signal.clone();
        let ready = member_ready.clone();
        async move {
            active.fetch_add(1, Ordering::SeqCst);
            let _guard = Active(active);
            if started.fetch_add(1, Ordering::SeqCst) == 1 {
                ready.cancel();
                if !drop_execution {
                    signal.cancel();
                }
            }
            std::future::pending::<()>().await;
            Ok(input)
        }
    });
    let workflow = bcode::workflow::WorkflowBuilder::new(
        "sdk-fan-out-cancellation",
        bcode::workflow::fan_out("members", step, 2),
    )
    .build()
    .expect("fan-out workflow");
    let pre_cancelled = bcode::workflow::WorkflowCancellation::new();
    pre_cancelled.cancel();
    assert!(matches!(
        workflow
            .run_with_cancellation(vec![0, 1, 2, 3], pre_cancelled)
            .await,
        Err(bcode::workflow::WorkflowError::Cancelled { .. })
    ));
    assert_eq!(started.load(Ordering::SeqCst), 0);
    assert_eq!(active.load(Ordering::SeqCst), 0);
    let mut execution = Box::pin(workflow.run_with_cancellation(vec![0, 1, 2, 3], parent));
    if drop_execution {
        switchy::unsync::select! {
            _ = &mut execution => panic!("pending members must not finish"),
            () = ready.cancelled() => {}
        }
        assert_eq!(active.load(Ordering::SeqCst), 2);
        drop(execution);
    } else {
        assert!(matches!(
            execution.await,
            Err(bcode::workflow::WorkflowError::Cancelled { .. })
        ));
    }
    assert_eq!(started.load(Ordering::SeqCst), 2);
    assert_eq!(active.load(Ordering::SeqCst), 0);
}

async fn run_fan_out_failure(panic: bool) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Released(bcode::workflow::WorkflowCancellation);
    impl Drop for Released {
        fn drop(&mut self) {
            self.0.cancel();
        }
    }
    let started = Arc::new(AtomicUsize::new(0));
    let member_started = Arc::clone(&started);
    let sibling_ready = bcode::workflow::WorkflowCancellation::new();
    let released = bcode::workflow::WorkflowCancellation::new();
    let member_released = released.clone();
    let step = bcode::workflow::Step::task("member", move |input: u32, _| {
        let started = Arc::clone(&member_started);
        let sibling_ready = sibling_ready.clone();
        let released = member_released.clone();
        async move {
            started.fetch_add(1, Ordering::SeqCst);
            if input == 0 {
                sibling_ready.cancelled().await;
                assert!(!panic, "fixture fan-out panic");
                return Err(bcode::workflow::WorkflowError::step(
                    "member",
                    "expected failure",
                ));
            }
            let _guard = Released(released);
            sibling_ready.cancel();
            std::future::pending::<()>().await;
            Ok(input)
        }
    });
    let workflow = bcode::workflow::WorkflowBuilder::new(
        "sdk-fan-out-failure",
        bcode::workflow::fan_out("members", step, 2),
    )
    .build()
    .expect("fan-out workflow");
    let error = workflow
        .run(vec![0, 1, 2, 3])
        .await
        .expect_err("member fails");
    if panic {
        assert!(matches!(
            error,
            bcode::workflow::WorkflowError::Panicked { .. }
        ));
    } else {
        assert!(error.to_string().contains("expected failure"));
    }
    assert_eq!(started.load(Ordering::SeqCst), 2);
    assert!(
        released.is_cancelled(),
        "pending sibling released before return"
    );
}

async fn run_fan_out_workflow() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ActiveMember(Arc<AtomicUsize>);
    impl Drop for ActiveMember {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let member_completed = Arc::clone(&completed);
    let second_finished = bcode::workflow::WorkflowCancellation::new();
    let member_active = Arc::clone(&active);
    let member_peak = Arc::clone(&peak);
    let step = bcode::workflow::Step::task("member", move |input: u32, _| {
        let active = Arc::clone(&member_active);
        let peak = Arc::clone(&member_peak);
        let completed = Arc::clone(&member_completed);
        let second_finished = second_finished.clone();
        async move {
            let count = active.fetch_add(1, Ordering::SeqCst) + 1;
            let _guard = ActiveMember(active);
            peak.fetch_max(count, Ordering::SeqCst);
            if input == 0 {
                second_finished.cancelled().await;
            }
            switchy::unsync::time::sleep(Duration::from_millis(u64::from(6 - input))).await;
            completed.lock().expect("completion recorder").push(input);
            if input == 1 {
                second_finished.cancel();
            }
            Ok(input * 2)
        }
    });
    let workflow = bcode::workflow::WorkflowBuilder::new(
        "sdk-fan-out",
        bcode::workflow::fan_out("members", step, 2),
    )
    .build()
    .expect("fan-out workflow");
    assert_eq!(
        workflow
            .run(vec![0, 1, 2, 3, 4])
            .await
            .expect("fan-out result"),
        [0, 2, 4, 6, 8]
    );
    assert_eq!(peak.load(Ordering::SeqCst), 2);
    assert_eq!(active.load(Ordering::SeqCst), 0);
    let completion_order = completed.lock().expect("completion recorder");
    assert_eq!(completion_order.first(), Some(&1));
    assert_eq!(completion_order.len(), 5);
}

async fn run_parallel_workflow() {
    let left = bcode::workflow::Step::map("left", |input: u32| Ok(input + 1));
    let right = bcode::workflow::Step::map("right", |input: u32| Ok(input + 2));
    let step = bcode::workflow::parallel_named_with_policy(
        "join",
        bcode::workflow::ParallelFailurePolicy::FailFast,
        left,
        right,
    );
    let workflow = bcode::workflow::WorkflowBuilder::new("sdk-parallel", step)
        .build()
        .expect("parallel workflow");
    assert_eq!(workflow.run(40).await.expect("parallel result"), (41, 42));
}

async fn run_parallel_cancellation() {
    let parent = bcode::workflow::WorkflowCancellation::new();
    let signal = parent.clone();
    let left_finished = bcode::workflow::WorkflowCancellation::new();
    let finished = left_finished.clone();
    let left = bcode::workflow::Step::map("left", move |(): ()| {
        finished.cancel();
        Ok(())
    });
    let right = bcode::workflow::Step::task("right", move |(): (), context| {
        let signal = signal.clone();
        let left_finished = left_finished.clone();
        async move {
            left_finished.cancelled().await;
            // Let the join consume the successful branch before cancelling its parent.
            switchy::unsync::task::yield_now().await;
            signal.cancel();
            context.cancellation().cancelled().await;
            Err::<(), _>(bcode::workflow::WorkflowError::Cancelled {
                step: "right".to_owned(),
            })
        }
    });
    let step = bcode::workflow::parallel_named_with_policy(
        "join",
        bcode::workflow::ParallelFailurePolicy::FailFast,
        left,
        right,
    );
    let workflow = bcode::workflow::WorkflowBuilder::new("parallel-cancellation", step)
        .build()
        .expect("workflow");
    let result = switchy::unsync::time::timeout(
        Duration::from_secs(1),
        workflow.run_with_cancellation((), parent),
    )
    .await
    .expect("parent cancellation reaches remaining branch");
    assert!(matches!(
        result,
        Err(bcode::workflow::WorkflowError::Cancelled { .. })
    ));
}

async fn run_parallel_failure() {
    struct Release(bcode::workflow::WorkflowCancellation);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.cancel();
        }
    }
    for failing_left in [false, true] {
        let started = bcode::workflow::WorkflowCancellation::new();
        let released = bcode::workflow::WorkflowCancellation::new();
        let start = started.clone();
        let release = released.clone();
        let pending = bcode::workflow::Step::task("pending", move |(): (), _context| {
            let start = start.clone();
            let guard = Release(release.clone());
            async move {
                let _guard = guard;
                start.cancel();
                std::future::pending::<Result<(), bcode::workflow::WorkflowError>>().await
            }
        });
        let failure = bcode::workflow::Step::task("failure", move |(): (), _context| {
            let started = started.clone();
            async move {
                started.cancelled().await;
                Err::<(), _>(bcode::workflow::WorkflowError::step(
                    "failure",
                    "fixture failure",
                ))
            }
        });
        let (left, right) = if failing_left {
            (failure, pending)
        } else {
            (pending, failure)
        };
        let step = bcode::workflow::parallel_named_with_policy(
            "join",
            bcode::workflow::ParallelFailurePolicy::FailFast,
            left,
            right,
        );
        let workflow = bcode::workflow::WorkflowBuilder::new("parallel-failure", step)
            .build()
            .expect("workflow");
        let result = switchy::unsync::time::timeout(Duration::from_secs(1), workflow.run(()))
            .await
            .expect("failure does not wait for pending sibling");
        assert!(matches!(result,
            Err(bcode::workflow::WorkflowError::Step { step, message })
                if step == "failure" && message == "fixture failure"
        ));
        assert!(
            released.is_cancelled(),
            "pending sibling future released before return"
        );
    }
}

#[cfg(feature = "config")]
async fn run_controlled_provider_context() -> bcode::Result<()> {
    use std::collections::BTreeMap;

    let mut config = bcode_config::BcodeConfig::default();
    config.model.provider_plugin_id = Some("context-provider".into());
    config.model.model_id = Some("context-model".into());
    config.model.auth_pool = Some("context-pool".into());
    let environment = bcode_config::ConfigEnvironmentSnapshot::isolated("sdk-simulation");
    let subscriptions = bcode_config::RuntimeAuthSubscriptions {
        pools: BTreeMap::from([(
            "context-pool".into(),
            bcode_config::RuntimeAuthSubscriptionPool {
                preferred_profile: Some("context-profile".into()),
                profiles: vec![bcode_config::RuntimeAuthSubscriptionProfile {
                    auth_profile: "context-profile".into(),
                    storage_profile: "stored-profile".into(),
                    vault: "/fixture/not-a-real-vault".into(),
                    provider: "openai".into(),
                    scheme: "api_key".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    let mut acquisitions = Vec::new();
    let sdk = bcode::Bcode::builder().provider_defaults_with_auth_resolver(
        &config,
        &environment,
        &subscriptions,
        |name, profile| {
            acquisitions.push(name.to_owned());
            assert_eq!(profile.backend, "sshenv");
            bcode_provider_auth::ResolvedProviderAuth {
                auth: bcode_model::ProviderAuthContext {
                    scheme: profile.scheme.clone(),
                    ..Default::default()
                },
                env: BTreeMap::new(),
            }
        },
    );
    let sdk = sdk.build();
    let context = sdk.provider_context().clone();
    assert_eq!(acquisitions, ["context-profile"]);
    assert_eq!(context.auth_pool.as_deref(), Some("context-pool"));
    assert_eq!(context.auth_profile.as_deref(), Some("context-profile"));
    assert_eq!(context.auth_candidates.len(), 1);
    assert_eq!(
        context
            .auth
            .as_ref()
            .and_then(|auth| auth.scheme.as_deref()),
        Some("api_key")
    );
    let session_id = "00000000-0000-4000-8000-000000000026"
        .parse()
        .expect("fixture ID");
    let identities = ScriptedRequestIdentities::new([ProviderRequestIdentity {
        session_id,
        turn_id: "controlled-context".into(),
    }])?;
    let agent = sdk
        .agent_from_context(session_id, "/fixture".into())
        .runtime(AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)))
        .build();
    struct ContextProvider(bcode::ProviderRequestContext);
    impl bcode::InProcessModelProvider for ContextProvider {
        fn run_turn(
            &self,
            request: bcode_model::ModelTurnRequest,
            _context: bcode::InProcessProviderContext,
        ) -> bcode::InProcessProviderFuture<'_> {
            assert_eq!(request.provider_context, self.0);
            assert_eq!(request.model_id, "context-model");
            Box::pin(async { Ok(bcode::InProcessProviderOutcome::EndTurn) })
        }
    }
    let mut provider = bcode::InProcessModelProviderAdapter::new(ContextProvider(context));
    let response = agent
        .generate_text_with_provider(&mut provider, "controlled context")
        .await;
    provider.shutdown_wait().await?;
    assert_eq!(
        response?.runtime.stop_reason,
        Some(bcode::StopReason::EndTurn)
    );
    Ok(())
}

async fn run() -> bcode::Result<()> {
    #[cfg(feature = "config")]
    run_controlled_provider_context().await?;
    run_workflow_repeat_cancellation().await;
    run_workflow_panic().await;
    for resource_claim in [false, true] {
        for cancel_waiter in [false, true] {
            run_workflow_contention(resource_claim, cancel_waiter).await;
        }
    }
    run_parallel_wait_all().await;
    run_parallel_cancellation().await;
    run_parallel_failure().await;
    run_fan_out_cancellation(false).await;
    run_fan_out_cancellation(true).await;
    run_fan_out_failure(false).await;
    run_fan_out_failure(true).await;
    run_fan_out_workflow().await;
    run_parallel_workflow().await;
    run_retry_cancellation().await;
    run_workflow_retry().await;
    run_workflow_cancellation(false).await?;
    run_workflow_cancellation(true).await?;
    run_explicit_workflow().await?;
    run_cache_lookup_panic().await?;
    run_cache_storage_panic().await?;
    run_cache_clock_errors().await?;
    for mode in [
        InProcessCleanup::Shutdown,
        InProcessCleanup::ShutdownWait,
        InProcessCleanup::AdapterDrop,
        InProcessCleanup::Cancel,
        InProcessCleanup::Deadline,
    ] {
        run_in_process_cleanup(mode).await?;
    }
    let mut in_process = bcode::InProcessModelProviderAdapter::new(InProcessEcho);
    let session_id = "00000000-0000-4000-8000-000000000023"
        .parse()
        .expect("fixture ID");
    let identities = ScriptedRequestIdentities::new(["echo-first", "echo-reuse"].map(|turn_id| {
        ProviderRequestIdentity {
            session_id,
            turn_id: turn_id.into(),
        }
    }))?;
    let agent = AgentBuilder::from_context(session_id, "/".into())
        .runtime(AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)))
        .build();
    for prompt in ["in-process smoke", "in-process reuse"] {
        let response = agent
            .generate_text_with_provider(&mut in_process, prompt)
            .await?;
        assert_eq!(response.text, "in-process answer");
        assert_eq!(
            response.runtime.stop_reason,
            Some(bcode::StopReason::EndTurn)
        );
    }
    in_process.shutdown_wait().await?;
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
            // A malformed provider batch must not reopen or overwrite completion.
            ProviderTurnEvent::TextDelta {
                text: "late text must not appear".into(),
            },
            ProviderTurnEvent::TurnFinished {
                stop_reason: bcode::StopReason::Cancelled,
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

    let mut provider_owner = provider.clone();
    let transcript = TextStreamRecorder::new(agent.stream_text_with_provider(provider, "hello"))
        .finish_up_to(100)
        .await;
    provider_owner.shutdown_wait().await?;
    let response = transcript
        .assert_finished()
        .expect("coherent successful stream");
    assert_eq!(response.text, "scripted answer");
    let deltas: Vec<_> = transcript
        .events()
        .into_iter()
        .filter_map(|event| match event {
            bcode::AgentEvent::TextDelta(text) => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, ["scripted answer"]);
    probe
        .assert_requests(&[ScriptedRequestExpectation::new()
            .provider_plugin_id("test-provider")
            .model_id("test-model")])
        .expect("captured request");
    probe.assert_finish_count(1).expect("provider cleanup");
    run_response_cache_scenario().await?;
    run_rate_limit_scenarios().await?;
    run_retry_scenarios().await?;
    run_response_cache_failure().await?;
    run_response_cache_cancellation().await?;
    run_terminal_scenarios().await?;
    run_tool_scenarios(false).await?;
    run_tool_scenarios(true).await?;
    for capacity in [1, 2, 4, 32] {
        run_backpressure_scenario(
            std::num::NonZeroUsize::new(capacity).expect("positive capacity"),
        )
        .await?;
    }
    run_pending_tool_cancellation(ToolCancellation::Explicit).await?;
    run_pending_tool_cancellation(ToolCancellation::StreamDrop).await?;
    run_pending_tool_cancellation(ToolCancellation::RecorderBudget).await?;
    run_pending_tool_cancellation(ToolCancellation::Deadline).await?;
    for operation in [ProviderFailure::Event, ProviderFailure::Poll] {
        for partial_output in [false, true] {
            run_provider_error(operation, partial_output).await?;
        }
    }
    run_provider_error(ProviderFailure::Start, false).await?;
    run_pre_cancelled().await?;
    run_sibling_cancellation().await?;
    // Keep the known abandonment regression mandatory, but run it last so it
    // cannot mask tool, permission, backpressure, and provider-error coverage.
    run_response_cache_terminal_case(CacheTermination::Drop).await?;
    Ok(())
}

#[derive(Default)]
struct PanickingStorageCache {
    aborts: std::sync::atomic::AtomicUsize,
    puts: std::sync::atomic::AtomicUsize,
    response: std::sync::Mutex<Option<bcode::GenerateTextResponse>>,
}

impl bcode::ModelResponseCache for PanickingStorageCache {
    fn get(
        &self,
        _request: &bcode::AgentTurnRequest,
    ) -> bcode::Result<Option<bcode::GenerateTextResponse>> {
        Ok(self.response.lock().expect("fixture cache lock").clone())
    }

    fn put(
        &self,
        _request: &bcode::AgentTurnRequest,
        response: &bcode::GenerateTextResponse,
    ) -> bcode::Result<()> {
        if self.puts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            panic!("fixture storage panic payload");
        }
        *self.response.lock().expect("fixture cache lock") = Some(response.clone());
        Ok(())
    }

    fn abort(&self, _request: &bcode::AgentTurnRequest) {
        self.aborts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

async fn run_cache_storage_panic() -> bcode::Result<()> {
    let session_id = "00000000-0000-4000-8000-000000000021"
        .parse()
        .expect("fixture ID");
    let identities = ScriptedRequestIdentities::new([
        ProviderRequestIdentity {
            session_id,
            turn_id: "cache-storage-panic".into(),
        },
        ProviderRequestIdentity {
            session_id,
            turn_id: "cache-storage-recovery".into(),
        },
    ])?;
    let cache = Arc::new(PanickingStorageCache::default());
    let agent = AgentBuilder::from_context(session_id, "/".into())
        .runtime(AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)))
        .provider_plugin("test-provider")
        .model("test-model")
        .response_cache(cache.clone())
        .build();
    let mut provider = ScriptedProvider::new([
        ScriptedProviderTurn::complete_text("completed before storage"),
        ScriptedProviderTurn::complete_text("recovered after storage panic"),
    ]);
    let probe = provider.probe();
    let result = agent
        .generate_text_with_provider(&mut provider, "storage panic")
        .await;
    assert!(
        matches!(result, Err(bcode::BcodeError::Cache(message)) if message == "cache storage task failed")
    );
    assert_eq!(probe.requests().len(), 1);
    probe
        .assert_finish_count(1)
        .expect("provider finished before cache storage");
    probe
        .assert_cancellation_count(0)
        .expect("completed provider not cancelled");
    assert_eq!(cache.aborts.load(std::sync::atomic::Ordering::SeqCst), 1);
    for cached in [false, true] {
        let response = agent
            .generate_text_with_provider(&mut provider, "storage panic")
            .await?;
        assert_eq!(response.text, "recovered after storage panic");
        assert_recovery_cache_status(&response, cached);
    }
    assert_eq!(probe.requests().len(), 2);
    probe
        .assert_finish_count(2)
        .expect("both providers finished");
    probe
        .assert_cancellation_count(0)
        .expect("recovery not cancelled");
    assert_eq!(cache.puts.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(cache.aborts.load(std::sync::atomic::Ordering::SeqCst), 1);
    Ok(())
}

async fn run_cache_clock_errors() -> bcode::Result<()> {
    let session_id = "00000000-0000-4000-8000-000000000022"
        .parse()
        .expect("fixture ID");
    for lease_overflow in [false, true] {
        let identities =
            ScriptedRequestIdentities::new((0..2).map(|attempt| ProviderRequestIdentity {
                session_id,
                turn_id: format!("cache-overflow-{lease_overflow}-{attempt}"),
            }))?;
        let cache = Arc::new(
            bcode::InMemoryModelResponseCache::new(
                if lease_overflow {
                    Duration::from_secs(60)
                } else {
                    Duration::MAX
                },
                std::num::NonZeroUsize::new(1).expect("positive capacity"),
            )
            .with_single_flight_timeout(if lease_overflow {
                Duration::MAX
            } else {
                Duration::from_secs(30)
            }),
        );
        let agent = AgentBuilder::from_context(session_id, "/".into())
            .runtime(
                AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)),
            )
            .provider_plugin("test-provider")
            .model("test-model")
            .response_cache(cache.clone())
            .build();
        let mut provider = ScriptedProvider::new([
            ScriptedProviderTurn::complete_text("overflow fixture"),
            ScriptedProviderTurn::complete_text("overflow fixture"),
        ]);
        let probe = provider.probe();
        for attempt in 1..=2 {
            let result = agent
                .generate_text_with_provider(&mut provider, "overflow")
                .await;
            assert!(matches!(result, Err(bcode::BcodeError::Cache(_))));
            let expected = if lease_overflow { 0 } else { attempt };
            assert_eq!(probe.requests().len(), expected);
            probe
                .assert_finish_count(expected)
                .expect("provider finished before storage error");
            probe
                .assert_cancellation_count(0)
                .expect("completed provider not cancelled");
        }
        cache.invalidate_all()?;
    }
    Ok(())
}

struct PanickingLookupCache;

impl bcode::ModelResponseCache for PanickingLookupCache {
    fn get(
        &self,
        _request: &bcode::AgentTurnRequest,
    ) -> bcode::Result<Option<bcode::GenerateTextResponse>> {
        panic!("fixture lookup panic payload");
    }

    fn put(
        &self,
        _request: &bcode::AgentTurnRequest,
        _response: &bcode::GenerateTextResponse,
    ) -> bcode::Result<()> {
        panic!("failed lookup must not reach storage");
    }
}

async fn run_cache_lookup_panic() -> bcode::Result<()> {
    let session_id = "00000000-0000-4000-8000-000000000020"
        .parse()
        .expect("fixture ID");
    let agent = AgentBuilder::from_context(session_id, "/".into())
        .provider_plugin("test-provider")
        .model("test-model")
        .response_cache(Arc::new(PanickingLookupCache))
        .build();
    let mut provider = ScriptedProvider::new([ScriptedProviderTurn::complete_text("must not run")]);
    let probe = provider.probe();
    let result = agent
        .generate_text_with_provider(&mut provider, "lookup panic")
        .await;
    assert!(
        matches!(result, Err(bcode::BcodeError::Cache(message)) if message == "cache lookup task failed")
    );
    assert!(
        probe.requests().is_empty(),
        "failed lookup must not dispatch provider"
    );
    probe
        .assert_finish_count(0)
        .expect("no provider cleanup without start");
    Ok(())
}

fn assert_cancelled_cache_write(response: &bcode::GenerateTextResponse) -> bcode::Result<()> {
    use bcode::ModelResponseCache;

    let cache = bcode::InMemoryModelResponseCache::new(
        Duration::from_secs(60),
        std::num::NonZeroUsize::new(1).expect("positive capacity"),
    );
    let request = bcode::AgentTurnRequest::new("test-model", "cancelled cache write");
    request.cancellation.cancel();
    assert!(matches!(
        cache.put(&request, response),
        Err(bcode::BcodeError::Runtime(bcode::RuntimeError::Cancelled))
    ));
    let fresh = bcode::AgentTurnRequest::new("test-model", "cancelled cache write");
    assert!(
        cache.get(&fresh)?.is_none(),
        "cancelled write must not populate cache"
    );
    cache.abort(&fresh);
    for lease_overflow in [false, true] {
        let cache = bcode::InMemoryModelResponseCache::new(
            if lease_overflow {
                Duration::from_secs(60)
            } else {
                Duration::MAX
            },
            std::num::NonZeroUsize::new(1).expect("positive capacity"),
        )
        .with_single_flight_timeout(if lease_overflow {
            Duration::MAX
        } else {
            Duration::from_secs(30)
        });
        let result = if lease_overflow {
            cache.get(&fresh).map(|_| ())
        } else {
            assert!(cache.get(&fresh)?.is_none());
            cache.put(&fresh, response)
        };
        assert!(
            matches!(result, Err(bcode::BcodeError::Cache(_))),
            "unrepresentable cache duration must fail"
        );
        cache.abort(&fresh);
        cache.invalidate_all()?;
    }
    Ok(())
}

async fn run_sibling_cancellation() -> bcode::Result<()> {
    let mut streams = Vec::new();
    for index in 0..2 {
        let session_id = format!("00000000-0000-4000-8000-00000000000{}", index + 8)
            .parse()
            .expect("fixture ID");
        let identities = ScriptedRequestIdentities::new([ProviderRequestIdentity {
            session_id,
            turn_id: format!("sibling-{index}"),
        }])?;
        let agent = AgentBuilder::from_context(session_id, "/".into())
            .runtime(
                AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)),
            )
            .provider_plugin("test-provider")
            .model("test-model")
            .build();
        let turn = ScriptedProviderTurn::new().events([ProviderTurnEvent::TextDelta {
            text: format!("sibling-{index}"),
        }]);
        let provider = ScriptedProvider::new([turn.pending()]);
        let probe = provider.probe();
        let cancellation = bcode::CancellationToken::new();
        let recorder = TextStreamRecorder::new(agent.stream_text_with_provider_and_cancellation(
            provider,
            "concurrent streams",
            cancellation.clone(),
        ));
        streams.push((recorder, cancellation, probe));
    }
    // Both producers have been spawned before either consumer is drained.
    for (recorder, _, _) in &mut streams {
        assert_eq!(recorder.consume_up_to(2).await, 2);
    }
    streams[0].1.cancel();
    for (index, (recorder, cancellation, probe)) in streams.into_iter().enumerate() {
        // The second provider cannot finish by itself: its script remains pending.
        // Check after the first stream's terminal has been consumed, then explicitly
        // cancel the sibling rather than relying on a delay to establish overlap.
        if index == 1 {
            assert!(!cancellation.is_cancelled());
            probe
                .assert_cancellation_count(0)
                .expect("sibling was not cancelled");
            probe
                .assert_finish_count(0)
                .expect("sibling remains active");
            cancellation.cancel();
        }
        let transcript = recorder.finish_up_to(100).await;
        transcript
            .assert_cancelled()
            .expect("explicitly selected stream cancelled");
        transcript
            .assert_event_order(&[
                bcode::AgentEvent::TurnStarted,
                bcode::AgentEvent::TextDelta(format!("sibling-{index}")),
            ])
            .expect("each stream receives only its own ordered events");
        probe
            .assert_finish_count(1)
            .expect("each provider finishes once");
        probe
            .assert_cancellation_count(1)
            .expect("each explicit cancellation delivered once");
        assert_eq!(probe.requests().len(), 1);
        assert_eq!(
            probe.requests()[0].request.turn_id,
            format!("sibling-{index}")
        );
    }
    Ok(())
}

async fn run_response_cache_scenario() -> bcode::Result<()> {
    let session_id = "00000000-0000-4000-8000-000000000008"
        .parse()
        .expect("fixture ID");
    let identities = ScriptedRequestIdentities::new((0..3).map(|index| ProviderRequestIdentity {
        session_id,
        turn_id: format!("cache-{index}"),
    }))?;
    let cache = Arc::new(bcode::InMemoryModelResponseCache::new(
        Duration::from_secs(1),
        std::num::NonZeroUsize::new(2).expect("positive capacity"),
    ));
    let agent = AgentBuilder::from_context(session_id, "/".into())
        .runtime(AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)))
        .provider_plugin("test-provider")
        .model("test-model")
        .response_cache(cache)
        .build();
    let mut provider = ScriptedProvider::new([
        ScriptedProviderTurn::complete_text("before expiry"),
        ScriptedProviderTurn::complete_text("after expiry"),
    ]);
    let probe = provider.probe();
    for populated in [false, true] {
        let cancellation = bcode::CancellationToken::new();
        cancellation.cancel();
        let cancelled = agent
            .generate_text_with_provider_and_cancellation(&mut provider, "cached", cancellation)
            .await;
        assert!(
            matches!(
                cancelled,
                Err(bcode::BcodeError::Runtime(bcode::RuntimeError::Cancelled))
            ),
            "pre-cancelled request must not succeed"
        );
        assert_eq!(probe.requests().len(), usize::from(populated));
        probe
            .assert_finish_count(usize::from(populated))
            .expect("cancelled request starts no provider");
        let response = agent
            .generate_text_with_provider(&mut provider, "cached")
            .await?;
        assert_eq!(response.text, "before expiry");
        assert_cancelled_cache_write(&response)?;
        assert_eq!(probe.requests().len(), 1, "hit must not dispatch provider");
        probe
            .assert_finish_count(1)
            .expect("miss provider released");
    }
    let streaming_provider =
        ScriptedProvider::new([ScriptedProviderTurn::complete_text("stream bypasses cache")]);
    let streaming_probe = streaming_provider.probe();
    let transcript =
        TextStreamRecorder::new(agent.stream_text_with_provider(streaming_provider, "cached"))
            .finish_up_to(100)
            .await;
    assert_eq!(
        transcript.assert_finished().expect("stream succeeds").text,
        "stream bypasses cache"
    );
    assert_eq!(streaming_probe.requests().len(), 1);
    streaming_probe
        .assert_finish_count(1)
        .expect("stream provider released");
    streaming_probe
        .assert_cancellation_count(0)
        .expect("stream not cancelled");
    let response = agent
        .generate_text_with_provider(&mut provider, "cached")
        .await?;
    assert_eq!(
        response.text, "before expiry",
        "stream must not overwrite cache"
    );
    assert_eq!(probe.requests().len(), 1, "buffered entry remains cached");
    switchy::unsync::time::sleep(Duration::from_secs(2)).await;
    let response = agent
        .generate_text_with_provider(&mut provider, "cached")
        .await?;
    assert_eq!(response.text, "after expiry");
    assert_eq!(
        probe.requests().len(),
        2,
        "expired entry must dispatch provider"
    );
    probe
        .assert_finish_count(2)
        .expect("both providers released");
    probe.assert_cancellation_count(0).expect("no cancellation");
    Ok(())
}

async fn run_response_cache_cancellation() -> bcode::Result<()> {
    for mode in [CacheTermination::Cancel, CacheTermination::Deadline] {
        run_response_cache_terminal_case(mode).await?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum CacheTermination {
    Cancel,
    Deadline,
    Drop,
}

async fn run_response_cache_terminal_case(mode: CacheTermination) -> bcode::Result<()> {
    let session_id = "00000000-0000-4000-8000-000000000019"
        .parse()
        .expect("fixture ID");
    let identities = ScriptedRequestIdentities::new((0..2).map(|index| ProviderRequestIdentity {
        session_id,
        turn_id: format!("cache-cancel-{index}"),
    }))?;
    let timeout = if matches!(mode, CacheTermination::Deadline) {
        Duration::from_secs(2)
    } else {
        Duration::from_secs(120)
    };
    let agent = AgentBuilder::from_context(session_id, "/".into())
        .timeout(timeout)
        .runtime(
            AgentRuntime::new()
                .with_poll_interval(Duration::from_mins(1))
                .with_provider_request_identity_source(Arc::new(identities)),
        )
        .provider_plugin("test-provider")
        .model("test-model")
        .response_cache(Arc::new(bcode::InMemoryModelResponseCache::new(
            Duration::from_secs(60),
            std::num::NonZeroUsize::new(2).expect("positive capacity"),
        )))
        .build();
    let mut provider = ScriptedProvider::new([
        ScriptedProviderTurn::new().pending(),
        ScriptedProviderTurn::complete_text("after cancellation"),
    ]);
    let probe = provider.probe();
    let cancellation = bcode::CancellationToken::new();
    let generation_started = switchy::time::instant_now();
    let mut generation = Box::pin(agent.generate_text_with_provider_and_cancellation(
        &mut provider,
        "recover",
        cancellation.clone(),
    ));
    loop {
        switchy::unsync::select! {
            result = &mut generation => panic!("pending provider completed: {result:?}"),
            () = switchy::unsync::task::yield_now() => {
                if !probe.requests().is_empty() { break; }
            }
        }
    }
    let termination_started = switchy::time::instant_now();
    if matches!(mode, CacheTermination::Drop) {
        drop(generation);
    } else if matches!(mode, CacheTermination::Deadline) {
        assert!(matches!(generation.await,
            Err(bcode::BcodeError::Runtime(bcode::RuntimeError::Timeout { timeout: actual })) if actual == timeout
        ));
        assert!(
            switchy::time::instant_now().duration_since(generation_started) >= timeout,
            "product deadline must not fire early"
        );
    } else {
        cancellation.cancel();
        assert!(matches!(
            generation.await,
            Err(bcode::BcodeError::Runtime(bcode::RuntimeError::Cancelled))
        ));
    }
    assert!(
        switchy::time::instant_now().duration_since(termination_started) < Duration::from_secs(30),
        "{mode:?}: termination must interrupt the one-minute poll interval"
    );
    probe
        .assert_finish_count(1)
        .unwrap_or_else(|error| panic!("{mode:?}: cancelled provider not released: {error:?}"));
    probe
        .assert_cancellation_count(1)
        .expect("provider cancelled once");
    let started = switchy::time::instant_now();
    for cached in [false, true] {
        let response = agent
            .generate_text_with_provider(&mut provider, "recover")
            .await?;
        assert_eq!(response.text, "after cancellation");
        assert_recovery_cache_status(&response, cached);
        assert_eq!(probe.requests().len(), 2, "recovery cached");
    }
    assert!(switchy::time::instant_now().duration_since(started) < Duration::from_secs(30));
    probe
        .assert_finish_count(2)
        .expect("all providers released");
    probe
        .assert_cancellation_count(1)
        .expect("recovery not cancelled");
    Ok(())
}

fn assert_recovery_cache_status(response: &bcode::GenerateTextResponse, cached: bool) {
    assert!(
        matches!(
            (&response.cache_status, cached),
            (bcode::ModelResponseCacheStatus::Stored { .. }, false)
                | (bcode::ModelResponseCacheStatus::Hit { .. }, true)
        ),
        "unexpected recovery cache provenance: {:?}",
        response.cache_status
    );
}

async fn run_response_cache_failure() -> bcode::Result<()> {
    for started in [false, true] {
        run_response_cache_failure_case(started).await?;
    }
    Ok(())
}

async fn run_response_cache_failure_case(started: bool) -> bcode::Result<()> {
    let session_id = "00000000-0000-4000-8000-000000000009"
        .parse()
        .expect("fixture ID");
    let identities = ScriptedRequestIdentities::new((0..2).map(|index| ProviderRequestIdentity {
        session_id,
        turn_id: format!("cache-failure-{index}"),
    }))?;
    let agent = AgentBuilder::from_context(session_id, "/".into())
        .runtime(AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)))
        .provider_plugin("test-provider")
        .model("test-model")
        .response_cache(Arc::new(bcode::InMemoryModelResponseCache::new(
            Duration::from_secs(60),
            std::num::NonZeroUsize::new(2).expect("positive capacity"),
        )))
        .build();
    let error = bcode::ProviderError {
        code: "cache_fixture_failure".into(),
        category: bcode::ProviderErrorCategory::ProviderInternal,
        message: "cache fixture failure".into(),
        retryable: false,
        provider_message: None,
        failure: None,
        request_id: None,
        diagnostic_context: Box::default(),
        sources: Box::default(),
        retry: None,
    };
    let failed_turn = if started {
        ScriptedProviderTurn::new().poll_error(error)
    } else {
        ScriptedProviderTurn::start_error(error)
    };
    let mut provider = ScriptedProvider::new([
        failed_turn,
        ScriptedProviderTurn::complete_text("recovered cache miss"),
    ]);
    let probe = provider.probe();
    let failure = agent
        .generate_text_with_provider(&mut provider, "recover")
        .await
        .expect_err("scripted provider must fail");
    assert!(
        matches!(
            failure,
            bcode::BcodeError::Runtime(bcode::RuntimeError::Provider { ref code, .. })
                if code == "cache_fixture_failure"
        ),
        "unexpected failure: {failure:?}"
    );
    assert_eq!(probe.requests().len(), 1);
    probe
        .assert_finish_count(usize::from(started))
        .expect("only a started provider has a handle to release");
    // A leaked miss lease must not be allowed to expire and mask missing abort cleanup.
    let recovery_started = switchy::time::instant_now();
    for cached in [false, true] {
        let response = agent
            .generate_text_with_provider(&mut provider, "recover")
            .await?;
        assert_eq!(response.text, "recovered cache miss");
        assert_recovery_cache_status(&response, cached);
        assert_eq!(probe.requests().len(), 2, "recovery is cached");
    }
    assert!(
        switchy::time::instant_now().duration_since(recovery_started) < Duration::from_secs(30)
    );
    probe
        .assert_finish_count(1 + usize::from(started))
        .expect("failed and recovered providers released");
    probe
        .assert_cancellation_count(usize::from(started))
        .expect("only failed started provider cancelled");
    Ok(())
}

struct FixtureRateLimiter(std::result::Result<bcode::ApplicationRateLimitDecision, String>);

impl bcode::ApplicationRateLimiter for FixtureRateLimiter {
    fn check(
        &self,
        request: &bcode::AgentTurnRequest,
    ) -> std::result::Result<bcode::ApplicationRateLimitDecision, String> {
        assert_eq!(request.model_id, "test-model");
        self.0.clone()
    }
}

async fn run_rate_limit_scenarios() -> bcode::Result<()> {
    for outcome in 0..3 {
        let decision = match outcome {
            0 => Ok(bcode::ApplicationRateLimitDecision::Allow),
            1 => Ok(bcode::ApplicationRateLimitDecision::Deny {
                reason: "fixture quota".into(),
                retry_at_unix: Some(1_700_000_001),
            }),
            _ => Err("fixture unavailable".into()),
        };
        let session_id = "00000000-0000-4000-8000-000000000010"
            .parse()
            .expect("fixture ID");
        let identities = ScriptedRequestIdentities::new([ProviderRequestIdentity {
            session_id,
            turn_id: format!("rate-limit-{outcome}"),
        }])?;
        let agent = AgentBuilder::from_context(session_id, "/".into())
            .runtime(
                AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)),
            )
            .provider_plugin("test-provider")
            .model("test-model")
            .middleware_layer(bcode::RateLimitMiddleware::new(
                "fixture",
                Arc::new(FixtureRateLimiter(decision)),
            ))
            .build();
        let mut provider = ScriptedProvider::new([ScriptedProviderTurn::complete_text("admitted")]);
        let probe = provider.probe();
        let result = agent
            .generate_text_with_provider(&mut provider, "limited")
            .await;
        match outcome {
            0 => assert_eq!(result?.text, "admitted"),
            1 => assert!(
                matches!(result, Err(bcode::BcodeError::RateLimited { limiter_id, reason, retry_at_unix: Some(1_700_000_001) }) if limiter_id == "fixture" && reason == "fixture quota")
            ),
            _ => assert!(
                matches!(result, Err(bcode::BcodeError::RateLimiter { limiter_id, message }) if limiter_id == "fixture" && message == "fixture unavailable")
            ),
        }
        let admitted = usize::from(outcome == 0);
        assert_eq!(
            probe.requests().len(),
            admitted,
            "only admitted requests dispatch"
        );
        probe
            .assert_finish_count(admitted)
            .expect("only admitted provider finishes");
        probe
            .assert_cancellation_count(0)
            .expect("no provider cancellation");
    }
    Ok(())
}

async fn run_retry_scenarios() -> bcode::Result<()> {
    for (retryable, retries, exhausted, hint_ms) in [
        (true, 1, false, None),
        (true, 0, false, None),
        (false, 1, false, None),
        (true, 1, true, None),
        (true, 1, false, Some(50)),
    ] {
        let session_id = "00000000-0000-4000-8000-000000000011"
            .parse()
            .expect("fixture ID");
        let identities =
            ScriptedRequestIdentities::new((0..2).map(|index| ProviderRequestIdentity {
                session_id,
                turn_id: format!("retry-{retryable}-{retries}-{index}"),
            }))?;
        let agent = AgentBuilder::from_context(session_id, "/".into())
            .runtime(
                AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)),
            )
            .provider_plugin("test-provider")
            .model("test-model")
            .retry_policy(
                bcode::RetryPolicy::new(retries, Duration::from_millis(10))
                    .with_max_delay(Duration::from_millis(100)),
            )
            .build();
        let error = bcode::ProviderError {
            code: "retry_fixture".into(),
            category: bcode::ProviderErrorCategory::ProviderInternal,
            message: "retry fixture".into(),
            retryable,
            provider_message: None,
            failure: None,
            request_id: None,
            diagnostic_context: Box::default(),
            sources: Box::default(),
            retry: hint_ms.map(|delay| {
                Box::new(bcode::ProviderRetryHint {
                    retry_after_ms: Some(delay),
                    retry_at_unix: None,
                    source: Some("fixture".into()),
                })
            }),
        };
        let mut turns = vec![ScriptedProviderTurn::start_error(error.clone())];
        if exhausted {
            let mut final_error = error;
            final_error.code = "retry_exhausted".into();
            turns.push(ScriptedProviderTurn::start_error(final_error));
        }
        turns.push(ScriptedProviderTurn::complete_text("retry recovered"));
        let mut provider = ScriptedProvider::new(turns);
        let probe = provider.probe();
        let started = switchy::time::instant_now();
        let result = agent
            .generate_text_with_provider(&mut provider, "retry")
            .await;
        let retried = retryable && retries > 0;
        let recovered = retried && !exhausted;
        if retried {
            assert!(
                switchy::time::instant_now().duration_since(started)
                    >= Duration::from_millis(hint_ms.unwrap_or(10))
            );
        }
        if recovered {
            assert_eq!(result?.text, "retry recovered");
        } else {
            let expected_code = if exhausted {
                "retry_exhausted"
            } else {
                "retry_fixture"
            };
            assert!(
                matches!(result, Err(bcode::BcodeError::Runtime(bcode::RuntimeError::Provider { code, .. })) if code == expected_code)
            );
        }
        assert_eq!(probe.requests().len(), 1 + usize::from(retried));
        probe
            .assert_finish_count(usize::from(recovered))
            .expect("only successful start is finished");
        probe.assert_cancellation_count(0).expect("no cancellation");
    }
    Ok(())
}

async fn run_pre_cancelled() -> bcode::Result<()> {
    let session_id = "00000000-0000-4000-8000-000000000007"
        .parse()
        .expect("fixture ID");
    let identities = ScriptedRequestIdentities::new([ProviderRequestIdentity {
        session_id,
        turn_id: "pre-cancelled-0".into(),
    }])?;
    let agent = AgentBuilder::from_context(session_id, "/".into())
        .runtime(AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)))
        .provider_plugin("test-provider")
        .model("test-model")
        .build();
    let provider = ScriptedProvider::new([ScriptedProviderTurn::complete_text("must not start")]);
    let probe = provider.probe();
    let cancellation = bcode::CancellationToken::new();
    cancellation.cancel();
    let transcript = TextStreamRecorder::new(agent.stream_text_with_provider_and_cancellation(
        provider,
        "already cancelled",
        cancellation,
    ))
    .finish_up_to(100)
    .await;
    transcript
        .assert_cancelled()
        .expect("coherent pre-start cancellation");
    probe
        .assert_requests(&[])
        .expect("no provider request after cancellation");
    probe
        .assert_finish_count(0)
        .expect("no provider round to finish");
    probe
        .assert_cancellation_count(0)
        .expect("no provider round to cancel");
    Ok(())
}

#[derive(Clone, Copy)]
enum ProviderFailure {
    Start,
    Poll,
    Event,
}

async fn run_provider_error(operation: ProviderFailure, partial_output: bool) -> bcode::Result<()> {
    let session_id = "00000000-0000-4000-8000-000000000006"
        .parse()
        .expect("fixture ID");
    let identities = ScriptedRequestIdentities::new([ProviderRequestIdentity {
        session_id,
        turn_id: "provider-error-0".into(),
    }])?;
    let agent = AgentBuilder::from_context(session_id, "/".into())
        .runtime(AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)))
        .provider_plugin("test-provider")
        .model("test-model")
        .retry_policy(bcode::RetryPolicy::new(
            u32::from(partial_output),
            Duration::from_millis(10),
        ))
        .build();
    let error = bcode::ProviderError {
        code: "fixture_failure".into(),
        category: bcode::ProviderErrorCategory::ProviderInternal,
        message: "fixture failure".into(),
        retryable: partial_output,
        provider_message: None,
        failure: None,
        request_id: None,
        diagnostic_context: Box::default(),
        sources: Box::default(),
        retry: None,
    };
    let deltas = partial_output.then(|| ProviderTurnEvent::TextDelta {
        text: "partial".into(),
    });
    let turn = match operation {
        ProviderFailure::Start => ScriptedProviderTurn::start_error(error),
        ProviderFailure::Poll => ScriptedProviderTurn::new().events(deltas).poll_error(error),
        ProviderFailure::Event => ScriptedProviderTurn::new().events(
            deltas
                .into_iter()
                .chain([ProviderTurnEvent::Error { error }]),
        ),
    };
    let provider = ScriptedProvider::new([
        turn,
        ScriptedProviderTurn::complete_text("must not retry visible output"),
    ]);
    let probe = provider.probe();
    let transcript =
        TextStreamRecorder::new(agent.stream_text_with_provider(provider, "fail after text"))
            .finish_up_to(100)
            .await;
    let error = transcript
        .assert_runtime_error()
        .expect("coherent error terminal");
    let source = if partial_output {
        let bcode::RuntimeError::ProviderAfterOutput(source) = error else {
            panic!("expected provider failure after output, got {error:?}");
        };
        source.as_ref()
    } else {
        error
    };
    assert!(
        matches!(source, bcode::RuntimeError::Provider { code, .. } if code == "fixture_failure")
    );
    let deltas: Vec<_> = transcript
        .events()
        .into_iter()
        .filter_map(|event| match event {
            bcode::AgentEvent::TextDelta(text) => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(
        deltas,
        if partial_output {
            vec!["partial"]
        } else {
            vec![]
        }
    );
    let started = usize::from(!matches!(operation, ProviderFailure::Start));
    probe
        .assert_finish_count(started)
        .expect("finish only a started turn");
    probe
        .assert_cancellation_count(started)
        .expect("cancel only a started turn");
    probe
        .assert_requests(&[ScriptedRequestExpectation::new()
            .provider_plugin_id("test-provider")
            .model_id("test-model")])
        .expect("no extra provider request");
    Ok(())
}

enum ToolCancellation {
    Explicit,
    StreamDrop,
    RecorderBudget,
    Deadline,
}

async fn run_pending_tool_cancellation(mode: ToolCancellation) -> bcode::Result<()> {
    let session_id = "00000000-0000-4000-8000-000000000005"
        .parse()
        .expect("fixture ID");
    let identities = ScriptedRequestIdentities::new([ProviderRequestIdentity {
        session_id,
        turn_id: "pending-tool-0".into(),
    }])?;
    let tool = ScriptedTool::new([ScriptedToolOutcome::PendingUntilCancelled]);
    let probe = tool.probe();
    let permissions = ScriptedPermissionPolicy::new([bcode::PermissionDecision::Allow]);
    let permission_probe = permissions.clone();
    let builder = AgentBuilder::from_context(session_id, "/".into())
        .runtime(AgentRuntime::new().with_provider_request_identity_source(Arc::new(identities)))
        .provider_plugin("test-provider")
        .model("test-model");
    // A deadline must not rescue broken drop-cancellation in the other scenarios.
    let builder = if matches!(mode, ToolCancellation::Deadline) {
        builder.timeout(Duration::from_secs(2))
    } else {
        builder
    };
    let agent = tool
        .register(
            builder,
            bcode::ToolDefinition {
                name: "scripted".into(),
                description: "Fixture tool".into(),
                input_schema: serde_json::json!({"type": "object"}),
            },
        )
        .custom_permission_policy(permissions)
        .build();
    let provider = ScriptedProvider::new([ScriptedProviderTurn::new().events([
        ProviderTurnEvent::ToolCallFinished {
            call: bcode::ToolCall {
                id: "pending-call".into(),
                name: "scripted".into(),
                arguments: serde_json::json!({"input": 1}),
            },
        },
        ProviderTurnEvent::TurnFinished {
            stop_reason: bcode::StopReason::ToolCall,
        },
    ])]);
    let provider_probe = provider.probe();
    let cancellation = bcode::CancellationToken::new();
    let stream = agent.stream_text_with_provider_and_cancellation(
        provider,
        "cancel active tool",
        cancellation.clone(),
    );
    // Observe invocation admission before cancellation; a fixed yield count cannot
    // establish that the permission and tool path was actually reached.
    for _ in 0..1_000 {
        if probe.invocation_count() == 1 {
            break;
        }
        switchy::unsync::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(probe.invocation_count(), 1);
    assert_eq!(probe.active_invocation_count(), 1);
    assert_eq!(permission_probe.requests().len(), 1);
    match mode {
        ToolCancellation::Deadline => {
            let transcript = TextStreamRecorder::new(stream).finish_up_to(100).await;
            assert!(matches!(
                transcript
                    .assert_runtime_error()
                    .expect("coherent tool deadline terminal"),
                bcode::RuntimeError::Timeout { .. }
            ));
        }
        ToolCancellation::StreamDrop => drop(stream),
        ToolCancellation::RecorderBudget => {
            let transcript = TextStreamRecorder::new(stream).finish_up_to(1).await;
            assert_eq!(transcript.items().len(), 1);
            assert!(!transcript.is_exhausted());
            assert!(transcript.assert_finished().is_err());
        }
        ToolCancellation::Explicit => {
            cancellation.cancel();
            let transcript = TextStreamRecorder::new(stream).finish_up_to(100).await;
            transcript
                .assert_cancelled()
                .expect("active tool cancellation reaches coherent terminal");
        }
    }
    for _ in 0..1_000 {
        if probe.active_invocation_count() == 0 {
            break;
        }
        switchy::unsync::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(probe.active_invocation_count(), 0, "tool future released");
    provider_probe
        .assert_requests(&[ScriptedRequestExpectation::new()
            .provider_plugin_id("test-provider")
            .model_id("test-model")])
        .expect("no provider continuation after tool cancellation");
    provider_probe
        .assert_finish_count(1)
        .expect("initial provider round finished once");
    Ok(())
}

async fn run_backpressure_scenario(capacity: std::num::NonZeroUsize) -> bcode::Result<()> {
    let session_id = "00000000-0000-4000-8000-000000000004"
        .parse()
        .expect("fixture ID");
    let identities = ScriptedRequestIdentities::new([ProviderRequestIdentity {
        session_id,
        turn_id: "backpressure-0".into(),
    }])?;
    let agent = AgentBuilder::from_context(session_id, "/".into())
        .runtime(
            AgentRuntime::new()
                .with_provider_request_identity_source(Arc::new(identities))
                .with_stream_buffer_capacity(capacity),
        )
        .provider_plugin("test-provider")
        .model("test-model")
        .build();
    let provider = ScriptedProvider::new([ScriptedProviderTurn::new()
        .events((0..8).map(|index| ProviderTurnEvent::TextDelta {
            text: index.to_string(),
        }))
        .events([ProviderTurnEvent::TurnFinished {
            stop_reason: bcode::StopReason::EndTurn,
        }])]);
    let probe = provider.probe();
    let stream = agent.stream_text_with_provider(provider, "buffered burst");
    // Wait for observable provider cleanup, not an assumed number of scheduler
    // yields. The consumer remains detached while the bounded producer fills.
    for _ in 0..1_000 {
        if probe.assert_finish_count(1).is_ok() {
            break;
        }
        switchy::unsync::time::sleep(Duration::from_millis(1)).await;
    }
    probe
        .assert_finish_count(1)
        .expect("producer finished within fixture budget");
    let transcript = TextStreamRecorder::new(stream).finish_up_to(100).await;
    if capacity.get() == 32 {
        let response = transcript.assert_finished().expect("burst fits buffer");
        assert_eq!(response.text, "01234567");
        let deltas: Vec<_> = transcript
            .events()
            .into_iter()
            .filter_map(|event| match event {
                bcode::AgentEvent::TextDelta(text) => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(
            deltas,
            (0..8).map(|index| index.to_string()).collect::<Vec<_>>()
        );
        probe
            .assert_cancellation_count(0)
            .expect("successful burst is not cancelled");
    } else {
        transcript
            .assert_backpressure_overflow(capacity.get())
            .expect("overflow is typed and terminal, not silent event loss");
        probe
            .assert_cancellation_count(1)
            .expect("overflow cancels provider work");
    }
    probe
        .assert_requests(&[ScriptedRequestExpectation::new()
            .provider_plugin_id("test-provider")
            .model_id("test-model")])
        .expect("overflow does not start another request");
    Ok(())
}

async fn run_tool_scenarios(retry: bool) -> bcode::Result<()> {
    for (decision, outcome, expected_error, expected) in [
        (
            Some(bcode::PermissionDecision::Allow),
            ScriptedToolOutcome::text("tool output"),
            false,
            "tool output",
        ),
        (
            Some(bcode::PermissionDecision::Deny("fixture denial".into())),
            ScriptedToolOutcome::text("must not execute"),
            true,
            "tool execution denied: fixture denial",
        ),
        (
            Some(bcode::PermissionDecision::Allow),
            ScriptedToolOutcome::text("delayed output").after(Duration::from_millis(3)),
            false,
            "delayed output",
        ),
        (
            Some(bcode::PermissionDecision::Allow),
            ScriptedToolOutcome::Error("fixture failure".into()),
            true,
            "fixture failure",
        ),
        (
            None,
            ScriptedToolOutcome::text("exhaustion must not execute"),
            true,
            "tool execution denied: scripted permission decisions exhausted",
        ),
    ] {
        let allowed = matches!(decision, Some(bcode::PermissionDecision::Allow));
        let session_id = "00000000-0000-4000-8000-000000000003"
            .parse()
            .expect("fixture ID");
        let identities =
            ScriptedRequestIdentities::new((0..3).map(|index| ProviderRequestIdentity {
                session_id,
                turn_id: format!("tool-{allowed}-{index}"),
            }))?;
        let permissions = ScriptedPermissionPolicy::new(decision);
        let permission_probe = permissions.clone();
        let tool = ScriptedTool::new([outcome]);
        let tool_probe = tool.probe();
        let agent = tool
            .register(
                AgentBuilder::from_context(session_id, "/".into())
                    .runtime(
                        AgentRuntime::new()
                            .with_provider_request_identity_source(Arc::new(identities)),
                    )
                    .provider_plugin("test-provider")
                    .model("test-model")
                    .retry_policy(bcode::RetryPolicy::new(1, Duration::from_millis(10))),
                bcode::ToolDefinition {
                    name: "scripted".into(),
                    description: "Fixture tool".into(),
                    input_schema: serde_json::json!({"type": "object"}),
                },
            )
            .custom_permission_policy(permissions)
            .build();
        let mut turns = vec![ScriptedProviderTurn::new().events([
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
        ])];
        if retry {
            turns.push(ScriptedProviderTurn::start_error(bcode::ProviderError {
                code: "continuation_retry".into(),
                category: bcode::ProviderErrorCategory::ProviderInternal,
                message: "fixture continuation failure".into(),
                retryable: true,
                provider_message: None,
                failure: None,
                request_id: None,
                diagnostic_context: Box::default(),
                sources: Box::default(),
                retry: None,
            }));
        }
        turns.push(ScriptedProviderTurn::complete_text("after tool"));
        let mut provider = ScriptedProvider::new(turns);
        let probe = provider.probe();
        let response = agent.run(&mut provider, "use tool").await?;
        assert_eq!(response.text, "after tool");
        assert_eq!(tool_probe.invocation_count(), usize::from(allowed));
        assert_eq!(
            tool_probe.active_invocation_count(),
            0,
            "completed tool invocation released"
        );
        let requests = permission_probe.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].context.session_id, session_id);
        assert!(response.steps.iter().any(|step| matches!(
            step,
            bcode::GenerationStep::ToolResult { result, .. }
                if result.is_error == expected_error &&
                    (if allowed && expected_error { result.output.contains(expected) }
                     else { result.output == expected })
        )));
        if allowed {
            assert_eq!(tool_probe.invocations()[0].request.arguments["input"], 1);
        }
        let provider_requests = probe.requests();
        assert_eq!(
            provider_requests.len(),
            2 + usize::from(retry),
            "exact continuation attempts"
        );
        if retry {
            assert_eq!(
                provider_requests[1].request.messages, provider_requests[2].request.messages,
                "retry preserves the committed tool result"
            );
        }
        let results: Vec<_> = provider_requests[1]
            .request
            .messages
            .iter()
            .filter(|message| message.role == bcode::MessageRole::Tool)
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                bcode::ModelContentBlock::ToolResult { result } => Some(result),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 1, "one tool result delivered to provider");
        assert_eq!(results[0].is_error, expected_error);
        assert!(
            response.steps.iter().any(|step| matches!(
                step, bcode::GenerationStep::ToolResult { result, .. } if result == results[0]
            )),
            "continuation must carry the actual runtime result"
        );
        probe
            .assert_finish_count(2)
            .expect("both provider requests finished");
        probe
            .assert_cancellation_count(0)
            .expect("successful continuation does not cancel providers");
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
        if cancelled {
            cancellation.cancel();
        }
        let transcript = recorder.finish_up_to(100).await;
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
