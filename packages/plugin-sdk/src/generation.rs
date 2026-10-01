//! Portable structured generation contracts.

use bcode_session_models::SessionId;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Bounded renderer-neutral request for one tool-free structured model generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginStructuredGenerationRequest {
    /// Optional source whose model-visible context is captured at submission.
    #[serde(default)]
    pub source_session_id: Option<SessionId>,
    pub session_name: String,
    pub system_prompt: String,
    pub prompt: String,
    pub output_name: String,
    pub output_schema: serde_json::Value,
    pub timeout_ms: u64,
}

/// Structured output and optional host-verified source provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginStructuredGenerationResult {
    /// Model-produced schema-validated output.
    pub output: serde_json::Value,
    /// Captured by the host, never supplied by the model.
    pub source: Option<bcode_session_models::SessionDerivationSourceSnapshot>,
}

/// Observation and cancellation handle for a structured generation operation.
/// Dropping or hiding a view does not request cancellation.
#[derive(Debug, Clone, Default)]
pub struct PluginStructuredGenerationControl {
    session: Arc<std::sync::Mutex<Option<SessionId>>>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl PluginStructuredGenerationControl {
    /// Return the generation session once the host has prepared it.
    #[allow(clippy::must_use_candidate)] // Option already carries must-use semantics.
    pub fn session_id(&self) -> Option<SessionId> {
        *self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Publish the host-owned session identity for semantic session-view observation.
    pub fn set_session_id(&self, session_id: SessionId) {
        *self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(session_id);
    }

    /// Request cancellation, including while the session is still being prepared.
    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Whether cancellation has been requested. This is not terminal acknowledgement.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Acquire)
    }
}
