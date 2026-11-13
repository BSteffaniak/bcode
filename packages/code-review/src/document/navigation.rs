//! Navigation targets and search.
//!
//! Targets are computed over the whole review. A target inside a file whose content is not
//! resident is reported at that file's header with `needs_load`, at the position of its
//! placeholder in the motion's order, so the client can load the file and repeat the motion
//! from where it started; nothing is skipped because it was not loaded yet. A target inside a
//! folded file is reported at its header with `opens`, so the client opens the file and
//! repeats the motion.

use super::items::ReviewItem;
use super::keys::{FileKey, FileRef, ItemKey};
use super::review::{BlockKind, ReviewDocument};
use bcode_code_review_models::{ReviewLineKind, ReviewLineSide};

/// Direction of a motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Toward the end of the review.
    Forward,
    /// Toward the start of the review.
    Backward,
}

/// What a motion looks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavKind {
    /// File headers.
    File,
    /// Hunk headers.
    Hunk,
    /// Threads and AI exchanges (including outdated ones).
    Thread,
    /// Threads with an unresolved comment.
    OpenThread,
    /// AI exchanges (answers and suggestions).
    AiAnswer,
    /// File headers of unviewed files.
    UnviewedFile,
    /// The first line of each run of changed lines.
    Change,
}

/// Result of a motion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavTarget {
    /// Item index to scroll to.
    pub index: usize,
    /// Key of that item.
    pub key: ItemKey,
    /// The real target lies inside this file, whose content is not resident: `index` and `key`
    /// are its header (to scroll to meanwhile); load the file and repeat the motion from the
    /// original position (resolved again by its key, since loading changes indices).
    pub needs_load: Option<FileRef>,
    /// The real target lies inside this folded file: `index` and `key` are its header. Open the
    /// file and repeat the motion from the original position.
    pub opens: Option<FileRef>,
}

/// A search over loaded content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    /// Pattern.
    pub pattern: String,
    /// Interpret `pattern` as a regular expression rather than a plain substring.
    pub regex: bool,
    /// Match case-sensitively.
    pub case_sensitive: bool,
}

/// One matching line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    /// File.
    pub file: FileRef,
    /// Side the line is keyed by.
    pub side: ReviewLineSide,
    /// Line number on that side.
    pub line: u32,
    /// Byte ranges of the matches within the line text.
    pub ranges: Vec<(usize, usize)>,
}

impl SearchMatch {
    /// Unified line key of the match; use [`ReviewDocument::index_of`] to resolve it in either
    /// layout.
    #[must_use]
    pub fn key(&self) -> ItemKey {
        ItemKey::Line {
            file: self.file.clone(),
            side: self.side,
            line: self.line,
        }
    }
}

/// Search results in document order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchResults {
    /// Matches in resident content (hunk lines and revealed context).
    pub matches: Vec<SearchMatch>,
    /// Files with changes whose content was not resident and was not searched.
    pub unsearched: Vec<FileRef>,
}

/// Errors from [`ReviewDocument::search`].
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    /// The regular expression is invalid or too large.
    #[error("invalid search pattern: {0}")]
    Pattern(#[from] regex::Error),
}

impl ReviewDocument {
    /// Next or previous target of `kind` strictly after (or before) item `from`.
    #[must_use]
    pub fn navigate(&self, from: usize, kind: NavKind, direction: Direction) -> Option<NavTarget> {
        let len = self.len();
        if len == 0 {
            return None;
        }
        let from = from.min(len - 1);
        let (start_block, _) = self.locate_index(from)?;
        let blocks: Box<dyn Iterator<Item = usize>> = match direction {
            Direction::Forward => Box::new(start_block..self.blocks.len()),
            Direction::Backward => Box::new((0..=start_block).rev()),
        };
        for block in blocks {
            let start = self.block_start(block);
            let items = &self.blocks[block].items;
            let candidates: Box<dyn Iterator<Item = usize>> = match direction {
                Direction::Forward => Box::new(0..items.len()),
                Direction::Backward => Box::new((0..items.len()).rev()),
            };
            for local in candidates {
                let index = start + local;
                let beyond = match direction {
                    Direction::Forward => index > from,
                    Direction::Backward => index < from,
                };
                if local == 0
                    && let Some(target) = self.folded_target(block, from, kind, direction)
                {
                    return Some(target);
                }
                if !beyond {
                    continue;
                }
                // Targets inside a non-resident file lie where its placeholder is (after its
                // file-level annotations); they are reported at its header, to be loaded.
                if let (ReviewItem::Placeholder(_), BlockKind::File(file)) =
                    (&items[local], self.blocks[block].kind)
                    && self.may_contain(file, kind)
                {
                    return Some(NavTarget {
                        index: start,
                        key: items[0].key(),
                        needs_load: Some(self.files[file].key.clone()),
                        opens: None,
                    });
                }
                if self.is_target(block, local, kind) {
                    return Some(NavTarget {
                        index,
                        key: items[local].key(),
                        needs_load: None,
                        opens: None,
                    });
                }
            }
        }
        None
    }

    fn is_target(&self, block: usize, local: usize, kind: NavKind) -> bool {
        let items = &self.blocks[block].items;
        let item = &items[local];
        match kind {
            NavKind::File => matches!(item, ReviewItem::FileHeader(_)),
            NavKind::UnviewedFile => {
                matches!(item, ReviewItem::FileHeader(header) if !header.viewed)
            }
            NavKind::Hunk => matches!(item, ReviewItem::HunkHeader(_)),
            NavKind::Thread => matches!(item, ReviewItem::Thread(_) | ReviewItem::AgentThread(_)),
            NavKind::OpenThread => matches!(item, ReviewItem::Thread(thread) if !thread.resolved),
            NavKind::AiAnswer => matches!(item, ReviewItem::AgentThread(_)),
            NavKind::Change => {
                item.is_change()
                    && !items[..local]
                        .iter()
                        .rev()
                        .find(|previous| previous.line_numbers().is_some())
                        .is_some_and(|previous| {
                            previous.is_change()
                                && !items[..local]
                                    .iter()
                                    .rev()
                                    .take_while(|between| between.line_numbers().is_none())
                                    .any(|between| matches!(between, ReviewItem::HunkHeader(_)))
                        })
            }
        }
    }

    /// The header of the folded file of `block` when its hidden content holds a target of
    /// `kind` that lies beyond item `from` in `direction`. The content would sit before the
    /// file's end (after its header and details).
    fn folded_target(
        &self,
        block: usize,
        from: usize,
        kind: NavKind,
        direction: Direction,
    ) -> Option<NavTarget> {
        let BlockKind::File(file) = self.blocks[block].kind else {
            return None;
        };
        if matches!(kind, NavKind::File | NavKind::UnviewedFile) || !self.folded(file) {
            return None;
        }
        let start = self.block_start(block);
        let items = &self.blocks[block].items;
        let content_at = start
            + items
                .iter()
                .take_while(|item| !matches!(item, ReviewItem::FileEnd(_)))
                .count();
        let beyond = match direction {
            Direction::Forward => content_at > from,
            Direction::Backward => content_at <= from,
        };
        (beyond && self.may_contain(file, kind)).then(|| NavTarget {
            index: start,
            key: items[0].key(),
            needs_load: None,
            opens: Some(self.files[file].key.clone()),
        })
    }

    /// Whether a non-resident (or folded) file can contain a target of `kind`.
    fn may_contain(&self, file: usize, kind: NavKind) -> bool {
        let state = &self.files[file];
        let changed = state.entry.additions + state.entry.deletions > 0;
        match kind {
            NavKind::File | NavKind::UnviewedFile => false,
            NavKind::Hunk | NavKind::Change => changed,
            NavKind::Thread | NavKind::OpenThread | NavKind::AiAnswer => {
                self.file_annotations[file].iter().any(|annotation| {
                    matches!(
                        self.annotations[*annotation].target,
                        super::anchors::AnchorTarget::Lines(_)
                    )
                })
            }
        }
    }

    /// Resolve a key to a navigation target: its index when present, or its file's header with
    /// `needs_load` when its file is not resident.
    #[must_use]
    pub fn jump_to(&self, key: &ItemKey) -> Option<NavTarget> {
        if let Some(index) = self.index_of(key) {
            return Some(NavTarget {
                index,
                key: self.item(index)?.key(),
                needs_load: None,
                opens: None,
            });
        }
        // Keys inside a file (including threads anchored in it) fall back to its header.
        let BlockKind::File(file) = self.blocks[self.block_of_key(key)?].kind else {
            return None;
        };
        if self.file_hidden(file) {
            return None;
        }
        let state = &self.files[file];
        let index = self.block_start(state.block);
        let folded = self.folded(file);
        Some(NavTarget {
            index,
            key: ItemKey::FileHeader(state.key.clone()),
            needs_load: (!folded && state.loaded().is_none()).then(|| state.key.clone()),
            opens: folded.then(|| state.key.clone()),
        })
    }

    /// Resolve a persisted key (for example a restored scroll anchor) to a current key.
    #[must_use]
    pub fn resolve_persisted(
        &self,
        key: &bcode_code_review_models::ReviewItemKey,
    ) -> Option<ItemKey> {
        use bcode_code_review_models::ReviewItemKey as Persisted;
        let file_ref = |source_id: &str, part: Option<u32>, path: &str| {
            let key = FileKey::new(source_id, part, path);
            self.file_index
                .get(&key)
                .map(|index| self.files[*index].key.clone())
        };
        match key {
            Persisted::SourceHeader { source_id } => Some(ItemKey::SourceHeader {
                source_id: source_id.clone(),
                part: self
                    .groups
                    .iter()
                    .find(|group| &group.source.source_id == source_id)
                    .and_then(|group| group.source.part.as_ref().map(|part| part.index)),
            }),
            Persisted::FileHeader {
                source_id,
                path,
                part,
            } => file_ref(source_id, *part, path).map(ItemKey::FileHeader),
            Persisted::HunkHeader {
                source_id,
                path,
                new_start,
                part,
            } => {
                let file = file_ref(source_id, *part, path)?;
                let state = &self.files[*self.file_index.get(&file)?];
                let hunk = state.loaded().and_then(|loaded| {
                    loaded
                        .ranges
                        .iter()
                        .find(|range| range.new_start == *new_start)
                });
                Some(hunk.map_or_else(
                    || ItemKey::FileHeader(file.clone()),
                    |range| ItemKey::HunkHeader {
                        file: file.clone(),
                        old_start: range.old_start,
                        new_start: range.new_start,
                    },
                ))
            }
            Persisted::Line {
                source_id,
                path,
                side,
                line,
                part,
            } => {
                let file = file_ref(source_id, *part, path)?;
                let unified = ItemKey::Line {
                    file,
                    side: *side,
                    line: *line,
                };
                Some(self.map_key(&unified).unwrap_or(unified))
            }
            Persisted::Thread { thread_id } => Some(ItemKey::Thread {
                thread_id: thread_id.clone(),
            }),
        }
    }

    /// Search resident content. Lines are matched one at a time; regular expressions do not
    /// span lines.
    ///
    /// # Errors
    ///
    /// * [`SearchError::Pattern`] when `query.regex` is set and the pattern is not a valid
    ///   regular expression, or exceeds the regex size limit.
    pub fn search(&self, query: &SearchQuery) -> Result<SearchResults, SearchError> {
        let pattern = if query.regex {
            query.pattern.clone()
        } else {
            regex::escape(&query.pattern)
        };
        let matcher = regex::RegexBuilder::new(&pattern)
            .case_insensitive(!query.case_sensitive)
            .build()?;
        let mut results = SearchResults::default();
        if query.pattern.is_empty() {
            return Ok(results);
        }
        let scan = |file: &FileRef, side, line, text: &str, out: &mut Vec<SearchMatch>| {
            let ranges: Vec<(usize, usize)> = matcher
                .find_iter(text)
                .filter(|found| !found.is_empty())
                .map(|found| (found.start(), found.end()))
                .collect();
            if !ranges.is_empty() {
                out.push(SearchMatch {
                    file: file.clone(),
                    side,
                    line,
                    ranges,
                });
            }
        };
        for (index, state) in self.files.iter().enumerate() {
            if self.file_hidden(index) {
                continue;
            }
            let Some(loaded) = state.loaded() else {
                if state.entry.additions + state.entry.deletions > 0 {
                    results.unsearched.push(state.key.clone());
                }
                continue;
            };
            let context_side = state.context_side();
            // Matches are ordered by position on the context side; a line without a
            // context-side number (a removed line) sorts after the context line before it.
            let mut file_matches: Vec<((u32, u32), SearchMatch)> = Vec::new();
            for hunk in &loaded.hunks {
                let mut cursor = (
                    super::gaps::hunk_span(&hunk.range, context_side)
                        .first
                        .saturating_sub(1),
                    0_u32,
                );
                for line in &hunk.lines {
                    let (side, number) = match (line.kind, line.new, line.old) {
                        (ReviewLineKind::Removed, _, Some(old)) | (_, None, Some(old)) => {
                            (ReviewLineSide::Old, old)
                        }
                        (_, Some(new), _) => (ReviewLineSide::New, new),
                        (_, None, None) => continue,
                    };
                    let own = match context_side {
                        ReviewLineSide::Old => line.old,
                        ReviewLineSide::New => line.new,
                    };
                    cursor = own.map_or((cursor.0, cursor.1 + 1), |own| (own, 0));
                    let mut found = Vec::new();
                    scan(&state.key, side, number, &line.text, &mut found);
                    file_matches.extend(found.into_iter().map(|found| (cursor, found)));
                }
            }
            for (line, text) in &loaded.context {
                if state.revealed.contains(*line) {
                    let mut found = Vec::new();
                    scan(&state.key, context_side, *line, text, &mut found);
                    file_matches.extend(found.into_iter().map(|found| ((*line, 0), found)));
                }
            }
            file_matches.sort_by_key(|(position, _)| *position);
            let file_matches = file_matches.into_iter().map(|(_, found)| found);
            results.matches.extend(file_matches);
        }
        Ok(results)
    }

    /// Next or previous search match relative to item `from`, resolving each match's key in the
    /// current layout.
    #[must_use]
    pub fn navigate_matches(
        &self,
        results: &SearchResults,
        from: usize,
        direction: Direction,
    ) -> Option<NavTarget> {
        let indices = self.match_indices(results);
        let found = match direction {
            Direction::Forward => indices.into_iter().find(|(index, ..)| *index > from),
            Direction::Backward => indices.into_iter().rev().find(|(index, ..)| *index < from),
        }?;
        Some(NavTarget {
            index: found.0,
            key: found.1,
            needs_load: None,
            opens: found.2,
        })
    }

    /// Where stepping through matches in `direction` wraps to: the first match going forward,
    /// the last going backward.
    #[must_use]
    pub fn wrapped_match(
        &self,
        results: &SearchResults,
        direction: Direction,
    ) -> Option<NavTarget> {
        let indices = self.match_indices(results);
        let found = match direction {
            Direction::Forward => indices.into_iter().next(),
            Direction::Backward => indices.into_iter().next_back(),
        }?;
        Some(NavTarget {
            index: found.0,
            key: found.1,
            needs_load: None,
            opens: found.2,
        })
    }

    /// The items holding a match in the current layout, in order, one per item. Matches in a
    /// folded file are at its header, with the file to open.
    fn match_indices(&self, results: &SearchResults) -> Vec<(usize, ItemKey, Option<FileRef>)> {
        let mut indices: Vec<(usize, ItemKey, Option<FileRef>)> = results
            .matches
            .iter()
            .filter_map(|found| {
                if let Some(index) = self.index_of(&found.key()) {
                    return Some((index, self.item(index)?.key(), None));
                }
                let file = *self.file_index.get(&found.file)?;
                if !self.folded(file) || self.file_hidden(file) {
                    return None;
                }
                let index = self.block_start(self.files[file].block);
                Some((index, self.item(index)?.key(), Some(found.file.clone())))
            })
            .collect();
        indices.sort_by_key(|(index, ..)| *index);
        indices.dedup_by_key(|(index, ..)| *index);
        indices
    }
}
