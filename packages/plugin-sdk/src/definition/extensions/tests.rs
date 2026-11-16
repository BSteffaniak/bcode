use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct Port {
    closed: AtomicBool,
    calls: AtomicUsize,
}
impl ProductExtensionPort for Port {
    fn ensure_active(&self) -> PluginResult<()> {
        if self.closed.load(Ordering::SeqCst) {
            Err(PluginCommandError::unavailable("closed"))
        } else {
            Ok(())
        }
    }
    fn invoke(
        &self,
        contract: String,
        _payload: Vec<u8>,
    ) -> PluginFuture<'_, PluginResult<Vec<u8>>> {
        Box::pin(async move {
            self.ensure_active()?;
            self.calls.fetch_add(1, Ordering::SeqCst);
            if contract == PRODUCT_EXTENSIONS_CONTRACT {
                encode_extension(&ProductExtensionDescriptor {
                    plugin_id: "bcode.selected".to_owned(),
                    role: ComponentRole::Frontend,
                    contracts: vec![bmux_plugin_sdk::RegistrationContract {
                        kind: bmux_plugin_sdk::RegistrationKind::Extension,
                        interface: "example/v1".to_owned(),
                        version: 1,
                        name: "example".to_owned(),
                    }],
                    native_frontend: false,
                })
            } else {
                Ok(vec![0, 255])
            }
        })
    }
}
fn ready<T>(mut future: std::pin::Pin<Box<dyn std::future::Future<Output = T> + '_>>) -> T {
    match future
        .as_mut()
        .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
    {
        std::task::Poll::Ready(value) => value,
        std::task::Poll::Pending => panic!("test port must be immediately ready"),
    }
}
#[test]
fn scoped_host_default_denies_without_replaying_unscoped_operation() {
    let port = Arc::new(Port {
        closed: AtomicBool::new(false),
        calls: AtomicUsize::new(0),
    });
    let selected = ProductExtensions::new(port.clone(), None);
    let services = bmux_plugin_sdk::definition::PluginHostServices::new(|_| {
        panic!("default-denied host authority must not be invoked")
    });
    assert!(
        ready(Box::pin(selected.invoke_with_host(
            "example/v1",
            vec![1],
            services
        )))
        .is_err()
    );
    // Only negotiation may use the unscoped route; the requested operation is never replayed.
    assert_eq!(port.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn selected_port_negotiates_and_never_substitutes_closed_owner() {
    let port = Arc::new(Port {
        closed: AtomicBool::new(false),
        calls: AtomicUsize::new(0),
    });
    let selected = ProductExtensions::new(port.clone(), None);
    assert_eq!(
        ready(Box::pin(selected.invoke("example/v1", vec![1]))).unwrap(),
        vec![0, 255]
    );
    assert_eq!(port.calls.load(Ordering::SeqCst), 2);
    assert!(ready(Box::pin(selected.invoke("unknown/v1", vec![]))).is_err());
    assert_eq!(port.calls.load(Ordering::SeqCst), 3); // discovery only, no unknown operation
    port.closed.store(true, Ordering::SeqCst);
    assert!(ready(Box::pin(selected.descriptor())).is_err());
    assert_eq!(port.calls.load(Ordering::SeqCst), 3);
}
#[cfg(feature = "tui")]
#[test]
fn dynamic_frontend_requires_adapter_and_retains_selected_owner() {
    struct Adapter;
    impl PortableFrontendAdapter for Adapter {
        fn registry(
            &self,
            owner: ProductExtensions,
            descriptor: &ProductExtensionDescriptor,
        ) -> PluginResult<crate::tui::PluginTuiRegistry> {
            owner.ensure_active()?;
            assert_eq!(descriptor.plugin_id, "bcode.selected");
            Ok(crate::tui::PluginTuiRegistry::default())
        }
    }
    let port = Arc::new(Port {
        closed: AtomicBool::new(false),
        calls: AtomicUsize::new(0),
    });
    let selected = ProductExtensions::new(port.clone(), None);
    assert!(ready(Box::pin(selected.resolve_frontend(None))).is_err());
    let frontend = ready(Box::pin(selected.resolve_frontend(Some(&Adapter)))).unwrap();
    assert!(frontend.registry().is_ok());
    drop(selected);
    assert!(frontend.owner().ensure_active().is_ok());
    port.closed.store(true, Ordering::SeqCst);
    assert!(frontend.registry().is_err());
}
