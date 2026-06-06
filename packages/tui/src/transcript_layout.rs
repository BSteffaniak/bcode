//! Cached transcript layout for virtualized TUI rendering.

use bmux_tui::prelude::Line;
use bmux_tui::retained_list::{RetainedListLayout, RetainedListLine};

/// Stable identity for a rendered transcript entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptLayoutSignature(String);

impl TranscriptLayoutSignature {
    /// Create a layout signature from owned text.
    #[must_use]
    pub const fn new(value: String) -> Self {
        Self(value)
    }
}

/// Fingerprint for inputs used to prepare transcript layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptLayoutFingerprint(String);

impl TranscriptLayoutFingerprint {
    /// Create a layout fingerprint from owned text.
    #[must_use]
    pub const fn new(value: String) -> Self {
        Self(value)
    }
}

/// Cached transcript layout rows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TranscriptLayoutCache {
    width: Option<u16>,
    fingerprint: Option<TranscriptLayoutFingerprint>,
    transcript_entries: RetainedListLayout<TranscriptLayoutSignature>,
    pending_entries: RetainedListLayout<TranscriptLayoutSignature>,
    history_banner: RetainedListLayout<TranscriptLayoutSignature>,
}

/// A rendered line inside the transcript's global row space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisibleTranscriptLine {
    /// Global transcript row index from the oldest row.
    pub row_index: usize,
    entry_index: usize,
    row_in_entry: usize,
    source: VisibleTranscriptSource,
}

/// Cached transcript entry source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibleTranscriptSource {
    /// Older-history status banner.
    HistoryBanner,
    /// Committed transcript item.
    Transcript,
    /// Pending submission item.
    Pending,
}

impl TranscriptLayoutCache {
    /// Return whether the cache already represents the given fingerprint.
    #[must_use]
    pub fn is_current(&self, fingerprint: &TranscriptLayoutFingerprint) -> bool {
        self.fingerprint.as_ref() == Some(fingerprint)
    }

    /// Synchronize the cache for a terminal width and current transcript data.
    pub fn sync<TS, TR, PS, PR, HS, HR, R>(
        &mut self,
        spec: TranscriptLayoutSpec<TS, TR, PS, PR, HS, HR, R>,
    ) where
        TS: Fn(usize) -> TranscriptLayoutSignature,
        TR: Fn(usize) -> Vec<Line>,
        PS: Fn(usize) -> TranscriptLayoutSignature,
        PR: Fn(usize) -> Vec<Line>,
        HS: FnOnce() -> Option<TranscriptLayoutSignature>,
        HR: FnOnce() -> Vec<Line>,
        R: FnOnce() -> bool,
    {
        if self.width != Some(spec.width) || (spec.reset)() {
            self.width = Some(spec.width);
            self.fingerprint = None;
            self.transcript_entries.clear();
            self.pending_entries.clear();
            self.history_banner.clear();
        }

        self.transcript_entries.sync(
            spec.transcript_len,
            spec.transcript_signature,
            spec.transcript_rows,
        );
        self.pending_entries
            .sync(spec.pending_len, spec.pending_signature, spec.pending_rows);
        match (spec.history_banner_signature)() {
            Some(signature) => {
                let rows = (spec.history_banner_rows)();
                self.history_banner
                    .sync(1, |_| signature.clone(), |_| rows.clone());
            }
            None => self.history_banner.clear(),
        }
        self.fingerprint = Some(spec.fingerprint);
    }

    /// Return total rendered row count for the cached transcript document.
    #[must_use]
    pub fn total_rows(&self) -> usize {
        self.history_banner
            .total_rows()
            .saturating_add(self.transcript_entries.total_rows())
            .saturating_add(self.pending_entries.total_rows())
    }

    /// Return visible cached rows for a top-origin start row and viewport height.
    #[must_use]
    pub fn visible_lines_from_top(
        &self,
        start: usize,
        viewport_height: u16,
    ) -> Vec<VisibleTranscriptLine> {
        let total_rows = self.total_rows();
        let end = start
            .saturating_add(usize::from(viewport_height))
            .min(total_rows);
        let mut visible = Vec::new();
        let mut row_cursor = 0usize;

        push_visible_from_layout(
            &mut visible,
            &mut row_cursor,
            start,
            end,
            VisibleTranscriptSource::HistoryBanner,
            &self.history_banner,
        );
        push_visible_from_layout(
            &mut visible,
            &mut row_cursor,
            start,
            end,
            VisibleTranscriptSource::Transcript,
            &self.transcript_entries,
        );
        push_visible_from_layout(
            &mut visible,
            &mut row_cursor,
            start,
            end,
            VisibleTranscriptSource::Pending,
            &self.pending_entries,
        );

        visible
    }

    /// Return cached line for a visible transcript line.
    #[must_use]
    pub fn line(&self, visible: VisibleTranscriptLine) -> Option<&Line> {
        match visible.source {
            VisibleTranscriptSource::HistoryBanner => self.history_banner.line(retained(visible)),
            VisibleTranscriptSource::Transcript => self.transcript_entries.line(retained(visible)),
            VisibleTranscriptSource::Pending => self.pending_entries.line(retained(visible)),
        }
    }

    /// Return the global start row for a cached transcript entry.
    #[must_use]
    pub fn entry_start_row(
        &self,
        source: VisibleTranscriptSource,
        entry_index: usize,
    ) -> Option<usize> {
        let mut row_cursor = 0usize;
        if source == VisibleTranscriptSource::HistoryBanner && entry_index == 0 {
            return Some(row_cursor);
        }
        row_cursor = row_cursor.saturating_add(self.history_banner.total_rows());
        if source == VisibleTranscriptSource::Transcript {
            return self
                .transcript_entries
                .entry_start_row(entry_index)
                .map(|start| start.saturating_add(row_cursor));
        }
        row_cursor = row_cursor.saturating_add(self.transcript_entries.total_rows());
        if source == VisibleTranscriptSource::Pending {
            return self
                .pending_entries
                .entry_start_row(entry_index)
                .map(|start| start.saturating_add(row_cursor));
        }
        None
    }
}

/// Specification used to synchronize transcript layout cache.
pub struct TranscriptLayoutSpec<TS, TR, PS, PR, HS, HR, R> {
    /// Render width.
    pub width: u16,
    /// Fingerprint for all layout-affecting inputs.
    pub fingerprint: TranscriptLayoutFingerprint,
    /// Current committed transcript item count.
    pub transcript_len: usize,
    /// Current pending submission count.
    pub pending_len: usize,
    /// Return signature for a committed transcript item.
    pub transcript_signature: TS,
    /// Render rows for a committed transcript item.
    pub transcript_rows: TR,
    /// Return signature for a pending submission.
    pub pending_signature: PS,
    /// Render rows for a pending submission.
    pub pending_rows: PR,
    /// Return optional older-history banner signature.
    pub history_banner_signature: HS,
    /// Render older-history banner rows.
    pub history_banner_rows: HR,
    /// Return whether all cached rows must be discarded.
    pub reset: R,
}

fn push_visible_from_layout(
    visible: &mut Vec<VisibleTranscriptLine>,
    row_cursor: &mut usize,
    start: usize,
    end: usize,
    source: VisibleTranscriptSource,
    layout: &RetainedListLayout<TranscriptLayoutSignature>,
) {
    let section_start = *row_cursor;
    let section_rows = layout.total_rows();
    let section_end = section_start.saturating_add(section_rows);
    if section_end > start && section_start < end {
        let local_start = start.saturating_sub(section_start);
        let local_height = end.saturating_sub(section_start).min(section_rows);
        visible.extend(
            layout
                .visible_lines_from_top(local_start, saturating_u16(local_height))
                .into_iter()
                .map(|line| VisibleTranscriptLine {
                    row_index: section_start.saturating_add(line.row_index),
                    entry_index: line.entry_index,
                    row_in_entry: line.row_in_entry,
                    source,
                }),
        );
    }
    *row_cursor = section_end;
}

const fn retained(visible: VisibleTranscriptLine) -> RetainedListLine {
    RetainedListLine {
        row_index: visible.row_index,
        entry_index: visible.entry_index,
        row_in_entry: visible.row_in_entry,
    }
}

fn saturating_u16(value: usize) -> u16 {
    u16::try_from(value.min(usize::from(u16::MAX))).unwrap_or(u16::MAX)
}
