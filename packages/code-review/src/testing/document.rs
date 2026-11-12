//! Synthetic review data and a fake data service for tests of the review document and the
//! screens over it.

use crate::document::{
    DocumentConfig, DocumentNeed, FileKey, ItemKey, ReviewDocument, ReviewItem, SourceInput, Splice,
};
use bcode_code_review_models::{
    ResolvedSource, ResolvedSourceMode, ReviewBlobLinesResponse, ReviewBlobRef, ReviewBlobSelector,
    ReviewFileHunksResponse, ReviewFileStatus, ReviewHunk, ReviewHunksTruncation, ReviewIndexEntry,
    ReviewIndexFlag, ReviewLine, ReviewLineKind, ReviewStackAttributionResponse, RevisionRef,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Deterministic xorshift64* generator (no external dependency needed for property tests).
pub struct Rng(u64);

impl Rng {
    /// A generator seeded with `seed`.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub const fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A value in `0..bound` (0 when `bound` is 0).
    pub const fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 { 0 } else { self.next() % bound }
    }

    /// A value in `low..=high`.
    ///
    /// # Panics
    ///
    /// Panics when the synthetic data is inconsistent (a test bug).
    pub fn range(&mut self, low: u32, high: u32) -> u32 {
        low + u32::try_from(self.below(u64::from(high - low) + 1)).expect("fits")
    }

    /// Shuffle `items` in place.
    ///
    /// # Panics
    ///
    /// Panics when the synthetic data is inconsistent (a test bug).
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for index in (1..items.len()).rev() {
            let other = usize::try_from(self.below(index as u64 + 1)).expect("fits");
            items.swap(index, other);
        }
    }
}

/// One edit in old-side coordinates: at old line `at` (1-based), remove `remove` lines and
/// insert `add` new lines.
#[derive(Debug, Clone, Copy)]
pub struct Edit {
    pub at: u32,
    pub remove: u32,
    pub add: u32,
}

/// A synthetic file with both sides and its unified diff.
#[derive(Debug, Clone)]
pub struct SynthFile {
    pub path: String,
    pub old_path: Option<String>,
    pub status: ReviewFileStatus,
    pub old: Option<Vec<String>>,
    pub new: Option<Vec<String>>,
    pub hunks: Vec<ReviewHunk>,
    pub binary: bool,
    pub patch_id: Option<String>,
    pub truncated: Option<ReviewHunksTruncation>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Same(u32, u32),
    Del(u32),
    Add(u32),
}

impl SynthFile {
    /// A modified file of `lines` old lines with `edits` (sorted, non-overlapping).
    #[must_use]
    pub fn modified(path: &str, lines: u32, edits: &[Edit], context: u32) -> Self {
        let old: Vec<String> = (1..=lines).map(|n| format!("{path} old {n}")).collect();
        let mut ops = Vec::new();
        let mut new_number = 0;
        let mut old_number = 1;
        let mut new = Vec::new();
        for edit in edits {
            while old_number < edit.at {
                new_number += 1;
                new.push(old[(old_number - 1) as usize].clone());
                ops.push(Op::Same(old_number, new_number));
                old_number += 1;
            }
            for _ in 0..edit.remove {
                ops.push(Op::Del(old_number));
                old_number += 1;
            }
            for index in 0..edit.add {
                new_number += 1;
                new.push(format!("{path} new {} ({index})", edit.at));
                ops.push(Op::Add(new_number));
            }
        }
        while old_number <= lines {
            new_number += 1;
            new.push(old[(old_number - 1) as usize].clone());
            ops.push(Op::Same(old_number, new_number));
            old_number += 1;
        }
        let hunks = hunks_from_ops(&ops, &old, &new, context);
        Self {
            path: path.to_string(),
            old_path: None,
            status: ReviewFileStatus::Modified,
            patch_id: Some(crate::viewed::versioned_fingerprint(&format!(
                "patch:{path}:{edits:?}"
            ))),
            old: Some(old),
            new: Some(new),
            hunks,
            binary: false,
            truncated: None,
        }
    }

    pub fn added(path: &str, lines: u32) -> Self {
        let new: Vec<String> = (1..=lines).map(|n| format!("{path} added {n}")).collect();
        let ops: Vec<Op> = (1..=lines).map(Op::Add).collect();
        Self {
            path: path.to_string(),
            old_path: None,
            status: ReviewFileStatus::Added,
            hunks: hunks_from_ops(&ops, &[], &new, 3),
            old: None,
            new: Some(new),
            binary: false,
            patch_id: Some(crate::viewed::versioned_fingerprint(&format!(
                "patch:{path}"
            ))),
            truncated: None,
        }
    }

    pub fn deleted(path: &str, lines: u32) -> Self {
        let old: Vec<String> = (1..=lines).map(|n| format!("{path} deleted {n}")).collect();
        let ops: Vec<Op> = (1..=lines).map(Op::Del).collect();
        Self {
            path: path.to_string(),
            old_path: None,
            status: ReviewFileStatus::Deleted,
            hunks: hunks_from_ops(&ops, &old, &[], 3),
            old: Some(old),
            new: None,
            binary: false,
            patch_id: Some(crate::viewed::versioned_fingerprint(&format!(
                "patch:{path}"
            ))),
            truncated: None,
        }
    }

    /// A binary file.
    #[must_use]
    pub fn binary(path: &str) -> Self {
        Self {
            path: path.to_string(),
            old_path: None,
            status: ReviewFileStatus::Modified,
            hunks: Vec::new(),
            old: Some(Vec::new()),
            new: Some(Vec::new()),
            binary: true,
            patch_id: None,
            truncated: None,
        }
    }

    /// Blob id of the old side.
    #[must_use]
    pub fn old_oid(&self) -> String {
        format!("old:{}", self.old_path.as_deref().unwrap_or(&self.path))
    }

    /// Blob id of the new side.
    #[must_use]
    pub fn new_oid(&self) -> String {
        format!("new:{}", self.path)
    }

    /// Index entry of the file in `source_id`.
    ///
    /// # Panics
    ///
    /// Panics when the synthetic data is inconsistent (a test bug).
    #[must_use]
    pub fn entry(&self, source_id: &str) -> ReviewIndexEntry {
        let count = |kind| {
            let lines = self.hunks.iter().flat_map(|hunk| &hunk.lines);
            u32::try_from(lines.filter(|line| line.kind == kind).count()).expect("fits")
        };
        let mut flags = BTreeSet::new();
        if self.binary {
            flags.insert(ReviewIndexFlag::Binary);
        }
        ReviewIndexEntry {
            source_id: source_id.to_string(),
            path: self.path.clone(),
            old_path: self.old_path.clone(),
            status: self.status,
            old_blob: if self.old.is_some() {
                ReviewBlobRef::Object {
                    oid: self.old_oid(),
                }
            } else {
                ReviewBlobRef::Absent
            },
            new_blob: if self.new.is_some() {
                ReviewBlobRef::Object {
                    oid: self.new_oid(),
                }
            } else {
                ReviewBlobRef::Absent
            },
            old_mode: None,
            new_mode: None,
            similarity: None,
            additions: count(ReviewLineKind::Added),
            deletions: count(ReviewLineKind::Removed),
            flags,
        }
    }

    /// Hunks of the file in `source_id`.
    #[must_use]
    pub fn hunks_response(&self, source_id: &str) -> ReviewFileHunksResponse {
        ReviewFileHunksResponse {
            source_id: source_id.to_string(),
            path: self.path.clone(),
            hunks: self.hunks.clone(),
            binary: self.binary,
            truncated: self.truncated,
            patch_id: self.patch_id.clone(),
        }
    }
}

fn hunks_from_ops(ops: &[Op], old: &[String], new: &[String], context: u32) -> Vec<ReviewHunk> {
    let context = context as usize;
    let changes: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| !matches!(op, Op::Same(..)))
        .map(|(index, _)| index)
        .collect();
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for index in changes {
        let start = index.saturating_sub(context);
        let end = (index + context).min(ops.len() - 1);
        match groups.last_mut() {
            Some(last) if start <= last.1 + 1 => last.1 = end,
            _ => groups.push((start, end)),
        }
    }
    groups
        .into_iter()
        .map(|(start, end)| {
            let lines: Vec<ReviewLine> = ops[start..=end]
                .iter()
                .map(|op| match *op {
                    Op::Same(o, n) => ReviewLine {
                        kind: ReviewLineKind::Context,
                        old_line: Some(o),
                        new_line: Some(n),
                        content: new[(n - 1) as usize].clone(),
                    },
                    Op::Del(o) => ReviewLine {
                        kind: ReviewLineKind::Removed,
                        old_line: Some(o),
                        new_line: None,
                        content: old[(o - 1) as usize].clone(),
                    },
                    Op::Add(n) => ReviewLine {
                        kind: ReviewLineKind::Added,
                        old_line: None,
                        new_line: Some(n),
                        content: new[(n - 1) as usize].clone(),
                    },
                })
                .collect();
            let old_lines: Vec<u32> = lines.iter().filter_map(|line| line.old_line).collect();
            let new_lines: Vec<u32> = lines.iter().filter_map(|line| line.new_line).collect();
            // Git reports an empty side as the line before the hunk.
            let before = |ops: &[Op], old_side: bool| {
                ops[..start]
                    .iter()
                    .rev()
                    .find_map(|op| match (*op, old_side) {
                        (Op::Same(o, _) | Op::Del(o), true) => Some(o),
                        (Op::Same(_, n) | Op::Add(n), false) => Some(n),
                        _ => None,
                    })
                    .unwrap_or(0)
            };
            ReviewHunk {
                old_start: old_lines
                    .first()
                    .copied()
                    .unwrap_or_else(|| before(ops, true)),
                old_count: u32::try_from(old_lines.len()).expect("fits"),
                new_start: new_lines
                    .first()
                    .copied()
                    .unwrap_or_else(|| before(ops, false)),
                new_count: u32::try_from(new_lines.len()).expect("fits"),
                heading: None,
                lines,
            }
        })
        .collect()
}

pub const SOURCE: &str = "source-1";

/// A resolved commit-range source with id `source_id`.
#[must_use]
pub fn resolved(source_id: &str) -> ResolvedSource {
    ResolvedSource {
        source_id: source_id.to_string(),
        base: RevisionRef::commit("base"),
        head: RevisionRef::commit("head"),
        mode: ResolvedSourceMode::Diff,
        paths: Vec::new(),
        line_range: None,
        part: None,
    }
}

/// A fake service answering needs from synthetic files.
pub struct Fake {
    pub files: BTreeMap<String, SynthFile>,
    pub attribution: BTreeMap<String, ReviewStackAttributionResponse>,
}

impl Fake {
    /// A fake serving `files`.
    #[must_use]
    pub fn new(files: Vec<SynthFile>) -> Self {
        Self {
            files: files
                .into_iter()
                .map(|file| (file.path.clone(), file))
                .collect(),
            attribution: BTreeMap::new(),
        }
    }

    /// Document input of the files as source `source_id`.
    #[must_use]
    pub fn input(&self, source_id: &str) -> SourceInput {
        SourceInput {
            source: resolved(source_id),
            label: source_id.to_string(),
            entries: self
                .files
                .values()
                .map(|file| file.entry(source_id))
                .collect(),
            stack: None,
            loading: false,
        }
    }

    /// A document over the files.
    #[must_use]
    pub fn document(&self, config: DocumentConfig) -> ReviewDocument {
        ReviewDocument::new(PathBuf::from("/repo"), vec![self.input(SOURCE)], config)
    }

    /// Key of the file at `path`.
    ///
    /// # Panics
    ///
    /// Panics when the synthetic data is inconsistent (a test bug).
    #[must_use]
    pub fn file(&self, path: &str) -> FileKey {
        assert!(
            self.files.contains_key(path),
            "unknown synthetic file {path}"
        );
        FileKey::new(SOURCE, None, path)
    }

    fn blob(&self, oid: &str) -> Option<&Vec<String>> {
        let (side, path) = oid.split_once(':')?;
        self.files.values().find_map(|file| match side {
            "old" if file.old_oid() == oid => file.old.as_ref(),
            "new" if file.path == path => file.new.as_ref(),
            _ => None,
        })
    }

    /// Answer one need. Returns whether the document applied it (an earlier answer in the same
    /// batch may have evicted the file and made the need stale).
    ///
    /// # Panics
    ///
    /// Panics when the synthetic data is inconsistent (a test bug).
    pub fn answer(&self, doc: &mut ReviewDocument, need: &DocumentNeed) -> bool {
        match need {
            DocumentNeed::Hunks {
                ticket, request, ..
            } => {
                let file = &self.files[&request.path];
                doc.accept_hunks(*ticket, &file.hunks_response(&request.source.source_id))
            }
            DocumentNeed::BlobLines {
                ticket, request, ..
            } => {
                let ReviewBlobSelector::Object { oid } = &request.blob else {
                    panic!("synthetic blobs are read by object id");
                };
                let binary = self
                    .files
                    .values()
                    .any(|file| file.binary && (file.old_oid() == *oid || file.new_oid() == *oid));
                let lines = self.blob(oid).expect("known blob");
                let first = (request.start_line.max(1) - 1) as usize;
                let end = (first + request.count as usize).min(lines.len());
                let response = ReviewBlobLinesResponse {
                    start_line: request.start_line.max(1),
                    lines: if binary || first >= lines.len() {
                        Vec::new()
                    } else {
                        lines[first..end].to_vec()
                    },
                    total_lines: u32::try_from(lines.len()).expect("fits"),
                    binary,
                };
                doc.accept_blob_lines(*ticket, &response)
            }
            DocumentNeed::Attribution {
                ticket, request, ..
            } => {
                let response = self
                    .attribution
                    .get(&request.path)
                    .cloned()
                    .unwrap_or_else(|| ReviewStackAttributionResponse {
                        path: request.path.clone(),
                        new_ranges: Vec::new(),
                        old_ranges: Vec::new(),
                        binary: false,
                    });
                doc.accept_attribution(*ticket, response)
            }
        }
    }

    /// Answer needs until the document asks for nothing more.
    ///
    /// # Panics
    ///
    /// Panics when the synthetic data is inconsistent (a test bug).
    pub fn settle(&self, doc: &mut ReviewDocument) {
        for _ in 0..10_000 {
            let needs = doc.take_needs();
            if needs.is_empty() {
                return;
            }
            for need in &needs {
                let _ = self.answer(doc, need);
            }
        }
        panic!("document never settled");
    }

    /// Load everything: a viewport over the whole document, settled.
    pub fn load_all(&self, doc: &mut ReviewDocument) {
        for _ in 0..1_000 {
            let len = doc.len();
            doc.set_viewport(0, len.saturating_sub(1));
            self.settle(doc);
            if doc.len() == len {
                return;
            }
        }
    }
}

/// Every item, in order.
///
/// # Panics
///
/// Panics when the synthetic data is inconsistent (a test bug).
#[must_use]
pub fn items(doc: &ReviewDocument) -> Vec<ReviewItem> {
    (0..doc.len())
        .map(|index| doc.item(index).expect("item").clone())
        .collect()
}

/// Every key, in order.
pub fn keys(doc: &ReviewDocument) -> Vec<ItemKey> {
    items(doc).iter().map(ReviewItem::key).collect()
}

/// A shadow copy of the item sequence maintained only through splices.
pub struct Shadow(pub Vec<Option<ReviewItem>>);

impl Shadow {
    pub fn new(doc: &ReviewDocument) -> Self {
        Self(items(doc).into_iter().map(Some).collect())
    }

    /// Apply splices (unknown inserted items become holes) then fill holes from the document,
    /// and assert the result equals a fresh enumeration. Items outside the splices must be
    /// unchanged, which the comparison proves.
    ///
    /// # Panics
    ///
    /// Panics when the synthetic data is inconsistent (a test bug).
    pub fn apply_and_check(&mut self, doc: &mut ReviewDocument, context: &str) {
        let splices: Vec<Splice> = doc.take_splices();
        let mut revision = None;
        for splice in &splices {
            assert!(
                splice.start + splice.removed <= self.0.len(),
                "{context}: splice {splice:?} out of range {}",
                self.0.len()
            );
            if let Some(previous) = revision {
                assert!(splice.revision > previous, "{context}: revisions increase");
            }
            revision = Some(splice.revision);
            self.0.splice(
                splice.start..splice.start + splice.removed,
                std::iter::repeat_n(None, splice.inserted),
            );
        }
        assert_eq!(self.0.len(), doc.len(), "{context}: length after splices");
        for (index, slot) in self.0.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = doc.item(index).cloned();
            }
        }
        let fresh = items(doc);
        for (index, (shadow, fresh)) in self.0.iter().zip(&fresh).enumerate() {
            assert_eq!(
                shadow.as_ref(),
                Some(fresh),
                "{context}: item {index} differs from a fresh enumeration"
            );
        }
        if let Some(last) = revision {
            assert_eq!(
                last,
                doc.revision(),
                "{context}: last splice carries the revision"
            );
        }
        assert_consistent(doc, context);
    }
}

/// Assert the document's derived state agrees with a from-scratch derivation (see
/// [`ReviewDocument::assert_consistent`]).
pub fn assert_consistent(doc: &ReviewDocument, context: &str) {
    doc.assert_consistent(context);
}
