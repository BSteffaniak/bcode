//! Retained exact-invocation product authority; no ambient or polling-thread binding.
use bcode_plugin_sdk::definition::ProductInvocationSettings;
/// Exact selected owner identity; IDs validate binding, never confer authority.
#[derive(Clone)]
pub struct Binding {
    pub runtime: bmux_plugin_runtime::PluginRuntimeScopeId,
    pub instance: bmux_plugin_runtime::PluginInstanceId,
    pub activation: bmux_plugin_runtime::PluginActivationId,
    pub invocation: bmux_plugin_runtime::RuntimeInvocationId,
    pub context: bcode_plugin_sdk::NativeServiceContext,
}

/// Per-operation retained product authority. The shared loader must install this
/// only on its newly allocated invocation; never bind it instance-wide.
pub struct InvocationDispatch {
    binding: Binding,
}
impl InvocationDispatch {
    #[must_use]
    pub const fn new(binding: Binding) -> Self {
        Self { binding }
    }
}
impl bmux_plugin_sdk::HostServiceDispatch for InvocationDispatch {
    fn invoke(
        &self,
        invocation: &bmux_plugin_runtime::InvocationScope,
        request: bmux_plugin_sdk::HostServiceRequest<'_>,
    ) -> bmux_plugin_sdk::Result<Vec<u8>> {
        dispatch_bound(&self.binding, invocation, &request)
    }
}
fn dispatch_bound(
    binding: &Binding,
    invocation: &bmux_plugin_runtime::InvocationScope,
    request: &bmux_plugin_sdk::HostServiceRequest<'_>,
) -> bmux_plugin_sdk::Result<Vec<u8>> {
    if invocation.runtime_id() != binding.runtime
        || invocation.instance_id() != binding.instance
        || invocation.id() != binding.invocation
        || invocation
            .activation_id()
            .map_err(|_| protocol("invocation activation unavailable"))?
            != binding.activation
        || request.capability != "bcode.product"
        || request.payload.len() > bcode_plugin_sdk::SERVICE_BRIDGE_MAX_REQUEST_BYTES
        || request.interface != bcode_plugin_sdk::definition::PRODUCT_HOST_INTERFACE
    {
        return Err(protocol("invocation host authority mismatch"));
    }
    let _admission = invocation
        .try_admit()
        .map_err(|_| protocol("invocation host is closed"))?;
    if binding.context.cancellation.is_cancelled() {
        return Err(bmux_plugin_sdk::PluginError::InvocationCancelled);
    }
    if request.operation == "invocation_context" {
        if request.kind != bmux_plugin_sdk::ServiceKind::Query {
            return Err(protocol("invocation context requires query semantics"));
        }
        if !request.payload.is_empty() {
            return Err(protocol("unexpected invocation context payload"));
        }
        return serde_json::to_vec(&ProductInvocationSettings {
            config: binding.context.config.clone(),
            transient_progress_limits: binding.context.transient_progress_limits,
        })
        .map_err(|_| protocol("invocation context encoding failed"));
    }
    if request.kind != bmux_plugin_sdk::ServiceKind::Command {
        return Err(protocol("product bridge requires command semantics"));
    }
    let product_request: bcode_plugin_sdk::ServiceBridgeRequest =
        serde_json::from_slice(request.payload)
            .map_err(|_| protocol("invalid invocation host operation"))?;
    let expected = match &product_request {
        bcode_plugin_sdk::ServiceBridgeRequest::Exchange(_) => "exchange",
        bcode_plugin_sdk::ServiceBridgeRequest::ReceiveInput { .. } => "receive_input",
        bcode_plugin_sdk::ServiceBridgeRequest::InvokeService(_) => "invoke_service",
        bcode_plugin_sdk::ServiceBridgeRequest::WriteArtifact(_) => "write_artifact",
    };
    if request.operation != expected {
        return Err(protocol("host operation family mismatch"));
    }
    let response =
        binding
            .context
            .bridge
            .request(&product_request)
            .map_err(|failure| match failure {
                bcode_plugin_sdk::ServiceBridgeError::Host { status }
                    if status == crate::NATIVE_BRIDGE_CANCELLED_BEFORE_DISPATCH =>
                {
                    bmux_plugin_sdk::PluginError::InvocationCancelled
                }
                // Cancelled from arbitrary bridge implementations carries no phase
                // proof. A real/uncertain error after effects remains transport failure.
                _ => protocol("authorized product host operation failed"),
            })?;
    serde_json::to_vec(&response).map_err(|_| protocol("product host response encoding failed"))
}
/// Explicit extension-call services bound to one real invocation scope.
pub struct ExtensionDispatch {
    pub invocation: bmux_plugin_runtime::InvocationScope,
    pub services: bmux_plugin_sdk::definition::PluginHostServices,
}
impl ExtensionDispatch {
    fn admit(
        &self,
        invocation: &bmux_plugin_runtime::InvocationScope,
    ) -> bmux_plugin_sdk::Result<bmux_plugin_runtime::AdmissionGuard> {
        if invocation.id() != self.invocation.id()
            || invocation.instance_id() != self.invocation.instance_id()
            || invocation.runtime_id() != self.invocation.runtime_id()
        {
            return Err(protocol("selected extension authority mismatch"));
        }
        self.invocation
            .try_admit()
            .map_err(|_| protocol("selected extension invocation closed"))
    }
}
impl bmux_plugin_sdk::HostServiceDispatch for ExtensionDispatch {
    fn open_retained_host_v2(
        &self,
        invocation: &bmux_plugin_runtime::InvocationScope,
    ) -> bmux_plugin_sdk::Result<bmux_plugin_sdk::abi::retained::RetainedHostPort> {
        let _admission = self.admit(invocation)?;
        self.services.open_retained_host_v2()
    }

    fn open_retained_host(
        &self,
        invocation: &bmux_plugin_runtime::InvocationScope,
    ) -> bmux_plugin_sdk::Result<bmux_plugin_sdk::abi::retained::RetainedHostPort> {
        let _admission = self.admit(invocation)?;
        self.services.open_retained_host()
    }

    fn invoke_async(
        &self,
        invocation: &bmux_plugin_runtime::InvocationScope,
        request: bmux_plugin_sdk::HostServiceOwnedRequest,
    ) -> bmux_plugin_sdk::definition::PluginFuture<'static, bmux_plugin_sdk::Result<Vec<u8>>> {
        // Admission happens before scheduling, under the same runtime protocol
        // as closure. The async callback never borrows renderer or stack state.
        let admission = self.admit(invocation);
        let services = self.services.clone();
        Box::pin(async move {
            let _admission = admission?;
            services.call_service_async(request).await
        })
    }
    fn invoke(
        &self,
        invocation: &bmux_plugin_runtime::InvocationScope,
        request: bmux_plugin_sdk::HostServiceRequest<'_>,
    ) -> bmux_plugin_sdk::Result<Vec<u8>> {
        let _admission = self.admit(invocation)?;
        self.services.invoke(request)
    }
}

fn protocol(details: &str) -> bmux_plugin_sdk::PluginError {
    bmux_plugin_sdk::PluginError::ServiceProtocol {
        details: details.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_plugin_sdk::HostServiceDispatch;

    fn context(label: &str) -> bcode_plugin_sdk::NativeServiceContext {
        bcode_plugin_sdk::NativeServiceContext {
            diagnostics: None,
            plugin_id: "test.authority".to_string(),
            request: bcode_plugin_sdk::ServiceRequest {
                interface_id: "test-service".to_string(),
                operation: "read".to_string(),
                payload: Vec::new(),
            },
            config: bcode_plugin_sdk::PluginConfigContext {
                config: serde_json::json!({"selected": label}),
                redacted_config: serde_json::Value::Null,
                secrets: std::collections::BTreeMap::from([(
                    "private".to_string(),
                    label.to_string(),
                )]),
                state_root: None,
                durable_state_root: None,
            },
            events: bcode_plugin_sdk::ServiceEventEmitter::default(),
            cancellation: bcode_plugin_sdk::ServiceCancellation::default(),
            bridge: bcode_plugin_sdk::ServiceBridge::default(),
            transient_progress_limits: bcode_plugin_sdk::TransientProgressLimits::default(),
        }
    }
    fn request() -> bmux_plugin_sdk::HostServiceRequest<'static> {
        bmux_plugin_sdk::HostServiceRequest {
            capability: "bcode.product",
            kind: bmux_plugin_sdk::ServiceKind::Query,
            interface: bcode_plugin_sdk::definition::PRODUCT_HOST_INTERFACE,
            operation: "invocation_context",
            payload: &[],
        }
    }
    fn dispatch(
        invocation: &bmux_plugin_runtime::InvocationScope,
        context: bcode_plugin_sdk::NativeServiceContext,
    ) -> InvocationDispatch {
        InvocationDispatch::new(Binding {
            runtime: invocation.runtime_id(),
            instance: invocation.instance_id(),
            activation: invocation.activation_id().expect("activation"),
            invocation: invocation.id(),
            context,
        })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn extension_async_callbacks_retain_admission_and_never_fallback_to_sync() {
        let instance = bmux_plugin_runtime::InstanceScope::new(
            bmux_plugin_runtime::PluginRuntimeScopeId::new(),
        );
        let activation = instance
            .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
            .expect("activation");
        activation.mark_active().expect("active");
        let invocation = activation.begin_invocation().expect("invocation");
        let sync_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = sync_calls.clone();
        let dispatch = ExtensionDispatch {
            invocation: invocation.clone(),
            services: bmux_plugin_sdk::definition::PluginHostServices::new(move |_| {
                observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(Vec::new())
            }),
        };
        let owned = || bmux_plugin_sdk::HostServiceOwnedRequest {
            capability: "bcode.frontend".to_string(),
            kind: bmux_plugin_sdk::ServiceKind::Command,
            interface: "test-frontend/v1".to_string(),
            operation: "effect".to_string(),
            payload: Vec::new(),
        };
        assert!(dispatch.invoke_async(&invocation, owned()).await.is_err());
        assert_eq!(sync_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let (release, released) = tokio::sync::watch::channel(false);
        let dispatch = ExtensionDispatch {
            invocation: invocation.clone(),
            services: dispatch.services.with_async(move |_| {
                let mut released = released.clone();
                Box::pin(async move {
                    loop {
                        let ready = *released.borrow_and_update();
                        if ready {
                            break;
                        }
                        released
                            .changed()
                            .await
                            .map_err(|_| protocol("test callback closed"))?;
                    }
                    Ok(b"committed".to_vec())
                })
            }),
        };
        let admitted = dispatch.invoke_async(&invocation, owned());
        invocation.close().expect("close");
        assert!(
            invocation.finish().is_err(),
            "admitted callback prevents retirement"
        );
        assert!(
            dispatch.invoke_async(&invocation, owned()).await.is_err(),
            "closed scopes reject new work"
        );
        release.send_replace(true);
        assert_eq!(admitted.await.expect("settled reply"), b"committed");
        invocation.drain().await.expect("drain");
        invocation.finish().expect("retire");
        activation.close().expect("close activation");
        activation.drain().await.expect("drain activation");
        activation.finish().expect("retire activation");
        instance.close().expect("close instance");
        instance.drain().await.expect("drain instance");
        instance.finish().expect("retire instance");
    }

    #[test]
    fn extension_host_services_are_bound_to_actual_scope_and_revoke_after_closure() {
        let instance = bmux_plugin_runtime::InstanceScope::new(
            bmux_plugin_runtime::PluginRuntimeScopeId::new(),
        );
        let activation = instance
            .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
            .expect("activation");
        activation.mark_active().expect("active");
        let invocation = activation.begin_invocation().expect("invocation");
        let sibling = activation.begin_invocation().expect("sibling");
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = calls.clone();
        let dispatch = ExtensionDispatch {
            invocation: invocation.clone(),
            services: bmux_plugin_sdk::definition::PluginHostServices::new(move |_| {
                observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(b"exact frontend".to_vec())
            }),
        };
        assert!(dispatch.invoke(&sibling, request()).is_err());
        assert_eq!(
            dispatch.invoke(&invocation, request()).expect("bound host"),
            b"exact frontend"
        );
        invocation.close().expect("closed");
        assert!(
            dispatch.invoke(&invocation, request()).is_err(),
            "captured host clones cannot reopen closed invocation"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        for scope in [invocation, sibling] {
            scope.close().expect("close");
            runtime.block_on(scope.drain()).expect("drain");
            scope.finish().expect("finish");
        }
        activation.close().expect("close activation");
        runtime
            .block_on(activation.drain())
            .expect("drain activation");
        activation.finish().expect("finish activation");
        instance.close().expect("close instance");
        runtime.block_on(instance.drain()).expect("drain instance");
        instance.finish().expect("finish instance");
    }

    #[test]
    fn native_bridge_cancellation_preserves_completed_reply_and_uncertain_errors() {
        use crate::invocation_callbacks::CallbackRegistration;
        use bcode_plugin_sdk::{
            ServiceBridge, ServiceBridgeError, ServiceBridgeRequest, ServiceBridgeResponse,
        };
        let request = ServiceBridgeRequest::Exchange(bcode_tool::ToolExchangeRequest {
            invocation_id: "phase-test".to_string(),
            exchange_id: "exchange".to_string(),
            producer_id: "producer".to_string(),
            schema: "test".to_string(),
            schema_version: 1,
            payload: serde_json::Value::Null,
            response_policy: bcode_tool::ToolExchangeResponsePolicy::Required,
        });
        let calls = std::sync::atomic::AtomicUsize::new(0);
        for (cancel_before, handler_fails) in [(true, false), (false, false), (false, true)] {
            let cancellation = bcode_plugin_sdk::ServiceCancellation::default();
            if cancel_before {
                cancellation.cancel();
            }
            let mut handler = |_, token: bcode_plugin_sdk::ServiceCancellation| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                token.cancel();
                if handler_fails {
                    return Err("post-effect outcome unavailable".to_string());
                }
                Ok(ServiceBridgeResponse::Exchange(
                    bcode_tool::ToolExchangeResolution::Responded {
                        payload: serde_json::json!({"committed": true}),
                    },
                ))
            };
            let mut state = crate::ServiceBridgeCallbackState {
                credentials: None,
                credential_reads: 0,
                on_bridge: &mut handler,
                cancellation,
            };
            let registration = CallbackRegistration::new(&mut state);
            let bridge = ServiceBridge::new(
                Some(crate::native_product_bridge_callback),
                registration.user_data(),
                bcode_plugin_sdk::ServiceCancellation::default(),
            );
            let result = bridge.request(&request);
            if cancel_before {
                assert!(
                    matches!(result, Err(ServiceBridgeError::Host { status }) if status == crate::NATIVE_BRIDGE_CANCELLED_BEFORE_DISPATCH)
                );
                assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
            } else if handler_fails {
                assert!(
                    matches!(result, Err(ServiceBridgeError::Host { status }) if status == crate::SERVICE_BRIDGE_STATUS_FAILED)
                );
            } else {
                assert!(
                    matches!(result, Ok(ServiceBridgeResponse::Exchange(_))),
                    "successful post-effect reply survives concurrent cancellation"
                );
            }
            drop(bridge);
            drop(registration);
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn extension_v2_acquisition_is_exact_invocation_and_never_falls_back() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let instance = bmux_plugin_runtime::InstanceScope::new(
            bmux_plugin_runtime::PluginRuntimeScopeId::new(),
        );
        let activation = instance
            .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
            .expect("activation");
        activation.mark_active().expect("active");
        let invocation = activation.begin_invocation().expect("invocation");
        let sibling = activation.begin_invocation().expect("sibling");
        let legacy_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let v2_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let legacy_count = legacy_calls.clone();
        let v2_count = v2_calls.clone();
        let services = bmux_plugin_sdk::definition::PluginHostServices::new(|_| {
            Err(protocol("unexpected service invocation"))
        })
        .with_retained_host(move || {
            legacy_count.fetch_add(1, Ordering::SeqCst);
            Err(protocol("legacy opener must not run"))
        })
        .with_retained_host_v2(move || {
            v2_count.fetch_add(1, Ordering::SeqCst);
            Err(protocol("selected V2 rejection"))
        });
        let dispatch = ExtensionDispatch {
            invocation: invocation.clone(),
            services,
        };
        assert!(dispatch.open_retained_host_v2(&sibling).is_err());
        assert_eq!(v2_calls.load(Ordering::SeqCst), 0);
        assert!(dispatch.open_retained_host_v2(&invocation).is_err());
        assert_eq!(v2_calls.load(Ordering::SeqCst), 1);
        invocation.close().expect("close invocation");
        assert!(dispatch.open_retained_host_v2(&invocation).is_err());
        assert_eq!(v2_calls.load(Ordering::SeqCst), 1);
        assert_eq!(legacy_calls.load(Ordering::SeqCst), 0);
        invocation.finish().expect("finish invocation");
        sibling.close().expect("close sibling");
        sibling.finish().expect("finish sibling");
        activation.close().expect("close activation");
        activation.finish().expect("finish activation");
        instance.close().expect("close instance");
        instance.finish().expect("finish instance");
    }

    #[test]
    fn retained_authority_is_cross_thread_exact_invocation_and_live_cancelled() {
        let instance = bmux_plugin_runtime::InstanceScope::new(
            bmux_plugin_runtime::PluginRuntimeScopeId::new(),
        );
        let activation = instance
            .begin_activation(bmux_plugin_runtime::PluginConcurrencyConfig::Concurrent)
            .expect("activation");
        activation.mark_active().expect("active");
        let first = activation.begin_invocation().expect("first");
        let second = activation.begin_invocation().expect("sibling");
        let first_context = context("first");
        let cancellation = first_context.cancellation.clone();
        let first_dispatch = dispatch(&first, first_context);
        let second_dispatch = dispatch(&second, context("second"));
        let mut wrong_kind = request();
        wrong_kind.kind = bmux_plugin_sdk::ServiceKind::Command;
        assert!(
            first_dispatch.invoke(&first, wrong_kind).is_err(),
            "configuration query cannot masquerade as a command"
        );
        let mut bridge_query = request();
        bridge_query.operation = "exchange";
        assert!(
            first_dispatch.invoke(&first, bridge_query).is_err(),
            "bridge effects cannot masquerade as a query"
        );
        std::thread::scope(|threads| {
            let response = threads
                .spawn(|| first_dispatch.invoke(&first, request()))
                .join()
                .expect("cross-thread callback")
                .expect("authority");
            let settings: ProductInvocationSettings =
                serde_json::from_slice(&response).expect("settings");
            assert_eq!(settings.config.secrets["private"], "first");
            assert!(
                first_dispatch.invoke(&second, request()).is_err(),
                "same-instance sibling cannot borrow caller authority"
            );
            let response = second_dispatch
                .invoke(&second, request())
                .expect("independent sibling");
            let settings: ProductInvocationSettings =
                serde_json::from_slice(&response).expect("settings");
            assert_eq!(settings.config.secrets["private"], "second");
        });
        cancellation.cancel();
        assert!(matches!(
            first_dispatch.invoke(&first, request()),
            Err(bmux_plugin_sdk::PluginError::InvocationCancelled)
        ));
        second.close().expect("close sibling");
        assert!(
            second_dispatch.invoke(&second, request()).is_err(),
            "stale retained port fails closed"
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        for invocation in [first, second] {
            invocation.close().expect("close");
            runtime.block_on(invocation.drain()).expect("drain");
            invocation.finish().expect("retire");
        }
        activation.close().expect("activation close");
        runtime
            .block_on(activation.drain())
            .expect("activation drain");
        activation.finish().expect("activation retire");
        instance.close().expect("instance close");
        runtime.block_on(instance.drain()).expect("instance drain");
        instance.finish().expect("instance retire");
    }
}
