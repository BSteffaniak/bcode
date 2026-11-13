//! Process-local transcript rows of the host shell.
//!
//! The host shell owns two kinds of presentation that are not canonical session history:
//! pending submissions (`PendingSubmissions`: staged, sending and queued messages the session
//! has not accepted) and process-local notices (`EphemeralNotices`: errors, slash-command
//! output, plugin activity). The engine receives them here as plain [`LocalRow`] data at the
//! end of every update, so they flow through the engine's normal input path and their
//! membership follows the shell state exactly (staged, sent, queued, accepted, cancelled, cleared, session switches).

use std::hash::{DefaultHasher, Hash, Hasher};
use std::time::Instant;

use bcode_session_view_models::{
    ChatMessageView, TextFormat, TranscriptViewItem, TranscriptViewItemId, TranscriptViewItemKind,
};
use bcode_tui_transcript::units::local::{LocalKey, LocalPlacement, LocalRow};

use super::TranscriptV2Host;
use crate::app::BmuxApp;
use crate::ephemeral_notices::EphemeralNotice;
use crate::pending_submission::{PendingSubmission, PendingSubmissionState};

/// Notices the transcript shows: every process-local notice of the shell.
fn shown_notices(app: &BmuxApp) -> impl Iterator<Item = &EphemeralNotice> + '_ {
    app.ephemeral_notices()
}

/// Pending submissions the transcript shows: nothing is drawn for a sent submission.
fn shown_pending(app: &BmuxApp) -> impl Iterator<Item = &PendingSubmission> + '_ {
    app.pending_submissions()
        .iter()
        .filter(|pending| !matches!(pending.state(), PendingSubmissionState::Sent))
}

/// Fingerprint of everything the local rows are built from, so an unchanged shell state costs
/// one pass without allocating rows.
fn fingerprint(app: &BmuxApp) -> u64 {
    let mut hasher = DefaultHasher::new();
    for notice in shown_notices(app) {
        notice.id().hash(&mut hasher);
        notice.after().hash(&mut hasher);
        notice.after_sequence().hash(&mut hasher);
        notice.role().hash(&mut hasher);
        notice.text().hash(&mut hasher);
        std::mem::discriminant(&notice.format()).hash(&mut hasher);
    }
    0xff_u8.hash(&mut hasher);
    for pending in shown_pending(app) {
        pending.id().0.hash(&mut hasher);
        pending.text().hash(&mut hasher);
        crate::pending_submission::pending_label(pending.state()).hash(&mut hasher);
    }
    hasher.finish()
}

/// A renderer-neutral item showing `message` as `kind`; [`LocalRow::new`] gives it its local
/// identity.
fn local_item(kind: TranscriptViewItemKind) -> TranscriptViewItem {
    TranscriptViewItem {
        id: TranscriptViewItemId::new("local"),
        revision: 1,
        sequence: None,
        timestamp_ms: None,
        output_location: None,
        streaming: false,
        kind,
    }
}

fn message(text: &str, format: TextFormat, display_label: Option<String>) -> ChatMessageView {
    ChatMessageView {
        submission_context: None,
        submission_status: None,
        activity: None,
        text: text.to_owned(),
        display_label,
        format,
    }
}

/// The local row of a pending submission: a user message titled `You · <state>` at the tail.
#[must_use]
pub fn pending_row(pending: &PendingSubmission) -> LocalRow {
    LocalRow::new(
        LocalKey::Submission(pending.id().0.clone()),
        LocalPlacement::Tail,
        local_item(TranscriptViewItemKind::UserMessage {
            // Pending text renders as Markdown.
            message: message(
                pending.text(),
                TextFormat::Markdown,
                Some(crate::pending_submission::pending_label(pending.state())),
            ),
        }),
    )
}

/// The notice rows of the shell state, in presentation order at the chronology boundary each
/// notice was raised at.
fn notice_rows(app: &BmuxApp) -> impl Iterator<Item = LocalRow> + '_ {
    shown_notices(app).map(|notice| {
        let role = notice.role();
        let key = if notice.source() == crate::ephemeral_notices::TURN_SUMMARY_SOURCE {
            LocalKey::Summary(notice.id().get())
        } else {
            LocalKey::Notice(notice.id().get())
        };
        LocalRow::new(
            key,
            LocalPlacement::After {
                item: notice.after().cloned(),
                sequence: notice.after_sequence(),
            },
            local_item(TranscriptViewItemKind::SystemMessage {
                message: message(
                    notice.text(),
                    notice.format(),
                    (role != "System").then(|| role.to_owned()),
                ),
            }),
        )
    })
}

/// A submission the shell accepted (and forgot) whose user message the engine has not
/// incorporated yet: it held the message during a window replacement, or the window is
/// detached and the tail is loading (LR4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandedOffSubmission {
    /// The pending row, as last shown.
    row: LocalRow,
    /// Canonical sequence of the accepting user message.
    sequence: u64,
}

impl TranscriptV2Host {
    /// Keep showing the pending row of a submission the user message at `sequence` accepts
    /// until the engine incorporates that message, so the submission never vanishes while
    /// the message waits for a window replacement (LR4). Called before the host shell applies
    /// the accepting message, which forgets the pending submission.
    pub fn hand_off_submission(&mut self, pending: &PendingSubmission, sequence: u64) {
        let row = pending_row(pending);
        if self
            .handoffs
            .iter()
            .all(|handoff| handoff.row.key != row.key)
        {
            self.handoffs.push(HandedOffSubmission { row, sequence });
        }
    }

    /// Bring the engine's local rows up to date with the shell state (LR1, LR4). Runs at the
    /// end of every update; an unchanged state is a no-op.
    ///
    /// Rows are the shell's notices, then submissions handed off at acceptance and not yet
    /// incorporated, then the shell's pending submissions in submission order.
    pub fn sync_local_rows(&mut self, app: &BmuxApp, at: Instant) {
        self.hand_off_recorded_submissions(app);
        let window = self.engine.window();
        let incorporated = |sequence: u64| {
            window.first_sequence.is_some_and(|first| first <= sequence)
                && window.last_sequence.is_some_and(|last| last >= sequence)
        };
        let pending = app.pending_submissions();
        self.handoffs.retain(|handoff| {
            !incorporated(handoff.sequence)
                && !pending
                    .iter()
                    .any(|shell| LocalKey::Submission(shell.id().0.clone()) == handoff.row.key)
        });
        let mut hasher = DefaultHasher::new();
        fingerprint(app).hash(&mut hasher);
        for handoff in &self.handoffs {
            handoff.row.key.hash(&mut hasher);
        }
        let fingerprint = hasher.finish();
        if self.local_rows == Some(fingerprint) {
            return;
        }
        self.local_rows = Some(fingerprint);
        let rows = notice_rows(app)
            .chain(self.handoffs.iter().map(|handoff| handoff.row.clone()))
            .chain(shown_pending(app).map(pending_row))
            .collect();
        let update = self.engine.set_local_rows(rows, at);
        self.absorb(update);
    }

    /// Hand off every submission shown last time that the shell has since forgotten because
    /// the session recorded it (LR4): its submission record, from the submit result or from
    /// attach hydration, or its acceptance event. The shell's session view then holds the
    /// submission's canonical item, whose sequence the engine must make resident before the
    /// row may leave. A submission forgotten for any other reason (cancellation, `/clear`, a
    /// session switch) has no such item and leaves at once.
    fn hand_off_recorded_submissions(&mut self, app: &BmuxApp) {
        let shown = shown_pending(app).map(pending_row).collect::<Vec<_>>();
        let forgotten = std::mem::replace(&mut self.shown_submissions, shown)
            .into_iter()
            .filter(|row| {
                self.shown_submissions
                    .iter()
                    .all(|shown| shown.key != row.key)
            });
        let mut handed_off = Vec::new();
        for row in forgotten {
            let LocalKey::Submission(id) = &row.key else {
                continue;
            };
            let canonical = TranscriptViewItemId::new(format!("submission:{id}"));
            if let Some(sequence) = app
                .session_view_snapshot()
                .transcript
                .items
                .iter()
                .find(|item| item.id == canonical)
                .and_then(|item| item.sequence)
            {
                handed_off.push(HandedOffSubmission { row, sequence });
            }
        }
        for handoff in handed_off {
            if self
                .handoffs
                .iter()
                .all(|existing| existing.row.key != handoff.row.key)
            {
                self.handoffs.push(handoff);
            }
        }
    }
}
