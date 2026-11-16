//! In-process test double for host executor and lifecycle tests.
//!
//! Production plugins load only through shared definitions (`Shared`) or shared
//! native artifacts (`SharedNative`). Executor admission, cleanup and retry
//! behavior is independent of which backend is selected, so these tests use a
//! plain Rust hook table whose outcomes they control directly. It is not a
//! plugin ABI and is compiled only for this crate's unit tests.

use crate::{
    AuthProviderContribution, AuthProviderRegistry, NativeEventContext, NativeServiceContext,
    PluginLoadError, ServiceResponse,
};

/// One service invocation delivered to a [`TestBackend`].
pub struct TestInvocation<'a> {
    pub context: NativeServiceContext,
    pub on_event: &'a mut dyn FnMut(Vec<u8>),
}

/// Controllable lifecycle, registration, service and event hooks.
///
/// Status hooks return `0` for success and any other value as the failure code
/// reported through the corresponding [`PluginLoadError`].
#[derive(Clone, Copy)]
pub struct TestBackend {
    pub activate: fn() -> i32,
    pub deactivate: fn() -> i32,
    pub register_commands: Option<fn(&mut bcode_command::CommandRegistry) -> i32>,
    pub register_auth_providers: Option<fn(&mut Vec<AuthProviderContribution>) -> i32>,
    pub invoke: fn(&mut TestInvocation<'_>) -> ServiceResponse,
    pub handle_event: fn(&NativeEventContext) -> i32,
}

impl std::fmt::Debug for TestBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestBackend").finish_non_exhaustive()
    }
}

impl TestBackend {
    pub(crate) fn lifecycle(
        plugin_id: &str,
        hook: &'static str,
        code: i32,
    ) -> Result<(), PluginLoadError> {
        if code == 0 {
            Ok(())
        } else {
            Err(PluginLoadError::LifecycleFailed {
                plugin_id: plugin_id.to_owned(),
                hook,
                code,
            })
        }
    }

    pub(crate) fn register_commands(
        self,
        plugin_id: &str,
        registry: &mut bcode_command::CommandRegistry,
    ) -> Result<(), PluginLoadError> {
        let code = self.register_commands.map_or(0, |hook| hook(registry));
        Self::lifecycle(plugin_id, "register_commands", code)
    }

    pub(crate) fn register_auth_providers(
        self,
        plugin_id: &str,
        registry: &mut AuthProviderRegistry,
    ) -> Result<(), PluginLoadError> {
        let mut contributions = Vec::new();
        let code = self
            .register_auth_providers
            .map_or(0, |hook| hook(&mut contributions));
        Self::lifecycle(plugin_id, "register_auth_providers", code)?;
        for contribution in contributions {
            registry
                .register(plugin_id, contribution)
                .map_err(|source| PluginLoadError::AuthRegistration {
                    plugin_id: plugin_id.to_owned(),
                    source,
                })?;
        }
        Ok(())
    }

    pub(crate) fn invoke(
        self,
        context: NativeServiceContext,
        on_event: &mut dyn FnMut(Vec<u8>),
    ) -> ServiceResponse {
        (self.invoke)(&mut TestInvocation { context, on_event })
    }

    pub(crate) fn handle_event(self, context: &NativeEventContext) -> Result<(), PluginLoadError> {
        match (self.handle_event)(context) {
            0 => Ok(()),
            code => Err(PluginLoadError::EventHandlerFailed {
                plugin_id: context.plugin_id.clone(),
                code,
            }),
        }
    }
}
