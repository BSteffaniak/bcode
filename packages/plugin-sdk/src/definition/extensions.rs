//! Selected-component extension access shared by static and dynamic packaging.
//!
//! The host supplies the retained execution port; this module is not an owner,
//! scheduler or loader. Native registries and portable adapters are negotiated
//! against the same actual selected instance, never a bundled plugin lookup.

use super::{
    Arc, BcodePluginService, ComponentRole, Deserialize, PluginCommandError, PluginFuture,
    PluginResult, Serialize, encode_extension,
};
#[cfg(test)]
mod tests;

/// Read the actual selected component's versioned extension registrations.
pub const PRODUCT_EXTENSIONS_CONTRACT: &str = "bcode.component-extensions/v1";

/// Component-owned contract observations, not authority or executable code.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductExtensionDescriptor {
    /// Actual plugin logical identity.
    pub plugin_id: String,
    /// Actual selected component role.
    pub role: ComponentRole,
    /// Exact portable contracts implemented by this instance.
    pub contracts: Vec<bmux_plugin_sdk::RegistrationContract>,
    /// An author-native frontend factory is present in this artifact.
    pub native_frontend: bool,
}

/// B2-owned execution port for one loaded component/activation.
///
/// Implementations
/// retain the actual owner and library, admit each call through its activation,
/// and reject closed/stale owners. Dropping an observer never unloads active work.
/// This trait is Rust-local; dynamic implementations use shared ABI operations.
pub trait ProductExtensionPort: Send + Sync {
    /// Verify current owner/activation admission without performing product work.
    ///
    /// # Errors
    /// Returns an error for inactive, closing, stale or incompatible components.
    fn ensure_active(&self) -> PluginResult<()>;
    /// Obtain a separately negotiated retained product host session capability.
    ///
    /// # Errors
    /// Returns unsupported unless the host binds the exact selected retained port.
    /// Never wrap invocation-local `host_services` or reconstruct authority from IDs.
    fn product_host_session(
        &self,
    ) -> Result<
        super::host_session::ProductHostSessionClient,
        super::host_session::HostSessionClientError,
    > {
        Err(super::host_session::HostSessionClientError::Unsupported)
    }
    /// Return the actual selected activation's Rust-local resource authority.
    ///
    /// # Errors
    /// Returns an error when the owner is inactive or does not expose resource authority.
    fn activation_scope(&self) -> PluginResult<bmux_plugin_runtime::ActivationScope> {
        Err(PluginCommandError::unavailable(
            "selected activation scope unavailable",
        ))
    }
    /// Return the actual selected activation's host resource ledger. The owner
    /// retires and releases its remaining entries during deactivation, after
    /// draining and before finishing the activation.
    ///
    /// # Errors
    /// Returns an error when the owner is inactive or exposes no resource ledger.
    fn activation_resources(&self) -> PluginResult<super::ActivationResources> {
        Err(PluginCommandError::unavailable(
            "selected activation resources unavailable",
        ))
    }
    /// Invoke with explicit request-local host authority, never an ambient fallback.
    fn invoke_with_host(
        &self,
        _contract: String,
        _payload: Vec<u8>,
        _host: bmux_plugin_sdk::definition::PluginHostServices,
    ) -> PluginFuture<'_, PluginResult<Vec<u8>>> {
        Box::pin(async {
            Err(PluginCommandError::unavailable(
                "selected scoped host invocation unavailable",
            ))
        })
    }
    /// Invoke exactly once through shared execution with caller-bound host ports,
    /// cancellation and outcome acknowledgment. Never replay on response growth.
    fn invoke(&self, contract: String, payload: Vec<u8>)
    -> PluginFuture<'_, PluginResult<Vec<u8>>>;
}

/// One retained selected-component registry. The optional native provider must
/// belong to the exact same instance as `port`; host wiring verifies that binding.
#[derive(Clone)]
pub struct ProductExtensions {
    port: Arc<dyn ProductExtensionPort>,
    provider: Option<Arc<dyn BcodePluginService>>,
    author_session: Option<super::host_session::ProductHostSessionClient>,
}
impl ProductExtensions {
    /// Acquire the selected owner's retained session port, independent of request
    /// invocation retirement. The returned capability preserves close/join/ack
    /// recovery after admission closes; individual cleanup calls do not reopen it.
    ///
    /// # Errors
    /// Returns unsupported until B4/M1 bind an explicitly negotiated retained port.
    pub fn product_host_session(
        &self,
    ) -> Result<
        super::host_session::ProductHostSessionClient,
        super::host_session::HostSessionClientError,
    > {
        if let Some(session) = &self.author_session {
            return Ok(session.clone());
        }
        self.port.product_host_session()
    }
    /// Attach an explicitly negotiated independent author capability. Static and
    /// native callers use the same client; this never wraps invocation services.
    #[must_use]
    pub fn with_product_host_session(
        mut self,
        session: super::host_session::ProductHostSessionClient,
    ) -> Self {
        self.author_session = Some(session);
        self
    }
    /// Obtain resource authority for this exact selected activation.
    ///
    /// # Errors
    /// Returns an error for a closed owner or an unsupported activation port.
    pub fn activation_scope(&self) -> PluginResult<bmux_plugin_runtime::ActivationScope> {
        self.port.ensure_active()?;
        self.port.activation_scope()
    }
    /// Obtain the host resource ledger of this exact selected activation.
    ///
    /// # Errors
    /// Returns an error for a closed owner or an unsupported activation port.
    pub fn activation_resources(&self) -> PluginResult<super::ActivationResources> {
        self.port.ensure_active()?;
        self.port.activation_resources()
    }
    /// Bind a selected host component, optionally with its actual native provider.
    #[must_use]
    pub fn new(
        port: Arc<dyn ProductExtensionPort>,
        provider: Option<Arc<dyn BcodePluginService>>,
    ) -> Self {
        Self {
            port,
            provider,
            author_session: None,
        }
    }
    /// Check that the retained selected owner still accepts frontend work.
    ///
    /// # Errors
    /// Returns owner admission failure; no bundled substitute is selected.
    pub fn ensure_active(&self) -> PluginResult<()> {
        self.port.ensure_active()
    }
    /// Inspect exact contracts on the selected instance without activating another.
    ///
    /// # Errors
    /// Returns admission, bounded protocol, identity or compatibility errors.
    pub async fn descriptor(&self) -> PluginResult<ProductExtensionDescriptor> {
        let bytes = self
            .invoke_raw(PRODUCT_EXTENSIONS_CONTRACT, Vec::new())
            .await?;
        let descriptor: ProductExtensionDescriptor = serde_json::from_slice(&bytes)
            .map_err(|_| PluginCommandError::failed("invalid component extension descriptor"))?;
        if descriptor.plugin_id.is_empty() || descriptor.contracts.len() > 1024 {
            return Err(PluginCommandError::failed(
                "invalid component extension descriptor",
            ));
        }
        for contract in &descriptor.contracts {
            contract.validate().map_err(PluginCommandError::failed)?;
        }
        Ok(descriptor)
    }
    async fn invoke_raw(&self, contract: &str, payload: Vec<u8>) -> PluginResult<Vec<u8>> {
        self.port.ensure_active()?;
        if payload.len() > crate::SERVICE_BRIDGE_MAX_REQUEST_BYTES {
            return Err(PluginCommandError::invalid_arguments(
                "extension request exceeds bound",
            ));
        }
        let bytes = self.port.invoke(contract.to_owned(), payload).await?;
        if bytes.len() > crate::SERVICE_BRIDGE_MAX_RESPONSE_BYTES {
            return Err(PluginCommandError::failed(
                "extension response exceeds bound; operation must not be replayed",
            ));
        }
        self.port.ensure_active()?;
        Ok(bytes)
    }
    /// Invoke a negotiated exact contract on the selected owner.
    ///
    /// # Errors
    /// Returns unknown/incompatible contract, owner, execution or protocol failures.
    pub async fn invoke(&self, contract: &str, payload: Vec<u8>) -> PluginResult<Vec<u8>> {
        let descriptor = self.descriptor().await?;
        #[cfg(feature = "tui")]
        let expected_version = if contract == crate::tui::portable::TUI_FRONTEND_CONTRACT {
            crate::tui::portable::TUI_FRONTEND_VERSION
        } else if contract == bmux_tui::presentation::portable::PORTABLE_PRESENTATION_CONTRACT_ID {
            bmux_tui::presentation::portable::PORTABLE_PRESENTATION_VERSION
        } else {
            1
        };
        #[cfg(not(feature = "tui"))]
        let expected_version = 1;
        if !descriptor
            .contracts
            .iter()
            .any(|entry| entry.interface == contract && entry.version == expected_version)
        {
            return Err(PluginCommandError::unavailable(
                "selected component contract not negotiated",
            ));
        }
        self.invoke_raw(contract, payload).await
    }
    /// Invoke a negotiated contract with explicit host services for this call.
    ///
    /// # Errors
    /// Returns owner, negotiation, admission or protocol errors without replay.
    pub async fn invoke_with_host(
        &self,
        contract: &str,
        payload: Vec<u8>,
        host: bmux_plugin_sdk::definition::PluginHostServices,
    ) -> PluginResult<Vec<u8>> {
        let descriptor = self.descriptor().await?;
        let version = descriptor
            .contracts
            .iter()
            .find(|entry| entry.interface == contract)
            .ok_or_else(|| {
                PluginCommandError::unavailable("selected component contract not negotiated")
            })?
            .version;
        #[cfg(feature = "tui")]
        let expected = if contract == crate::tui::portable::TUI_FRONTEND_CONTRACT {
            crate::tui::portable::TUI_FRONTEND_VERSION
        } else if contract == bmux_tui::presentation::portable::PORTABLE_PRESENTATION_CONTRACT_ID {
            bmux_tui::presentation::portable::PORTABLE_PRESENTATION_VERSION
        } else {
            1
        };
        #[cfg(not(feature = "tui"))]
        let expected = 1;
        if version != expected || payload.len() > crate::SERVICE_BRIDGE_MAX_REQUEST_BYTES {
            return Err(PluginCommandError::invalid_arguments(
                "extension version or request bound mismatch",
            ));
        }
        self.ensure_active()?;
        let response = self
            .port
            .invoke_with_host(contract.to_owned(), payload, host)
            .await?;
        if response.len() > crate::SERVICE_BRIDGE_MAX_RESPONSE_BYTES {
            return Err(PluginCommandError::failed(
                "extension response exceeds bound; do not replay",
            ));
        }
        self.ensure_active()?;
        Ok(response)
    }

    /// Observe this exact component's native CLI registration when available.
    /// Portable components use the same owner's descriptor/invoke operations.
    ///
    /// # Errors
    /// Returns an owner admission failure; no replacement provider is consulted.
    pub fn native_cli_registration(&self) -> PluginResult<Option<crate::StaticCliRegistration>> {
        self.ensure_active()?;
        Ok(self
            .provider
            .as_ref()
            .and_then(|provider| provider.cli_registration()))
    }
    /// Resolve B4's portable CLI descriptor on this exact selected component.
    ///
    /// # Errors
    /// Returns admission, compatibility or decoding failure.
    pub async fn cli_descriptor(&self) -> PluginResult<Option<bcode_command::cli::CliDescriptor>> {
        let bytes = self
            .invoke(bcode_command::cli::CLI_DESCRIBE_CONTRACT, Vec::new())
            .await?;
        serde_json::from_slice(&bytes)
            .map_err(|_| PluginCommandError::failed("invalid CLI descriptor"))
    }
    /// Classify with the author's actual parser before daemon admission.
    ///
    /// # Errors
    /// Returns parsing, admission or protocol failure.
    pub async fn classify_cli(
        &self,
        request: &bcode_command::cli::CliArgvRequest,
    ) -> PluginResult<bcode_command::cli::CliClassification> {
        let bytes = self
            .invoke(
                bcode_command::cli::CLI_CLASSIFY_CONTRACT,
                encode_extension(request)?,
            )
            .await?;
        serde_json::from_slice(&bytes)
            .map_err(|_| PluginCommandError::failed("invalid CLI classification"))
    }
    /// Invoke structured-output CLI once, never as a model tool operation.
    ///
    /// # Errors
    /// Returns admission, parsing, invocation or protocol failure. Errors do not
    /// establish that no effects occurred and must not trigger automatic replay.
    pub async fn invoke_cli(
        &self,
        request: &bcode_command::cli::CliArgvRequest,
    ) -> PluginResult<bcode_command::cli::CliOutcome> {
        let bytes = self
            .invoke(
                bcode_command::cli::CLI_INVOKE_CONTRACT,
                encode_extension(request)?,
            )
            .await?;
        serde_json::from_slice(&bytes)
            .map_err(|_| PluginCommandError::failed("invalid CLI outcome"))
    }
    /// Construct a fresh selected frontend registry and retain its owner. Dynamic
    /// components must supply B3's negotiated M7 adapter; absence is an error.
    ///
    /// # Errors
    /// Returns owner, role, compatibility or adapter failures. Never substitutes
    /// a bundled factory for a dynamic selected component.
    #[cfg(feature = "tui")]
    pub async fn resolve_frontend(
        &self,
        portable: Option<&dyn PortableFrontendAdapter>,
    ) -> PluginResult<SelectedProductFrontend> {
        let descriptor = self.descriptor().await?;
        if descriptor.role != ComponentRole::Frontend {
            return Err(PluginCommandError::unavailable(
                "selected component is not a frontend",
            ));
        }
        let registry = if let Some(factory) = self
            .provider
            .as_ref()
            .and_then(|provider| provider.frontend_registry_factory())
        {
            factory()
        } else {
            portable
                .ok_or_else(|| {
                    PluginCommandError::unavailable(
                        "selected dynamic frontend requires portable adapter",
                    )
                })?
                .registry(self.clone(), &descriptor)?
        };
        self.ensure_active()?;
        Ok(SelectedProductFrontend {
            owner: self.clone(),
            descriptor,
            registry: Arc::new(registry),
        })
    }
}

/// B3's native adapter around M7 resource/ack/input/retained-replay/local-hit
///
/// operations. Each constructed surface/controller must retain `owner`; this
/// trait transports no `PaintCx`, frame or native registry layout across the ABI.
#[cfg(feature = "tui")]
pub trait PortableFrontendAdapter: Send + Sync {
    /// Negotiate required M7/product contracts and build fresh local proxies.
    ///
    /// # Errors
    /// Reject unsupported contracts; do not omit unsupported rich resources.
    fn registry(
        &self,
        owner: ProductExtensions,
        descriptor: &ProductExtensionDescriptor,
    ) -> PluginResult<crate::tui::PluginTuiRegistry>;
}

/// Frontend-local registry with retained selected owner/activation. B3 retains
/// this object for every surface/controller using its registry, not just loading.
#[cfg(feature = "tui")]
pub struct SelectedProductFrontend {
    owner: ProductExtensions,
    descriptor: ProductExtensionDescriptor,
    registry: Arc<crate::tui::PluginTuiRegistry>,
}
#[cfg(feature = "tui")]
impl SelectedProductFrontend {
    /// Observe the selected identity/contracts, not a replacement bundled plugin.
    #[must_use]
    pub const fn descriptor(&self) -> &ProductExtensionDescriptor {
        &self.descriptor
    }
    /// Read the registry while retaining this lease alongside created resources.
    ///
    /// # Errors
    /// Returns an error when the selected owner is closing or no longer active.
    pub fn registry(&self) -> PluginResult<&crate::tui::PluginTuiRegistry> {
        self.owner.ensure_active()?;
        Ok(&self.registry)
    }
    /// Retained owner for portable resource calls and per-surface admission.
    #[must_use]
    pub const fn owner(&self) -> &ProductExtensions {
        &self.owner
    }
}
