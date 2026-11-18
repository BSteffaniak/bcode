//! Bcode product contracts adapted to the shared BMUX plugin runtime.
//!
//! Definitions are inert, factories are host-scoped, and these adapters own no
//! scheduler, loader, activation admission or ABI. The shared host must retain
//! and fence typed service handles exactly as it does generated BPDL providers.

pub mod activation_resources;
pub mod composer;
pub mod extensions;
pub mod host_session;
pub mod host_session_models;
mod manifest;
pub mod resources;
pub use activation_resources::{ActivationResource, ActivationResources};
#[cfg(feature = "tui")]
pub use extensions::{PortableFrontendAdapter, SelectedProductFrontend};
pub use extensions::{ProductExtensionDescriptor, ProductExtensionPort, ProductExtensions};
#[cfg(test)]
mod tests;

use std::sync::{Arc, Mutex};

pub use bmux_plugin_sdk::definition::{
    ComponentRole, ContractDescriptor, PluginCallContext, PluginCreateContext, PluginDefinition,
    PluginFactory, PluginFuture, PluginInstance, PluginLifecycleContext, PluginMetadata,
    PluginResult,
};
use bmux_plugin_sdk::{PluginCommandError, TypedServiceRegistry};
use serde::{Deserialize, Serialize};

use crate::{
    ConcurrentRustPlugin, NativeEventContext, NativeServiceContext, PluginConfigContext,
    PluginError, RustPlugin, ServiceCancellation, ServiceEventEmitter, ServiceResponse,
    TransientProgressLimits,
};

/// Rust-native Bcode operations registered on the shared instance, not an ABI.
///
/// All existing product service payloads and live capabilities remain unchanged.
pub trait BcodePluginService: Send + Sync {
    /// Execute the same controller resource operation without encoding on native calls.
    ///
    /// # Errors
    /// Returns unavailable, stale-resource or controller failures.
    fn interaction_operation(
        &self,
        _call: &PluginCallContext,
        _request: resources::InteractionResourceRequest,
    ) -> PluginResult<resources::InteractionResourceResponse> {
        Err(PluginCommandError::unavailable(
            "interaction resources not registered",
        ))
    }
    /// Fresh renderer-neutral controller factory registry for this frontend.
    fn interaction_registry(&self) -> Option<crate::interaction::PluginInteractionRegistry> {
        None
    }
    /// Fresh composer factory registry for this frontend.
    fn composer_registry(&self) -> Option<crate::composer::PluginComposerRegistry> {
        None
    }
    /// Native CLI registration for this selected component only; never ABI data.
    fn cli_registration(&self) -> Option<crate::StaticCliRegistration> {
        None
    }
    /// Fresh native registry source for selected-component frontend loading.
    #[cfg(feature = "tui")]
    fn frontend_registry_factory(&self) -> Option<fn() -> crate::tui::PluginTuiRegistry> {
        None
    }
    /// Construct a fresh local frontend registry for this selected component.
    #[cfg(feature = "tui")]
    fn frontend_registry(&self) -> Option<crate::tui::PluginTuiRegistry> {
        None
    }
    /// Invoke with host-bound configuration, cancellation, events and grants.
    fn invoke(&self, context: NativeServiceContext) -> ServiceResponse;
    /// Deliver a host-bound product event.
    ///
    /// # Errors
    /// Returns a plugin failure or an unavailable/reentrant implementation error.
    fn event(&self, context: NativeEventContext) -> Result<(), PluginError>;
}

/// Registry identity for the Rust-native product adapter.
pub const PRODUCT_SERVICE_INTERFACE: &str = "bcode.product-plugin/v1";
/// Shared event kind whose payload is a serialized Bcode `PluginEvent`.
///
/// Keeping its topic and byte payload in the owning product contract preserves
/// non-JSON events without guessing an encoding from the topic.
pub const PRODUCT_EVENT_KIND: &str = "bcode.product-event/v1";

/// Read-only activation snapshot extension. Empty request; repeated reads do not
/// rerun registration hooks. The shared host owns publication/revocation.
pub const ACTIVATION_CONTRIBUTIONS_EXTENSION: &str = "bcode.activation-contributions/v1";
/// Host-authorized product capabilities for one shared invocation. This value
/// is Rust-native and cannot be reconstructed from a caller's service payload.
pub struct ProductInvocationContext {
    /// Invocation-local configuration and credentials resolved by the host.
    pub config: PluginConfigContext,
    /// Progress bounds resolved by the host.
    pub transient_progress_limits: TransientProgressLimits,
    /// Retained host port preserving workspace grants, tools, workflow and
    /// generation/interaction/composer operations with normal authorization.
    pub bridge: crate::ServiceBridge,
}

/// Adapter supplied by the product host for one loaded instance.
///
/// The host must
/// verify the call's live invocation identity and retain/revoke its capabilities
/// through shared runtime admission. Payloads never select their own authority.
pub trait ProductInvocationHost: Send + Sync {
    /// Resolve capabilities for an admitted shared invocation.
    ///
    /// # Errors
    /// Returns an error for unknown/closed invocations or unavailable services.
    fn resolve(&self, call: &PluginCallContext) -> Result<ProductInvocationContext, PluginError>;
}

/// Product host operation contract transported over BMUX's invocation-bound port.
pub const PRODUCT_HOST_INTERFACE: &str = "bcode.invocation-host/v1";

/// Outcome of one dispatched product host bridge operation.
///
/// A bridge that fails after dispatch reports a product result, carried in the
/// payload, not a transport failure of the invocation port: the shared port
/// treats a failed host call as fatal to the whole operation, while the author
/// must observe a bridge failure (including cancellation during the bridge) as
/// an ordinary error it can report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProductHostOutcome {
    /// The host bridge resolved the request.
    Responded {
        /// Resolution of the requested operation.
        response: crate::ServiceBridgeResponse,
    },
    /// The bridge observed cancellation of the invocation.
    Cancelled,
    /// The host bridge failed with a bridge status after dispatch.
    Failed {
        /// Bridge status reported by the host.
        status: i32,
    },
}

impl ProductHostOutcome {
    /// Classify a host bridge result for transport to the author.
    #[must_use]
    pub fn from_result(
        result: Result<crate::ServiceBridgeResponse, crate::ServiceBridgeError>,
    ) -> Self {
        match result {
            Ok(response) => Self::Responded { response },
            Err(crate::ServiceBridgeError::Cancelled) => Self::Cancelled,
            Err(crate::ServiceBridgeError::Host { status }) => Self::Failed { status },
            Err(_) => Self::Failed {
                status: crate::SERVICE_BRIDGE_STATUS_FAILED,
            },
        }
    }

    /// The author-visible bridge result.
    ///
    /// # Errors
    /// Returns the host's cancellation or failure.
    pub fn into_result(self) -> Result<crate::ServiceBridgeResponse, crate::ServiceBridgeError> {
        match self {
            Self::Responded { response } => Ok(response),
            Self::Cancelled => Err(crate::ServiceBridgeError::Cancelled),
            Self::Failed { status } => Err(crate::ServiceBridgeError::Host { status }),
        }
    }
}

/// Adapter from the shared host port to Bcode's existing product operation sum.
///
/// The shared port supplies provenance/admission; product host dispatch owns the
/// operation's authorization and behavior. No native pointer or TLS is involved.
#[derive(Debug, Clone)]
pub struct SharedProductHostBridge(pub bmux_plugin_sdk::definition::PluginHostServices);

impl SharedProductHostBridge {
    /// Adapt M6's retained invocation port without reconstructing authority.
    #[must_use]
    pub fn from_bound(port: bmux_plugin_sdk::BoundHostServicePort) -> Self {
        Self(bmux_plugin_sdk::definition::PluginHostServices::from_bound(
            port,
        ))
    }
}

/// Resolver backed only by the explicitly injected shared invocation port.
///
/// Dynamic packaging receives the same live port through the shared ABI gate.
pub struct SharedInvocationHost;

/// Host-resolved configuration for the admitted invocation. This response is
/// secret-bearing internal invocation data, not discovery or public diagnostics.
#[derive(Serialize, Deserialize)]
pub struct ProductInvocationSettings {
    /// Host-resolved configuration and credentials for the actual plugin.
    pub config: PluginConfigContext,
    /// Host-enforced progress limits.
    pub transient_progress_limits: TransientProgressLimits,
}

impl ProductInvocationHost for SharedInvocationHost {
    fn resolve(&self, call: &PluginCallContext) -> Result<ProductInvocationContext, PluginError> {
        let services = call
            .host_services
            .clone()
            .ok_or_else(|| PluginError::failed("Bcode invocation host is not bound"))?;
        let bytes = services
            .invoke(bmux_plugin_sdk::HostServiceRequest {
                capability: "bcode.product",
                kind: bmux_plugin_sdk::ServiceKind::Query,
                interface: PRODUCT_HOST_INTERFACE,
                operation: "invocation_context",
                payload: &[],
            })
            .map_err(|_| PluginError::failed("Bcode invocation context unavailable"))?;
        if bytes.len() > crate::SERVICE_BRIDGE_MAX_RESPONSE_BYTES {
            return Err(PluginError::failed(
                "Bcode invocation context exceeds bound",
            ));
        }
        let settings: ProductInvocationSettings = serde_json::from_slice(&bytes)
            .map_err(|_| PluginError::failed("invalid Bcode invocation context"))?;
        Ok(ProductInvocationContext {
            config: settings.config,
            transient_progress_limits: settings.transient_progress_limits,
            bridge: crate::ServiceBridge::from_owned(
                Arc::new(SharedProductHostBridge(services)),
                ServiceCancellation::from_shared(call.cancellation.clone()),
            ),
        })
    }
}

impl crate::ProductHostBridge for SharedProductHostBridge {
    fn request(
        &self,
        request: &crate::ServiceBridgeRequest,
    ) -> Result<crate::ServiceBridgeResponse, crate::ServiceBridgeError> {
        let payload = serde_json::to_vec(request)
            .map_err(|error| crate::ServiceBridgeError::Encode(error.to_string()))?;
        let operation = match request {
            crate::ServiceBridgeRequest::Exchange(_) => "exchange",
            crate::ServiceBridgeRequest::ReceiveInput { .. } => "receive_input",
            crate::ServiceBridgeRequest::InvokeService(_) => "invoke_service",
            crate::ServiceBridgeRequest::WriteArtifact(_) => "write_artifact",
        };
        let response = self
            .0
            .invoke(bmux_plugin_sdk::HostServiceRequest {
                capability: "bcode.product",
                kind: bmux_plugin_sdk::ServiceKind::Command,
                interface: PRODUCT_HOST_INTERFACE,
                operation,
                payload: &payload,
            })
            .map_err(|error| match error {
                // Typed rejection before dispatch: no effects occurred.
                bmux_plugin_sdk::PluginError::InvocationCancelled => {
                    crate::ServiceBridgeError::Cancelled
                }
                _ => crate::ServiceBridgeError::Unavailable,
            })?;
        serde_json::from_slice::<ProductHostOutcome>(&response)
            .map_err(|_| {
                crate::ServiceBridgeError::Decode("invalid product host response".to_owned())
            })?
            .into_result()
    }
}

trait ProductImplementation: BcodePluginService + 'static {
    fn activate(&self) -> Result<(), PluginError>;
    fn deactivate(&self) -> Result<(), PluginError>;
    fn contributions(
        &self,
    ) -> Result<bcode_plugin_models::activation::ActivationContributions, PluginError>;
}

std::thread_local! {
    /// Exclusive implementations whose lock the current thread already holds.
    static HELD_EXCLUSIVE: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Releases one thread-held exclusive marker, including on unwind.
struct HeldExclusive(usize);
impl Drop for HeldExclusive {
    fn drop(&mut self) {
        HELD_EXCLUSIVE.with(|held| {
            let mut held = held.borrow_mut();
            if let Some(position) = held.iter().rposition(|key| *key == self.0) {
                held.remove(position);
            }
        });
    }
}

struct Serial<P>(Mutex<P>);
impl<P: RustPlugin> Serial<P> {
    fn with<T>(&self, operation: impl FnOnce(&mut P) -> T) -> Result<T, PluginError> {
        // Independent callers queue on the implementation lock, as exclusive
        // plugins always have. A nested entry from a call that already holds
        // this lock would wait on its own parent, so it is rejected instead.
        let key = std::ptr::from_ref(self).cast::<()>() as usize;
        if HELD_EXCLUSIVE.with(|held| held.borrow().contains(&key)) {
            return Err(PluginError::failed(
                "exclusive plugin is busy or unavailable",
            ));
        }
        let mut plugin = self
            .0
            .lock()
            .map_err(|_| PluginError::failed("exclusive plugin is busy or unavailable"))?;
        HELD_EXCLUSIVE.with(|held| held.borrow_mut().push(key));
        let _held = HeldExclusive(key);
        Ok(operation(&mut plugin))
    }
}
impl<P: RustPlugin> BcodePluginService for Serial<P> {
    fn invoke(&self, context: NativeServiceContext) -> ServiceResponse {
        self.with(|plugin| plugin.invoke_service(context))
            .unwrap_or_else(|_| {
                ServiceResponse::error("plugin_busy", "exclusive plugin is busy or unavailable")
            })
    }
    fn event(&self, context: NativeEventContext) -> Result<(), PluginError> {
        self.with(|plugin| plugin.handle_event(context))?
    }
}
impl<P: RustPlugin> ProductImplementation for Serial<P> {
    fn activate(&self) -> Result<(), PluginError> {
        self.with(RustPlugin::activate)?
    }
    fn deactivate(&self) -> Result<(), PluginError> {
        self.with(RustPlugin::deactivate)?
    }
    fn contributions(
        &self,
    ) -> Result<bcode_plugin_models::activation::ActivationContributions, PluginError> {
        self.with(|plugin| {
            let (mut snapshot, auth) =
                collect_contributions(|registrar| plugin.register_commands(registrar))?;
            plugin.register_auth_providers(crate::AuthRegistrar::collect(auth.clone()))?;
            snapshot.auth_providers = encode_collected(&auth)?;
            Ok(snapshot)
        })?
    }
}

struct Concurrent<P>(P);
impl<P: ConcurrentRustPlugin> BcodePluginService for Concurrent<P> {
    fn invoke(&self, context: NativeServiceContext) -> ServiceResponse {
        self.0.invoke_service_concurrent(context)
    }
    fn event(&self, context: NativeEventContext) -> Result<(), PluginError> {
        self.0.handle_event_concurrent(context)
    }
}
impl<P: ConcurrentRustPlugin> ProductImplementation for Concurrent<P> {
    fn activate(&self) -> Result<(), PluginError> {
        self.0.activate_concurrent()
    }
    fn deactivate(&self) -> Result<(), PluginError> {
        self.0.deactivate_concurrent()
    }
    fn contributions(
        &self,
    ) -> Result<bcode_plugin_models::activation::ActivationContributions, PluginError> {
        let (mut snapshot, auth) =
            collect_contributions(|registrar| self.0.register_commands_concurrent(registrar))?;
        self.0
            .register_auth_providers_concurrent(crate::AuthRegistrar::collect(auth.clone()))?;
        snapshot.auth_providers = encode_collected(&auth)?;
        Ok(snapshot)
    }
}

type AuthCollection = Arc<Mutex<Vec<bcode_provider_auth_models::AuthProviderContribution>>>;

fn encode_collected<T: Serialize>(values: &Mutex<Vec<T>>) -> Result<Vec<Vec<u8>>, PluginError> {
    values
        .lock()
        .map_err(|_| PluginError::failed("contribution collection unavailable"))?
        .iter()
        .map(|value| {
            serde_json::to_vec(value)
                .map_err(|_| PluginError::failed("contribution encoding failed"))
        })
        .collect()
}

fn collect_contributions(
    register: impl FnOnce(crate::CommandRegistrar) -> Result<(), PluginError>,
) -> Result<
    (
        bcode_plugin_models::activation::ActivationContributions,
        AuthCollection,
    ),
    PluginError,
> {
    let commands = Arc::new(Mutex::new(Vec::new()));
    register(crate::CommandRegistrar::collect(commands.clone()))?;
    Ok((
        bcode_plugin_models::activation::ActivationContributions {
            commands: encode_collected(&commands)?,
            ..bcode_plugin_models::activation::ActivationContributions::default()
        },
        Arc::new(Mutex::new(Vec::new())),
    ))
}

/// Structured-output CLI author entrypoint. Parsing stays at the author-local
/// boundary; only argv and product result data cross the extension contract.
pub type PortableCliInvoke = fn(
    clap::ArgMatches,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<bcode_command::cli::CliOutcome, String>>
            + Send
            + 'static,
    >,
>;

/// Factory for a fresh scoped product extension.
pub type ProductExtensionFactory = fn(PluginCreateContext) -> Arc<dyn ProductComponentExtension>;

/// Component-local author registrations. Factories remain Rust-native; only
/// versioned extension operations/values are transported by the shared ABI.
#[derive(Clone, Default)]
pub struct ProductRegistrations {
    /// Frontend-local CLI handler; never dispatched through model tools.
    pub cli: Option<crate::StaticCliRegistration>,
    /// Structured-output CLI handler. Absence rejects portable execution rather
    /// than redirecting process-global stdout/stderr or invoking a print handler.
    pub portable_cli: Option<PortableCliInvoke>,
    /// A fresh native frontend registry per selected frontend instance.
    #[cfg(feature = "tui")]
    pub frontend: Option<fn() -> crate::tui::PluginTuiRegistry>,
    /// Portable frontend adapters owned by the product presentation layer.
    pub extensions: std::collections::BTreeMap<String, ProductExtensionFactory>,
    /// Existing renderer-neutral interaction factory declarations.
    pub interactions: Option<fn() -> crate::interaction::PluginInteractionRegistry>,
    /// Configurable bound for the portable interaction resource adapter.
    pub interaction_policy: resources::InteractionResourcePolicy,
    /// Existing local composer contributor factories.
    pub composer: Option<fn() -> crate::composer::PluginComposerRegistry>,
}

impl ProductRegistrations {
    fn instance_extensions(
        &self,
        scope: &PluginCreateContext,
    ) -> std::collections::BTreeMap<String, Arc<dyn ProductComponentExtension>> {
        let extensions: std::collections::BTreeMap<_, _> = self
            .extensions
            .iter()
            .map(|(contract, factory)| (contract.clone(), factory(scope.clone())))
            .collect();
        #[cfg(feature = "tui")]
        {
            let mut extensions = extensions;
            if scope.role == ComponentRole::Frontend
                && let Some(factory) = self.frontend
            {
                extensions.insert(
                    crate::tui::portable::TUI_FRONTEND_CONTRACT.to_owned(),
                    crate::tui::portable::extension(factory, scope.clone()),
                );
            }
            extensions
        }
        #[cfg(not(feature = "tui"))]
        {
            extensions
        }
    }

    /// Attach renderer-neutral interaction controllers for native and portable dispatch.
    #[must_use]
    pub fn with_interactions(
        mut self,
        factory: fn() -> crate::interaction::PluginInteractionRegistry,
    ) -> Self {
        self.interactions = Some(factory);
        self
    }
    /// Attach composer contributors to this selected local frontend.
    #[must_use]
    pub fn with_composer(
        mut self,
        factory: fn() -> crate::composer::PluginComposerRegistry,
    ) -> Self {
        self.composer = Some(factory);
        self
    }
    /// Attach native CLI grammar/classification and local execution.
    #[must_use]
    pub const fn with_cli(mut self, cli: crate::StaticCliRegistration) -> Self {
        self.cli = Some(cli);
        self
    }
    /// Attach the handler that returns output instead of printing to the process.
    #[must_use]
    pub const fn with_portable_cli(mut self, invoke: PortableCliInvoke) -> Self {
        self.portable_cli = Some(invoke);
        self
    }
    /// Attach a fresh frontend registry factory independent of static packaging.
    ///
    /// Each frontend instance installs B3's portable adapter with this same factory
    /// and creation context. The frontend-v2 operation contract carries M7 retained-v2
    /// scenes; it is not wire-compatible with frontend-v1 responses. Backend instances
    /// do not install or advertise either frontend presentation contract.
    #[cfg(feature = "tui")]
    #[must_use]
    pub fn with_frontend(mut self, frontend: fn() -> crate::tui::PluginTuiRegistry) -> Self {
        self.frontend = Some(frontend);
        self
    }
    /// Attach a negotiated product extension adapter, rejecting duplicate names.
    ///
    /// # Errors
    /// Returns an error when a name is invalid, reserved or already registered.
    pub fn register_extension(
        &mut self,
        contract: String,
        extension: fn(PluginCreateContext) -> Arc<dyn ProductComponentExtension>,
    ) -> PluginResult<()> {
        #[cfg(feature = "tui")]
        if contract == crate::tui::portable::TUI_FRONTEND_CONTRACT
            || contract == bmux_tui::presentation::portable::PORTABLE_PRESENTATION_CONTRACT_ID
        {
            return Err(PluginCommandError::invalid_arguments(
                "frontend contracts are installed by with_frontend",
            ));
        }
        if contract.is_empty()
            || contract.len() > 256
            || contract.chars().any(char::is_control)
            || contract.starts_with("bcode.cli.")
            || contract == ACTIVATION_CONTRIBUTIONS_EXTENSION
            || contract == resources::INTERACTION_RESOURCE_CONTRACT
            || contract == composer::COMPOSER_ACTIVATE_CONTRACT
            || contract == extensions::PRODUCT_EXTENSIONS_CONTRACT
            || self.extensions.contains_key(&contract)
        {
            return Err(PluginCommandError::invalid_arguments(
                "invalid or duplicate component contract",
            ));
        }
        self.extensions.insert(contract, extension);
        Ok(())
    }
}

/// Versioned product adapter using shared instance extension dispatch.
///
/// Concrete
/// presentation owners retain geometry/revision/commit authority; this trait
/// introduces no alternate resource manager or renderer layout across the ABI.
pub trait ProductComponentExtension: Send + Sync {
    /// Release instance-local extension resources during successful deactivation.
    ///
    /// # Errors
    /// Returns an error when cleanup needs retry; the host must retain the instance.
    fn close(&self) -> PluginResult<()> {
        Ok(())
    }
    /// Scoped author entrypoint. Override to capture an independently retained
    /// host session while the call is live; default keeps existing synchronous
    /// extensions compatible without opening capabilities they did not request.
    fn invoke_scoped(
        &self,
        scope: PluginCreateContext,
        call: PluginCallContext,
        payload: Vec<u8>,
    ) -> PluginFuture<'_, PluginResult<Vec<u8>>> {
        let _ = scope;
        self.invoke(call, payload)
    }
    /// Invoke an already negotiated product contract.
    fn invoke(
        &self,
        call: PluginCallContext,
        payload: Vec<u8>,
    ) -> PluginFuture<'_, PluginResult<Vec<u8>>>;
}

struct NativeProductService<P> {
    implementation: Arc<P>,
    registrations: ProductRegistrations,
    role: ComponentRole,
    interaction_resources: Arc<resources::InteractionResources>,
}
impl<P: ProductImplementation> BcodePluginService for NativeProductService<P> {
    fn interaction_operation(
        &self,
        call: &PluginCallContext,
        request: resources::InteractionResourceRequest,
    ) -> PluginResult<resources::InteractionResourceResponse> {
        if self.role != ComponentRole::Frontend {
            return Err(PluginCommandError::unavailable(
                "frontend component required",
            ));
        }
        self.interaction_resources.execute(call, request)
    }
    fn interaction_registry(&self) -> Option<crate::interaction::PluginInteractionRegistry> {
        if self.role != ComponentRole::Frontend {
            return None;
        }
        self.registrations.interactions.map(|factory| factory())
    }
    fn composer_registry(&self) -> Option<crate::composer::PluginComposerRegistry> {
        if self.role != ComponentRole::Frontend {
            return None;
        }
        self.registrations.composer.map(|factory| factory())
    }
    fn invoke(&self, context: NativeServiceContext) -> ServiceResponse {
        if self.role != ComponentRole::Backend {
            return ServiceResponse::error(
                "wrong_component",
                "backend service unavailable on frontend component",
            );
        }
        self.implementation.invoke(context)
    }
    fn event(&self, context: NativeEventContext) -> Result<(), PluginError> {
        self.implementation.event(context)
    }
    fn cli_registration(&self) -> Option<crate::StaticCliRegistration> {
        (self.role == ComponentRole::Frontend)
            .then_some(self.registrations.cli)
            .flatten()
    }
    #[cfg(feature = "tui")]
    fn frontend_registry_factory(&self) -> Option<fn() -> crate::tui::PluginTuiRegistry> {
        if self.role != ComponentRole::Frontend {
            return None;
        }
        self.registrations.frontend
    }
    #[cfg(feature = "tui")]
    fn frontend_registry(&self) -> Option<crate::tui::PluginTuiRegistry> {
        if self.role != ComponentRole::Frontend {
            return None;
        }
        self.registrations.frontend.map(|create| create())
    }
}

struct ProductInstance<P> {
    scope: PluginCreateContext,
    implementation: Arc<P>,
    plugin_id: String,
    role: ComponentRole,
    registrations: ProductRegistrations,
    contributions: Mutex<Option<bcode_plugin_models::activation::ActivationContributions>>,
    host: Arc<dyn ProductInvocationHost>,
    extensions: std::collections::BTreeMap<String, Arc<dyn ProductComponentExtension>>,
    composer_registry: Option<crate::composer::PluginComposerRegistry>,
    interaction_resources: Arc<resources::InteractionResources>,
}

fn encode_extension(value: &impl Serialize) -> PluginResult<Vec<u8>> {
    let encoded = serde_json::to_vec(value)
        .map_err(|_| PluginCommandError::failed("component response encoding failed"))?;
    if encoded.len() > crate::SERVICE_BRIDGE_MAX_RESPONSE_BYTES {
        return Err(PluginCommandError::failed(
            "component response exceeds bound; do not replay operation",
        ));
    }
    Ok(encoded)
}

fn product_error(error: &PluginError) -> PluginCommandError {
    PluginCommandError::new(error.code(), error.message())
}

impl<P: ProductImplementation> ProductInstance<P> {
    fn invoke_product(
        &self,
        call: PluginCallContext,
        context: bmux_plugin_sdk::NativeServiceContext,
        events: ServiceEventEmitter,
    ) -> PluginResult<bmux_plugin_sdk::ServiceResponse> {
        if self.role != ComponentRole::Backend {
            return Err(PluginCommandError::unavailable(
                "backend operation requires backend component",
            ));
        }
        if context.plugin_id != self.plugin_id {
            return Err(PluginCommandError::unavailable("plugin identity mismatch"));
        }
        if call.cancellation.is_cancelled() {
            return Err(PluginCommandError::unavailable("invocation cancelled"));
        }
        let invocation = self
            .host
            .resolve(&call)
            .map_err(|error| product_error(&error))?;
        let response = self.implementation.invoke(NativeServiceContext {
            plugin_id: self.plugin_id.clone(),
            request: crate::ServiceRequest {
                interface_id: context.request.service.interface_id,
                operation: context.request.operation,
                payload: context.request.payload,
            },
            config: invocation.config,
            events,
            cancellation: ServiceCancellation::from_shared(call.cancellation),
            bridge: invocation.bridge,
            transient_progress_limits: invocation.transient_progress_limits,
        });
        Ok(bmux_plugin_sdk::ServiceResponse {
            payload: response.payload,
            error: response.error.map(|error| bmux_plugin_sdk::ServiceError {
                code: error.code,
                message: error.message,
            }),
        })
    }
}

impl<P: ProductImplementation> ProductInstance<P> {
    async fn invoke_cli_extension(
        &self,
        call: PluginCallContext,
        contract: &str,
        payload: &[u8],
    ) -> PluginResult<Vec<u8>> {
        use bcode_command::cli::{
            CLI_CLASSIFY_CONTRACT, CLI_DESCRIBE_CONTRACT, CLI_INVOKE_CONTRACT,
        };
        if contract == CLI_DESCRIBE_CONTRACT {
            if !payload.is_empty() {
                return Err(PluginCommandError::invalid_arguments(
                    "CLI describe expects no payload",
                ));
            }
            let descriptor = if self.role == ComponentRole::Frontend {
                self.registrations
                    .cli
                    .map(|registration| {
                        bcode_command::cli::CliDescriptor::extract((registration.command)())
                    })
                    .transpose()
                    .map_err(|_| PluginCommandError::unavailable("unsupported CLI declaration"))?
            } else {
                None
            };
            return encode_extension(&descriptor);
        }
        if contract == CLI_CLASSIFY_CONTRACT || contract == CLI_INVOKE_CONTRACT {
            if self.role != ComponentRole::Frontend {
                return Err(PluginCommandError::unavailable(
                    "CLI requires frontend component",
                ));
            }
            let registration = self
                .registrations
                .cli
                .ok_or_else(|| PluginCommandError::unavailable("CLI not registered"))?;
            let request: bcode_command::cli::CliArgvRequest = serde_json::from_slice(payload)
                .map_err(|_| PluginCommandError::invalid_arguments("invalid CLI argv"))?;
            let matches = match bcode_command::cli::descriptor::parse_local(
                (registration.command)(),
                &request,
            ) {
                Ok(matches) => matches,
                Err(error) if contract == CLI_INVOKE_CONTRACT => {
                    // Help/parser diagnostics are values, never process output.
                    let mut result = bcode_command::cli::CliOutcome {
                        exit_code: if error.use_stderr() { 2 } else { 0 },
                        ..bcode_command::cli::CliOutcome::default()
                    };
                    if error.use_stderr() {
                        result.stderr = error.to_string().into_bytes();
                    } else {
                        result.stdout = error.to_string().into_bytes();
                    }
                    return encode_extension(&result);
                }
                Err(_) => {
                    return Err(PluginCommandError::invalid_arguments(
                        "CLI argv rejected by author parser",
                    ));
                }
            };
            if contract == CLI_CLASSIFY_CONTRACT {
                return encode_extension(&bcode_command::cli::CliClassification {
                    requires_daemon: (registration.requires_daemon)(&matches),
                });
            }
            if call.cancellation.is_cancelled() {
                return Err(PluginCommandError::unavailable("CLI invocation cancelled"));
            }
            let invoke = self.registrations.portable_cli.ok_or_else(|| {
                PluginCommandError::unavailable("structured CLI output adapter not registered")
            })?;
            let outcome = invoke(matches).await.map_err(PluginCommandError::failed)?;
            if outcome.host_action.is_some()
                && (outcome.exit_code != 0
                    || !outcome.stdout.is_empty()
                    || !outcome.stderr.is_empty())
            {
                return Err(PluginCommandError::failed(
                    "CLI host action conflicts with output or failure",
                ));
            }
            return encode_extension(&outcome);
        }
        Err(PluginCommandError::invalid_arguments(
            "unsupported CLI contract",
        ))
    }
}

impl<P: ProductImplementation> PluginInstance for ProductInstance<P> {
    fn activate(&self, _context: PluginLifecycleContext) -> PluginFuture<'_, PluginResult<()>> {
        Box::pin(async move {
            let snapshot = if self.role == ComponentRole::Backend {
                self.implementation
                    .activate()
                    .map_err(|error| product_error(&error))?;
                self.implementation
                    .contributions()
                    .map_err(|error| product_error(&error))?
            } else {
                bcode_plugin_models::activation::ActivationContributions::default()
            };
            *self
                .contributions
                .lock()
                .map_err(|_| PluginCommandError::unavailable("contribution state unavailable"))? =
                Some(snapshot);
            Ok(())
        })
    }
    fn deactivate(&self, _context: PluginLifecycleContext) -> PluginFuture<'_, PluginResult<()>> {
        Box::pin(async move {
            self.interaction_resources.clear()?;
            for extension in self.extensions.values() {
                extension.close()?;
            }
            if self.role == ComponentRole::Backend {
                self.implementation
                    .deactivate()
                    .map_err(|error| product_error(&error))?;
            }
            *self
                .contributions
                .lock()
                .map_err(|_| PluginCommandError::unavailable("contribution state unavailable"))? =
                None;
            Ok(())
        })
    }
    fn invoke(
        &self,
        call: PluginCallContext,
        context: bmux_plugin_sdk::NativeServiceContext,
    ) -> PluginFuture<'_, PluginResult<bmux_plugin_sdk::ServiceResponse>> {
        Box::pin(async move { self.invoke_product(call, context, ServiceEventEmitter::default()) })
    }
    fn invoke_streaming(
        &self,
        call: PluginCallContext,
        context: bmux_plugin_sdk::NativeStreamingServiceContext,
    ) -> PluginFuture<'_, PluginResult<bmux_plugin_sdk::ServiceResponse>> {
        Box::pin(async move {
            let events =
                ServiceEventEmitter::from_shared(call.invocation_id.clone(), context.events);
            self.invoke_product(call, context.service, events)
        })
    }
    fn event(&self, event: bmux_plugin_sdk::PluginEvent) -> PluginFuture<'_, PluginResult<()>> {
        Box::pin(async move {
            if event.kind.as_str() != PRODUCT_EVENT_KIND {
                return Err(PluginCommandError::invalid_arguments(
                    "unsupported Bcode event contract",
                ));
            }
            let event = serde_json::from_value(event.payload)
                .map_err(|_| PluginCommandError::invalid_arguments("invalid product event"))?;
            self.implementation
                .event(NativeEventContext {
                    plugin_id: self.plugin_id.clone(),
                    event,
                })
                .map_err(|error| product_error(&error))
        })
    }
    fn extension(
        &self,
        call: PluginCallContext,
        contract: String,
        payload: Vec<u8>,
    ) -> PluginFuture<'_, PluginResult<Vec<u8>>> {
        Box::pin(async move {
            use bcode_command::cli::{
                CLI_CLASSIFY_CONTRACT, CLI_DESCRIBE_CONTRACT, CLI_INVOKE_CONTRACT,
            };
            if payload.len() > crate::SERVICE_BRIDGE_MAX_REQUEST_BYTES {
                return Err(PluginCommandError::invalid_arguments(
                    "component request exceeds bound",
                ));
            }
            if contract == extensions::PRODUCT_EXTENSIONS_CONTRACT {
                if !payload.is_empty() {
                    return Err(PluginCommandError::invalid_arguments(
                        "extension discovery expects empty payload",
                    ));
                }
                #[cfg(feature = "tui")]
                let native_frontend = self.registrations.frontend.is_some();
                #[cfg(not(feature = "tui"))]
                let native_frontend = false;
                return encode_extension(&ProductExtensionDescriptor {
                    plugin_id: self.plugin_id.clone(),
                    role: self.role,
                    contracts: self.registration_contracts()?,
                    native_frontend,
                });
            }
            if matches!(
                contract.as_str(),
                CLI_DESCRIBE_CONTRACT | CLI_CLASSIFY_CONTRACT | CLI_INVOKE_CONTRACT
            ) {
                return self.invoke_cli_extension(call, &contract, &payload).await;
            }
            if contract == composer::COMPOSER_ACTIVATE_CONTRACT {
                if self.role != ComponentRole::Frontend || call.cancellation.is_cancelled() {
                    return Err(PluginCommandError::unavailable(
                        "frontend composer unavailable",
                    ));
                }
                let request: composer::ComposerActivateRequest = serde_json::from_slice(&payload)
                    .map_err(|_| {
                    PluginCommandError::invalid_arguments("invalid composer activation request")
                })?;
                let registry = self
                    .composer_registry
                    .as_ref()
                    .ok_or_else(|| PluginCommandError::unavailable("composer not registered"))?;
                let activation = registry
                    .activate(&request.kind, &request.document)
                    .map_err(|_| {
                        PluginCommandError::invalid_arguments("composer activation rejected")
                    })?;
                return encode_extension(&activation);
            }
            if contract == resources::INTERACTION_RESOURCE_CONTRACT {
                if self.role != ComponentRole::Frontend {
                    return Err(PluginCommandError::unavailable(
                        "interaction requires frontend component",
                    ));
                }
                return self.interaction_resources.invoke(&call, &payload);
            }
            if let Some(extension) = self.extensions.get(&contract) {
                if self.role != ComponentRole::Frontend {
                    return Err(PluginCommandError::unavailable(
                        "presentation requires frontend component",
                    ));
                }
                return extension
                    .invoke_scoped(self.scope.clone(), call, payload)
                    .await;
            }
            if contract != ACTIVATION_CONTRIBUTIONS_EXTENSION || !payload.is_empty() {
                return Err(PluginCommandError::invalid_arguments(
                    "unsupported product extension",
                ));
            }
            let guard = self
                .contributions
                .lock()
                .map_err(|_| PluginCommandError::unavailable("contribution state unavailable"))?;
            let snapshot = guard
                .as_ref()
                .ok_or_else(|| PluginCommandError::unavailable("plugin activation not ready"))?;
            let result = serde_json::to_vec(snapshot)
                .map_err(|_| PluginCommandError::unavailable("contribution encoding failed"));
            drop(guard);
            result
        })
    }
    fn registration_contracts(&self) -> PluginResult<Vec<bmux_plugin_sdk::RegistrationContract>> {
        let mut interfaces = vec![
            ACTIVATION_CONTRIBUTIONS_EXTENSION.to_owned(),
            extensions::PRODUCT_EXTENSIONS_CONTRACT.to_owned(),
        ];
        if self.role == ComponentRole::Frontend {
            if self.registrations.cli.is_some() {
                interfaces.push(bcode_command::cli::CLI_DESCRIBE_CONTRACT.to_owned());
                interfaces.push(bcode_command::cli::CLI_CLASSIFY_CONTRACT.to_owned());
                if self.registrations.portable_cli.is_some() {
                    interfaces.push(bcode_command::cli::CLI_INVOKE_CONTRACT.to_owned());
                }
            }
            if self.registrations.composer.is_some() {
                interfaces.push(composer::COMPOSER_ACTIVATE_CONTRACT.to_owned());
            }
            if self.registrations.interactions.is_some() {
                interfaces.push(resources::INTERACTION_RESOURCE_CONTRACT.to_owned());
            }
            interfaces.extend(self.extensions.keys().cloned());
            #[cfg(feature = "tui")]
            if self.registrations.frontend.is_some() {
                interfaces.push(crate::tui::portable::TUI_FRONTEND_CONTRACT.to_owned());
                interfaces.push(
                    bmux_tui::presentation::portable::PORTABLE_PRESENTATION_CONTRACT_ID.to_owned(),
                );
            }
        }
        // The selected declaration is authoritative even before runtime extension
        // discovery. Both routes may contribute the same operation contract.
        interfaces.sort();
        interfaces.dedup();
        Ok(interfaces
            .into_iter()
            .map(|interface| bmux_plugin_sdk::RegistrationContract {
                kind: bmux_plugin_sdk::RegistrationKind::Extension,
                name: interface.clone(),
                version: {
                    #[cfg(feature = "tui")]
                    if interface == crate::tui::portable::TUI_FRONTEND_CONTRACT {
                        crate::tui::portable::TUI_FRONTEND_VERSION
                    } else if interface
                        == bmux_tui::presentation::portable::PORTABLE_PRESENTATION_CONTRACT_ID
                    {
                        bmux_tui::presentation::portable::PORTABLE_PRESENTATION_VERSION
                    } else {
                        1
                    }
                    #[cfg(not(feature = "tui"))]
                    {
                        1
                    }
                },
                interface,
            })
            .collect())
    }
    fn register_typed_services(&self, registry: &mut TypedServiceRegistry) -> PluginResult<()> {
        let provider: Arc<dyn BcodePluginService> = Arc::new(NativeProductService {
            implementation: self.implementation.clone(),
            registrations: self.registrations.clone(),
            role: self.role,
            interaction_resources: self.interaction_resources.clone(),
        });
        registry.insert_typed(
            bmux_plugin_sdk::HostScope::new("bcode.product")
                .map_err(|_| PluginCommandError::unavailable("invalid product scope"))?,
            bmux_plugin_sdk::ServiceKind::Command,
            PRODUCT_SERVICE_INTERFACE,
            provider,
        );
        Ok(())
    }
}

/// Adapt an existing exclusive implementation without global singleton state.
///
/// The host owns lifecycle admission; concurrent or recursive exclusive calls
/// fail explicitly. Use the concurrent adapter for callbacks that make nested calls.
#[must_use]
pub fn rust_plugin_definition<P: RustPlugin>(metadata: PluginMetadata) -> PluginDefinition {
    rust_plugin_definition_with_host::<P>(metadata, |_| Arc::new(SharedInvocationHost))
}

/// Adapt an exclusive implementation with a host-bound invocation resolver.
///
/// The resolver factory is invoked once per loaded instance, not at discovery.
#[must_use]
pub fn rust_plugin_definition_with_host<P: RustPlugin>(
    metadata: PluginMetadata,
    host: impl Fn(PluginCreateContext) -> Arc<dyn ProductInvocationHost> + Send + Sync + 'static,
) -> PluginDefinition {
    rust_plugin_definition_with_registrations::<P>(metadata, P::component_registrations(), host)
}

/// Create one product adapter retaining actual component registrations.
#[must_use]
pub fn rust_plugin_definition_with_registrations<P: RustPlugin>(
    metadata: PluginMetadata,
    registrations: ProductRegistrations,
    host: impl Fn(PluginCreateContext) -> Arc<dyn ProductInvocationHost> + Send + Sync + 'static,
) -> PluginDefinition {
    let (metadata, declaration_error) = manifest::shared_metadata(metadata);
    let plugin_id = metadata.plugin_id.clone();
    PluginDefinition::new(metadata, move |scope: PluginCreateContext| {
        if let Some(error) = &declaration_error {
            return Err(PluginCommandError::failed(error));
        }
        Ok(Arc::new(ProductInstance {
            scope: scope.clone(),
            implementation: Arc::new(Serial(Mutex::new(P::default()))),
            role: scope.role,
            registrations: registrations.clone(),
            composer_registry: registrations.composer.map(|factory| factory()),
            extensions: registrations.instance_extensions(&scope),
            interaction_resources: Arc::new(resources::InteractionResources::new(
                registrations.interactions,
                registrations.interaction_policy,
            )),
            plugin_id: plugin_id.clone(),
            contributions: Mutex::new(None),
            host: host(scope),
        }) as Arc<dyn PluginInstance>)
    })
}

/// Adapt an existing concurrent implementation; each factory call creates fresh state.
#[must_use]
pub fn concurrent_plugin_definition<P: ConcurrentRustPlugin>(
    metadata: PluginMetadata,
) -> PluginDefinition {
    concurrent_plugin_definition_with_host::<P>(metadata, |_| Arc::new(SharedInvocationHost))
}

/// Adapt a concurrent implementation with a host-bound invocation resolver.
#[must_use]
pub fn concurrent_plugin_definition_with_host<P: ConcurrentRustPlugin>(
    metadata: PluginMetadata,
    host: impl Fn(PluginCreateContext) -> Arc<dyn ProductInvocationHost> + Send + Sync + 'static,
) -> PluginDefinition {
    concurrent_plugin_definition_with_registrations::<P>(
        metadata,
        P::component_registrations(),
        host,
    )
}

/// Create one concurrent product adapter retaining actual component registrations.
#[must_use]
pub fn concurrent_plugin_definition_with_registrations<P: ConcurrentRustPlugin>(
    metadata: PluginMetadata,
    registrations: ProductRegistrations,
    host: impl Fn(PluginCreateContext) -> Arc<dyn ProductInvocationHost> + Send + Sync + 'static,
) -> PluginDefinition {
    let (metadata, declaration_error) = manifest::shared_metadata(metadata);
    let plugin_id = metadata.plugin_id.clone();
    PluginDefinition::new(metadata, move |scope: PluginCreateContext| {
        if let Some(error) = &declaration_error {
            return Err(PluginCommandError::failed(error));
        }
        Ok(Arc::new(ProductInstance {
            scope: scope.clone(),
            implementation: Arc::new(Concurrent(P::default())),
            role: scope.role,
            registrations: registrations.clone(),
            composer_registry: registrations.composer.map(|factory| factory()),
            extensions: registrations.instance_extensions(&scope),
            interaction_resources: Arc::new(resources::InteractionResources::new(
                registrations.interactions,
                registrations.interaction_policy,
            )),
            plugin_id: plugin_id.clone(),
            contributions: Mutex::new(None),
            host: host(scope),
        }) as Arc<dyn PluginInstance>)
    })
}

/// Derive shared identity and contracts from the existing product manifest.
///
/// All product metadata remains embedded; parsing never activates an instance.
///
/// # Errors
/// Returns an error for malformed manifests or missing identity/version fields.
pub fn metadata_from_manifest(
    manifest: &str,
    registrations: &ProductRegistrations,
) -> PluginResult<PluginMetadata> {
    let value: toml::Value = toml::from_str(manifest)
        .map_err(|_| PluginCommandError::invalid_arguments("invalid Bcode plugin manifest"))?;
    let identity = |field: &str| -> PluginResult<String> {
        value
            .get(field)
            .and_then(toml::Value::as_str)
            .filter(|value| {
                !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
            })
            .map(str::to_owned)
            .ok_or_else(|| {
                PluginCommandError::invalid_arguments("missing or invalid plugin identity")
            })
    };
    let mut roles = vec![ComponentRole::Backend];
    let native_frontend = {
        #[cfg(feature = "tui")]
        {
            registrations.frontend.is_some()
        }
        #[cfg(not(feature = "tui"))]
        {
            false
        }
    };
    if registrations.cli.is_some()
        || native_frontend
        || registrations.interactions.is_some()
        || registrations.composer.is_some()
        || !registrations.extensions.is_empty()
    {
        roles.push(ComponentRole::Frontend);
    }
    let mut interfaces = std::collections::BTreeSet::from([PRODUCT_SERVICE_INTERFACE.to_owned()]);
    if let Some(services) = value.get("services").and_then(toml::Value::as_array) {
        for service in services {
            let interface = service
                .get("interface_id")
                .and_then(toml::Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    PluginCommandError::invalid_arguments("service interface missing")
                })?;
            interfaces.insert(interface.to_owned());
        }
    }
    let required_capabilities = value
        .get("required_capabilities")
        .map(|values| {
            values
                .as_array()
                .ok_or_else(|| {
                    PluginCommandError::invalid_arguments("invalid required capabilities")
                })?
                .iter()
                .map(|value| {
                    value.as_str().map(str::to_owned).ok_or_else(|| {
                        PluginCommandError::invalid_arguments("invalid required capability")
                    })
                })
                .collect::<PluginResult<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(PluginMetadata {
        plugin_id: identity("id")?,
        version: identity("version")?,
        roles,
        required_capabilities,
        contracts: interfaces
            .into_iter()
            .map(|interface_id| {
                let major = interface_id
                    .rsplit_once("/v")
                    .and_then(|(_, version)| version.parse::<u16>().ok())
                    .filter(|version| *version != 0)
                    .ok_or_else(|| {
                        PluginCommandError::invalid_arguments(
                            "unsupported product interface version",
                        )
                    })?;
                Ok(ContractDescriptor {
                    interface_id,
                    major,
                    minor: 0,
                })
            })
            .collect::<PluginResult<Vec<_>>>()?,
        manifest_toml: Some(manifest.to_owned()),
    })
}

/// Manifest-based exclusive adapter, sharing the same injectable host factory.
///
/// # Errors
/// Returns an error for malformed product metadata or shared declarations.
pub fn rust_plugin_definition_from_manifest<P: RustPlugin>(
    manifest: &str,
    registrations: ProductRegistrations,
) -> PluginResult<PluginDefinition> {
    let metadata = metadata_from_manifest(manifest, &registrations)?;
    let definition =
        rust_plugin_definition_with_registrations::<P>(metadata, registrations, |_| {
            Arc::new(SharedInvocationHost)
        });
    definition.declarations()?;
    Ok(definition)
}

/// Manifest-based concurrent adapter, preserving auth and streaming dispatch.
///
/// # Errors
/// Returns an error for malformed product metadata or shared declarations.
pub fn concurrent_plugin_definition_from_manifest<P: ConcurrentRustPlugin>(
    manifest: &str,
    registrations: ProductRegistrations,
) -> PluginResult<PluginDefinition> {
    let metadata = metadata_from_manifest(manifest, &registrations)?;
    let definition =
        concurrent_plugin_definition_with_registrations::<P>(metadata, registrations, |_| {
            Arc::new(SharedInvocationHost)
        });
    definition.declarations()?;
    Ok(definition)
}

/// Declare a product implementation: emits `plugin_definition()`, and the
/// single dynamic-library entry symbol when the BMUX packager builds the crate.
///
/// The packager runs `cargo rustc --crate-type cdylib --features dynamic-<role>`
/// with `--cfg bmux_plugin_dylib`; only then does the expansion export the entry,
/// restricted to that role, exactly as BMUX's `plugin!` does. Workspace and
/// bundled builds export nothing. Plugin crates declare the
/// `dynamic-backend`/`dynamic-frontend` role features and never name the cfg.
#[macro_export]
macro_rules! define_plugin {
    (concurrent $plugin:ty, metadata = $metadata:expr, registrations = $registrations:expr $(,)?) => {
        pub fn plugin_definition() -> $crate::definition::PluginDefinition {
            $crate::definition::concurrent_plugin_definition_with_registrations::<$plugin>(
                $metadata, $registrations, |_| std::sync::Arc::new($crate::definition::SharedInvocationHost))
        }
        $crate::__shared::packaged_entry!(plugin_definition);
    };
    ($plugin:ty, metadata = $metadata:expr, registrations = $registrations:expr $(,)?) => {
        pub fn plugin_definition() -> $crate::definition::PluginDefinition {
            $crate::definition::rust_plugin_definition_with_registrations::<$plugin>(
                $metadata, $registrations, |_| std::sync::Arc::new($crate::definition::SharedInvocationHost))
        }
        $crate::__shared::packaged_entry!(plugin_definition);
    };
    (serial $plugin:ty, $manifest:expr, registrations = $registrations:expr $(,)?) => {
        pub fn plugin_definition() -> $crate::definition::PluginDefinition {
            $crate::definition::rust_plugin_definition_from_manifest::<$plugin>($manifest, $registrations)
                .expect("embedded Bcode plugin declaration must be valid")
        }
        $crate::__shared::packaged_entry!(plugin_definition);
    };
    (concurrent $plugin:ty, $manifest:expr, registrations = $registrations:expr $(,)?) => {
        pub fn plugin_definition() -> $crate::definition::PluginDefinition {
            $crate::definition::concurrent_plugin_definition_from_manifest::<$plugin>($manifest, $registrations)
                .expect("embedded Bcode plugin declaration must be valid")
        }
        $crate::__shared::packaged_entry!(plugin_definition);
    };
    (serial $plugin:ty, $manifest:expr $(,)?) => {
        $crate::define_plugin!(serial $plugin, $manifest,
            registrations = <$plugin as $crate::RustPlugin>::component_registrations());
    };
    (concurrent $plugin:ty, $manifest:expr $(,)?) => {
        $crate::define_plugin!(concurrent $plugin, $manifest,
            registrations = <$plugin as $crate::RustPlugin>::component_registrations());
    };
}
