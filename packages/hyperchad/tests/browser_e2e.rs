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
    let state = HyperChadAppState::new_in_working_directory(
        client,
        fixture["token"].as_str().ok_or("token")?,
        root,
    );
    let address_file = PathBuf::from(fixture["address_file"].as_str().ok_or("address_file")?);
    let builder = bcode_hyperchad::init_with_snapshot(
        bcode_session_view_models::SessionViewSnapshot::empty(),
        Vec::new(),
    )
    .with_router(router_from_state(state))
    .with_actix_bind_address("127.0.0.1".to_owned())
    .with_actix_port(0)
    .with_actix_on_bound(move |address| {
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
        route: "fixture.json".into(),
        target: AssetPathTarget::FileContents(
            serde_json::to_vec(&json!({"session": fixture["session"], "token": fixture["token"]}))?
                .into(),
        ),
        not_found_behavior: None,
    });
    let assets =
        bcode_hyperchad::TuiAssets::load(Path::new(fixture["bundle"].as_str().ok_or("bundle")?))?;
    let (app, _runtime) = bcode_hyperchad::build_app_with_runtime(assets.with_routes(builder))
        .map_err(|error| error.to_string())?;
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

fn plugins() -> TestResult<bcode_plugin::PluginRuntimeHost> {
    Ok(
        bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
            &bcode_plugin::PluginSelection {
                mode: bcode_plugin::PluginSelectionMode::Explicit,
                enabled: BTreeSet::from(["bcode.fake-provider".into()]),
                disabled: BTreeSet::new(),
            },
            &[bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../plugins/fake-provider-plugin/bcode-plugin.toml"),
                bcode_fake_provider_plugin::static_plugin(),
            )],
        )?,
    )
}

fn config() -> bcode_config::BcodeConfig {
    let mut config = bcode_config::BcodeConfig::default();
    config.model.provider_plugin_id = Some("bcode.fake-provider".into());
    config.model.model_id = Some("fake-echo".into());
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
            .arg("--token")
            .arg(token)
            .output()
    })
    .await;
    drop(http);
    let output = output??;
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
    let receipt: SubmissionReceipt = serde_json::from_value(report["receipt"].clone())?;
    let text = report["text"]
        .as_str()
        .ok_or("browser omitted submitted text")?
        .to_owned();
    canonical(&history(client, session.id).await?, &receipt, &text)?;
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
    let shutdown = bcode_agent_runtime::CancellationToken::new();
    let server = bcode_server::run_embedded_with_locations_and_shutdown(
        endpoint.clone(),
        test_config,
        plugins()?,
        bcode_model_catalog::ModelCatalogResolver::embedded(),
        vec!["bcode.fake-provider".into()],
        shutdown.clone(),
        Some(locations.clone()),
    );
    let work = async {
        let result = exercise(&client, directory.path(), chromium, bundle, python).await;
        shutdown.cancel();
        result
    };
    let (server_result, result) = tokio::join!(server, work);
    server_result?;
    let (session, receipt, text, operation) = result?;
    let shutdown = bcode_agent_runtime::CancellationToken::new();
    let server = bcode_server::run_embedded_with_locations_and_shutdown(
        endpoint,
        config(),
        plugins()?,
        bcode_model_catalog::ModelCatalogResolver::embedded(),
        vec!["bcode.fake-provider".into()],
        shutdown.clone(),
        Some(locations),
    );
    let verify = async {
        let result = retry_after_restart(
            &client,
            directory.path(),
            session,
            &receipt,
            &text,
            &operation,
        )
        .await;
        shutdown.cancel();
        result
    };
    let (server_result, result) = tokio::join!(server, verify);
    server_result?;
    result
}
