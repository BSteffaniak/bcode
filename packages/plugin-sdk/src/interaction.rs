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
///
/// Display and debug diagnostics omit payloads, which may contain untrusted data.
#[derive(Clone, PartialEq, Eq)]
pub enum PluginInteractionRegistryError {
    /// No factory is registered for this interaction kind.
    UnsupportedKind(String),
    /// Factory failed to open a controller.
    OpenFailed(String),
    /// A factory advertised a blank interaction kind.
    InvalidKind,
    /// More than one factory advertised the same interaction kind.
    ConflictingFactories,
}

impl fmt::Debug for PluginInteractionRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnsupportedKind(_) => "UnsupportedKind",
            Self::OpenFailed(_) => "OpenFailed",
            Self::InvalidKind => "InvalidKind",
            Self::ConflictingFactories => "ConflictingFactories",
        })
    }
}

impl fmt::Display for PluginInteractionRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedKind(_) => formatter.write_str("unsupported interaction kind"),
            Self::InvalidKind => formatter.write_str("invalid interaction kind"),
            Self::ConflictingFactories => formatter.write_str("conflicting controller factories"),
            Self::OpenFailed(_) => formatter.write_str("failed to open interaction"),
        }
    }
}

impl Error for PluginInteractionRegistryError {}

/// Snapshot serialization failed. Details are withheld because they may contain secrets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PluginInteractionSnapshotError;

impl fmt::Display for PluginInteractionSnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("interaction snapshot serialization failed")
    }
}

impl Error for PluginInteractionSnapshotError {}

/// Renderer-neutral plugin interaction controller using JSON snapshots.
pub trait PluginInteractionController: Send {
    /// Stable interaction kind.
    fn kind(&self) -> &'static str;

    /// Return the current domain snapshot as JSON.
    ///
    /// # Errors
    ///
    /// Returns a normalized error if the snapshot cannot be serialized.
    fn snapshot_json(&self) -> Result<Value, PluginInteractionSnapshotError>;

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
    factories: BTreeMap<String, Option<Box<dyn PluginInteractionControllerFactory>>>,
}

impl fmt::Debug for PluginInteractionRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PluginInteractionRegistry")
            .field("registered_kind_count", &self.factories.len())
            .finish()
    }
}

impl PluginInteractionRegistry {
    /// Register a low-level controller factory.
    ///
    /// Duplicate kinds are permanently ambiguous for this registry and cannot be opened.
    /// Empty or whitespace-only kinds are invalid and are not registered.
    pub fn register_factory(&mut self, factory: Box<dyn PluginInteractionControllerFactory>) {
        let _ = self.try_register_factory(factory);
    }

    /// Register a factory and report invalid or conflicting registration.
    ///
    /// Conflicts disable the kind, including the previously registered factory.
    ///
    /// # Errors
    ///
    /// Returns [`PluginInteractionRegistryError::InvalidKind`] for blank kinds or
    /// [`PluginInteractionRegistryError::ConflictingFactories`] for duplicate kinds.
    pub fn try_register_factory(
        &mut self,
        factory: Box<dyn PluginInteractionControllerFactory>,
    ) -> Result<(), PluginInteractionRegistryError> {
        let kind = factory.interaction_kind();
        if kind.trim().is_empty() {
            return Err(PluginInteractionRegistryError::InvalidKind);
        }
        match self.factories.entry(kind.to_owned()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(Some(factory));
                Ok(())
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                entry.insert(None);
                Err(PluginInteractionRegistryError::ConflictingFactories)
            }
        }
    }

    /// Register a typed interaction with the default JSON adapter.
    pub fn register_interaction<T>(&mut self)
    where
        T: PluginInteraction,
    {
        self.register_factory(Box::new(TypedInteractionFactory::<T>::new()));
    }

    /// Register a typed interaction and report invalid or conflicting registration.
    ///
    /// # Errors
    ///
    /// Returns [`PluginInteractionRegistryError::InvalidKind`] for blank kinds or
    /// [`PluginInteractionRegistryError::ConflictingFactories`] for duplicate kinds.
    pub fn try_register_interaction<T>(&mut self) -> Result<(), PluginInteractionRegistryError>
    where
        T: PluginInteraction,
    {
        self.try_register_factory(Box::new(TypedInteractionFactory::<T>::new()))
    }

    /// Return whether this registry supports `kind`.
    #[must_use]
    pub fn supports(&self, kind: &str) -> bool {
        self.factories.get(kind).is_some_and(Option::is_some)
    }

    /// Open a registered controller.
    ///
    /// # Errors
    ///
    /// Returns an error when no factory exists, registrations conflict, initialization fails, or the returned
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
        let factory = factory
            .as_ref()
            .ok_or(PluginInteractionRegistryError::ConflictingFactories)?;
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

    fn snapshot_json(&self) -> Result<Value, PluginInteractionSnapshotError> {
        serde_json::to_value(self.inner.snapshot()).map_err(|_| PluginInteractionSnapshotError)
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

    fn snapshot_json(&self) -> Result<Value, PluginInteractionSnapshotError> {
        serde_json::to_value(self.inner.snapshot()).map_err(|_| PluginInteractionSnapshotError)
    }

    fn handle_input(&mut self, input: InteractionInput) -> InteractionOutput {
        self.inner.handle_input(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_kind_display_does_not_echo_untrusted_input() {
        let registry = PluginInteractionRegistry::default();
        let kind = "private-token\n\u{1b}[2J";
        let Err(error) = registry.open(kind, Value::Null) else {
            panic!("unknown kind must not open")
        };
        assert_eq!(
            error,
            PluginInteractionRegistryError::UnsupportedKind(kind.to_owned())
        );
        assert_eq!(error.to_string(), "unsupported interaction kind");
    }

    #[test]
    fn registry_error_diagnostics_omit_untrusted_payloads() {
        let private = "private-token\n\u{1b}[2J";
        for (error, display, debug) in [
            (
                PluginInteractionRegistryError::UnsupportedKind(private.to_owned()),
                "unsupported interaction kind",
                "UnsupportedKind",
            ),
            (
                PluginInteractionRegistryError::OpenFailed(private.to_owned()),
                "failed to open interaction",
                "OpenFailed",
            ),
            (
                PluginInteractionRegistryError::InvalidKind,
                "invalid interaction kind",
                "InvalidKind",
            ),
            (
                PluginInteractionRegistryError::ConflictingFactories,
                "conflicting controller factories",
                "ConflictingFactories",
            ),
        ] {
            assert_eq!(error.to_string(), display);
            assert_eq!(format!("{error:?}"), debug);
            assert_eq!(format!("{error:#?}"), debug);
            assert!(error.source().is_none());
        }
    }

    #[test]
    fn typed_snapshot_failure_is_explicit_and_secret_safe() {
        struct Snapshot;
        impl Serialize for Snapshot {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("private snapshot content"))
            }
        }
        struct Interaction;
        impl PluginInteraction for Interaction {
            const KIND: &'static str = "failing-snapshot";
            type Request = ();
            type Snapshot = Snapshot;

            fn new((): ()) -> Self {
                Self
            }

            fn snapshot(&self) -> Snapshot {
                Snapshot
            }

            fn handle_input(&mut self, _: InteractionInput) -> InteractionOutput {
                panic!("snapshot access must not dispatch input")
            }
        }
        let mut registry = PluginInteractionRegistry::default();
        registry.try_register_interaction::<Interaction>().unwrap();
        let controller = registry.open(Interaction::KIND, Value::Null).unwrap();
        let error = controller.snapshot_json().unwrap_err();
        assert_eq!(error, PluginInteractionSnapshotError);
        assert_eq!(
            error.to_string(),
            "interaction snapshot serialization failed"
        );
        assert!(!format!("{error:?}").contains("private snapshot content"));
    }

    #[test]
    fn json_adapter_distinguishes_null_from_snapshot_failure() {
        struct Snapshot(bool);
        impl Serialize for Snapshot {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                if self.0 {
                    Err(serde::ser::Error::custom("private snapshot content"))
                } else {
                    serializer.serialize_unit()
                }
            }
        }
        struct Controller(bool);
        impl InteractionController for Controller {
            type Snapshot = Snapshot;

            fn kind(&self) -> &'static str {
                "example.snapshot"
            }

            fn snapshot(&self) -> Snapshot {
                Snapshot(self.0)
            }

            fn handle_input(&mut self, _: InteractionInput) -> InteractionOutput {
                panic!("snapshot access must not dispatch input")
            }
        }
        let mut controller = JsonInteractionController::new(Controller(false));
        assert_eq!(controller.snapshot_json(), Ok(Value::Null));
        controller.inner_mut().0 = true;
        let error = controller.snapshot_json().unwrap_err();
        assert_eq!(error, PluginInteractionSnapshotError);
        assert_eq!(
            error.to_string(),
            "interaction snapshot serialization failed"
        );
        assert!(!format!("{error:?}").contains("private snapshot content"));
        controller.inner_mut().0 = false;
        assert_eq!(controller.snapshot_json(), Ok(Value::Null));
    }

    #[test]
    fn registry_debug_omits_factory_kinds() {
        struct Factory;
        impl PluginInteractionControllerFactory for Factory {
            fn interaction_kind(&self) -> &'static str {
                "private-plugin-kind"
            }

            fn open(
                &self,
                _: Value,
            ) -> Result<BoxedPluginInteractionController, PluginInteractionError> {
                panic!("debug formatting must not open controllers")
            }
        }
        let mut registry = PluginInteractionRegistry::default();
        registry.try_register_factory(Box::new(Factory)).unwrap();
        assert_eq!(
            format!("{registry:?}"),
            "PluginInteractionRegistry { registered_kind_count: 1 }"
        );
        assert!(!format!("{registry:#?}").contains("private-plugin-kind"));
    }

    #[test]
    fn registry_does_not_register_blank_kinds() {
        struct Factory(&'static str);
        impl PluginInteractionControllerFactory for Factory {
            fn interaction_kind(&self) -> &'static str {
                self.0
            }
            fn open(
                &self,
                _: Value,
            ) -> Result<BoxedPluginInteractionController, PluginInteractionError> {
                panic!("invalid factory must never open")
            }
        }
        let mut registry = PluginInteractionRegistry::default();
        for kind in ["", " ", "\t\n", "\u{2003}"] {
            for _ in 0..2 {
                assert_eq!(
                    registry.try_register_factory(Box::new(Factory(kind))),
                    Err(PluginInteractionRegistryError::InvalidKind)
                );
                assert!(!registry.supports(kind));
                let Err(error) = registry.open(kind, Value::Null) else {
                    panic!("blank kind opened")
                };
                assert_eq!(
                    error,
                    PluginInteractionRegistryError::UnsupportedKind(kind.to_owned())
                );
            }
        }
    }

    #[test]
    fn registry_preserves_valid_kind_and_request_identity() {
        struct Controller(Value);
        impl PluginInteractionController for Controller {
            fn kind(&self) -> &'static str {
                " example "
            }
            fn snapshot_json(&self) -> Result<Value, PluginInteractionSnapshotError> {
                Ok(self.0.clone())
            }
            fn handle_input(&mut self, _: InteractionInput) -> InteractionOutput {
                panic!("no input expected")
            }
        }
        struct Factory;
        impl PluginInteractionControllerFactory for Factory {
            fn interaction_kind(&self) -> &'static str {
                " example "
            }
            fn open(
                &self,
                request: Value,
            ) -> Result<BoxedPluginInteractionController, PluginInteractionError> {
                Ok(Box::new(Controller(request)))
            }
        }
        let mut registry = PluginInteractionRegistry::default();
        registry.register_factory(Box::new(Factory));
        assert!(registry.supports(" example "));
        assert!(!registry.supports("example"));
        let request = serde_json::json!({"opaque": [null, 42, "original"]});
        let controller = registry.open(" example ", request.clone()).unwrap();
        assert_eq!(controller.kind(), " example ");
        assert_eq!(controller.snapshot_json(), Ok(request.clone()));
        let other_request = serde_json::json!({"opaque": ["different"]});
        let other = registry.open(" example ", other_request.clone()).unwrap();
        assert_eq!(other.snapshot_json(), Ok(other_request.clone()));
        assert_eq!(controller.snapshot_json(), Ok(request.clone()));
        drop(registry);
        assert_eq!(controller.snapshot_json(), Ok(request));
        assert_eq!(other.snapshot_json(), Ok(other_request));
    }

    #[test]
    fn registry_conflicting_factories_fail_closed_without_opening() {
        struct Factory(&'static str);
        impl PluginInteractionControllerFactory for Factory {
            fn interaction_kind(&self) -> &'static str {
                self.0
            }
            fn open(
                &self,
                _: Value,
            ) -> Result<BoxedPluginInteractionController, PluginInteractionError> {
                panic!("conflicting factory must not be invoked")
            }
        }
        let mut registry = PluginInteractionRegistry::default();
        registry.register_factory(Box::new(Factory("other")));
        registry
            .try_register_factory(Box::new(Factory("duplicate")))
            .unwrap();
        assert!(registry.supports("duplicate"));
        for _ in 0..3 {
            assert_eq!(
                registry.try_register_factory(Box::new(Factory("duplicate"))),
                Err(PluginInteractionRegistryError::ConflictingFactories)
            );
            assert!(!registry.supports("duplicate"));
            assert!(registry.supports("other"));
            let Err(error) = registry.open("duplicate", Value::Null) else {
                panic!("ambiguous registry opened")
            };
            assert_eq!(error, PluginInteractionRegistryError::ConflictingFactories);
        }
    }

    #[test]
    fn typed_factory_rejects_malformed_request_without_disabling_kind() {
        struct Interaction(u32);
        impl PluginInteraction for Interaction {
            const KIND: &'static str = "typed";
            type Request = u32;
            type Snapshot = u32;

            fn new(request: u32) -> Self {
                Self(request)
            }

            fn snapshot(&self) -> u32 {
                self.0
            }

            fn handle_input(&mut self, _: InteractionInput) -> InteractionOutput {
                panic!("request validation must not dispatch input")
            }
        }
        let mut registry = PluginInteractionRegistry::default();
        registry.try_register_interaction::<Interaction>().unwrap();
        for request in [
            Value::Null,
            serde_json::json!("private-request"),
            serde_json::json!(-1),
        ] {
            let Err(error) = registry.open("typed", request) else {
                panic!("malformed typed request opened")
            };
            assert_eq!(
                error,
                PluginInteractionRegistryError::OpenFailed(
                    "controller initialization failed".to_owned()
                )
            );
            assert!(registry.supports("typed"));
        }
        let controller = registry.open("typed", serde_json::json!(42)).unwrap();
        assert_eq!(controller.kind(), "typed");
        assert_eq!(controller.snapshot_json(), Ok(serde_json::json!(42)));
        assert_eq!(
            registry.try_register_interaction::<Interaction>(),
            Err(PluginInteractionRegistryError::ConflictingFactories)
        );
        assert!(!registry.supports("typed"));
    }

    #[test]
    fn registry_unknown_kind_does_not_invoke_registered_factory() {
        struct Factory;
        impl PluginInteractionControllerFactory for Factory {
            fn interaction_kind(&self) -> &'static str {
                "known"
            }

            fn open(
                &self,
                _: Value,
            ) -> Result<BoxedPluginInteractionController, PluginInteractionError> {
                panic!("an unknown kind must not invoke another factory")
            }
        }
        let mut registry = PluginInteractionRegistry::default();
        registry.try_register_factory(Box::new(Factory)).unwrap();
        for kind in ["unknown", "", " known", "known ", "KNOWN"] {
            assert!(!registry.supports(kind));
            let Err(error) = registry.open(kind, Value::Null) else {
                panic!("unknown controller kind opened")
            };
            assert_eq!(
                error,
                PluginInteractionRegistryError::UnsupportedKind(kind.to_owned())
            );
        }
        assert!(registry.supports("known"));
    }

    #[test]
    fn registry_releases_conflicting_factory_resources() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Factory(Arc<AtomicUsize>);
        impl Drop for Factory {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        impl PluginInteractionControllerFactory for Factory {
            fn interaction_kind(&self) -> &'static str {
                "resource-owner"
            }
            fn open(
                &self,
                _: Value,
            ) -> Result<BoxedPluginInteractionController, PluginInteractionError> {
                panic!("conflicting factory must not open")
            }
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut registry = PluginInteractionRegistry::default();
        registry.register_factory(Box::new(Factory(Arc::clone(&dropped))));
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        registry.register_factory(Box::new(Factory(Arc::clone(&dropped))));
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
        registry.register_factory(Box::new(Factory(Arc::clone(&dropped))));
        assert_eq!(dropped.load(Ordering::SeqCst), 3);
        assert!(!registry.supports("resource-owner"));
        drop(registry);
        assert_eq!(dropped.load(Ordering::SeqCst), 3);
        assert_eq!(Arc::strong_count(&dropped), 1);
    }

    #[test]
    fn registry_competing_factories_are_order_independent() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Factory(usize, Arc<AtomicUsize>);
        impl PluginInteractionControllerFactory for Factory {
            fn interaction_kind(&self) -> &'static str {
                "competing"
            }
            fn open(
                &self,
                _: Value,
            ) -> Result<BoxedPluginInteractionController, PluginInteractionError> {
                self.1.store(self.0, Ordering::SeqCst);
                Err(std::io::Error::other("factory invoked").into())
            }
        }
        for order in [[1, 2], [2, 1]] {
            let invoked = Arc::new(AtomicUsize::new(0));
            let mut registry = PluginInteractionRegistry::default();
            for identity in order {
                registry.register_factory(Box::new(Factory(identity, Arc::clone(&invoked))));
            }
            let Err(error) = registry.open("competing", Value::Null) else {
                panic!("ambiguous registry opened")
            };
            assert_eq!(error, PluginInteractionRegistryError::ConflictingFactories);
            assert_eq!(invoked.load(Ordering::SeqCst), 0);
            assert!(!registry.supports("competing"));
        }
    }

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
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        struct WrongController(Arc<AtomicUsize>);
        impl Drop for WrongController {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        impl PluginInteractionController for WrongController {
            fn kind(&self) -> &'static str {
                "wrong"
            }
            fn snapshot_json(&self) -> Result<Value, PluginInteractionSnapshotError> {
                panic!("mismatched controller must never expose a snapshot")
            }
            fn handle_input(&mut self, _: InteractionInput) -> InteractionOutput {
                panic!("mismatched controller must never receive input")
            }
        }
        struct Factory(Arc<AtomicUsize>);
        impl PluginInteractionControllerFactory for Factory {
            fn interaction_kind(&self) -> &'static str {
                "expected"
            }
            fn open(
                &self,
                _: Value,
            ) -> Result<BoxedPluginInteractionController, PluginInteractionError> {
                Ok(Box::new(WrongController(Arc::clone(&self.0))))
            }
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut registry = PluginInteractionRegistry::default();
        registry.register_factory(Box::new(Factory(Arc::clone(&dropped))));
        let result = registry.open("expected", Value::Null);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
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
    fn adapter_selection_ignores_conflicts_outside_winning_route() {
        let winner = adapter("winner", 1, 3, "web", 10);
        let lower = adapter("lower", 1, 3, "web", 1);
        let mut lower_conflict = lower.clone();
        lower_conflict.tui_surface_kind = Some("different".to_owned());
        let mut unrelated = adapter("unrelated", 1, 3, "web", 100);
        unrelated.producer_id = "other.plugin".to_owned();
        let mut unrelated_conflict = unrelated.clone();
        unrelated_conflict.tui_surface_kind = Some("different".to_owned());
        let mut adapters = vec![
            lower,
            unrelated,
            winner.clone(),
            lower_conflict,
            unrelated_conflict,
        ];
        for _ in 0..adapters.len() {
            assert_eq!(
                select_interaction_adapter(
                    &adapters,
                    "example.plugin",
                    "example.request",
                    2,
                    "web",
                ),
                Some(&winner)
            );
            adapters.rotate_left(1);
        }
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
        assert!(!valid.supports("example.request", u32::MAX));
        let full_range = adapter("full-range", 1, u32::MAX, "web", 10);
        assert!(full_range.supports("example.request", u32::MAX));
        assert!(!full_range.supports("example.request", 0));
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
