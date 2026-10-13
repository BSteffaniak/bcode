//! Trusted one-shot execution through the same application operations as `send --wait`.

use super::{BcodeClient, Cli, CliError, ClientError, PromptInput, SessionId};
use std::path::PathBuf;
use std::time::Duration;

/// Headless prompt source and bounded execution policy.
#[derive(Debug, clap::Args)]
pub struct RunOptions {
    /// Read bounded UTF-8 prompt data from a trusted regular file.
    #[arg(long, required_unless_present = "stdin", conflicts_with = "stdin")]
    file: Option<PathBuf>,
    /// Read bounded UTF-8 prompt data from stdin; the runtime budget includes waiting for EOF.
    #[arg(long)]
    stdin: bool,
    /// Trusted working directory. This is not an OS sandbox.
    #[arg(long)]
    cwd: PathBuf,
    /// Explicit registered agent profile for unattended tool policy.
    #[arg(long, default_value = "build")]
    agent: String,
    /// Total budget for input, preflight, session creation, admission, and observation.
    #[arg(long, default_value_t = 600, value_parser = clap::value_parser!(u64).range(1..=86400))]
    timeout_secs: u64,
    /// Additional bounded time to request cancellation and observe terminal settlement.
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..=300))]
    cancellation_grace_secs: u64,
}

/// Claim a new state location before any initialization can write into it.
///
/// The parent directory must be trusted and private. Atomic creation rejects
/// existing directories, files, and symlinks; callers must never share this root
/// with another process. This is lifecycle isolation, not an OS sandbox.
///
/// # Errors
/// Returns an error when the root is not new and absolute, an external store is
/// selected, creation fails, or the failure envelope cannot be written.
pub fn claim_fresh_state(root: Option<&std::path::Path>) -> Result<(), CliError> {
    claim_fresh_state_with_environment(root, &bcode_config::ProcessConfigEnvironment)
}

fn claim_fresh_state_with_environment(
    root: Option<&std::path::Path>,
    environment: &impl bcode_config::ConfigEnvironment,
) -> Result<(), CliError> {
    let result = (|| {
        if environment
            .var_os(bcode_config::BCODE_SESSION_STORE_DIR_ENV)
            .is_some()
        {
            return Err(CliError::InvalidArguments(
                "run does not permit an external session-store override".to_owned(),
            ));
        }
        let root = root.filter(|root| root.is_absolute()).ok_or_else(|| {
            CliError::InvalidArguments(
                "run requires an unused absolute --state-root under a trusted private parent"
                    .to_owned(),
            )
        })?;
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        builder.create(root).map_err(|_| CliError::InvalidArguments(
            "run requires a freshly created private state root; the path must not exist and its parent must exist".to_owned(),
        ))
    })();
    if result.is_err() {
        super::write_json_result(
            &mut std::io::stdout().lock(),
            &serde_json::json!({
                "schema_version": 1, "session_id": null, "receipt": null, "outcome": null,
                "observation": "unverified", "cancellation_requested": false,
            }),
        )?;
    }
    result
}

fn budget_error() -> CliError {
    CliError::InvalidArguments(
        "headless runtime budget expired; do not automatically replay".to_owned(),
    )
}

/// Apply the shared headless budget without altering operation semantics.
///
/// # Errors
/// Returns the operation error, or a budget-expired error.
pub async fn request<T>(
    deadline: Option<tokio::time::Instant>,
    operation: impl std::future::Future<Output = Result<T, CliError>>,
) -> Result<T, CliError> {
    if let Some(deadline) = deadline {
        tokio::time::timeout_at(deadline, operation)
            .await
            .map_err(|_| budget_error())?
    } else {
        operation.await
    }
}

// Detached input/preflight threads do not keep the Tokio runtime alive when stdin never closes.
// They perform no admission and cannot replay a turn after a timeout.
async fn blocking<T: Send + 'static>(
    deadline: tokio::time::Instant,
    operation: impl FnOnce() -> Result<T, CliError> + Send + 'static,
) -> Result<T, CliError> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = sender.send(operation());
    });
    request(Some(deadline), async {
        receiver.await.map_err(|_| {
            CliError::InvalidArguments("headless input/preflight worker failed".to_owned())
        })?
    })
    .await
}

/// Execute exactly one prompt through application admission and observation.
///
/// # Errors
/// Returns input, preflight, transport, observation, or terminal failure errors.
pub async fn run(
    cli: &Cli,
    options: &RunOptions,
    launch_options: bcode_tui::TuiLaunchOptions,
) -> Result<(), CliError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(options.timeout_secs);
    let mut session_id = None;
    let preparation = async {
        if cli.config_only.is_none() || cli.profile.is_none() || cli.state_root.is_none() {
            return Err(CliError::InvalidArguments(
                "run requires --config-only, --profile, and --state-root; use an isolated trusted state root".to_owned(),
            ));
        }
        if cli.new || cli.onboard {
            return Err(CliError::InvalidArguments("run cannot launch onboarding or a TUI".to_owned()));
        }
        let input = PromptInput { message: None, file: options.file.clone(), stdin: options.stdin };
        let text = blocking(deadline, move || input.read()).await?;
        let selection = blocking(deadline, super::startup::headless_preflight).await?;
        let cwd = std::fs::canonicalize(&options.cwd)?;
        if !cwd.is_dir() {
            return Err(CliError::InvalidArguments("--cwd must be a directory".to_owned()));
        }
        let client = BcodeClient::default_endpoint();
        request(Some(deadline), super::ensure_server_running()).await?;
        let session = request(Some(deadline), async {
            Ok(client.create_session_in_working_directory(None, cwd).await?)
        }).await?;
        session_id = Some(session.id);
        request(Some(deadline), async {
            Ok(client.set_session_agent(session.id, options.agent.clone()).await?)
        }).await?;
        let status = request(Some(deadline), async { Ok(client.session_model_status(session.id).await?) }).await?;
        let observed_selection = (status.provider_plugin_id, status.effective_model_id, status.auth_profile);
        let requested_selection = (selection.provider_plugin_id, selection.model_id, selection.auth_profile);
        if observed_selection != requested_selection {
            return Err(CliError::InvalidArguments("daemon selection differs from explicit headless configuration; no turn submitted".to_owned()));
        }
        Ok((client, session.id, text))
    }.await;
    let (client, session_id, text) = match preparation {
        Ok(value) => value,
        Err(error) => {
            super::write_json_result(
                &mut std::io::stdout().lock(),
                &serde_json::json!({
                    "schema_version": 1, "session_id": session_id, "receipt": null,
                    "outcome": null, "observation": "unverified", "cancellation_requested": false,
                }),
            )?;
            return Err(error);
        }
    };
    super::send_message_observed(
        &client,
        session_id,
        super::SendOptions {
            input: PromptInput {
                message: Some(text),
                file: None,
                stdin: false,
            },
            placement: None,
            producer: "bcode.run".to_owned(),
            idempotency_key: None,
            background: false,
            json: true,
            observation: Some(super::TurnObservationOptions {
                timeout: deadline.saturating_duration_since(tokio::time::Instant::now()),
                cancellation_grace: Some(Duration::from_secs(options.cancellation_grace_secs)),
            }),
            launch_options,
        },
        Some(deadline),
    )
    .await
}

/// Observe exact-turn settlement while detecting noninteractive input waits.
///
/// # Errors
/// Returns transport, timeout, interaction, permission, or interruption errors.
pub async fn observe(
    client: &BcodeClient,
    watcher: &mut bcode_client::SessionWatcher,
    session_id: SessionId,
    receipt: &bcode_session_models::TurnReceipt,
    timeout: Duration,
) -> Result<bcode_session_models::ModelTurnOutcome, ClientError> {
    let turn_id = receipt.turn_id.to_string();
    let observation = watcher.recover_and_wait_for_turn_outcome(&turn_id, timeout);
    tokio::pin!(observation);
    let monitor = async {
        loop {
            let permissions = client.list_permissions().await?;
            if permissions
                .iter()
                .any(|permission| permission.session_id == session_id)
            {
                return Err(ClientError::Protocol(
                    "noninteractive run requires permission; cancellation requested".to_owned(),
                ));
            }
            let exchanges = client.list_pending_tool_exchanges().await?;
            if exchanges
                .iter()
                .any(|exchange| exchange.session_id == session_id)
            {
                return Err(ClientError::Protocol(
                    "noninteractive run requires interaction; cancellation requested".to_owned(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    tokio::select! {
        biased;
        result = &mut observation => result,
        result = monitor => result,
        signal = tokio::signal::ctrl_c() => {
            let _ = signal;
            Err(ClientError::Protocol("headless run interrupted; cancellation requested".to_owned()))
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    #[test]
    fn fresh_state_claim_rejects_reuse_without_touching_existing_contents() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("one-shot-state");
        let environment = bcode_config::ConfigEnvironmentSnapshot::isolated(parent.path());
        claim_fresh_state_with_environment(Some(&root), &environment).unwrap();
        let sentinel = root.join("existing-daemon-config");
        std::fs::write(&sentinel, "preserved endpoint/backend identity").unwrap();
        assert!(claim_fresh_state_with_environment(Some(&root), &environment).is_err());
        assert_eq!(
            std::fs::read_to_string(sentinel).unwrap(),
            "preserved endpoint/backend identity"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn fresh_state_claim_rejects_symlinks() {
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("missing-target");
        let link = parent.path().join("state-link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let environment = bcode_config::ConfigEnvironmentSnapshot::isolated(parent.path());
        assert!(claim_fresh_state_with_environment(Some(&link), &environment).is_err());
        assert!(!target.exists());
    }

    #[test]
    fn fresh_state_claim_rejects_external_store_without_creating_root() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("new-state");
        let mut environment = bcode_config::ConfigEnvironmentSnapshot::isolated(parent.path());
        environment.set_var(
            bcode_config::BCODE_SESSION_STORE_DIR_ENV,
            parent.path().join("external-store"),
        );
        assert!(claim_fresh_state_with_environment(Some(&root), &environment).is_err());
        assert!(!root.exists());
    }

    #[test]
    fn reviewed_ci_policy_parses_without_credentials_or_model_selection() {
        let config: bcode_config::BcodeConfig =
            toml::from_str(include_str!("../../../.github/bcode/ci-agent.toml")).unwrap();
        let policy = &config.agent["build"];
        assert_eq!(
            serde_json::to_value(policy.permission.external_directory).unwrap(),
            "deny"
        );
        assert!(!policy.tools["workflow.stage_delegation"]);
        assert_eq!(
            serde_json::to_value(policy.permission.command["cargo test*"]).unwrap(),
            "allow"
        );
    }

    #[test]
    fn run_selects_explicit_agent_policy() {
        let cli = Cli::try_parse_from(["bcode", "run", "--cwd", ".", "--stdin", "--agent", "plan"])
            .unwrap();
        let Some(super::super::Commands::Run(options)) = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(options.agent, "plan");
    }

    #[test]
    fn run_requires_a_single_prompt_source_and_directory() {
        for arguments in [
            vec!["bcode", "run"],
            vec!["bcode", "run", "--cwd", "."],
            vec!["bcode", "run", "--cwd", ".", "--stdin", "--file", "prompt"],
            vec![
                "bcode",
                "run",
                "--cwd",
                ".",
                "--stdin",
                "--timeout-secs",
                "0",
            ],
        ] {
            assert!(Cli::try_parse_from(arguments).is_err());
        }
        assert!(
            Cli::try_parse_from([
                "bcode",
                "--config-only",
                "trusted.toml",
                "--profile",
                "test",
                "--state-root",
                "/private/state",
                "--no-credential-discovery",
                "--json-errors",
                "run",
                "--cwd",
                ".",
                "--file",
                "prompt.txt",
                "--timeout-secs",
                "600",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(["bcode", "--profile", "test", "run", "--cwd", ".", "--stdin"])
                .is_ok()
        );
    }

    #[tokio::test]
    async fn input_budget_does_not_wait_for_a_blocked_reader() {
        let (release, wait) = std::sync::mpsc::channel();
        let result = blocking(
            tokio::time::Instant::now() + Duration::from_millis(10),
            move || {
                let _ = wait.recv();
                Ok(())
            },
        )
        .await;
        release.send(()).unwrap();
        assert!(matches!(result, Err(CliError::InvalidArguments(_))));
    }
}
