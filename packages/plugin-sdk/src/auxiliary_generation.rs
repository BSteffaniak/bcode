//! Portable request-only contracts for explicitly selected, bounded tool-free inference.

use bcode_model::TokenUsage;
use bcode_session_models::SessionId;
use serde::{Deserialize, Serialize};

/// Bounded no-tools request using the session's explicitly named model profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuxiliaryGenerationRequest {
    /// Canonical session used only for resolved configuration and ownership.
    pub session_id: SessionId,
    /// Explicit profile resolved through the central model resolver.
    pub model_profile: String,
    /// Request-only trusted system instruction.
    pub system_prompt: String,
    /// Request-only bounded untrusted evidence.
    pub prompt: String,
    /// Complete request token admission bound, including system instruction.
    pub max_input_tokens: u64,
    /// Output token cap.
    pub max_output_tokens: u32,
    /// Operation deadline; cancellation must propagate to provider work.
    pub timeout_ms: u64,
}

/// Normalized response; no provider-native data or durable generation session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuxiliaryGenerationResult {
    /// Bounded final assistant text, not tool calls or reasoning.
    pub text: String,
    /// Centrally resolved effective model identity.
    pub model_id: String,
    /// Centrally resolved provider plugin identity.
    pub provider_plugin_id: String,
    /// Normalized usage when supplied by the provider.
    pub usage: Option<TokenUsage>,
}
