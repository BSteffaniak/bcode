//! Old-engine plugin visual projection over the shared plugin-visual contract.
//!
//! The canonical input and the portable registry live in [`crate::plugin_visual`]; this module
//! keeps the transcript-item projections only the old transcript engine uses.

pub use crate::plugin_visual::canonical::{CanonicalPluginVisual, CanonicalToolVisual};

/// Static producer-owned visual adapters shared by native and browser BMUX hosts.
pub use crate::plugin_visual::portable;

impl CanonicalToolVisual {
    /// Resolve a request/result/draft using the existing native projection.
    /// Request schemas are supplied by the host's plugin declaration registry;
    /// unknown schemas preserve the semantic fallback rather than inventing one.
    #[must_use]
    pub fn for_item(
        item: &super::item::TranscriptItem,
        request_schema: Option<(&str, u32)>,
    ) -> Option<Self> {
        use super::item::TranscriptItemKind;
        match item.kind() {
            TranscriptItemKind::ToolRequest {
                tool_call_id,
                producer_plugin_id,
                tool_name,
                ..
            } => {
                let (schema, version) = request_schema?;
                Self::from_request(
                    tool_call_id,
                    producer_plugin_id.as_deref(),
                    tool_name,
                    schema.to_owned(),
                    version,
                    item.text(),
                )
            }
            TranscriptItemKind::ToolResult {
                artifact: Some(artifact),
                ..
            } => Some(Self::from_artifact(artifact)),
            TranscriptItemKind::ToolContribution {
                contribution,
                placement,
                ..
            } if *placement != bcode_session_models::ToolContributionPlacement::Hidden => {
                Some(Self::from_contribution(contribution))
            }
            TranscriptItemKind::ToolRequestDraft { draft } => Some(Self::from_draft(item, draft)),
            _ => None,
        }
    }

    /// Build the native draft adapter input from the already projected draft.
    #[must_use]
    pub fn from_draft(
        item: &super::item::TranscriptItem,
        draft: &bcode_session_view_models::ToolRequestDraftView,
    ) -> Self {
        Self::from_draft_view(
            draft,
            &serde_json::json!(
                item.source_view_item_revision()
                    .unwrap_or_else(|| item.revision())
            ),
        )
    }
}
