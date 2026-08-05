//! Transcript layout projection preparation.

use bmux_tui::geometry::Rect;

use super::app::{BmuxApp, TranscriptItems};
use super::pending_submission::PendingSubmission;
use super::render;
use super::transcript::TranscriptItem;
use super::transcript_layout::{
    TranscriptLayoutFingerprint, TranscriptLayoutSignature, TranscriptLayoutSpec,
};
use bcode_config::TuiDiffViewerConfig;
use std::time::Instant;

const MAX_DIRTY_VISUALS_PER_LAYOUT_SYNC: usize = 64;

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

fn transcript_item_rows(
    app: &BmuxApp,
    item: &TranscriptItem,
    input: &TranscriptLayoutInput<'_>,
) -> Vec<bmux_tui::prelude::Line> {
    if let Some(interaction) = item.interaction()
        && !interaction.resolved
        && let Some(height) = input
            .active_interaction_layout
            .filter(|(id, _)| *id == interaction.interaction_id)
            .map(|(_, height)| height)
    {
        return vec![bmux_tui::prelude::Line::default(); usize::from(height.max(1))];
    }
    let markdown = render::transcript_markdown_projection_for_layout(app, item, input.width);
    render::transcript_item_rows_from_item_with_markdown(
        item,
        input.width,
        input.plugin_host,
        input.diff_viewer_config,
        markdown.as_deref(),
    )
}

fn sync_layout(app: &mut BmuxApp, width: u16) {
    render::set_markdown_details_open(app.markdown_details_open());
    render::set_plugin_visual_theme(&app.presented_theme());
    let started = Instant::now();
    let elapsed_dirty_visuals =
        app.drain_elapsed_dirty_visuals_bounded(MAX_DIRTY_VISUALS_PER_LAYOUT_SYNC);
    let transcript_dirty_items = app.drain_transcript_dirty_items();
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
    if !transcript_dirty_items.is_empty()
        && transcript_layout.structure_is_current(&structural_fingerprint)
    {
        transcript_layout.sync_transcript_entries(
            fingerprint,
            &transcript_dirty_items,
            |index| transcript_item_signature(&input.transcript[index], &input),
            |index| transcript_item_rows(app, &input.transcript[index], &input),
        );
        *app.transcript_layout_mut() = transcript_layout;
        return;
    }
    let mut dirty_visuals =
        input
            .plugin_host
            .map_or_else(std::collections::BTreeSet::new, |host| {
                host.drain_dirty_visuals_bounded(
                    MAX_DIRTY_VISUALS_PER_LAYOUT_SYNC.saturating_sub(elapsed_dirty_visuals.len()),
                )
            });
    dirty_visuals.extend(elapsed_dirty_visuals);
    if !dirty_visuals.is_empty() && transcript_layout.structure_is_current(&structural_fingerprint)
    {
        transcript_layout.sync_visuals(
            fingerprint,
            &dirty_visuals,
            |index| transcript_item_signature(&input.transcript[index], &input),
            |index| transcript_item_rows(app, &input.transcript[index], &input),
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
        transcript_rows: |index| transcript_item_rows(app, &input.transcript[index], &input),
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
    transcript: TranscriptItems<'a>,
    plugin_host: Option<&'a crate::plugin_tui::PluginTuiPresentation>,
    diff_viewer_config: TuiDiffViewerConfig,
    pending: &'a [PendingSubmission],
    elapsed_layout_revision: u64,
    transcript_projection_revision: u64,
    active_interaction_layout: Option<(&'a str, u16)>,
    markdown_presentation_revision: u64,
    pending_submissions_projection_revision: u64,
    theme_fingerprint: u64,
    has_older_history: bool,
    loading_older_history: bool,
}

impl<'a> TranscriptLayoutInput<'a> {
    fn from_app(app: &'a BmuxApp, width: u16) -> Self {
        Self {
            width,
            transcript: app.transcript(),
            plugin_host: app.plugin_presentation(),
            diff_viewer_config: app.effective_diff_viewer_config(),
            pending: app.pending_submissions(),
            elapsed_layout_revision: app.elapsed_layout_revision(),
            transcript_projection_revision: app.transcript_projection_revision(),
            active_interaction_layout: app.active_interaction_layout(),
            markdown_presentation_revision: app.markdown_presentation_revision(),
            pending_submissions_projection_revision: app.pending_submissions_projection_revision(),
            theme_fingerprint: app.presented_theme().fingerprint,
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
            "width:{};diff:{:?};theme:{};history:{}:{};presentation:{presentation};transcript-rev:{};markdown-presentation-rev:{};transcript-len:{};pending-rev:{};pending-len:{}",
            self.width,
            self.diff_viewer_config,
            self.theme_fingerprint,
            self.has_older_history,
            self.loading_older_history,
            self.transcript_projection_revision,
            self.markdown_presentation_revision,
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
    let pending = [];
    let input = TranscriptLayoutInput {
        width,
        transcript: TranscriptItems::new(&transcript, &[]),
        plugin_host,
        diff_viewer_config: TuiDiffViewerConfig::default(),
        pending: &pending,
        elapsed_layout_revision: 0,
        transcript_projection_revision: 0,
        active_interaction_layout: None,
        markdown_presentation_revision: 0,
        pending_submissions_projection_revision: 0,
        theme_fingerprint: 0,
        has_older_history: false,
        loading_older_history: false,
    };
    transcript_item_signature(item, &input)
}

#[cfg(test)]
#[must_use]
pub fn test_layout_signature_with_theme(
    item: &TranscriptItem,
    width: u16,
    theme_fingerprint: u64,
) -> TranscriptLayoutSignature {
    let transcript = [item.clone()];
    let pending = [];
    let input = TranscriptLayoutInput {
        width,
        transcript: TranscriptItems::new(&transcript, &[]),
        plugin_host: None,
        diff_viewer_config: TuiDiffViewerConfig::default(),
        pending: &pending,
        elapsed_layout_revision: 0,
        transcript_projection_revision: 0,
        active_interaction_layout: None,
        markdown_presentation_revision: 0,
        pending_submissions_projection_revision: 0,
        theme_fingerprint,
        has_older_history: false,
        loading_older_history: false,
    };
    transcript_item_signature(item, &input)
}

#[cfg(test)]
#[test]
fn theme_fingerprint_changes_versioned_transcript_layout_signature_exactly() {
    let item = TranscriptItem::with_format(
        "Bcode",
        "# themed markdown".to_owned(),
        bcode_session_view_models::TextFormat::Markdown,
    );
    let first = test_layout_signature_with_theme(&item, 80, 11);
    let repeated = test_layout_signature_with_theme(&item, 80, 11);
    let changed = test_layout_signature_with_theme(&item, 80, 12);

    assert_eq!(first, repeated);
    assert_ne!(first, changed);
    assert!(first.as_str().contains("theme:11"));
    assert!(changed.as_str().contains("theme:12"));
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
    let visual_revision = item.visual_invocation_id().map_or(0, |invocation_id| {
        input
            .plugin_host
            .map_or(0, |host| host.visual_revision(invocation_id))
    });
    TranscriptLayoutSignature::new(format!(
        "{};theme:{};presentation-generation:{presentation_generation};visual-rev:{visual_revision}",
        base.as_str(),
        input.theme_fingerprint
    ))
}
