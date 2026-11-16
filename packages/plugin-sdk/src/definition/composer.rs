//! Bounded local composer activation operation; search remains the existing
//! `bcode.composer/v1` service and never runs in this activation adapter.
use serde::{Deserialize, Serialize};

/// Exact portable composer activation contract.
pub const COMPOSER_ACTIVATE_CONTRACT: &str = "bcode.composer-activate/v1";
/// Existing contributor grammar and document contracts, without renderer layout.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposerActivateRequest {
    /// Exact registered activation kind.
    pub kind: String,
    /// Current bounded document/revision.
    pub document: crate::composer::ComposerDocument,
}
