//! Behavioral coverage for immutable service configurations over shared executors.
use super::*;
use bcode_plugin_sdk::{SERVICE_STATUS_BUFFER_TOO_SMALL, SERVICE_STATUS_OK, StaticPluginVtable};
use std::ffi::c_void;
use std::sync::OnceLock;

fn config(label: &str) -> ResolvedPluginConfig {
    let label = label.to_owned();
    ResolvedPluginConfig::new(serde_json::json!(label), serde_json::json!("redacted"))
        .with_state_root(PathBuf::from(format!("/test/{label}")))
        .with_secret_resolver(Arc::new(move |_, _, value| {
            assert_eq!(value, &serde_json::json!(label));
            Ok(BTreeMap::from([("secret".to_owned(), label.clone())]))
        }))
}

fn lifecycle(_: *const c_void) -> i32 {
    SERVICE_STATUS_OK
}

#[allow(clippy::too_many_arguments)]
fn service(
    _: *const c_void,
    input: *const u8,
    input_len: usize,
    output: *mut u8,
    capacity: usize,
    length: *mut usize,
    _: Option<ServiceEventCallback>,
    _: *mut c_void,
    _: Option<ServiceBridgeCallback>,
    _: *mut c_void,
    _: Option<ServiceCancellationWaitCallback>,
    _: *mut c_void,
) -> i32 {
    let input = unsafe { std::slice::from_raw_parts(input, input_len) };
    let context: NativeServiceContext = serde_json::from_slice(input).unwrap();
    let response = ServiceResponse::json(&context.config).unwrap();
    let encoded = serde_json::to_vec(&response).unwrap();
    unsafe { *length = encoded.len() };
    if encoded.len() > capacity {
        return SERVICE_STATUS_BUFFER_TOO_SMALL;
    }
    unsafe { std::ptr::copy_nonoverlapping(encoded.as_ptr(), output, encoded.len()) };
    SERVICE_STATUS_OK
}

fn runtime(concurrency: PluginConcurrencyConfig) -> PluginRuntimeHost {
    let mut manifest = tests::test_manifest("view-test");
    manifest.concurrency = concurrency;
    let plugin = LoadedPlugin {
        config: config("default"),
        manifest,
        backend: LoadedPluginBackend::Static {
            vtable: StaticPluginVtable {
                instance: std::ptr::null(),
                manifest: |_: &'static OnceLock<Option<std::ffi::CString>>| std::ptr::null(),
                activate: lifecycle,
                deactivate: lifecycle,
                register_commands: None,
                register_auth_providers: None,
                invoke_service_streaming: service,
                cli_registration: None,
                handle_event: |_, _, _| SERVICE_STATUS_OK,
            },
        },
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
