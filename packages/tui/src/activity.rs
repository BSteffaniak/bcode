//! Current TUI activity state.

use bcode_session_models::ModelTurnOutcome;

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
