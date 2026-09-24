//! Indexed retained rows for transcript layout projection.

use std::collections::{BTreeMap, BTreeSet};

use bmux_tui::prelude::Line;

use super::transcript_layout::{
    TranscriptLayoutRows, TranscriptLayoutSignature, VisibleTranscriptLine, VisibleTranscriptSource,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexedEntry {
    signature: TranscriptLayoutSignature,
    rows: Vec<Line>,
    row_count: usize,
    markdown: Option<(
        std::sync::Arc<bcode_markdown_render::MarkdownRenderResult>,
        usize,
    )>,
    selection: BTreeMap<usize, bcode_plugin_sdk::tui::PluginTuiSelectionRow>,
    anchors: Vec<bcode_plugin_sdk::tui_visual::TuiVisualAnchor>,
    changed_end: usize,
    previous_changed_end: usize,
}

impl IndexedEntry {
    fn new(signature: TranscriptLayoutSignature, rows: TranscriptLayoutRows) -> Self {
        let row_count = rows.len();
        let mut markdown = None;
        let mut selection = BTreeMap::new();
        let (rows, anchors) = match rows {
            TranscriptLayoutRows::Markdown {
                rows,
                anchors,
                projection,
                body_start,
            } => {
                markdown = Some((projection, body_start));
                (rows, anchors)
            }
            TranscriptLayoutRows::Selected {
                rows,
                anchors,
                selection: retained,
            } => {
                selection = retained;
                (rows, anchors)
            }
            TranscriptLayoutRows::Rendered(rows) => (rows, Vec::new()),
            TranscriptLayoutRows::Anchored { rows, anchors } => (rows, anchors),
            TranscriptLayoutRows::BlankSpan(0) => (Vec::new(), Vec::new()),
            TranscriptLayoutRows::BlankSpan(_) => (vec![Line::default()], Vec::new()),
        };
        Self {
            signature,
            rows,
            row_count,
            markdown,
            selection,
            anchors,
            changed_end: row_count,
            previous_changed_end: 0,
        }
    }

    fn replace(&mut self, signature: TranscriptLayoutSignature, rows: TranscriptLayoutRows) {
        let mut next = Self::new(signature, rows);
        next.previous_changed_end = self.changed_end;
        next.changed_end = if self.row_count == next.row_count {
            (0..self.rows.len().max(next.rows.len()))
                .rev()
                .find(|&row| self.rows.get(row) != next.rows.get(row))
                .map_or(0, |row| row.saturating_add(1))
        } else {
            self.row_count.max(next.row_count)
        }
        .max(self.changed_end);
        *self = next;
    }

    fn line(&self, row: usize) -> Option<&Line> {
        if row >= self.row_count {
            return None;
        }
        if self.rows.len() == 1 && self.row_count > 1 {
            self.rows.first()
        } else {
            self.rows.get(row)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct FenwickRows {
    tree: Vec<usize>,
}

impl FenwickRows {
    fn rebuild(&mut self, entries: &[IndexedEntry]) {
        self.tree = vec![0; entries.len().saturating_add(1)];
        for (index, entry) in entries.iter().enumerate() {
            self.add(index, entry.row_count);
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

#[allow(
    unknown_lints,
    clippy::manual_isolate_lowest_one,
    reason = "isolate_lowest_one is unstable on the supported local Rust 1.95 toolchain"
)]
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
    changed_entries: BTreeSet<usize>,
    entries: Vec<IndexedEntry>,
    rows: FenwickRows,
}

impl IndexedSection {
    fn clear(&mut self) {
        self.entries.clear();
        self.changed_entries.clear();
        self.rows.tree.clear();
    }

    fn sync<S, R>(&mut self, len: usize, signature: S, mut render_rows: R) -> (usize, usize)
    where
        S: Fn(usize) -> TranscriptLayoutSignature,
        R: FnMut(usize) -> TranscriptLayoutRows,
    {
        self.changed_entries.retain(|&index| index < len);
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
                    entry.replace(signature, rows);
                    if entry.changed_end > 0 {
                        self.changed_entries.insert(index);
                    }
                }
                None => {
                    let rows = render_rows(index);
                    changed = changed.saturating_add(1);
                    rows_regenerated = rows_regenerated.saturating_add(rows.len());
                    self.entries.push(IndexedEntry::new(signature, rows));
                    self.changed_entries.insert(index);
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
        R: FnMut(usize) -> TranscriptLayoutRows,
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
            let old_rows = entry.row_count;
            let new_rows = rows.len();
            entry.replace(signature, rows);
            if entry.changed_end > 0 {
                self.changed_entries.insert(index);
            }
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
        self.entries.get(entry_index)?.line(row_in_entry)
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
            let entry_end = row_cursor.saturating_add(entry.row_count);
            let row_start = start.saturating_sub(row_cursor).min(entry.row_count);
            let row_end = end.saturating_sub(row_cursor).min(entry.row_count);
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
    pub fn changed_content_below(&self, bottom: usize) -> bool {
        [&self.transcript, &self.pending]
            .into_iter()
            .enumerate()
            .any(|(section, entries)| {
                let base = self.history.total_rows()
                    + if section == 1 {
                        self.transcript.total_rows()
                    } else {
                        0
                    };
                entries.changed_entries.iter().any(|&index| {
                    let entry = &entries.entries[index];
                    entry.changed_end > 0
                        && base + entries.rows.prefix(index) + entry.changed_end > bottom
                })
            })
    }

    pub fn suppress_visual_content_changes(&mut self, invocations: &BTreeSet<String>) {
        for invocation in invocations {
            if let Some(indexes) = self.invocation_entries.get(invocation) {
                for &index in indexes {
                    if let Some(entry) = self.transcript.entries.get_mut(index) {
                        entry.changed_end = entry.previous_changed_end;
                    }
                }
            }
        }
    }

    pub fn clear_content_changes(&mut self) {
        for section in [&mut self.transcript, &mut self.pending] {
            for index in std::mem::take(&mut section.changed_entries) {
                section.entries[index].changed_end = 0;
                section.entries[index].previous_changed_end = 0;
            }
        }
    }

    pub fn source_position(&self, index: usize, row: usize) -> Option<usize> {
        let (projection, body_start) = self.transcript.entries.get(index)?.markdown.as_ref()?;
        let row = row.checked_sub(*body_start)?;
        let row = u16::try_from(row).ok()?;
        projection
            .selection_provenance_for_rows(row..row.saturating_add(1))
            .iter()
            .filter_map(|unit| unit.source_ranges.first().map(|range| range.start))
            .min()
    }

    pub fn source_row(&self, index: usize, position: usize) -> Option<usize> {
        let (projection, body_start) = self.transcript.entries.get(index)?.markdown.as_ref()?;
        projection
            .selection_provenance()
            .iter()
            .filter(|unit| {
                unit.source_ranges
                    .iter()
                    .any(|range| range.contains(&position))
            })
            .flat_map(|unit| unit.rects.iter())
            .map(|rect| body_start.saturating_add(usize::from(rect.y)))
            .min()
    }

    pub fn selection_row(
        &self,
        index: usize,
        row: usize,
    ) -> Option<&bcode_plugin_sdk::tui::PluginTuiSelectionRow> {
        self.transcript.entries.get(index)?.selection.get(&row)
    }

    pub fn content_anchor(&self, index: usize, row: usize) -> Option<(&str, usize)> {
        self.transcript
            .entries
            .get(index)?
            .anchors
            .iter()
            .filter(|anchor| anchor.row <= row)
            .max_by_key(|anchor| anchor.row)
            .and_then(|anchor| {
                anchor.source.as_ref().map_or_else(
                    || Some((anchor.key.as_str(), row.saturating_sub(anchor.row))),
                    // Source ranges describe this row, not the unmapped rows after it.
                    // Those rows retain the caller's item-row fallback.
                    |source| {
                        (anchor.row == row).then_some((source.identity.as_str(), source.start))
                    },
                )
            })
    }

    pub fn resolve_content_anchor(&self, index: usize, key: &str, offset: usize) -> Option<usize> {
        let entry = self.transcript.entries.get(index)?;
        if let Some(anchor) = entry.anchors.iter().find(|anchor| {
            anchor.source.as_ref().is_some_and(|source| {
                source.identity == key
                    && ((source.start..source.end).contains(&offset)
                        || source.start == source.end && source.start == offset)
            })
        }) {
            return Some(anchor.row);
        }
        let start = entry.anchors.iter().find(|anchor| anchor.key == key)?.row;
        let end = entry
            .anchors
            .iter()
            .filter(|anchor| anchor.row > start)
            .map(|anchor| anchor.row)
            .min()
            .unwrap_or(entry.row_count);
        Some(start.saturating_add(offset).min(end.saturating_sub(1)))
    }

    pub fn content_anchor_row(&self, index: usize, key: &str) -> Option<usize> {
        self.transcript
            .entries
            .get(index)?
            .anchors
            .iter()
            .find(|anchor| anchor.key == key)
            .map(|anchor| anchor.row)
    }

    pub fn clear(&mut self) {
        self.history.clear();
        self.transcript.clear();
        self.pending.clear();
        self.invocation_entries.clear();
    }

    pub fn sync_history<S, R>(&mut self, signature: Option<S>, rows: R) -> (usize, usize)
    where
        S: Into<TranscriptLayoutSignature>,
        R: FnOnce() -> TranscriptLayoutRows,
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
        R: FnMut(usize) -> TranscriptLayoutRows,
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
        R: FnMut(usize) -> TranscriptLayoutRows,
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
        R: FnMut(usize) -> TranscriptLayoutRows,
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

    /// Synchronize only selected transcript entry indexes.
    pub fn sync_transcript_entries<S, R>(
        &mut self,
        indexes: &BTreeSet<usize>,
        signature: S,
        rows: R,
    ) -> (usize, usize, usize)
    where
        S: Fn(usize) -> TranscriptLayoutSignature,
        R: FnMut(usize) -> TranscriptLayoutRows,
    {
        let scanned = indexes.len();
        let (changed, rows_regenerated) = self.transcript.sync_entries(indexes, signature, rows);
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

    pub fn entry_row_count(
        &self,
        source: VisibleTranscriptSource,
        entry_index: usize,
    ) -> Option<usize> {
        self.section(source)
            .entries
            .get(entry_index)
            .map(|entry| entry.row_count)
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

#[cfg(test)]
mod tests {
    #[test]
    fn content_changes_compare_rows_and_survive_timer_only_replacement() {
        let signature = |n| TranscriptLayoutSignature::new(format!("revision-{n}"));
        let rows = |body: &str| {
            TranscriptLayoutRows::Rendered(vec![Line::from("header"), Line::from(body)])
        };
        let mut entry = IndexedEntry::new(signature(0), rows("old"));
        entry.changed_end = 0;
        entry.replace(signature(1), rows("old"));
        assert_eq!(entry.changed_end, 0, "invalidation is not visual activity");
        entry.replace(signature(2), rows("new"));
        assert_eq!(entry.changed_end, 2, "same-height output changes count");
        entry.replace(
            signature(3),
            TranscriptLayoutRows::Rendered(vec![Line::from("timer"), Line::from("new")]),
        );
        entry.changed_end = entry.previous_changed_end;
        assert_eq!(
            entry.changed_end, 2,
            "timer suppression retains unpresented output changes"
        );
        entry.changed_end = 0;
        entry.replace(
            signature(4),
            TranscriptLayoutRows::Rendered(vec![Line::from("later timer"), Line::from("new")]),
        );
        assert_eq!(
            entry.changed_end, 1,
            "header changes do not mark the body changed"
        );
    }

    #[test]
    fn selection_geometry_belongs_to_each_retained_projection() {
        let mut narrow = IndexedTranscriptLayout::default();
        let mut wide = IndexedTranscriptLayout::default();
        let projection = |text: &str| TranscriptLayoutRows::Selected {
            rows: vec![Line::from(text)],
            anchors: Vec::new(),
            selection: std::collections::BTreeMap::from([(
                0,
                bcode_plugin_sdk::tui::PluginTuiSelectionRow {
                    identity: "same-source".to_owned(),
                    byte_start: 0,
                    text: text.to_owned(),
                    cells: Vec::new(),
                    revision: 1,
                },
            )]),
        };
        narrow.sync_transcript(
            1,
            |_| TranscriptLayoutSignature::new("narrow".to_owned()),
            |_| projection("abc"),
            |_| None,
        );
        wide.sync_transcript(
            1,
            |_| TranscriptLayoutSignature::new("wide".to_owned()),
            |_| projection("abcdef"),
            |_| None,
        );
        assert_eq!(narrow.selection_row(0, 0).unwrap().text, "abc");
        assert_eq!(wide.selection_row(0, 0).unwrap().text, "abcdef");
        assert!(narrow.selection_row(0, 1).is_none());
        // Replacement releases geometry along with its old painted rows.
        narrow.sync_transcript(
            1,
            |_| TranscriptLayoutSignature::new("replacement".to_owned()),
            |_| TranscriptLayoutRows::Rendered(vec![Line::from("new")]),
            |_| None,
        );
        assert!(narrow.selection_row(0, 0).is_none());
        assert_eq!(wide.selection_row(0, 0).unwrap().text, "abcdef");
    }

    #[test]
    fn shrinking_region_anchor_cannot_escape_into_following_region() {
        use bcode_plugin_sdk::tui_visual::TuiVisualAnchor;
        let mut layout = IndexedTranscriptLayout::default();
        layout.sync_transcript(
            1,
            |_| TranscriptLayoutSignature::new("regions".to_owned()),
            |_| TranscriptLayoutRows::Anchored {
                rows: vec![Line::default(); 10],
                anchors: vec![
                    TuiVisualAnchor {
                        key: "body".to_owned(),
                        source: None,
                        row: 1,
                    },
                    TuiVisualAnchor {
                        key: "status".to_owned(),
                        source: None,
                        row: 4,
                    },
                ],
            },
            |_| None,
        );
        assert_eq!(layout.resolve_content_anchor(0, "body", 20), Some(3));
        assert_eq!(layout.resolve_content_anchor(0, "missing", 0), None);
    }

    #[test]
    fn source_correspondence_does_not_capture_unmapped_following_rows() {
        use bcode_plugin_sdk::tui_visual::{TuiVisualAnchor, TuiVisualSourceRange};
        let mut layout = IndexedTranscriptLayout::default();
        layout.sync_transcript(
            1,
            |_| TranscriptLayoutSignature::new("source and trailing chrome".to_owned()),
            |_| TranscriptLayoutRows::Anchored {
                rows: vec![Line::default(); 5],
                anchors: vec![TuiVisualAnchor {
                    key: "source-row".to_owned(),
                    row: 1,
                    source: Some(TuiVisualSourceRange {
                        identity: "source".to_owned(),
                        start: 0,
                        end: 4,
                    }),
                }],
            },
            |_| None,
        );
        assert_eq!(layout.content_anchor(0, 1), Some(("source", 0)));
        for row in 2..5 {
            assert_eq!(layout.content_anchor(0, row), None);
        }
    }

    #[test]
    fn accepted_markdown_source_survives_width_reflow() {
        use bcode_markdown_render::{MarkdownRenderOptions, render_markdown};
        let source = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu";
        let mut layout = IndexedTranscriptLayout::default();
        let make_rows = |width| {
            let projection =
                std::sync::Arc::new(render_markdown(source, &MarkdownRenderOptions::new(width)));
            TranscriptLayoutRows::Markdown {
                rows: projection.lines.clone(),
                anchors: Vec::new(),
                projection,
                body_start: 0,
            }
        };
        layout.sync_transcript(
            1,
            |_| TranscriptLayoutSignature::new("wide".to_owned()),
            |_| make_rows(32),
            |_| None,
        );
        let position = layout
            .source_position(0, 1)
            .expect("source position on second row");
        layout.sync_transcript(
            1,
            |_| TranscriptLayoutSignature::new("narrow".to_owned()),
            |_| make_rows(12),
            |_| None,
        );
        let row = layout
            .source_row(0, position)
            .expect("same source after reflow");
        assert!(row > 1);
        let entry = &layout.transcript.entries[0];
        let (projection, _) = entry.markdown.as_ref().expect("accepted projection");
        assert!(projection.selection_provenance().iter().any(|unit| {
            unit.source_ranges
                .iter()
                .any(|range| range.contains(&position))
                && unit.rects.iter().any(|rect| usize::from(rect.y) == row)
        }));
    }

    #[test]
    fn source_position_resolves_inside_a_reflowed_row() {
        use bcode_plugin_sdk::tui_visual::{TuiVisualAnchor, TuiVisualSourceRange};
        let mut layout = IndexedTranscriptLayout::default();
        let make = |row, start, end| TuiVisualAnchor {
            key: format!("row-{row}"),
            row,
            source: Some(TuiVisualSourceRange {
                identity: "capture:line".to_owned(),
                start,
                end,
            }),
        };
        layout.sync_transcript(
            1,
            |_| TranscriptLayoutSignature::new("narrow".to_owned()),
            |_| TranscriptLayoutRows::Anchored {
                rows: vec![Line::default(); 3],
                anchors: vec![make(0, 0, 3), make(1, 3, 6), make(2, 6, 9)],
            },
            |_| None,
        );
        assert_eq!(layout.content_anchor(0, 2), Some(("capture:line", 6)));
        layout.sync_transcript(
            1,
            |_| TranscriptLayoutSignature::new("wide".to_owned()),
            |_| TranscriptLayoutRows::Anchored {
                rows: vec![Line::default()],
                anchors: vec![make(0, 0, 9)],
            },
            |_| None,
        );
        assert_eq!(layout.resolve_content_anchor(0, "capture:line", 6), Some(0));
        assert_eq!(layout.resolve_content_anchor(0, "stale:line", 6), None);
        assert_eq!(layout.resolve_content_anchor(0, "capture:line", 9), None);
    }

    #[test]
    fn accepted_content_key_resolves_after_header_growth() {
        use bcode_plugin_sdk::tui_visual::TuiVisualAnchor;
        let mut layout = IndexedTranscriptLayout::default();
        layout.sync_transcript(
            1,
            |_| TranscriptLayoutSignature::new("draft".to_owned()),
            |_| TranscriptLayoutRows::Anchored {
                rows: vec![Line::default(); 5],
                anchors: vec![TuiVisualAnchor {
                    key: "body".to_owned(),
                    row: 1,
                    source: None,
                }],
            },
            |_| None,
        );
        assert_eq!(layout.content_anchor(0, 3), Some(("body", 2)));
        layout.sync_transcript(
            1,
            |_| TranscriptLayoutSignature::new("finished".to_owned()),
            |_| TranscriptLayoutRows::Anchored {
                rows: vec![Line::default(); 7],
                anchors: vec![TuiVisualAnchor {
                    key: "body".to_owned(),
                    row: 3,
                    source: None,
                }],
            },
            |_| None,
        );
        assert_eq!(layout.content_anchor_row(0, "body"), Some(3));
    }
    use super::*;

    #[test]
    fn repeated_blank_rows_retain_logical_extent_without_materializing_each_row() {
        let mut layout = IndexedTranscriptLayout::default();
        layout.sync_transcript(
            1,
            |_| TranscriptLayoutSignature::new("interaction".to_owned()),
            |_| TranscriptLayoutRows::BlankSpan(100_000),
            |_| None,
        );

        assert_eq!(layout.total_rows(), 100_000);
        assert_eq!(
            layout.entry_row_count(VisibleTranscriptSource::Transcript, 0),
            Some(100_000)
        );
        assert_eq!(layout.transcript.entries[0].rows.len(), 1);
        let visible = layout.visible_lines_from_top(99_995, 5);
        assert_eq!(visible.len(), 5);
        assert!(visible.iter().all(|line| layout.line(*line).is_some()));
    }
}
