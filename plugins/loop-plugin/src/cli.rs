//! Plugin-owned noninteractive goal entry. No slash-command emulation.
use super::{
    BcodeClient, DEFAULT_MAX_ITERATIONS, LoopWorkflowInput, MAX_PROMPT_BYTES, SessionId, SetupKind,
    build_start_request, goal, legacy_state_exists, unsupported_legacy_message,
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

    /// Opt in to a living progress document and its planning phase.
    #[arg(long, conflicts_with = "no_progress_document")]
    progress_document: bool,
    /// Keep progress-document planning disabled (the default).
    #[arg(long)]
    no_progress_document: bool,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum JudgeFailure {
    Pause,
    AgentFallback,
}

impl GoalCli {
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
        requires_daemon: true,
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
    let judgement = args.judgement()?;
    check_judgement(&client, judgement.as_ref()).await?;
    let collaboration = args.swarm.mode().collaboration();
    let guidance = super::swarm::guidance(&args.guidance, &args.swarm.swarm_instructions);
    if guidance.len() > MAX_PROMPT_BYTES {
        return Err("combined guidance is too large".into());
    }
    if collaboration == crate::CollaborationMode::Requested {
        check_delegation(&client).await?;
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
        let mut generation =
            goal::generation_request(&args.objective, &guidance, progress, collaboration);
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
    let mut request = build_start_request(
        &input,
        args.session,
        SetupKind::Goal,
        collaboration,
        progress,
        args.swarm.worker_attempts.map(std::num::NonZeroU64::get),
    )?;
    if let Some(concurrency) = args.swarm.concurrency {
        request.limits.concurrency_cap = Some(concurrency.get());
    }
    if collaboration == crate::CollaborationMode::Requested {
        check_delegation(&client).await?;
    }
    check_judgement(&client, input.judgement_evaluation.as_ref()).await?;
    if progress {
        let setup = goal::ProgressDocumentSetup::headless(
            args.objective,
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
    let started = admit(&client, request).await?;
    println!(
        "{}",
        serde_json::json!({"session_id": args.session, "run_id": started.run.run_id, "runtime_work_id": started.runtime_work_id, "status": "admitted (not completed)"})
    );
    Ok(())
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
    let limits = &request.limits;
    client
        .start_workflow(bcode_workflow::WorkflowStartRequest {
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
        })
        .await
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
