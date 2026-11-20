//! A real in-process daemon with the fake provider, and a bridge serving it, shared by the
//! bridge's end-to-end tests.

use std::time::Duration;

use bcode_client::BcodeClient;
use bcode_session_bridge::models::{BridgeEvent, ToolInvocation, VersionRange};
use bcode_session_bridge::{BridgeClient, BridgeEvents};
use bcode_session_bridge_server::{BridgeOptions, serve};

pub const WAIT: Duration = Duration::from_secs(30);

pub struct Daemon {
    pub directory: tempfile::TempDir,
    pub client: BcodeClient,
    shutdown: bcode_agent_runtime::CancellationToken,
    server: tokio::task::JoinHandle<()>,
}

impl Daemon {
    pub async fn stop(self) {
        self.shutdown.cancel();
        let _ = tokio::time::timeout(WAIT, self.server).await;
    }

    pub fn workspace(&self) -> String {
        self.directory.path().display().to_string()
    }
}

fn config() -> bcode_config::BcodeConfig {
    let mut config = bcode_config::BcodeConfig::default();
    config.model.provider_plugin_id = Some("bcode.fake-provider".into());
    config.model.model_id = Some("fake-echo".into());
    config.skills.enabled = false;
    // The fake provider rejects prompt-cache hints.
    config.model.prompt_cache.mode =
        serde_json::from_value(serde_json::json!("off")).expect("prompt cache mode");
    config
}

fn plugin_ids() -> Vec<String> {
    vec![
        "bcode.fake-provider".into(),
        "bcode.sdk-session-import".into(),
    ]
}

/// Start a daemon with the fake provider, SDK session import, and `extra` plugins.
pub async fn start_daemon(extra: Vec<(bcode_plugin::StaticBundledPlugin, &str)>) -> Daemon {
    let directory = tempfile::tempdir().expect("directory");
    let locations = bcode_config::resolve_state_location_set_with_environment(
        &bcode_config::StateConfig::default(),
        &bcode_config::StateLocationSelection {
            root: Some(directory.path().join("state")),
            profile: None,
        },
        &bcode_config::ConfigEnvironmentSnapshot::new(
            std::collections::BTreeMap::new(),
            directory.path().to_path_buf(),
        ),
    )
    .expect("locations");
    // The daemon only binds sockets inside a private directory.
    let ipc_directory = directory.path().join("ipc");
    std::fs::create_dir(&ipc_directory).expect("ipc directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&ipc_directory, std::fs::Permissions::from_mode(0o700))
            .expect("private ipc directory");
    }
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(ipc_directory.join("bridge.sock"));
    let config = config();
    let mut ids = plugin_ids();
    ids.extend(extra.iter().map(|(_, id)| (*id).to_owned()));
    let selection = bcode_config::plugin_selection_with_default_plugin_ids(&config, ids.clone());
    let context = bcode_ipc::ClientRuntimeContext {
        working_directory: Some(directory.path().to_path_buf()),
        effective_config_toml: Some(Box::new(
            bcode_config::encode_effective_config(&config).expect("config"),
        )),
        selected_provider_plugin_id: Some("bcode.fake-provider".into()),
        selected_model_id: Some("fake-echo".into()),
        ..bcode_ipc::ClientRuntimeContext::default()
    };
    let client = BcodeClient::for_state_location(endpoint.clone(), locations.primary())
        .with_runtime_context(Some(context));
    let mut static_plugins = vec![
        bcode_plugin::StaticBundledPlugin::from_definition(
            include_str!("../../../../../plugins/fake-provider-plugin/bcode-plugin.toml"),
            bcode_fake_provider_plugin::plugin_definition,
        ),
        bcode_plugin::StaticBundledPlugin::from_definition(
            include_str!("../../../../../plugins/sdk-session-import-plugin/bcode-plugin.toml"),
            bcode_sdk_session_import_plugin::plugin_definition,
        ),
    ];
    static_plugins.extend(extra.into_iter().map(|(plugin, _)| plugin));
    let plugins =
        bcode_server::load_embedded_plugins(&selection, &static_plugins).expect("plugins");
    let shutdown = bcode_agent_runtime::CancellationToken::new();
    let server = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            let result = bcode_server::run_embedded_with_locations_and_shutdown(
                endpoint,
                config,
                plugins,
                bcode_model_catalog::ModelCatalogResolver::embedded(),
                ids,
                shutdown,
                Some(locations),
            )
            .await;
            if let Err(error) = result {
                eprintln!("embedded daemon failed: {error}");
            }
        }
    });
    tokio::time::timeout(WAIT, async {
        while client.ping().await.is_err() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("daemon answers");
    Daemon {
        directory,
        client,
        shutdown,
        server,
    }
}

pub fn options(versions: VersionRange) -> BridgeOptions {
    BridgeOptions {
        versions,
        bcode_version: "test".into(),
        executable: "/opt/bcode/bin/bcode".into(),
    }
}

/// Serve a bridge for `daemon` that accepts `bridge_versions`, and connect an application that
/// speaks `app_versions`.
pub async fn connect(
    daemon: &Daemon,
    bridge_versions: VersionRange,
    app_versions: VersionRange,
) -> Result<(BridgeClient, BridgeEvents), bcode_session_bridge::BridgeClientError> {
    let (app_side, bridge_side) = tokio::io::duplex(1024 * 1024);
    let (bridge_read, bridge_write) = tokio::io::split(bridge_side);
    tokio::spawn(serve(
        bridge_read,
        bridge_write,
        daemon.client.clone(),
        options(bridge_versions),
    ));
    let (app_read, app_write) = tokio::io::split(app_side);
    BridgeClient::connect(app_read, app_write, "test-app", app_versions).await
}

pub async fn bridge(daemon: &Daemon) -> (BridgeClient, BridgeEvents) {
    connect(daemon, VersionRange::supported(), VersionRange::supported())
        .await
        .expect("bridge")
}

/// The next event matching `select`, skipping others.
pub async fn next_event<T>(
    events: &mut BridgeEvents,
    mut select: impl FnMut(BridgeEvent) -> Option<T>,
) -> T {
    tokio::time::timeout(WAIT, async {
        loop {
            let event = events.recv().await.expect("bridge open");
            if let Some(selected) = select(event) {
                return selected;
            }
        }
    })
    .await
    .expect("event arrives")
}

pub async fn invocation(events: &mut BridgeEvents) -> ToolInvocation {
    next_event(events, |event| match event {
        BridgeEvent::ToolInvoke(invocation) => Some(invocation),
        _ => None,
    })
    .await
}
