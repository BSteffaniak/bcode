//! Domain-owned, payload-free observations at canonical interaction boundaries.
//!
//! These records do not authorize a response, replace canonical checkpoints, or
//! imply that a cancelled tool had no effects. Client resolution tasks may have
//! no turn task-local; session identity therefore comes from the validated owner.

use crate::ServerState;
use bcode_session_models::SessionId;

/// Emit payload-free diagnostics for the validated session owner.
pub fn emit(state: &ServerState, session_id: SessionId, name: &str, data: serde_json::Value) {
    context(state, session_id).emit("interaction", name, data, false);
}

/// Bind diagnostics to the validated invocation without inventing turn ancestry.
pub fn tool_context(
    state: &ServerState,
    session_id: SessionId,
    tool_call_id: &str,
) -> bcode_xray::CaptureContext {
    bind_tool_context(&context(state, session_id), tool_call_id)
}

fn bind_tool_context(
    capture: &bcode_xray::CaptureContext,
    tool_call_id: &str,
) -> bcode_xray::CaptureContext {
    let mut correlation = capture.correlation().clone();
    if correlation.tool_call_id.as_deref() != Some(tool_call_id) {
        // Same-session client work can target an older invocation. Do not invent
        // turn/run ancestry when the active task is not that invocation.
        correlation.turn_id = None;
        correlation.request_id = None;
        correlation.run_id = None;
        correlation.activation_id = None;
        correlation.attempt_id = None;
    }
    correlation.tool_call_id = Some(tool_call_id.to_owned());
    capture.with_correlation(correlation)
}

/// Emit payload-free diagnostics correlated with the validated invocation.
pub fn emit_tool(
    state: &ServerState,
    session_id: SessionId,
    tool_call_id: &str,
    name: &str,
    data: serde_json::Value,
) {
    tool_context(state, session_id, tool_call_id).emit("interaction", name, data, false);
}

/// Classify an exchange resolution without exposing response or error payloads.
pub const fn exchange_outcome(
    resolution: &bcode_session_models::ToolExchangeResolution,
) -> &'static str {
    use bcode_session_models::ToolExchangeResolution as Resolution;
    match resolution {
        Resolution::Responded { .. } => "responded",
        Resolution::Cancelled => "cancelled",
        Resolution::TimedOut => "timed_out",
        Resolution::NoCompatibleConsumer => "no_compatible_consumer",
        Resolution::ConsumerDetached => "consumer_detached",
        Resolution::Failed { .. } => "failed",
    }
}

fn context(state: &ServerState, session_id: SessionId) -> bcode_xray::CaptureContext {
    owner_context(&state.xray, session_id.to_string())
}

fn owner_context(hub: &bcode_xray::XrayHub, session_id: String) -> bcode_xray::CaptureContext {
    if let Some(capture) = bcode_xray::CaptureContext::current()
        && capture.correlation().session_id == session_id
    {
        return capture;
    }
    hub.context(bcode_xray::XrayCorrelation {
        session_id,
        ..Default::default()
    })
}

/// Tracks completion separately from abandonment at an interaction boundary.
pub struct InteractionObservation {
    capture: bcode_xray::CaptureContext,
    operation: &'static str,
    started: std::time::Instant,
    finished: bool,
}

impl InteractionObservation {
    pub(crate) fn start_tool(
        state: &ServerState,
        session_id: SessionId,
        tool_call_id: &str,
        operation: &'static str,
    ) -> Self {
        let capture = tool_context(state, session_id, tool_call_id);
        capture.emit(
            "interaction",
            "operation_started",
            serde_json::json!({"operation": operation}),
            false,
        );
        Self {
            capture,
            operation,
            started: std::time::Instant::now(),
            finished: false,
        }
    }
    pub(crate) fn start(
        state: &ServerState,
        session_id: SessionId,
        operation: &'static str,
    ) -> Self {
        let capture = context(state, session_id);
        capture.emit(
            "interaction",
            "operation_started",
            serde_json::json!({"operation": operation}),
            false,
        );
        Self {
            capture,
            operation,
            started: std::time::Instant::now(),
            finished: false,
        }
    }

    pub(crate) fn finish(&mut self, outcome: &'static str) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.capture.emit("interaction", "operation_finished", serde_json::json!({
            "operation": self.operation,
            "outcome": outcome,
            "duration_ms": u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        }), false);
    }
}

impl Drop for InteractionObservation {
    fn drop(&mut self) {
        self.finish("unfinished");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_another_invocation_clears_unproved_turn_ancestry() {
        let hub = bcode_xray::XrayHub::default();
        let capture = hub.context(bcode_xray::XrayCorrelation {
            session_id: "owner".into(),
            turn_id: Some("wrong-turn".into()),
            tool_call_id: Some("other-call".into()),
            run_id: Some("wrong-run".into()),
            ..Default::default()
        });
        let bound = bind_tool_context(&capture, "target-call");
        assert_eq!(bound.correlation().session_id, "owner");
        assert_eq!(
            bound.correlation().tool_call_id.as_deref(),
            Some("target-call")
        );
        assert!(bound.correlation().turn_id.is_none());
        assert!(bound.correlation().run_id.is_none());
    }

    #[test]
    fn exchange_outcomes_never_serialize_response_bodies_or_failure_messages() {
        use bcode_session_models::ToolExchangeResolution as Resolution;
        assert_eq!(
            exchange_outcome(&Resolution::Responded {
                payload: serde_json::json!({"private": "secret"})
            }),
            "responded"
        );
        assert_eq!(
            exchange_outcome(&Resolution::Failed {
                code: "private".into(),
                message: "secret".into()
            }),
            "failed"
        );
        assert_eq!(
            exchange_outcome(&Resolution::ConsumerDetached),
            "consumer_detached"
        );
        assert_eq!(exchange_outcome(&Resolution::Cancelled), "cancelled");
        assert_eq!(exchange_outcome(&Resolution::TimedOut), "timed_out");
        assert_eq!(
            exchange_outcome(&Resolution::NoCompatibleConsumer),
            "no_compatible_consumer"
        );
    }

    #[tokio::test]
    async fn foreign_client_context_cannot_replace_validated_interaction_owner() {
        let hub = bcode_xray::XrayHub::default();
        let foreign = hub.context(bcode_xray::XrayCorrelation {
            session_id: "foreign".into(),
            turn_id: Some("foreign-turn".into()),
            ..Default::default()
        });
        foreign
            .scope(async {
                let owner = owner_context(&hub, "owner".into());
                assert_eq!(owner.correlation().session_id, "owner");
                assert!(owner.correlation().turn_id.is_none());
            })
            .await;
    }

    #[tokio::test]
    async fn originating_turn_context_is_preserved_for_same_session() {
        let hub = bcode_xray::XrayHub::default();
        let capture = hub.context(bcode_xray::XrayCorrelation {
            session_id: "owner".into(),
            turn_id: Some("turn".into()),
            tool_call_id: Some("tool".into()),
            ..Default::default()
        });
        capture
            .scope(async {
                let owner = owner_context(&hub, "owner".into());
                assert_eq!(owner.correlation(), capture.correlation());
            })
            .await;
    }

    #[tokio::test]
    async fn interaction_guard_drop_reports_unfinished_not_cancelled() {
        let hub = bcode_xray::XrayHub::default();
        let mut observation = hub
            .observe(bcode_xray::XrayObserveRequest {
                schema_version: bcode_xray::XRAY_SCHEMA_VERSION,
                session_id: "owner".into(),
                deep_capture: false,
            })
            .expect("observe");
        drop(InteractionObservation {
            capture: owner_context(&hub, "owner".into()),
            operation: "exchange_cleanup",
            started: std::time::Instant::now(),
            finished: false,
        });
        observation.finalize().expect("cutoff");
        let _ = observation.recv().await.expect("ready");
        let event = observation.recv().await.expect("unfinished");
        assert_eq!(event.event.name, "operation_finished");
        assert_eq!(event.event.data["outcome"], "unfinished");
    }
}
