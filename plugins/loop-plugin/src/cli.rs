//! Plugin-owned noninteractive goal entry. No slash-command emulation.
#[cfg(test)]
use super::build_start_request;
use super::{
    BcodeClient, DEFAULT_MAX_ITERATIONS, LoopWorkflowInput, MAX_PROMPT_BYTES, SessionId, SetupKind,
    build_start_request_with_policy, goal, legacy_state_exists, unsupported_legacy_message,
};
use bcode_plugin_sdk::{StaticCliFuture, StaticCliOutcome, StaticCliRegistration};
use clap::{CommandFactory, FromArgMatches, Parser};

#[derive(Debug, Parser)]
#[command(
    name = "goal",
    about = "Generate and start a durable goal with the configured real model"
)]
struct GoalCli {
    /// Persisted source for bounded context capture and the goal's workspace/association.
    /// Model selection and authorization use ordinary invocation configuration.
    #[arg(long)]
    session: SessionId,
    /// User objective; generated instructions retain this verbatim as task data.
    #[arg(long)]
    objective: String,
    /// Use an authored stop condition instead of model-generated goal instructions.
    #[arg(long)]
    stop_condition: Option<String>,
    #[arg(long, default_value = "")]
    guidance: String,
    #[command(flatten)]
    swarm: super::swarm::Arguments,
    #[arg(long, default_value_t = DEFAULT_MAX_ITERATIONS)]
    max_iterations: u64,
    /// Opt into a judgement provider plugin (for example bcode.jev).
    #[arg(long, requires_all = ["judge_model", "judge_account"])]
    judge_provider: Option<String>,
    /// Exact judgement model ID; does not change the conversational model.
    #[arg(long, requires = "judge_provider")]
    judge_model: Option<String>,
    /// Provider-owned account; an explicit empty value selects daemon environment credentials.
    #[arg(long, requires = "judge_provider")]
    judge_account: Option<String>,
    /// Required completion probability percentage.
    #[arg(long, requires = "judge_provider", value_parser = clap::value_parser!(u8).range(1..=100))]
    judge_threshold: Option<u8>,
    /// Behavior if judgement fails (default: pause).
    #[arg(long, requires = "judge_provider", value_enum)]
    judge_failure: Option<JudgeFailure>,

    #[command(flatten)]
    output: GoalOutput,
    /// Opt in to a living progress document and its planning phase.
    #[arg(long, conflicts_with = "no_progress_document")]
    progress_document: bool,
    /// Keep progress-document planning disabled (the default).
    #[arg(long)]
    no_progress_document: bool,
}

/// Definition preparation and machine-readable output are independent of goal
/// execution policy and document planning.
#[derive(Debug, clap::Args)]
struct GoalOutput {
    /// Prepare and register the exact goal definition without provider generation,
    /// run admission or dispatch. Requires an authored stop condition and prohibits
    /// progress-document writes.
    #[arg(long, requires_all = ["json", "stop_condition"], conflicts_with = "progress_document")]
    prepare: bool,
    /// Emit the prepared typed workflow start request as JSON.
    #[arg(long)]
    json: bool,
    /// Preselect the exact fresh run identity for a separately configured
    /// diagnostic grant. Preparation does not admit this run or grant capture.
    #[arg(long, requires = "prepare", value_parser = parse_prepared_run_id)]
    run_id: Option<String>,
}

fn parse_prepared_run_id(value: &str) -> Result<String, String> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err("Prepared run identity must be nonempty, at most 256 bytes, and contain no control characters".into());
    }
    Ok(value.to_owned())
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum JudgeFailure {
    Pause,
    AgentFallback,
}

impl GoalCli {
    fn generation_request(
        &self,
        guidance: &str,
    ) -> bcode_plugin_sdk::tui::PluginStructuredGenerationRequest {
        goal::generation_request_with_policy(
            &self.objective,
            guidance,
            self.progress_document,
            self.swarm.mode().collaboration(),
            self.swarm.swarm_policy,
        )
    }

    fn judgement(&self) -> Result<Option<super::judgement_evaluation::EvaluationConfig>, String> {
        let Some(provider) = &self.judge_provider else {
            return Ok(None);
        };
        let config = super::judgement_evaluation::EvaluationConfig {
            provider_plugin_id: provider.clone(),
            model_id: self.judge_model.clone().ok_or("Missing judge model")?,
            auth_profile: self.judge_account.clone().ok_or("Missing judge account")?,
            threshold_percent: self.judge_threshold.unwrap_or(90),
            on_failure: match self.judge_failure.unwrap_or(JudgeFailure::Pause) {
                JudgeFailure::Pause => super::judgement_evaluation::FailurePolicy::Pause,
                JudgeFailure::AgentFallback => {
                    super::judgement_evaluation::FailurePolicy::AgentFallback
                }
            },
        };
        config.validate().map_err(str::to_string)?;
        Ok(Some(config))
    }
}

pub fn registration() -> StaticCliRegistration {
    StaticCliRegistration {
        requires_daemon: |_| true,
        command: GoalCli::command,
        invoke,
    }
}

fn invoke(matches: clap::ArgMatches) -> StaticCliFuture {
    Box::pin(async move {
        let args = GoalCli::from_arg_matches(&matches).map_err(|error| error.to_string())?;
        run(args).await?;
        Ok(StaticCliOutcome::default())
    })
}

async fn check_delegation(client: &BcodeClient) -> Result<(), String> {
    let client = client.clone();
    goal::require_delegation(Box::pin(async move {
        client
            .workflow_delegation_preflight("bcode.workflow".into())
            .await
            .map_err(|error| bcode_plugin_sdk::tui::PluginTuiHostError::Internal(error.to_string()))
    }))
    .await
}

fn validate_args(args: &GoalCli) -> Result<(), String> {
    if args.output.prepare && (!args.output.json || args.progress_document) {
        return Err("Goal preparation requires --json and forbids progress-document writes".into());
    }
    if args.output.run_id.is_some() && !args.output.prepare {
        return Err("A preselected run identity requires --prepare".into());
    }
    if args.output.prepare && args.stop_condition.is_none() {
        return Err("Goal preparation requires an authored --stop-condition; provider generation is not preparation".into());
    }
    LoopWorkflowInput::new(
        args.objective.clone(),
        "validation".into(),
        args.max_iterations,
    )?;
    if args.guidance.len() > MAX_PROMPT_BYTES {
        return Err("additional guidance is too large".into());
    }
    if legacy_state_exists(args.session) {
        return Err(unsupported_legacy_message().into());
    }
    Ok(())
}

async fn run(args: GoalCli) -> Result<(), String> {
    validate_args(&args)?;
    let client = BcodeClient::default_endpoint();
    let mut request = build_goal_request(&client, &args).await?;
    if let Some(run_id) = &args.output.run_id {
        request.run_id = Some(parse_prepared_run_id(run_id)?);
    }
    if args.output.prepare {
        let prepared = prepare(&client, request).await?;
        println!(
            "{}",
            serde_json::to_string(&prepared).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    let started = admit(&client, request).await?;
    println!(
        "{}",
        serde_json::json!({"session_id": args.session, "run_id": started.run.run_id, "runtime_work_id": started.runtime_work_id, "status": "admitted (not completed)"})
    );
    Ok(())
}

/// Build the exact goal definition and input with ordinary preflight checks and
/// explicitly requested document setup. The caller validates preparation before
/// entering here, preventing generation and document mutation in that mode.
async fn build_goal_request(
    client: &BcodeClient,
    args: &GoalCli,
) -> Result<bcode_plugin_sdk::tui::PluginWorkflowStartRequest, String> {
    let judgement = args.judgement()?;
    check_judgement(client, judgement.as_ref()).await?;
    let collaboration = args.swarm.mode().collaboration();
    let guidance = super::swarm::guidance(&args.guidance, &args.swarm.swarm_instructions);
    if guidance.len() > MAX_PROMPT_BYTES {
        return Err("combined guidance is too large".into());
    }
    if collaboration == crate::CollaborationMode::Requested {
        check_delegation(client).await?;
    }
    let progress = args.progress_document;
    let (mut input, source) = if let Some(condition) = &args.stop_condition {
        if progress {
            return Err(
                "Authored stop conditions cannot be combined with progress-document generation"
                    .into(),
            );
        }
        let prompt = format!("{}\n\n{}", args.objective, guidance);
        (
            LoopWorkflowInput::new(prompt, condition.clone(), args.max_iterations)?,
            None,
        )
    } else {
        let mut generation = args.generation_request(&guidance);
        generation.source_session_id = Some(args.session);
        let generated = client
            .generate_structured_output(
                generation,
                bcode_plugin_sdk::generation::PluginStructuredGenerationControl::default(),
            )
            .await
            .map_err(|error| error.to_string())?;
        let source = generated
            .source
            .ok_or("Generation returned no source context provenance")?;
        if source.session_id != args.session {
            return Err("Generation context belongs to another session".into());
        }
        let provenance = format!(
            "Generation source session {} at sequence {} (generation {}).\n",
            source.session_id, source.latest_sequence, source.generation
        );
        (
            goal::decode_generation(
                generated.output,
                &args.objective,
                &guidance,
                args.max_iterations,
                &provenance,
            )?,
            Some(source),
        )
    };
    input.judgement_evaluation = judgement;
    let mut request = build_start_request_with_policy(
        &input,
        args.session,
        SetupKind::Goal,
        collaboration,
        progress,
        args.swarm.worker_attempts.map(std::num::NonZeroU64::get),
        args.swarm.swarm_policy,
    )?;
    if let Some(concurrency) = args.swarm.concurrency {
        request.limits.concurrency_cap = Some(concurrency.get());
    }
    if collaboration == crate::CollaborationMode::Requested {
        check_delegation(client).await?;
    }
    check_judgement(client, input.judgement_evaluation.as_ref()).await?;
    if progress {
        let setup = goal::ProgressDocumentSetup::headless(
            args.objective.clone(),
            guidance,
            source.ok_or("Missing generation source")?,
        );
        let document = client
            .session_working_document(setup.request(
                args.session,
                request.run_id.as_deref().ok_or("Missing run identity")?,
            ))
            .await
            .map_err(|error| error.to_string())?;
        let document = document.ok_or("Progress document preparation returned no document")?;
        goal::attach_document(&mut request, &document)?;
    }
    Ok(request)
}

async fn check_judgement(
    client: &BcodeClient,
    config: Option<&super::judgement_evaluation::EvaluationConfig>,
) -> Result<(), String> {
    let Some(config) = config else {
        return Ok(());
    };
    let selection = bcode_model::judgement::SetupSelection {
        provider_plugin_id: config.provider_plugin_id.clone(),
        model_id: config.model_id.clone(),
        auth_profile: config.auth_profile.clone(),
    };
    if client
        .validate_judgement_setup(&selection)
        .await
        .map_err(|error| error.to_string())?
    {
        Ok(())
    } else {
        Err("Judgement model or account is unavailable; check provider enablement and authentication".into())
    }
}

async fn admit(
    client: &BcodeClient,
    request: bcode_plugin_sdk::tui::PluginWorkflowStartRequest,
) -> Result<bcode_workflow::WorkflowRunStartResponse, String> {
    client
        .start_workflow(start_request(request))
        .await
        .map_err(|error| error.to_string())
}

/// Exact conversion shared by normal admission and preparation. Empty workspace
/// means the server derives the canonical parent-session workspace at admission,
/// identically to normal goal start; the caller must not guess from process cwd.
fn start_request(
    request: bcode_plugin_sdk::tui::PluginWorkflowStartRequest,
) -> bcode_workflow::WorkflowStartRequest {
    let limits = &request.limits;
    bcode_workflow::WorkflowStartRequest {
        identity: request.identity,
        definition: request.definition,
        run_id: request.run_id,
        workspace_snapshot: None,
        parent_session_id: request.parent_session_id,
        input: request.input,
        binding: bcode_workflow_store::WorkflowRunBinding {
            owner_plugin_id: request.binding.owner_plugin_id,
            workflow_kind: request.binding.workflow_kind,
            scope_key: request.binding.scope_key,
            display_label: request.binding.display_label,
            single_active: request.binding.single_active,
        },
        limits: bcode_workflow_store::WorkflowRunLimits {
            deadline_at_ms: limits.maximum_duration_ms.map(|duration| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|now| u64::try_from(now.as_millis()).unwrap_or(u64::MAX))
                    .unwrap_or_default()
                    .saturating_add(duration)
            }),
            node_execution_cap: limits.node_execution_cap,
            concurrency_cap: limits.concurrency_cap,
            cycle_cap: limits.cycle_cap,
            retry_cap: limits.retry_cap,
            recursion_depth_cap: limits.recursion_depth_cap,
            descendant_cap: limits.descendant_cap,
        },
    }
}

/// Preparation persists only an exact validated compiled definition through the
/// existing idempotent registration API. It creates no workflow run, session work,
/// dispatch, document, diagnostic grant or execution authority. Ordinary start
/// authorization and session/workspace checks still run when this request is used.
async fn prepare(
    client: &BcodeClient,
    request: bcode_plugin_sdk::tui::PluginWorkflowStartRequest,
) -> Result<bcode_workflow::WorkflowRunStartRequest, String> {
    prepare_registered(request, |registration| async {
        client
            .register_workflow_definition(registration)
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    })
    .await
}

/// The preparation executor has only definition-registration capability: no
/// admission, document, tool or diagnostic-authority operation is accepted here.
async fn prepare_registered<F, Fut>(
    request: bcode_plugin_sdk::tui::PluginWorkflowStartRequest,
    register: F,
) -> Result<bcode_workflow::WorkflowRunStartRequest, String>
where
    F: FnOnce(bcode_workflow::WorkflowDefinitionRegistrationRequest) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let request = start_request(request);
    let prepared = prepared_run(&request)?;
    register(bcode_workflow::WorkflowDefinitionRegistrationRequest {
        definition_id: request.identity.definition_id,
        version: request.identity.definition_version,
        definition: request.definition,
    })
    .await?;
    Ok(prepared)
}

fn prepared_run(
    request: &bcode_workflow::WorkflowStartRequest,
) -> Result<bcode_workflow::WorkflowRunStartRequest, String> {
    if request.run_id.as_ref().is_none_or(String::is_empty) {
        return Err("Goal preparation requires a stable run identity".into());
    }
    Ok(bcode_workflow::WorkflowRunStartRequest {
        definition_id: request.identity.definition_id.clone(),
        definition_version: request.identity.definition_version,
        run_id: request.run_id.clone(),
        workspace_snapshot: request.workspace_snapshot.clone().unwrap_or_default(),
        parent_session_id: request.parent_session_id,
        parent_session_generation: None,
        binding: Some(request.binding.clone()),
        input: Some(request.input.clone()),
        limits: request.limits.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn prepare_executes_only_exact_definition_registration_and_returns_unadmitted_run() {
        let input = LoopWorkflowInput::new("Implement".into(), "Verify".into(), 2).unwrap();
        let request = build_start_request(
            &input,
            SessionId::new(),
            SetupKind::Goal,
            crate::CollaborationMode::Disabled,
            false,
            None,
        )
        .unwrap();
        let expected_run = request.run_id.clone();
        let expected_definition = request.definition.clone();
        let expected_identity = request.identity.clone();
        let expected_input = request.input.clone();
        let registrations = std::cell::Cell::new(0);
        let prepared = prepare_registered(request, |registration| {
            registrations.set(registrations.get() + 1);
            assert_eq!(registration.definition, expected_definition);
            assert_eq!(registration.definition_id, expected_identity.definition_id);
            assert_eq!(registration.version, expected_identity.definition_version);
            std::future::ready(Ok(()))
        })
        .await
        .unwrap();
        assert_eq!(registrations.get(), 1);
        assert_eq!(prepared.run_id, expected_run);
        assert_eq!(prepared.definition_id, expected_identity.definition_id);
        assert_eq!(prepared.input, Some(expected_input));
    }

    #[tokio::test]
    async fn prepare_rejects_missing_identity_before_registration_and_propagates_denial() {
        let input = LoopWorkflowInput::new("Implement".into(), "Verify".into(), 2).unwrap();
        let mut request = build_start_request(
            &input,
            SessionId::new(),
            SetupKind::Goal,
            crate::CollaborationMode::Disabled,
            false,
            None,
        )
        .unwrap();
        request.run_id = None;
        let called = std::cell::Cell::new(false);
        let result = prepare_registered(request, |_| {
            called.set(true);
            std::future::ready(Ok(()))
        })
        .await;
        assert!(result.is_err());
        assert!(!called.get());
        let request = build_start_request(
            &input,
            SessionId::new(),
            SetupKind::Goal,
            crate::CollaborationMode::Disabled,
            false,
            None,
        )
        .unwrap();
        assert_eq!(
            prepare_registered(request, |_| std::future::ready(Err(
                "registration denied".into()
            )))
            .await
            .unwrap_err(),
            "registration denied"
        );
    }

    #[test]
    fn prepare_requires_json_and_rejects_document_mutation_before_execution() {
        let id = SessionId::new().to_string();
        let base = [
            "goal",
            "--session",
            &id,
            "--objective",
            "Implement",
            "--worker-attempts",
            "1",
        ];
        assert!(GoalCli::try_parse_from(base.into_iter().chain(["--prepare"])).is_err());
        assert!(GoalCli::try_parse_from(base.into_iter().chain(["--prepare", "--json"])).is_err());
        assert!(
            GoalCli::try_parse_from(base.into_iter().chain([
                "--prepare",
                "--json",
                "--progress-document",
            ]))
            .is_err()
        );
        let args = GoalCli::try_parse_from(base.into_iter().chain([
            "--prepare",
            "--json",
            "--stop-condition",
            "Tests pass",
        ]))
        .unwrap();
        assert!(validate_args(&args).is_ok());
        assert!(args.output.prepare);
        let mut missing_condition = args;
        missing_condition.stop_condition = None;
        assert!(validate_args(&missing_condition).is_err());
        let normal = GoalCli::try_parse_from(base).unwrap();
        assert!(!normal.output.prepare);
        assert!(!normal.output.json);
    }

    #[test]
    fn preselected_preparation_identity_is_explicit_and_bounded() {
        let session = SessionId::new().to_string();
        let base = ["goal", "--session", &session, "--objective", "Implement"];
        assert!(
            GoalCli::try_parse_from(base.into_iter().chain(["--run-id", "exact-root",])).is_err()
        );
        let prepared = GoalCli::try_parse_from(base.into_iter().chain([
            "--prepare",
            "--json",
            "--stop-condition",
            "Verify",
            "--run-id",
            "exact-root",
        ]))
        .unwrap();
        assert_eq!(prepared.output.run_id.as_deref(), Some("exact-root"));
        assert!(validate_args(&prepared).is_ok());
        assert!(parse_prepared_run_id("").is_err());
        assert!(parse_prepared_run_id("run\nforeign").is_err());
        assert!(parse_prepared_run_id(&"x".repeat(257)).is_err());
        assert!(parse_prepared_run_id(&"x".repeat(256)).is_ok());
    }

    #[test]
    fn flattened_output_options_preserve_normal_goal_and_preparation_modes() {
        let session = SessionId::new().to_string();
        let base = ["goal", "--session", &session, "--objective", "Implement"];
        let normal =
            GoalCli::try_parse_from(base.into_iter().chain(["--json", "--no-progress-document"]))
                .unwrap();
        assert!(normal.output.json);
        assert!(!normal.output.prepare);
        assert!(normal.no_progress_document);
        assert!(!normal.progress_document);
        assert!(validate_args(&normal).is_ok());
        let prepared = GoalCli::try_parse_from(base.into_iter().chain([
            "--prepare",
            "--json",
            "--stop-condition",
            "Verify",
        ]))
        .unwrap();
        assert!(prepared.output.prepare && prepared.output.json);
        assert!(validate_args(&prepared).is_ok());
        assert!(
            GoalCli::try_parse_from(base.into_iter().chain([
                "--prepare",
                "--json",
                "--stop-condition",
                "Verify",
                "--progress-document",
            ]))
            .is_err()
        );
    }

    #[test]
    fn prepared_run_preserves_exact_normal_goal_input_binding_identity_and_limits() {
        let input =
            LoopWorkflowInput::new("Implement exactly".into(), "Verify exactly".into(), 3).unwrap();
        let request = build_start_request(
            &input,
            SessionId::new(),
            SetupKind::Goal,
            crate::CollaborationMode::Disabled,
            false,
            None,
        )
        .unwrap();
        let normal = start_request(request);
        let prepared = prepared_run(&normal).unwrap();
        assert_eq!(prepared.definition_id, normal.identity.definition_id);
        assert_eq!(
            prepared.definition_version,
            normal.identity.definition_version
        );
        assert_eq!(prepared.parent_session_id, normal.parent_session_id);
        assert_eq!(prepared.run_id, normal.run_id);
        assert_eq!(prepared.input, Some(normal.input.clone()));
        assert_eq!(prepared.binding, Some(normal.binding.clone()));
        assert_eq!(prepared.limits, normal.limits);
        assert!(prepared.workspace_snapshot.is_empty()); // same canonical-session derivation
        let roundtrip: bcode_workflow::WorkflowRunStartRequest =
            serde_json::from_str(&serde_json::to_string(&prepared).unwrap()).unwrap();
        assert_eq!(roundtrip, prepared);
        let mut missing_identity = normal;
        missing_identity.run_id = None;
        assert!(prepared_run(&missing_identity).is_err());
    }

    #[test]
    fn judgement_arguments_are_explicit_and_compile_into_the_goal() {
        let id = SessionId::new().to_string();
        let base = ["goal", "--session", &id, "--objective", "Implement"];
        assert!(
            GoalCli::try_parse_from(base)
                .unwrap()
                .judgement()
                .unwrap()
                .is_none()
        );
        for extra in [
            vec!["--judge-provider", "bcode.jev"],
            vec!["--judge-model", "jev-latest"],
            vec!["--judge-account", "jev"],
            vec!["--judge-threshold", "90"],
            vec!["--judge-failure", "pause"],
        ] {
            assert!(GoalCli::try_parse_from(base.into_iter().chain(extra)).is_err());
        }
        let judge = [
            "--judge-provider",
            "bcode.jev",
            "--judge-model",
            "jev-latest",
            "--judge-account",
            "jev",
        ];
        let args = GoalCli::try_parse_from(base.into_iter().chain(judge)).unwrap();
        let config = args.judgement().unwrap().unwrap();
        assert_eq!(config.threshold_percent, 90);
        assert_eq!(
            config.on_failure,
            super::super::judgement_evaluation::FailurePolicy::Pause
        );
        for threshold in ["0", "101"] {
            assert!(
                GoalCli::try_parse_from(
                    base.into_iter()
                        .chain(judge)
                        .chain(["--judge-threshold", threshold])
                )
                .is_err()
            );
        }
        let args = GoalCli::try_parse_from(base.into_iter().chain(judge).chain([
            "--judge-threshold",
            "95",
            "--judge-failure",
            "agent-fallback",
        ]))
        .unwrap();
        let custom = args.judgement().unwrap().unwrap();
        assert_eq!(custom.threshold_percent, 95);
        assert_eq!(
            custom.on_failure,
            super::super::judgement_evaluation::FailurePolicy::AgentFallback
        );
        let mut input = LoopWorkflowInput::new("Implement".into(), "Verify".into(), 1).unwrap();
        input.judgement_evaluation = Some(config.clone());
        let request = build_start_request(
            &input,
            SessionId::new(),
            SetupKind::Goal,
            crate::CollaborationMode::Disabled,
            false,
            None,
        )
        .unwrap();
        assert_eq!(
            request.input["judgement_evaluation"],
            serde_json::to_value(config).unwrap()
        );
        assert!(
            request
                .definition
                .nodes
                .contains_key("loop.judgement.evaluate")
        );
        let ambient = GoalCli::try_parse_from(base.into_iter().chain([
            "--judge-provider",
            "bcode.jev",
            "--judge-model",
            "jev-latest",
            "--judge-account",
            "",
        ]))
        .unwrap();
        assert_eq!(ambient.judgement().unwrap().unwrap().auth_profile, "");
    }

    #[test]
    fn headless_arguments_require_source_and_positive_worker_allowance() {
        assert!(GoalCli::try_parse_from(["goal", "--objective", "Implement"]).is_err());
        let session = uuid::Uuid::new_v4().to_string();
        assert!(
            GoalCli::try_parse_from([
                "goal",
                "--session",
                &session,
                "--objective",
                "Implement",
                "--worker-attempts",
                "0"
            ])
            .is_err()
        );
        let args = GoalCli::try_parse_from([
            "goal",
            "--session",
            &session,
            "--objective",
            "Implement",
            "--collaborate",
        ])
        .unwrap();
        assert!(args.swarm.collaborate);
        assert!(!args.no_progress_document);
        assert!(!args.progress_document);
        assert_eq!(args.max_iterations, DEFAULT_MAX_ITERATIONS);
    }

    #[test]
    fn agent_modes_and_limits_have_cli_surface_parity() {
        let session = SessionId::new();
        let id = session.to_string();
        let base = ["goal", "--session", &id, "--objective", "Implement"];
        assert_eq!(
            GoalCli::try_parse_from(base).unwrap().swarm.mode(),
            crate::swarm::Mode::Auto
        );
        assert!(
            GoalCli::try_parse_from(
                base.into_iter()
                    .chain(["--agents", "auto", "--collaborate"])
            )
            .is_err()
        );
        assert!(GoalCli::try_parse_from(base.into_iter().chain(["--concurrency", "0"])).is_err());
        let mut uncapped_surface = crate::LoopSurface::new(Some(session));
        uncapped_surface.origin = SetupKind::Goal;
        uncapped_surface.prompt = crate::text_state("Implement");
        uncapped_surface.condition = crate::text_state("Verify");
        assert_eq!(
            uncapped_surface
                .build_start_request(session)
                .unwrap()
                .limits
                .concurrency_cap,
            None
        );
        assert!(
            GoalCli::try_parse_from(base)
                .unwrap()
                .swarm
                .concurrency
                .is_none()
        );
        for mode in ["auto", "collaborate", "no-delegation"] {
            let args = GoalCli::try_parse_from(base.into_iter().chain([
                "--agents",
                mode,
                "--worker-attempts",
                "9",
                "--concurrency",
                "2",
                "--swarm-instructions",
                "Review independently",
            ]))
            .unwrap();
            let mut surface = crate::LoopSurface::new(Some(session));
            surface.origin = SetupKind::Goal;
            surface.collaboration = args.swarm.mode().collaboration();
            surface.worker_attempts = args.swarm.worker_attempts.map(std::num::NonZeroU64::get);
            surface.swarm_ui.configure(&args.swarm);
            surface.prompt = crate::text_state("Implement");
            surface.condition = crate::text_state("Verify");
            let ui = surface.build_start_request(session).unwrap();
            let input = LoopWorkflowInput::new(
                crate::swarm::guidance("Implement", &args.swarm.swarm_instructions),
                crate::swarm::guidance("Verify", &args.swarm.swarm_instructions),
                DEFAULT_MAX_ITERATIONS,
            )
            .unwrap();
            let mut cli = build_start_request(
                &input,
                session,
                SetupKind::Goal,
                args.swarm.mode().collaboration(),
                false,
                Some(9),
            )
            .unwrap();
            cli.limits.concurrency_cap = Some(2);
            assert_eq!(ui.definition, cli.definition);
            assert_eq!(ui.input, cli.input);
            assert_eq!(ui.limits, cli.limits);
        }
    }

    #[test]
    fn adaptive_policy_cli_and_surface_emit_same_candidate() {
        let session = SessionId::new();
        let session_text = session.to_string();
        for mode in ["auto", "collaborate", "no-delegation"] {
            let args = GoalCli::try_parse_from([
                "goal",
                "--session",
                &session_text,
                "--objective",
                "Implement",
                "--agents",
                mode,
                "--swarm-policy",
                "adaptive-v1",
            ])
            .unwrap();
            let mut surface = crate::LoopSurface::new(Some(session));
            surface.origin = SetupKind::Goal;
            surface.collaboration = args.swarm.mode().collaboration();
            surface.swarm_policy = args.swarm.swarm_policy;
            surface.prompt = crate::text_state("Implement");
            surface.condition = crate::text_state("Verify");
            let ui = surface.build_start_request(session).unwrap();
            let input =
                LoopWorkflowInput::new("Implement".into(), "Verify".into(), DEFAULT_MAX_ITERATIONS)
                    .unwrap();
            let cli = build_start_request_with_policy(
                &input,
                session,
                SetupKind::Goal,
                args.swarm.mode().collaboration(),
                false,
                None,
                args.swarm.swarm_policy,
            )
            .unwrap();
            assert_eq!(ui.definition, cli.definition);
            assert_eq!(ui.input, cli.input);
            assert_eq!(ui.limits, cli.limits);
            let evaluator: bcode_workflow::WorkflowPromptConfiguration = serde_json::from_value(
                ui.definition.nodes["loop.evaluation"].configuration.clone(),
            )
            .unwrap();
            assert!(
                evaluator
                    .system_prompt
                    .contains("Adaptive swarm evaluation v1")
            );
        }
    }

    #[test]
    fn progress_document_requires_explicit_opt_in() {
        let session = SessionId::new().to_string();
        let base = ["goal", "--session", &session, "--objective", "Implement"];
        assert!(!GoalCli::try_parse_from(base).unwrap().progress_document);
        let enabled =
            GoalCli::try_parse_from(base.into_iter().chain(["--progress-document"])).unwrap();
        assert!(enabled.progress_document);
        let disabled =
            GoalCli::try_parse_from(base.into_iter().chain(["--no-progress-document"])).unwrap();
        assert!(!disabled.progress_document);
        assert!(
            GoalCli::try_parse_from(
                base.into_iter()
                    .chain(["--progress-document", "--no-progress-document"]),
            )
            .is_err()
        );
    }

    #[test]
    fn headless_request_matches_surface_semantics() {
        let session = SessionId::new();
        let mut surface = crate::LoopSurface::new(Some(session));
        surface.origin = SetupKind::Goal;
        surface.collaboration = crate::CollaborationMode::Requested;
        surface.worker_attempts = Some(12);
        surface.prompt = crate::text_state("Implement");
        surface.condition = crate::text_state("Verify");
        let ui = surface.build_start_request(session).unwrap();
        let input =
            LoopWorkflowInput::new("Implement".into(), "Verify".into(), DEFAULT_MAX_ITERATIONS)
                .unwrap();
        let cli = build_start_request(
            &input,
            session,
            SetupKind::Goal,
            crate::CollaborationMode::Requested,
            false,
            Some(12),
        )
        .unwrap();
        assert_eq!(ui.definition, cli.definition);
        assert_eq!(ui.input, cli.input);
        assert_eq!(ui.binding, cli.binding);
        assert_eq!(ui.limits, cli.limits);
    }
}
