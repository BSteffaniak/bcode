//! Selected native CLI extension handles. Loader-owned handles retain activation
//! authority; discovery payloads cannot choose an instance or grant execution.
use bcode_command::cli::{
    CLI_CLASSIFY_CONTRACT, CLI_DESCRIBE_CONTRACT, CLI_INVOKE_CONTRACT, CliArgvRequest,
    CliClassification, CliDescriptor, CliOutcome,
};
use std::{future::Future, pin::Pin, sync::Arc};

/// Future returned by an activation-bound local extension call.
pub type CliExtensionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send + 'a>>;

/// Activation-bound extension transport implemented by the selected plugin host.
pub trait CliExtension: Send + Sync {
    /// Call once with bounded request/response bytes. Host enforces admission,
    /// cancellation, provenance and local-only authorization before invocation.
    fn call(&self, contract: &'static str, payload: Vec<u8>) -> CliExtensionFuture<'_>;
}

/// Verified selected owner and retained extension adapter, supplied by startup.
#[derive(Clone)]
pub struct SelectedCliExtension {
    /// Identity verified against the selected artifact's embedded declaration.
    pub plugin_id: String,
    /// Absolute source path resolved and identity-verified by the startup loader.
    /// Must match the explicit source override; never inferred from plugin id.
    pub source_path: std::path::PathBuf,
    /// Exact activation, not a lookup by untrusted request payload.
    pub extension: Arc<dyn CliExtension>,
    /// Bound imposed by the startup transport policy.
    pub max_response_bytes: usize,
}

/// Adapter retaining B2's selected shared owner; no callbacks cross the ABI.
struct ProductCliExtension(bcode_plugin_sdk::definition::ProductExtensions);

impl CliExtension for ProductCliExtension {
    fn call(&self, contract: &'static str, payload: Vec<u8>) -> CliExtensionFuture<'_> {
        Box::pin(async move {
            self.0
                .invoke(contract, payload)
                .await
                .map_err(|_| "selected CLI extension failed; effects may be unresolved".to_owned())
        })
    }
}

impl SelectedCliExtension {
    /// Attach B2's verified, already active selected owner to CLI dispatch.
    ///
    /// The loader must verify the source path against embedded identity before
    /// calling this function. This retains the exact owner, not a plugin-id lookup.
    ///
    /// # Errors
    /// Returns unavailable for an inactive or unsupported loaded component.
    pub fn from_loaded(
        plugin: &bcode_plugin::LoadedPlugin,
        source_path: std::path::PathBuf,
    ) -> Result<Self, bcode_plugin::PluginLoadError> {
        Ok(Self {
            plugin_id: plugin.manifest().id.clone(),
            source_path,
            extension: Arc::new(ProductCliExtension(plugin.product_extensions()?)),
            max_response_bytes: bcode_plugin_sdk::SERVICE_BRIDGE_MAX_RESPONSE_BYTES,
        })
    }

    async fn request<T: serde::de::DeserializeOwned>(
        &self,
        contract: &'static str,
        payload: Vec<u8>,
    ) -> Result<T, String> {
        let bytes = self.extension.call(contract, payload).await.map_err(|_| {
            "plugin CLI extension call failed; effects may be unresolved".to_owned()
        })?;
        if bytes.len() > self.max_response_bytes {
            return Err("plugin CLI response exceeds negotiated limit".to_owned());
        }
        serde_json::from_slice(&bytes).map_err(|_| "invalid plugin CLI response".to_owned())
    }

    /// Query inert grammar. No activation hooks may be rerun by this operation.
    ///
    /// # Errors
    /// Returns owner admission, transport, response-bound or descriptor decoding failures.
    pub async fn describe(&self) -> Result<Option<CliDescriptor>, String> {
        self.request(CLI_DESCRIBE_CONTRACT, Vec::new()).await
    }

    /// Parse/classify inside the author process without executing the command.
    ///
    /// # Errors
    /// Returns argument, owner admission, transport or response decoding failures.
    pub async fn classify(&self, request: &CliArgvRequest) -> Result<CliClassification, String> {
        let payload = serde_json::to_vec(request).map_err(|_| "invalid CLI request".to_owned())?;
        self.request(CLI_CLASSIFY_CONTRACT, payload).await
    }

    /// Invoke once; buffer growth must not reexecute the author handler.
    ///
    /// # Errors
    /// Returns admission, invocation, transport or response decoding failures.
    /// An error does not establish absence of effects; do not automatically retry.
    pub async fn invoke(&self, request: &CliArgvRequest) -> Result<CliOutcome, String> {
        let payload = serde_json::to_vec(request).map_err(|_| "invalid CLI request".to_owned())?;
        self.request(CLI_INVOKE_CONTRACT, payload).await
    }
}

/// Present a validated native command result only at the local CLI boundary.
///
/// # Errors
/// Refuses mixed output/terminal ownership and reports I/O or host-action failure.
pub async fn present_outcome(
    _plugin_id: &str,
    outcome: CliOutcome,
    max_output_bytes: usize,
) -> Result<(), crate::CliError> {
    use std::io::Write as _;
    let policy = bcode_command::cli::CliCommandPolicy {
        command_name: String::new(),
        requires_daemon: false,
        accepts_stdin: false,
        allows_host_action: true,
    };
    crate::portable_plugin_cli::validate_outcome(&policy, &outcome, max_output_bytes)
        .map_err(|error| crate::CliError::PluginCli(error.to_string()))?;
    if let Some(action) = outcome.host_action {
        match action {
            bcode_command::cli::CliHostAction::OpenTuiSurface {
                surface_kind,
                repo_path,
                options,
            } => {
                let repo_path = crate::resolve_surface_repo_path(repo_path)?;
                Box::pin(crate::run_plugin_surface_in_shell(
                    surface_kind,
                    repo_path,
                    options,
                ))
                .await?;
            }
            bcode_command::cli::CliHostAction::AttachSession { session_id } => {
                crate::ensure_server_running().await?;
                crate::attach_session(session_id).await?;
            }
        }
    } else {
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(&outcome.stdout)
            .and_then(|()| stdout.flush())
            .map_err(|_| crate::CliError::PluginCli("CLI stdout write failed".to_owned()))?;
        let mut stderr = std::io::stderr().lock();
        stderr
            .write_all(&outcome.stderr)
            .and_then(|()| stderr.flush())
            .map_err(|_| crate::CliError::PluginCli("CLI stderr write failed".to_owned()))?;
    }
    if outcome.exit_code != 0 {
        return Err(crate::CliError::PluginCliExit(outcome.exit_code));
    }
    Ok(())
}
