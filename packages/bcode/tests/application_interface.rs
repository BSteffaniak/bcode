#![cfg(feature = "app")]
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! External consumers exercise public operations; only the fixture owns host startup.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use bcode_session_models::{
    CreateSessionRequest, ProjectionWindowAnchor, ProjectionWindowDirection,
    ProjectionWindowLimits, ProjectionWindowRequest, ProjectionWindowTarget,
    SESSION_OBSERVATION_SCHEMA_VERSION, SessionApplication, SessionEventKind,
    SessionHistoryDirection, SessionId, SessionInspectionCategory, SessionInspectionQuery,
    SessionObservationApplication, SessionObservationEnd, SessionObservationError,
    SessionObservationEvent, SessionObservationRequest, SessionObservationStream,
    SessionProjectionKind, TurnAdmission, TurnAdmissionMetadata, TurnPriority, TurnRejectionReason,
};

// Name the existing public semantic snapshot through the public observation contract. This
// integration target does not need a new direct dependency on the session-view models crate.
type SemanticSnapshot = <bcode_server::SessionObservation as SessionObservationStream>::Snapshot;

fn observation_request(session_id: SessionId) -> SessionObservationRequest {
    SessionObservationRequest {
        schema_version: SESSION_OBSERVATION_SCHEMA_VERSION,
        session_id,
        window: ProjectionWindowRequest {
            projection: SessionProjectionKind::Transcript,
            anchor: ProjectionWindowAnchor::Latest,
            direction: ProjectionWindowDirection::Backward,
            target: ProjectionWindowTarget {
                min_items: Some(32),
                ..ProjectionWindowTarget::default()
            },
            limits: ProjectionWindowLimits {
                max_items: 64,
                max_events_scanned: 256,
                max_bytes: 1024 * 1024,
            },
        },
        max_updates: 4096,
        max_snapshot_bytes: 4 * 1024 * 1024,
    }
}

const fn inspection_query(category: SessionInspectionCategory) -> SessionInspectionQuery {
    SessionInspectionQuery {
        category,
        cursor: None,
        limit: 64,
        direction: SessionHistoryDirection::Backward,
    }
}

async fn snapshot<S>(
    stream: &mut S,
    session_id: SessionId,
    sequence: u64,
    initial: bool,
) -> SemanticSnapshot
where
    S: SessionObservationStream<Snapshot = SemanticSnapshot>,
{
    let delivery = tokio::time::timeout(Duration::from_secs(10), stream.recv())
        .await
        .expect("semantic observation timed out")
        .expect("semantic observation ended unexpectedly");
    assert_eq!(delivery.schema_version, SESSION_OBSERVATION_SCHEMA_VERSION);
    assert_eq!(delivery.session_id, session_id);
    assert_eq!(delivery.sequence, sequence);
    let SessionObservationEvent::Snapshot {
        initial: actual_initial,
        snapshot,
    } = delivery.event
    else {
        panic!("expected semantic snapshot, not observation termination");
    };
    assert_eq!(actual_initial, initial);
    assert_eq!(snapshot.session_id, Some(session_id));
    snapshot
}

async fn lifecycle_consumer<A>(application: &A, directory: &Path, next_directory: &Path)
where
    A: SessionApplication + SessionObservationApplication,
    A::Observation: SessionObservationStream<Snapshot = SemanticSnapshot>,
{
    let invalid = application
        .create_session(CreateSessionRequest {
            name: None,
            working_directory: "relative".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(invalid.code, "session_working_directory_must_be_absolute");
    let session = application
        .create_session(CreateSessionRequest {
            name: Some("external consumer".to_owned()),
            working_directory: directory.to_path_buf(),
        })
        .await
        .unwrap();
    assert_eq!(session.title(), Some("external consumer"));
    assert_eq!(session.working_directory, directory);
    let mut invalid_request = observation_request(session.id);
    invalid_request.schema_version = SESSION_OBSERVATION_SCHEMA_VERSION + 1;
    assert!(matches!(
        application.observe_session(invalid_request).await,
        Err(SessionObservationError::UnsupportedSchema)
    ));
    let mut observation = application
        .observe_session(observation_request(session.id))
        .await
        .unwrap();
    let initial = snapshot(&mut observation, session.id, 0, true).await;
    assert_eq!(initial.title.as_deref(), Some("external consumer"));
    assert_eq!(initial.working_directory.as_deref(), Some(directory));
    let renamed = application
        .rename_session(session.id, Some("renamed by consumer".to_owned()))
        .await
        .unwrap();
    assert_eq!(renamed.id, session.id);
    assert_eq!(renamed.title(), Some("renamed by consumer"));
    let update = snapshot(&mut observation, session.id, 1, false).await;
    assert_eq!(update.title.as_deref(), Some("renamed by consumer"));
    assert!(update.latest_sequence > initial.latest_sequence);
    let changed = application
        .change_session_working_directory(session.id, next_directory.to_path_buf())
        .await
        .unwrap();
    assert!(changed.changed);
    assert_eq!(changed.session.working_directory, next_directory);
    let update = snapshot(&mut observation, session.id, 2, false).await;
    assert_eq!(update.working_directory.as_deref(), Some(next_directory));
    observation.close();
    observation.close();
    let end = observation.recv().await.unwrap();
    assert_closed_observation(&mut observation, session.id, &end).await;
    verify_canonical_changes(application, session.id, directory, next_directory).await;
}

async fn assert_closed_observation<S>(
    observation: &mut S,
    session_id: SessionId,
    end: &bcode_session_models::SessionObservationEnvelope<SemanticSnapshot>,
) where
    S: SessionObservationStream<Snapshot = SemanticSnapshot>,
{
    assert_eq!(end.session_id, session_id);
    assert_eq!(end.sequence, 3);
    assert!(matches!(
        &end.event,
        SessionObservationEvent::Ended {
            reason: SessionObservationEnd::Closed
        }
    ));
    assert!(observation.recv().await.is_none());
    observation.close();
    assert!(observation.recv().await.is_none());
}

async fn verify_canonical_changes<A>(
    application: &A,
    session_id: SessionId,
    directory: &Path,
    next_directory: &Path,
) where
    A: SessionApplication + SessionObservationApplication,
    A::Observation: SessionObservationStream<Snapshot = SemanticSnapshot>,
{
    let page = application
        .inspect_session(
            session_id,
            inspection_query(SessionInspectionCategory::SelectionChanges),
        )
        .await
        .unwrap();
    assert_eq!(page.session_id, session_id);
    assert!(!page.has_more);
    assert!(page.compatibility_issues.is_empty());
    assert!(page.events.iter().any(|event| matches!(
        &event.kind,
        SessionEventKind::WorkingDirectoryChanged {
            old_working_directory,
            new_working_directory,
        } if old_working_directory == directory && new_working_directory == next_directory
    )));
    let observation = application
        .observe_session(observation_request(session_id))
        .await
        .unwrap();
    drop(observation);
    let renamed = application
        .rename_session(session_id, Some("after observation drop".to_owned()))
        .await
        .unwrap();
    assert_eq!(renamed.title(), Some("after observation drop"));
    let mut before_rejection = application
        .observe_session(observation_request(session_id))
        .await
        .unwrap();
    let before = snapshot(&mut before_rejection, session_id, 0, true).await;
    drop(before_rejection);
    let admission = application
        .submit_turn(
            session_id,
            "background work must not execute".to_owned(),
            TurnAdmissionMetadata {
                priority: TurnPriority::Background,
                ..TurnAdmissionMetadata::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        admission,
        TurnAdmission::Rejected(TurnRejectionReason::ExecutionPolicy)
    );
    let unchanged = application
        .change_session_working_directory(session_id, next_directory.to_path_buf())
        .await
        .unwrap();
    assert!(!unchanged.changed);
    let page = application
        .inspect_session(
            session_id,
            inspection_query(SessionInspectionCategory::TerminalOutcomes),
        )
        .await
        .unwrap();
    assert!(page.events.is_empty());
    let mut fresh = application
        .observe_session(observation_request(session_id))
        .await
        .unwrap();
    let state = snapshot(&mut fresh, session_id, 0, true).await;
    assert_eq!(state.title.as_deref(), Some("after observation drop"));
    assert_eq!(state.working_directory.as_deref(), Some(next_directory));
    assert_eq!(state.latest_sequence, before.latest_sequence);
    assert!(state.runtime.active_turn_id.is_none());
    fresh.close();
}

#[cfg(all(
    feature = "static-bundled-fake-provider-plugin",
    feature = "static-bundled-default-agents-plugin"
))]
async fn execution_consumer<A>(application: &A, directory: &Path)
where
    A: SessionApplication + SessionObservationApplication,
    A::Observation: SessionObservationStream<Snapshot = SemanticSnapshot>,
{
    let session = application
        .create_session(CreateSessionRequest {
            name: Some("offline execution".to_owned()),
            working_directory: directory.to_path_buf(),
        })
        .await
        .unwrap();
    application
        .set_session_model(
            session.id,
            Some("bcode.fake-provider".to_owned()),
            "fake-echo".to_owned(),
        )
        .await
        .unwrap();
    application
        .set_session_agent(session.id, "build".to_owned())
        .await
        .unwrap();
    // Close and drop are tested while a real accepted turn is active, not merely while idle.
    // A third turn tests explicit cancellation and its distinct authoritative outcome.
    for mode in 0..3 {
        exercise_observation_lifetime(application, session.id, mode).await;
    }
}

#[cfg(all(
    feature = "static-bundled-fake-provider-plugin",
    feature = "static-bundled-default-agents-plugin"
))]
async fn exercise_observation_lifetime<A>(application: &A, session_id: SessionId, mode: u8)
where
    A: SessionApplication + SessionObservationApplication,
    A::Observation: SessionObservationStream<Snapshot = SemanticSnapshot>,
{
    use bcode_session_models::ModelTurnOutcome;

    let mut observation = application
        .observe_session(observation_request(session_id))
        .await
        .unwrap();
    snapshot(&mut observation, session_id, 0, true).await;
    let admission = application
        .submit_turn(
            session_id,
            format!("offline echo {mode}"),
            TurnAdmissionMetadata::default(),
        )
        .await
        .unwrap();
    let TurnAdmission::Accepted(receipt) = admission else {
        panic!("expected accepted offline turn, got {admission:?}");
    };
    let turn_id = receipt.turn_id.to_string();
    wait_until_active(&mut observation, session_id, &turn_id).await;
    match mode {
        0 => observation.close(),
        1 => drop(observation),
        _ => {
            assert!(
                application
                    .cancel_session_turn(session_id, false, Some(turn_id.clone()))
                    .await
                    .unwrap()
            );
            observation.close();
        }
    }
    let (outcome, message) = wait_for_terminal(application, session_id, &turn_id).await;
    let expected_outcome = if mode == 2 {
        ModelTurnOutcome::Cancelled
    } else {
        ModelTurnOutcome::Completed
    };
    assert_eq!(
        outcome, expected_outcome,
        "exact-turn terminal message: {message:?}"
    );
    // Reopening from bounded state must preserve the terminal projection.
    let mut fresh = application
        .observe_session(observation_request(session_id))
        .await
        .unwrap();
    let state = snapshot(&mut fresh, session_id, 0, true).await;
    assert_eq!(state.runtime.last_turn_outcome, Some(outcome));
    assert!(state.runtime.active_turn_id.is_none());
    fresh.close();
}

#[cfg(all(
    feature = "static-bundled-fake-provider-plugin",
    feature = "static-bundled-default-agents-plugin"
))]
async fn wait_until_active<S>(observation: &mut S, session_id: SessionId, turn_id: &str)
where
    S: SessionObservationStream<Snapshot = SemanticSnapshot>,
{
    tokio::time::timeout(Duration::from_secs(10), async {
        for sequence in 1..4096 {
            let state = snapshot(observation, session_id, sequence, false).await;
            if state.runtime.active_turn_id.as_deref() == Some(turn_id) {
                return;
            }
        }
        panic!("active turn was not observed within the update budget");
    })
    .await
    .expect("accepted turn did not become active");
}

#[cfg(all(
    feature = "static-bundled-fake-provider-plugin",
    feature = "static-bundled-default-agents-plugin"
))]
async fn wait_for_terminal<A: SessionApplication>(
    application: &A,
    session_id: SessionId,
    expected_turn_id: &str,
) -> (bcode_session_models::ModelTurnOutcome, Option<String>) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let page = application
                .inspect_session(
                    session_id,
                    inspection_query(SessionInspectionCategory::TerminalOutcomes),
                )
                .await
                .unwrap();
            assert!(
                !page.has_more,
                "fixture exceeded its bounded terminal window"
            );
            for event in page.events {
                if let SessionEventKind::ModelTurnFinished {
                    turn_id,
                    outcome,
                    message,
                } = event.kind
                    && turn_id == expected_turn_id
                {
                    return (outcome, message);
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("exact admitted turn has no authoritative terminal outcome")
}

#[test]
fn public_session_interface_external_consumer() {
    isolated_host("public_session_interface_external_consumer", false);
}

#[cfg(all(
    feature = "static-bundled-fake-provider-plugin",
    feature = "static-bundled-default-agents-plugin"
))]
#[test]
fn public_session_interface_offline_execution_external_consumer() {
    isolated_host(
        "public_session_interface_offline_execution_external_consumer",
        true,
    );
}

fn isolated_host(test_name: &str, fake_provider: bool) {
    const CHILD: &str = "BCODE_TEST_APPLICATION_INTERFACE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(host_fixture(fake_provider));
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env_clear()
        .env(CHILD, "1")
        .env("HOME", root.path())
        .env("BCODE_STATE_DIR", root.path().join("state"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .current_dir(root.path())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "isolated application consumer failed: {status}"
            );
            return;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("isolated application consumer timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

async fn host_fixture(fake_provider: bool) {
    let directory = std::env::current_dir().unwrap().canonicalize().unwrap();
    let next_directory = directory.join("next");
    std::fs::create_dir(&next_directory).unwrap();
    let (config, plugins, default_plugin_ids) = host_services(fake_provider);
    let shutdown = bcode_agent_runtime::CancellationToken::new();
    let signal = shutdown.clone();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let ready: bcode_server::EmbeddedWorkflowReady = Box::new(move |application| {
        Box::pin(async move {
            let session = application.session_application();
            Box::pin(lifecycle_consumer(&session, &directory, &next_directory)).await;
            #[cfg(all(
                feature = "static-bundled-fake-provider-plugin",
                feature = "static-bundled-default-agents-plugin"
            ))]
            if fake_provider {
                execution_consumer(&session, &directory).await;
            }
            sender.send(()).unwrap();
            signal.cancel();
        })
    });
    bcode_server::run_embedded_with_workflow_application(
        bcode_ipc::IpcEndpoint::unix_socket(
            std::env::current_dir().unwrap().join("application.sock"),
        ),
        config,
        plugins,
        bcode_model_catalog::ModelCatalogResolver::embedded(),
        default_plugin_ids,
        shutdown,
        ready,
    )
    .await
    .unwrap();
    receiver.await.unwrap();
}

fn host_services(
    fake_provider: bool,
) -> (
    bcode_config::BcodeConfig,
    bcode_plugin::PluginRuntimeHost,
    Vec<String>,
) {
    #[cfg(all(
        feature = "static-bundled-fake-provider-plugin",
        feature = "static-bundled-default-agents-plugin"
    ))]
    if fake_provider {
        let ids = vec![
            "bcode.fake-provider".to_owned(),
            "bcode.default-agents".to_owned(),
        ];
        let selection = bcode_plugin::PluginSelection {
            mode: bcode_plugin::PluginSelectionMode::Explicit,
            enabled: ids.iter().cloned().collect(),
            disabled: std::collections::BTreeSet::new(),
        };
        let plugins = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
            &selection,
            &bcode_bundled_plugins::static_bundled_plugins(),
        )
        .unwrap();
        let mut config = bcode_config::BcodeConfig::default();
        config.model.provider_plugin_id = Some("bcode.fake-provider".to_owned());
        config.model.model_id = Some("fake-echo".to_owned());
        // fake-echo has no prompt-cache capability; exercise ordinary turns without cache hints.
        config.model.prompt_cache.mode = serde_json::from_value(serde_json::json!("off"))
            .expect("decode the supported prompt-cache mode");
        config
            .model
            .settings
            .insert("fake_turn_delay_ms".to_owned(), "2000".to_owned());
        return (config, plugins, ids);
    }
    assert!(
        !fake_provider,
        "offline execution requires both static plugin features"
    );
    let plugins = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
        &bcode_plugin::PluginSelection {
            mode: bcode_plugin::PluginSelectionMode::Explicit,
            enabled: std::collections::BTreeSet::new(),
            disabled: std::collections::BTreeSet::new(),
        },
        &[],
    )
    .unwrap();
    (bcode_config::BcodeConfig::default(), plugins, Vec::new())
}
