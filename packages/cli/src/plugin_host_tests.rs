//! Local CLI plugin hosts grant explicit activation exactly as the CLI entry points do.
use super::*;

const HELLO_MANIFEST: &str = include_str!("../../../examples/hello-plugin/bcode-plugin.toml");

fn hello() -> bcode_plugin::StaticBundledPlugin {
    bcode_plugin::StaticBundledPlugin::from_definition(
        HELLO_MANIFEST,
        bcode_hello_plugin::plugin_definition,
    )
}

fn hello_config() -> bcode_config::BcodeConfig {
    let mut config = bcode_config::BcodeConfig::default();
    config.plugins.enabled.insert("example.hello".to_owned());
    config
}

/// Stage the packaged hello backend wrapper as an installed plugin root:
/// the verbatim declaration plus the artifact under its declared library name.
fn hello_wrapper_root() -> tempfile::TempDir {
    let artifact = PathBuf::from(
        std::env::var_os("BCODE_HELLO_BACKEND_WRAPPER_ARTIFACT").expect(
            "build the hello backend wrapper with package-plugin.py --component backend --feature tui; set BCODE_HELLO_BACKEND_WRAPPER_ARTIFACT to its absolute path",
        ),
    );
    assert!(artifact.is_absolute() && artifact.is_file());
    let manifest: bcode_plugin::PluginManifest = toml::from_str(HELLO_MANIFEST).unwrap();
    let bcode_plugin::PluginRuntime::Native(runtime) = &manifest.runtime;
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("hello");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("bcode-plugin.toml"), HELLO_MANIFEST).unwrap();
    std::fs::copy(&artifact, directory.join(&runtime.library)).unwrap();
    root
}

fn discovered_hello(root: &tempfile::TempDir) -> Vec<bcode_plugin::RegisteredPlugin> {
    discover_plugins_for_cli(&[root.path().to_path_buf()]).unwrap()
}

fn assert_echo(mut host: bcode_plugin::PluginHost) {
    let response = host
        .invoke_service_by_interface("example-hello/v1", "echo", b"activated".to_vec())
        .expect("selected plugin serves through its activation");
    assert!(response.error.is_none());
    assert_eq!(response.payload, b"activated");
    host.deactivate_all().unwrap();
}

#[test]
fn provider_and_auth_command_host_activates_bundled_plugins() {
    assert_echo(cli_plugin_host(&hello_config(), Vec::new(), &[hello()]).unwrap());
}

#[test]
fn provider_and_auth_command_host_activates_discovered_plugins() {
    let root = hello_wrapper_root();
    assert_echo(cli_plugin_host(&hello_config(), discovered_hello(&root), &[]).unwrap());
}

#[test]
fn plugin_service_commands_activate_bundled_plugins() {
    assert_echo(plugin_service_host(&hello_config(), Vec::new(), &[hello()]).unwrap());
}

#[test]
fn plugin_service_commands_activate_discovered_plugins() {
    let root = hello_wrapper_root();
    assert_echo(plugin_service_host(&hello_config(), discovered_hello(&root), &[]).unwrap());
}

#[test]
fn plugin_check_activates_each_selected_discovered_plugin() {
    let root = hello_wrapper_root();
    let mut checked = Vec::new();
    let count = check_selected_plugins(
        &plugin_selection_for_config(&hello_config()),
        discovered_hello(&root),
        |plugin_id| {
            checked.push(plugin_id.to_owned());
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(count, 1);
    assert_eq!(checked, ["example.hello"]);
}

#[test]
fn local_plugin_publish_delivers_to_activated_discovered_plugins() {
    let root = hello_wrapper_root();
    let delivered = publish_local_plugin_event(
        &hello_config(),
        discovered_hello(&root),
        "example.event",
        b"hello event",
    )
    .unwrap();
    assert_eq!(delivered, 1);
}
