//! Typed supporting visual planning. Illustrations never become video evidence.
use serde::{Deserialize, Serialize};

/// Source-grounded subject proposed for illustration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisualOpportunity {
    /// Existing chapter receiving the illustration.
    pub section_id: String,
    /// Exact named subject present in the chapter or source title.
    pub subject: String,
    /// What an image of this subject should clarify.
    pub purpose: String,
}
/// Candidate illustration plan, not permission to download.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisualPlan {
    /// Prefer a few high-value illustrations over repeated talking heads or decoration.
    pub opportunities: Vec<VisualOpportunity>,
}
/// Multimodal check of a candidate against the intended explanatory purpose.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisualReview {
    /// False when irrelevant, ambiguous, misleading, or merely decorative.
    pub useful: bool,
    /// Plain caption describing only what is actually visible.
    pub caption: String,
}
/// External illustration origin, never a source-video observation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IllustrationOrigin {
    /// Original publisher's description page.
    pub page_url: String,
    /// Exact downloaded URL.
    pub image_url: String,
    /// Attribution text from source metadata, retained as untrusted plain text.
    pub attribution: String,
    /// Recognized license identifier.
    pub license: String,
    /// Source-provided license details URL.
    pub license_url: String,
    /// What this supporting illustration explains.
    pub purpose: String,
}
