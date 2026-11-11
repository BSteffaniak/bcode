//! Item construction for one block (spec sections 5.1–5.3).
//!
//! A file block is built from the file's index entry, its resident content, its revealed
//! context, and the annotations anchored in it. Unified and side-by-side items come from the
//! same data; only line items differ.

use super::anchors::{AnchorLines, AnchorTarget, AnnotationBody};
use super::document::{
    AttributionState, Block, BlockKind, Content, DocHunk, DocLine, FileState, Loaded,
    ReviewDocument,
};
use super::gaps::{self, Region};
use super::items::{
    AgentThreadItem, AnchorPlacement, ComposerItem, FileHeaderItem, FileNoticeItem, GapItem,
    GapState, HunkHeaderItem, LayoutMode, LineContent, LineItem, LineKind, LinePairItem,
    OutdatedReason, PairSide, PlaceholderItem, ReviewItem, SourceHeaderItem, StackLayerLabel,
    ThreadItem,
};
use super::keys::{FileNoticeKind, FileRef};
use bcode_code_review_models::{ReviewIndexFlag, ReviewLineKind, ReviewLineSide};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Items of one block with their hunk-section parents.
pub struct Built {
    pub items: Vec<ReviewItem>,
    pub hunk_parent: Vec<Option<u32>>,
}

/// Estimated rows of a block: placeholders count their estimate, every other item one row.
pub fn block_rows(block: &Block) -> u64 {
    block
        .items
        .iter()
        .map(|item| match item {
            ReviewItem::Placeholder(item) => u64::from(item.estimated_rows),
            _ => 1,
        })
        .sum()
}

/// Estimated rows of a file's content from its `+/-` counts: changed lines, plus a header and
/// six context lines per estimated hunk of up to ten changed lines.
#[must_use]
pub fn estimate_rows(additions: u32, deletions: u32, layout: LayoutMode) -> u32 {
    let changed = additions.saturating_add(deletions);
    if changed == 0 {
        return 1;
    }
    let hunks = changed.div_ceil(10).max(1);
    let lines = match layout {
        LayoutMode::Unified => changed,
        LayoutMode::SideBySide => additions.max(deletions),
    };
    lines.saturating_add(hunks.saturating_mul(7))
}

/// What an item emitted at an annotation slot shows.
#[derive(Debug, Clone, Copy)]
enum Emit {
    Annotation(usize, AnchorPlacement),
    Composer,
}

/// Where an anchor resolves within a resident file.
enum Location {
    /// A hunk line of this kind; the slot key is the anchor's own side and line.
    Hunk(ReviewLineKind),
    /// An unchanged line outside hunks, by context-side line.
    Region {
        context_line: u32,
        revealed: bool,
    },
    /// Past the last returned hunk of a truncated diff.
    Truncated,
    Missing,
}

/// Annotation slots of one file block.
#[derive(Default)]
struct Slots {
    file_level: Vec<Emit>,
    lines: BTreeMap<(ReviewLineSide, u32), Vec<Emit>>,
    /// Hidden anchors by context-side line.
    hidden: BTreeMap<u32, Vec<Emit>>,
}

impl ReviewDocument {
    pub(super) fn build_block(&self, block: usize) -> Built {
        match self.blocks[block].kind {
            BlockKind::ReviewNotes => self.build_review_notes(),
            BlockKind::SourceHeader(group) => self.build_source_header(group),
            BlockKind::File(file) => self.build_file(file),
        }
    }

    fn build_review_notes(&self) -> Built {
        let mut items = Vec::new();
        for &index in &self.review_annotations {
            let placement = if self.annotations[index].target == AnchorTarget::Review {
                AnchorPlacement::Review
            } else {
                AnchorPlacement::Outdated(OutdatedReason::FileMissing)
            };
            items.push(self.annotation_item(index, placement));
        }
        if let Some(anchor) = &self.composer
            && anchor
                .file
                .as_ref()
                .is_none_or(|file| !self.file_index.contains_key(file))
        {
            items.push(ReviewItem::Composer(ComposerItem {
                anchor: anchor.clone(),
            }));
        }
        let hunk_parent = vec![None; items.len()];
        Built { items, hunk_parent }
    }

    fn build_source_header(&self, group: usize) -> Built {
        let group = &self.groups[group];
        if !group.show_header {
            return Built {
                items: Vec::new(),
                hunk_parent: Vec::new(),
            };
        }
        let layer = group.stack.as_ref().and_then(|stack| {
            let top = stack.stack.layers.last()?;
            Some(StackLayerLabel {
                number: top.number,
                title: top.title.clone(),
                position: stack.stack.layers.len(),
                count: stack.stack.layers.len(),
                whole_stack: stack.whole,
            })
        });
        let label = group.source.part.as_ref().map_or_else(
            || group.label.clone(),
            |part| format!("{} · {}", group.label, part.label),
        );
        let item = ReviewItem::SourceHeader(SourceHeaderItem {
            source_id: group.source.source_id.clone(),
            part: group.source.part.as_ref().map(|part| part.index),
            label,
            layer,
            file_count: group.files.len(),
            viewed_count: group.viewed,
        });
        Built {
            items: vec![item],
            hunk_parent: vec![None],
        }
    }

    fn annotation_item(&self, index: usize, placement: AnchorPlacement) -> ReviewItem {
        let annotation = &self.annotations[index];
        let lines = match &annotation.target {
            AnchorTarget::Lines(lines) => {
                Some((lines.end_side, lines.start_line(), lines.end_line()))
            }
            AnchorTarget::Review | AnchorTarget::File => None,
        };
        match &annotation.body {
            AnnotationBody::Thread {
                comment_ids,
                resolved,
                kind,
                severity,
            } => ReviewItem::Thread(ThreadItem {
                thread_id: match &annotation.key {
                    super::keys::ItemKey::Thread { thread_id } => thread_id.clone(),
                    _ => String::new(),
                },
                placement,
                comment_ids: comment_ids.clone(),
                suggestion_ids: annotation.suggestion_ids.clone(),
                resolved: *resolved,
                kind: *kind,
                severity: *severity,
                lines,
            }),
            AnnotationBody::Agent { exchange_id } => ReviewItem::AgentThread(AgentThreadItem {
                id: match &annotation.key {
                    super::keys::ItemKey::AgentThread { id } => id.clone(),
                    _ => String::new(),
                },
                exchange_id: exchange_id.clone(),
                placement,
                suggestion_ids: annotation.suggestion_ids.clone(),
                lines,
            }),
        }
    }

    fn emit(&self, emit: Emit, items: &mut Vec<ReviewItem>) {
        match emit {
            Emit::Annotation(index, placement) => {
                items.push(self.annotation_item(index, placement));
            }
            Emit::Composer => {
                if let Some(anchor) = &self.composer {
                    items.push(ReviewItem::Composer(ComposerItem {
                        anchor: anchor.clone(),
                    }));
                }
            }
        }
    }

    fn build_file(&self, file: usize) -> Built {
        let state = &self.files[file];
        let key = state.key.clone();
        let attribution = match &state.attribution {
            AttributionState::Loaded(attribution) => Some(attribution),
            _ => None,
        };
        let mut items = vec![ReviewItem::FileHeader(FileHeaderItem {
            file: key.clone(),
            status: state.entry.status,
            old_path: state.entry.old_path.clone(),
            additions: state.entry.additions,
            deletions: state.entry.deletions,
            flags: state.entry.flags.clone(),
            viewed: state.viewed.is_viewed(),
            load: state.load_state(),
            layers: attribution
                .map(super::document::Attribution::layers)
                .unwrap_or_default(),
            thread_count: self.file_annotations[file].len(),
        })];
        let mut parents = vec![None];
        let regions = state.regions();
        let slots = self.slots(file, &regions);
        for emit in &slots.file_level {
            self.emit(*emit, &mut items);
        }
        parents.resize(items.len(), None);
        match &state.content {
            Content::Unloaded | Content::Loading(_) => {
                items.push(ReviewItem::Placeholder(PlaceholderItem {
                    file: key,
                    estimated_rows: if self.groups[state.group].source.mode
                        == bcode_code_review_models::ResolvedSourceMode::Browse
                    {
                        1
                    } else {
                        estimate_rows(state.entry.additions, state.entry.deletions, self.layout)
                    },
                    loading: matches!(state.content, Content::Loading(_)),
                }));
                parents.push(None);
            }
            Content::Failed(reason) => {
                items.push(notice(
                    &key,
                    FileNoticeKind::Unavailable,
                    Some(reason.clone()),
                ));
                parents.push(None);
            }
            Content::Loaded(loaded) => {
                let mut builder = FileBuilder {
                    doc: self,
                    state,
                    loaded,
                    key,
                    items,
                    parents,
                    slots,
                };
                builder.content(&regions);
                return Built {
                    items: builder.items,
                    hunk_parent: builder.parents,
                };
            }
        }
        Built {
            items,
            hunk_parent: parents,
        }
    }

    /// Resolve every annotation of `file` (and the composer) to a slot.
    fn slots(&self, file: usize, regions: &[Region]) -> Slots {
        let state = &self.files[file];
        let mut slots = Slots::default();
        let place = |emit: Emit, target: &AnchorTarget, slots: &mut Slots| {
            let AnchorTarget::Lines(lines) = target else {
                slots.file_level.push(emit);
                return;
            };
            let Some(loaded) = state.loaded() else {
                // Line anchors are placed once content is resident; the composer stays visible.
                if matches!(emit, Emit::Composer) {
                    slots.file_level.push(emit);
                }
                return;
            };
            let (placement, slot) = locate_anchor(state, loaded, regions, lines);
            let emit = match emit {
                Emit::Annotation(index, _) => Emit::Annotation(index, placement),
                Emit::Composer => Emit::Composer,
            };
            match slot {
                Slot::File => slots.file_level.push(emit),
                Slot::Line(side, line) => slots.lines.entry((side, line)).or_default().push(emit),
                Slot::Hidden(line) => slots.hidden.entry(line).or_default().push(emit),
            }
        };
        for &index in &self.file_annotations[file] {
            let emit = Emit::Annotation(index, AnchorPlacement::File);
            place(emit, &self.annotations[index].target, &mut slots);
        }
        if let Some(anchor) = &self.composer
            && anchor.file.as_ref() == Some(&state.key)
        {
            place(Emit::Composer, &anchor.target, &mut slots);
        }
        slots
    }
}

enum Slot {
    File,
    Line(ReviewLineSide, u32),
    Hidden(u32),
}

fn locate_anchor(
    state: &FileState,
    loaded: &Loaded,
    regions: &[Region],
    lines: &AnchorLines,
) -> (AnchorPlacement, Slot) {
    let end = (lines.end_side, lines.end_line());
    let start = (lines.end_side, lines.start_line());
    let end_location = locate_line(state, loaded, regions, end.0, end.1);
    let outdated = |reason| (AnchorPlacement::Outdated(reason), Slot::File);
    if lines.is_range {
        if matches!(
            locate_line(state, loaded, regions, start.0, start.1),
            Location::Missing
        ) {
            return outdated(OutdatedReason::LineMissing);
        }
    } else {
        let kind_matches = match (&end_location, lines.end_kind) {
            (Location::Hunk(kind), expected) => *kind == expected,
            (Location::Region { .. }, kind) => kind == ReviewLineKind::Context,
            (Location::Truncated | Location::Missing, _) => true,
        };
        if !kind_matches {
            return outdated(OutdatedReason::LineChanged);
        }
    }
    match end_location {
        Location::Hunk(_) => (AnchorPlacement::Line, Slot::Line(end.0, end.1)),
        Location::Region {
            context_line,
            revealed: true,
        } => (
            AnchorPlacement::Line,
            Slot::Line(state.context_side(), context_line),
        ),
        Location::Region { context_line, .. } => {
            (AnchorPlacement::HiddenContext, Slot::Hidden(context_line))
        }
        Location::Truncated => (AnchorPlacement::HiddenContext, Slot::File),
        Location::Missing => outdated(OutdatedReason::LineMissing),
    }
}

fn locate_line(
    state: &FileState,
    loaded: &Loaded,
    regions: &[Region],
    side: ReviewLineSide,
    line: u32,
) -> Location {
    if line == 0 || loaded.binary {
        return Location::Missing;
    }
    let index = loaded
        .ranges
        .partition_point(|range| gaps::hunk_span(range, side).next <= line);
    if let Some(hunk) = loaded.hunks.get(index)
        && gaps::hunk_span(&hunk.range, side).first <= line
    {
        let found = hunk.lines.iter().find(|candidate| match side {
            ReviewLineSide::Old => candidate.old == Some(line),
            ReviewLineSide::New => candidate.new == Some(line),
        });
        return found.map_or(Location::Missing, |line| Location::Hunk(line.kind));
    }
    let context_side = state.context_side();
    for region in regions {
        let context_line = if side == context_side {
            Some(line)
        } else {
            region
                .other_offset
                .and_then(|offset| u32::try_from(i64::from(line) - offset).ok())
        };
        if let Some(context_line) = context_line
            && region.contains(context_line)
        {
            return Location::Region {
                context_line,
                revealed: region.end.is_some() && state.revealed.contains(context_line),
            };
        }
    }
    if loaded.truncated.is_some()
        && loaded
            .ranges
            .last()
            .is_some_and(|last| line >= gaps::hunk_span(last, side).next)
    {
        return Location::Truncated;
    }
    Location::Missing
}

fn notice(file: &FileRef, kind: FileNoticeKind, reason: Option<Arc<str>>) -> ReviewItem {
    ReviewItem::FileNotice(FileNoticeItem {
        file: file.clone(),
        kind,
        reason,
        truncation: None,
    })
}

const fn line_kind(kind: ReviewLineKind) -> LineKind {
    match kind {
        ReviewLineKind::Context => LineKind::Context,
        ReviewLineKind::Added => LineKind::Added,
        ReviewLineKind::Removed => LineKind::Removed,
    }
}

struct FileBuilder<'a> {
    doc: &'a ReviewDocument,
    state: &'a FileState,
    loaded: &'a Loaded,
    key: FileRef,
    items: Vec<ReviewItem>,
    parents: Vec<Option<u32>>,
    slots: Slots,
}

impl FileBuilder<'_> {
    fn push(&mut self, item: ReviewItem, parent: Option<u32>) {
        self.items.push(item);
        self.parents.push(parent);
    }

    fn layer(&self, side: ReviewLineSide, line: Option<u32>, kind: LineKind) -> Option<u64> {
        if !kind.is_change() {
            return None;
        }
        match &self.state.attribution {
            AttributionState::Loaded(attribution) => attribution.layer(side, line?),
            _ => None,
        }
    }

    /// Emit the annotations anchored at `(side, line)`.
    fn after_line(&mut self, side: ReviewLineSide, line: Option<u32>, parent: Option<u32>) {
        let Some(line) = line else { return };
        if let Some(emits) = self.slots.lines.remove(&(side, line)) {
            for emit in emits {
                let before = self.items.len();
                self.doc.emit(emit, &mut self.items);
                self.parents.resize(self.items.len(), parent);
                debug_assert!(self.items.len() > before || matches!(emit, Emit::Composer));
            }
        }
    }

    fn content(&mut self, regions: &[Region]) {
        let loaded = self.loaded;
        if loaded.binary || self.state.entry.has(ReviewIndexFlag::Binary) && loaded.hunks.is_empty()
        {
            let item = notice(&self.key, FileNoticeKind::Binary, None);
            self.push(item, None);
            return;
        }
        let browse = self.doc.groups[self.state.group].source.mode
            == bcode_code_review_models::ResolvedSourceMode::Browse;
        if loaded.hunks.is_empty() && !browse && loaded.truncated.is_none() {
            let item = notice(&self.key, FileNoticeKind::NoTextChanges, None);
            self.push(item, None);
        }
        let mut next_region = 0;
        for (index, hunk) in loaded.hunks.iter().enumerate() {
            while next_region < regions.len() && regions[next_region].before_hunk == Some(index) {
                self.region(&regions[next_region]);
                next_region += 1;
            }
            self.hunk(hunk);
        }
        for region in &regions[next_region..] {
            self.region(region);
        }
        if let Some(truncation) = loaded.truncated {
            self.push(
                ReviewItem::FileNotice(FileNoticeItem {
                    file: self.key.clone(),
                    kind: FileNoticeKind::TooLarge,
                    reason: None,
                    truncation: Some(truncation),
                }),
                None,
            );
        }
        // Anchors that found no item (for example a hidden anchor after the trailing open gap
        // was closed by the line count) are shown at the file end rather than dropped.
        let leftovers: Vec<Emit> = std::mem::take(&mut self.slots.lines)
            .into_values()
            .chain(std::mem::take(&mut self.slots.hidden).into_values())
            .flatten()
            .collect();
        for emit in leftovers {
            self.doc.emit(emit, &mut self.items);
            self.parents.resize(self.items.len(), None);
        }
    }

    fn hunk(&mut self, hunk: &DocHunk) {
        let header = u32::try_from(self.items.len()).ok();
        let layers = match &self.state.attribution {
            AttributionState::Loaded(attribution) => {
                let mut layers: Vec<u64> = hunk
                    .lines
                    .iter()
                    .filter_map(|line| match line.kind {
                        ReviewLineKind::Added => attribution.layer(ReviewLineSide::New, line.new?),
                        ReviewLineKind::Removed => {
                            attribution.layer(ReviewLineSide::Old, line.old?)
                        }
                        ReviewLineKind::Context => None,
                    })
                    .collect();
                layers.sort_unstable();
                layers.dedup();
                layers
            }
            _ => Vec::new(),
        };
        self.push(
            ReviewItem::HunkHeader(HunkHeaderItem {
                file: self.key.clone(),
                old_start: hunk.range.old_start,
                old_count: hunk.range.old_count,
                new_start: hunk.range.new_start,
                new_count: hunk.range.new_count,
                heading: hunk.heading.clone(),
                layers,
            }),
            header,
        );
        match self.doc.layout {
            LayoutMode::Unified => {
                for line in &hunk.lines {
                    self.unified_line(line, header);
                }
            }
            LayoutMode::SideBySide => self.paired_lines(&hunk.lines, header),
        }
    }

    fn unified_line(&mut self, line: &DocLine, parent: Option<u32>) {
        let kind = line_kind(line.kind);
        let side = if kind == LineKind::Removed {
            ReviewLineSide::Old
        } else {
            ReviewLineSide::New
        };
        let layer = self.layer(
            side,
            if side == ReviewLineSide::Old {
                line.old
            } else {
                line.new
            },
            kind,
        );
        self.push(
            ReviewItem::Line(LineItem {
                file: self.key.clone(),
                kind,
                old_line: line.old,
                new_line: line.new,
                content: LineContent::Text(line.text.clone()),
                layer,
            }),
            parent,
        );
        self.after_line(ReviewLineSide::Old, line.old, parent);
        self.after_line(ReviewLineSide::New, line.new, parent);
    }

    fn pair_side(&self, line: &DocLine, side: ReviewLineSide) -> Option<PairSide> {
        let number = match side {
            ReviewLineSide::Old => line.old,
            ReviewLineSide::New => line.new,
        }?;
        let kind = line_kind(line.kind);
        Some(PairSide {
            line: number,
            kind,
            content: LineContent::Text(line.text.clone()),
            layer: self.layer(side, Some(number), kind),
        })
    }

    fn push_pair(&mut self, old: Option<PairSide>, new: Option<PairSide>, parent: Option<u32>) {
        let emphasize = old
            .as_ref()
            .is_some_and(|side| side.kind == LineKind::Removed)
            && new
                .as_ref()
                .is_some_and(|side| side.kind == LineKind::Added);
        let (old_line, new_line) = (
            old.as_ref().map(|side| side.line),
            new.as_ref().map(|side| side.line),
        );
        self.push(
            ReviewItem::LinePair(LinePairItem {
                file: self.key.clone(),
                old,
                new,
                emphasize,
            }),
            parent,
        );
        self.after_line(ReviewLineSide::Old, old_line, parent);
        self.after_line(ReviewLineSide::New, new_line, parent);
    }

    /// Pair removed and added runs of a hunk positionally; context lines pair with themselves.
    fn paired_lines(&mut self, lines: &[DocLine], parent: Option<u32>) {
        let mut removed: Vec<&DocLine> = Vec::new();
        let mut added: Vec<&DocLine> = Vec::new();
        let flush = |this: &mut Self, removed: &mut Vec<&DocLine>, added: &mut Vec<&DocLine>| {
            for row in 0..removed.len().max(added.len()) {
                let old = removed
                    .get(row)
                    .and_then(|line| this.pair_side(line, ReviewLineSide::Old));
                let new = added
                    .get(row)
                    .and_then(|line| this.pair_side(line, ReviewLineSide::New));
                this.push_pair(old, new, parent);
            }
            removed.clear();
            added.clear();
        };
        for line in lines {
            match line.kind {
                ReviewLineKind::Removed => {
                    if !added.is_empty() {
                        flush(self, &mut removed, &mut added);
                    }
                    removed.push(line);
                }
                ReviewLineKind::Added => added.push(line),
                ReviewLineKind::Context => {
                    flush(self, &mut removed, &mut added);
                    let old = self.pair_side(line, ReviewLineSide::Old);
                    let new = self.pair_side(line, ReviewLineSide::New);
                    self.push_pair(old, new, parent);
                }
            }
        }
        flush(self, &mut removed, &mut added);
    }

    fn region(&mut self, region: &Region) {
        let side = self.state.context_side();
        let Some(end) = region.end else {
            let hidden_threads = self.slots.hidden.values().map(Vec::len).sum();
            self.push(
                ReviewItem::Gap(GapItem {
                    file: self.key.clone(),
                    side,
                    start: region.start,
                    end: None,
                    other_start: region.other_line(region.start),
                    state: GapState::OpenEnded,
                    hidden_threads,
                }),
                None,
            );
            let hidden = std::mem::take(&mut self.slots.hidden);
            for emit in hidden.into_values().flatten() {
                self.doc.emit(emit, &mut self.items);
                self.parents.resize(self.items.len(), None);
            }
            return;
        };
        let mut cursor = region.start;
        for (start, stop) in self.state.revealed.intersect(region.start, end) {
            if start > cursor {
                self.gap(region, cursor, start - 1);
            }
            for line in start..=stop {
                self.expanded_line(region, line);
            }
            cursor = stop.saturating_add(1);
        }
        if cursor <= end && cursor >= region.start {
            self.gap(region, cursor, end);
        }
    }

    fn gap(&mut self, region: &Region, start: u32, end: u32) {
        let hidden: Vec<u32> = self
            .slots
            .hidden
            .range(start..=end)
            .map(|(line, _)| *line)
            .collect();
        let hidden_threads = hidden
            .iter()
            .filter_map(|line| self.slots.hidden.get(line))
            .map(Vec::len)
            .sum();
        self.push(
            ReviewItem::Gap(GapItem {
                file: self.key.clone(),
                side: self.state.context_side(),
                start,
                end: Some(end),
                other_start: region.other_line(start),
                state: GapState::Hidden,
                hidden_threads,
            }),
            None,
        );
        for line in hidden {
            if let Some(emits) = self.slots.hidden.remove(&line) {
                for emit in emits {
                    self.doc.emit(emit, &mut self.items);
                    self.parents.resize(self.items.len(), None);
                }
            }
        }
    }

    fn expanded_content(&self, line: u32) -> LineContent {
        if let Some(text) = self.loaded.context.get(&line) {
            return LineContent::Text(text.clone());
        }
        self.loaded
            .unavailable
            .iter()
            .rev()
            .find(|(start, end, _)| (*start..=*end).contains(&line))
            .map_or(LineContent::Loading, |(_, _, reason)| {
                LineContent::Unavailable(reason.clone())
            })
    }

    fn expanded_line(&mut self, region: &Region, line: u32) {
        let side = self.state.context_side();
        let other = region.other_line(line);
        let content = self.expanded_content(line);
        let (old, new) = match side {
            ReviewLineSide::New => (other, Some(line)),
            ReviewLineSide::Old => (Some(line), other),
        };
        match self.doc.layout {
            LayoutMode::Unified => {
                self.push(
                    ReviewItem::Line(LineItem {
                        file: self.key.clone(),
                        kind: LineKind::Expanded,
                        old_line: old,
                        new_line: new,
                        content,
                        layer: None,
                    }),
                    None,
                );
            }
            LayoutMode::SideBySide => {
                let make = |line: Option<u32>| {
                    line.map(|line| PairSide {
                        line,
                        kind: LineKind::Expanded,
                        content: content.clone(),
                        layer: None,
                    })
                };
                self.push(
                    ReviewItem::LinePair(LinePairItem {
                        file: self.key.clone(),
                        old: make(old),
                        new: make(new),
                        emphasize: false,
                    }),
                    None,
                );
            }
        }
        self.after_line(side, Some(line), None);
    }
}
