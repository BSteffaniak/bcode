//! Transcript layout projection preparation.

use bcode_config::TuiInlineDiffConfig;
use bmux_tui::geometry::Rect;

use super::app::{BmuxApp, LiveToolPreviewState};
use super::pending_submission::PendingSubmission;
use super::render;
use super::transcript::TranscriptItem;
use super::transcript_layout::{
    TranscriptLayoutFingerprint, TranscriptLayoutSignature, TranscriptLayoutSpec,
};

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
    let mut transcript_layout = std::mem::take(app.transcript_layout_mut());
    let input = TranscriptLayoutInput::from_app(app, width);
    let fingerprint = input.fingerprint();
    if transcript_layout.is_current(&fingerprint) {
        *app.transcript_layout_mut() = transcript_layout;
        return;
    }
    transcript_layout.sync(TranscriptLayoutSpec {
        width,
        fingerprint,
        transcript_len: input.transcript.len(),
        pending_len: input.pending.len(),
        transcript_signature: |index| transcript_item_signature(&input.transcript[index], &input),
        transcript_rows: |index| {
            render::transcript_item_rows(
                input.transcript,
                input.live_tool_previews,
                index,
                input.width,
                input.inline_diff_config,
            )
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
    pending: &'a [PendingSubmission],
    transcript_projection_revision: u64,
    pending_submissions_projection_revision: u64,
    has_older_history: bool,
    loading_older_history: bool,
    inline_diff_config: TuiInlineDiffConfig,
}

impl<'a> TranscriptLayoutInput<'a> {
    fn from_app(app: &'a BmuxApp, width: u16) -> Self {
        Self {
            width,
            transcript: app.transcript(),
            live_tool_previews: app.live_tool_previews(),
            pending: app.pending_submissions(),
            transcript_projection_revision: app.transcript_projection_revision(),
            pending_submissions_projection_revision: app.pending_submissions_projection_revision(),
            has_older_history: app.has_older_history(),
            loading_older_history: app.loading_older_history(),
            inline_diff_config: app.inline_diff_config(),
        }
    }

    fn fingerprint(&self) -> TranscriptLayoutFingerprint {
        let transcript = self
            .transcript
            .iter()
            .map(|item| {
                let elapsed = render::terminal_elapsed_signature_fragment(item).unwrap_or_default();
                let live_preview_revision = match item.kind() {
                    super::transcript::TranscriptItemKind::LiveToolPreviewAnchor {
                        tool_call_id,
                        ..
                    } => self
                        .live_tool_previews
                        .get(tool_call_id)
                        .map_or(0, |preview| preview.revision),
                    _ => 0,
                };
                format!(
                    "{}:{}:{elapsed}:live-preview:{live_preview_revision}",
                    item.id().get(),
                    item.revision()
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let pending = self
            .pending
            .iter()
            .map(|pending| format!("{}:{:?}", pending.text(), pending.state()))
            .collect::<Vec<_>>()
            .join(",");
        TranscriptLayoutFingerprint::new(format!(
            "width:{};diff:{:?};history:{}:{};transcript-rev:{};transcript:{};pending-rev:{};pending:{}",
            self.width,
            self.inline_diff_config,
            self.has_older_history,
            self.loading_older_history,
            self.transcript_projection_revision,
            transcript,
            self.pending_submissions_projection_revision,
            pending
        ))
    }
}

fn transcript_item_signature(
    item: &TranscriptItem,
    input: &TranscriptLayoutInput<'_>,
) -> TranscriptLayoutSignature {
    let base = render::transcript_item_signature(item, input.width, input.inline_diff_config);
    let live_preview_revision = match item.kind() {
        super::transcript::TranscriptItemKind::LiveToolPreviewAnchor { tool_call_id, .. } => input
            .live_tool_previews
            .get(tool_call_id)
            .map_or(0, |preview| preview.revision),
        _ => 0,
    };
    TranscriptLayoutSignature::new(format!(
        "{};live-preview-rev:{live_preview_revision}",
        base.as_str()
    ))
}
