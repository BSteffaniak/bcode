//! Plugin-owned noninteractive goal entry. No slash-command emulation.
use super::{
    BcodeClient, CollaborationMode, DEFAULT_MAX_ITERATIONS, LoopWorkflowInput, MAX_PROMPT_BYTES,
    SessionId, SetupKind, build_start_request, goal, legacy_state_exists,
    unsupported_legacy_message,
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
    #[arg(long, default_value = "")]
    guidance: String,
    #[arg(long)]
    collaborate: bool,
    #[arg(long, default_value_t = DEFAULT_MAX_ITERATIONS)]
    max_iterations: u64,
    #[arg(long)]
    worker_attempts: Option<std::num::NonZeroU64>,
    /// Disable the normally enabled living progress document.
    #[arg(long)]
    no_progress_document: bool,
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

async fn run(args: GoalCli) -> Result<(), String> {
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
    let client = BcodeClient::default_endpoint();
    let collaboration = if args.collaborate {
        CollaborationMode::Requested
    } else {
        CollaborationMode::Disabled
    };
    if args.collaborate {
        check_delegation(&client).await?;
    }
    let progress = !args.no_progress_document;
    let mut generation =
        goal::generation_request(&args.objective, &args.guidance, progress, collaboration);
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
    let input = goal::decode_generation(
        generated.output,
        &args.objective,
        &args.guidance,
        args.max_iterations,
        &provenance,
    )?;
    let mut request = build_start_request(
        &input,
        args.session,
        SetupKind::Goal,
        collaboration,
        progress,
        args.worker_attempts.map(std::num::NonZeroU64::get),
    )?;
    if args.collaborate {
        check_delegation(&client).await?;
    }
    if progress {
        let setup = goal::ProgressDocumentSetup::headless(args.objective, args.guidance, source);
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
        assert!(args.collaborate);
        assert!(!args.no_progress_document);
        assert_eq!(args.max_iterations, DEFAULT_MAX_ITERATIONS);
    }

    #[test]
    fn headless_request_matches_surface_semantics() {
        let session = SessionId::new();
        let mut surface = crate::LoopSurface::new(Some(session));
        surface.origin = SetupKind::Goal;
        surface.collaboration = CollaborationMode::Requested;
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
            CollaborationMode::Requested,
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
