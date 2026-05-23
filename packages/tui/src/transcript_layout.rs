//! Cached transcript layout for virtualized TUI rendering.

use bmux_tui::prelude::Line;

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

/// Cached transcript layout rows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TranscriptLayoutCache {
    width: Option<u16>,
    transcript_entries: Vec<CachedTranscriptEntry>,
    pending_entries: Vec<CachedTranscriptEntry>,
    history_banner: Option<CachedTranscriptEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedTranscriptEntry {
    signature: TranscriptLayoutSignature,
    rows: Vec<Line>,
}

impl CachedTranscriptEntry {
    const fn new(signature: TranscriptLayoutSignature, rows: Vec<Line>) -> Self {
        Self { signature, rows }
    }

    const fn row_count(&self) -> usize {
        self.rows.len()
    }
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
            self.transcript_entries.clear();
            self.pending_entries.clear();
            self.history_banner = None;
        }

        sync_entries(
            &mut self.transcript_entries,
            spec.transcript_len,
            spec.transcript_signature,
            spec.transcript_rows,
        );
        sync_entries(
            &mut self.pending_entries,
            spec.pending_len,
            spec.pending_signature,
            spec.pending_rows,
        );
        self.history_banner = match (spec.history_banner_signature)() {
            Some(signature) => match self.history_banner.take() {
                Some(entry) if entry.signature == signature => Some(entry),
                _ => Some(CachedTranscriptEntry::new(
                    signature,
                    (spec.history_banner_rows)(),
                )),
            },
            None => None,
        };
    }

    /// Return total rendered row count for the cached transcript document.
    #[must_use]
    pub fn total_rows(&self) -> usize {
        self.history_banner
            .iter()
            .map(CachedTranscriptEntry::row_count)
            .sum::<usize>()
            .saturating_add(
                self.transcript_entries
                    .iter()
                    .map(CachedTranscriptEntry::row_count)
                    .sum::<usize>(),
            )
            .saturating_add(
                self.pending_entries
                    .iter()
                    .map(CachedTranscriptEntry::row_count)
                    .sum::<usize>(),
            )
    }

    /// Return visible cached rows for a bottom-origin scroll offset and viewport height.
    #[must_use]
    pub fn visible_lines(
        &self,
        scroll_offset: usize,
        viewport_height: u16,
    ) -> Vec<VisibleTranscriptLine> {
        let total_rows = self.total_rows();
        let end = total_rows.saturating_sub(scroll_offset).min(total_rows);
        let start = end.saturating_sub(usize::from(viewport_height));
        let mut visible = Vec::new();
        let mut row_cursor = 0usize;

        if let Some(entry) = &self.history_banner {
            push_visible_for_entry(
                &mut visible,
                start,
                end,
                &mut row_cursor,
                VisibleTranscriptSource::HistoryBanner,
                0,
                entry.row_count(),
            );
        }
        for (index, entry) in self.transcript_entries.iter().enumerate() {
            push_visible_for_entry(
                &mut visible,
                start,
                end,
                &mut row_cursor,
                VisibleTranscriptSource::Transcript,
                index,
                entry.row_count(),
            );
        }
        for (index, entry) in self.pending_entries.iter().enumerate() {
            push_visible_for_entry(
                &mut visible,
                start,
                end,
                &mut row_cursor,
                VisibleTranscriptSource::Pending,
                index,
                entry.row_count(),
            );
        }

        visible
    }

    /// Return a cached line for a visible row descriptor.
    #[must_use]
    pub fn line(&self, visible: VisibleTranscriptLine) -> Option<&Line> {
        let entry = match visible.source {
            VisibleTranscriptSource::HistoryBanner => self.history_banner.as_ref(),
            VisibleTranscriptSource::Transcript => self.transcript_entries.get(visible.entry_index),
            VisibleTranscriptSource::Pending => self.pending_entries.get(visible.entry_index),
        }?;
        entry.rows.get(visible.row_in_entry)
    }
}

/// Transcript layout synchronization input.
pub struct TranscriptLayoutSpec<TS, TR, PS, PR, HS, HR, R> {
    /// Render width.
    pub width: u16,
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

fn sync_entries<S, R>(
    entries: &mut Vec<CachedTranscriptEntry>,
    len: usize,
    signature_for: S,
    rows_for: R,
) where
    S: Fn(usize) -> TranscriptLayoutSignature,
    R: Fn(usize) -> Vec<Line>,
{
    entries.truncate(len);
    for index in 0..len {
        let signature = signature_for(index);
        if entries
            .get(index)
            .is_some_and(|entry| entry.signature == signature)
        {
            continue;
        }
        let entry = CachedTranscriptEntry::new(signature, rows_for(index));
        if let Some(slot) = entries.get_mut(index) {
            *slot = entry;
        } else {
            entries.push(entry);
        }
    }
}

fn push_visible_for_entry(
    visible: &mut Vec<VisibleTranscriptLine>,
    start: usize,
    end: usize,
    row_cursor: &mut usize,
    source: VisibleTranscriptSource,
    entry_index: usize,
    row_count: usize,
) {
    let entry_start = *row_cursor;
    let entry_end = entry_start.saturating_add(row_count);
    if entry_end > start && entry_start < end {
        let first = start.saturating_sub(entry_start);
        let last = end.saturating_sub(entry_start).min(row_count);
        visible.extend((first..last).map(|row_in_entry| VisibleTranscriptLine {
            row_index: entry_start.saturating_add(row_in_entry),
            entry_index,
            row_in_entry,
            source,
        }));
    }
    *row_cursor = entry_end;
}
