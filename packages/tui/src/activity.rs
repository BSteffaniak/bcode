//! Current TUI activity state.

use bcode_session_models::ModelTurnOutcome;

/// Current high-level TUI activity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityState {
    /// No active model/tool work.
    Idle,
    /// Waiting for a model response.
    Thinking,
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
    /// Writing a file through a filesystem tool.
    WritingFile,
    /// Editing a file through a filesystem tool.
    EditingFile,
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
