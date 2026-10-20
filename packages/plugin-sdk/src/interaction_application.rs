//! Public caller-bound, transport-independent permission and interaction access.
//!
//! The owning application authenticates callers, owns pending state, and consumes
//! plugin-produced semantic effects. Discovery and snapshots confer no authority.

pub use bcode_plugin_models::interaction_controller::{
    InteractionControllerError, InteractionControllerInput, InteractionControllerSnapshot,
};
use bcode_session_models::{PendingToolExchangeSummary, PermissionSummary, SessionId};
use bcode_tool::ToolExchangeResolution;

/// Public Rust interaction boundary for finite, bounded operations. Snapshots do
/// not promise durable reconnect or controller-state persistence.
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
