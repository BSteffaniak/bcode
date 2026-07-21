use bcode::Bcode;
use bcode_plugin::{PluginRuntimeHost, PluginSelection};

#[tokio::main(flavor = "current_thread")]
async fn main() -> bcode::Result<()> {
    let selection = PluginSelection::all_enabled();
    let static_plugins = bcode_bundled_plugins::static_bundled_plugins();
    let plugins =
        PluginRuntimeHost::load_defaults_with_static_bundled(&selection, &static_plugins)?;

    let bcode = Bcode::builder()
        .plugin_runtime(plugins)
        .provider("bcode.fake-provider")
        .default_model("bcode.fake-provider:fake-echo")
        .build();
    println!(
        "default model: {:?}",
        bcode
            .default_model_selector()
            .map(|selector| selector.model_id())
    );
    let capabilities = bcode.provider_capabilities("bcode.fake-provider").await?;
    let models = bcode.provider_models("bcode.fake-provider").await?;
    println!(
        "provider={} models={}",
        capabilities.provider_id,
        models.models.len()
    );

    let discovered = bcode.discover_tools().await?;
    println!("discovered plugin tools: {}", discovered.len());
    let agent = bcode
        .agent_with_discovered_tools()
        .await?
        .name("embedded-fake-provider")
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
