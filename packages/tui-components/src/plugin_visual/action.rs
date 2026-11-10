//! Narrow typed intents of a plugin visual; execution remains with the embedding host.
//!
//! A region names an intent and the adapter-local cells that show it. Registering an
//! adapter never authorizes execution: hosts resolve a region only from a freshly accepted
//! projection of the same adapter at the same accepted revision, then apply their own
//! capability policy.

use bmux_tui::geometry::Rect;

/// Most action regions one plugin visual may retain.
pub const MAX_VISUAL_ACTIONS: usize = 128;
/// Most retained bytes of all action regions of one plugin visual.
pub const MAX_VISUAL_ACTION_BYTES: usize = 4 * 1024 * 1024;

/// A narrow intent from a visible accepted plugin visual region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisualAction {
    /// Open an already classified destination; host capability policy still applies.
    Open(bcode_markdown_render::MarkdownDestination),
    /// Copy exact accepted source text, never reconstructed raster cells.
    Copy(String),
    /// Reveal a semantic artifact through the host's artifact capability.
    Artifact {
        /// Canonical artifact identity.
        artifact_id: String,
    },
    /// Producer-scoped action interpreted by its registered controller only.
    Plugin {
        /// Producer owning the control.
        producer_id: String,
        /// Producer-local control identity.
        control_id: String,
        /// Bounded producer payload.
        payload: serde_json::Value,
    },
}

/// Adapter-local visible action geometry returned by a prepared adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualActionRegion {
    /// Stable adapter-local identity, not a row index.
    pub id: String,
    /// Adapter-local cells (`y` is the adapter row) measured with the accepted rows.
    pub area: Rect,
    /// Narrow host intent.
    pub action: VisualAction,
}

const fn plugin_payload_limits() -> bcode_session_models::retention::ObservationRetentionLimits {
    bcode_session_models::retention::ObservationRetentionLimits {
        scalar_capacity_bytes: 64 * 1024,
        collection_entries: 4096,
        value_nodes: 8192,
        depth: 32,
        copies: 1,
    }
}

/// Retained bytes of `regions`, saturating; `usize::MAX` for unsupported payloads.
#[must_use]
pub fn retained_bytes(regions: &[VisualActionRegion]) -> usize {
    regions.iter().fold(0usize, |total, region| {
        let bytes = match &region.action {
            VisualAction::Copy(text) => text.capacity(),
            VisualAction::Artifact { artifact_id } => artifact_id.capacity(),
            VisualAction::Plugin {
                producer_id,
                control_id,
                payload,
            } => bcode_session_models::retention::ObservationRetentionUsage::json(
                payload,
                plugin_payload_limits(),
            )
            .map_or(usize::MAX, |usage| {
                usage
                    .scalar_capacity_bytes
                    .saturating_add(usage.collection_entries.saturating_mul(64))
            })
            .saturating_add(producer_id.capacity())
            .saturating_add(control_id.capacity()),
            VisualAction::Open(destination) => match destination {
                bcode_markdown_render::MarkdownDestination::Web(url) => url.as_str().len(),
                bcode_markdown_render::MarkdownDestination::Fragment(text) => text.capacity(),
                bcode_markdown_render::MarkdownDestination::LocalPath(path) => {
                    path.as_os_str().len()
                }
                _ => usize::MAX,
            },
        };
        total
            .saturating_add(region.id.capacity())
            .saturating_add(bytes)
    })
}

/// Whether `regions` are bounded, uniquely identified and lie inside a `width` by `rows`
/// visual.
#[must_use]
pub fn supported(regions: &[VisualActionRegion], width: u16, rows: usize) -> bool {
    if regions.len() > MAX_VISUAL_ACTIONS {
        return false;
    }
    let mut ids = std::collections::BTreeSet::new();
    regions.iter().all(|region| {
        !region.id.is_empty()
            && region.id.len() <= 256
            && ids.insert(&region.id)
            && !region.area.is_empty()
            && region.area.right() <= width
            && usize::from(region.area.bottom()) <= rows
            && match &region.action {
                VisualAction::Copy(text) => text.len() <= 256 * 1024,
                VisualAction::Artifact { artifact_id } => {
                    !artifact_id.is_empty() && artifact_id.len() <= 512
                }
                VisualAction::Plugin {
                    producer_id,
                    control_id,
                    payload,
                } => {
                    !producer_id.is_empty()
                        && producer_id.len() <= 256
                        && !control_id.is_empty()
                        && control_id.len() <= 256
                        && bcode_session_models::retention::ObservationRetentionUsage::json(
                            payload,
                            plugin_payload_limits(),
                        )
                        .is_ok()
                }
                VisualAction::Open(destination) => matches!(
                    destination,
                    bcode_markdown_render::MarkdownDestination::Web(_)
                        | bcode_markdown_render::MarkdownDestination::LocalPath(_)
                        | bcode_markdown_render::MarkdownDestination::Fragment(_)
                ),
            }
    }) && retained_bytes(regions) <= MAX_VISUAL_ACTION_BYTES
}
