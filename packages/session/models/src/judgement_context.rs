//! Request-only optional evidence contracts. These are not canonical history events.

use serde::{Deserialize, Serialize};

/// Producer domain for an original retrieved evidence snippet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JudgementContextSource {
    /// Confined file range.
    File,
    /// Bounded search hit.
    Search,
    /// Bounded history excerpt.
    History,
    /// Evidence from an available memory producer.
    Memory,
    /// Optional tool supplement, never its mandatory protocol response.
    ToolResult,
}

/// Original producer-owned snippet; ranking may select IDs but never rewrite this content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgementContextCandidate {
    /// Stable opaque identity, bounded to 128 UTF-8 bytes.
    pub id: String,
    /// Owning source domain.
    pub source: JudgementContextSource,
    /// Original source locator, bounded to 1024 UTF-8 bytes.
    pub provenance: String,
    /// Whole original snippet, bounded to 2048 UTF-8 bytes.
    pub snippet: String,
    /// Mandatory/explicit evidence retains its original slot and is never excluded.
    pub protected: bool,
}

impl JudgementContextCandidate {
    /// Compatibility schema for transient producer contributions containing one candidate.
    pub const SCHEMA: &str = "bcode.context-candidate/v1";

    /// Whether this original contribution satisfies the portable bounded contract.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        !self.id.trim().is_empty()
            && self.id.len() <= 128
            && !self.provenance.trim().is_empty()
            && self.provenance.len() <= 1024
            && !self.snippet.trim().is_empty()
            && self.snippet.len() <= 2048
    }
}
