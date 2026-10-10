//! Request-only milestone host bridge payloads.

pub use crate::auxiliary_generation::{AuxiliaryGenerationRequest, AuxiliaryGenerationResult};

use bcode_session_models::SessionId;
use serde::{Deserialize, Serialize};

/// Application host bridge ID; reads remain available without the policy plugin.
pub const MILESTONES_HOST_INTERFACE_ID: &str = "bcode.session-milestones-host/v1";

/// Plugin command request; the host owns identities, source ranges and persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum MilestonesHostRequest {
    /// One bounded read page, formatted by host from canonical values.
    Read { session_id: SessionId },
    /// Explicit paid generation under opt-in policy and ordinary host authorization.
    Generate { session_id: SessionId },
    /// Automatic evaluation requested by an authenticated settled-turn observer.
    EvaluateSettled { session_id: SessionId },
}

/// Bounded plain-text result; never exposes model prompt or provider errors.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MilestonesHostResponse {
    /// Whether the operation succeeded or intentionally skipped.
    pub success: bool,
    /// Plain text response for native frontend adaptation.
    pub text: String,
}
