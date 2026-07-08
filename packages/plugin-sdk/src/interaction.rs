//! Plugin-contributed renderer-neutral interaction controllers.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

pub use bcode_tool::{
    InteractionControlId, InteractionController, InteractionInput, InteractionNavigation,
    InteractionOutput, InteractionValue,
};
use serde_json::Value;

/// Boxed error returned by interaction controller factories.
pub type PluginInteractionError = Box<dyn Error + Send + Sync>;

/// Boxed renderer-neutral interaction controller.
pub type BoxedPluginInteractionController = Box<dyn PluginInteractionController>;

/// Errors returned by plugin interaction registries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginInteractionRegistryError {
    /// No factory is registered for this interaction kind.
    UnsupportedKind(String),
    /// Factory failed to open a controller.
    OpenFailed(String),
}

impl fmt::Display for PluginInteractionRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedKind(kind) => {
                write!(formatter, "unsupported interaction kind: {kind}")
            }
            Self::OpenFailed(message) => write!(formatter, "failed to open interaction: {message}"),
        }
    }
}

impl Error for PluginInteractionRegistryError {}

/// Renderer-neutral plugin interaction controller using JSON snapshots.
pub trait PluginInteractionController: Send {
    /// Stable interaction kind.
    fn kind(&self) -> &'static str;

    /// Return the current domain snapshot as JSON.
    fn snapshot_json(&self) -> Value;

    /// Handle semantic input from any renderer/client.
    fn handle_input(&mut self, input: InteractionInput) -> InteractionOutput;
}

/// Factory for renderer-neutral interaction controllers.
pub trait PluginInteractionControllerFactory: Send + Sync {
    /// Stable interaction kind handled by this factory.
    fn interaction_kind(&self) -> &'static str;

    /// Open a controller from plugin-defined request JSON.
    ///
    /// # Errors
    ///
    /// Returns an error when the request cannot be decoded or initialized.
    fn open(
        &self,
        request: Value,
    ) -> Result<BoxedPluginInteractionController, PluginInteractionError>;
}

/// Registry of renderer-neutral interaction controller factories.
#[derive(Default)]
pub struct PluginInteractionRegistry {
    factories: BTreeMap<String, Box<dyn PluginInteractionControllerFactory>>,
}

impl fmt::Debug for PluginInteractionRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PluginInteractionRegistry")
            .field(
                "interaction_kinds",
                &self.factories.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl PluginInteractionRegistry {
    /// Register a controller factory.
    pub fn register_factory(&mut self, factory: Box<dyn PluginInteractionControllerFactory>) {
        self.factories
            .insert(factory.interaction_kind().to_owned(), factory);
    }

    /// Return whether this registry supports `kind`.
    #[must_use]
    pub fn supports(&self, kind: &str) -> bool {
        self.factories.contains_key(kind)
    }

    /// Open a registered controller.
    ///
    /// # Errors
    ///
    /// Returns an error when no factory exists or the factory fails to open the controller.
    pub fn open(
        &self,
        kind: &str,
        request: Value,
    ) -> Result<BoxedPluginInteractionController, PluginInteractionRegistryError> {
        let factory = self
            .factories
            .get(kind)
            .ok_or_else(|| PluginInteractionRegistryError::UnsupportedKind(kind.to_owned()))?;
        factory
            .open(request)
            .map_err(|error| PluginInteractionRegistryError::OpenFailed(error.to_string()))
    }
}

/// Adapter from a strongly typed [`InteractionController`] to JSON snapshots.
pub struct JsonInteractionController<T> {
    inner: T,
}

impl<T> JsonInteractionController<T> {
    /// Create a JSON adapter.
    #[must_use]
    pub const fn new(inner: T) -> Self {
        Self { inner }
    }

    /// Return the inner controller.
    #[must_use]
    pub const fn inner(&self) -> &T {
        &self.inner
    }

    /// Return the mutable inner controller.
    #[must_use]
    pub const fn inner_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T> PluginInteractionController for JsonInteractionController<T>
where
    T: InteractionController + Send,
{
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }

    fn snapshot_json(&self) -> Value {
        serde_json::to_value(self.inner.snapshot()).unwrap_or(Value::Null)
    }

    fn handle_input(&mut self, input: InteractionInput) -> InteractionOutput {
        self.inner.handle_input(input)
    }
}
