//! Indexed retained rows for transcript layout projection.

use std::collections::{BTreeMap, BTreeSet};

use bmux_tui::prelude::Line;

use super::transcript_layout::{
    TranscriptLayoutSignature, VisibleTranscriptLine, VisibleTranscriptSource,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexedEntry {
    signature: TranscriptLayoutSignature,
    rows: Vec<Line>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct FenwickRows {
    tree: Vec<usize>,
}

impl FenwickRows {
    fn rebuild(&mut self, entries: &[IndexedEntry]) {
        self.tree = vec![0; entries.len().saturating_add(1)];
        for (index, entry) in entries.iter().enumerate() {
            self.add(index, entry.rows.len());
        }
    }

    fn replace(&mut self, index: usize, old: usize, new: usize) {
        if new >= old {
            self.add(index, new - old);
        } else {
            self.subtract(index, old - new);
        }
    }

    fn add(&mut self, index: usize, value: usize) {
        let mut cursor = index.saturating_add(1);
        while cursor < self.tree.len() {
            self.tree[cursor] = self.tree[cursor].saturating_add(value);
            cursor = cursor.saturating_add(lowbit(cursor));
        }
    }

    fn subtract(&mut self, index: usize, value: usize) {
        let mut cursor = index.saturating_add(1);
        while cursor < self.tree.len() {
            self.tree[cursor] = self.tree[cursor].saturating_sub(value);
            cursor = cursor.saturating_add(lowbit(cursor));
        }
    }

    fn prefix(&self, end: usize) -> usize {
        let mut cursor = end.min(self.tree.len().saturating_sub(1));
        let mut total = 0_usize;
        while cursor > 0 {
            total = total.saturating_add(self.tree[cursor]);
            cursor -= lowbit(cursor);
        }
        total
    }

    fn total(&self) -> usize {
        self.prefix(self.tree.len().saturating_sub(1))
    }

    fn entry_at_row(&self, row: usize) -> Option<(usize, usize)> {
        if row >= self.total() {
            return None;
        }
        let len = self.tree.len().saturating_sub(1);
        let mut index = 0_usize;
        let mut prefix = 0_usize;
        let mut step = highest_power_of_two(len);
        while step > 0 {
            let next = index.saturating_add(step);
            if next <= len && prefix.saturating_add(self.tree[next]) <= row {
                index = next;
                prefix = prefix.saturating_add(self.tree[next]);
            }
            step /= 2;
        }
        Some((index, prefix))
    }
}

const fn lowbit(value: usize) -> usize {
    value & value.wrapping_neg()
}

const fn highest_power_of_two(value: usize) -> usize {
    if value == 0 {
        0
    } else {
        1_usize << value.ilog2()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct IndexedSection {
    entries: Vec<IndexedEntry>,
    rows: FenwickRows,
}

impl IndexedSection {
    fn clear(&mut self) {
        self.entries.clear();
        self.rows.tree.clear();
    }

    fn sync<S, R>(&mut self, len: usize, signature: S, mut render_rows: R) -> (usize, usize)
    where
        S: Fn(usize) -> TranscriptLayoutSignature,
        R: FnMut(usize) -> Vec<Line>,
    {
        self.entries.truncate(len);
        let mut changed = 0_usize;
        let mut rows_regenerated = 0_usize;
        for index in 0..len {
            let signature = signature(index);
            match self.entries.get_mut(index) {
                Some(entry) if entry.signature == signature => {}
                Some(entry) => {
                    let rows = render_rows(index);
                    changed = changed.saturating_add(1);
                    rows_regenerated = rows_regenerated.saturating_add(rows.len());
                    *entry = IndexedEntry { signature, rows };
                }
                None => {
                    let rows = render_rows(index);
                    changed = changed.saturating_add(1);
                    rows_regenerated = rows_regenerated.saturating_add(rows.len());
                    self.entries.push(IndexedEntry { signature, rows });
                }
            }
        }
        self.rows.rebuild(&self.entries);
        (changed, rows_regenerated)
    }

    fn sync_entries<S, R>(
        &mut self,
        indexes: &BTreeSet<usize>,
        signature: S,
        mut render_rows: R,
    ) -> (usize, usize)
    where
        S: Fn(usize) -> TranscriptLayoutSignature,
        R: FnMut(usize) -> Vec<Line>,
    {
        let mut changed = 0_usize;
        let mut rows_regenerated = 0_usize;
        for index in indexes.iter().copied() {
            let Some(entry) = self.entries.get_mut(index) else {
                continue;
            };
            let signature = signature(index);
            if entry.signature == signature {
                continue;
            }
            let rows = render_rows(index);
            let old_rows = entry.rows.len();
            let new_rows = rows.len();
            *entry = IndexedEntry { signature, rows };
            self.rows.replace(index, old_rows, new_rows);
            changed = changed.saturating_add(1);
            rows_regenerated = rows_regenerated.saturating_add(new_rows);
        }
        (changed, rows_regenerated)
    }

    fn total_rows(&self) -> usize {
        self.rows.total()
    }

    fn entry_start_row(&self, entry_index: usize) -> Option<usize> {
        (entry_index < self.entries.len()).then(|| self.rows.prefix(entry_index))
    }

    fn line(&self, entry_index: usize, row_in_entry: usize) -> Option<&Line> {
        self.entries.get(entry_index)?.rows.get(row_in_entry)
    }

    fn line_at_row(&self, row: usize) -> Option<(usize, usize)> {
        let (entry_index, entry_start) = self.rows.entry_at_row(row)?;
        Some((entry_index, row.saturating_sub(entry_start)))
    }

    fn visible_lines(
        &self,
        source: VisibleTranscriptSource,
        global_start: usize,
        start: usize,
        end: usize,
        output: &mut Vec<VisibleTranscriptLine>,
    ) {
        if start >= end || self.entries.is_empty() {
            return;
        }
        let Some((mut entry_index, entry_start)) = self.rows.entry_at_row(start) else {
            return;
        };
        let mut row_cursor = entry_start;
        while entry_index < self.entries.len() && row_cursor < end {
            let entry = &self.entries[entry_index];
            let entry_end = row_cursor.saturating_add(entry.rows.len());
            let row_start = start.saturating_sub(row_cursor).min(entry.rows.len());
            let row_end = end.saturating_sub(row_cursor).min(entry.rows.len());
            output.extend((row_start..row_end).map(|row_in_entry| {
                VisibleTranscriptLine {
                    row_index: global_start
                        .saturating_add(row_cursor)
                        .saturating_add(row_in_entry),
                    entry_index,
                    row_in_entry,
                    source,
                }
            }));
            row_cursor = entry_end;
            entry_index = entry_index.saturating_add(1);
        }
    }
}

/// Transcript-specific retained rows with indexed row offsets and invocation ownership.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexedTranscriptLayout {
    history: IndexedSection,
    transcript: IndexedSection,
    pending: IndexedSection,
    invocation_entries: BTreeMap<String, BTreeSet<usize>>,
}

impl IndexedTranscriptLayout {
    pub fn clear(&mut self) {
        self.history.clear();
        self.transcript.clear();
        self.pending.clear();
        self.invocation_entries.clear();
    }

    pub fn sync_history<S, R>(&mut self, signature: Option<S>, rows: R) -> (usize, usize)
    where
        S: Into<TranscriptLayoutSignature>,
        R: FnOnce() -> Vec<Line>,
    {
        let signature = signature.map(Into::into);
        let rendered_rows = signature.as_ref().map(|_| rows()).unwrap_or_default();
        self.history.sync(
            usize::from(signature.is_some()),
            |_| signature.clone().expect("history signature"),
            |_| rendered_rows.clone(),
        )
    }

    pub fn sync_transcript<S, R, I>(
        &mut self,
        len: usize,
        signature: S,
        rows: R,
        invocation_id: I,
    ) -> (usize, usize)
    where
        S: Fn(usize) -> TranscriptLayoutSignature,
        R: FnMut(usize) -> Vec<Line>,
        I: Fn(usize) -> Option<String>,
    {
        let result = self.transcript.sync(len, signature, rows);
        self.invocation_entries.clear();
        for index in 0..len {
            if let Some(invocation_id) = invocation_id(index) {
                self.invocation_entries
                    .entry(invocation_id)
                    .or_default()
                    .insert(index);
            }
        }
        result
    }

    pub fn sync_pending<S, R>(&mut self, len: usize, signature: S, rows: R) -> (usize, usize)
    where
        S: Fn(usize) -> TranscriptLayoutSignature,
        R: FnMut(usize) -> Vec<Line>,
    {
        self.pending.sync(len, signature, rows)
    }

    pub fn sync_visuals<S, R>(
        &mut self,
        invocation_ids: &BTreeSet<String>,
        signature: S,
        rows: R,
    ) -> (usize, usize, usize)
    where
        S: Fn(usize) -> TranscriptLayoutSignature,
        R: FnMut(usize) -> Vec<Line>,
    {
        let indexes = invocation_ids
            .iter()
            .filter_map(|invocation_id| self.invocation_entries.get(invocation_id))
            .flatten()
            .copied()
            .collect::<BTreeSet<_>>();
        let scanned = indexes.len();
        let (changed, rows_regenerated) = self.transcript.sync_entries(&indexes, signature, rows);
        (scanned, changed, rows_regenerated)
    }

    pub fn total_rows(&self) -> usize {
        self.history
            .total_rows()
            .saturating_add(self.transcript.total_rows())
            .saturating_add(self.pending.total_rows())
    }

    pub fn visible_lines_from_top(
        &self,
        start: usize,
        viewport_height: u16,
    ) -> Vec<VisibleTranscriptLine> {
        let end = start
            .saturating_add(usize::from(viewport_height))
            .min(self.total_rows());
        let mut output = Vec::new();
        let mut global_start = 0_usize;
        for (source, section) in [
            (VisibleTranscriptSource::HistoryBanner, &self.history),
            (VisibleTranscriptSource::Transcript, &self.transcript),
            (VisibleTranscriptSource::Pending, &self.pending),
        ] {
            let section_end = global_start.saturating_add(section.total_rows());
            if section_end > start && global_start < end {
                let local_start = start.saturating_sub(global_start);
                let local_end = end.saturating_sub(global_start).min(section.total_rows());
                section.visible_lines(source, global_start, local_start, local_end, &mut output);
            }
            global_start = section_end;
        }
        output
    }

    pub fn line(&self, visible: VisibleTranscriptLine) -> Option<&Line> {
        self.section(visible.source)
            .line(visible.entry_index, visible.row_in_entry)
    }

    pub fn line_at_row(&self, row: usize) -> Option<VisibleTranscriptLine> {
        let mut global_start = 0_usize;
        for (source, section) in [
            (VisibleTranscriptSource::HistoryBanner, &self.history),
            (VisibleTranscriptSource::Transcript, &self.transcript),
            (VisibleTranscriptSource::Pending, &self.pending),
        ] {
            let section_end = global_start.saturating_add(section.total_rows());
            if row < section_end {
                let (entry_index, row_in_entry) =
                    section.line_at_row(row.saturating_sub(global_start))?;
                return Some(VisibleTranscriptLine {
                    row_index: row,
                    entry_index,
                    row_in_entry,
                    source,
                });
            }
            global_start = section_end;
        }
        None
    }

    pub fn first_entry_start_at_or_after_row(&self, row: usize) -> Option<usize> {
        let line = self.line_at_row(row)?;
        if line.row_in_entry == 0 {
            return Some(row);
        }
        let section = self.section(line.source);
        if let Some(next) = section.entry_start_row(line.entry_index.saturating_add(1)) {
            let section_start = match line.source {
                VisibleTranscriptSource::HistoryBanner => 0,
                VisibleTranscriptSource::Transcript => self.history.total_rows(),
                VisibleTranscriptSource::Pending => self
                    .history
                    .total_rows()
                    .saturating_add(self.transcript.total_rows()),
            };
            return Some(section_start.saturating_add(next));
        }
        match line.source {
            VisibleTranscriptSource::HistoryBanner => self
                .entry_start_row(VisibleTranscriptSource::Transcript, 0)
                .or_else(|| self.entry_start_row(VisibleTranscriptSource::Pending, 0)),
            VisibleTranscriptSource::Transcript => {
                self.entry_start_row(VisibleTranscriptSource::Pending, 0)
            }
            VisibleTranscriptSource::Pending => None,
        }
    }

    pub fn entry_start_row(
        &self,
        source: VisibleTranscriptSource,
        entry_index: usize,
    ) -> Option<usize> {
        let section_start = match source {
            VisibleTranscriptSource::HistoryBanner => 0,
            VisibleTranscriptSource::Transcript => self.history.total_rows(),
            VisibleTranscriptSource::Pending => self
                .history
                .total_rows()
                .saturating_add(self.transcript.total_rows()),
        };
        self.section(source)
            .entry_start_row(entry_index)
            .map(|row| section_start.saturating_add(row))
    }

    const fn section(&self, source: VisibleTranscriptSource) -> &IndexedSection {
        match source {
            VisibleTranscriptSource::HistoryBanner => &self.history,
            VisibleTranscriptSource::Transcript => &self.transcript,
            VisibleTranscriptSource::Pending => &self.pending,
        }
    }
}
