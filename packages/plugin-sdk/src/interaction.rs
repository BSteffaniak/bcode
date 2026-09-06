//! Plugin-contributed renderer-neutral interaction controllers.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::marker::PhantomData;

pub use bcode_tool::{
    InteractionControlId, InteractionController, InteractionInput, InteractionNavigation,
    InteractionOutput, InteractionValue,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Boxed error returned by interaction controller factories.
pub type PluginInteractionError = Box<dyn Error + Send + Sync>;

/// Boxed renderer-neutral interaction controller.
pub type BoxedPluginInteractionController = Box<dyn PluginInteractionController>;

/// High-level typed interaction contract for plugin authors.
pub trait PluginInteraction: Send + 'static {
    /// Stable interaction kind.
    const KIND: &'static str;

    /// Request payload used to initialize this interaction.
    type Request: DeserializeOwned;
    /// Renderer-neutral snapshot exposed to clients.
    type Snapshot: Serialize + Send;

    /// Create an interaction from a decoded request.
    fn new(request: Self::Request) -> Self;

    /// Return the current renderer-neutral snapshot.
    fn snapshot(&self) -> Self::Snapshot;

    /// Handle semantic input from any renderer/client.
    fn handle_input(&mut self, input: InteractionInput) -> InteractionOutput;
}

/// Advertised local adapter route for one opaque exchange schema/version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInteractionAdapterCapability {
    /// Plugin or adapter that owns the exchange schema.
    pub producer_id: String,
    /// Producer-owned exchange request schema.
    pub exchange_schema: String,
    /// Minimum supported exchange schema version, inclusive.
    pub min_schema_version: u32,
    /// Maximum supported exchange schema version, inclusive.
    pub max_schema_version: u32,
    /// Platform that owns and executes this adapter, such as `tui` or `web`.
    pub platform_id: String,
    /// Selection priority within one platform; larger values win.
    pub priority: u16,
    /// Renderer-neutral controller kind.
    pub interaction_kind: String,
    /// Optional native TUI surface kind.
    pub tui_surface_kind: Option<String>,
}

impl PluginInteractionAdapterCapability {
    /// Return whether this adapter supports an exchange envelope.
    #[must_use]
    pub fn supports(&self, schema: &str, schema_version: u32) -> bool {
        self.min_schema_version > 0
            && !self.producer_id.trim().is_empty()
            && !self.exchange_schema.trim().is_empty()
            && !self.platform_id.trim().is_empty()
            && !self.interaction_kind.trim().is_empty()
            && self.exchange_schema == schema
            && (self.min_schema_version..=self.max_schema_version).contains(&schema_version)
    }
}

/// Select the highest-priority adapter for one platform and opaque exchange envelope.
///
/// Equal priorities prefer the lexicographically first controller kind. Conflicting
/// capabilities at that winning rank fail closed; identical duplicates are harmless.
#[must_use]
pub fn select_interaction_adapter<'a>(
    adapters: &'a [PluginInteractionAdapterCapability],
    producer_id: &str,
    schema: &str,
    schema_version: u32,
    platform_id: &str,
) -> Option<&'a PluginInteractionAdapterCapability> {
    let matches = || {
        adapters.iter().filter(|adapter| {
            adapter.producer_id == producer_id
                && adapter.platform_id == platform_id
                && adapter.supports(schema, schema_version)
        })
    };
    let selected = matches().max_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| right.interaction_kind.cmp(&left.interaction_kind))
    })?;
    if matches().any(|candidate| {
        candidate.priority == selected.priority
            && candidate.interaction_kind == selected.interaction_kind
            && candidate != selected
    }) {
        return None;
    }
    Some(selected)
}

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
    /// Register a low-level controller factory.
    pub fn register_factory(&mut self, factory: Box<dyn PluginInteractionControllerFactory>) {
        self.factories
            .insert(factory.interaction_kind().to_owned(), factory);
    }

    /// Register a typed interaction with the default JSON adapter.
    pub fn register_interaction<T>(&mut self)
    where
        T: PluginInteraction,
    {
        self.register_factory(Box::new(TypedInteractionFactory::<T>::new()));
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
    /// Returns an error when no factory exists, initialization fails, or the returned
    /// controller kind differs from the registered kind. Factory error details are
    /// not exposed because they may contain sensitive request data.
    pub fn open(
        &self,
        kind: &str,
        request: Value,
    ) -> Result<BoxedPluginInteractionController, PluginInteractionRegistryError> {
        let factory = self
            .factories
            .get(kind)
            .ok_or_else(|| PluginInteractionRegistryError::UnsupportedKind(kind.to_owned()))?;
        let controller = factory.open(request).map_err(|_| {
            PluginInteractionRegistryError::OpenFailed(
                "controller initialization failed".to_owned(),
            )
        })?;
        if controller.kind() != kind {
            return Err(PluginInteractionRegistryError::OpenFailed(
                "controller kind does not match the registered factory".to_owned(),
            ));
        }
        Ok(controller)
    }
}

/// Default factory for typed plugin interactions.
pub struct TypedInteractionFactory<T> {
    marker: PhantomData<fn() -> T>,
}

impl<T> TypedInteractionFactory<T> {
    /// Create a typed interaction factory.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            marker: PhantomData,
        }
    }
}

impl<T> Default for TypedInteractionFactory<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> PluginInteractionControllerFactory for TypedInteractionFactory<T>
where
    T: PluginInteraction,
{
    fn interaction_kind(&self) -> &'static str {
        T::KIND
    }

    fn open(
        &self,
        request: Value,
    ) -> Result<BoxedPluginInteractionController, PluginInteractionError> {
        let request = serde_json::from_value::<T::Request>(request)?;
        Ok(Box::new(TypedInteractionController::new(T::new(request))))
    }
}

/// Default JSON controller adapter for typed plugin interactions.
pub struct TypedInteractionController<T> {
    inner: T,
}

impl<T> TypedInteractionController<T> {
    /// Create a typed controller adapter.
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

impl<T> PluginInteractionController for TypedInteractionController<T>
where
    T: PluginInteraction,
{
    fn kind(&self) -> &'static str {
        T::KIND
    }

    fn snapshot_json(&self) -> Value {
        serde_json::to_value(self.inner.snapshot()).unwrap_or(Value::Null)
    }

    fn handle_input(&mut self, input: InteractionInput) -> InteractionOutput {
        self.inner.handle_input(input)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_does_not_expose_factory_error_details() {
        struct Factory;
        impl PluginInteractionControllerFactory for Factory {
            fn interaction_kind(&self) -> &'static str {
                "example"
            }
            fn open(
                &self,
                _: Value,
            ) -> Result<BoxedPluginInteractionController, PluginInteractionError> {
                Err(std::io::Error::other("secret-token-from-request").into())
            }
        }
        let mut registry = PluginInteractionRegistry::default();
        registry.register_factory(Box::new(Factory));
        let Err(error) = registry.open("example", Value::Null) else {
            panic!("initialization must fail");
        };
        assert_eq!(
            error,
            PluginInteractionRegistryError::OpenFailed(
                "controller initialization failed".to_owned()
            )
        );
        assert!(!error.to_string().contains("secret-token"));
        assert!(!format!("{error:?}").contains("secret-token"));
    }

    #[test]
    fn registry_rejects_factory_controller_kind_mismatch() {
        struct WrongController;
        impl PluginInteractionController for WrongController {
            fn kind(&self) -> &'static str {
                "wrong"
            }
            fn snapshot_json(&self) -> Value {
                Value::Null
            }
            fn handle_input(&mut self, _: InteractionInput) -> InteractionOutput {
                panic!("mismatched controller must never receive input")
            }
        }
        struct Factory;
        impl PluginInteractionControllerFactory for Factory {
            fn interaction_kind(&self) -> &'static str {
                "expected"
            }
            fn open(
                &self,
                _: Value,
            ) -> Result<BoxedPluginInteractionController, PluginInteractionError> {
                Ok(Box::new(WrongController))
            }
        }
        let mut registry = PluginInteractionRegistry::default();
        registry.register_factory(Box::new(Factory));
        let result = registry.open("expected", Value::Null);
        assert!(
            matches!(result, Err(PluginInteractionRegistryError::OpenFailed(message))
            if message == "controller kind does not match the registered factory")
        );
    }

    fn adapter(
        kind: &str,
        min_schema_version: u32,
        max_schema_version: u32,
        platform_id: &str,
        priority: u16,
    ) -> PluginInteractionAdapterCapability {
        PluginInteractionAdapterCapability {
            producer_id: "example.plugin".to_owned(),
            exchange_schema: "example.request".to_owned(),
            min_schema_version,
            max_schema_version,
            platform_id: platform_id.to_owned(),
            priority,
            interaction_kind: kind.to_owned(),
            tui_surface_kind: None,
        }
    }

    #[test]
    fn adapter_selection_matches_platform_and_version_range_then_priority() {
        let adapters = vec![
            adapter("lower", 1, 3, "tui", 10),
            adapter("web", 1, 3, "web", 100),
            adapter("higher", 2, 4, "tui", 20),
        ];
        let selected =
            select_interaction_adapter(&adapters, "example.plugin", "example.request", 3, "tui")
                .expect("matching adapter");

        assert_eq!(selected.interaction_kind, "higher");
        assert!(
            select_interaction_adapter(&adapters, "example.plugin", "example.request", 5, "tui")
                .is_none()
        );
    }

    #[test]
    fn adapter_selection_rejects_conflicting_winning_capabilities() {
        let first = adapter("controller", 1, 3, "web", 10);
        let mut conflicting = first.clone();
        conflicting.tui_surface_kind = Some("other".to_owned());
        for adapters in [
            vec![first.clone(), conflicting.clone()],
            vec![conflicting, first.clone()],
        ] {
            assert!(
                select_interaction_adapter(
                    &adapters,
                    "example.plugin",
                    "example.request",
                    2,
                    "web"
                )
                .is_none()
            );
        }
        let duplicates = [first.clone(), first];
        assert!(
            select_interaction_adapter(&duplicates, "example.plugin", "example.request", 2, "web")
                .is_some()
        );
    }

    #[test]
    fn adapter_selection_rejects_empty_route_identifiers() {
        for empty in ["", " \t\n"] {
            for field in 0..4 {
                let mut malformed = adapter("controller", 1, 1, "web", 100);
                match field {
                    0 => malformed.producer_id = empty.to_owned(),
                    1 => malformed.exchange_schema = empty.to_owned(),
                    2 => malformed.platform_id = empty.to_owned(),
                    _ => malformed.interaction_kind = empty.to_owned(),
                }
                let adapters = [malformed];
                let candidate = &adapters[0];
                assert!(
                    select_interaction_adapter(
                        &adapters,
                        &candidate.producer_id,
                        &candidate.exchange_schema,
                        1,
                        &candidate.platform_id
                    )
                    .is_none()
                );
            }
        }
    }

    #[test]
    fn adapter_selection_rejects_invalid_version_ranges() {
        for (min, max) in [(0, 0), (0, 3), (3, 2)] {
            let adapters = [adapter("invalid", min, max, "web", 100)];
            for version in [0, 1, 2, 3, u32::MAX] {
                assert!(
                    select_interaction_adapter(
                        &adapters,
                        "example.plugin",
                        "example.request",
                        version,
                        "web"
                    )
                    .is_none()
                );
            }
        }
        let valid = adapter("valid", 1, 3, "web", 10);
        assert!(valid.supports("example.request", 1));
        assert!(valid.supports("example.request", 3));
        assert!(!valid.supports("example.request", 0));
        assert!(!valid.supports("example.request", 4));
    }

    #[test]
    fn adapter_selection_is_deterministic_for_equal_priority() {
        let adapters = vec![
            adapter("zeta", 1, 1, "web", 10),
            adapter("alpha", 1, 1, "web", 10),
        ];
        let selected =
            select_interaction_adapter(&adapters, "example.plugin", "example.request", 1, "web")
                .expect("matching adapter");

        assert_eq!(selected.interaction_kind, "alpha");
    }
}
