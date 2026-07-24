//! Cached transcript layout for virtualized TUI rendering.

use bmux_tui::prelude::Line;
use std::collections::BTreeSet;
use std::time::Instant;

use super::indexed_transcript_layout::IndexedTranscriptLayout;

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

    /// Return the fingerprint text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
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
    structural_fingerprint: Option<TranscriptLayoutFingerprint>,
    entries: IndexedTranscriptLayout,
    sync_stats: Vec<TranscriptLayoutSyncStats>,
}

/// A rendered line inside the transcript's global row space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisibleTranscriptLine {
    /// Global transcript row index from the oldest row.
    pub row_index: usize,
    pub(crate) entry_index: usize,
    pub(crate) row_in_entry: usize,
    pub(crate) source: VisibleTranscriptSource,
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

    /// Return whether the cache structure matches inputs excluding isolated visual revisions.
    #[must_use]
    pub fn structure_is_current(&self, fingerprint: &TranscriptLayoutFingerprint) -> bool {
        self.structural_fingerprint.as_ref() == Some(fingerprint)
    }

    /// Synchronize only transcript entries owned by dirty visual invocations.
    pub fn sync_visuals<S, R>(
        &mut self,
        fingerprint: TranscriptLayoutFingerprint,
        invocation_ids: &BTreeSet<String>,
        signature: S,
        rows: R,
    ) -> TranscriptLayoutSyncStats
    where
        S: Fn(usize) -> TranscriptLayoutSignature,
        R: FnMut(usize) -> Vec<Line>,
    {
        let started = Instant::now();
        let (entries_scanned, signatures_changed, rows_regenerated) =
            self.entries.sync_visuals(invocation_ids, signature, rows);
        self.fingerprint = Some(fingerprint);
        let stats = TranscriptLayoutSyncStats {
            invalidation: TranscriptLayoutInvalidation::Incremental,
            entries_scanned,
            signatures_changed,
            entries_rebuilt: signatures_changed,
            rows_regenerated,
            duration_micros: u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
        };
        self.sync_stats.push(stats);
        stats
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
    pub fn sync<TS, TR, TI, PS, PR, HS, HR, R>(
        &mut self,
        spec: TranscriptLayoutSpec<TS, TR, TI, PS, PR, HS, HR, R>,
    ) -> TranscriptLayoutSyncStats
    where
        TS: Fn(usize) -> TranscriptLayoutSignature,
        TR: Fn(usize) -> Vec<Line>,
        TI: Fn(usize) -> Option<String>,
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
            self.structural_fingerprint = None;
            self.entries.clear();
        }

        let history_signature = (spec.history_banner_signature)();
        let entries_scanned = spec
            .transcript_len
            .saturating_add(spec.pending_len)
            .saturating_add(usize::from(history_signature.is_some()));
        let (history_changed, history_rows) = self
            .entries
            .sync_history(history_signature, spec.history_banner_rows);
        let (transcript_changed, transcript_rows) = self.entries.sync_transcript(
            spec.transcript_len,
            spec.transcript_signature,
            spec.transcript_rows,
            spec.transcript_invocation_id,
        );
        let (pending_changed, pending_rows) =
            self.entries
                .sync_pending(spec.pending_len, spec.pending_signature, spec.pending_rows);
        let signatures_changed = history_changed
            .saturating_add(transcript_changed)
            .saturating_add(pending_changed);
        let rows_regenerated = history_rows
            .saturating_add(transcript_rows)
            .saturating_add(pending_rows);
        self.fingerprint = Some(spec.fingerprint);
        self.structural_fingerprint = Some(spec.structural_fingerprint);
        let stats = TranscriptLayoutSyncStats {
            invalidation,
            entries_scanned,
            signatures_changed,
            entries_rebuilt: signatures_changed,
            rows_regenerated,
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
        self.entries.visible_lines_from_top(start, viewport_height)
    }

    /// Return cached line for a visible transcript line.
    #[must_use]
    pub fn line(&self, visible: VisibleTranscriptLine) -> Option<&Line> {
        self.entries.line(visible)
    }

    /// Return visible cached row metadata for one global row index.
    #[cfg(test)]
    #[must_use]
    pub fn line_at_row(&self, row: usize) -> Option<VisibleTranscriptLine> {
        self.entries.line_at_row(row)
    }

    /// Return the first distinct cached transcript entry start at or after `row`.
    #[must_use]
    pub fn first_entry_start_at_or_after_row(&self, row: usize) -> Option<usize> {
        self.entries.first_entry_start_at_or_after_row(row)
    }

    /// Return the global start row for a cached transcript entry.
    #[must_use]
    pub fn entry_start_row(
        &self,
        source: VisibleTranscriptSource,
        entry_index: usize,
    ) -> Option<usize> {
        self.entries.entry_start_row(source, entry_index)
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

/// Specification used to synchronize transcript layout cache.
pub struct TranscriptLayoutSpec<TS, TR, TI, PS, PR, HS, HR, R> {
    /// Render width.
    pub width: u16,
    /// Fingerprint for all layout-affecting inputs.
    pub fingerprint: TranscriptLayoutFingerprint,
    /// Structural fingerprint excluding isolated adapter-owned visual revisions.
    pub structural_fingerprint: TranscriptLayoutFingerprint,
    /// Current committed transcript item count.
    pub transcript_len: usize,
    /// Current pending submission count.
    pub pending_len: usize,
    /// Return signature for a committed transcript item.
    pub transcript_signature: TS,
    /// Render rows for a committed transcript item.
    pub transcript_rows: TR,
    /// Return the generic invocation id owning a transcript item, when any.
    pub transcript_invocation_id: TI,
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
        impl Fn(usize) -> Option<String>,
        impl Fn(usize) -> TranscriptLayoutSignature,
        impl Fn(usize) -> Vec<Line>,
        impl FnOnce() -> Option<TranscriptLayoutSignature>,
        impl FnOnce() -> Vec<Line>,
        impl FnOnce() -> bool,
    > {
        TranscriptLayoutSpec {
            width: 80,
            fingerprint: TranscriptLayoutFingerprint::new(fingerprint.to_owned()),
            structural_fingerprint: TranscriptLayoutFingerprint::new(format!(
                "structural-{fingerprint}"
            )),
            transcript_len,
            pending_len: 0,
            transcript_signature: |index| TranscriptLayoutSignature::new(format!("item-{index}")),
            transcript_rows: |index| vec![Line::from(format!("row-{index}"))],
            transcript_invocation_id: |_| None,
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
    fn targeted_visual_sync_updates_row_index_without_scanning_siblings() {
        let mut cache = TranscriptLayoutCache::default();
        cache.sync(TranscriptLayoutSpec {
            width: 80,
            fingerprint: TranscriptLayoutFingerprint::new("initial".to_owned()),
            structural_fingerprint: TranscriptLayoutFingerprint::new("structure".to_owned()),
            transcript_len: 3,
            pending_len: 0,
            transcript_signature: |index| TranscriptLayoutSignature::new(format!("item-{index}")),
            transcript_rows: |_| vec![Line::from("row")],
            transcript_invocation_id: |index| Some(format!("call-{index}")),
            pending_signature: |index| TranscriptLayoutSignature::new(format!("pending-{index}")),
            pending_rows: |_| Vec::new(),
            history_banner_signature: || None,
            history_banner_rows: Vec::new,
            reset: || false,
        });
        let stats = cache.sync_visuals(
            TranscriptLayoutFingerprint::new("updated".to_owned()),
            &BTreeSet::from(["call-1".to_owned()]),
            |index| TranscriptLayoutSignature::new(format!("updated-{index}")),
            |_| vec![Line::from("one"), Line::from("two"), Line::from("three")],
        );

        assert_eq!(stats.entries_scanned, 1);
        assert_eq!(stats.entries_rebuilt, 1);
        assert_eq!(stats.rows_regenerated, 3);
        assert_eq!(cache.total_rows(), 5);
        assert_eq!(
            cache.entry_start_row(VisibleTranscriptSource::Transcript, 2),
            Some(4)
        );
        let visible = cache.visible_lines_from_top(3, 2);
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].entry_index, 1);
        assert_eq!(visible[0].row_in_entry, 2);
        assert_eq!(visible[1].entry_index, 2);
        assert_eq!(visible[1].row_in_entry, 0);
    }

    #[test]
    fn first_entry_navigation_crosses_indexed_sections() {
        let mut cache = TranscriptLayoutCache::default();
        cache.sync(TranscriptLayoutSpec {
            width: 80,
            fingerprint: TranscriptLayoutFingerprint::new("initial".to_owned()),
            structural_fingerprint: TranscriptLayoutFingerprint::new("structure".to_owned()),
            transcript_len: 1,
            pending_len: 1,
            transcript_signature: |_| TranscriptLayoutSignature::new("item".to_owned()),
            transcript_rows: |_| vec![Line::from("a"), Line::from("b")],
            transcript_invocation_id: |_| None,
            pending_signature: |_| TranscriptLayoutSignature::new("pending".to_owned()),
            pending_rows: |_| vec![Line::from("pending")],
            history_banner_signature: || Some(TranscriptLayoutSignature::new("history".to_owned())),
            history_banner_rows: || vec![Line::from("history")],
            reset: || false,
        });

        assert_eq!(cache.first_entry_start_at_or_after_row(0), Some(0));
        assert_eq!(cache.first_entry_start_at_or_after_row(2), Some(3));
        assert_eq!(cache.first_entry_start_at_or_after_row(3), Some(3));
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
