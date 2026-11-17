//! Code reads at a review's pinned revisions: search, the hunks around lines, related files,
//! and file content at a version the review knows.
//!
//! `filesystem.grep` reads the working tree; these read the revisions the review shows (a pull
//! request's head, a commit), so what an agent finds is what the reviewer sees. Every result is
//! bounded and says when it was cut, and each found line or file says how it relates to the
//! review: changed, hunk context, unchanged in a diff file, in a file the review shows without a
//! diff, or outside the review.

use super::{AssistantError, OpenReview, ReviewTools, Side, write_hunks};
use crate::data::grep::{self, GrepHit, GrepMode, GrepQuery};
use crate::guide::cite::relation;
use bcode_code_review_models::{
    CitationRelation, ListLayerVersionsRequest, ListReviewGuidesRequest, ResolvedSourceMode,
    ReviewBlobLinesRequest, ReviewBlobSelector, ReviewFileHunksRequest, ReviewHunk,
    ReviewIndexEntry, ReviewLineInterval, ReviewLineSide, ReviewSourceKind, RevisionRef,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// Matches a search returns at most.
pub const MAX_SEARCH_HITS: usize = 200;
/// Matches a search returns when the caller does not say.
pub const DEFAULT_SEARCH_HITS: usize = 100;
/// Files whose hunks a search reads to relate its matches to the diff.
const MAX_RELATED_HUNK_FILES: usize = 100;
/// Related files returned at most.
pub const MAX_RELATED: usize = 100;
/// Related files returned when the caller does not say.
pub const DEFAULT_RELATED: usize = 30;
/// Paths a related-files listing reads at most while looking for tests.
const MAX_TEST_CANDIDATES: usize = 500;
/// Diff lines `diff_hunks` returns at most.
const MAX_HUNK_LINES: u32 = 2_000;
/// Hex digits an abbreviated revision needs at least.
const MIN_ABBREVIATED_OID: usize = 7;

/// How a code search matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// The text as it is.
    #[default]
    Text,
    /// The text as a whole word.
    Word,
    /// A POSIX extended regular expression.
    Regex,
    /// Definitions of an identifier (Rust, Python, JavaScript and TypeScript, Go), then its
    /// other uses as a word.
    Symbol,
}

/// A code search.
#[derive(Debug, Clone, Default)]
pub struct CodeSearch {
    /// What to find.
    pub query: String,
    /// How it matches.
    pub mode: SearchMode,
    /// Path globs to search in; empty for the whole tree.
    pub paths: Vec<String>,
    /// The side read: the head (new) or the base (old).
    pub side: Side,
    /// The source whose revision is read (id, label, or `#102`); by default the newest head
    /// (the last source) for the new side and the first source's base for the old side.
    pub source: Option<String>,
    /// Matches returned at most.
    pub max: Option<usize>,
    /// Whether case is ignored.
    pub ignore_case: bool,
}

/// One match of a code search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchHitView {
    /// Path.
    pub path: String,
    /// Line (1-based).
    pub line: u32,
    /// The line's text.
    pub text: String,
    /// `definition` or `reference`, for a symbol search.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'static str>,
    /// Whether the file is one the review shows.
    pub in_review: bool,
    /// How the line relates to the review; absent when its file's hunks were not read (past
    /// the files a search relates).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation: Option<CitationRelation>,
}

/// What a code search found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchView {
    /// The revision searched (`abc1234 (#102 head)`, `working tree`).
    pub revision: String,
    /// The source whose revision was searched.
    pub source: String,
    /// The matches, definitions first for a symbol search.
    pub hits: Vec<SearchHitView>,
    /// Whether there were more matches than returned.
    pub truncated: bool,
    /// What to do about a cut or empty result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Kinds of related files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelatedKind {
    /// Tests of the file, by naming convention.
    Tests,
    /// Files that import or use its module.
    Callers,
    /// Configuration files that mention its module or package.
    Config,
    /// Markdown files that mention it.
    Docs,
}

impl RelatedKind {
    /// Every kind, in the order they are listed.
    pub const ALL: [Self; 4] = [Self::Tests, Self::Callers, Self::Config, Self::Docs];
}

/// One related file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelatedFile {
    /// Path.
    pub path: String,
    /// How it relates.
    pub kind: RelatedKind,
    /// Why it matched.
    pub why: String,
    /// Whether the file is one the review shows.
    pub in_review: bool,
}

/// Files related to one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelatedView {
    /// The file.
    pub path: String,
    /// The revision read.
    pub revision: String,
    /// The module or package name looked for.
    pub name: String,
    /// The related files, by kind.
    pub files: Vec<RelatedFile>,
    /// Whether more were found than returned.
    pub truncated: bool,
}

/// A review's files, by source index, with their path sets.
struct Listing {
    entries: Vec<(usize, ReviewIndexEntry)>,
    paths: BTreeSet<String>,
}

impl ReviewTools<'_> {
    fn listing(&self, review: &OpenReview) -> Result<Listing, AssistantError> {
        let entries = self.entries(review)?;
        let paths = entries
            .iter()
            .map(|(_, entry)| entry.path.clone())
            .collect();
        Ok(Listing { entries, paths })
    }

    /// The source index `name` names (id, label, or pull request number), or the default for
    /// `side`: the last source for the new side, the first for the old side.
    fn pick_source(
        review: &OpenReview,
        name: Option<&str>,
        side: Side,
    ) -> Result<usize, AssistantError> {
        if review.resolved.is_empty() {
            return Err(AssistantError::Invalid(
                "the review has no resolved sources".to_owned(),
            ));
        }
        let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) else {
            return Ok(match side {
                Side::New => review.resolved.len() - 1,
                Side::Old => 0,
            });
        };
        let number = name.trim_start_matches('#').parse::<u64>().ok();
        review
            .resolved
            .iter()
            .position(|resolved| {
                let source = review
                    .workspace
                    .sources
                    .iter()
                    .find(|source| source.id == resolved.source_id);
                resolved.source_id == name
                    || source.is_some_and(|source| {
                        source.label == name
                            || number
                                .is_some_and(|number| source_number(&source.kind) == Some(number))
                    })
            })
            .ok_or_else(|| {
                AssistantError::NotFound(format!(
                    "no source of this review is named {name}; sources: {}",
                    source_names(review).join(", ")
                ))
            })
    }

    fn hunks_of(
        &self,
        review: &OpenReview,
        index: usize,
        entry: &ReviewIndexEntry,
        context: Option<u32>,
    ) -> Result<Vec<ReviewHunk>, AssistantError> {
        Ok(self
            .data
            .file_hunks_get(&ReviewFileHunksRequest {
                repo_path: review.repo_root.clone(),
                source: review.resolved[index].clone(),
                path: entry.path.clone(),
                old_path: entry.old_path.clone(),
                context_lines: context,
                ignore_whitespace: false,
                max_lines: None,
            })?
            .hunks)
    }

    /// Search the code at one of the review's pinned revisions.
    ///
    /// # Errors
    ///
    /// * [`AssistantError::Invalid`] for an empty query, a symbol that is not an identifier, an
    ///   invalid regular expression, or a path pattern leaving the repository.
    /// * [`AssistantError::NotFound`] for an unknown source.
    /// * [`AssistantError::Data`] when the repository cannot be read.
    pub fn search_code(
        &self,
        review: &OpenReview,
        search: &CodeSearch,
    ) -> Result<SearchView, AssistantError> {
        let query = search.query.trim();
        if query.is_empty() {
            return Err(AssistantError::Invalid("`query` is required".to_owned()));
        }
        let index = Self::pick_source(review, search.source.as_deref(), search.side)?;
        let resolved = &review.resolved[index];
        let revision = match search.side {
            Side::New => resolved.head.clone(),
            Side::Old => resolved.base.clone(),
        };
        let max = search
            .max
            .unwrap_or(DEFAULT_SEARCH_HITS)
            .clamp(1, MAX_SEARCH_HITS);
        let base = GrepQuery {
            pattern: query,
            mode: GrepMode::Fixed,
            ignore_case: search.ignore_case,
            paths: &search.paths,
        };
        let run = |query: &GrepQuery<'_>, max: usize| {
            grep::grep(&review.repo_root, &revision, query, max).map_err(|error| match error {
                crate::DataError::Git(message) => {
                    AssistantError::Invalid(format!("the search failed: {message}"))
                }
                other => AssistantError::Data(other),
            })
        };
        let mut found: Vec<(GrepHit, Option<&'static str>)> = Vec::new();
        let truncated;
        match search.mode {
            SearchMode::Text | SearchMode::Word | SearchMode::Regex => {
                let mode = match search.mode {
                    SearchMode::Word => GrepMode::Word,
                    SearchMode::Regex => GrepMode::Regex,
                    _ => GrepMode::Fixed,
                };
                let (hits, more) = run(&GrepQuery { mode, ..base }, max)?;
                truncated = more;
                found.extend(hits.into_iter().map(|hit| (hit, None)));
            }
            SearchMode::Symbol => {
                if !grep::is_identifier(query) {
                    return Err(AssistantError::Invalid(format!(
                        "a symbol search takes one identifier (letters, digits, _), not {query:?}; use mode text or regex"
                    )));
                }
                let pattern = grep::definition_pattern(query);
                let (definitions, more) = run(
                    &GrepQuery {
                        pattern: &pattern,
                        mode: GrepMode::Regex,
                        ..base
                    },
                    max,
                )?;
                let mut cut = more;
                let seen: BTreeSet<(String, u32)> = definitions
                    .iter()
                    .map(|hit| (hit.path.clone(), hit.line))
                    .collect();
                let room = max - definitions.len();
                found.extend(definitions.into_iter().map(|hit| (hit, Some("definition"))));
                if room > 0 {
                    let (references, more) = run(
                        &GrepQuery {
                            mode: GrepMode::Word,
                            ..base
                        },
                        room + seen.len(),
                    )?;
                    let references: Vec<GrepHit> = references
                        .into_iter()
                        .filter(|hit| !seen.contains(&(hit.path.clone(), hit.line)))
                        .collect();
                    cut |= more || references.len() > room;
                    found.extend(
                        references
                            .into_iter()
                            .take(room)
                            .map(|hit| (hit, Some("reference"))),
                    );
                }
                truncated = cut;
            }
        }
        let listing = self.listing(review)?;
        let diff_files: BTreeMap<&str, &ReviewIndexEntry> = listing
            .entries
            .iter()
            .filter(|(owner, _)| *owner == index)
            .map(|(_, entry)| (entry.path.as_str(), entry))
            .collect();
        let line_side = match search.side {
            Side::New => ReviewLineSide::New,
            Side::Old => ReviewLineSide::Old,
        };
        let mut hunks: BTreeMap<String, Option<Vec<ReviewHunk>>> = BTreeMap::new();
        let mut hits = Vec::with_capacity(found.len());
        for (hit, kind) in found {
            let relation = if resolved.mode == ResolvedSourceMode::Browse {
                listing
                    .paths
                    .contains(&hit.path)
                    .then_some(CitationRelation::InReview)
                    .or(Some(CitationRelation::OutsideReview))
            } else if let Some(entry) = diff_files.get(hit.path.as_str()) {
                if !hunks.contains_key(&hit.path) && hunks.len() < MAX_RELATED_HUNK_FILES {
                    let read = self.hunks_of(review, index, entry, None).ok();
                    hunks.insert(hit.path.clone(), read);
                }
                hunks.get(&hit.path).and_then(|read| {
                    read.as_ref().map(|read| {
                        relation(
                            read,
                            line_side,
                            ReviewLineInterval {
                                start: hit.line,
                                end: hit.line,
                            },
                        )
                    })
                })
            } else if listing.paths.contains(&hit.path) {
                Some(CitationRelation::Unchanged)
            } else {
                Some(CitationRelation::OutsideReview)
            };
            hits.push(SearchHitView {
                in_review: listing.paths.contains(&hit.path),
                path: hit.path,
                line: hit.line,
                text: hit.text,
                kind,
                relation,
            });
        }
        let note = if truncated {
            Some(format!(
                "more than {max} matches; narrow the query or pass `paths`"
            ))
        } else if hits.is_empty() {
            Some("nothing matched at this revision".to_owned())
        } else {
            None
        };
        Ok(SearchView {
            revision: revision_label(&revision, &label_of(review, index), search.side),
            source: resolved.source_id.clone(),
            hits,
            truncated,
            note,
        })
    }

    /// The hunks of one file that overlap `lines` on `side` (every hunk without `lines`), as
    /// numbered lines.
    ///
    /// # Errors
    ///
    /// * [`AssistantError::NotFound`] when the file is not in the review.
    /// * [`AssistantError::Invalid`] for a source that shows files rather than a diff, or a
    ///   reversed range.
    /// * [`AssistantError::Data`] when the repository cannot be read.
    pub fn diff_hunks(
        &self,
        review: &OpenReview,
        path: &str,
        source: Option<&str>,
        side: Side,
        lines: Option<ReviewLineInterval>,
        context: Option<u32>,
    ) -> Result<String, AssistantError> {
        let (index, entry) = self.find_file(review, path, source)?;
        if review.resolved[index].mode == ResolvedSourceMode::Browse {
            return Err(AssistantError::Invalid(format!(
                "{} is browsed, not diffed; read it with code_review.file_content",
                entry.path
            )));
        }
        if let Some(lines) = lines
            && lines.start > lines.end
        {
            return Err(AssistantError::Invalid(
                "`end_line` must not be before `start_line`".to_owned(),
            ));
        }
        let hunks = self.hunks_of(review, index, &entry, Some(context.unwrap_or(3).min(20)))?;
        let overlapping: Vec<ReviewHunk> = hunks
            .into_iter()
            .filter(|hunk| {
                lines.is_none_or(|lines| {
                    let (start, count) = match side {
                        Side::New => (hunk.new_start, hunk.new_count),
                        Side::Old => (hunk.old_start, hunk.old_count),
                    };
                    let end = start.saturating_add(count.max(1)).saturating_sub(1);
                    start <= lines.end && lines.start <= end
                })
            })
            .collect();
        let mut out = format!("{} ({})\n", entry.path, label_of(review, index));
        if overlapping.is_empty() {
            let _ = writeln!(
                out,
                "no hunk overlaps {}; the lines are unchanged (read them with code_review.file_content)",
                lines.map_or_else(
                    || "the file".to_owned(),
                    |lines| format!("lines {}-{}", lines.start, lines.end)
                )
            );
            return Ok(out);
        }
        out.push_str("columns: old line, new line, marker (+ added, - removed), text\n");
        let mut shown = 0_u32;
        let mut kept = Vec::new();
        let total = overlapping.len();
        for hunk in overlapping {
            let size = u32::try_from(hunk.lines.len()).unwrap_or(u32::MAX);
            if shown > 0 && shown.saturating_add(size) > MAX_HUNK_LINES {
                break;
            }
            shown = shown.saturating_add(size);
            kept.push(hunk);
        }
        let kept_count = kept.len();
        write_hunks(&mut out, &kept);
        if kept_count < total {
            let _ = writeln!(
                out,
                "… {kept_count} of {total} hunks shown; ask for a narrower range"
            );
        }
        Ok(out)
    }

    /// Files related to `path` at the head of its source: tests by naming convention, files
    /// that import or use its module, configuration and docs that mention it.
    ///
    /// # Errors
    ///
    /// * [`AssistantError::Invalid`] for an empty path.
    /// * [`AssistantError::NotFound`] for an unknown source.
    /// * [`AssistantError::Data`] when the repository cannot be read.
    pub fn related_files(
        &self,
        review: &OpenReview,
        path: &str,
        source: Option<&str>,
        kinds: &[RelatedKind],
        max: Option<usize>,
    ) -> Result<RelatedView, AssistantError> {
        let path = path.trim().trim_start_matches("./");
        if path.is_empty() {
            return Err(AssistantError::Invalid("`path` is required".to_owned()));
        }
        let index = match (source, self.find_file(review, path, None)) {
            (Some(_), _) | (None, Err(_)) => Self::pick_source(review, source, Side::New)?,
            (None, Ok((index, _))) => index,
        };
        let revision = review.resolved[index].head.clone();
        let max = max.unwrap_or(DEFAULT_RELATED).clamp(1, MAX_RELATED);
        let kinds: Vec<RelatedKind> = if kinds.is_empty() {
            RelatedKind::ALL.to_vec()
        } else {
            let mut kinds = kinds.to_vec();
            kinds.sort();
            kinds.dedup();
            kinds
        };
        let name = module_name(path);
        let file_name = path.rsplit('/').next().unwrap_or(path).to_owned();
        let listing = self.listing(review)?;
        let mut files: Vec<RelatedFile> = Vec::new();
        let mut truncated = false;
        let mut cut = false;
        let mut push = |files: &mut Vec<RelatedFile>, found: String, kind, why: String| {
            if found == path || files.iter().any(|file| file.path == found) {
                return;
            }
            if files.len() >= max {
                truncated = true;
                return;
            }
            files.push(RelatedFile {
                in_review: listing.paths.contains(&found),
                path: found,
                kind,
                why,
            });
        };
        let repo = &review.repo_root;
        for kind in kinds {
            match kind {
                RelatedKind::Tests => {
                    let (candidates, _) =
                        grep::list_paths(repo, &revision, MAX_TEST_CANDIDATES, |candidate| {
                            candidate.contains(name.as_str())
                        })?;
                    for candidate in candidates {
                        if let Some(why) = test_reason(path, &name, &candidate) {
                            push(&mut files, candidate, RelatedKind::Tests, why);
                        }
                    }
                }
                RelatedKind::Callers => {
                    let escaped = grep::escape_regex(&name);
                    let pattern = format!(
                        "(^|[^A-Za-z0-9_])(use|mod|import|from|require|include)[^A-Za-z0-9_].*(^|[^A-Za-z0-9_])({escaped})([^A-Za-z0-9_]|$)|(^|[^A-Za-z0-9_]){escaped}::"
                    );
                    let (found, more) = grep::grep_files(
                        repo,
                        &revision,
                        &GrepQuery {
                            pattern: &pattern,
                            mode: GrepMode::Regex,
                            ignore_case: false,
                            paths: &[],
                        },
                        max + 1,
                    )?;
                    cut |= more;
                    for found in found {
                        push(
                            &mut files,
                            found,
                            RelatedKind::Callers,
                            format!("imports or uses `{name}`"),
                        );
                    }
                }
                RelatedKind::Config => {
                    let names = name_variants(&name);
                    let pattern = format!(
                        "(^|[^A-Za-z0-9_-])({})([^A-Za-z0-9_-]|$)",
                        names
                            .iter()
                            .map(|name| grep::escape_regex(name))
                            .collect::<Vec<_>>()
                            .join("|")
                    );
                    let globs: Vec<String> = ["**/*.toml", "**/*.yaml", "**/*.yml", "**/*.json"]
                        .map(str::to_owned)
                        .to_vec();
                    let (found, more) = grep::grep_files(
                        repo,
                        &revision,
                        &GrepQuery {
                            pattern: &pattern,
                            mode: GrepMode::Regex,
                            ignore_case: false,
                            paths: &globs,
                        },
                        max + 1,
                    )?;
                    cut |= more;
                    for found in found {
                        push(
                            &mut files,
                            found,
                            RelatedKind::Config,
                            format!("mentions `{}`", names.join("` or `")),
                        );
                    }
                }
                RelatedKind::Docs => {
                    let globs = vec!["**/*.md".to_owned()];
                    let (found, more) = grep::grep_files(
                        repo,
                        &revision,
                        &GrepQuery {
                            pattern: &file_name,
                            mode: GrepMode::Fixed,
                            ignore_case: false,
                            paths: &globs,
                        },
                        max + 1,
                    )?;
                    cut |= more;
                    for found in found {
                        push(
                            &mut files,
                            found,
                            RelatedKind::Docs,
                            format!("mentions {file_name}"),
                        );
                    }
                }
            }
        }
        Ok(RelatedView {
            truncated: truncated || cut,
            path: path.to_owned(),
            revision: revision_label(&revision, &label_of(review, index), Side::New),
            name,
            files,
        })
    }

    /// Lines of one file at `revision`: an abbreviated or full commit id the review knows (its
    /// sources' bases and heads, its stacks' layers, the versions of its pull requests the
    /// review has recorded, and the heads its guides are pinned to).
    ///
    /// # Errors
    ///
    /// * [`AssistantError::Invalid`] for a revision the review does not know, or an ambiguous
    ///   abbreviation.
    /// * [`AssistantError::NotFound`] when the file does not exist at the revision.
    /// * [`AssistantError::Store`] / [`AssistantError::Data`] when state or the repository
    ///   cannot be read.
    pub fn file_content_at(
        &self,
        review: &OpenReview,
        path: &str,
        revision: &str,
        start_line: Option<u32>,
        count: Option<u32>,
    ) -> Result<String, AssistantError> {
        let path = path.trim().trim_start_matches("./");
        let oid = self.known_revision(review, revision)?;
        let start = start_line.unwrap_or(1).max(1);
        let count = count
            .unwrap_or(super::DEFAULT_CONTENT_LINES)
            .clamp(1, super::MAX_CONTENT_LINES);
        let lines = self
            .data
            .blob_lines_get(&ReviewBlobLinesRequest {
                repo_path: review.repo_root.clone(),
                blob: ReviewBlobSelector::Revision {
                    revision: RevisionRef::commit(&oid),
                    path: path.to_owned(),
                },
                start_line: start,
                count,
            })
            .map_err(|error| match error {
                crate::DataError::NotFound(_) => {
                    AssistantError::NotFound(format!("{path} does not exist at {}", short(&oid)))
                }
                other => AssistantError::Data(other),
            })?;
        if lines.binary {
            return Ok(format!("{path} is binary at {}\n", short(&oid)));
        }
        let mut out = format!(
            "{path} at {}, lines {}-{} of {}\n",
            short(&oid),
            lines.start_line,
            lines.start_line
                + u32::try_from(lines.lines.len())
                    .unwrap_or(0)
                    .saturating_sub(1),
            lines.total_lines
        );
        for (offset, line) in lines.lines.iter().enumerate() {
            let number = lines.start_line + u32::try_from(offset).unwrap_or(u32::MAX);
            let _ = writeln!(out, "{number:>6} {line}");
        }
        Ok(out)
    }

    /// The full commit id `revision` abbreviates, among those the review knows.
    fn known_revision(
        &self,
        review: &OpenReview,
        revision: &str,
    ) -> Result<String, AssistantError> {
        let wanted = revision.trim().to_ascii_lowercase();
        if wanted.len() < MIN_ABBREVIATED_OID
            || !wanted.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(AssistantError::Invalid(format!(
                "`revision` is head, base, or a commit id (at least {MIN_ABBREVIATED_OID} hex digits) the review knows, not {revision:?}"
            )));
        }
        let known = self.known_revisions(review)?;
        let matching: Vec<&String> = known
            .iter()
            .filter(|oid| oid.starts_with(&wanted))
            .collect();
        match matching.as_slice() {
            [oid] => Ok((*oid).clone()),
            [] => Err(AssistantError::Invalid(format!(
                "{revision} is not a version this review knows; pass head, base, or one of: {}",
                known
                    .iter()
                    .take(20)
                    .map(|oid| short(oid))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
            _ => Err(AssistantError::Invalid(format!(
                "{revision} is ambiguous; pass more of the commit id"
            ))),
        }
    }

    /// Commit ids the review knows: sources, stacks, recorded pull request versions, and guide
    /// heads.
    fn known_revisions(&self, review: &OpenReview) -> Result<BTreeSet<String>, AssistantError> {
        let mut known = BTreeSet::new();
        for resolved in &review.resolved {
            known.extend(
                [&resolved.base, &resolved.head]
                    .into_iter()
                    .filter_map(RevisionRef::commit_oid)
                    .map(str::to_owned),
            );
        }
        for stack in &review.stacks {
            known.insert(stack.base_oid.clone());
            for layer in &stack.layers {
                known.insert(layer.base_oid.clone());
                known.insert(layer.head_oid.clone());
            }
        }
        let mut numbers: BTreeMap<(String, String), Vec<u64>> = BTreeMap::new();
        for source in &review.workspace.sources {
            match &source.kind {
                ReviewSourceKind::PullRequest {
                    provider,
                    repository,
                    number,
                }
                | ReviewSourceKind::StackLayer {
                    provider,
                    repository,
                    number,
                    ..
                } => numbers
                    .entry((provider.clone(), repository.clone()))
                    .or_default()
                    .push(*number),
                _ => {}
            }
        }
        for stack in &review.stacks {
            if let Some(source) = review
                .workspace
                .sources
                .iter()
                .find(|source| source.id == stack.source_id)
                && let ReviewSourceKind::Stack {
                    provider,
                    repository,
                    ..
                } = &source.kind
            {
                numbers
                    .entry((provider.clone(), repository.clone()))
                    .or_default()
                    .extend(stack.layers.iter().map(|layer| layer.number));
            }
        }
        for ((provider, repository), numbers) in numbers {
            let versions = self.store.list_layer_versions(&ListLayerVersionsRequest {
                repo_path: review.repo_root.clone(),
                provider,
                repository,
                numbers,
            })?;
            for version in versions.versions {
                known.insert(version.head_oid);
                known.extend(version.base_oid);
            }
        }
        let (target, scope) = review.scope();
        let guides = self.store.list_guides(&ListReviewGuidesRequest {
            repo_path: review.repo_root.clone(),
            target,
            scope: Some(scope),
            include_archived: true,
        })?;
        for guide in guides.guides {
            for summary in self
                .store
                .list_guide_versions(&bcode_code_review_models::ListGuideVersionsRequest {
                    repo_path: review.repo_root.clone(),
                    guide_id: guide.guide.guide_id.clone(),
                })?
                .versions
            {
                if let Ok(Some(record)) = self
                    .store
                    .guide_version(&bcode_code_review_models::GetGuideVersionRequest {
                        repo_path: review.repo_root.clone(),
                        guide_id: summary.guide_id.clone(),
                        version: Some(summary.version),
                    })
                    .map(|response| response.version)
                {
                    for head in record.version.heads.values() {
                        known.extend(
                            [&head.base, &head.head]
                                .into_iter()
                                .filter_map(RevisionRef::commit_oid)
                                .map(str::to_owned),
                        );
                    }
                }
            }
        }
        Ok(known)
    }
}

/// The pull request number of a source kind, for pull request and stack layer sources.
pub(crate) const fn source_number(kind: &ReviewSourceKind) -> Option<u64> {
    match kind {
        ReviewSourceKind::PullRequest { number, .. }
        | ReviewSourceKind::StackLayer { number, .. } => Some(*number),
        _ => None,
    }
}

/// Every resolved source's label (or id), for messages.
fn source_names(review: &OpenReview) -> Vec<String> {
    (0..review.resolved.len())
        .map(|index| label_of(review, index))
        .collect()
}

/// The label of resolved source `index` (its id when it has none).
pub(crate) fn label_of(review: &OpenReview, index: usize) -> String {
    let resolved = &review.resolved[index];
    review
        .workspace
        .sources
        .iter()
        .find(|source| source.id == resolved.source_id)
        .map(|source| source.label.clone())
        .filter(|label| !label.is_empty())
        .unwrap_or_else(|| resolved.source_id.clone())
}

/// A short commit id.
pub(crate) fn short(oid: &str) -> String {
    oid.chars().take(12).collect()
}

fn revision_label(revision: &RevisionRef, source: &str, side: Side) -> String {
    let side = match side {
        Side::New => "head",
        Side::Old => "base",
    };
    match revision {
        RevisionRef::Commit { oid } => format!("{} ({source} {side})", short(oid)),
        RevisionRef::EmptyTree => format!("empty tree ({source} {side})"),
        RevisionRef::Index => format!("index ({source} {side})"),
        RevisionRef::WorkingTree => format!("working tree ({source} {side})"),
    }
}

/// The module or package name of `path`: the file's stem, or for an entry file (`lib.rs`,
/// `mod.rs`, `main.rs`, `index.ts`, `__init__.py`) its directory's name, skipping a `src`
/// directory.
fn module_name(path: &str) -> String {
    let mut parts: Vec<&str> = path.split('/').collect();
    let file = parts.pop().unwrap_or(path);
    let stem = file.split('.').next().unwrap_or(file);
    if matches!(stem, "lib" | "mod" | "main" | "index" | "__init__") {
        while let Some(directory) = parts.pop() {
            if directory != "src" {
                return directory.replace('-', "_");
            }
        }
    }
    stem.to_owned()
}

/// `name` and its spelling with `_` and `-` swapped, as packages are named.
fn name_variants(name: &str) -> Vec<String> {
    let mut names = vec![name.to_owned()];
    for variant in [name.replace('_', "-"), name.replace('-', "_")] {
        if !names.contains(&variant) {
            names.push(variant);
        }
    }
    names
}

/// Why `candidate` is a test of `path` (module `name`), when it is one.
fn test_reason(path: &str, name: &str, candidate: &str) -> Option<String> {
    if candidate == path {
        return None;
    }
    let file = candidate.rsplit('/').next().unwrap_or(candidate);
    let stem = file.split('.').next().unwrap_or(file);
    let directory = path.rsplit_once('/').map_or("", |(directory, _)| directory);
    let own_tests = [
        format!("{directory}/{name}/tests.rs"),
        format!("{name}/tests.rs"),
    ];
    if own_tests
        .iter()
        .any(|own| own.trim_start_matches('/') == candidate)
        || candidate.starts_with(
            &format!("{directory}/{name}/tests/")
                .trim_start_matches('/')
                .to_owned(),
        )
    {
        return Some(format!("the test module of {name}"));
    }
    let named = [
        format!("{name}_test"),
        format!("{name}_tests"),
        format!("test_{name}"),
        format!("{name}_spec"),
        format!("{name}Test"),
        format!("{name}Spec"),
    ];
    if named.contains(&stem.to_owned()) {
        return Some(format!("named as a test of {name}"));
    }
    let suffixes = [".test.", ".spec."];
    if stem == name && suffixes.iter().any(|suffix| file.contains(suffix)) {
        return Some(format!("named as a test of {name}"));
    }
    let in_test_directory = candidate
        .split('/')
        .any(|part| matches!(part, "tests" | "test" | "__tests__" | "spec"));
    if in_test_directory && stem.contains(name) {
        return Some(format!("in a test directory and named after {name}"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_names_come_from_the_file_or_its_package() {
        assert_eq!(module_name("crates/net/src/budget.rs"), "budget");
        assert_eq!(module_name("crates/net-io/src/lib.rs"), "net_io");
        assert_eq!(module_name("web/retry/index.ts"), "retry");
        assert_eq!(module_name("pkg/retry/__init__.py"), "retry");
    }

    #[test]
    fn tests_are_recognized_by_naming_convention() {
        let path = "src/net/budget.rs";
        for (candidate, related) in [
            ("src/net/budget/tests.rs", true),
            ("src/net/budget_test.rs", true),
            ("tests/budget_limits.rs", true),
            ("web/budget.test.ts", true),
            ("py/test_budget.py", true),
            ("src/net/budget.rs", false),
            ("src/net/budgetary.rs", false),
        ] {
            assert_eq!(
                test_reason(path, "budget", candidate).is_some(),
                related,
                "{candidate}"
            );
        }
    }
}
