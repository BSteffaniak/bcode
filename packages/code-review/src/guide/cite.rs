//! Citation checks: a requested citation against the revisions a guide is pinned to.
//!
//! [`check_citation`] resolves the source, revision, path, and range of a [`CitationRequest`],
//! verifies its quote (moving the range when the quote is found exactly once elsewhere), relates
//! the range to the review's hunks, and records what identifies the cited text. A citation is
//! never dropped: what does not check out is kept with a problem that says how to fix it.
//!
//! The checks read through [`CitationReader`], so they are deterministic and testable without a
//! repository; [`ReviewDataReader`] reads a repository through [`ReviewData`].

use super::text::{
    excerpt, find_runs, interval_at, line_similarity, quote_hash, same_exact, same_loose, slice,
};
use crate::data::search::fuzzy_score;
use crate::data::{DataError, ReviewData};
use bcode_code_review_models::{
    Citation, CitationCheck, CitationCheckStatus, CitationProblem, CitationRelation,
    CitationRelationHint, CitationRequest, GUIDE_CITE_ADD_QUOTE, GUIDE_CITE_ADJUSTED,
    GUIDE_CITE_AMBIGUOUS_SOURCE, GUIDE_CITE_DELETED_ON_SIDE, GUIDE_CITE_INVALID_RANGE,
    GUIDE_CITE_NO_SOURCES, GUIDE_CITE_NOT_IN_DIFF, GUIDE_CITE_PATH_NOT_FOUND,
    GUIDE_CITE_QUOTE_AMBIGUOUS, GUIDE_CITE_QUOTE_MISMATCH, GUIDE_CITE_QUOTE_NOT_FOUND,
    GUIDE_CITE_RANGE_OUT_OF_BOUNDS, GUIDE_CITE_RENAMED, GUIDE_CITE_UNKNOWN_LAYER,
    GUIDE_CITE_UNREADABLE, REVIEW_BLOB_LINES_MAX_COUNT, ResolvedSource, ResolvedSourceMode,
    ReviewBlobLinesRequest, ReviewBlobSelector, ReviewFileHunksRequest, ReviewFileStatus,
    ReviewFilesSearchRequest, ReviewHunk, ReviewIndexRequest, ReviewLineInterval, ReviewLineKind,
    ReviewLineSide, RevisionRef, Verification,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// Similarity from which a near quote is offered as the best candidate.
pub const QUOTE_SIMILARITY_THRESHOLD: f64 = 0.8;
/// Most candidate ranges a problem lists.
const MAX_CANDIDATES: usize = 10;
/// Most quote lines compared; a longer quote is compared by its first lines.
const MAX_QUOTE_LINES: usize = 200;
/// Most line comparisons a close-match search makes; past it, only lines near the cited range
/// are searched.
const MAX_SIMILARITY_WORK: usize = 400_000;
/// Lines on each side of the cited range a bounded close-match search covers.
const SIMILARITY_NEIGHBORHOOD: usize = 2_000;
/// Most lines of a file read for a check.
pub const MAX_CITE_FILE_LINES: u32 = 200_000;
/// Most files of one source's diff considered.
const MAX_DIFF_FILES: usize = 20_000;

/// One source a guide is pinned to, as citations name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiteSource {
    /// Source id.
    pub source_id: String,
    /// Display label (`#102`, a branch name).
    pub label: String,
    /// Pull request number, for a pull request layer.
    pub number: Option<u64>,
    /// Pinned base.
    pub base: RevisionRef,
    /// Pinned head.
    pub head: RevisionRef,
    /// Whether the source shows whole files without a diff.
    pub browse: bool,
}

impl CiteSource {
    /// The revision of `side`.
    #[must_use]
    pub const fn revision(&self, side: ReviewLineSide) -> &RevisionRef {
        match side {
            ReviewLineSide::New => &self.head,
            ReviewLineSide::Old => &self.base,
        }
    }

    fn named(&self, name: &str) -> bool {
        let name = name.trim();
        name == self.source_id
            || (!self.label.is_empty() && name == self.label)
            || self.number.is_some_and(|number| {
                name.trim_start_matches('#').parse::<u64>().ok() == Some(number)
            })
    }
}

/// One file of a source's diff (or listing, for a browse source).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFile {
    /// New path (old path for deleted files).
    pub path: String,
    /// Old path, for renames and copies.
    pub old_path: Option<String>,
    /// Status.
    pub status: ReviewFileStatus,
    /// Old-side blob id, when an object.
    pub old_blob: Option<String>,
    /// New-side blob id, when an object.
    pub new_blob: Option<String>,
}

/// A definition site of a symbol, offered when a quote is not found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolSite {
    /// Path.
    pub path: String,
    /// Line (1-based).
    pub line: u32,
    /// The line's text.
    pub text: String,
}

/// What citation checks read.
pub trait CitationReader {
    /// The pinned sources, primary first.
    fn sources(&self) -> &[CiteSource];

    /// The files of a source's diff (or listing).
    ///
    /// # Errors
    ///
    /// * A message when the listing cannot be read.
    fn diff_files(&self, source_id: &str) -> Result<Vec<DiffFile>, String>;

    /// The hunks of one diff file of a source.
    ///
    /// # Errors
    ///
    /// * A message when the hunks cannot be read.
    fn hunks(&self, source_id: &str, file: &DiffFile) -> Result<Vec<ReviewHunk>, String>;

    /// The lines of `path` at `revision`; `None` when it does not exist there.
    ///
    /// # Errors
    ///
    /// * A message when it exists but cannot be read (binary, too large, a read error).
    fn file(&self, revision: &RevisionRef, path: &str) -> Result<Option<Vec<String>>, String>;

    /// Paths at `revision` resembling `path`, best first.
    fn path_suggestions(&self, _revision: &RevisionRef, _path: &str) -> Vec<String> {
        Vec::new()
    }

    /// Where `symbol` is defined at `revision`.
    fn symbol_sites(&self, _revision: &RevisionRef, _symbol: &str) -> Vec<SymbolSite> {
        Vec::new()
    }
}

/// Check every request in order (see [`check_citation`]).
#[must_use]
pub fn check_citations(
    reader: &dyn CitationReader,
    requests: &[CitationRequest],
    now_ms: u64,
) -> Vec<CitationCheck> {
    requests
        .iter()
        .map(|request| check_citation(reader, request, now_ms))
        .collect()
}

/// Check one requested citation. The returned citation has an empty id for the caller to
/// assign; it is always returned, with its problems, never dropped.
#[must_use]
pub fn check_citation(
    reader: &dyn CitationReader,
    request: &CitationRequest,
    now_ms: u64,
) -> CitationCheck {
    let mut check = Check::new(request);
    check.run(reader, now_ms);
    check.finish()
}

/// The state of one check as it proceeds.
struct Check<'a> {
    request: &'a CitationRequest,
    citation: Citation,
    problems: Vec<CitationProblem>,
    warnings: Vec<CitationProblem>,
    adjusted: bool,
}

/// What resolving a citation's source found.
struct Resolved<'s> {
    source: Option<&'s CiteSource>,
    entry: Option<DiffFile>,
}

impl<'a> Check<'a> {
    fn new(request: &'a CitationRequest) -> Self {
        Self {
            request,
            citation: Citation {
                id: String::new(),
                source_id: None,
                path: request.path.clone(),
                side: request.side,
                lines: ReviewLineInterval {
                    start: request.start_line,
                    end: request.end_line,
                },
                revision: RevisionRef::WorkingTree,
                blob_oid: None,
                quote_hash: String::new(),
                excerpt: String::new(),
                relation: CitationRelation::OutsideReview,
                verification: Verification::Unverified,
            },
            problems: Vec::new(),
            warnings: Vec::new(),
            adjusted: false,
        }
    }

    fn problem(&mut self, code: &str, message: impl Into<String>, suggestion: serde_json::Value) {
        self.problems.push(CitationProblem {
            code: code.to_owned(),
            message: message.into(),
            suggestion,
        });
    }

    fn warn(&mut self, code: &str, message: impl Into<String>, suggestion: serde_json::Value) {
        self.warnings.push(CitationProblem {
            code: code.to_owned(),
            message: message.into(),
            suggestion,
        });
    }

    fn run(&mut self, reader: &dyn CitationReader, now_ms: u64) {
        let Some(resolved) = self.resolve_source(reader) else {
            return;
        };
        let side = self.request.side;
        let Some(source) = resolved.source else {
            return;
        };
        self.citation.revision = source.revision(side).clone();
        let path = self.read_path(resolved.entry.as_ref());
        if !self.side_exists(resolved.entry.as_ref()) {
            return;
        }
        let lines = match reader.file(&self.citation.revision, &path) {
            Ok(Some(lines)) => lines,
            Ok(None) => {
                let suggestions = reader.path_suggestions(&self.citation.revision, &path);
                self.problem(
                    GUIDE_CITE_PATH_NOT_FOUND,
                    format!(
                        "{path} does not exist at the cited revision; cite a path that exists \
                         (see suggestions)"
                    ),
                    json!({ "suggestions": suggestions }),
                );
                return;
            }
            Err(message) => {
                self.problem(
                    GUIDE_CITE_UNREADABLE,
                    format!("{path} cannot be read at the cited revision: {message}"),
                    serde_json::Value::Null,
                );
                return;
            }
        };
        self.citation.path = path;
        self.citation.blob_oid = resolved.entry.as_ref().and_then(|entry| match side {
            ReviewLineSide::New => entry.new_blob.clone(),
            ReviewLineSide::Old => entry.old_blob.clone(),
        });
        if !self.range_fits(lines.len()) {
            return;
        }
        self.match_quote(reader, &lines, now_ms);
        let range = slice(&lines, self.citation.lines).unwrap_or_default();
        self.citation.quote_hash = quote_hash(range);
        self.citation.excerpt = excerpt(range);
        self.relate(reader, source, resolved.entry.as_ref());
    }

    /// Step 1: which source the citation reads; `None` after a problem.
    fn resolve_source<'s>(&mut self, reader: &'s dyn CitationReader) -> Option<Resolved<'s>> {
        let sources = reader.sources();
        let Some(primary) = sources.first() else {
            self.problem(
                GUIDE_CITE_NO_SOURCES,
                "the review has no sources to cite",
                serde_json::Value::Null,
            );
            return None;
        };
        let path = self.request.path.clone();
        let named = self
            .request
            .layer
            .as_deref()
            .or(self.request.source.as_deref());
        if let Some(name) = named {
            let Some(source) = sources.iter().find(|source| source.named(name)) else {
                let layers: Vec<String> = sources.iter().map(label_of).collect();
                self.citation.revision = primary.revision(self.request.side).clone();
                self.problem(
                    GUIDE_CITE_UNKNOWN_LAYER,
                    format!("the review has no source or layer {name}; name one of the layers"),
                    json!({ "layers": layers }),
                );
                return None;
            };
            self.citation.source_id = Some(source.source_id.clone());
            let entry = self.entry_in(reader, source, &path);
            return Some(Resolved {
                source: Some(source),
                entry,
            });
        }
        let mut hits: Vec<(&CiteSource, DiffFile)> = Vec::new();
        for source in sources {
            if let Some(entry) = self.entry_in(reader, source, &path) {
                hits.push((source, entry));
            }
        }
        if hits.len() > 1 {
            let candidates: Vec<String> = hits.iter().map(|(source, _)| label_of(source)).collect();
            self.citation.revision = primary.revision(self.request.side).clone();
            self.problem(
                GUIDE_CITE_AMBIGUOUS_SOURCE,
                format!(
                    "{path} is in several sources of the review; name the layer or source the \
                     citation reads"
                ),
                json!({ "candidates": candidates }),
            );
            return None;
        }
        if let Some((source, entry)) = hits.pop() {
            self.citation.source_id = Some(source.source_id.clone());
            return Some(Resolved {
                source: Some(source),
                entry: Some(entry),
            });
        }
        // Repository context outside every source, at the primary source's revisions.
        Some(Resolved {
            source: Some(primary),
            entry: None,
        })
    }

    fn entry_in(
        &mut self,
        reader: &dyn CitationReader,
        source: &CiteSource,
        path: &str,
    ) -> Option<DiffFile> {
        match reader.diff_files(&source.source_id) {
            Ok(files) => files
                .into_iter()
                .find(|file| file.path == path || file.old_path.as_deref() == Some(path)),
            Err(message) => {
                self.warn(
                    GUIDE_CITE_UNREADABLE,
                    format!(
                        "the files of {} could not be listed: {message}",
                        label_of(source)
                    ),
                    serde_json::Value::Null,
                );
                None
            }
        }
    }

    /// The path to read on the cited side: a renamed file's old side has its old path.
    fn read_path(&self, entry: Option<&DiffFile>) -> String {
        match (entry, self.request.side) {
            (Some(entry), ReviewLineSide::Old) => {
                entry.old_path.clone().unwrap_or_else(|| entry.path.clone())
            }
            _ => self.request.path.clone(),
        }
    }

    /// Step 3, from the diff: renames and files absent on the cited side.
    fn side_exists(&mut self, entry: Option<&DiffFile>) -> bool {
        let Some(entry) = entry else {
            return true;
        };
        let side = self.request.side;
        if side == ReviewLineSide::New
            && entry.path != self.request.path
            && entry.old_path.as_deref() == Some(self.request.path.as_str())
        {
            self.problem(
                GUIDE_CITE_RENAMED,
                format!(
                    "the change renames {} to {}; cite the new path on the new side",
                    self.request.path, entry.path
                ),
                json!({ "to": entry.path }),
            );
            return false;
        }
        let absent = match side {
            ReviewLineSide::New => entry.status == ReviewFileStatus::Deleted,
            ReviewLineSide::Old => entry.status == ReviewFileStatus::Added,
        };
        if absent {
            let (missing, other) = match side {
                ReviewLineSide::New => ("deleted", "old"),
                ReviewLineSide::Old => ("added", "new"),
            };
            self.problem(
                GUIDE_CITE_DELETED_ON_SIDE,
                format!(
                    "the change {missing} {}, so it has no {} side; cite the {other} side",
                    entry.path,
                    side_name(side)
                ),
                json!({ "use_side": other }),
            );
            return false;
        }
        true
    }

    /// Step 4: the range is non-empty and inside the file.
    fn range_fits(&mut self, file_lines: usize) -> bool {
        let lines = self.citation.lines;
        if lines.start == 0 || lines.end < lines.start {
            self.problem(
                GUIDE_CITE_INVALID_RANGE,
                format!(
                    "lines {}-{} are not a range; give 1-based start_line <= end_line",
                    lines.start, lines.end
                ),
                serde_json::Value::Null,
            );
            return false;
        }
        if usize::try_from(lines.end).map_or(true, |end| end > file_lines) {
            self.problem(
                GUIDE_CITE_RANGE_OUT_OF_BOUNDS,
                format!(
                    "{} has {file_lines} lines; lines {}-{} run past its end",
                    self.citation.path, lines.start, lines.end
                ),
                json!({ "file_lines": file_lines }),
            );
            return false;
        }
        true
    }

    /// Step 5: verify the quote, moving the range when the quote is found exactly once
    /// elsewhere.
    fn match_quote(&mut self, reader: &dyn CitationReader, lines: &[String], now_ms: u64) {
        let Some(quote) = self.request.quote.as_deref().map(quote_lines) else {
            self.no_quote();
            return;
        };
        if quote.is_empty() {
            self.no_quote();
            return;
        }
        let quote = &quote[..quote.len().min(MAX_QUOTE_LINES)];
        let requested = self.citation.lines;
        let range = slice(lines, requested).unwrap_or_default();
        if !find_runs(range, quote, same_exact, 1).is_empty()
            || !find_runs(range, quote, same_loose, 1).is_empty()
        {
            self.citation.verification = Verification::Verified {
                at_ms: now_ms,
                adjusted_from: None,
            };
            return;
        }
        let mut hits = find_runs(lines, quote, same_exact, MAX_CANDIDATES + 1);
        if hits.is_empty() {
            hits = find_runs(lines, quote, same_loose, MAX_CANDIDATES + 1);
        }
        match hits.as_slice() {
            [only] => {
                self.citation.lines = interval_at(*only, quote.len());
                self.citation.verification = Verification::Verified {
                    at_ms: now_ms,
                    adjusted_from: Some(requested),
                };
                self.adjusted = true;
                self.warn(
                    GUIDE_CITE_ADJUSTED,
                    format!(
                        "the quote is at lines {}-{}, not {}-{}; the citation was moved there",
                        self.citation.lines.start,
                        self.citation.lines.end,
                        requested.start,
                        requested.end
                    ),
                    json!({ "from": requested, "to": self.citation.lines }),
                );
            }
            [] => self.near_quote(reader, lines, quote),
            many => {
                let candidates: Vec<ReviewLineInterval> = many
                    .iter()
                    .take(MAX_CANDIDATES)
                    .map(|start| interval_at(*start, quote.len()))
                    .collect();
                self.problem(
                    GUIDE_CITE_QUOTE_AMBIGUOUS,
                    format!(
                        "the quote is not at lines {}-{} and occurs {} times elsewhere in {}; \
                         cite one of the candidate ranges",
                        requested.start,
                        requested.end,
                        many.len(),
                        self.citation.path
                    ),
                    json!({ "candidates": candidates }),
                );
            }
        }
    }

    fn no_quote(&mut self) {
        self.warn(
            GUIDE_CITE_ADD_QUOTE,
            "the citation has no quote, so it is kept unverified; add the cited text as quote \
             to verify it",
            serde_json::Value::Null,
        );
    }

    /// The quote is nowhere exactly: offer the closest range, or the symbol's definitions.
    fn near_quote(&mut self, reader: &dyn CitationReader, lines: &[String], quote: &[String]) {
        if let Some((start, similarity)) = closest(lines, quote, self.citation.lines) {
            let best = interval_at(start, quote.len());
            let text = excerpt(slice(lines, best).unwrap_or_default());
            let similarity = (similarity * 100.0).round() / 100.0;
            self.problem(
                GUIDE_CITE_QUOTE_MISMATCH,
                format!(
                    "the quote does not match {}; the closest text is at lines {}-{} \
                     ({similarity} similar): quote it exactly or cite that range",
                    self.citation.path, best.start, best.end
                ),
                json!({ "best": { "range": best, "similarity": similarity, "text": text } }),
            );
            return;
        }
        let sites: Vec<serde_json::Value> = self
            .request
            .symbol
            .as_deref()
            .map(|symbol| reader.symbol_sites(&self.citation.revision, symbol))
            .unwrap_or_default()
            .into_iter()
            .take(MAX_CANDIDATES)
            .map(|site| json!({ "path": site.path, "line": site.line, "text": site.text }))
            .collect();
        let hint = if sites.is_empty() {
            "check the path and revision, and quote text that is in the file"
        } else {
            "the symbol is defined at the suggested sites; cite one of them"
        };
        self.problem(
            GUIDE_CITE_QUOTE_NOT_FOUND,
            format!("the quote is not in {}; {hint}", self.citation.path),
            if sites.is_empty() {
                serde_json::Value::Null
            } else {
                json!({ "symbol_sites": sites })
            },
        );
    }

    /// Step 6: how the range relates to the review.
    fn relate(
        &mut self,
        reader: &dyn CitationReader,
        source: &CiteSource,
        entry: Option<&DiffFile>,
    ) {
        let Some(entry) = entry else {
            self.citation.relation = CitationRelation::OutsideReview;
            self.hint_not_in_diff();
            return;
        };
        if source.browse {
            self.citation.relation = CitationRelation::InReview;
            self.hint_not_in_diff();
            return;
        }
        let hunks = match reader.hunks(&source.source_id, entry) {
            Ok(hunks) => hunks,
            Err(message) => {
                self.warn(
                    GUIDE_CITE_UNREADABLE,
                    format!("the hunks of {} could not be read: {message}", entry.path),
                    serde_json::Value::Null,
                );
                Vec::new()
            }
        };
        self.citation.relation = relation(&hunks, self.request.side, self.citation.lines);
        self.hint_not_in_diff();
    }

    fn hint_not_in_diff(&mut self) {
        if self.request.relation_hint == Some(CitationRelationHint::Diff)
            && self.citation.relation != CitationRelation::Changed
        {
            let lines = self.citation.lines;
            self.warn(
                GUIDE_CITE_NOT_IN_DIFF,
                format!(
                    "no changed line of the review is in {} lines {}-{}; it is kept and shown as \
                     unchanged code",
                    self.citation.path, lines.start, lines.end
                ),
                json!({ "relation": self.citation.relation }),
            );
        }
    }

    fn finish(mut self) -> CitationCheck {
        let status = if let Some(first) = self.problems.first() {
            self.citation.verification = Verification::Problem {
                code: first.code.clone(),
                message: first.message.clone(),
            };
            CitationCheckStatus::Problem
        } else if self.adjusted {
            CitationCheckStatus::Adjusted
        } else if self.citation.verification.is_verified() {
            CitationCheckStatus::Verified
        } else {
            CitationCheckStatus::Unverified
        };
        CitationCheck {
            citation: self.citation,
            status,
            problems: self.problems,
            warnings: self.warnings,
        }
    }
}

fn label_of(source: &CiteSource) -> String {
    if source.label.is_empty() {
        source.source_id.clone()
    } else {
        source.label.clone()
    }
}

const fn side_name(side: ReviewLineSide) -> &'static str {
    match side {
        ReviewLineSide::New => "new",
        ReviewLineSide::Old => "old",
    }
}

/// A quote's lines, normalized, without leading and trailing blank lines.
fn quote_lines(quote: &str) -> Vec<String> {
    let lines: Vec<String> = quote
        .lines()
        .map(|line| line.trim_end().to_owned())
        .collect();
    let first = lines.iter().position(|line| !line.trim().is_empty());
    let last = lines.iter().rposition(|line| !line.trim().is_empty());
    match (first, last) {
        (Some(first), Some(last)) => lines[first..=last].to_vec(),
        _ => Vec::new(),
    }
}

/// The start and similarity of the window of `lines` most like `quote`, when at least
/// [`QUOTE_SIMILARITY_THRESHOLD`] similar. Large files are searched near `near` only.
fn closest(lines: &[String], quote: &[String], near: ReviewLineInterval) -> Option<(usize, f64)> {
    if quote.len() > lines.len() {
        return None;
    }
    let windows = lines.len() - quote.len() + 1;
    let (from, to) = if windows.saturating_mul(quote.len()) <= MAX_SIMILARITY_WORK {
        (0, windows)
    } else {
        let center = usize::try_from(near.start).unwrap_or(0).saturating_sub(1);
        (
            center.saturating_sub(SIMILARITY_NEIGHBORHOOD).min(windows),
            center.saturating_add(SIMILARITY_NEIGHBORHOOD).min(windows),
        )
    };
    let mut best: Option<(usize, f64)> = None;
    for start in from..to {
        let total: f64 = quote
            .iter()
            .zip(&lines[start..])
            .map(|(want, have)| line_similarity(have, want))
            .sum();
        #[allow(clippy::cast_precision_loss)]
        let score = total / quote.len() as f64;
        if best.is_none_or(|(_, top)| score > top) {
            best = Some((start, score));
        }
    }
    best.filter(|(_, score)| *score >= QUOTE_SIMILARITY_THRESHOLD)
}

/// How `lines` on `side` relate to `hunks`: changed when a changed line is in them, hunk context
/// when they meet a hunk, unchanged otherwise.
#[must_use]
pub fn relation(
    hunks: &[ReviewHunk],
    side: ReviewLineSide,
    lines: ReviewLineInterval,
) -> CitationRelation {
    let within = |line: u32| (lines.start..=lines.end).contains(&line);
    let changed = hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .any(|line| match side {
            ReviewLineSide::New => {
                line.kind == ReviewLineKind::Added && line.new_line.is_some_and(within)
            }
            ReviewLineSide::Old => {
                line.kind == ReviewLineKind::Removed && line.old_line.is_some_and(within)
            }
        });
    if changed {
        return CitationRelation::Changed;
    }
    let meets = hunks.iter().any(|hunk| {
        let (start, count) = match side {
            ReviewLineSide::New => (hunk.new_start, hunk.new_count),
            ReviewLineSide::Old => (hunk.old_start, hunk.old_count),
        };
        count > 0 && start <= lines.end && lines.start < start.saturating_add(count)
    });
    if meets {
        CitationRelation::HunkContext
    } else {
        CitationRelation::Unchanged
    }
}

/// A file read: its lines, `None` when absent, or why it could not be read.
type FileRead = Result<Option<Vec<String>>, String>;

/// Citation reads through [`ReviewData`] at the revisions resolved sources pin.
///
/// Diff listings, hunks, and file lines are read once per reader and kept for its lifetime, so
/// checking a batch of citations reads each file once.
pub struct ReviewDataReader<'a> {
    data: &'a ReviewData,
    repo_path: PathBuf,
    sources: Vec<CiteSource>,
    resolved: BTreeMap<String, ResolvedSource>,
    files: Mutex<BTreeMap<String, Result<Vec<DiffFile>, String>>>,
    contents: Mutex<BTreeMap<(RevisionRef, String), FileRead>>,
}

impl<'a> ReviewDataReader<'a> {
    /// A reader of `sources` (primary first) in the repository at `repo_path`; `labels` names
    /// sources by id (`#102`), and `numbers` gives pull request layers their numbers.
    #[must_use]
    pub fn new(
        data: &'a ReviewData,
        repo_path: PathBuf,
        sources: &[ResolvedSource],
        labels: &BTreeMap<String, String>,
        numbers: &BTreeMap<String, u64>,
    ) -> Self {
        Self {
            data,
            repo_path,
            sources: sources
                .iter()
                .map(|source| CiteSource {
                    source_id: source.source_id.clone(),
                    label: labels.get(&source.source_id).cloned().unwrap_or_default(),
                    number: numbers.get(&source.source_id).copied(),
                    base: source.base.clone(),
                    head: source.head.clone(),
                    browse: source.mode == ResolvedSourceMode::Browse,
                })
                .collect(),
            resolved: sources
                .iter()
                .map(|source| (source.source_id.clone(), source.clone()))
                .collect(),
            files: Mutex::new(BTreeMap::new()),
            contents: Mutex::new(BTreeMap::new()),
        }
    }

    fn list(&self, source: &ResolvedSource) -> Result<Vec<DiffFile>, String> {
        let mut files = Vec::new();
        let mut cursor = None;
        loop {
            let page = self
                .data
                .index_get(&ReviewIndexRequest {
                    repo_path: self.repo_path.clone(),
                    source: source.clone(),
                    cursor,
                    limit: None,
                })
                .map_err(|error| error.to_string())?;
            files.extend(page.entries.into_iter().map(|entry| DiffFile {
                path: entry.path,
                old_path: entry.old_path,
                status: entry.status,
                old_blob: entry.old_blob.oid().map(str::to_owned),
                new_blob: entry.new_blob.oid().map(str::to_owned),
            }));
            cursor = page.next_cursor;
            if cursor.is_none() || files.len() >= MAX_DIFF_FILES {
                return Ok(files);
            }
        }
    }
}

impl CitationReader for ReviewDataReader<'_> {
    fn sources(&self) -> &[CiteSource] {
        &self.sources
    }

    fn diff_files(&self, source_id: &str) -> Result<Vec<DiffFile>, String> {
        if let Some(known) = lock(&self.files).get(source_id) {
            return known.clone();
        }
        let Some(source) = self.resolved.get(source_id) else {
            return Err(format!("unknown source {source_id}"));
        };
        let listed = self.list(source);
        lock(&self.files).insert(source_id.to_owned(), listed.clone());
        listed
    }

    fn hunks(&self, source_id: &str, file: &DiffFile) -> Result<Vec<ReviewHunk>, String> {
        let Some(source) = self.resolved.get(source_id) else {
            return Err(format!("unknown source {source_id}"));
        };
        self.data
            .file_hunks_get(&ReviewFileHunksRequest {
                repo_path: self.repo_path.clone(),
                source: source.clone(),
                path: file.path.clone(),
                old_path: file.old_path.clone(),
                context_lines: None,
                ignore_whitespace: false,
                max_lines: None,
            })
            .map(|response| response.hunks)
            .map_err(|error| error.to_string())
    }

    fn file(&self, revision: &RevisionRef, path: &str) -> Result<Option<Vec<String>>, String> {
        let key = (revision.clone(), path.to_owned());
        if let Some(known) = lock(&self.contents).get(&key) {
            return known.clone();
        }
        let read = read_file(self.data, &self.repo_path, revision, path);
        lock(&self.contents).insert(key, read.clone());
        read
    }

    fn path_suggestions(&self, revision: &RevisionRef, path: &str) -> Vec<String> {
        let name = path.rsplit('/').next().unwrap_or(path);
        let mut scored: Vec<(u64, String)> = Vec::new();
        for source in &self.sources {
            if let Ok(files) = self.diff_files(&source.source_id) {
                for file in files {
                    if let Some(score) = fuzzy_score(&file.path, name) {
                        scored.push((score, file.path));
                    }
                }
            }
        }
        if let Ok(found) = self.data.files_search(&ReviewFilesSearchRequest {
            repo_path: self.repo_path.clone(),
            query: name.to_owned(),
            revision: (!revision.is_live()).then(|| revision.clone()),
            cursor: None,
            limit: Some(5),
        }) {
            for (rank, entry) in found.files.into_iter().enumerate() {
                scored.push((1_000 + u64::try_from(rank).unwrap_or(0), entry.path));
            }
        }
        scored.sort();
        let mut suggestions: Vec<String> = Vec::new();
        for (_, path) in scored {
            if !suggestions.contains(&path) {
                suggestions.push(path);
            }
            if suggestions.len() >= 5 {
                break;
            }
        }
        suggestions
    }
}

/// The lines of `path` at `revision`, read in pages; `None` when it does not exist there.
///
/// # Errors
///
/// * A message for binary content, a file over [`MAX_CITE_FILE_LINES`], or a read error.
pub fn read_file(
    data: &ReviewData,
    repo_path: &std::path::Path,
    revision: &RevisionRef,
    path: &str,
) -> Result<Option<Vec<String>>, String> {
    let mut lines: Vec<String> = Vec::new();
    loop {
        let start_line = u32::try_from(lines.len() + 1).unwrap_or(u32::MAX);
        let page = match data.blob_lines_get(&ReviewBlobLinesRequest {
            repo_path: repo_path.to_path_buf(),
            blob: ReviewBlobSelector::Revision {
                revision: revision.clone(),
                path: path.to_owned(),
            },
            start_line,
            count: REVIEW_BLOB_LINES_MAX_COUNT,
        }) {
            Ok(page) => page,
            Err(DataError::NotFound(_)) => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        if page.binary {
            return Err("the file is binary".to_owned());
        }
        if page.total_lines > MAX_CITE_FILE_LINES {
            return Err(format!(
                "the file has {} lines; at most {MAX_CITE_FILE_LINES} are read",
                page.total_lines
            ));
        }
        let read = page.lines.len();
        lines.extend(page.lines);
        if read == 0 || lines.len() >= usize::try_from(page.total_lines).unwrap_or(usize::MAX) {
            return Ok(Some(lines));
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
