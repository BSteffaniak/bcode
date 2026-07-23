//! Cached transcript layout for virtualized TUI rendering.

use bmux_tui::prelude::Line;
use bmux_tui::retained_sectioned_list::{RetainedSectionedListLayout, RetainedSectionedListLine};
use std::cell::Cell;
use std::time::Instant;

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

/// Reason one transcript layout synchronization performed work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptLayoutInvalidation {
    /// The fingerprint already matched, so no entries were scanned or rebuilt.
    CacheHit,
    /// Layout inputs changed without requiring a complete cache reset.
    Incremental,
    /// Terminal width changed and required a complete cache reset.
    Width,
    /// The caller explicitly requested a complete cache reset.
    Explicit,
}

impl TranscriptLayoutInvalidation {
    /// Return the stable low-cardinality metric label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::CacheHit => "cache_hit",
            Self::Incremental => "incremental",
            Self::Width => "width",
            Self::Explicit => "explicit",
        }
    }
}

/// Work performed by one transcript layout synchronization attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptLayoutSyncStats {
    /// Invalidation category for this attempt.
    pub invalidation: TranscriptLayoutInvalidation,
    /// Entry signatures inspected by the retained layout.
    pub entries_scanned: usize,
    /// Entry signatures whose rows were regenerated.
    pub signatures_changed: usize,
    /// Entries rebuilt after a signature change.
    pub entries_rebuilt: usize,
    /// Total rows generated for rebuilt entries.
    pub rows_regenerated: usize,
    /// Complete synchronization duration in microseconds.
    pub duration_micros: u64,
}

impl TranscriptLayoutSyncStats {
    /// Construct a cache-hit observation.
    #[must_use]
    pub const fn cache_hit(duration_micros: u64) -> Self {
        Self {
            invalidation: TranscriptLayoutInvalidation::CacheHit,
            entries_scanned: 0,
            signatures_changed: 0,
            entries_rebuilt: 0,
            rows_regenerated: 0,
            duration_micros,
        }
    }
}

/// Cached transcript layout rows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TranscriptLayoutCache {
    width: Option<u16>,
    fingerprint: Option<TranscriptLayoutFingerprint>,
    entries: RetainedSectionedListLayout<VisibleTranscriptSource, TranscriptLayoutSignature>,
    sync_stats: Vec<TranscriptLayoutSyncStats>,
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

    /// Record one cache-hit synchronization attempt.
    pub fn record_cache_hit(&mut self, duration_micros: u64) {
        self.sync_stats
            .push(TranscriptLayoutSyncStats::cache_hit(duration_micros));
    }

    /// Drain synchronization work observations accumulated since the previous call.
    pub fn drain_sync_stats(&mut self) -> Vec<TranscriptLayoutSyncStats> {
        std::mem::take(&mut self.sync_stats)
    }

    /// Synchronize the cache for a terminal width and current transcript data.
    pub fn sync<TS, TR, PS, PR, HS, HR, R>(
        &mut self,
        spec: TranscriptLayoutSpec<TS, TR, PS, PR, HS, HR, R>,
    ) -> TranscriptLayoutSyncStats
    where
        TS: Fn(usize) -> TranscriptLayoutSignature,
        TR: Fn(usize) -> Vec<Line>,
        PS: Fn(usize) -> TranscriptLayoutSignature,
        PR: Fn(usize) -> Vec<Line>,
        HS: FnOnce() -> Option<TranscriptLayoutSignature>,
        HR: FnOnce() -> Vec<Line>,
        R: FnOnce() -> bool,
    {
        let started = Instant::now();
        let width_changed = self.width != Some(spec.width);
        let explicit_reset = (spec.reset)();
        let invalidation = if width_changed {
            TranscriptLayoutInvalidation::Width
        } else if explicit_reset {
            TranscriptLayoutInvalidation::Explicit
        } else {
            TranscriptLayoutInvalidation::Incremental
        };
        if width_changed || explicit_reset {
            self.width = Some(spec.width);
            self.fingerprint = None;
            self.entries.clear();
        }

        let history_signature = (spec.history_banner_signature)();
        let entries_scanned = spec
            .transcript_len
            .saturating_add(spec.pending_len)
            .saturating_add(usize::from(history_signature.is_some()));
        let signatures_changed = Cell::new(0_usize);
        let rows_regenerated = Cell::new(0_usize);

        self.entries.sync_sections([
            VisibleTranscriptSource::HistoryBanner,
            VisibleTranscriptSource::Transcript,
            VisibleTranscriptSource::Pending,
        ]);
        self.entries.sync_section(
            &VisibleTranscriptSource::Transcript,
            spec.transcript_len,
            spec.transcript_signature,
            |index| {
                let rows = (spec.transcript_rows)(index);
                signatures_changed.set(signatures_changed.get().saturating_add(1));
                rows_regenerated.set(rows_regenerated.get().saturating_add(rows.len()));
                rows
            },
        );
        self.entries.sync_section(
            &VisibleTranscriptSource::Pending,
            spec.pending_len,
            spec.pending_signature,
            |index| {
                let rows = (spec.pending_rows)(index);
                signatures_changed.set(signatures_changed.get().saturating_add(1));
                rows_regenerated.set(rows_regenerated.get().saturating_add(rows.len()));
                rows
            },
        );
        match history_signature {
            Some(signature) => {
                let rows = (spec.history_banner_rows)();
                self.entries.sync_section(
                    &VisibleTranscriptSource::HistoryBanner,
                    1,
                    |_| signature.clone(),
                    |_| {
                        signatures_changed.set(signatures_changed.get().saturating_add(1));
                        rows_regenerated.set(rows_regenerated.get().saturating_add(rows.len()));
                        rows.clone()
                    },
                );
            }
            None => self
                .entries
                .clear_section(&VisibleTranscriptSource::HistoryBanner),
        }
        self.fingerprint = Some(spec.fingerprint);
        let stats = TranscriptLayoutSyncStats {
            invalidation,
            entries_scanned,
            signatures_changed: signatures_changed.get(),
            entries_rebuilt: signatures_changed.get(),
            rows_regenerated: rows_regenerated.get(),
            duration_micros: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
        };
        self.sync_stats.push(stats);
        stats
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

    /// Return a stable pointer to the first cached row of a transcript entry for cache-reuse tests.
    #[cfg(test)]
    #[must_use]
    pub fn transcript_entry_row_ptr(&self, entry_index: usize) -> Option<*const Line> {
        let start = self.entry_start_row(VisibleTranscriptSource::Transcript, entry_index)?;
        let line = self.line_at_row(start)?;
        self.line(line).map(std::ptr::from_ref)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::type_complexity)]
    fn spec(
        fingerprint: &str,
        transcript_len: usize,
    ) -> TranscriptLayoutSpec<
        impl Fn(usize) -> TranscriptLayoutSignature,
        impl Fn(usize) -> Vec<Line>,
        impl Fn(usize) -> TranscriptLayoutSignature,
        impl Fn(usize) -> Vec<Line>,
        impl FnOnce() -> Option<TranscriptLayoutSignature>,
        impl FnOnce() -> Vec<Line>,
        impl FnOnce() -> bool,
    > {
        TranscriptLayoutSpec {
            width: 80,
            fingerprint: TranscriptLayoutFingerprint::new(fingerprint.to_owned()),
            transcript_len,
            pending_len: 0,
            transcript_signature: |index| TranscriptLayoutSignature::new(format!("item-{index}")),
            transcript_rows: |index| vec![Line::from(format!("row-{index}"))],
            pending_signature: |index| TranscriptLayoutSignature::new(format!("pending-{index}")),
            pending_rows: |_| Vec::new(),
            history_banner_signature: || None,
            history_banner_rows: Vec::new,
            reset: || false,
        }
    }

    #[test]
    fn sync_stats_report_scans_and_rebuilds() {
        let mut cache = TranscriptLayoutCache::default();

        let initial = cache.sync(spec("one", 10));
        assert_eq!(initial.invalidation, TranscriptLayoutInvalidation::Width);
        assert_eq!(initial.entries_scanned, 10);
        assert_eq!(initial.signatures_changed, 10);
        assert_eq!(initial.entries_rebuilt, 10);
        assert_eq!(initial.rows_regenerated, 10);

        let unchanged = cache.sync(spec("two", 10));
        assert_eq!(
            unchanged.invalidation,
            TranscriptLayoutInvalidation::Incremental
        );
        assert_eq!(unchanged.entries_scanned, 10);
        assert_eq!(unchanged.signatures_changed, 0);
        assert_eq!(unchanged.entries_rebuilt, 0);
        assert_eq!(unchanged.rows_regenerated, 0);
    }

    #[test]
    fn cache_hits_are_recorded_without_scanning() {
        let mut cache = TranscriptLayoutCache::default();
        cache.record_cache_hit(7);

        assert_eq!(
            cache.drain_sync_stats(),
            vec![TranscriptLayoutSyncStats::cache_hit(7)]
        );
        assert!(cache.drain_sync_stats().is_empty());
    }
}
