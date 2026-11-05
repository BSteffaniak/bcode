//! CLI application adapter. Native programs and credentials are never command-line arguments.
use crate::{BcodeClient, CliError};
use bcode_ipc::harness::{
    MANAGED_SESSION_INTERFACE_ID, MANAGED_SESSION_VERSION, ManagedSessionOperation as Op,
    ManagedSessionRequest,
};
use clap::Subcommand;
use std::{fs::File, io::Read as _, path::PathBuf};

/// Managed external-session commands. All output is renderer-neutral JSON.
#[derive(Debug, Subcommand)]
pub enum HarnessCommand {
    /// Create/open native transport. Prepared is not native creation success; retain its
    /// exact target for first submission and wait for `BindingConfirmed` observations.
    Start {
        #[arg(long)]
        request: PathBuf,
    },
    /// Inspect a managed session through its canonical binding.
    #[command(alias = "inspect")]
    Status {
        #[arg(long)]
        request: PathBuf,
    },
    /// Submit next-turn input or explicit capability-gated steering; never guesses placement.
    #[command(alias = "followup")]
    Send {
        #[arg(long)]
        request: PathBuf,
    },
    /// Answer one exact question/approval/assistance interaction.
    Respond {
        #[arg(long)]
        request: PathBuf,
    },
    /// Request interruption. Acknowledgement does not prove settlement.
    #[command(alias = "interrupt")]
    Cancel {
        #[arg(long)]
        request: PathBuf,
    },
    /// Read a bounded live observation page.
    Observe {
        #[arg(long)]
        request: PathBuf,
    },
    /// Reconcile retained IDs after lost acknowledgement without repeating a mutation.
    Reconcile {
        #[arg(long)]
        request: PathBuf,
    },
    /// Close native transport without assuming native work settled.
    Close {
        #[arg(long)]
        request: PathBuf,
    },
    /// Read bounded canonical history even when the plugin is disabled/offline.
    History {
        session_id: bcode_session_models::SessionId,
        #[arg(long)]
        after: Option<String>,
        #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(u16).range(1..=128))]
        limit: u16,
    },
    /// Dedicated transient authentication. Files may contain sensitive callbacks; never pass credentials as flags.
    Auth {
        #[command(subcommand)]
        command: HarnessAuthCommand,
    },
}
#[derive(Debug, Subcommand)]
pub enum HarnessAuthCommand {
    /// Execute an admitted native login in this client terminal, never daemon stdio.
    Handoff {
        #[arg(long)]
        request: PathBuf,
        #[arg(long, required = true)]
        execute: bool,
    },
    Acknowledge {
        #[arg(long)]
        request: PathBuf,
    },
    Begin {
        #[arg(long)]
        request: PathBuf,
    },
    Status {
        #[arg(long)]
        request: PathBuf,
    },
    Continue {
        #[arg(long)]
        request: PathBuf,
    },
    Cancel {
        #[arg(long)]
        request: PathBuf,
    },
}

fn read_request(path: &PathBuf) -> Result<String, CliError> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take((bcode_harness_models::MAX_HARNESS_WIRE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > bcode_harness_models::MAX_HARNESS_WIRE_BYTES {
        return Err(CliError::InvalidArguments(
            "managed request exceeds 1 MiB".into(),
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| CliError::InvalidArguments("managed request must be UTF-8 JSON".into()))
}

pub async fn execute(command: HarnessCommand) -> Result<(), CliError> {
    if let HarnessCommand::Auth {
        command:
            HarnessAuthCommand::Handoff {
                request,
                execute: true,
            },
    } = &command
    {
        return execute_handoff(request).await;
    }
    let (operation, request_json) = match command {
        HarnessCommand::Start { request } => (Op::Start, read_request(&request)?),
        HarnessCommand::Status { request } => (Op::Inspect, read_request(&request)?),
        HarnessCommand::Send { request } => (Op::Followup, read_request(&request)?),
        HarnessCommand::Respond { request } => (Op::Respond, read_request(&request)?),
        HarnessCommand::Cancel { request } => (Op::Interrupt, read_request(&request)?),
        HarnessCommand::Observe { request } => (Op::Observe, read_request(&request)?),
        HarnessCommand::Reconcile { request } => (Op::Reconcile, read_request(&request)?),
        HarnessCommand::Close { request } => (Op::Close, read_request(&request)?),
        HarnessCommand::History {
            session_id,
            after,
            limit,
        } => (
            Op::History,
            serde_json::to_string(&bcode_ipc::harness::ManagedSessionHistoryRequest {
                session_id,
                after,
                limit,
            })?,
        ),
        HarnessCommand::Auth { command } => match command {
            HarnessAuthCommand::Handoff { .. } => {
                return Err(CliError::InvalidArguments(
                    "native login requires explicit --execute".into(),
                ));
            }
            HarnessAuthCommand::Acknowledge { request } => {
                (Op::AuthAcknowledge, read_request(&request)?)
            }
            HarnessAuthCommand::Begin { request } => (Op::AuthBegin, read_request(&request)?),
            HarnessAuthCommand::Status { request } => (Op::AuthStatus, read_request(&request)?),
            HarnessAuthCommand::Continue { request } => (Op::AuthContinue, read_request(&request)?),
            HarnessAuthCommand::Cancel { request } => (Op::AuthCancel, read_request(&request)?),
        },
    };
    let request = ManagedSessionRequest {
        schema_version: MANAGED_SESSION_VERSION,
        operation,
        request_json,
    };
    if operation.is_auth() && operation != Op::AuthHandoff {
        let auth: bcode_ipc::harness::ManagedSessionAuthRequest =
            serde_json::from_str(&request.request_json)
                .map_err(|_| CliError::InvalidArguments("invalid managed auth request".into()))?;
        if auth.sensitive_interaction {
            return Err(CliError::InvalidArguments("sensitive authentication interactions require the dedicated client auth surface, not JSON stdout".into()));
        }
    }
    request
        .validate_envelope()
        .map_err(|message| CliError::InvalidArguments(message.into()))?;
    if operation == Op::History {
        request
            .decode_history()
            .map_err(|message| CliError::InvalidArguments(message.into()))?;
    } else if !operation.is_auth() {
        request
            .decode_call()
            .map_err(|message| CliError::InvalidArguments(message.into()))?;
    }
    // This interface is reserved by the daemon dispatcher, not resolved by plugin discovery.
    // Reuse the existing public client handshake/config/state-root routing, with no raw socket.
    let response = BcodeClient::default_endpoint()
        .call_plugin_service(
            MANAGED_SESSION_INTERFACE_ID.into(),
            operation.as_str().into(),
            serde_json::to_vec(&request)?,
        )
        .await?;
    if response.error.is_some()
        || response.payload.len() > bcode_harness_models::MAX_HARNESS_WIRE_BYTES
    {
        return Err(CliError::InvalidArguments(
            "managed session response unavailable".into(),
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(&response.payload)
        .map_err(|_| CliError::InvalidArguments("invalid managed session response".into()))?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

async fn terminal_request(
    client: &mut bcode_client::ClientConnection,
    operation: Op,
    request_json: String,
) -> Result<bcode_ipc::ManagedSessionResponse, CliError> {
    client
        .managed_session_auth(ManagedSessionRequest {
            schema_version: MANAGED_SESSION_VERSION,
            operation,
            request_json,
        })
        .await
        .map_err(|source| CliError::ManagedAuth { operation, source })
}

async fn prepare_verified_handoff(
    client: &mut bcode_client::ClientConnection,
    path: &PathBuf,
) -> Result<
    (
        bcode_ipc::harness::ManagedSessionHandoffRequest,
        bcode_ipc::harness::ManagedNativeLoginPlan,
    ),
    CliError,
> {
    use bcode_ipc::harness::{
        ManagedNativeLoginPlan, ManagedSessionHandoffRequest, ManagedSessionPrepareHandoffRequest,
    };
    let input: ManagedSessionPrepareHandoffRequest = serde_json::from_str(&read_request(path)?)
        .map_err(|_| CliError::InvalidArguments("invalid native handoff preparation".into()))?;
    let response =
        terminal_request(client, Op::AuthPrepare, serde_json::to_string(&input)?).await?;
    let snapshot: bcode_provider_auth_models::managed::ManagedFlowSnapshot =
        serde_json::from_str(&response.response_json)
            .map_err(|_| CliError::InvalidArguments("invalid native handoff reservation".into()))?;
    let mut selection = ManagedSessionHandoffRequest {
        session_id: input.session_id,
        instance_id: input.instance_id,
        interface_id: input.interface_id,
        handle: snapshot.handle,
        interaction_id: String::new(),
    };
    let response =
        terminal_request(client, Op::AuthStatus, serde_json::to_string(&selection)?).await?;
    let interaction: Option<(
        u64,
        bcode_provider_auth_models::managed::SensitiveAuthInteraction,
    )> = serde_json::from_str(&response.response_json)
        .map_err(|_| CliError::InvalidArguments("invalid native handoff interaction".into()))?;
    let Some((
        _,
        bcode_provider_auth_models::managed::SensitiveAuthInteraction::NativeHandoff {
            ref request_id,
        },
    )) = interaction
    else {
        return Err(CliError::InvalidArguments(
            "native handoff not available".into(),
        ));
    };
    selection.interaction_id = request_id.clone();
    let response =
        terminal_request(client, Op::AuthHandoff, serde_json::to_string(&selection)?).await?;
    let plan: ManagedNativeLoginPlan = serde_json::from_str(&response.response_json)
        .map_err(|_| CliError::InvalidArguments("invalid native login plan".into()))?;
    verify_handoff_executable(&plan).await?;
    Ok((selection, plan))
}

async fn verify_handoff_executable(
    plan: &bcode_ipc::harness::ManagedNativeLoginPlan,
) -> Result<(), CliError> {
    use std::process::Stdio;
    plan.validate()
        .map_err(|message| CliError::InvalidArguments(message.into()))?;
    // Use the admitted absolute executable directly. Never launch a shell or inherit ambient
    // credential overrides. Verify version under the same namespace before interactive login.
    let mut version = tokio::process::Command::new(&plan.executable);
    version
        .arg("--version")
        .env_clear()
        .envs(&plan.environment)
        .current_dir(&plan.working_directory)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    let mut child = version.spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| CliError::InvalidArguments("native version unavailable".into()))?;
    let verified = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        use tokio::io::AsyncReadExt as _;
        let mut bytes = Vec::new();
        (&mut stdout).take(4097).read_to_end(&mut bytes).await?;
        if bytes.len() > 4096 {
            return Ok::<_, std::io::Error>(false);
        }
        let status = child.wait().await?;
        Ok(status.success()
            && std::str::from_utf8(&bytes).is_ok_and(|text| {
                text.trim() == plan.expected_version
                    || text
                        .split_whitespace()
                        .any(|part| part == plan.expected_version)
            }))
    })
    .await;
    if !matches!(verified, Ok(Ok(true))) {
        let _ = child.kill().await;
        return Err(CliError::InvalidArguments(
            "native executable version does not match admitted plan".into(),
        ));
    }
    Ok(())
}

async fn execute_handoff(path: &PathBuf) -> Result<(), CliError> {
    use std::{io::IsTerminal as _, process::Stdio};
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(CliError::InvalidArguments(
            "native login requires an interactive client terminal".into(),
        ));
    }
    // This entry point is reached only after the explicit --execute consent gate.
    let mut client = BcodeClient::default_endpoint()
        .connect("bcode-cli-auth-terminal")
        .await?;
    let (selection, plan) = prepare_verified_handoff(&mut client, path).await?;
    let status = tokio::process::Command::new(&plan.executable)
        .args(&plan.arguments)
        .env_clear()
        .envs(&plan.environment)
        .current_dir(&plan.working_directory)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .status()
        .await?;
    // status() has waited for/reaped this exact child. Exit success is not auth readiness.
    terminal_request(
        &mut client,
        Op::AuthReaped,
        serde_json::to_string(&selection)?,
    )
    .await?;
    let response = terminal_request(
        &mut client,
        Op::AuthContinue,
        serde_json::to_string(&selection)?,
    )
    .await?;
    let snapshot: bcode_provider_auth_models::managed::ManagedFlowSnapshot =
        serde_json::from_str(&response.response_json)
            .map_err(|_| CliError::InvalidArguments("invalid auth reinspection snapshot".into()))?;
    if snapshot.phase == bcode_provider_auth_models::managed::ManagedFlowPhase::Settled {
        terminal_request(
            &mut client,
            Op::AuthAcknowledge,
            serde_json::to_string(&selection)?,
        )
        .await?;
    }
    let value: serde_json::Value = serde_json::from_str(&response.response_json)
        .map_err(|_| CliError::InvalidArguments("invalid auth status response".into()))?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    if !status.success() {
        return Err(CliError::InvalidArguments(
            "native login exited unsuccessfully; auth was reinspected without replay".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    #[derive(Parser)]
    struct Args {
        #[command(subcommand)]
        command: HarnessCommand,
    }
    #[test]
    fn commands_require_files_not_inline_credentials() {
        assert!(
            Args::try_parse_from([
                "harness",
                "auth",
                "handoff",
                "--request",
                "handoff.json",
                "--execute"
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from(["harness", "auth", "handoff", "--request", "handoff.json"])
                .is_err()
        );
        assert!(
            Args::try_parse_from(["harness", "auth", "continue", "--request", "callback.json"])
                .is_ok()
        );
        assert!(
            Args::try_parse_from(["harness", "auth", "continue", "--token", "secret"]).is_err()
        );
        assert!(Args::try_parse_from(["harness", "send", "--request", "input.json"]).is_ok());
        assert!(
            Args::try_parse_from([
                "harness",
                "history",
                "062eb7ca-1970-4e5c-9f2e-43121a388b11",
                "--limit",
                "129"
            ])
            .is_err()
        );
    }
}
