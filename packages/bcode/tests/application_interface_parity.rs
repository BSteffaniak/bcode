#![cfg(all(feature = "app", feature = "daemon-client"))]
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Same-host direct application and real IPC parity, without provider spending.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use bcode_session_models::{
    CreateSessionRequest, SessionApplication, SessionApplicationFailure, SessionHistoryDirection,
    SessionInspectionCategory, SessionInspectionPage, SessionInspectionQuery, SessionSummary,
};

/// Static labels only: never print request arguments, configuration or daemon output.
struct Phase {
    label: String,
    started: Instant,
}

impl Phase {
    fn start(label: impl Into<String>) -> Self {
        let label = label.into();
        eprintln!("[parity-phase] begin {label}");
        Self {
            label,
            started: Instant::now(),
        }
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        eprintln!(
            "[parity-phase] end {} elapsed_ms={} unwinding={}",
            self.label,
            self.started.elapsed().as_millis(),
            std::thread::panicking()
        );
    }
}

async fn bounded_phase<T>(
    label: &'static str,
    seconds: u64,
    future: impl std::future::Future<Output = T>,
) -> T {
    let _phase = Phase::start(label);
    tokio::time::timeout(Duration::from_secs(seconds), future)
        .await
        .unwrap_or_else(|_| {
            panic!("parity phase timed out: {label} after {seconds}s; effects may be unknown")
        })
}

fn reap_child(child: &mut std::process::Child, label: &'static str) {
    let _phase = Phase::start(label);
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                eprintln!("[parity-phase] child reap unresolved after 5s: {label}");
                assert!(
                    std::thread::panicking(),
                    "fixture child did not terminate after kill: {label}"
                );
                return;
            }
        }
    }
}

#[test]
fn direct_and_ipc_session_application_parity() {
    const CHILD: &str = "BCODE_TEST_APPLICATION_PARITY_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _phase = Phase::start("isolated-child-runtime");
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(host_fixture());
        return;
    }
    let _phase = Phase::start("isolated-child-spawn-and-wait");
    let root = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "direct_and_ipc_session_application_parity",
            "--nocapture",
        ])
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
                "application parity child failed: {status}"
            );
            return;
        }
        if Instant::now() >= deadline {
            reap_child(&mut child, "isolated-child-timeout-reap");
            panic!("application parity child timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn assert_compaction_input_equal(
    before: &bcode_session_models::SessionHistoryPage,
    after: &bcode_session_models::SessionHistoryPage,
) {
    // A no-op emits canonical diagnostics. Preserve all other events exactly;
    // only the owner's explicit skipped-compaction diagnostic is non-input here.
    let input = |page: &bcode_session_models::SessionHistoryPage| {
        page.events
            .iter()
            .filter(|event| {
                !matches!(&event.kind, bcode_session_models::SessionEventKind::TraceEvent { trace }
            if trace.phase == bcode_session_models::SessionTracePhase::ContextCompactionSkipped)
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(
        input(before),
        input(after),
        "model input and selection contributions must remain unchanged"
    );
}

fn assert_skipped_trace_appended(
    before: &bcode_session_models::SessionHistoryPage,
    after: &bcode_session_models::SessionHistoryPage,
) {
    assert!(
        after.events.len() > before.events.len(),
        "no-op preserves canonical trace publication"
    );
    assert_eq!(
        &after.events[..before.events.len()],
        before.events.as_slice(),
        "existing canonical history is preserved"
    );
    assert!(
        after.events[before.events.len()..]
            .iter()
            .all(|event| matches!(&event.kind,
        bcode_session_models::SessionEventKind::TraceEvent { trace }
        if trace.phase == bcode_session_models::SessionTracePhase::ContextCompactionSkipped)),
        "only skipped-compaction diagnostics may be appended for this empty fixture"
    );
    assert_compaction_input_equal(before, after);
}

async fn settled_catalog<A: SessionApplication>(
    application: &A,
    directory: &Path,
) -> bcode_session_models::SessionCatalogSnapshot {
    bounded_phase("session-catalog-settlement", 10, async {
        loop {
            let snapshot = application
                .session_catalog_snapshot(directory.to_path_buf())
                .await
                .unwrap();
            // The compatibility snapshot can return a nonempty partial Loading
            // result. That is discovery progress, not terminal coverage.
            if !matches!(
                snapshot.catalog_status,
                bcode_session_models::SessionCatalogStatus::Loading
                    | bcode_session_models::SessionCatalogStatus::NotStarted
            ) {
                assert!(
                    matches!(
                        snapshot.catalog_status,
                        bcode_session_models::SessionCatalogStatus::Loaded
                    ),
                    "isolated native catalog must settle loaded, not degraded or failed"
                );
                return snapshot;
            }
            eprintln!(
                "[parity-phase] catalog-still-loading revision={} visible_count={}",
                snapshot.catalog_revision,
                snapshot.sessions.len()
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
}

const fn query() -> SessionInspectionQuery {
    SessionInspectionQuery {
        category: SessionInspectionCategory::SelectionChanges,
        cursor: None,
        limit: 100,
        direction: SessionHistoryDirection::Forward,
    }
}

const fn history_query() -> bcode_session_models::SessionHistoryQuery {
    bcode_session_models::SessionHistoryQuery {
        cursor: None,
        limit: 100,
        direction: SessionHistoryDirection::Forward,
    }
}

struct DirectEvidence {
    session: SessionSummary,
    page: SessionInspectionPage,
    create_error: SessionApplicationFailure,
    directory_error: SessionApplicationFailure,
    missing_error: SessionApplicationFailure,
    missing_id: bcode_session_models::SessionId,
    spawn_request: bcode_session_models::SessionSpawnRequest,
    spawn_result: bcode_session_models::SessionSpawnResult,
    compaction: bcode_session_models::application::SessionCompactionResult,
    compaction_session_id: bcode_session_models::SessionId,
    compaction_history: bcode_session_models::SessionHistoryPage,
    note_request: bcode_session_models::SessionPresentationNoteRequest,
    note_history: bcode_session_models::SessionHistoryPage,
    catalog: bcode_session_models::SessionCatalogSnapshot,
}

async fn direct_consumer<A: SessionApplication>(
    application: &A,
    directory: &Path,
) -> DirectEvidence {
    let created = application
        .create_session(CreateSessionRequest {
            name: Some("direct → IPC 🦀".to_owned()),
            working_directory: directory.to_path_buf(),
        })
        .await
        .unwrap();
    // Keep compaction parity on a dedicated canonical session: the lifecycle
    // fixture below subsequently appends rename and directory selection events.
    let compaction_session = application
        .create_session(CreateSessionRequest {
            name: Some("empty compaction parity".to_owned()),
            working_directory: directory.to_path_buf(),
        })
        .await
        .unwrap();
    let compaction_session_id = compaction_session.id;
    let compaction_history = application
        .session_history_page(compaction_session_id, history_query())
        .await
        .unwrap();
    let compaction = bounded_phase(
        "direct-empty-session-compaction",
        10,
        application.compact_session(compaction_session_id),
    )
    .await
    .unwrap();
    let direct_compaction_history = application
        .session_history_page(compaction_session_id, history_query())
        .await
        .unwrap();
    assert_skipped_trace_appended(&compaction_history, &direct_compaction_history);
    assert!(
        matches!(
            &compaction,
            bcode_session_models::application::SessionCompactionResult::Noop(_)
        ),
        "empty history must not claim completed compaction"
    );
    let renamed = application
        .rename_session(created.id, Some("e\u{301} renamed".to_owned()))
        .await
        .unwrap();
    assert_eq!(renamed.title(), Some("e\u{301} renamed"));
    let next = directory.join("next");
    std::fs::create_dir(&next).unwrap();
    let changed = application
        .change_session_working_directory(created.id, next)
        .await
        .unwrap();
    assert!(changed.changed);
    let page = application
        .inspect_session(created.id, query())
        .await
        .unwrap();
    assert!(!page.events.is_empty());
    assert!(!page.has_more);
    assert!(page.compatibility_issues.is_empty());
    let create_error = application
        .create_session(CreateSessionRequest {
            name: None,
            working_directory: "relative".into(),
        })
        .await
        .unwrap_err();
    let directory_error = application
        .change_session_working_directory(created.id, "relative".into())
        .await
        .unwrap_err();
    let missing_id = bcode_session_models::SessionId::new();
    let missing_error = application
        .rename_session(missing_id, Some("missing".to_owned()))
        .await
        .unwrap_err();
    let note_session = application
        .create_session(CreateSessionRequest {
            name: Some("presentation fixture".to_owned()),
            working_directory: directory.to_path_buf(),
        })
        .await
        .unwrap();
    let note_request = bcode_session_models::SessionPresentationNoteRequest {
        session_id: note_session.id,
        source_id: "direct-consumer".to_owned(),
        note_id: "direct-note".to_owned(),
        text: "**literal e\u{301} 🦀**\n".to_owned(),
        format: bcode_session_models::SessionPresentationNoteFormat::Markdown,
    };
    application
        .append_presentation_note(note_request.clone())
        .await
        .unwrap();
    for (note_id, format, text) in [
        (
            "direct-plain",
            bcode_session_models::SessionPresentationNoteFormat::PlainText,
            "plain λ",
        ),
        (
            "direct-json",
            bcode_session_models::SessionPresentationNoteFormat::Json,
            "{\"status\":\"presentation\"}",
        ),
    ] {
        let mut note = note_request.clone();
        note.note_id = note_id.to_owned();
        note.format = format;
        note.text = text.to_owned();
        application.append_presentation_note(note).await.unwrap();
    }
    let note_history = application
        .session_history_page(note_session.id, history_query())
        .await
        .unwrap();
    assert!(note_history.events.iter().any(|event| matches!(&event.kind,
        bcode_session_models::SessionEventKind::PluginStatusNote { text, note_id, .. }
        if text == &note_request.text && note_id == &note_request.note_id)));
    assert_eq!(
        note_history
            .events
            .iter()
            .filter(|event| matches!(
                &event.kind,
                bcode_session_models::SessionEventKind::PluginStatusNote { .. }
            ))
            .count(),
        3
    );
    assert!(note_history.events.iter().all(|event| !matches!(
        &event.kind,
        bcode_session_models::SessionEventKind::UserMessage { .. }
    )));
    let refreshed = bounded_phase(
        "direct-session-catalog-refresh",
        10,
        application.refresh_session_catalog(directory.to_path_buf(), None),
    )
    .await
    .unwrap();
    assert!(!matches!(
        refreshed.catalog_status,
        bcode_session_models::SessionCatalogStatus::NotStarted
    ));
    let catalog = settled_catalog(application, directory).await;
    assert!(
        catalog
            .sessions
            .iter()
            .any(|session| session.id == note_session.id)
    );
    let spawn_request = bcode_session_models::SessionSpawnRequest {
        version: 99,
        creation: bcode_session_models::SessionChildCreateRequest {
            child_id: bcode_session_models::SessionId::new(),
            parent_id: created.id,
            name: Some("not created".to_owned()),
            working_directory: directory.to_path_buf(),
        },
        worktree_directory: None,
        input: "must not run".to_owned(),
        admission: bcode_session_models::TurnAdmissionMetadata::default(),
        auth_profile: None,
    };
    let spawn_result = application.spawn_session(spawn_request.clone()).await;
    spawn_result.validate().unwrap();
    assert!(spawn_result.session.is_none());
    assert!(spawn_result.admission.is_none());
    assert_eq!(
        spawn_result.failure.as_ref().unwrap().stage,
        bcode_session_models::SessionSpawnStage::Preparation
    );
    DirectEvidence {
        session: changed.session,
        page,
        create_error,
        directory_error,
        missing_error,
        missing_id,
        spawn_request,
        spawn_result,
        compaction,
        compaction_session_id,
        compaction_history,
        note_request,
        note_history,
        catalog,
    }
}

async fn ipc_consumer(ipc: bcode_client::BcodeClient, direct: DirectEvidence, directory: PathBuf) {
    let catalog = settled_catalog(&ipc, &directory).await;
    assert!(
        catalog
            .sessions
            .iter()
            .any(|session| session.id == direct.note_request.session_id)
    );
    assert_eq!(
        direct
            .catalog
            .catalog_sources
            .iter()
            .map(|source| &source.source_id)
            .collect::<Vec<_>>(),
        catalog
            .catalog_sources
            .iter()
            .map(|source| &source.source_id)
            .collect::<Vec<_>>(),
        "discovery source identity parity"
    );
    let refreshed = SessionApplication::refresh_session_catalog(&ipc, directory.clone(), None)
        .await
        .unwrap();
    assert!(!matches!(
        refreshed.catalog_status,
        bcode_session_models::SessionCatalogStatus::NotStarted
    ));
    let note_history = SessionApplication::session_history_page(
        &ipc,
        direct.note_request.session_id,
        history_query(),
    )
    .await
    .unwrap();
    assert_eq!(
        direct.note_history, note_history,
        "direct note canonical publication is identical through IPC"
    );
    let mut note = direct.note_request.clone();
    note.note_id = "ipc-note".to_owned();
    SessionApplication::append_presentation_note(&ipc, note.clone())
        .await
        .unwrap();
    for (note_id, format, text) in [
        (
            "ipc-plain",
            bcode_session_models::SessionPresentationNoteFormat::PlainText,
            "plain λ",
        ),
        (
            "ipc-json",
            bcode_session_models::SessionPresentationNoteFormat::Json,
            "{\"status\":\"presentation\"}",
        ),
    ] {
        let mut request = note.clone();
        request.note_id = note_id.to_owned();
        request.format = format;
        request.text = text.to_owned();
        SessionApplication::append_presentation_note(&ipc, request)
            .await
            .unwrap();
    }
    let history = SessionApplication::session_history_page(&ipc, note.session_id, history_query())
        .await
        .unwrap();
    assert!(history.events.iter().any(|event| matches!(&event.kind,
        bcode_session_models::SessionEventKind::PluginStatusNote { text, note_id, .. }
        if text == &note.text && note_id == &note.note_id)));
    assert_eq!(
        history
            .events
            .iter()
            .filter(|event| matches!(
                &event.kind,
                bcode_session_models::SessionEventKind::PluginStatusNote { .. }
            ))
            .count(),
        6
    );
    assert!(history.events.iter().all(|event| !matches!(
        &event.kind,
        bcode_session_models::SessionEventKind::UserMessage { .. }
    )));
    let compaction_input = SessionApplication::session_history_page(
        &ipc,
        direct.compaction_session_id,
        history_query(),
    )
    .await
    .unwrap();
    assert_compaction_input_equal(&direct.compaction_history, &compaction_input);
    let compaction = bounded_phase(
        "ipc-empty-session-compaction",
        10,
        SessionApplication::compact_session(&ipc, direct.compaction_session_id),
    )
    .await
    .unwrap();
    assert_eq!(
        direct.compaction, compaction,
        "same-state exact compaction no-op reason parity"
    );
    let after_compaction = SessionApplication::session_history_page(
        &ipc,
        direct.compaction_session_id,
        history_query(),
    )
    .await
    .unwrap();
    assert_skipped_trace_appended(&compaction_input, &after_compaction);
    let spawn_result = SessionApplication::spawn_session(&ipc, direct.spawn_request).await;
    spawn_result.validate().unwrap();
    assert_eq!(
        direct.spawn_result, spawn_result,
        "composed spawn failure receipts are frontend-independent"
    );
    let summary = SessionApplication::session_summary(&ipc, direct.session.id)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&summary).unwrap(),
        serde_json::to_value(&direct.session).unwrap()
    );
    let ipc_page = SessionApplication::inspect_session(&ipc, direct.session.id, query())
        .await
        .unwrap();
    assert_eq!(
        direct.page, ipc_page,
        "canonical history is identical across boundaries"
    );
    let unchanged = SessionApplication::change_session_working_directory(
        &ipc,
        direct.session.id,
        direct.session.working_directory.clone(),
    )
    .await
    .unwrap();
    assert!(!unchanged.changed);
    assert_eq!(
        serde_json::to_value(&direct.session).unwrap(),
        serde_json::to_value(&unchanged.session).unwrap()
    );
    let created = SessionApplication::create_session(
        &ipc,
        CreateSessionRequest {
            name: Some("direct → IPC 🦀".to_owned()),
            working_directory: directory,
        },
    )
    .await
    .unwrap();
    let renamed =
        SessionApplication::rename_session(&ipc, created.id, Some("e\u{301} renamed".to_owned()))
            .await
            .unwrap();
    assert_eq!(renamed.title(), direct.session.title());
    let changed = SessionApplication::change_session_working_directory(
        &ipc,
        created.id,
        direct.session.working_directory,
    )
    .await
    .unwrap();
    assert!(changed.changed);
    let page = SessionApplication::inspect_session(&ipc, created.id, query())
        .await
        .unwrap();
    let direct_kinds: Vec<_> = direct.page.events.iter().map(|event| &event.kind).collect();
    let ipc_kinds: Vec<_> = page.events.iter().map(|event| &event.kind).collect();
    assert_eq!(
        direct_kinds, ipc_kinds,
        "same mutations produce the same canonical event semantics"
    );
    let create_error = SessionApplication::create_session(
        &ipc,
        CreateSessionRequest {
            name: None,
            working_directory: "relative".into(),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(
        direct.create_error.code, create_error.code,
        "create validation error parity"
    );
    let directory_error =
        SessionApplication::change_session_working_directory(&ipc, created.id, "relative".into())
            .await
            .unwrap_err();
    assert_eq!(
        direct.directory_error.code, directory_error.code,
        "directory validation error parity"
    );
    let missing_error =
        SessionApplication::rename_session(&ipc, direct.missing_id, Some("missing".to_owned()))
            .await
            .unwrap_err();
    assert_eq!(
        direct.missing_error.code, missing_error.code,
        "missing-session error parity"
    );
}

async fn auth_preference_direct<A: bcode_provider_auth_models::application::AuthPoolApplication>(
    application: &A,
) -> bcode_provider_auth_models::application::AuthPoolPreference {
    let directory = std::env::current_dir().unwrap();
    std::fs::write(directory.join("bcode.toml"), "[plugins]\ndefault = \"none\"\n[auth.profiles.first]\nbackend = \"sshenv\"\nscheme = \"chatgpt\"\n[auth.profiles.second]\nbackend = \"sshenv\"\nscheme = \"chatgpt\"\n[auth.pools.test]\nprofiles = [\"first\", \"second\"]\nstrategy = \"failover\"\n").unwrap();
    application
        .apply_auth_pool_preference(bcode_provider_auth_models::SetAuthPoolPreferenceRequest {
            pool: "test".to_owned(),
            profile: Some("second".to_owned()),
        })
        .await
        .unwrap();
    let pools = application.auth_pool_list().await.unwrap();
    let pool = pools.iter().find(|pool| pool.pool == "test").unwrap();
    assert_eq!(pool.preferred_profile.as_deref(), Some("second"));
    let preference = application
        .auth_pool_preference("test".to_owned())
        .await
        .unwrap();
    assert_eq!(preference.profile.as_deref(), Some("second"));
    preference
}

async fn command_direct<A: bcode_command::application::CommandApplication>(
    application: &A,
) -> (
    bcode_command::application::CommandInvocationResult,
    bcode_command::application::CommandCancellationAcknowledgment,
) {
    assert!(
        application
            .list_commands("missing.command.owner".to_owned())
            .await
            .is_err()
    );
    let result = application
        .invoke_command(bcode_command::application::CommandInvocationRequest {
            plugin_id: "missing.command.owner".to_owned(),
            invocation_id: "missing-command-1".to_owned(),
            agent_id: None,
            launch_root: Some(std::env::current_dir().unwrap()),
            deadline_ms: 1000,
            command: bcode_command::InvokeCommandRequest {
                command_id: "missing".to_owned(),
                args: std::collections::BTreeMap::new(),
                context: None,
            },
        })
        .await;
    assert_eq!(result.invocation_id, "missing-command-1");
    assert!(matches!(
        &result.outcome,
        bcode_command::application::CommandInvocationOutcome::Failed(_)
    ));
    let cancellation = application
        .cancel_command("no-active-command".to_owned())
        .await
        .unwrap();
    assert_eq!(cancellation.invocation_id, "no-active-command");
    assert!(
        !cancellation.accepted,
        "absence must not claim terminal cancellation"
    );
    (result, cancellation)
}

async fn host_fixture() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_target(false)
        .with_ansi(false)
        .try_init();
    let directory = std::env::current_dir().unwrap().canonicalize().unwrap();
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(directory.join("parity.sock"));
    let ipc = bcode_client::BcodeClient::new(endpoint.clone());
    let plugins = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
        &bcode_plugin::PluginSelection {
            mode: bcode_plugin::PluginSelectionMode::Explicit,
            enabled: std::collections::BTreeSet::new(),
            disabled: std::collections::BTreeSet::new(),
        },
        &[],
    )
    .unwrap();
    let shutdown = bcode_agent_runtime::CancellationToken::new();
    let signal = shutdown.clone();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let ready: bcode_server::EmbeddedWorkflowReady = Box::new(move |application| {
        Box::pin(async move {
            let _callback_phase = Phase::start("embedded-ready-callback");
            use bcode_daemon_lifecycle::application::RuntimeApplication as _;
            use bcode_model_catalog::application::ModelApplication as _;
            use bcode_workflow_view_models::application::SessionProgressApplication as _;
            let services = application.services_application();
            let models = services.model_catalog_snapshot().await.unwrap();
            assert!(
                !models.providers.is_empty(),
                "full model catalog must not become a session snapshot"
            );
            let catalog_response =
                bcode_ipc::Response::Ok(bcode_ipc::ResponsePayload::ModelCatalogSnapshot {
                    catalog: Box::new(models.clone()),
                });
            let encoded = bcode_ipc::encode_response(&catalog_response).unwrap();
            eprintln!(
                "[parity-codec] variant=ModelCatalogSnapshot codec=typed-stable bytes={}",
                encoded.len()
            );
            let decoded = bcode_ipc::decode_response(&encoded).unwrap_or_else(|error| {
                let category = match error {
                    bcode_ipc::CodecError::Deserialize(ref cause) => {
                        // Inspect only known codec classifications, never log serde's
                        // potentially payload-bearing diagnostic message.
                        let message = cause.to_string();
                        if message.contains("deserialize_any") { "deserialize/unsupported_deserialize_any" }
                        else if message.contains("type not supported") || message.contains("unsupported type") { "deserialize/unsupported_type" }
                        else if message.contains("unexpected end") { "deserialize/unexpected_eof" }
                        else if message.contains("variant") { "deserialize/invalid_variant_or_shape" }
                        else if message.contains("trailing") { "deserialize/trailing_bytes" }
                        else { "deserialize/other" }
                    },
                    bcode_ipc::CodecError::Serialize(_) => "serialize",
                    bcode_ipc::CodecError::PayloadTooLarge { .. } => "payload_too_large",
                    bcode_ipc::CodecError::InvalidChunk(_) => "invalid_chunk",
                    bcode_ipc::CodecError::Io(_) => "io",
                    bcode_ipc::CodecError::UnsupportedVersion { .. } => "unsupported_version",
                    bcode_ipc::CodecError::EventConversion(_) => "event_conversion",
                };
                panic!("[parity-codec] ModelCatalogSnapshot typed-stable roundtrip failed category={category}; no payload logged");
            });
            assert_eq!(
                decoded, catalog_response,
                "full catalog response codec roundtrip"
            );
            let frames = bcode_ipc::encode_envelope_frames(&bcode_ipc::Envelope::new(
                77,
                bcode_ipc::EnvelopeKind::Response,
                encoded,
            ))
            .unwrap();
            let framed: Vec<u8> = frames.into_iter().flatten().collect();
            let envelope = bounded_phase(
                "catalog-raw-frame-reassembly",
                5,
                bcode_ipc::recv_envelope(&mut framed.as_slice()),
            )
            .await
            .unwrap();
            assert_eq!(envelope.request_id, 77);
            assert_eq!(
                bcode_ipc::decode_response(&envelope.payload).unwrap(),
                catalog_response
            );
            let model_refresh = bounded_phase(
                "direct-model-catalog-refresh",
                10,
                services.refresh_model_catalog(),
            )
            .await
            .unwrap();
            assert!(
                !model_refresh.remote_enabled,
                "embedded fixture must not use remote model discovery"
            );
            let runtime = services.runtime_status().await.unwrap();
            assert_eq!(runtime.connected_client_count, 0);
            let command_evidence = bounded_phase(
                "direct-command-consumer",
                10,
                command_direct(&application.command_application()),
            )
            .await;
            let auth_preference = bounded_phase(
                "direct-auth-consumer",
                10,
                auth_preference_direct(&services),
            )
            .await;
            let direct = bounded_phase(
                "direct-session-consumer",
                20,
                direct_consumer(&application.session_application(), &directory),
            )
            .await;
            let progress = services
                .session_progress(direct.note_request.session_id)
                .await
                .unwrap();
            assert!(
                matches!(
                    &progress.observation,
                    bcode_workflow_view_models::SessionProgressObservation::Unavailable { .. }
                ),
                "unowned session coverage must not be reported idle"
            );
            let missing_progress = services.session_progress(direct.missing_id).await.unwrap();
            assert_eq!(missing_progress.session_id, direct.missing_id.to_string());
            assert_eq!(missing_progress.observation,
                bcode_workflow_view_models::SessionProgressObservation::Unavailable {
                    reason: bcode_workflow_view_models::SessionProgressUnavailableReason::CoverageUnavailable,
                }, "nonowning observation must not manufacture coverage for a missing identity");
            // The borrowed application stays in the startup callback. IPC starts only
            // after returning to the host's accept loop, using owned result evidence.
            let consumer = tokio::spawn(async move {
                let _ipc_phase = Phase::start("ipc-services-consumer");
                use bcode_daemon_lifecycle::application::{
                    RuntimeApplication as _, RuntimeShutdownMode,
                };
                use bcode_model_catalog::application::ModelApplication as _;
                use bcode_provider_auth_models::application::AuthPoolApplication as _;
                let ipc_progress = bcode_workflow_view_models::application::SessionProgressApplication::session_progress(
                    &ipc, direct.note_request.session_id).await.unwrap();
                assert_eq!(
                    progress, ipc_progress,
                    "content-free unavailable observation parity"
                );
                let ipc_missing_progress = bcode_workflow_view_models::application::SessionProgressApplication::session_progress(
                    &ipc, direct.missing_id).await.unwrap();
                assert_eq!(
                    missing_progress, ipc_missing_progress,
                    "missing-identity unavailable progress parity"
                );
                assert_eq!(
                    serde_json::to_value(&models).unwrap(),
                    serde_json::to_value(ipc.model_catalog_snapshot().await.unwrap()).unwrap(),
                    "full model catalog identity/contents must be transport-independent"
                );
                let refreshed = ipc.refresh_model_catalog().await.unwrap();
                assert_eq!(model_refresh.embedded_revision, refreshed.embedded_revision);
                assert!(!refreshed.remote_enabled);
                let status = ipc.runtime_status().await.unwrap();
                assert!(status.connected_client_count >= 1);
                assert_eq!(
                    runtime.selected_provider_plugin_id,
                    status.selected_provider_plugin_id
                );
                assert_eq!(runtime.selected_model_id, status.selected_model_id);
                assert_eq!(
                    runtime.workflow_unavailable_reason,
                    status.workflow_unavailable_reason
                );
                let ipc_command_evidence =
                    bounded_phase("ipc-command-consumer", 10, command_direct(&ipc)).await;
                assert_eq!(
                    command_evidence, ipc_command_evidence,
                    "typed command failure/cancellation acknowledgment parity"
                );
                let config = std::fs::read(directory.join("bcode.toml")).unwrap();
                let pools =
                    bcode_provider_auth_models::application::AuthPoolApplication::auth_pool_list(
                        &ipc,
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    pools
                        .iter()
                        .find(|pool| pool.pool == "test")
                        .unwrap()
                        .preferred_profile
                        .as_deref(),
                    Some("second")
                );
                assert_eq!(
                    auth_preference,
                    ipc.auth_pool_preference("test".to_owned()).await.unwrap()
                );
                bcode_provider_auth_models::application::AuthPoolApplication::apply_auth_pool_preference(&ipc,
                    bcode_provider_auth_models::SetAuthPoolPreferenceRequest { pool: "test".to_owned(), profile: None }).await.unwrap();
                assert_eq!(
                    ipc.auth_pool_preference("test".to_owned())
                        .await
                        .unwrap()
                        .profile
                        .as_deref(),
                    Some("first")
                );
                assert_eq!(config, std::fs::read(directory.join("bcode.toml")).unwrap());
                bounded_phase(
                    "ipc-session-consumer",
                    20,
                    ipc_consumer(ipc.clone(), direct, directory),
                )
                .await;
                let receipt = bounded_phase(
                    "ipc-runtime-shutdown-admission",
                    10,
                    ipc.request_runtime_shutdown(RuntimeShutdownMode::Force),
                )
                .await
                .unwrap();
                assert_eq!(
                    receipt.mode,
                    RuntimeShutdownMode::Force,
                    "shutdown receipt is admission, not proof work had no effects"
                );
            });
            tokio::spawn(async move {
                let result = consumer.await;
                eprintln!("[parity-phase] ipc-consumer-task-settled; cancelling-host");
                signal.cancel();
                let _ = sender.send(result);
            });
        })
    });
    bounded_phase(
        "embedded-host-start-callback-serve-shutdown",
        70,
        bcode_server::run_embedded_with_workflow_application(
            endpoint,
            bcode_config::BcodeConfig::default(),
            plugins,
            bcode_model_catalog::ModelCatalogResolver::embedded(),
            Vec::new(),
            shutdown,
            ready,
        ),
    )
    .await
    .unwrap();
    bounded_phase("ipc-consumer-result-after-host-exit", 5, receiver)
        .await
        .unwrap()
        .unwrap();
}

/// Owns the fixture daemon until all CLI callers have exited; never uses active state.
struct CliFixture {
    daemon: std::process::Child,
    root: tempfile::TempDir,
}

impl Drop for CliFixture {
    fn drop(&mut self) {
        reap_child(&mut self.daemon, "cli-fixture-daemon-reap");
    }
}

impl CliFixture {
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bcode"));
        command
            .env_clear()
            .env("HOME", self.root.path())
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_DATA_HOME", self.root.path().join("data"))
            .env("BCODE_STATE_DIR", self.root.path().join("state"))
            .current_dir(self.root.path());
        command
    }

    fn start() -> Self {
        let _phase = Phase::start("cli-daemon-spawn-and-ready-poll");
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("bcode.toml"),
            "[plugins]\ndefault = \"none\"\n",
        )
        .unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_bcode"));
        let daemon = command
            .env_clear()
            .env("HOME", root.path())
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("BCODE_STATE_DIR", root.path().join("state"))
            .current_dir(root.path())
            .args(["server", "run"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(root.path().join("daemon.log")).unwrap())
            .spawn()
            .unwrap();
        let mut fixture = Self { daemon, root };
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(
                fixture.daemon.try_wait().unwrap().is_none(),
                "fixture daemon exited"
            );
            if fixture.run(&["server", "status"]).status.success() {
                return fixture;
            }
            assert!(
                Instant::now() < deadline,
                "fixture daemon startup timed out"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        let operation = format!(
            "cli-{}-{}",
            args.first().copied().unwrap_or("none"),
            args.get(1).copied().unwrap_or("none")
        );
        let _phase = Phase::start(operation.clone());
        use std::io::Read as _;
        let mut child = self
            .command()
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // Drain both pipes concurrently; bound retained bytes without blocking writers.
        let drain = |mut pipe: Box<dyn std::io::Read + Send>| {
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let retained = pipe
                    .by_ref()
                    .take(2 * 1024 * 1024 + 1)
                    .read_to_end(&mut bytes);
                let drained = std::io::copy(&mut pipe, &mut std::io::sink());
                let _ = sender.send(retained.and(drained).map(|_| bytes));
            });
            receiver
        };
        let stdout = drain(Box::new(child.stdout.take().unwrap()));
        let stderr = drain(Box::new(child.stderr.take().unwrap()));
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                reap_child(&mut child, "cli-operation-timeout-reap");
                panic!("CLI operation timed out: {operation}; effects may be unknown");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        eprintln!("[parity-phase] process-exited {operation}; waiting for pipe EOF");
        let stdout = stdout.recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("CLI stdout EOF timed out after process exit: {operation}; descendant may hold pipe"))
            .expect("CLI stdout capture failed");
        let stderr = stderr.recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("CLI stderr EOF timed out after process exit: {operation}; descendant may hold pipe"))
            .expect("CLI stderr capture failed");
        assert!(
            stdout.len() <= 2 * 1024 * 1024 && stderr.len() <= 2 * 1024 * 1024,
            "fixture output exceeded bounded capture"
        );
        std::process::Output {
            status,
            stdout,
            stderr,
        }
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

#[test]
fn cli_catalog_refresh_note_and_progress_preserve_canonical_history() {
    let fixture = CliFixture::start();
    let created = fixture.json(&["session", "create", "bounded automation", "--json"]);
    let id = created["id"].as_str().unwrap();
    let compaction_before: bcode_session_models::SessionHistoryPage = serde_json::from_value(
        fixture.json(&["session", "history", id, "--limit", "100", "--json"]),
    )
    .unwrap();
    let noop = fixture.json(&["session", "compact", id, "--json"]);
    assert_eq!(
        noop["status"], "context_compaction_noop",
        "empty compaction must not claim completed work"
    );
    assert_eq!(noop["session_id"], id);
    assert_eq!(noop["message"], "nothing new to compact");
    let compaction_after: bcode_session_models::SessionHistoryPage = serde_json::from_value(
        fixture.json(&["session", "history", id, "--limit", "100", "--json"]),
    )
    .unwrap();
    assert_skipped_trace_appended(&compaction_before, &compaction_after);
    let repeated_noop = fixture.json(&["session", "compact", id, "--json"]);
    assert_eq!(
        noop, repeated_noop,
        "diagnostic trace must not change the same-input no-op semantic result"
    );
    let repeated_history: bcode_session_models::SessionHistoryPage = serde_json::from_value(
        fixture.json(&["session", "history", id, "--limit", "100", "--json"]),
    )
    .unwrap();
    assert_skipped_trace_appended(&compaction_after, &repeated_history);
    let output = fixture.run(&[
        "session",
        "compact",
        "00000000-0000-4000-8000-000000000099",
        "--json",
    ]);
    assert!(
        !output.status.success(),
        "missing-session compaction must not claim completion"
    );
    assert!(output.stdout.is_empty());
    let unavailable = fixture.json(&["session", "progress", id, "--json"]);
    assert_eq!(
        unavailable["observation"]["status"], "unavailable",
        "missing coverage must explicitly remain unavailable, not idle"
    );
    let missing_id = "00000000-0000-4000-8000-000000000099";
    let missing_progress = fixture.json(&["session", "progress", missing_id, "--json"]);
    assert_eq!(missing_progress["session_id"], missing_id);
    assert_eq!(missing_progress["observation"], serde_json::to_value(
        bcode_workflow_view_models::SessionProgressObservation::Unavailable {
            reason: bcode_workflow_view_models::SessionProgressUnavailableReason::CoverageUnavailable,
        }).unwrap(), "CLI preserves the same portable unavailable coverage observation as direct/IPC");
    let history = fixture.json(&["session", "history", id, "--limit", "100", "--json"]);
    let refresh = fixture.json(&["session", "refresh", "--json"]);
    assert_eq!(refresh["schema_version"], 1);
    assert!(refresh["catalog_revision"].is_u64());
    assert!(refresh["catalog_sources"].is_array());
    assert!(
        !refresh["catalog_status"].is_null(),
        "loading/degradation must remain explicit"
    );
    let catalog = fixture.json(&["session", "list", "--with-status", "--json"]);
    assert_eq!(catalog["schema_version"], 1);
    assert!(
        catalog["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| session["id"] == id)
    );
    let progress = fixture.json(&["session", "progress", id, "--json"]);
    assert_eq!(progress["session_id"], id);
    assert!(
        !progress["observation"].is_null(),
        "unavailable must not be hidden as idle"
    );
    assert_eq!(
        history,
        fixture.json(&["session", "history", id, "--limit", "100", "--json"]),
        "catalog and progress reads must not append history"
    );
    let text = "**presentation e\u{301} 🦀**\n";
    std::fs::write(fixture.root.path().join("note.md"), text).unwrap();
    assert_eq!(
        fixture.json(&[
            "session",
            "append-presentation-note",
            id,
            "--source-id",
            "automation",
            "--note-id",
            "note",
            "--text-file",
            "note.md",
            "--format",
            "markdown"
        ]),
        serde_json::Value::Null
    );
    let after = fixture.json(&["session", "history", id, "--limit", "100", "--json"]);
    let events = after["events"].as_array().unwrap();
    let notes: Vec<_> = events
        .iter()
        .filter_map(|event| event["kind"].get("plugin_status_note"))
        .collect();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["text"], text);
    assert_eq!(notes[0]["note_id"], "note");
    assert!(
        events
            .iter()
            .all(|event| event["kind"].get("user_message").is_none()),
        "note must not admit a turn"
    );
}

#[test]
fn cli_auth_pool_preferences_are_user_state_not_configuration_or_turns() {
    let fixture = CliFixture::start();
    let config = "[plugins]\ndefault = \"none\"\n[auth.profiles.first]\nbackend = \"sshenv\"\nscheme = \"chatgpt\"\n[auth.profiles.second]\nbackend = \"sshenv\"\nscheme = \"chatgpt\"\n[auth.pools.test]\nprofiles = [\"first\", \"second\"]\nstrategy = \"failover\"\n";
    let config_path = fixture.root.path().join("bcode.toml");
    std::fs::write(&config_path, config).unwrap();
    let created = fixture.json(&["session", "create", "preference survivor", "--json"]);
    let id = created["id"].as_str().unwrap();
    let history = fixture.json(&["session", "history", id, "--limit", "100", "--json"]);
    let set = fixture.json(&[
        "session",
        "set-auth-pool",
        "test",
        "--profile",
        "second",
        "--json",
    ]);
    assert_eq!(set["status"], "auth_pool_preference_set");
    let pools = fixture.json(&["auth", "pool", "daemon-status"]);
    let pool = pools
        .as_array()
        .unwrap()
        .iter()
        .find(|pool| pool["pool"] == "test")
        .unwrap();
    assert_eq!(pool["preferred_profile"], "second");
    assert_eq!(pool["preference_source"], "interactive_state");
    fixture.json(&["session", "set-auth-pool", "test", "--clear", "--json"]);
    let pools = fixture.json(&["auth", "pool", "daemon-status"]);
    let pool = pools
        .as_array()
        .unwrap()
        .iter()
        .find(|pool| pool["pool"] == "test")
        .unwrap();
    assert_eq!(pool["preferred_profile"], "first");
    assert_eq!(pool["preference_source"], "pool_order");
    let output = fixture.run(&[
        "session",
        "set-auth-pool",
        "test",
        "--profile",
        "private-invalid-profile-canary",
        "--json",
    ]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("private-invalid-profile-canary"));
    assert_eq!(std::fs::read_to_string(config_path).unwrap(), config);
    assert_eq!(
        history,
        fixture.json(&["session", "history", id, "--limit", "100", "--json"])
    );
}

#[test]
fn cli_missing_search_maintenance_provider_preserves_canonical_session() {
    let fixture = CliFixture::start();
    let created = fixture.json(&["session", "create", "maintenance survivor", "--json"]);
    let id = created["id"].as_str().unwrap();
    let history = fixture.json(&["session", "history", id, "--limit", "100", "--json"]);
    for operation in ["search-purge", "search-rebuild"] {
        let output = fixture.run(&[
            "session",
            operation,
            "--provider",
            "missing.provider",
            "--confirm",
            "private-confirmation-canary",
            "--json",
        ]);
        assert!(
            !output.status.success(),
            "missing provider must not report maintenance completed"
        );
        assert!(output.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("private-confirmation-canary"));
        assert_eq!(
            history,
            fixture.json(&["session", "history", id, "--limit", "100", "--json"])
        );
        assert_eq!(
            created["id"],
            fixture.json(&["session", "summary", id])["id"]
        );
    }
}
