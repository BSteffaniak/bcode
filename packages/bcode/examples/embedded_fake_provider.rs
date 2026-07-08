use bcode::Bcode;
use bcode_plugin::{PluginRuntimeHost, PluginSelection};

#[tokio::main(flavor = "current_thread")]
async fn main() -> bcode::Result<()> {
    let selection = PluginSelection::all_enabled();
    let static_plugins = bcode_bundled_plugins::static_bundled_plugins();
    let plugins =
        PluginRuntimeHost::load_defaults_with_static_bundled(&selection, &static_plugins)?;

    let bcode = Bcode::builder().plugin_runtime(plugins).build();
    let agent = bcode
        .agent()
        .name("embedded-fake-provider")
        .provider_plugin("bcode.fake-provider")
        .model("fake-echo")
        .build();

    let response = agent.generate_text("hello embedded plugins").await?;
    println!("{}", response.text);
    println!(
        "stop={:?} latency={}ms events={}",
        response.runtime.stop_reason,
        response.runtime.latency_ms,
        response.runtime.events.len()
    );

    Ok(())
}
