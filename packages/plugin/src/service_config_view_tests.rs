//! Behavioral coverage for immutable service configurations over shared executors.
use super::*;

fn config(label: &str) -> ResolvedPluginConfig {
    let label = label.to_owned();
    ResolvedPluginConfig::new(serde_json::json!(label), serde_json::json!("redacted"))
        .with_state_root(PathBuf::from(format!("/test/{label}")))
        .with_secret_resolver(Arc::new(move |_, _, value| {
            assert_eq!(value, &serde_json::json!(label));
            Ok(BTreeMap::from([("secret".to_owned(), label.clone())]))
        }))
}

fn lifecycle() -> i32 {
    0
}

fn service(invocation: &mut test_backend::TestInvocation<'_>) -> ServiceResponse {
    ServiceResponse::json(&invocation.context.config).unwrap()
}

fn runtime(concurrency: PluginConcurrencyConfig) -> PluginRuntimeHost {
    let mut manifest = tests::test_manifest("view-test");
    manifest.concurrency = concurrency;
    let plugin = LoadedPlugin {
        config: config("default"),
        manifest,
        backend: LoadedPluginBackend::Test(test_backend::TestBackend {
            activate: lifecycle,
            deactivate: lifecycle,
            register_commands: None,
            register_auth_providers: None,
            invoke: service,
            handle_event: |_| 0,
        }),
    };
    let mut host = PluginHost::default();
    host.activate_plugin(plugin).unwrap();
    PluginRuntimeHost::from(host)
}

fn view(runtime: &PluginRuntimeHost, label: &str) -> PluginServiceConfigView {
    runtime.service_config_view(BTreeMap::from([("view-test".to_owned(), config(label))]))
}

async fn invoke(view: &PluginServiceConfigView) -> ServiceResponse {
    view.invoke_service_with_bridge_scoped(
        "view-test",
        "view-test",
        "inspect",
        Vec::new(),
        PluginInvocationScope::Global,
        None,
    )
    .await
    .unwrap()
}

fn assert_config(response: &ServiceResponse, label: &str) {
    let context: PluginConfigContext = serde_json::from_slice(&response.payload).unwrap();
    assert_eq!(context.config, serde_json::json!(label));
    assert_eq!(context.redacted_config, serde_json::json!("redacted"));
    assert_eq!(
        context.secrets.get("secret").map(String::as_str),
        Some(label)
    );
    assert_eq!(
        context.state_root,
        Some(PathBuf::from(format!("/test/{label}")))
    );
}

#[tokio::test]
async fn views_isolate_resolvers_and_state_roots_without_changing_default() {
    for concurrency in [
        PluginConcurrencyConfig::Exclusive,
        PluginConcurrencyConfig::Concurrent,
    ] {
        let runtime = runtime(concurrency);
        let first = view(&runtime, "first");
        let second = view(&runtime, "second");
        assert!(first.same_config_view(&first.clone()));
        assert!(!first.same_config_view(&second));
        assert_eq!(first.runtime_generation(), runtime.runtime_generation());
        let (a, b) = tokio::join!(invoke(&first), invoke(&second));
        assert_config(&a, "first");
        assert_config(&b, "second");
        let default = runtime
            .invoke_service_scoped(
                "view-test",
                "view-test",
                "inspect",
                Vec::new(),
                PluginInvocationScope::Global,
            )
            .await
            .unwrap();
        assert_config(&default, "default");
        assert_config(&invoke(&first).await, "first");
        runtime.deactivate_all().await.unwrap();
        drop(runtime);
        assert!(
            first
                .invoke_service_with_bridge_scoped(
                    "view-test",
                    "view-test",
                    "inspect",
                    Vec::new(),
                    PluginInvocationScope::Global,
                    None,
                )
                .await
                .is_err()
        );
        drop(first);
    }
}

#[tokio::test]
async fn queued_view_keeps_its_snapshot_when_another_view_is_created() {
    let runtime = runtime(PluginConcurrencyConfig::Exclusive);
    let (entered, mut started) = tokio::sync::mpsc::unbounded_channel();
    let (release, wait) = std::sync::mpsc::channel();
    let wait = std::sync::Mutex::new(wait);
    let blocker = config("blocker").with_secret_resolver(Arc::new(move |_, _, _| {
        entered.send(()).unwrap();
        wait.lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        Ok(BTreeMap::new())
    }));
    let blocking = runtime.service_config_view(BTreeMap::from([("view-test".to_owned(), blocker)]));
    let first = tokio::spawn(async move { invoke(&blocking).await });
    tokio::time::timeout(Duration::from_secs(2), started.recv())
        .await
        .unwrap()
        .unwrap();
    let queued = view(&runtime, "queued");
    let second = tokio::spawn(async move { invoke(&queued).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !runtime
            .executor_statuses()
            .iter()
            .any(|status| status.queued > 0)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let newest = view(&runtime, "newest");
    release.send(()).unwrap();
    first.await.unwrap();
    assert_config(&second.await.unwrap(), "queued");
    assert_config(&invoke(&newest).await, "newest");
    runtime.deactivate_all().await.unwrap();
    drop(runtime);
}

#[tokio::test]
async fn invocation_credentials_do_not_become_view_configuration_or_leak_to_next_call() {
    let runtime = runtime(PluginConcurrencyConfig::Exclusive);
    let configured = view(&runtime, "configured");
    let bridge = PluginInvocationBridge::new(|_, _| Err("unused".to_owned())).with_credentials(
        Arc::new(|_, _| panic!("service must not request credentials")),
    );
    let response = configured
        .invoke_service_with_bridge_scoped(
            "view-test",
            "view-test",
            "inspect",
            Vec::new(),
            PluginInvocationScope::Global,
            Some(bridge),
        )
        .await
        .unwrap();
    let context: PluginConfigContext = serde_json::from_slice(&response.payload).unwrap();
    assert!(context.secrets.is_empty());
    assert_eq!(context.config, serde_json::json!("configured"));
    assert_config(&invoke(&configured).await, "configured");
    drop(configured);
    runtime.deactivate_all().await.unwrap();
    drop(runtime);
}

#[tokio::test]
async fn absent_configuration_never_falls_back_and_async_bridge_remains_rejected() {
    let runtime = runtime(PluginConcurrencyConfig::Exclusive);
    let empty = runtime.service_config_view(BTreeMap::new());
    assert!(
        empty
            .invoke_service_with_bridge_scoped(
                "view-test",
                "view-test",
                "inspect",
                Vec::new(),
                PluginInvocationScope::Global,
                None,
            )
            .await
            .is_err()
    );
    drop(empty);
    let configured = view(&runtime, "configured");
    let bridge = PluginInvocationBridge::new_async(|_, _| async { panic!("must not execute") });
    assert!(matches!(
        configured
            .invoke_service_with_bridge_scoped(
                "view-test",
                "view-test",
                "inspect",
                Vec::new(),
                PluginInvocationScope::Global,
                Some(bridge),
            )
            .await,
        Err(PluginLoadError::UnsupportedAsyncBridge { .. })
    ));
    drop(configured);
    runtime.deactivate_all().await.unwrap();
    drop(runtime);
}
