//! CLI application adapter. Native programs and credentials are never command-line arguments.
mod configured;

use crate::CliError;
use bcode_ipc::harness::{
    MANAGED_SESSION_INTERFACE_ID, MANAGED_SESSION_VERSION, ManagedSessionOperation as Op,
    ManagedSessionRequest,
};
use clap::Subcommand;
use std::{fs::File, io::Read as _, path::PathBuf};

/// Managed external-session commands. All output is renderer-neutral JSON.
#[derive(Debug, Subcommand)]
pub enum HarnessCommand {
    /// List configured enabled instances; discovery does not prove authentication readiness.
    #[command(alias = "catalog")]
    List,
    /// Open an explicitly selected configured instance; prepared transport is not confirmation.
    Open {
        #[command(flatten)]
        options: configured::OpenOptions,
    },
    /// Explicitly resume one gracefully released existing managed native session.
    /// Never resends previous input or adopts the latest unrelated native conversation.
    Recover {
        session_id: bcode_session_models::SessionId,
        #[arg(long)]
        instance: String,
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        generation: u64,
        /// Fresh retained recovery identity; repeat only to inspect, never to force replay.
        #[arg(long)]
        recovery_id: String,
    },
    /// Discover exact execution models through caller-bound managed application admission.
    Models {
        #[arg(long)]
        request: PathBuf,
    },
    /// Create a session for an exact discovered model/route/account selection without fallback.
    StartSelected {
        #[arg(long)]
        request: PathBuf,
    },
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
    /// Change one mid-session native setting (model or permission mode). Acknowledgement does
    /// not prove native effect; an ambiguous outcome is never replayed.
    Configure {
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
    ///
    /// Each page holds up to `--limit` observations across stored events. Pass the returned
    /// `next_after` as `--after` to continue; an empty page may still advance the cursor, and
    /// history is caught up only when the returned `next_after` equals `--after`.
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

fn recovery_request_json(
    session_id: bcode_session_models::SessionId,
    instance_id: String,
    expected_generation: u64,
    recovery_id: String,
) -> Result<String, CliError> {
    let recovery = bcode_ipc::harness::ManagedSessionRecoveryRequest {
        session_id,
        instance_id,
        expected_generation,
        recovery_id,
    };
    recovery
        .validate()
        .map_err(|message| CliError::InvalidArguments(message.into()))?;
    Ok(serde_json::to_string(&recovery)?)
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
        HarnessCommand::List => return configured::list().await,
        HarnessCommand::Open { options } => return configured::open(options).await,
        HarnessCommand::Recover {
            session_id,
            instance,
            generation,
            recovery_id,
        } => (
            Op::Recover,
            recovery_request_json(session_id, instance, generation, recovery_id)?,
        ),
        HarnessCommand::Models { request } => return execute_models(&request).await,
        HarnessCommand::StartSelected { request } => return execute_selected_start(&request).await,
        HarnessCommand::Start { request } => (Op::Start, read_request(&request)?),
        HarnessCommand::Status { request } => (Op::Inspect, read_request(&request)?),
        HarnessCommand::Send { request } => (Op::Followup, read_request(&request)?),
        HarnessCommand::Respond { request } => (Op::Respond, read_request(&request)?),
        HarnessCommand::Cancel { request } => (Op::Interrupt, read_request(&request)?),
        HarnessCommand::Configure { request } => (Op::Configure, read_request(&request)?),
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
    } else if operation != Op::Recover && !operation.is_auth() {
        request
            .decode_call()
            .map_err(|message| CliError::InvalidArguments(message.into()))?;
    }
    // This interface is reserved by the daemon dispatcher, not resolved by plugin discovery.
    // Reuse the existing public client handshake/config/state-root routing, with no raw socket.
    let response = bcode_client::ManagedSessionClient::default_endpoint()
        .call_plugin_service(
            MANAGED_SESSION_INTERFACE_ID.into(),
            operation.as_str().into(),
            serde_json::to_vec(&request)?,
        )
        .await?;
    validate_and_print_managed_response(&response)
}

fn validate_and_print_managed_response(
    response: &bcode_ipc::PluginServiceResponse,
) -> Result<(), CliError> {
    if response.error.is_some()
        || response.payload.len() > bcode_harness_models::MAX_HARNESS_WIRE_BYTES
    {
        return Err(CliError::InvalidArguments(
            "managed session response unavailable".into(),
        ));
    }
    print_managed_response(&response.payload)
}

async fn execute_models(path: &PathBuf) -> Result<(), CliError> {
    let input: bcode_ipc::harness::ManagedExecutionCatalogRequest =
        serde_json::from_str(&read_request(path)?)
            .map_err(|_| CliError::InvalidArguments("invalid execution catalog request".into()))?;
    input
        .validate()
        .map_err(|_| CliError::InvalidArguments("invalid execution catalog request".into()))?;
    let response = bcode_client::ManagedSessionClient::default_endpoint()
        .discover_execution_catalog(input)
        .await
        .map_err(|_| {
            CliError::InvalidArguments(
                "execution catalog unavailable; no execution or fallback was attempted".into(),
            )
        })?;
    print_execution_response(&response)
}

async fn execute_selected_start(path: &PathBuf) -> Result<(), CliError> {
    let input: bcode_ipc::harness::ManagedSelectedStartRequest =
        serde_json::from_str(&read_request(path)?)
            .map_err(|_| CliError::InvalidArguments("invalid selected start request".into()))?;
    input
        .validate()
        .map_err(|_| CliError::InvalidArguments("invalid selected start request".into()))?;
    let response = bcode_client::ManagedSessionClient::default_endpoint()
        .start_selected_session(input).await
        .map_err(|_| CliError::InvalidArguments("selected start unavailable or uncertain; inspect canonical session history before any further creation attempt. No replay or fallback was attempted".into()))?;
    print_execution_response(&response)
}

fn print_execution_response(response: &impl serde::Serialize) -> Result<(), CliError> {
    let payload = serde_json::to_vec(response)
        .map_err(|_| CliError::InvalidArguments("invalid execution application response".into()))?;
    if payload.len() > bcode_harness_models::MAX_HARNESS_WIRE_BYTES {
        return Err(CliError::InvalidArguments(
            "execution application response exceeds bounds; inspect session status without replay"
                .into(),
        ));
    }
    print_managed_response(&payload)
}

fn print_managed_response(payload: &[u8]) -> Result<(), CliError> {
    let value: serde_json::Value = serde_json::from_slice(payload)
        .map_err(|_| CliError::InvalidArguments("invalid managed session response".into()))?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

#[cfg(not(unix))]
async fn execute_handoff(_path: &PathBuf) -> Result<(), CliError> {
    Err(CliError::InvalidArguments(
        "native terminal handoff is unsupported on this platform".into(),
    ))
}

#[cfg(unix)]
async fn execute_handoff(path: &PathBuf) -> Result<(), CliError> {
    use std::io::IsTerminal as _;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(CliError::InvalidArguments(
            "native login requires an interactive client terminal".into(),
        ));
    }
    let input: bcode_ipc::harness::ManagedSessionPrepareHandoffRequest =
        serde_json::from_str(&read_request(path)?)
            .map_err(|_| CliError::InvalidArguments("invalid native handoff preparation".into()))?;
    // Reached only after the explicit --execute consent gate. Keep the caller sealed and
    // bound to canonical session/instance evidence; unrelated model auth is not materialized.
    let mut flow = bcode_client::ManagedSessionClient::default_endpoint()
        .session_auth_flow(input.session_id, &input.instance_id)
        .await?;
    let (exit_success, snapshot) = flow.terminal_handoff(input.interface_id).await?;
    println!("{}", serde_json::to_string_pretty(&snapshot)?);
    if !exit_success {
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
    fn recovery_request_preserves_exact_identity_and_rejects_invalid_generation() {
        let session = bcode_session_models::SessionId::new();
        let encoded = super::recovery_request_json(
            session,
            "selected-instance".into(),
            7,
            "retained-recovery".into(),
        )
        .unwrap();
        let decoded: bcode_ipc::harness::ManagedSessionRecoveryRequest =
            serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.session_id, session);
        assert_eq!(decoded.instance_id, "selected-instance");
        assert_eq!(decoded.expected_generation, 7);
        assert_eq!(decoded.recovery_id, "retained-recovery");
        assert!(
            super::recovery_request_json(
                session,
                "selected-instance".into(),
                0,
                "retained-recovery".into()
            )
            .is_err()
        );
    }

    #[test]
    fn execution_commands_accept_only_explicit_request_files() {
        for command in ["models", "start-selected"] {
            assert!(Args::try_parse_from(["harness", command]).is_err());
            assert!(Args::try_parse_from(["harness", command, "--model", "unselected"]).is_err());
        }
        assert!(matches!(
            Args::try_parse_from(["harness", "models", "--request", "catalog.json"]).unwrap().command,
            HarnessCommand::Models { request } if request == std::path::Path::new("catalog.json")
        ));
        assert!(matches!(
            Args::try_parse_from(["harness", "start-selected", "--request", "selected.json"]).unwrap().command,
            HarnessCommand::StartSelected { request } if request == std::path::Path::new("selected.json")
        ));
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
