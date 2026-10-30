//! Test-owned exact-artifact backend process lifecycle, never a user daemon.
use super::{
    ActixChild, BTreeMap, BcodeClient, Path, PathBuf, TestResult, config, plugin_ids, plugins,
    ready, require,
};
use std::time::Duration;

pub fn spawn(root: &Path) -> TestResult<ActixChild> {
    Ok(ActixChild(
        std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "production_backend_child",
                "--ignored",
                "--nocapture",
            ])
            .env("BCODE_BACKEND_FIXTURE_ROOT", root)
            .stdin(std::process::Stdio::null())
            .spawn()?,
    ))
}

pub async fn serve(root: PathBuf) -> TestResult<()> {
    let locations = bcode_config::resolve_state_location_set_with_environment(
        &bcode_config::StateConfig::default(),
        &bcode_config::StateLocationSelection {
            root: Some(root.join("state")),
            profile: None,
        },
        &bcode_config::ConfigEnvironmentSnapshot::new(BTreeMap::new(), root.clone()),
    )?;
    let config = config();
    let selection = config.resolved_model_selection();
    require(
        selection.provider_plugin_id.as_deref() == Some("bcode.fake-provider")
            && selection.model_id.as_deref() == Some("fake-echo"),
        "fixture selected a non-deterministic provider",
    )?;
    let releases = async {
        let request = root.join("release-cancel-gate");
        while !request.exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        bcode_fake_provider_plugin::release_fake_event_script_gate("browser-cancel-release");
        std::fs::write(root.join("cancel-gate-released"), b"released")
    };
    let server = bcode_server::run_embedded_with_locations_and_shutdown(
        bcode_ipc::IpcEndpoint::unix_socket(root.join("ipc/test.sock")),
        config,
        plugins()?,
        bcode_model_catalog::ModelCatalogResolver::embedded(),
        plugin_ids(),
        bcode_agent_runtime::CancellationToken::new(),
        Some(locations),
    );
    let (server_result, release_result) = tokio::join!(server, releases);
    release_result?;
    server_result?;
    Ok(())
}

pub async fn restart_on_request(
    root: &Path,
    client: &BcodeClient,
    backend: &mut ActixChild,
) -> TestResult<()> {
    let request = root.join("backend-restart-request");
    while !request.exists() {
        if root.join("browser-exercise-finished").exists() {
            return Err("browser exited before required nonterminal backend restart".into());
        }
        if let Some(status) = backend.0.try_wait()? {
            return Err(format!("isolated backend exited before restart: {status}").into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // Abrupt loss is intentional: graceful shutdown would test cancellation,
    // not the disposable-daemon invariant. This handle owns only our child.
    backend.0.kill()?;
    backend.0.wait()?;
    *backend = spawn(root)?;
    ready(client).await?;
    std::fs::write(
        root.join("backend-restart-complete"),
        b"same-artifact-test-backend-ready",
    )?;
    Ok(())
}
