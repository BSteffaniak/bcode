//! Typed renderer-boundary actions; execution remains with the embedding host.
use bmux_tui::geometry::Rect;

/// A narrow intent from a visible accepted transcript region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptAction {
    /// Open an already classified destination; host capability policy still applies.
    Open(bcode_markdown_render::MarkdownDestination),
    /// Copy exact accepted source text, never reconstructed raster cells.
    Copy(String),
    /// Reveal a semantic artifact through the host's artifact capability.
    Artifact { artifact_id: String },
    /// Producer-scoped action interpreted by its registered controller only.
    Plugin {
        producer_id: String,
        control_id: String,
        payload: serde_json::Value,
    },
    /// Local presentation behavior (disclosure, footnote, diagram inspection).
    Presentation {
        contribution_id: String,
        kind: Box<bcode_markdown_render::MarkdownContributionKind>,
    },
}

/// Item-local visible action geometry returned by a prepared adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptActionRegion {
    /// Stable adapter-local identity, not a row index.
    pub id: String,
    /// Item-local geometry measured with the accepted visual.
    pub area: Rect,
    /// Narrow host intent.
    pub action: TranscriptAction,
}

pub(super) fn retained_bytes(regions: &[TranscriptActionRegion]) -> usize {
    regions.iter().fold(0usize, |total, region| {
        let bytes = match &region.action {
            TranscriptAction::Copy(text) => text.capacity(),
            TranscriptAction::Artifact { artifact_id } => artifact_id.capacity(),
            TranscriptAction::Plugin {
                producer_id,
                control_id,
                payload,
            } => {
                let limits = bcode_session_models::retention::ObservationRetentionLimits {
                    scalar_capacity_bytes: 64 * 1024,
                    collection_entries: 4096,
                    value_nodes: 8192,
                    depth: 32,
                    copies: 1,
                };
                bcode_session_models::retention::ObservationRetentionUsage::json(payload, limits)
                    .map_or(usize::MAX, |usage| {
                        usage
                            .scalar_capacity_bytes
                            .saturating_add(usage.collection_entries.saturating_mul(64))
                    })
                    .saturating_add(producer_id.capacity())
                    .saturating_add(control_id.capacity())
            }
            TranscriptAction::Open(destination) => match destination {
                bcode_markdown_render::MarkdownDestination::Web(url) => url.as_str().len(),
                bcode_markdown_render::MarkdownDestination::Fragment(text) => text.capacity(),
                bcode_markdown_render::MarkdownDestination::LocalPath(path) => {
                    path.as_os_str().len()
                }
                _ => usize::MAX,
            },
            TranscriptAction::Presentation { .. } => usize::MAX,
        };
        total
            .saturating_add(region.id.capacity())
            .saturating_add(bytes)
    })
}

pub(super) fn supported(regions: &[TranscriptActionRegion], width: u16, rows: usize) -> bool {
    if regions.len() > 128 {
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
                TranscriptAction::Copy(text) => text.len() <= 256 * 1024,
                TranscriptAction::Artifact { artifact_id } => {
                    !artifact_id.is_empty() && artifact_id.len() <= 512
                }
                TranscriptAction::Plugin {
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
                            bcode_session_models::retention::ObservationRetentionLimits {
                                scalar_capacity_bytes: 64 * 1024,
                                collection_entries: 4096,
                                value_nodes: 8192,
                                depth: 32,
                                copies: 1,
                            },
                        )
                        .is_ok()
                }
                TranscriptAction::Open(destination) => matches!(
                    destination,
                    bcode_markdown_render::MarkdownDestination::Web(_)
                        | bcode_markdown_render::MarkdownDestination::LocalPath(_)
                        | bcode_markdown_render::MarkdownDestination::Fragment(_)
                ),
                TranscriptAction::Presentation { .. } => false,
            }
    })
}
