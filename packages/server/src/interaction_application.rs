//! Caller-bound interaction application access, independent of IPC and renderers.
//!
//! The owning host authenticates the caller before constructing this handle. Pending
//! exchange identity and selected storage location remain authoritative; plugins only
//! propose semantic effects, which this application consumes through canonical state.

use std::sync::Arc;

use bcode_plugin::interaction_controller::{
    ControllerInputPreparation, ControllerOpenPreparation, InteractionControllerError,
    InteractionControllerOpen,
};
use bcode_plugin_models::interaction_controller::{
    INTERACTION_CONTROLLER_VERSION, InteractionControllerInput, InteractionControllerSnapshot,
    InteractionOutput,
};
use bcode_session_models::{ClientId, PendingToolExchangeSummary, PermissionSummary, SessionId};

use crate::{ServerState, ToolExchangeResolution, interaction_operations, session_operations};

/// Application handle bound to an authenticated, registered consumer.
pub struct InteractionApplicationHandle<'a> {
    state: &'a Arc<ServerState>,
    client_id: ClientId,
}

impl<'a> InteractionApplicationHandle<'a> {
    /// Bind a caller after the host has authenticated and registered its context.
    #[must_use]
    pub const fn new(state: &'a Arc<ServerState>, client_id: ClientId) -> Self {
        Self { state, client_id }
    }

    async fn require_session(
        &self,
        session_id: SessionId,
    ) -> Result<(), InteractionControllerError> {
        if self
            .state
            .shutdown_requested
            .load(std::sync::atomic::Ordering::SeqCst)
            || !self
                .state
                .client_runtime_contexts
                .lock()
                .await
                .contains_key(&self.client_id)
        {
            return Err(InteractionControllerError::Unauthorized);
        }
        session_operations::require_selected_location(self.state, session_id)
            .await
            .map_err(|_| InteractionControllerError::Unavailable)?;
        if self
            .state
            .active_session_namespace_mismatch(session_id, self.client_id)
            .await
            .is_some()
        {
            return Err(InteractionControllerError::Unauthorized);
        }
        Ok(())
    }

    async fn pending_exchange(
        &self,
        exchange_id: &str,
    ) -> Result<PendingToolExchangeSummary, InteractionControllerError> {
        let pending = self
            .state
            .pending_tool_exchanges
            .lock()
            .await
            .get(exchange_id)
            .map(|exchange| exchange.summary.clone())
            .ok_or(InteractionControllerError::NotPending)?;
        self.require_session(pending.session_id).await?;
        // Semantic controller capability is explicit and versioned, never inferred
        // from renderer metadata. Raw resolution retains its exact schema admission.
        if !interaction_operations::client_supports_controller(self.state, self.client_id).await {
            return Err(InteractionControllerError::Unauthorized);
        }
        Ok(pending)
    }

    /// Return pending exchanges for one selected session, not a daemon-wide inventory.
    ///
    /// # Errors
    /// Rejects unavailable/incompatible session locations and unregistered callers.
    pub async fn pending_exchanges(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<PendingToolExchangeSummary>, InteractionControllerError> {
        self.require_session(session_id).await?;
        Ok(
            interaction_operations::list_pending_tool_exchanges(self.state)
                .await
                .into_iter()
                .filter(|pending| pending.session_id == session_id)
                .collect(),
        )
    }

    /// Return current pending permissions for one selected session.
    ///
    /// # Errors
    /// Rejects unavailable/incompatible session locations and unregistered callers.
    pub async fn pending_permissions(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<PermissionSummary>, InteractionControllerError> {
        self.require_session(session_id).await?;
        Ok(interaction_operations::list_permissions(self.state)
            .await
            .into_iter()
            .filter(|pending| pending.session_id == session_id)
            .collect())
    }

    /// Resolve a permission through its canonical owner. No plugin effect can grant a permission.
    ///
    /// # Errors
    /// Rejects caller/session mismatch; returns false when no longer pending.
    pub async fn resolve_permission(
        &self,
        session_id: SessionId,
        permission_id: &str,
        approved: bool,
        remember: bool,
    ) -> Result<bool, InteractionControllerError> {
        self.require_session(session_id).await?;
        let permissions = interaction_operations::list_permissions(self.state).await;
        let Some(permission) = permissions
            .iter()
            .find(|permission| permission.permission_id == permission_id)
        else {
            return Ok(false);
        };
        if permission.session_id != session_id {
            return Err(InteractionControllerError::Unauthorized);
        }
        Ok(interaction_operations::resolve_permission(
            self.state,
            permission_id,
            approved,
            remember,
        )
        .await)
    }

    /// Open a dynamically discovered plugin-owned semantic controller.
    ///
    /// # Errors
    /// Rejects future versions, unauthorized consumers, unsupported routes and bounds.
    pub async fn open_controller(
        &self,
        version: u32,
        exchange_id: &str,
    ) -> Result<InteractionControllerSnapshot, InteractionControllerError> {
        if version != INTERACTION_CONTROLLER_VERSION {
            return Err(InteractionControllerError::InvalidContract);
        }
        let preparation = {
            let mut controllers = self.state.interaction_controllers.lock().await;
            let pending = self.pending_exchange(exchange_id).await?;
            controllers.prepare_open(InteractionControllerOpen {
                owner: &self.client_id.to_string(),
                session_id: &pending.session_id.to_string(),
                exchange_id,
                producer: &pending.request.producer_id,
                schema: &pending.request.schema,
                schema_version: pending.request.schema_version,
                payload: &pending.request.payload,
            })?
        };
        match preparation {
            ControllerOpenPreparation::Existing(snapshot) => Ok(snapshot),
            ControllerOpenPreparation::Invoke(ticket) => {
                let result = ticket.invoke(&self.state.plugins).await;
                let mut controllers = self.state.interaction_controllers.lock().await;
                if let Err(error) = self.pending_exchange(exchange_id).await {
                    controllers.remove_exchange(exchange_id);
                    return Err(error);
                }
                controllers.commit_open(ticket, result)
            }
        }
    }

    /// Inspect a controller without replaying canonical history or interpreting plugin state.
    ///
    /// # Errors
    /// Rejects unauthorized consumers and terminal/missing exchanges.
    pub async fn controller_snapshot(
        &self,
        exchange_id: &str,
    ) -> Result<InteractionControllerSnapshot, InteractionControllerError> {
        let controllers = self.state.interaction_controllers.lock().await;
        self.pending_exchange(exchange_id).await?;
        controllers.snapshot(&self.client_id.to_string(), exchange_id)
    }

    /// Apply semantic input and consume plugin effects through the canonical pending exchange.
    ///
    /// # Errors
    /// Rejects stale/conflicting input, unavailable plugins, malformed effects and unauthorized consumers.
    pub async fn controller_input(
        &self,
        input: InteractionControllerInput,
    ) -> Result<InteractionControllerSnapshot, InteractionControllerError> {
        let preparation = {
            let controllers = self.state.interaction_controllers.lock().await;
            self.pending_exchange(&input.exchange_id).await?;
            controllers.prepare_input(&self.client_id.to_string(), &input)?
        };
        let (ticket, transition) = match preparation {
            ControllerInputPreparation::Existing(snapshot) => return Ok(snapshot),
            ControllerInputPreparation::Invoke(ticket) => {
                let transition = ticket.invoke(&self.state.plugins).await?;
                // Validate effects before committing a revision, so malformed plugin output
                // leaves the prior state untouched and cannot freeze the exchange terminal.
                controller_resolution(&transition.output)?;
                (ticket, transition)
            }
        };
        let mut controllers = self.state.interaction_controllers.lock().await;
        if let Err(error) = self.pending_exchange(&input.exchange_id).await {
            controllers.remove_exchange(&input.exchange_id);
            return Err(error);
        }
        let snapshot = controllers.commit_input(&ticket, transition)?;
        let resolution = controller_resolution(&snapshot.output)?;
        if let Some(resolution) = resolution {
            controllers.remove_exchange(&input.exchange_id);
            let completed = interaction_operations::complete_tool_exchange_under_controller_lock(
                self.state,
                &input.exchange_id,
                resolution,
            )
            .await;
            if !completed {
                return Err(InteractionControllerError::NotPending);
            }
        }
        drop(controllers);
        Ok(snapshot)
    }

    /// Submit an opaque schema-aware raw resolution when no semantic controller is available.
    ///
    /// # Errors
    /// Rejects host-only outcomes, oversized resolutions and incompatible consumers.
    pub async fn resolve_exchange(
        &self,
        exchange_id: &str,
        resolution: ToolExchangeResolution,
    ) -> Result<bool, InteractionControllerError> {
        let pending = self
            .state
            .pending_tool_exchanges
            .lock()
            .await
            .get(exchange_id)
            .map(|exchange| exchange.summary.clone())
            .ok_or(InteractionControllerError::NotPending)?;
        self.require_session(pending.session_id).await?;
        interaction_operations::resolve_tool_exchange(
            self.state,
            self.client_id,
            exchange_id,
            resolution,
        )
        .await
        .map_err(|_| InteractionControllerError::InvalidContract)
    }
}

impl bcode_plugin_sdk::interaction_application::InteractionApplication
    for InteractionApplicationHandle<'_>
{
    async fn pending_exchanges(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<PendingToolExchangeSummary>, InteractionControllerError> {
        InteractionApplicationHandle::pending_exchanges(self, session_id).await
    }
    async fn pending_permissions(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<PermissionSummary>, InteractionControllerError> {
        InteractionApplicationHandle::pending_permissions(self, session_id).await
    }
    async fn resolve_permission(
        &self,
        session_id: SessionId,
        permission_id: &str,
        approved: bool,
        remember: bool,
    ) -> Result<bool, InteractionControllerError> {
        InteractionApplicationHandle::resolve_permission(
            self,
            session_id,
            permission_id,
            approved,
            remember,
        )
        .await
    }
    async fn resolve_exchange(
        &self,
        exchange_id: &str,
        resolution: ToolExchangeResolution,
    ) -> Result<bool, InteractionControllerError> {
        InteractionApplicationHandle::resolve_exchange(self, exchange_id, resolution).await
    }
    async fn open_controller(
        &self,
        version: u32,
        exchange_id: &str,
    ) -> Result<InteractionControllerSnapshot, InteractionControllerError> {
        InteractionApplicationHandle::open_controller(self, version, exchange_id).await
    }
    async fn controller_snapshot(
        &self,
        exchange_id: &str,
    ) -> Result<InteractionControllerSnapshot, InteractionControllerError> {
        InteractionApplicationHandle::controller_snapshot(self, exchange_id).await
    }
    async fn controller_input(
        &self,
        input: InteractionControllerInput,
    ) -> Result<InteractionControllerSnapshot, InteractionControllerError> {
        InteractionApplicationHandle::controller_input(self, input).await
    }
}

fn controller_resolution(
    output: &InteractionOutput,
) -> Result<Option<ToolExchangeResolution>, InteractionControllerError> {
    let resolution = match output {
        InteractionOutput::None | InteractionOutput::Redraw => return Ok(None),
        InteractionOutput::Submitted { payload } => ToolExchangeResolution::Responded {
            payload: payload.clone(),
        },
        InteractionOutput::Cancelled => ToolExchangeResolution::Cancelled,
    };
    // Plugin output must obey canonical resolution bounds, not only snapshot bounds.
    interaction_operations::validate_controller_resolution(&resolution)
        .map_err(|_| InteractionControllerError::InvalidContract)?;
    Ok(Some(resolution))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn effects_are_consumed_without_interpreting_opaque_plugin_business_payloads() {
        let payload = json!({"plugin_owned": {"status":"dismissed"}});
        assert_eq!(
            controller_resolution(&InteractionOutput::Submitted {
                payload: payload.clone()
            }),
            Ok(Some(ToolExchangeResolution::Responded { payload }))
        );
        assert_eq!(
            controller_resolution(&InteractionOutput::Cancelled),
            Ok(Some(ToolExchangeResolution::Cancelled))
        );
        assert_eq!(controller_resolution(&InteractionOutput::Redraw), Ok(None));
        assert_eq!(controller_resolution(&InteractionOutput::None), Ok(None));
    }
    #[test]
    fn oversized_plugin_effect_cannot_bypass_canonical_resolution_bounds() {
        assert_eq!(
            controller_resolution(&InteractionOutput::Submitted {
                payload: json!("x".repeat(64 * 1024))
            }),
            Err(InteractionControllerError::InvalidContract)
        );
    }
}
