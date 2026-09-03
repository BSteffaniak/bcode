//! Current TUI activity state.

use bcode_session_models::{ModelTurnOutcome, RuntimeWorkKind, RuntimeWorkStatus};
use bcode_session_view_models::RuntimeWorkView;

/// Current high-level TUI activity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityState {
    /// No active model/tool work.
    Idle,
    /// Preparing the model request payload.
    PreparingModelRequest,
    /// Starting a provider request.
    StartingProviderRequest {
        /// Provider identifier.
        provider: String,
        /// Provider round, when known.
        round: Option<u32>,
    },
    /// Waiting for a provider response.
    WaitingForProvider {
        /// Provider identifier.
        provider: String,
        /// Provider round, when known.
        round: Option<u32>,
    },
    /// Preparing tool execution from a provider tool call.
    PreparingToolExecution {
        /// Tool name.
        name: String,
    },
    /// Preparing a follow-up request after a tool or permission step.
    PreparingFollowUpRequest,
    /// Finalizing the active model turn.
    FinalizingModelTurn,
    /// Running tracked background/runtime work.
    RuntimeWork {
        /// Concrete runtime-work progress label.
        detail: String,
    },
    /// Compacting context before a model response.
    Compacting {
        /// User-facing progress detail.
        detail: String,
    },
    /// Receiving streamed model output.
    Streaming {
        /// Number of visible streamed characters received in this activity.
        chars: usize,
    },
    /// Receiving provider stream progress.
    ProviderStream {
        /// User-facing progress detail.
        detail: String,
    },
    /// Waiting to retry a provider request after quota/rate-limit reset.
    RetryWait {
        /// User-facing message.
        message: String,
        /// Unix timestamp when retry should occur.
        retry_at_unix: u64,
    },
    /// Running a tool.
    RunningTool {
        /// Tool name.
        name: String,
    },
    /// Waiting for a permission decision.
    WaitingPermission {
        /// Tool name.
        name: String,
    },
    /// Cancelling the active turn.
    Cancelling,
}

impl ActivityState {
    /// Return whether two values represent the same timed phase.
    #[must_use]
    pub fn same_phase_as(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::StartingProviderRequest {
                    provider: left_provider,
                    round: left_round,
                },
                Self::StartingProviderRequest {
                    provider: right_provider,
                    round: right_round,
                },
            )
            | (
                Self::WaitingForProvider {
                    provider: left_provider,
                    round: left_round,
                },
                Self::WaitingForProvider {
                    provider: right_provider,
                    round: right_round,
                },
            ) => left_provider == right_provider && left_round == right_round,
            (
                Self::PreparingToolExecution { name: left },
                Self::PreparingToolExecution { name: right },
            )
            | (Self::RunningTool { name: left }, Self::RunningTool { name: right })
            | (Self::WaitingPermission { name: left }, Self::WaitingPermission { name: right }) => {
                left == right
            }
            _ => std::mem::discriminant(self) == std::mem::discriminant(other),
        }
    }
}

/// Format active runtime work for the compact terminal activity surface.
#[must_use]
pub fn runtime_work_detail(runtime_work: &[RuntimeWorkView]) -> Option<String> {
    let running_tools = runtime_work
        .iter()
        .filter(|work| {
            work.kind == RuntimeWorkKind::Tool && work.status == RuntimeWorkStatus::Running
        })
        .count();
    if running_tools > 1 {
        return Some(format!("running {running_tools} tools"));
    }
    let work = runtime_work.iter().min_by(|left, right| {
        runtime_work_priority(left)
            .cmp(&runtime_work_priority(right))
            .then_with(|| left.work_id.cmp(&right.work_id))
    })?;
    let prefix = match work.status {
        RuntimeWorkStatus::Queued => "queued",
        RuntimeWorkStatus::Cancelling => "cancelling",
        RuntimeWorkStatus::Running => match work.kind {
            RuntimeWorkKind::ModelTurn => "running",
            RuntimeWorkKind::Tool => "running tool",
            RuntimeWorkKind::PluginInvocation => "running plugin",
            RuntimeWorkKind::EventDelivery => "delivering event",
            RuntimeWorkKind::Workflow => "running workflow",
            RuntimeWorkKind::WorkflowNode => "running workflow node",
        },
        RuntimeWorkStatus::Completed
        | RuntimeWorkStatus::Cancelled
        | RuntimeWorkStatus::Failed
        | RuntimeWorkStatus::TimedOut
        | RuntimeWorkStatus::Suspended => return None,
    };
    let detail = match (work.label.is_empty(), work.message.as_deref()) {
        (true, Some(message)) => message.to_owned(),
        (true, None) => work.work_id.to_string(),
        (false, Some(message)) if message != work.label => {
            format!("{} — {message}", work.label)
        }
        (false, _) => work.label.clone(),
    };
    Some(format!("{prefix}: {detail}"))
}

const fn runtime_work_priority(work: &RuntimeWorkView) -> u8 {
    match (work.status, work.kind) {
        (RuntimeWorkStatus::Cancelling, _) => 0,
        (RuntimeWorkStatus::Queued, _) => 1,
        (RuntimeWorkStatus::Running, RuntimeWorkKind::Tool) => 2,
        (RuntimeWorkStatus::Running, RuntimeWorkKind::PluginInvocation) => 3,
        (RuntimeWorkStatus::Running, RuntimeWorkKind::WorkflowNode) => 4,
        (RuntimeWorkStatus::Running, RuntimeWorkKind::Workflow) => 5,
        (RuntimeWorkStatus::Running, RuntimeWorkKind::EventDelivery) => 6,
        (RuntimeWorkStatus::Running, RuntimeWorkKind::ModelTurn) => 7,
        (
            RuntimeWorkStatus::Completed
            | RuntimeWorkStatus::Cancelled
            | RuntimeWorkStatus::Failed
            | RuntimeWorkStatus::TimedOut
            | RuntimeWorkStatus::Suspended,
            _,
        ) => 8,
    }
}

/// Return a status label for a model turn outcome.
#[must_use]
pub const fn model_turn_outcome_label(outcome: ModelTurnOutcome) -> &'static str {
    match outcome {
        ModelTurnOutcome::Completed => "done",
        ModelTurnOutcome::Cancelled => "cancelled",
        ModelTurnOutcome::Error => "error",
        ModelTurnOutcome::IdleTimeout => "idle timeout",
        ModelTurnOutcome::ToolRoundLimitReached => "tool round limit reached",
        ModelTurnOutcome::ProviderUnavailable => "provider unavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::WorkId;

    #[test]
    fn runtime_work_detail_is_terminal_presentation_owned() {
        let running = |id: &str, kind, label: &str, message: Option<&str>| RuntimeWorkView {
            work_id: WorkId::new(id),
            kind,
            label: label.to_owned(),
            status: RuntimeWorkStatus::Running,
            cancellable: true,
            message: message.map(ToOwned::to_owned),
            completed_units: None,
            total_units: None,
            updated_at_ms: None,
        };
        let one = running("work-1", RuntimeWorkKind::Tool, "shell", Some("halfway"));
        assert_eq!(
            runtime_work_detail(std::slice::from_ref(&one)).as_deref(),
            Some("running tool: shell — halfway")
        );
        let model = running(
            "model-1",
            RuntimeWorkKind::ModelTurn,
            "model turn model-1",
            None,
        );
        assert_eq!(
            runtime_work_detail(&[model, one.clone()]).as_deref(),
            Some("running tool: shell — halfway")
        );
        let two = running("work-2", RuntimeWorkKind::Tool, "web", None);
        assert_eq!(
            runtime_work_detail(&[one, two]).as_deref(),
            Some("running 2 tools")
        );
        let queued = RuntimeWorkView {
            work_id: WorkId::new("a-work"),
            kind: RuntimeWorkKind::ModelTurn,
            label: "queued turn".to_owned(),
            status: RuntimeWorkStatus::Queued,
            cancellable: false,
            message: None,
            completed_units: None,
            total_units: None,
            updated_at_ms: None,
        };
        let plugin = running("z-work", RuntimeWorkKind::PluginInvocation, "plugin", None);
        assert_eq!(
            runtime_work_detail(&[plugin, queued]).as_deref(),
            Some("queued: queued turn")
        );

        let cancelling = RuntimeWorkView {
            status: RuntimeWorkStatus::Cancelling,
            ..running("work-3", RuntimeWorkKind::PluginInvocation, "plugin", None)
        };
        assert_eq!(
            runtime_work_detail(&[cancelling]).as_deref(),
            Some("cancelling: plugin")
        );
    }
}
