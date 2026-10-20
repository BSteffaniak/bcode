#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! A bounded application consumer; no server hosting or canonical storage access.
//! Uses an explicitly verified daemon executable, state and configuration selection.
//! See docs/sdk.md for the independent consumer launch recipe.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use bcode::BcodeClient;
use bcode_config::{ConfigLoadOverrides, StateLocationSelection};
use bcode_daemon_lifecycle::DaemonTarget;
use bcode_plugin_sdk::interaction::InteractionInput;
use bcode_plugin_sdk::interaction_application::{
    InteractionApplication, InteractionControllerInput,
};
use bcode_session_models::{
    CreateSessionRequest, ModelTurnOutcome, ProjectionWindowAnchor, ProjectionWindowDirection,
    ProjectionWindowLimits, ProjectionWindowRequest, ProjectionWindowTarget,
    SESSION_OBSERVATION_SCHEMA_VERSION, SessionApplication, SessionChildCreateRequest,
    SessionEventKind, SessionHistoryDirection, SessionHistoryQuery, SessionId,
    SessionObservationApplication, SessionObservationEvent, SessionObservationRequest,
    SessionProjectionKind, SessionSpawnRequest, TurnAdmission, TurnAdmissionMetadata, TurnReceipt,
};

// A selected client is passed in, not a host handle, event store or daemon-private context.
struct Selection {
    provider: String,
    model: String,
    agent: String,
}

async fn run_session(
    client: &BcodeClient,
    directory: PathBuf,
    selection: Selection,
    prompt: String,
    inputs: &[InteractionInput],
) -> Result<(), String> {
    let parent = SessionApplication::create_session(
        client,
        CreateSessionRequest {
            name: Some("Rust application consumer".into()),
            working_directory: directory.clone(),
        },
    )
    .await
    .map_err(|error| format!("create: {error:?}"))?;
    println!("parent session: {}", parent.id);
    SessionApplication::set_session_agent(client, parent.id, selection.agent)
        .await
        .map_err(|error| format!("agent selection: {error:?}"))?;
    SessionApplication::set_session_model(
        client,
        parent.id,
        Some(selection.provider),
        selection.model,
    )
    .await
    .map_err(|error| format!("model selection: {error:?}"))?;

    // Establish compatible consumer authority before a tool can request interaction.
    let controller = client
        .interaction_controller_consumer()
        .await
        .map_err(|error| error.to_string())?;
    let admission = SessionApplication::submit_turn(
        client,
        parent.id,
        prompt,
        TurnAdmissionMetadata::default(),
    )
    .await
    .map_err(|error| format!("admission failed; do not blindly retry: {error:?}"))?;
    let receipt = admitted(admission)?;
    let outcome = supervise(client, &controller, parent.id, &receipt, inputs).await?;
    if outcome != ModelTurnOutcome::Completed {
        return Err(format!("parent turn ended: {outcome:?}"));
    }

    // Child ID pins this immutable request. Retrying after ambiguity must retain the
    // exact request, not mint a new ID. Preparation/admission can fail after creation.
    let child_id = SessionId::new();
    let request = SessionSpawnRequest {
        version: 1,
        creation: SessionChildCreateRequest {
            parent_id: parent.id,
            child_id,
            name: Some("Rust consumer child".into()),
            working_directory: directory,
        },
        worktree_directory: None,
        input: "Reply with one short greeting. Do not use tools.".into(),
        admission: TurnAdmissionMetadata::default(),
        auth_profile: None,
    };
    println!("child request identity: {child_id}");
    let result = SessionApplication::spawn_session(client, request).await;
    result
        .validate()
        .map_err(|error| format!("invalid spawn result: {error:?}"))?;
    if let Some(failure) = result.failure {
        return Err(format!(
            "spawn failed: {failure:?}; created session: {:?}; inspect before retry",
            result.session.map(|session| session.id)
        ));
    }
    let child = result.session.ok_or("spawn did not return a session")?;
    let receipt = admitted(result.admission.ok_or("spawn did not return admission")?)?;
    let outcome = supervise(client, &controller, child.id, &receipt, &[]).await?;
    if outcome != ModelTurnOutcome::Completed {
        return Err(format!("child turn ended: {outcome:?}"));
    }
    Ok(())
}

fn admitted(admission: TurnAdmission) -> Result<TurnReceipt, String> {
    match admission {
        TurnAdmission::Accepted(receipt)
        | TurnAdmission::Existing(receipt)
        | TurnAdmission::Deferred(receipt) => {
            println!("admitted turn: {}", receipt.turn_id);
            Ok(receipt)
        }
        other => Err(format!("turn not runnable: {other:?}")),
    }
}

fn observation_request(session_id: SessionId) -> SessionObservationRequest {
    SessionObservationRequest {
        schema_version: SESSION_OBSERVATION_SCHEMA_VERSION,
        session_id,
        window: ProjectionWindowRequest {
            projection: SessionProjectionKind::Transcript,
            anchor: ProjectionWindowAnchor::Latest,
            direction: ProjectionWindowDirection::Backward,
            target: ProjectionWindowTarget {
                min_items: Some(8),
                ..Default::default()
            },
            limits: ProjectionWindowLimits {
                max_items: 16,
                max_events_scanned: 64,
                max_bytes: 262_144,
            },
        },
        max_updates: 64,
        max_snapshot_bytes: 262_144,
    }
}

async fn supervise(
    client: &BcodeClient,
    controller: &impl InteractionApplication,
    session_id: SessionId,
    receipt: &TurnReceipt,
    inputs: &[InteractionInput],
) -> Result<ModelTurnOutcome, String> {
    // Timeout bounds observation, not execution. All early errors below also take
    // the explicit exact-turn cancellation path; no automatic mutation replay.
    let observed = tokio::time::timeout(Duration::from_secs(30), async {
        let mut observation = client
            .observe_session(observation_request(session_id))
            .await
            .map_err(|error| format!("observation: {error:?}"))?;
        let mut handled = BTreeSet::new();
        loop {
            if let Some(outcome) = terminal_outcome(client, session_id, receipt).await? {
                observation.close();
                return Ok(outcome);
            }
            handle_pending_interactions(controller, session_id, inputs, &mut handled).await?;
            // recv is cancellation-safe. A timeout retains its pending pull.
            match tokio::time::timeout(Duration::from_millis(250), observation.recv()).await {
                Ok(Some(delivery)) => match delivery.event {
                    SessionObservationEvent::Snapshot { initial, snapshot } => {
                        println!(
                            "semantic snapshot sequence={} initial={initial} revision={:?}",
                            delivery.sequence, snapshot.revision
                        );
                    }
                    SessionObservationEvent::Ended { reason } => {
                        return Err(format!(
                            "observation ended: {reason:?}; no replay attempted"
                        ));
                    }
                },
                Ok(None) => return Err("observation ended without a turn outcome".into()),
                Err(_) => {}
            }
        }
    })
    .await;
    match observed {
        Ok(Ok(outcome)) => Ok(outcome),
        failure => {
            eprintln!("supervision stopped: {failure:?}");
            let cancelled = tokio::time::timeout(
                Duration::from_secs(5),
                SessionApplication::cancel_session_turn(
                    client,
                    session_id,
                    false,
                    Some(receipt.turn_id.to_string()),
                ),
            )
            .await;
            eprintln!("exact cancellation acknowledgement (not terminal): {cancelled:?}");
            let terminal = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some(outcome) = terminal_outcome(client, session_id, receipt).await? {
                        return Ok::<_, String>(outcome);
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await;
            match terminal {
                Ok(Ok(outcome)) => Err(format!(
                    "supervision failed; final turn outcome: {outcome:?}"
                )),
                other => Err(format!(
                    "terminal outcome unresolved: {other:?}; inspect session {session_id}; do not retry"
                )),
            }
        }
    }
}

async fn terminal_outcome(
    client: &BcodeClient,
    session_id: SessionId,
    receipt: &TurnReceipt,
) -> Result<Option<ModelTurnOutcome>, String> {
    // One bounded current-history page. Absence is unknown, never fabricated completion.
    let page = SessionApplication::session_history_page(
        client,
        session_id,
        SessionHistoryQuery {
            cursor: None,
            limit: 64,
            direction: SessionHistoryDirection::Backward,
        },
    )
    .await
    .map_err(|error| format!("terminal inspection: {error:?}"))?;
    for event in page.events {
        if let SessionEventKind::ModelTurnFinished {
            turn_id, outcome, ..
        } = event.kind
            && turn_id == receipt.turn_id.to_string()
        {
            return Ok(Some(outcome));
        }
    }
    Ok(None)
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 10 {
        return Err("usage: application_session DAEMON_EXE SHA256 STATE_ROOT CONFIG_DIRECTORY CONFIG_FILE MODEL_PROFILE ABSOLUTE_CWD AGENT PROMPT INPUTS_JSON_FILE; inputs file is a JSON array (use [] for no interactions)".into());
    }
    let directory = PathBuf::from(&args[6]);
    if !directory.is_absolute() || !directory.is_dir() {
        return Err("working directory must be an existing absolute directory".into());
    }
    let inputs = load_controller_inputs(&args[9])?;
    let config_file = PathBuf::from(&args[4]);
    if !config_file.is_absolute() || !config_file.is_file() {
        return Err("configuration file must be an existing absolute file".into());
    }
    let overrides = ConfigLoadOverrides {
        exclusive_config_path: Some(config_file),
        cli_config_toml: Some(bcode_config::model_profile_override_toml(&args[5])),
        ..Default::default()
    };
    let config = bcode_config::load_config_with_overrides(&overrides).map_err(|_| {
        "explicit configuration could not be resolved; no defaults substituted".to_owned()
    })?;
    let selection = config.resolved_model_selection();
    let provider = selection
        .provider_plugin_id
        .ok_or("profile must select a provider")?;
    let model = selection.model_id.ok_or("profile must select a model")?;
    println!(
        "selected profile={} provider={provider} model={model}",
        args[5]
    );
    let location = bcode_config::resolve_state_location_set(
        &config.state,
        &StateLocationSelection {
            root: Some(PathBuf::from(&args[2])),
            profile: None,
        },
    )
    .map_err(|_| "explicit state location unavailable; no fallback".to_owned())?;
    let target = DaemonTarget::verify(
        PathBuf::from(&args[0]),
        args[1].clone(),
        location.primary().clone(),
        PathBuf::from(&args[3]),
    )
    .await
    .map_err(|error| error.to_string())?;
    let client = BcodeClient::for_daemon(target, &config, directory.clone())
        .map_err(|error| error.to_string())?;
    run_session(
        &client,
        directory,
        Selection {
            provider,
            model,
            agent: args[7].clone(),
        },
        args[8].clone(),
        &inputs,
    )
    .await
}

async fn handle_pending_interactions(
    controller: &impl InteractionApplication,
    session_id: SessionId,
    inputs: &[InteractionInput],
    handled: &mut BTreeSet<String>,
) -> Result<(), String> {
    for permission in controller
        .pending_permissions(session_id)
        .await
        .map_err(|error| format!("permissions: {error:?}"))?
    {
        // Safe demonstration policy: explicitly deny, never remember.
        let resolved = controller
            .resolve_permission(session_id, &permission.permission_id, false, false)
            .await
            .map_err(|error| format!("permission denial: {error:?}"))?;
        println!("permission denied: {resolved}");
    }
    for exchange in controller
        .pending_exchanges(session_id)
        .await
        .map_err(|error| format!("pending interaction: {error:?}"))?
    {
        let id = exchange.request.exchange_id;
        if handled.contains(&id) {
            continue;
        }
        if inputs.is_empty() || !handled.is_empty() {
            return Err("interaction needs explicit caller inputs; stopping safely".into());
        }
        let mut snapshot = controller
            .open_controller(1, &id)
            .await
            .map_err(|error| format!("open controller: {error:?}"))?;
        println!("interaction {}: {}", id, snapshot.snapshot);
        for input in inputs {
            snapshot = controller
                .controller_input(InteractionControllerInput {
                    version: 1,
                    exchange_id: id.clone(),
                    expected_revision: snapshot.revision,
                    input: input.clone(),
                })
                .await
                .map_err(|error| {
                    format!("controller input; effects may have committed: {error:?}")
                })?;
        }
        handled.insert(id);
    }
    Ok(())
}

fn load_controller_inputs(path: &str) -> Result<Vec<InteractionInput>, String> {
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    use std::io::Read as _;
    let mut bytes = Vec::new();
    file.take(65_537)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > 65_536 {
        return Err("inputs file exceeds 64 KiB".into());
    }
    let inputs: Vec<InteractionInput> =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if inputs.len() > 32 {
        return Err("at most 32 explicit controller inputs are supported".into());
    }
    Ok(inputs)
}
