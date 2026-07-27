//! Stable current runtime-work projection value codecs.

use bcode_session_models::{RuntimeWorkKind, RuntimeWorkStatus};

#[must_use]
pub const fn runtime_work_kind_name(kind: RuntimeWorkKind) -> &'static str {
    match kind {
        RuntimeWorkKind::Tool => "tool",
        RuntimeWorkKind::PluginInvocation => "plugin_invocation",
        RuntimeWorkKind::ModelTurn => "model_turn",
        RuntimeWorkKind::EventDelivery => "event_delivery",
        RuntimeWorkKind::Workflow => "workflow",
        RuntimeWorkKind::WorkflowNode => "workflow_node",
    }
}

#[must_use]
pub fn parse_runtime_work_kind(value: &str) -> RuntimeWorkKind {
    match value {
        "plugin_invocation" => RuntimeWorkKind::PluginInvocation,
        "model_turn" => RuntimeWorkKind::ModelTurn,
        "event_delivery" => RuntimeWorkKind::EventDelivery,
        "workflow" => RuntimeWorkKind::Workflow,
        "workflow_node" => RuntimeWorkKind::WorkflowNode,
        _ => RuntimeWorkKind::Tool,
    }
}

#[must_use]
pub const fn runtime_work_status_name(status: RuntimeWorkStatus) -> &'static str {
    match status {
        RuntimeWorkStatus::Queued => "queued",
        RuntimeWorkStatus::Running => "running",
        RuntimeWorkStatus::Cancelling => "cancelling",
        RuntimeWorkStatus::Completed => "completed",
        RuntimeWorkStatus::Failed => "failed",
        RuntimeWorkStatus::TimedOut => "timed_out",
        RuntimeWorkStatus::Cancelled => "cancelled",
    }
}

#[must_use]
pub fn parse_runtime_work_status(value: &str) -> RuntimeWorkStatus {
    match value {
        "queued" => RuntimeWorkStatus::Queued,
        "cancelling" => RuntimeWorkStatus::Cancelling,
        "completed" => RuntimeWorkStatus::Completed,
        "failed" => RuntimeWorkStatus::Failed,
        "timed_out" => RuntimeWorkStatus::TimedOut,
        "cancelled" => RuntimeWorkStatus::Cancelled,
        _ => RuntimeWorkStatus::Running,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_work_values_round_trip() {
        for status in [
            RuntimeWorkStatus::Queued,
            RuntimeWorkStatus::Running,
            RuntimeWorkStatus::Cancelling,
            RuntimeWorkStatus::Completed,
            RuntimeWorkStatus::Failed,
            RuntimeWorkStatus::TimedOut,
            RuntimeWorkStatus::Cancelled,
        ] {
            assert_eq!(
                parse_runtime_work_status(runtime_work_status_name(status)),
                status
            );
        }
    }
}
