//! Transcript layout projection preparation.

use bmux_tui::geometry::Rect;

use super::app::{BmuxApp, LiveToolPreviewState};
use super::pending_submission::PendingSubmission;
use super::render;
use super::transcript::TranscriptItem;
use super::transcript_layout::{
    TranscriptLayoutFingerprint, TranscriptLayoutSignature, TranscriptLayoutSpec,
};
use bcode_config::TuiDiffViewerConfig;
use std::time::Instant;

/// Prepare transcript layout and viewport projections for a frame body.
pub fn prepare_for_body(app: &mut BmuxApp, body: Rect) {
    let initial_transcript_area = render::transcript_area_for_body(app, body);
    sync_layout(app, initial_transcript_area.width);
    sync_viewport(app, initial_transcript_area);
    let latest_bar_height = u16::from(app.newer_transcript_content_below());
    if latest_bar_height == 0 {
        return;
    }

    let body = Rect::new(
        body.x,
        body.y,
        body.width,
        body.height.saturating_sub(latest_bar_height),
    );
    let transcript_area = render::transcript_area_for_body(app, body);
    sync_layout(app, transcript_area.width);
    sync_viewport(app, transcript_area);
}

fn sync_viewport(app: &mut BmuxApp, transcript_area: Rect) {
    app.sync_transcript_scroll_max(
        max_scroll_offset(app, transcript_area),
        max_bottom_overscroll(transcript_area),
        app.transcript_layout().total_rows(),
        transcript_area.height,
    );
    app.sync_transcript_anchor_requests();
}

fn max_scroll_offset(app: &BmuxApp, area: Rect) -> usize {
    if area.is_empty() || app.transcript().is_empty() && app.pending_submissions().is_empty() {
        return 0;
    }
    app.transcript_layout()
        .total_rows()
        .saturating_sub(usize::from(area.height))
}

fn max_bottom_overscroll(area: Rect) -> usize {
    usize::from(area.height).saturating_sub(1)
}

fn sync_layout(app: &mut BmuxApp, width: u16) {
    let started = Instant::now();
    let elapsed_dirty_visuals = app.drain_elapsed_dirty_visuals();
    let mut transcript_layout = std::mem::take(app.transcript_layout_mut());
    let input = TranscriptLayoutInput::from_app(app, width);
    let fingerprint = input.fingerprint();
    let structural_fingerprint = input.structural_fingerprint();
    if transcript_layout.is_current(&fingerprint) {
        transcript_layout
            .record_cache_hit(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));
        *app.transcript_layout_mut() = transcript_layout;
        return;
    }
    let mut dirty_visuals = input.plugin_host.map_or_else(
        std::collections::BTreeSet::new,
        crate::plugin_tui::PluginTuiPresentation::drain_dirty_visuals,
    );
    dirty_visuals.extend(elapsed_dirty_visuals);
    if !dirty_visuals.is_empty() && transcript_layout.structure_is_current(&structural_fingerprint)
    {
        transcript_layout.sync_visuals(
            fingerprint,
            &dirty_visuals,
            |index| transcript_item_signature(&input.transcript[index], &input),
            |index| {
                render::transcript_item_rows(
                    input.transcript,
                    input.live_tool_previews,
                    index,
                    input.width,
                    input.plugin_host,
                    input.diff_viewer_config,
                )
            },
        );
        *app.transcript_layout_mut() = transcript_layout;
        return;
    }
    transcript_layout.sync(TranscriptLayoutSpec {
        width,
        fingerprint,
        structural_fingerprint,
        transcript_len: input.transcript.len(),
        pending_len: input.pending.len(),
        transcript_signature: |index| transcript_item_signature(&input.transcript[index], &input),
        transcript_rows: |index| {
            render::transcript_item_rows(
                input.transcript,
                input.live_tool_previews,
                index,
                input.width,
                input.plugin_host,
                input.diff_viewer_config,
            )
        },
        transcript_invocation_id: |index: usize| {
            input.transcript[index]
                .visual_invocation_id()
                .map(ToOwned::to_owned)
        },
        pending_signature: |index| {
            render::pending_submission_signature(&input.pending[index], width)
        },
        pending_rows: |index| render::pending_submission_rows(&input.pending[index], width),
        history_banner_signature: || {
            render::history_banner_text(input.has_older_history, input.loading_older_history)
                .map(|text| TranscriptLayoutSignature::new(format!("history:{width}:{text}")))
        },
        history_banner_rows: || {
            render::history_banner_rows(input.has_older_history, input.loading_older_history)
        },
        reset: || false,
    });
    *app.transcript_layout_mut() = transcript_layout;
}

struct TranscriptLayoutInput<'a> {
    width: u16,
    transcript: &'a [TranscriptItem],
    live_tool_previews: &'a std::collections::BTreeMap<String, LiveToolPreviewState>,
    plugin_host: Option<&'a crate::plugin_tui::PluginTuiPresentation>,
    diff_viewer_config: TuiDiffViewerConfig,
    pending: &'a [PendingSubmission],
    elapsed_layout_revision: u64,
    transcript_structural_projection_revision: u64,
    pending_submissions_projection_revision: u64,
    has_older_history: bool,
    loading_older_history: bool,
}

impl<'a> TranscriptLayoutInput<'a> {
    fn from_app(app: &'a BmuxApp, width: u16) -> Self {
        Self {
            width,
            transcript: app.transcript(),
            live_tool_previews: app.live_tool_previews(),
            plugin_host: app.plugin_presentation(),
            diff_viewer_config: app.effective_diff_viewer_config(),
            pending: app.pending_submissions(),
            elapsed_layout_revision: app.elapsed_layout_revision(),
            transcript_structural_projection_revision: app
                .transcript_structural_projection_revision(),
            pending_submissions_projection_revision: app.pending_submissions_projection_revision(),
            has_older_history: app.has_older_history(),
            loading_older_history: app.loading_older_history(),
        }
    }

    fn fingerprint(&self) -> TranscriptLayoutFingerprint {
        let presentation = self.plugin_host.map_or_else(
            || "none".to_owned(),
            |host| {
                format!(
                    "{}:{}:{}",
                    std::ptr::from_ref(host).addr(),
                    host.revision(),
                    host.visual_generation()
                )
            },
        );
        TranscriptLayoutFingerprint::new(format!(
            "{};elapsed-rev:{};visual-generation:{presentation}",
            self.structural_fingerprint().as_str(),
            self.elapsed_layout_revision
        ))
    }

    fn structural_fingerprint(&self) -> TranscriptLayoutFingerprint {
        let presentation = self.plugin_host.map_or_else(
            || "none".to_owned(),
            |host| format!("{}:{}", std::ptr::from_ref(host).addr(), host.revision()),
        );
        TranscriptLayoutFingerprint::new(format!(
            "width:{};diff:{:?};history:{}:{};presentation:{presentation};transcript-rev:{};transcript-len:{};pending-rev:{};pending-len:{}",
            self.width,
            self.diff_viewer_config,
            self.has_older_history,
            self.loading_older_history,
            self.transcript_structural_projection_revision,
            self.transcript.len(),
            self.pending_submissions_projection_revision,
            self.pending.len()
        ))
    }
}

#[cfg(test)]
#[must_use]
pub fn test_layout_signature(
    item: &TranscriptItem,
    width: u16,
    plugin_host: Option<&crate::plugin_tui::PluginTuiPresentation>,
) -> TranscriptLayoutSignature {
    let transcript = [item.clone()];
    let live_tool_previews = std::collections::BTreeMap::new();
    let pending = [];
    let input = TranscriptLayoutInput {
        width,
        transcript: &transcript,
        live_tool_previews: &live_tool_previews,
        plugin_host,
        diff_viewer_config: TuiDiffViewerConfig::default(),
        pending: &pending,
        elapsed_layout_revision: 0,
        transcript_structural_projection_revision: 0,
        pending_submissions_projection_revision: 0,
        has_older_history: false,
        loading_older_history: false,
    };
    transcript_item_signature(item, &input)
}

fn transcript_item_signature(
    item: &TranscriptItem,
    input: &TranscriptLayoutInput<'_>,
) -> TranscriptLayoutSignature {
    let base = render::transcript_item_signature(item, input.width, ());
    let presentation_generation = input.plugin_host.map_or_else(
        || "none".to_owned(),
        |host| format!("{}:{}", std::ptr::from_ref(host).addr(), host.revision()),
    );
    let live_preview_revision = match item.kind() {
        super::transcript::TranscriptItemKind::LiveToolPreviewAnchor { tool_call_id, .. } => input
            .live_tool_previews
            .get(tool_call_id)
            .map_or(0, |preview| preview.revision),
        _ => 0,
    };
    let visual_revision = item.visual_invocation_id().map_or(0, |invocation_id| {
        input
            .plugin_host
            .map_or(0, |host| host.visual_revision(invocation_id))
    });
    TranscriptLayoutSignature::new(format!(
        "{};presentation-generation:{presentation_generation};visual-rev:{visual_revision};live-preview-rev:{live_preview_revision}",
        base.as_str()
    ))
}
