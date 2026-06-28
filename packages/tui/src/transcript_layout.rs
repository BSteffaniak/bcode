//! Cached transcript layout for virtualized TUI rendering.

use bmux_tui::prelude::Line;
use bmux_tui::retained_sectioned_list::{RetainedSectionedListLayout, RetainedSectionedListLine};

/// Stable identity for a rendered transcript entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptLayoutSignature(String);

impl TranscriptLayoutSignature {
    /// Create a layout signature from owned text.
    #[must_use]
    pub const fn new(value: String) -> Self {
        Self(value)
    }

    /// Return the signature text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
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
    entries: RetainedSectionedListLayout<VisibleTranscriptSource, TranscriptLayoutSignature>,
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

impl VisibleTranscriptLine {
    /// Return the cached entry index for this row.
    #[must_use]
    pub const fn entry_index(self) -> usize {
        self.entry_index
    }

    /// Return the cached row index within this row's entry.
    #[must_use]
    pub const fn row_in_entry(self) -> usize {
        self.row_in_entry
    }

    /// Return the cached entry source for this row.
    #[must_use]
    pub const fn source(self) -> VisibleTranscriptSource {
        self.source
    }
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
            self.entries.clear();
        }

        self.entries.sync_sections([
            VisibleTranscriptSource::HistoryBanner,
            VisibleTranscriptSource::Transcript,
            VisibleTranscriptSource::Pending,
        ]);
        self.entries.sync_section(
            &VisibleTranscriptSource::Transcript,
            spec.transcript_len,
            spec.transcript_signature,
            spec.transcript_rows,
        );
        self.entries.sync_section(
            &VisibleTranscriptSource::Pending,
            spec.pending_len,
            spec.pending_signature,
            spec.pending_rows,
        );
        match (spec.history_banner_signature)() {
            Some(signature) => {
                let rows = (spec.history_banner_rows)();
                self.entries.sync_section(
                    &VisibleTranscriptSource::HistoryBanner,
                    1,
                    |_| signature.clone(),
                    |_| rows.clone(),
                );
            }
            None => self
                .entries
                .clear_section(&VisibleTranscriptSource::HistoryBanner),
        }
        self.fingerprint = Some(spec.fingerprint);
    }

    /// Return total rendered row count for the cached transcript document.
    #[must_use]
    pub fn total_rows(&self) -> usize {
        self.entries.total_rows()
    }

    /// Return visible cached rows for a top-origin start row and viewport height.
    #[must_use]
    pub fn visible_lines_from_top(
        &self,
        start: usize,
        viewport_height: u16,
    ) -> Vec<VisibleTranscriptLine> {
        self.entries
            .visible_lines_from_top(start, viewport_height)
            .into_iter()
            .map(VisibleTranscriptLine::from)
            .collect()
    }

    /// Return cached line for a visible transcript line.
    #[must_use]
    pub fn line(&self, visible: VisibleTranscriptLine) -> Option<&Line> {
        self.entries.line(&RetainedSectionedListLine::from(visible))
    }

    /// Return visible cached row metadata for one global row index.
    #[must_use]
    pub fn line_at_row(&self, row: usize) -> Option<VisibleTranscriptLine> {
        self.visible_lines_from_top(row, 1).into_iter().next()
    }

    /// Return the first distinct cached transcript entry start at or after `row`.
    #[must_use]
    pub fn first_entry_start_at_or_after_row(&self, row: usize) -> Option<usize> {
        (row..self.total_rows()).find(|candidate| self.entry_starts_at_row(*candidate))
    }

    /// Return whether a distinct cached transcript entry starts at `row`.
    #[must_use]
    pub fn entry_starts_at_row(&self, row: usize) -> bool {
        self.line_at_row(row)
            .is_some_and(|line| line.row_in_entry == 0)
    }

    /// Return the global start row for a cached transcript entry.
    #[must_use]
    pub fn entry_start_row(
        &self,
        source: VisibleTranscriptSource,
        entry_index: usize,
    ) -> Option<usize> {
        self.entries.entry_start_row(&source, entry_index)
    }
}

impl From<RetainedSectionedListLine<VisibleTranscriptSource>> for VisibleTranscriptLine {
    fn from(line: RetainedSectionedListLine<VisibleTranscriptSource>) -> Self {
        Self {
            row_index: line.row_index,
            entry_index: line.entry_index,
            row_in_entry: line.row_in_entry,
            source: line.section,
        }
    }
}

impl From<VisibleTranscriptLine> for RetainedSectionedListLine<VisibleTranscriptSource> {
    fn from(line: VisibleTranscriptLine) -> Self {
        Self {
            row_index: line.row_index,
            section: line.source,
            entry_index: line.entry_index,
            row_in_entry: line.row_in_entry,
        }
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
