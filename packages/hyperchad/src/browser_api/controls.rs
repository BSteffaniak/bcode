//! Allowlisted, observation-fenced controls. Never accepts `SessionViewAction`,
//! execution settings, generic invocation or host-owned exchange resolutions.
use bcode_session_view_models::{SessionViewAction, SessionViewSnapshot};
use hyperchad::{renderer::Content, router::RouteRequest};
use serde::Deserialize;

use super::{
    BACKEND_TIMEOUT, BrowserActionResponse, ObservationGuard, error, json_content, parse_body,
};
use crate::{HyperChadAppState, InteractionForm, InteractionInputKind, InteractionSubmissionGuard};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlRequest {
    session_id: bcode_session_models::SessionId,
    epoch: bcode_session_models::SessionId,
    revision: u64,
    action: Control,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Control {
    Permission {
        permission_id: String,
        approved: bool,
        remember: bool,
    },
    PermissionBatch {
        batch_id: String,
        approved: bool,
    },
    Interaction {
        interaction_id: String,
        input: Input,
    },
}

// Producer-neutral input, not an arbitrary final exchange resolution. In
// particular callers cannot send timed_out, failed, closed or host metadata.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Input {
    Activate {
        control_id: String,
    },
    Change {
        control_id: String,
        value: serde_json::Value,
    },
    Focus {
        control_id: String,
    },
    Blur {
        control_id: String,
    },
    Navigate {
        direction: bcode_tool::InteractionNavigation,
    },
    Submit {
        value: Option<serde_json::Value>,
    },
    Cancel,
}

impl Input {
    fn form(
        self,
        session_id: bcode_session_models::SessionId,
        interaction_id: String,
    ) -> Result<InteractionForm, &'static str> {
        let mut form = InteractionForm {
            session_id: session_id.to_string(),
            interaction_id,
            kind: InteractionInputKind::Cancel,
            control_id: None,
            value: None,
            value_is_json: false,
            direction: None,
        };
        match self {
            Self::Activate { control_id } => {
                form.kind = InteractionInputKind::Activate;
                form.control_id = Some(control_id);
            }
            Self::Change { control_id, value } => {
                form.kind = InteractionInputKind::Change;
                form.control_id = Some(control_id);
                form.value =
                    Some(serde_json::to_string(&value).map_err(|_| "invalid_control_input")?);
                form.value_is_json = true;
            }
            Self::Focus { control_id } => {
                form.kind = InteractionInputKind::Focus;
                form.control_id = Some(control_id);
            }
            Self::Blur { control_id } => {
                form.kind = InteractionInputKind::Blur;
                form.control_id = Some(control_id);
            }
            Self::Navigate { direction } => {
                form.kind = InteractionInputKind::Navigate;
                form.direction = Some(
                    match direction {
                        bcode_tool::InteractionNavigation::Next => "next",
                        bcode_tool::InteractionNavigation::Previous => "previous",
                    }
                    .to_owned(),
                );
            }
            Self::Submit { value } => {
                form.kind = InteractionInputKind::Submit;
                form.value = value
                    .map(|value| serde_json::to_string(&value))
                    .transpose()
                    .map_err(|_| "invalid_control_input")?;
                form.value_is_json = true;
            }
            Self::Cancel => {}
        }
        if form.control_id.as_ref().is_some_and(|id| !valid_id(id)) {
            return Err("invalid_control_identity");
        }
        Ok(form)
    }
}

fn valid_id(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
}

fn permission_action(
    observed: &SessionViewSnapshot,
    fresh: &SessionViewSnapshot,
    session_id: bcode_session_models::SessionId,
    permission_id: String,
    approved: bool,
    remember: bool,
) -> Result<SessionViewAction, &'static str> {
    if !valid_id(&permission_id) {
        return Err("invalid_control_identity");
    }
    let previous = observed
        .permissions
        .iter()
        .find(|permission| permission.permission_id == permission_id && !permission.resolved)
        .ok_or("stale_control")?;
    let current = fresh
        .permissions
        .iter()
        .find(|permission| permission.permission_id == permission_id && !permission.resolved)
        .ok_or("stale_control")?;
    // A routed checkpoint can belong to a source session; hydration establishes
    // destination membership. Compare every displayed checkpoint field, not just ID.
    if previous != current
        || (remember && !current.can_remember)
        || fresh.session_id != Some(session_id)
    {
        return Err("stale_control");
    }
    Ok(SessionViewAction::ResolvePermission {
        permission_id,
        approved,
        remember,
    })
}

fn batch_action(
    observed: &SessionViewSnapshot,
    fresh: &SessionViewSnapshot,
    batch_id: String,
    approved: bool,
) -> Result<SessionViewAction, &'static str> {
    if !valid_id(&batch_id) {
        return Err("invalid_control_identity");
    }
    let members = |snapshot: &SessionViewSnapshot| {
        snapshot
            .permissions
            .iter()
            .filter(|permission| {
                !permission.resolved
                    && permission
                        .batch
                        .as_ref()
                        .is_some_and(|batch| batch.batch_id == batch_id)
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    let previous = members(observed);
    let current = members(fresh);
    if previous.is_empty() || previous != current {
        return Err("stale_control");
    }
    let count = current.len();
    let mut indices = std::collections::BTreeSet::new();
    if current.iter().any(|permission| {
        permission.batch.as_ref().is_none_or(|batch| {
            batch.call_count != count
                || batch.call_index >= count
                || !indices.insert(batch.call_index)
        })
    }) {
        return Err("incomplete_permission_batch");
    }
    Ok(SessionViewAction::ResolvePermissionBatch { batch_id, approved })
}

impl HyperChadAppState {
    pub(super) async fn browser_control(&self, request: RouteRequest) -> Content {
        if !self.authorizes(&request) {
            return error("unauthorized");
        }
        let query: ControlRequest = match parse_body(&request) {
            Ok(value) => value,
            Err(code) => return error(code),
        };
        let Ok(mut observations) = self.browser_api.try_lock() else {
            return error("browser_busy");
        };
        let guard = ObservationGuard {
            session_id: query.session_id,
            epoch: query.epoch,
            revision: query.revision,
        };
        let observed = match observations.guarded_snapshot(&guard) {
            Ok(value) => value.clone(),
            Err(code) => return error(code),
        };
        let permissions = observations
            .permissions
            .get(&query.session_id)
            .cloned()
            .unwrap_or_default();
        // Consume this observation before any controller mutation or admission.
        // A retry must first refresh; this is not an exactly-once receipt.
        let Some(next_revision) = observations.revision.checked_add(1) else {
            return error("revision_exhausted");
        };
        observations.revision = next_revision;
        if let Some((revision, _)) = observations.snapshots.get_mut(&query.session_id) {
            *revision = next_revision;
        }
        // This lock also bounds concurrent browser mutations. Timeout does not prove
        // rejection: no canonical exactly-once receipt exists for these controls.
        match tokio::time::timeout(
            BACKEND_TIMEOUT,
            self.execute_browser_control(query.session_id, &observed, &permissions, query.action),
        )
        .await
        {
            Ok(Ok(content)) => content,
            Ok(Err(code)) => error(code),
            Err(_) => error("control_outcome_unknown"),
        }
    }

    async fn execute_browser_control(
        &self,
        session_id: bcode_session_models::SessionId,
        observed: &SessionViewSnapshot,
        permissions: &super::permissions::PermissionObservation,
        control: Control,
    ) -> Result<Content, &'static str> {
        let mut fresh = self
            .session_snapshot(session_id)
            .await
            .map_err(|_| "control_state_unavailable")?;
        if !matches!(&control, Control::Interaction { .. }) {
            self.browser_permission_observation(session_id, &mut fresh)
                .await?;
        }
        let decision = match &control {
            Control::Permission {
                permission_id,
                approved,
                remember,
            } => {
                permission_action(
                    observed,
                    &fresh,
                    session_id,
                    permission_id.clone(),
                    *approved,
                    *remember,
                )?;
                Some((
                    permissions.decision(
                        session_id,
                        Some(permission_id),
                        None,
                        *approved,
                        *remember,
                    )?,
                    false,
                ))
            }
            Control::PermissionBatch { batch_id, approved } => {
                batch_action(observed, &fresh, batch_id.clone(), *approved)?;
                Some((
                    permissions.decision(session_id, None, Some(batch_id), *approved, false)?,
                    true,
                ))
            }
            Control::Interaction { .. } => None,
        };
        if let Some((decision, batch)) = decision {
            let result = self
                .client
                .resolve_permissions_conditional(decision)
                .await
                .map_err(|_| "control_outcome_unknown")?;
            let bcode_session_models::ConditionalPermissionResult::Resolved { resolved } = result
            else {
                return Err("conditional_permission_not_resolved");
            };
            let outcome = if batch {
                bcode_session_view_models::SessionViewActionOutcome::PermissionBatchResolved {
                    resolved_count: resolved,
                }
            } else {
                bcode_session_view_models::SessionViewActionOutcome::PermissionResolved {
                    resolved: resolved == 1,
                }
            };
            return Ok(json_content(&BrowserActionResponse {
                schema_version: 1,
                outcome,
            }));
        }
        let Control::Interaction {
            interaction_id,
            input,
        } = control
        else {
            return Err("invalid_control");
        };
        self.execute_browser_interaction(session_id, observed, &fresh, interaction_id, input)
            .await
    }
    async fn execute_browser_interaction(
        &self,
        session_id: bcode_session_models::SessionId,
        observed: &SessionViewSnapshot,
        fresh: &SessionViewSnapshot,
        interaction_id: String,
        input: Input,
    ) -> Result<Content, &'static str> {
        if !valid_id(&interaction_id) {
            return Err("invalid_control_identity");
        }
        let previous = observed
            .interactions
            .iter()
            .find(|value| value.interaction_id == interaction_id && !value.resolved)
            .ok_or("stale_control")?;
        let current = fresh
            .interactions
            .iter()
            .find(|value| value.interaction_id == interaction_id && !value.resolved)
            .ok_or("stale_control")?;
        if previous != current {
            return Err("stale_control");
        }
        let pending = self
            .client
            .inspect_pending_tool_exchange(&interaction_id)
            .await
            .map_err(|_| "control_state_unavailable")?
            .ok_or("stale_control")?;
        if !pending.is_addressed_to(session_id) {
            return Err("stale_control");
        }
        let exchange = pending.request;
        if current.producer_id.as_deref() != Some(exchange.producer_id.as_str())
            || current.exchange_schema.as_deref() != Some(exchange.schema.as_str())
            || current.exchange_schema_version != Some(exchange.schema_version)
        {
            return Err("stale_control");
        }
        let _submission = InteractionSubmissionGuard::acquire(
            &self.interaction_submissions,
            &exchange.exchange_id,
        )
        .map_err(|_| "control_busy")?;
        let form = input.form(session_id, interaction_id.clone())?;
        let input =
            crate::interaction_input_from_form(&form).map_err(|_| "invalid_control_input")?;
        let output = self
            .apply_local_interaction_input(&exchange, input)
            .map_err(|_| "control_adapter_unavailable")?;
        let resolution = match output {
            Some(bcode_tool::InteractionOutput::None | bcode_tool::InteractionOutput::Redraw) => {
                return Ok(json_content(&BrowserActionResponse {
                    schema_version: 1,
                    outcome: bcode_session_view_models::SessionViewActionOutcome::None,
                }));
            }
            Some(bcode_tool::InteractionOutput::Submitted { payload }) => {
                bcode_session_models::ToolExchangeResolution::Responded { payload }
            }
            Some(bcode_tool::InteractionOutput::Cancelled) => {
                bcode_session_models::ToolExchangeResolution::Cancelled
            }
            None => {
                crate::generic_interaction_resolution(&form)
                    .map_err(|_| "invalid_control_input")?
                    .ok_or("control_adapter_unavailable")?
                    .0
            }
        };
        // Keep the shared interaction submission guard through resolution.
        let client = if crate::local_interaction_adapter(&exchange).is_none() {
            self.client
                .clone()
                .with_interaction_adapter(crate::generic_interaction_adapter(&exchange))
        } else {
            self.client.clone()
        };
        let resolved = client
            .resolve_tool_exchange(interaction_id, resolution)
            .await
            .map_err(|_| "control_outcome_unknown")?;
        crate::release_interaction_controller(&self.interaction_controllers, &exchange.exchange_id)
            .map_err(|_| "control_outcome_unknown")?;
        let outcome =
            bcode_session_view_models::SessionViewActionOutcome::InteractionResolved { resolved };
        Ok(json_content(&BrowserActionResponse {
            schema_version: 1,
            outcome,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn permission_controls_require_unchanged_pending_membership_and_complete_batch() {
        let session_id = bcode_session_models::SessionId::new();
        let permission = bcode_session_view_models::PermissionView {
            permission_id: "p".to_owned(),
            session_id: Some(session_id),
            tool_call_id: "call".to_owned(),
            tool_name: "tool".to_owned(),
            arguments_json: "{}".to_owned(),
            batch: Some(bcode_session_view_models::PermissionBatchView {
                batch_id: "batch".to_owned(),
                call_index: 0,
                call_count: 1,
            }),
            agent_id: "agent".to_owned(),
            title: None,
            policy_source: None,
            detail: None,
            resolved: false,
            approved: None,
            can_remember: false,
        };
        let mut observed = SessionViewSnapshot {
            session_id: Some(session_id),
            ..SessionViewSnapshot::empty()
        };
        observed.permissions.push(permission);
        let mut fresh = observed.clone();
        assert!(
            permission_action(&observed, &fresh, session_id, "p".to_owned(), true, false).is_ok()
        );
        assert!(
            permission_action(&observed, &fresh, session_id, "p".to_owned(), true, true).is_err()
        );
        assert!(batch_action(&observed, &fresh, "batch".to_owned(), true).is_ok());
        fresh.permissions[0].arguments_json = "changed".to_owned();
        assert!(
            permission_action(&observed, &fresh, session_id, "p".to_owned(), true, false).is_err()
        );
        fresh = observed.clone();
        fresh.permissions[0].resolved = true;
        assert!(
            permission_action(&observed, &fresh, session_id, "p".to_owned(), false, false).is_err()
        );
        observed.permissions[0].batch.as_mut().unwrap().call_count = 2;
        assert!(batch_action(&observed, &observed, "batch".to_owned(), true).is_err());
    }

    #[test]
    fn controls_reject_unbounded_authority_and_host_owned_resolutions() {
        for json in [
            r#"{"type":"submit_message","text":"shell"}"#,
            r#"{"type":"permission","permission_id":"p","approved":true,"remember":false,"execution":{}}"#,
            r#"{"type":"interaction","interaction_id":"i","input":{"type":"tick","elapsed_ms":1}}"#,
            r#"{"type":"interaction","interaction_id":"i","resolution":{"type":"failed"}}"#,
        ] {
            assert!(serde_json::from_str::<Control>(json).is_err());
        }
    }
}
