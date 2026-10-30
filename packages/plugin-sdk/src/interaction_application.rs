//! Public caller-bound, transport-independent permission and interaction access.
//!
//! The owning application authenticates callers, owns pending state, and consumes
//! plugin-produced semantic effects. Discovery and snapshots confer no authority.

/// Session-scoped semantic consumer decorator; discovery never filters away foreign results.
///
/// Exchange-only ephemeral methods are unsupported because they carry no session binding.
pub struct FilteredInteractionApplication<A> {
    inner: A,
    session_id: SessionId,
}

impl<A> FilteredInteractionApplication<A> {
    /// Bind an application boundary to one explicitly authorized session.
    #[must_use]
    pub const fn new(inner: A, session_id: SessionId) -> Self {
        Self { inner, session_id }
    }

    fn check_binding(
        &self,
        binding: &DurableInteractionBinding,
    ) -> Result<(), InteractionControllerError> {
        if binding.session_id != self.session_id.to_string() {
            return Err(InteractionControllerError::Unauthorized);
        }
        binding.validate()
    }
}

impl<A: InteractionApplication> InteractionApplication for FilteredInteractionApplication<A> {
    async fn pending_exchanges(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<PendingToolExchangeSummary>, InteractionControllerError> {
        if session_id != self.session_id {
            return Err(InteractionControllerError::Unauthorized);
        }
        let exchanges = self.inner.pending_exchanges(session_id).await?;
        if exchanges
            .iter()
            .any(|exchange| exchange.session_id != session_id)
        {
            return Err(InteractionControllerError::Unauthorized);
        }
        Ok(exchanges)
    }

    async fn pending_permissions(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<PermissionSummary>, InteractionControllerError> {
        if session_id != self.session_id {
            return Err(InteractionControllerError::Unauthorized);
        }
        let permissions = self.inner.pending_permissions(session_id).await?;
        if permissions
            .iter()
            .any(|permission| permission.session_id != session_id)
        {
            return Err(InteractionControllerError::Unauthorized);
        }
        Ok(permissions)
    }

    async fn resolve_permission(
        &self,
        session_id: SessionId,
        permission_id: &str,
        approved: bool,
        remember: bool,
    ) -> Result<bool, InteractionControllerError> {
        if session_id != self.session_id {
            return Err(InteractionControllerError::Unauthorized);
        }
        self.inner
            .resolve_permission(session_id, permission_id, approved, remember)
            .await
    }

    fn open_controller(
        &self,
        _version: u32,
        _exchange_id: &str,
    ) -> impl std::future::Future<
        Output = Result<InteractionControllerSnapshot, InteractionControllerError>,
    > {
        std::future::ready(Err(InteractionControllerError::Unsupported))
    }
    fn controller_snapshot(
        &self,
        _exchange_id: &str,
    ) -> impl std::future::Future<
        Output = Result<InteractionControllerSnapshot, InteractionControllerError>,
    > {
        std::future::ready(Err(InteractionControllerError::Unsupported))
    }
    fn controller_input(
        &self,
        _input: InteractionControllerInput,
    ) -> impl std::future::Future<
        Output = Result<InteractionControllerSnapshot, InteractionControllerError>,
    > {
        std::future::ready(Err(InteractionControllerError::Unsupported))
    }
    fn resolve_exchange(
        &self,
        _exchange_id: &str,
        _resolution: ToolExchangeResolution,
    ) -> impl std::future::Future<Output = Result<bool, InteractionControllerError>> {
        std::future::ready(Err(InteractionControllerError::Unsupported))
    }

    async fn durable_controllers(
        &self,
        session_id: String,
        query: DurableInteractionQuery,
    ) -> Result<DurableInteractionPage, InteractionControllerError> {
        if session_id != self.session_id.to_string() {
            return Err(InteractionControllerError::Unauthorized);
        }
        query.validate()?;
        let page = self
            .inner
            .durable_controllers(session_id.clone(), query.clone())
            .await?;
        page.validate(&session_id, &query)?;
        Ok(page)
    }

    async fn open_durable_controller(
        &self,
        version: u32,
        binding: DurableInteractionBinding,
    ) -> Result<DurableInteractionView, InteractionControllerError> {
        self.check_binding(&binding)?;
        let view = self
            .inner
            .open_durable_controller(version, binding.clone())
            .await?;
        view.validate()?;
        if view.binding != binding {
            return Err(InteractionControllerError::Conflict);
        }
        Ok(view)
    }

    async fn durable_controller_view(
        &self,
        version: u32,
        binding: DurableInteractionBinding,
    ) -> Result<DurableInteractionView, InteractionControllerError> {
        self.check_binding(&binding)?;
        let view = self
            .inner
            .durable_controller_view(version, binding.clone())
            .await?;
        view.validate()?;
        if view.binding != binding {
            return Err(InteractionControllerError::Conflict);
        }
        Ok(view)
    }

    async fn durable_controller_input(
        &self,
        input: DurableInteractionInput,
    ) -> Result<DurableInteractionReceipt, InteractionControllerError> {
        self.check_binding(&input.binding)?;
        input.validate()?;
        let receipt = self.inner.durable_controller_input(input.clone()).await?;
        receipt.validate()?;
        if receipt.input != input {
            return Err(InteractionControllerError::Conflict);
        }
        Ok(receipt)
    }
}

pub use bcode_plugin_models::interaction_controller::{
    DURABLE_INTERACTION_VERSION, DurableInteractionBinding, DurableInteractionInput,
    DurableInteractionOperation, DurableInteractionPage, DurableInteractionQuery,
    DurableInteractionReceipt, DurableInteractionView, INTERACTION_CONTROLLER_VERSION,
    InteractionControllerError, InteractionControllerInput, InteractionControllerSnapshot,
    validate_durable_size,
};
use bcode_session_models::{PendingToolExchangeSummary, PermissionSummary, SessionId};
use bcode_tool::ToolExchangeResolution;

/// Public Rust interaction boundary for finite, bounded operations.
///
/// Ephemeral controller methods do not promise durable reconnect. Separate durable methods
/// require an opted-in producer and canonical commit before acknowledgement.
#[allow(async_fn_in_trait)]
pub trait InteractionApplication: Send + Sync {
    /// List pending exchanges in one selected session.
    ///
    /// # Errors
    /// Rejects unauthorized callers and incompatible/unavailable session locations.
    async fn pending_exchanges(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<PendingToolExchangeSummary>, InteractionControllerError>;
    /// List pending permissions in one selected session.
    ///
    /// # Errors
    /// Rejects unauthorized callers and incompatible/unavailable session locations.
    async fn pending_permissions(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<PermissionSummary>, InteractionControllerError>;
    /// Resolve a canonical permission; plugins cannot grant permissions through effects.
    ///
    /// # Errors
    /// Rejects caller/session mismatch; false means no longer pending.
    async fn resolve_permission(
        &self,
        session_id: SessionId,
        permission_id: &str,
        approved: bool,
        remember: bool,
    ) -> Result<bool, InteractionControllerError>;
    /// Open the exact pending exchange's dynamically discovered controller.
    ///
    /// # Errors
    /// Rejects unauthorized consumers, future/unsupported contracts, bounds and plugin failures.
    async fn open_controller(
        &self,
        version: u32,
        exchange_id: &str,
    ) -> Result<InteractionControllerSnapshot, InteractionControllerError>;
    /// Read current renderer-neutral semantics without canonical replay or mutation.
    ///
    /// # Errors
    /// Rejects missing/terminal exchanges and unauthorized consumers.
    async fn controller_snapshot(
        &self,
        exchange_id: &str,
    ) -> Result<InteractionControllerSnapshot, InteractionControllerError>;
    /// Apply semantic input and canonically consume its effects with revision fencing.
    ///
    /// # Errors
    /// Rejects stale/conflicting input, unauthorized consumers, unsupported contracts, bounds and plugin failures.
    async fn controller_input(
        &self,
        input: InteractionControllerInput,
    ) -> Result<InteractionControllerSnapshot, InteractionControllerError>;
    /// Discover bounded canonical durable views in one authorized session.
    ///
    /// # Errors
    /// Defaults to `Unsupported`; rejects unauthorized scope, invalid bounds, stale generations
    /// and malformed responses without inferring bindings or truncating results.
    async fn durable_controllers(
        &self,
        _session_id: String,
        _query: DurableInteractionQuery,
    ) -> Result<DurableInteractionPage, InteractionControllerError> {
        Err(InteractionControllerError::Unsupported)
    }

    /// Open or restore an explicitly opted-in, canonically committed durable interaction.
    ///
    /// Attachment authorizes observation/input, not execution lifetime or tool replay.
    ///
    /// # Errors
    /// Defaults to `Unsupported`; hosts reject unauthorized bindings, incompatible versions,
    /// oversized values and missing canonical readiness rather than falling back to v1.
    async fn open_durable_controller(
        &self,
        _version: u32,
        _binding: DurableInteractionBinding,
    ) -> Result<DurableInteractionView, InteractionControllerError> {
        Err(InteractionControllerError::Unsupported)
    }

    /// Read the current committed durable view, including terminal state when retained.
    ///
    /// # Errors
    /// Defaults to `Unsupported`; rejects identity/compatibility/authorization failures.
    async fn durable_controller_view(
        &self,
        _version: u32,
        _binding: DurableInteractionBinding,
    ) -> Result<DurableInteractionView, InteractionControllerError> {
        Err(InteractionControllerError::Unsupported)
    }

    /// Commit input before acknowledgement. Exact retries, including after terminal
    /// resolution, return the original receipt; conflicting operation reuse fails.
    ///
    /// # Errors
    /// Defaults to `Unsupported`; rejects stale predecessors, conflicting retries,
    /// terminal new operations, unavailable producers and uncommitted transitions.
    async fn durable_controller_input(
        &self,
        _input: DurableInteractionInput,
    ) -> Result<DurableInteractionReceipt, InteractionControllerError> {
        Err(InteractionControllerError::Unsupported)
    }

    /// Submit an opaque schema-aware resolution via the generic fallback.
    ///
    /// # Errors
    /// Rejects incompatible consumers, host-only outcomes and encoded size overflow.
    async fn resolve_exchange(
        &self,
        exchange_id: &str,
        resolution: ToolExchangeResolution,
    ) -> Result<bool, InteractionControllerError>;
}

#[cfg(test)]
mod discovery_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Application {
        page: DurableInteractionPage,
        calls: AtomicUsize,
    }

    impl InteractionApplication for &Application {
        fn pending_exchanges(
            &self,
            _: SessionId,
        ) -> impl std::future::Future<
            Output = Result<Vec<PendingToolExchangeSummary>, InteractionControllerError>,
        > {
            std::future::ready(Err(InteractionControllerError::Unsupported))
        }
        fn pending_permissions(
            &self,
            _: SessionId,
        ) -> impl std::future::Future<Output = Result<Vec<PermissionSummary>, InteractionControllerError>>
        {
            std::future::ready(Err(InteractionControllerError::Unsupported))
        }
        fn resolve_permission(
            &self,
            _: SessionId,
            _: &str,
            _: bool,
            _: bool,
        ) -> impl std::future::Future<Output = Result<bool, InteractionControllerError>> {
            std::future::ready(Err(InteractionControllerError::Unsupported))
        }
        fn open_controller(
            &self,
            _: u32,
            _: &str,
        ) -> impl std::future::Future<
            Output = Result<InteractionControllerSnapshot, InteractionControllerError>,
        > {
            std::future::ready(Err(InteractionControllerError::Unsupported))
        }
        fn controller_snapshot(
            &self,
            _: &str,
        ) -> impl std::future::Future<
            Output = Result<InteractionControllerSnapshot, InteractionControllerError>,
        > {
            std::future::ready(Err(InteractionControllerError::Unsupported))
        }
        fn controller_input(
            &self,
            _: InteractionControllerInput,
        ) -> impl std::future::Future<
            Output = Result<InteractionControllerSnapshot, InteractionControllerError>,
        > {
            std::future::ready(Err(InteractionControllerError::Unsupported))
        }
        fn resolve_exchange(
            &self,
            _: &str,
            _: ToolExchangeResolution,
        ) -> impl std::future::Future<Output = Result<bool, InteractionControllerError>> {
            std::future::ready(Err(InteractionControllerError::Unsupported))
        }
        fn durable_controllers(
            &self,
            _: String,
            _: DurableInteractionQuery,
        ) -> impl std::future::Future<Output = Result<DurableInteractionPage, InteractionControllerError>>
        {
            self.calls.fetch_add(1, Ordering::Relaxed);
            std::future::ready(Ok(self.page.clone()))
        }
    }

    fn ready<T>(future: impl std::future::Future<Output = T>) -> T {
        let mut future = std::pin::pin!(future);
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(value) => value,
            std::task::Poll::Pending => panic!("test application must complete synchronously"),
        }
    }

    fn assert_send<T: Send>(_: &T) {}

    #[test]
    fn unsupported_filtered_methods_return_send_ready_futures() {
        let application = Application {
            page: DurableInteractionPage {
                generation: 0,
                views: Vec::new(),
                next_after_event_sequence: None,
            },
            calls: AtomicUsize::new(0),
        };
        let filtered = FilteredInteractionApplication::new(&application, SessionId::new());
        let open = filtered.open_controller(1, "exchange");
        assert_send(&open);
        assert_eq!(ready(open), Err(InteractionControllerError::Unsupported));
        let snapshot = filtered.controller_snapshot("exchange");
        assert_send(&snapshot);
        assert_eq!(
            ready(snapshot),
            Err(InteractionControllerError::Unsupported)
        );
        let input = filtered.controller_input(InteractionControllerInput {
            version: 1,
            exchange_id: "exchange".to_owned(),
            expected_revision: 0,
            input: bcode_plugin_models::interaction_controller::InteractionInput::Submit,
        });
        assert_send(&input);
        assert_eq!(ready(input), Err(InteractionControllerError::Unsupported));
        let resolve = filtered.resolve_exchange("exchange", ToolExchangeResolution::Cancelled);
        assert_send(&resolve);
        assert_eq!(ready(resolve), Err(InteractionControllerError::Unsupported));
        assert_eq!(application.calls.load(Ordering::Relaxed), 0);
    }

    fn view(session_id: &str) -> DurableInteractionView {
        serde_json::from_value(serde_json::json!({
            "version":1,"binding":{"session_id":session_id,"turn_id":"turn","invocation_id":"call",
            "request_sequence":1,"exchange_id":"exchange","tool_name":"tool","request_digest":"a".repeat(64),
            "exchange_schema_version":1,"capability":{"route":{"producer_id":"producer","exchange_schema":"request",
            "min_schema_version":1,"max_schema_version":1,"interaction_kind":"kind"},"checkpoint_schema":"checkpoint",
            "checkpoint_version":1,"continuation_schema":"complete","continuation_version":1}},"revision":2,
            "snapshot":{"version":1,"exchange_id":"exchange","route":{"producer_id":"producer","exchange_schema":"request",
            "min_schema_version":1,"max_schema_version":1,"interaction_kind":"kind"},"schema_version":1,"revision":2,
            "snapshot":{},"output":{"type":"none"}}
        })).unwrap()
    }

    #[test]
    fn filtered_discovery_enforces_scope_and_bounds_before_and_after_dispatch() {
        let session_id = SessionId::new();
        let query = DurableInteractionQuery {
            limit: 1,
            after_event_sequence: None,
            expected_generation: Some(7),
        };
        let mut application = Application {
            page: DurableInteractionPage {
                generation: 7,
                views: vec![view(&session_id.to_string())],
                next_after_event_sequence: Some(3),
            },
            calls: AtomicUsize::new(0),
        };
        {
            let filtered = FilteredInteractionApplication::new(&application, session_id);
            assert_eq!(
                ready(filtered.durable_controllers("foreign".into(), query.clone())),
                Err(InteractionControllerError::Unauthorized)
            );
            for limit in [0, 101] {
                assert_eq!(
                    ready(filtered.durable_controllers(
                        session_id.to_string(),
                        DurableInteractionQuery {
                            limit,
                            ..query.clone()
                        }
                    )),
                    Err(InteractionControllerError::LimitExceeded)
                );
            }
            assert_eq!(application.calls.load(Ordering::Relaxed), 0);
            assert_eq!(
                ready(filtered.durable_controllers(session_id.to_string(), query.clone())),
                Ok(application.page.clone())
            );
        }
        application.page.views[0].binding.session_id = SessionId::new().to_string();
        assert_eq!(
            ready(
                FilteredInteractionApplication::new(&application, session_id)
                    .durable_controllers(session_id.to_string(), query.clone())
            ),
            Err(InteractionControllerError::Unauthorized)
        );
        application.page.views[0] = view(&session_id.to_string());
        application.page.views[0].binding.request_digest.clear();
        assert_eq!(
            ready(
                FilteredInteractionApplication::new(&application, session_id)
                    .durable_controllers(session_id.to_string(), query.clone())
            ),
            Err(InteractionControllerError::InvalidContract)
        );
        application.page.views[0] = view(&session_id.to_string());
        application.page.views.push(view(&session_id.to_string()));
        assert_eq!(
            ready(
                FilteredInteractionApplication::new(&application, session_id)
                    .durable_controllers(session_id.to_string(), query.clone())
            ),
            Err(InteractionControllerError::LimitExceeded)
        );
        let _ = application.page.views.pop();
        application.page.views[0].snapshot.snapshot = serde_json::json!(
            "x".repeat(bcode_plugin_models::interaction_controller::MAX_CONTROLLER_BYTES)
        );
        assert_eq!(
            ready(
                FilteredInteractionApplication::new(&application, session_id)
                    .durable_controllers(session_id.to_string(), query)
            ),
            Err(InteractionControllerError::LimitExceeded)
        );
    }
}
