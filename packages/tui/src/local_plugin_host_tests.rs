//! Every local TUI plugin host grants explicit activation, exactly as its launcher,
//! setup flow or presentation loader builds it in production.
use super::*;

const HELLO_MANIFEST: &str = include_str!("../../../examples/hello-plugin/bcode-plugin.toml");

fn hello() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::from_definition(
        HELLO_MANIFEST,
        bcode_hello_plugin::plugin_definition,
    )
}

fn hello_selection() -> bcode_plugin::PluginSelection {
    bcode_plugin::PluginSelection {
        mode: bcode_plugin::PluginSelectionMode::Explicit,
        enabled: std::collections::BTreeSet::from(["example.hello".to_owned()]),
        disabled: std::collections::BTreeSet::new(),
    }
}

fn assert_host_echo(mut host: bcode_plugin::PluginHost) {
    let response = host
        .invoke_service(
            "example.hello",
            "example-hello/v1",
            "echo",
            b"activated".to_vec(),
        )
        .expect("selected plugin serves through its activation");
    assert!(response.error.is_none());
    assert_eq!(response.payload, b"activated");
    host.deactivate_all().unwrap();
}

async fn assert_runtime_echo(runtime: bcode_plugin::PluginRuntimeHost) {
    let response = runtime
        .invoke_service(
            "example.hello",
            "example-hello/v1",
            "echo",
            b"activated".to_vec(),
        )
        .await
        .expect("selected plugin serves through its activation");
    assert!(response.error.is_none());
    assert_eq!(response.payload, b"activated");
}

#[tokio::test]
async fn eval_metrics_ralph_and_surface_launchers_activate_bundled_plugins() {
    assert_runtime_echo(
        tokio::task::spawn_blocking(|| load_launcher_runtime(&[hello()]))
            .await
            .unwrap()
            .expect("launcher runtime loads with explicit activation"),
    )
    .await;
}

#[test]
fn setup_flows_activate_selected_bundled_plugins() {
    assert_host_echo(load_local_plugin_host(&hello_selection(), &[hello()]).unwrap());
}

#[test]
fn plugin_tui_default_host_activates_bundled_plugins() {
    assert_host_echo(
        plugin_tui::load_default_host_with_static_bundled(&hello_selection(), &[hello()]).unwrap(),
    );
}

#[tokio::test]
async fn plugin_tui_default_runtime_activates_bundled_plugins() {
    assert_runtime_echo(
        tokio::task::spawn_blocking(|| {
            plugin_tui::load_default_runtime_with_static_bundled(&[hello()])
        })
        .await
        .unwrap()
        .expect("client runtime loads with explicit activation"),
    )
    .await;
}
