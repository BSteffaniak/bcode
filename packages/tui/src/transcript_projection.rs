//! Transcript layout projection preparation.

use bmux_tui::geometry::Rect;

use super::app::BmuxApp;
use super::pending_submission::PendingSubmission;
use super::render;
use super::transcript::TranscriptItem;
#[cfg(test)]
use super::transcript_document::TranscriptDocument;
use super::transcript_document::TranscriptItems;
use super::transcript_layout::{
    TranscriptLayoutFingerprint, TranscriptLayoutRows, TranscriptLayoutSignature,
    TranscriptLayoutSpec,
};
use bcode_config::TuiDiffViewerConfig;
use std::time::Instant;

const MAX_DIRTY_VISUALS_PER_LAYOUT_SYNC: usize = 64;

/// Prepare transcript layout and viewport projections for a frame body.
pub fn prepare_for_body(app: &mut BmuxApp, body: Rect) {
    app.begin_transcript_presentation();
    let initial_transcript_area = render::transcript_area_for_body(app, body);
    app.begin_transcript_geometry_update();
    sync_layout(app, initial_transcript_area.width);
    // Chrome is decided against the full body. A bar cannot create its own reason
    // to exist by reducing the viewport, and measurement never consumes navigation.
    let latest_bar_height =
        u16::from(app.needs_latest_bar_at_height(initial_transcript_area.height));
    let body = Rect::new(
        body.x,
        body.y,
        body.width,
        body.height.saturating_sub(latest_bar_height),
    );
    let transcript_area = render::transcript_area_for_body(app, body);
    sync_viewport(app, transcript_area);
}

#[cfg(test)]
pub fn sync_layout_for_test(app: &mut BmuxApp, width: u16) {
    sync_layout(app, width);
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
) -> TranscriptLayoutRows {
    if let Some(interaction) = item.interaction()
        && !interaction.resolved
        && let Some(height) = input
            .active_interaction_layout
            .filter(|(id, _)| *id == interaction.interaction_id)
            .map(|(_, height)| height)
    {
        return TranscriptLayoutRows::BlankSpan(usize::from(height.max(1)));
    }
    let markdown = render::transcript_markdown_projection_for_layout(
        app,
        item,
        render::transcript_item_body_width(item, input.width),
    );
    let (rows, anchors, selection) = render::transcript_item_layout_with_keymap(
        item,
        input.width,
        input.plugin_host,
        input.diff_viewer_config,
        markdown.as_deref(),
        app.effective_keymap(),
    );
    // A grouped item may render no rows; it then has no Markdown body.
    if let Some(projection) = markdown.filter(|_| !rows.is_empty())
        && matches!(
            item.kind(),
            super::transcript::TranscriptItemKind::AssistantMessage
                | super::transcript::TranscriptItemKind::UserMessage
                | super::transcript::TranscriptItemKind::System
        )
    {
        let container =
            render::TranscriptItemLayout::resolve(&app.presented_theme(), item, input.width);
        let body_start = rows.len().saturating_sub(
            projection
                .lines
                .len()
                .saturating_add(1)
                .saturating_add(container.bottom_rows()),
        );
        TranscriptLayoutRows::Markdown {
            rows,
            anchors,
            projection,
            body_start,
        }
    } else {
        TranscriptLayoutRows::Selected {
            rows,
            anchors,
            selection: selection.into_iter().collect(),
        }
    }
}

fn sync_layout(app: &mut BmuxApp, width: u16) {
    // Include asynchronous adapter/Markdown acceptance and width changes, not
    // just semantic event application, in the anchor reconciliation boundary.
    app.capture_stable_transcript_anchor();
    render::set_markdown_details_open(app.markdown_details_open());
    render::set_plugin_visual_theme(&app.presented_theme());
    app.refresh_structure_turns();
    let started = Instant::now();
    let elapsed_dirty_visuals =
        app.drain_elapsed_dirty_visuals_bounded(MAX_DIRTY_VISUALS_PER_LAYOUT_SYNC);
    let mut transcript_dirty_items = app.drain_transcript_dirty_items();
    if record_cache_hit(app, width, started) {
        return;
    }
    // Groups change only when the layout inputs do; items whose group role
    // changed are laid out again alongside the source damage.
    transcript_dirty_items.extend(app.sync_structure_groups());
    render::set_structure(app.structure_snapshot());
    let mut transcript_layout = std::mem::take(app.transcript_layout_mut());
    let elapsed_only;
    {
        let input = TranscriptLayoutInput::from_app(app, width);
        let fingerprint = input.fingerprint();
        let structural_fingerprint = input.structural_fingerprint();
        let mut dirty_visuals =
            input
                .plugin_host
                .map_or_else(std::collections::BTreeSet::new, |host| {
                    host.drain_dirty_visuals_bounded(
                        MAX_DIRTY_VISUALS_PER_LAYOUT_SYNC
                            .saturating_sub(elapsed_dirty_visuals.len()),
                    )
                });
        elapsed_only = elapsed_dirty_visuals
            .difference(&dirty_visuals)
            .filter(|invocation| {
                !transcript_dirty_items.iter().any(|&index| {
                    input.transcript[index].visual_invocation_id() == Some(invocation.as_str())
                })
            })
            .cloned()
            .collect();
        dirty_visuals.extend(elapsed_dirty_visuals);
        if (!dirty_visuals.is_empty() || !transcript_dirty_items.is_empty())
            && transcript_layout.structure_is_current(&structural_fingerprint)
        {
            if !transcript_dirty_items.is_empty() {
                transcript_layout.sync_transcript_entries(
                    fingerprint.clone(),
                    &transcript_dirty_items,
                    |index| transcript_item_signature(&input.transcript[index], &input),
                    |index| transcript_item_rows(app, &input.transcript[index], &input),
                );
            }
            if !dirty_visuals.is_empty() {
                transcript_layout.sync_visuals(
                    fingerprint,
                    &dirty_visuals,
                    |index| transcript_item_signature(&input.transcript[index], &input),
                    |index| transcript_item_rows(app, &input.transcript[index], &input),
                );
            }
            transcript_layout.suppress_visual_content_changes(&elapsed_only);
            *app.transcript_layout_mut() = transcript_layout;
            return;
        }
        transcript_layout.sync(TranscriptLayoutSpec {
            width,
            fingerprint,
            structural_fingerprint,
            transcript_len: input.transcript.len(),
            pending_len: input.pending.len(),
            transcript_signature: |index| {
                transcript_item_signature(&input.transcript[index], &input)
            },
            transcript_rows: |index| transcript_item_rows(app, &input.transcript[index], &input),
            transcript_invocation_id: |index: usize| {
                input.transcript[index]
                    .visual_invocation_id()
                    .map(ToOwned::to_owned)
            },
            pending_signature: |index| {
                render::pending_submission_signature(&input.pending[index], width)
            },
            pending_rows: |index| {
                render::pending_submission_rows(&input.pending[index], width).into()
            },
            history_banner_signature: || {
                render::history_banner_text(input.has_older_history, input.loading_older_history)
                    .map(|text| TranscriptLayoutSignature::new(format!("history:{width}:{text}")))
            },
            history_banner_rows: || {
                render::history_banner_rows(input.has_older_history, input.loading_older_history)
                    .into()
            },
            reset: || false,
        });
    }
    transcript_layout.suppress_visual_content_changes(&elapsed_only);
    *app.transcript_layout_mut() = transcript_layout;
}

/// Record a cache hit and return `true` when the retained layout already
/// represents the current inputs; groups and rows are then left untouched.
fn record_cache_hit(app: &mut BmuxApp, width: u16, started: Instant) -> bool {
    if !app
        .transcript_layout()
        .is_current(&TranscriptLayoutInput::from_app(app, width).fingerprint())
    {
        return false;
    }
    render::set_structure(app.structure_snapshot());
    app.transcript_layout_mut()
        .record_cache_hit(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));
    true
}

struct TranscriptLayoutInput<'a> {
    app: &'a BmuxApp,
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
    structure_policy_fingerprint: u64,
    structure_revision: u64,
    structure_turns: String,
    has_older_history: bool,
    loading_older_history: bool,
}

impl<'a> TranscriptLayoutInput<'a> {
    fn from_app(app: &'a BmuxApp, width: u16) -> Self {
        Self {
            app,
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
            structure_policy_fingerprint: app.structure().policy_fingerprint(),
            structure_revision: app.structure().revision(),
            structure_turns: app.structure().turn_key(),
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
            "{};transcript-rev:{};elapsed-rev:{};visual-generation:{presentation};structure-rev:{}",
            self.structural_fingerprint().as_str(),
            self.transcript_projection_revision,
            self.elapsed_layout_revision,
            self.structure_revision
        ))
    }

    fn structural_fingerprint(&self) -> TranscriptLayoutFingerprint {
        let presentation = self.plugin_host.map_or_else(
            || "none".to_owned(),
            |host| format!("{}:{}", std::ptr::from_ref(host).addr(), host.revision()),
        );
        TranscriptLayoutFingerprint::new(format!(
            "width:{};diff:{:?};theme:{};structure:{}:{};history:{}:{};presentation:{presentation};markdown-presentation-rev:{};transcript-len:{};pending-rev:{};pending-len:{}",
            self.width,
            self.diff_viewer_config,
            self.theme_fingerprint,
            self.structure_policy_fingerprint,
            self.structure_turns,
            self.has_older_history,
            self.loading_older_history,
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
    let mut transcript = TranscriptDocument::default();
    transcript.push_ephemeral("test".to_owned(), item.clone());
    let pending = [];
    let app = BmuxApp::new_with_history(None, &[], &[], false);
    let input = TranscriptLayoutInput {
        app: &app,
        width,
        transcript: transcript.items(),
        plugin_host,
        diff_viewer_config: TuiDiffViewerConfig::default(),
        pending: &pending,
        elapsed_layout_revision: 0,
        transcript_projection_revision: 0,
        active_interaction_layout: None,
        markdown_presentation_revision: 0,
        pending_submissions_projection_revision: 0,
        theme_fingerprint: 0,
        structure_policy_fingerprint: 0,
        structure_revision: 0,
        structure_turns: String::new(),
        has_older_history: false,
        loading_older_history: false,
    };
    transcript_item_signature(item, &input)
}

#[cfg(test)]
#[must_use]
pub fn test_layout_signature_with_interaction_height(
    item: &TranscriptItem,
    width: u16,
    interaction_id: &str,
    height: u16,
) -> TranscriptLayoutSignature {
    let mut transcript = TranscriptDocument::default();
    transcript.push_ephemeral("test".to_owned(), item.clone());
    let pending = [];
    let app = BmuxApp::new_with_history(None, &[], &[], false);
    let input = TranscriptLayoutInput {
        app: &app,
        width,
        transcript: transcript.items(),
        plugin_host: None,
        diff_viewer_config: TuiDiffViewerConfig::default(),
        pending: &pending,
        elapsed_layout_revision: 0,
        transcript_projection_revision: 0,
        active_interaction_layout: Some((interaction_id, height)),
        markdown_presentation_revision: 0,
        pending_submissions_projection_revision: 0,
        theme_fingerprint: 0,
        structure_policy_fingerprint: 0,
        structure_revision: 0,
        structure_turns: String::new(),
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
    let mut transcript = TranscriptDocument::default();
    transcript.push_ephemeral("test".to_owned(), item.clone());
    let pending = [];
    let app = BmuxApp::new_with_history(None, &[], &[], false);
    let input = TranscriptLayoutInput {
        app: &app,
        width,
        transcript: transcript.items(),
        plugin_host: None,
        diff_viewer_config: TuiDiffViewerConfig::default(),
        pending: &pending,
        elapsed_layout_revision: 0,
        transcript_projection_revision: 0,
        active_interaction_layout: None,
        markdown_presentation_revision: 0,
        pending_submissions_projection_revision: 0,
        theme_fingerprint,
        structure_policy_fingerprint: 0,
        structure_revision: 0,
        structure_turns: String::new(),
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

#[cfg(test)]
#[test]
fn accepted_exact_markdown_projection_changes_the_layout_signature() {
    let mut app = BmuxApp::new_with_history(None, &[], &[], false);
    app.push_ephemeral_system_markdown("A finalized **reply**".to_owned());
    let width = 80;
    let signature = |app: &BmuxApp| {
        let input = TranscriptLayoutInput::from_app(app, width);
        let signature = transcript_item_signature(&input.transcript[0], &input);
        drop(input);
        signature
    };
    let before = signature(&app);
    // Accepting the exact projection must re-lay out rows painted from a previous one.
    let item = app.transcript()[0].clone();
    let options = render::markdown_render_options(
        &app,
        &item,
        render::transcript_item_body_width(&item, width),
    );
    let _ = app.transcript_markdown_cache().project(&item, options);
    let after = signature(&app);
    assert_ne!(before, after);
    assert_eq!(after, signature(&app));
    drop(app);
}

fn transcript_item_signature(
    item: &TranscriptItem,
    input: &TranscriptLayoutInput<'_>,
) -> TranscriptLayoutSignature {
    let base = render::transcript_item_signature(item, input.width, ());
    // Rows painted from a superseded (previous-revision) Markdown projection must be laid out
    // again once the exact projection is accepted; the source revision alone does not change.
    let markdown_exact = render::transcript_markdown_selection_projection(
        input.app,
        item,
        render::transcript_item_body_width(item, input.width),
    )
    .is_some();
    let presentation_generation = input.plugin_host.map_or_else(
        || "none".to_owned(),
        |host| format!("{}:{}", std::ptr::from_ref(host).addr(), host.revision()),
    );
    let visual_revision = item.visual_invocation_id().map_or(0, |invocation_id| {
        input
            .plugin_host
            .map_or(0, |host| host.visual_revision(invocation_id))
    });
    let interaction_layout = item.interaction().and_then(|interaction| {
        input
            .active_interaction_layout
            .filter(|(id, _)| *id == interaction.interaction_id)
            .map(|(id, height)| format!("{id}:{height}"))
    });
    TranscriptLayoutSignature::new(format!(
        "{};theme:{};presentation-generation:{presentation_generation};visual-rev:{visual_revision};interaction-layout:{};markdown-exact:{markdown_exact}",
        base.as_str(),
        input.theme_fingerprint,
        interaction_layout.as_deref().unwrap_or("none")
    ))
}
