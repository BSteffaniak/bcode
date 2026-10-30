#![cfg(feature = "renderer-html-actix")]
#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Opt-in real browser -> registered router -> IPC -> durable backend acceptance.
//! No CLI daemon discovery, user configuration, external provider, or shared state.
//! HTTP is the production HTML/Actix adapter, running in an exact-executable,
//! test-owned child with immutable validated assets. No proxy rewrites requests.
//!
//! Coordinator invocation after building the actual WASM distribution:
//! `BCODE_E2E_CHROMIUM=/absolute/chromium BCODE_E2E_BUNDLE=/absolute/dist
//! BCODE_E2E_PYTHON=/absolute/python3 cargo test --locked -p bcode_hyperchad
//! --features renderer-html-actix,static-bundled-question-plugin
//! --test browser_e2e -- --ignored --nocapture`
//! Run under the locked Nix toolchain; no build or browser fallback is performed.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bcode_client::BcodeClient;
use bcode_hyperchad::{HyperChadAppState, router_from_state};
use bcode_session_models::{
    SessionEvent, SessionEventKind, SessionHistoryDirection, SessionHistoryQuery, SessionId,
    SubmissionReceipt,
};
use hyperchad::renderer::Content;
use hyperchad::router::{RequestInfo, RouteRequest};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

type TestResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[path = "browser_acceptance/backend.rs"]
mod backend;
#[path = "browser_acceptance/policy_roundtrip.rs"]
mod policy_roundtrip;
#[path = "browser_acceptance/scenarios.rs"]
mod scenarios;

#[tokio::test]
#[ignore = "exact-executable isolated backend child"]
async fn production_backend_child() -> TestResult<()> {
    let root = PathBuf::from(
        std::env::var_os("BCODE_BACKEND_FIXTURE_ROOT").ok_or("fixture root required")?,
    );
    backend::serve(root).await
}

fn require(condition: bool, message: &str) -> TestResult<()> {
    if !condition {
        return Err(std::io::Error::other(message).into());
    }
    Ok(())
}

#[cfg(feature = "renderer-html-actix")]
#[test]
#[ignore = "test-owned Actix child; invoked only by browser acceptance parent"]
fn production_actix_child() -> TestResult<()> {
    use hyperchad::renderer::assets::{AssetPathTarget, StaticAssetRoute};
    let Ok(fixture) = std::env::var("BCODE_ACTIX_FIXTURE") else {
        return Err("Actix fixture path required".into());
    };
    let fixture: Value = serde_json::from_slice(&std::fs::read(fixture)?)?;
    let root = PathBuf::from(fixture["root"].as_str().ok_or("root")?);
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(PathBuf::from(
        fixture["socket"].as_str().ok_or("socket")?,
    ));
    let state_root = PathBuf::from(fixture["state_root"].as_str().ok_or("state_root")?);
    let locations = bcode_config::resolve_state_location_set_with_environment(
        &bcode_config::StateConfig::default(),
        &bcode_config::StateLocationSelection {
            root: Some(state_root),
            profile: None,
        },
        &bcode_config::ConfigEnvironmentSnapshot::new(BTreeMap::new(), root.clone()),
    )?;
    let client = BcodeClient::for_state_location(endpoint, locations.primary());
    let policy = bcode_tui::browser_presentation::export_with_plugins(
        &config(),
        &root,
        &bcode_plugin::PluginSelection {
            mode: bcode_plugin::PluginSelectionMode::Explicit,
            enabled: plugin_ids().into_iter().collect(),
            disabled: BTreeSet::new(),
        },
        &static_plugins(),
    )?;
    let state = HyperChadAppState::new_in_working_directory(
        client,
        fixture["token"].as_str().ok_or("token")?,
        root,
    )
    .with_browser_policy(policy)?;
    let origin_state = state.clone();
    let address_file = PathBuf::from(fixture["address_file"].as_str().ok_or("address_file")?);
    let builder = bcode_hyperchad::init_with_snapshot(
        bcode_session_view_models::SessionViewSnapshot::empty(),
        Vec::new(),
    )
    .with_router(router_from_state(state))
    .with_actix_bind_address("127.0.0.1".to_owned())
    .with_actix_port(0)
    .with_actix_on_bound(move |address| {
        let _state = origin_state.clone().with_browser_origins(&[
            format!("http://{address}"), "https://embed.example.invalid".into(),
        ]).expect("install exact fixture listener and embed origins");
        std::fs::write(&address_file, address.to_string()).expect("publish test listener");
    })
    .with_static_asset_route(StaticAssetRoute {
        route: "backend-e2e.html".into(),
        target: AssetPathTarget::FileContents(
            include_bytes!("../../tui-web/tests/backend-e2e.html")
                .as_slice()
                .into(),
        ),
        not_found_behavior: None,
    })
    .with_static_asset_route(StaticAssetRoute {
        route: "production-evidence.mjs".into(),
        target: AssetPathTarget::FileContents(
            include_bytes!("../../tui-web/tests/production-evidence.mjs")
                .as_slice()
                .into(),
        ),
        not_found_behavior: None,
    })
    .with_static_asset_route(StaticAssetRoute {
        route: "production-scenarios.mjs".into(),
        target: AssetPathTarget::FileContents(
            include_bytes!("../../tui-web/tests/production-scenarios.mjs").as_slice().into(),
        ),
        not_found_behavior: None,
    })
    .with_static_asset_route(StaticAssetRoute {
        route: "fixture.json".into(),
        target: AssetPathTarget::FileContents(
            serde_json::to_vec(&json!({"session": fixture["session"], "token": fixture["token"], "scenarios": fixture["scenarios"]}))?
                .into(),
        ),
        not_found_behavior: None,
    });
    let assets =
        bcode_hyperchad::TuiAssets::load(Path::new(fixture["bundle"].as_str().ok_or("bundle")?))?;
    let (mut app, _runtime) = bcode_hyperchad::build_app_with_runtime(assets.with_routes(builder))
        .map_err(|error| error.to_string())?;
    bcode_hyperchad::configure_browser_origins(
        &mut app,
        &["https://embed.example.invalid".into()],
    )?;
    app.handle_serve().map_err(|error| error.to_string())?;
    Ok(())
}

struct ActixChild(std::process::Child);
impl Drop for ActixChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn static_plugins() -> Vec<bcode_plugin::StaticBundledPlugin> {
    vec![
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/fake-provider-plugin/bcode-plugin.toml"),
            bcode_fake_provider_plugin::static_plugin(),
        ),
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/question-plugin/bcode-plugin.toml"),
            bcode_question_plugin::static_plugin(),
        ),
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/shell-plugin/bcode-plugin.toml"),
            bcode_shell_plugin::static_plugin(),
        ),
        bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/filesystem-plugin/bcode-plugin.toml"),
            bcode_filesystem_plugin::static_plugin(),
        ),
    ]
}

fn plugin_ids() -> Vec<String> {
    vec![
        "bcode.fake-provider".into(),
        "bcode.question".into(),
        "bcode.filesystem".into(),
        "bcode.shell".into(),
    ]
}

fn plugins() -> TestResult<bcode_plugin::PluginRuntimeHost> {
    Ok(
        bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
            &bcode_plugin::PluginSelection {
                mode: bcode_plugin::PluginSelectionMode::Explicit,
                enabled: BTreeSet::from([
                    "bcode.fake-provider".into(),
                    "bcode.question".into(),
                    "bcode.filesystem".into(),
                    "bcode.shell".into(),
                ]),
                disabled: BTreeSet::new(),
            },
            &static_plugins(),
        )?,
    )
}

fn config() -> bcode_config::BcodeConfig {
    let mut config = bcode_config::BcodeConfig::default();
    config.model.provider_plugin_id = Some("bcode.fake-provider".into());
    config.model.model_id = Some("fake-echo".into());
    config
        .tui
        .keybindings
        .chat
        .insert("enter".into(), "tui.input.newline".into());
    config
        .tui
        .keybindings
        .chat
        .insert("ctrl+enter".into(), "tui.input.submitSteering".into());
    config
        .tui
        .keybindings
        .chat
        .insert("ctrl+alt+x".into(), "app.interrupt".into());
    config
        .model
        .settings
        .insert("fake_prompt_tool_directives".into(), "true".into());
    config
        .agent
        .entry("build".into())
        .or_default()
        .permission
        .write
        .insert(
            "*".into(),
            serde_json::from_value(json!("ask")).expect("ask policy"),
        );
    config
        .agent
        .entry("build".into())
        .or_default()
        .permission
        .read
        .insert(
            "*".into(),
            serde_json::from_value(json!("allow")).expect("read policy"),
        );
    config.tools.shell.env.mode = bcode_config::ShellToolEnvMode::Inherit;
    config
        .agent
        .entry("build".into())
        .or_default()
        .permission
        .command
        .insert(
            "cat browser-overbudget.txt".into(),
            serde_json::from_value(json!("allow")).expect("exact fixture command policy"),
        );
    scenarios::configure(&mut config);
    config.skills.enabled = false;
    config.model.prompt_cache.mode = serde_json::from_value(json!("off"))
        .expect("valid isolated fake-provider prompt cache mode");
    config
}

async fn ready(client: &BcodeClient) -> TestResult<()> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            // Verifies the exact integration-test executable and isolated state location.
            if client.verified_server_status().await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await?;
    Ok(())
}

async fn history(client: &BcodeClient, session: SessionId) -> TestResult<Vec<SessionEvent>> {
    let page = client
        .session_history_page(
            session,
            SessionHistoryQuery {
                cursor: None,
                limit: 1_000,
                direction: SessionHistoryDirection::Forward,
            },
        )
        .await?;
    require(
        !page.has_more,
        "fixture unexpectedly exceeds one bounded canonical page",
    )?;
    Ok(page.events)
}

fn canonical(events: &[SessionEvent], receipt: &SubmissionReceipt, text: &str) -> TestResult<()> {
    require(
        events
            .iter()
            .filter(|event| matches!(event.kind, SessionEventKind::SubmissionAccepted { .. }))
            .count()
            == 1,
        "auth rejection and conflicting retry must create no extra canonical acceptances",
    )?;
    require(
        events
            .iter()
            .filter(|event| matches!(event.kind, SessionEventKind::SubmissionActivated { .. }))
            .count()
            == 1,
        "auth rejection and retry must create no extra activations",
    )?;
    let accepted: Vec<_> = events
        .iter()
        .filter(|event| {
            matches!(
                &event.kind, SessionEventKind::SubmissionAccepted { submission }
                    if submission.id == receipt.submission_id && submission.text == text
            )
        })
        .collect();
    require(
        accepted.len() == 1,
        "exactly one canonical acceptance required",
    )?;
    require(
        accepted[0].sequence == receipt.accepted_event_sequence,
        "receipt must name canonical acceptance sequence",
    )?;
    require(events.iter().filter(|event| matches!(
        &event.kind, SessionEventKind::SubmissionActivated { submission_id, text: actual, .. }
            if submission_id == &receipt.submission_id && actual == text
    )).count() == 1, "retry must not activate a second turn")?;
    require(
        events
            .iter()
            .filter(|event| {
                matches!(
                    &event.kind,
                    SessionEventKind::AssistantMessage { text: actual }
                    | SessionEventKind::AssistantResponseSegment { text: actual, .. }
                    | SessionEventKind::PositionedAssistantResponseSegment { text: actual, .. }
                        if actual == &format!("fake: {text}")
                )
            })
            .count()
            == 1,
        "one deterministic real provider response required",
    )
}

// This integration lifecycle keeps fixture startup, browser execution, and
// canonical verification in one scope so their owned children cannot escape cleanup.
#[allow(clippy::too_many_lines)]
async fn exercise(
    client: &BcodeClient,
    root: &Path,
    chromium: PathBuf,
    bundle: PathBuf,
    python: PathBuf,
) -> TestResult<(SessionId, SubmissionReceipt, String, Value)> {
    ready(client).await?;
    let session = client
        .create_session_in_working_directory(Some("browser-e2e".into()), root.to_path_buf())
        .await?;
    let scenarios = scenarios::create(client, root).await?;
    let token = SessionId::new().to_string();
    let address_file = root.join("actix-address");
    let fixture_file = root.join("actix-fixture.json");
    std::fs::write(
        &fixture_file,
        serde_json::to_vec(&json!({
            "root": root,
            "socket": root.join("ipc/test.sock"),
            "state_root": root.join("state"),
            "token": token,
            "session": session.id,
            "scenarios": scenarios,
            "bundle": bundle,
            "address_file": address_file,
        }))?,
    )?;
    let mut http = ActixChild(
        std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "production_actix_child",
                "--ignored",
                "--nocapture",
            ])
            .env("BCODE_ACTIX_FIXTURE", fixture_file)
            .stdin(std::process::Stdio::null())
            .spawn()?,
    );
    let address = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(address) = std::fs::read_to_string(&address_file) {
                break Ok::<_, Box<dyn std::error::Error + Send + Sync>>(address);
            }
            if let Some(status) = http.0.try_wait()? {
                break Err(format!("Actix fixture exited: {status}").into());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await??;
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/test-tui-web-e2e.py");
    let fixture_root = root.to_path_buf();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(python)
            .arg(script)
            .arg("--chromium")
            .arg(chromium)
            .arg("--bundle")
            .arg(bundle)
            .arg("--backend")
            .arg(format!("http://{address}"))
            .arg("--session")
            .arg(session.id.to_string())
            .arg("--fixture-root")
            .arg(fixture_root)
            .arg("--token")
            .arg(token)
            .output()
    })
    .await;
    drop(http);
    let output = output??;
    if !output.status.success() {
        let events = history(client, session.id).await?;
        let accepted = events
            .iter()
            .filter(|event| matches!(event.kind, SessionEventKind::SubmissionAccepted { .. }))
            .count();
        let activated = events
            .iter()
            .filter(|event| matches!(event.kind, SessionEventKind::SubmissionActivated { .. }))
            .count();
        eprintln!(
            "BROWSER_FAILURE_CANONICAL session={} events={} accepted={accepted} activated={activated}",
            session.id,
            events.len()
        );
    }
    require(
        output.status.success(),
        &format!(
            "browser acceptance failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )?;
    let report: Value = serde_json::from_slice(&output.stdout)?;
    require(report["passed"] == true, "browser did not pass")?;
    require(
        report["negative_controls"] == 7 && report["disposed_requests"] == 0,
        "browser omitted stale-control or disposal assertions",
    )?;
    let checkpoints = report["checkpoints"]
        .as_array()
        .ok_or("browser omitted committed Canvas checkpoints")?;
    require(checkpoints.len() == 4, "incomplete Canvas checkpoints")?;
    for checkpoint in checkpoints {
        require(
            checkpoint["ink"].as_u64().is_some_and(|ink| ink > 0)
                && checkpoint["pixel_sha256"]
                    .as_str()
                    .is_some_and(|digest| digest.len() == 64),
            "invalid committed Canvas checkpoint",
        )?;
    }
    let receipt: SubmissionReceipt = serde_json::from_value(report["receipt"].clone())?;
    let text = report["text"]
        .as_str()
        .ok_or("browser omitted submitted text")?
        .to_owned();
    let canonical_events = history(client, session.id).await?;
    canonical(&canonical_events, &receipt, &text)?;
    require(
        !canonical_events
            .iter()
            .any(|event| matches!(event.kind, SessionEventKind::PermissionResolved { .. })),
        "rejected browser controls must not resolve canonical permissions",
    )?;
    scenarios::canonical(client, root, &scenarios, &report["scenarios"]).await?;
    // Read back through a fresh client, not retained HTTP/browser state.
    println!(
        "BROWSER_E2E {}",
        json!({"browser": report, "session_id": session.id})
    );
    Ok((session.id, receipt, text, report["operation"].clone()))
}

async fn retry_after_restart(
    client: &BcodeClient,
    root: &Path,
    session: SessionId,
    receipt: &SubmissionReceipt,
    text: &str,
    operation: &Value,
) -> TestResult<()> {
    ready(client).await?;
    let router = router_from_state(HyperChadAppState::new_in_working_directory(
        client.clone(),
        "restart-test-token",
        root.to_path_buf(),
    ));
    let mut retry = RouteRequest::from_path(
        "/api/session-view/submit-message?token=restart-test-token",
        RequestInfo::default(),
    );
    retry.method = "POST".parse()?;
    retry
        .headers
        .insert("content-type".into(), "application/json".into());
    retry.body = Some(std::sync::Arc::new(serde_json::to_vec(operation)?.into()));
    let response = router
        .navigate(retry)
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))?
        .ok_or("restart retry returned no content")?;
    let Content::Raw { data, .. } = response else {
        return Err("restart retry returned non-JSON".into());
    };
    let response: Value = serde_json::from_slice(&data)?;
    let retried: SubmissionReceipt =
        serde_json::from_value(response["outcome"]["receipt"].clone())?;
    require(
        &retried == receipt,
        "restart must retain exact canonical receipt",
    )?;
    canonical(&history(client, session).await?, receipt, text)
}

fn explicit_path(name: &str) -> TestResult<PathBuf> {
    let path =
        PathBuf::from(std::env::var_os(name).ok_or_else(|| format!("{name} must be explicit"))?);
    require(
        path.is_absolute() && path.exists(),
        "test tool/bundle path must be absolute and exist",
    )?;
    Ok(path)
}

#[cfg(feature = "renderer-html-actix")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit BCODE_E2E_CHROMIUM, BCODE_E2E_BUNDLE and BCODE_E2E_PYTHON; no builds or user runtime"]
async fn real_wasm_registered_router_durable_retry_and_restart() -> TestResult<()> {
    require(
        cfg!(feature = "static-bundled-question-plugin"),
        "real production acceptance requires static-bundled-question-plugin controller registration",
    )?;
    let chromium = explicit_path("BCODE_E2E_CHROMIUM")?;
    let bundle = explicit_path("BCODE_E2E_BUNDLE")?;
    let python = explicit_path("BCODE_E2E_PYTHON")?;
    let executable = std::env::current_exe()?;
    println!(
        "TEST_EXECUTABLE {} sha256:{}",
        executable.display(),
        Sha256::digest(std::fs::read(&executable)?).iter().fold(
            String::new(),
            |mut output, byte| {
                write!(&mut output, "{byte:02x}").expect("string formatting");
                output
            }
        )
    );
    let directory = tempfile::tempdir()?;
    let state_root = directory.path().join("state");
    std::fs::create_dir(&state_root)?;
    let locations = bcode_config::resolve_state_location_set_with_environment(
        &bcode_config::StateConfig::default(),
        &bcode_config::StateLocationSelection {
            root: Some(state_root),
            profile: None,
        },
        &bcode_config::ConfigEnvironmentSnapshot::new(
            BTreeMap::new(),
            directory.path().to_path_buf(),
        ),
    )?;
    let ipc_directory = directory.path().join("ipc");
    std::fs::create_dir(&ipc_directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&ipc_directory, std::fs::Permissions::from_mode(0o700))?;
    }
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(ipc_directory.join("test.sock"));
    let client = BcodeClient::for_state_location(endpoint.clone(), locations.primary());
    let test_config = config();
    let selection = test_config.resolved_model_selection();
    require(
        selection.provider_plugin_id.as_deref() == Some("bcode.fake-provider")
            && selection.model_id.as_deref() == Some("fake-echo"),
        "ambient model selection conflicts with isolated fake provider; unset model/provider overrides",
    )?;
    let mut backend = backend::spawn(directory.path())?;
    ready(&client).await?;
    let work = async {
        let result = exercise(&client, directory.path(), chromium, bundle, python).await;
        std::fs::write(
            directory.path().join("browser-exercise-finished"),
            b"finished",
        )?;
        result
    };
    let restart = backend::restart_on_request(directory.path(), &client, &mut backend);
    let (result, restart_result) = tokio::join!(work, restart);
    let (session, receipt, text, operation) = result?;
    restart_result?;
    backend.0.kill()?;
    backend.0.wait()?;
    backend = backend::spawn(directory.path())?;
    ready(&client).await?;
    let result = retry_after_restart(
        &client,
        directory.path(),
        session,
        &receipt,
        &text,
        &operation,
    )
    .await;
    drop(backend);
    result
}
