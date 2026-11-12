//! Per-source file index (`review.index.get`) from `git diff --raw --numstat -z`.

use super::{DataError, decode_cursor, fingerprint, git, page, page_size};
use bcode_code_review_models::{
    REVIEW_INDEX_DEFAULT_PAGE_SIZE, REVIEW_INDEX_MAX_PAGE_SIZE, ResolvedSource, ResolvedSourceMode,
    ReviewBlobRef, ReviewFileStatus, ReviewIndexEntry, ReviewIndexFlag, ReviewIndexRequest,
    ReviewIndexResponse, RevisionRef,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Immutable (commit-to-commit) listings kept for paging.
const MAX_CACHED_LISTINGS: usize = 8;

const MODE_SYMLINK: &str = "120000";
const MODE_GITLINK: &str = "160000";

/// A complete listing of one resolved source.
#[derive(Debug)]
pub struct IndexListing {
    fingerprint: String,
    entries: Vec<ReviewIndexEntry>,
    additions: u64,
    deletions: u64,
}

/// Bounded cache of listings whose sides are both immutable commits.
#[derive(Debug, Default)]
pub struct IndexCache {
    entries: VecDeque<(String, Arc<IndexListing>)>,
}

impl IndexCache {
    fn get(&mut self, key: &str) -> Option<Arc<IndexListing>> {
        let position = self.entries.iter().position(|(cached, _)| cached == key)?;
        let entry = self.entries.remove(position)?;
        let listing = Arc::clone(&entry.1);
        self.entries.push_back(entry);
        Some(listing)
    }

    fn insert(&mut self, key: String, listing: Arc<IndexListing>) {
        self.entries.retain(|(cached, _)| *cached != key);
        if self.entries.len() >= MAX_CACHED_LISTINGS {
            self.entries.pop_front();
        }
        self.entries.push_back((key, listing));
    }
}

/// Return one page of the source's index.
pub fn index_page(
    cache: &Mutex<IndexCache>,
    root: &Path,
    request: &ReviewIndexRequest,
) -> Result<ReviewIndexResponse, DataError> {
    let listing = listing(cache, root, &request.source)?;
    let offset = decode_cursor(request.cursor.as_deref(), &listing.fingerprint)?;
    let size = page_size(
        request.limit,
        REVIEW_INDEX_DEFAULT_PAGE_SIZE,
        REVIEW_INDEX_MAX_PAGE_SIZE,
    );
    let (mut entries, next_cursor) = page(&listing.entries, offset, size, &listing.fingerprint)?;
    // Listings are shared by every source with the same revisions; entries name the asking one.
    for entry in &mut entries {
        if entry.source_id != request.source.source_id {
            entry.source_id.clone_from(&request.source.source_id);
        }
    }
    Ok(ReviewIndexResponse {
        source_id: request.source.source_id.clone(),
        entries,
        next_cursor,
        total_entries: u32::try_from(listing.entries.len()).unwrap_or(u32::MAX),
        total_additions: listing.additions,
        total_deletions: listing.deletions,
    })
}

fn listing(
    cache: &Mutex<IndexCache>,
    root: &Path,
    source: &ResolvedSource,
) -> Result<Arc<IndexListing>, DataError> {
    for path in &source.paths {
        git::validate_relative_path(path)?;
    }
    let immutable = !source.base.is_live() && !source.head.is_live();
    let key = serde_json::to_string(&(
        root,
        &source.base,
        &source.head,
        &source.mode,
        &source.paths,
    ))
    .map_err(|error| DataError::InvalidRequest(error.to_string()))?;
    // The cache lock is held only to look up and to store, never while git runs.
    if immutable && let Some(listing) = lock(cache).get(&key) {
        return Ok(listing);
    }
    let (output, entries) = match source.mode {
        ResolvedSourceMode::Diff => {
            let args = diff_args(root, source)?;
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let output = git::output(root, &args)?;
            let entries = parse_raw_numstat(&source.source_id, &output)?;
            (output, entries)
        }
        ResolvedSourceMode::Browse => {
            let mut args = vec!["ls-files", "-z", "--stage", "--"];
            args.extend(source.paths.iter().map(String::as_str));
            let output = git::output(root, &args)?;
            let entries = parse_ls_files_stage(&source.source_id, &output);
            (output, entries)
        }
    };
    let (additions, deletions) = entries.iter().fold((0_u64, 0_u64), |(add, del), entry| {
        (
            add.saturating_add(u64::from(entry.additions)),
            del.saturating_add(u64::from(entry.deletions)),
        )
    });
    let listing = Arc::new(IndexListing {
        fingerprint: fingerprint(&[key.as_bytes(), &output]),
        entries,
        additions,
        deletions,
    });
    if immutable {
        lock(cache).insert(key, Arc::clone(&listing));
    }
    Ok(listing)
}

/// Resolve one side to a tree-ish argument (`None` for the index or working tree).
fn tree_arg(root: &Path, side: &RevisionRef) -> Result<Option<String>, DataError> {
    match side {
        RevisionRef::Commit { oid } => {
            git::validate_oid(oid)?;
            Ok(Some(oid.clone()))
        }
        RevisionRef::EmptyTree => git::empty_tree_oid(root).map(Some),
        RevisionRef::Index | RevisionRef::WorkingTree => Ok(None),
    }
}

/// The `git diff` revision arguments selecting `base` and `head`.
///
/// * commit → commit: `diff BASE HEAD`
/// * commit → index: `diff --cached BASE`
/// * commit → working tree: `diff BASE`
/// * index → working tree: `diff`
///
/// # Errors
///
/// * [`DataError::InvalidRequest`] for any other combination.
pub fn revision_args(
    root: &Path,
    base: &RevisionRef,
    head: &RevisionRef,
) -> Result<Vec<String>, DataError> {
    let base_tree = tree_arg(root, base)?;
    let head_tree = tree_arg(root, head)?;
    Ok(match (base, head, base_tree, head_tree) {
        (_, _, Some(base), Some(head)) => vec![base, head],
        (_, RevisionRef::Index, Some(base), None) => vec!["--cached".to_string(), base],
        (_, RevisionRef::WorkingTree, Some(base), None) => vec![base],
        (RevisionRef::Index, RevisionRef::WorkingTree, None, None) => Vec::new(),
        _ => {
            return Err(DataError::InvalidRequest(format!(
                "unsupported revision pair: {base:?} → {head:?}"
            )));
        }
    })
}

/// Common `git diff` options for machine-readable, configuration-independent output.
pub const DIFF_COMMON: [&str; 5] = [
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--find-renames",
    "--ignore-submodules=none",
];

fn diff_args(root: &Path, source: &ResolvedSource) -> Result<Vec<String>, DataError> {
    let mut args: Vec<String> = ["diff", "--raw", "--numstat", "-z", "--no-abbrev"]
        .into_iter()
        .chain(DIFF_COMMON)
        .map(str::to_string)
        .collect();
    args.extend(revision_args(root, &source.base, &source.head)?);
    args.push("--".to_string());
    args.extend(source.paths.iter().cloned());
    Ok(args)
}

/// Added and removed line counts from one numstat record; `None` for binary content (`-`).
type NumstatCounts = (Option<u32>, Option<u32>);

struct RawRecord {
    old_mode: String,
    new_mode: String,
    old_oid: String,
    new_oid: String,
    status: char,
    similarity: Option<u8>,
    old_path: String,
    new_path: String,
}

/// Parse `--raw --numstat -z` output. Git prints every raw record first and then every numstat
/// record, both in the same file order.
fn parse_raw_numstat(source_id: &str, output: &[u8]) -> Result<Vec<ReviewIndexEntry>, DataError> {
    let tokens = git::split_nul(output);
    let mut tokens = tokens.into_iter().peekable();
    let mut raw = Vec::new();
    while let Some(token) = tokens.next_if(|token| token.starts_with(':')) {
        let fields: Vec<&str> = token[1..].split(' ').collect();
        let [old_mode, new_mode, old_oid, new_oid, status] = fields.as_slice() else {
            return Err(DataError::Git(format!(
                "unexpected raw diff record: {token}"
            )));
        };
        let status_char = status.chars().next().unwrap_or('X');
        let similarity = status.get(1..).and_then(|score| score.parse::<u8>().ok());
        let first = tokens
            .next()
            .ok_or_else(|| DataError::Git("raw diff record without a path".to_string()))?;
        let (old_path, new_path) = if matches!(status_char, 'R' | 'C') {
            let second = tokens
                .next()
                .ok_or_else(|| DataError::Git("rename record without a destination".to_string()))?;
            (first, second)
        } else {
            (first.clone(), first)
        };
        raw.push(RawRecord {
            old_mode: (*old_mode).to_string(),
            new_mode: (*new_mode).to_string(),
            old_oid: (*old_oid).to_string(),
            new_oid: (*new_oid).to_string(),
            status: status_char,
            similarity,
            old_path,
            new_path,
        });
    }

    let mut counts: BTreeMap<String, Vec<NumstatCounts>> = BTreeMap::new();
    while let Some(token) = tokens.next() {
        let mut fields = token.splitn(3, '\t');
        let added = fields.next().unwrap_or_default().parse::<u32>().ok();
        let deleted = fields.next().unwrap_or_default().parse::<u32>().ok();
        let path = match fields.next() {
            Some(path) if !path.is_empty() => path.to_string(),
            _ => {
                tokens.next();
                tokens.next().unwrap_or_default()
            }
        };
        counts.entry(path).or_default().push((added, deleted));
    }
    let mut used: BTreeMap<String, usize> = BTreeMap::new();

    Ok(raw
        .into_iter()
        .map(|record| {
            let numstat_path = record.new_path.clone();
            let index = used.entry(numstat_path.clone()).or_insert(0);
            let (added, deleted) = counts
                .get(&numstat_path)
                .and_then(|values| values.get(*index).copied())
                .unwrap_or((Some(0), Some(0)));
            *index += 1;
            entry_from_raw(source_id, &record, added, deleted)
        })
        .collect())
}

fn blob_ref(mode: &str, oid: &str, is_new_side: bool) -> ReviewBlobRef {
    if mode == "000000" {
        return ReviewBlobRef::Absent;
    }
    if git::is_null_oid(oid) {
        // A present side without an object id is unhashed working-tree content.
        return if is_new_side {
            ReviewBlobRef::WorkingTree
        } else {
            ReviewBlobRef::Absent
        };
    }
    ReviewBlobRef::Object {
        oid: oid.to_string(),
    }
}

fn entry_from_raw(
    source_id: &str,
    record: &RawRecord,
    added: Option<u32>,
    deleted: Option<u32>,
) -> ReviewIndexEntry {
    let status = match record.status {
        'A' => ReviewFileStatus::Added,
        'D' => ReviewFileStatus::Deleted,
        'R' => ReviewFileStatus::Renamed,
        'C' => ReviewFileStatus::Copied,
        'M' | 'T' => ReviewFileStatus::Modified,
        _ => ReviewFileStatus::Unknown,
    };
    let old_present = record.old_mode != "000000";
    let new_present = record.new_mode != "000000";
    let path = if new_present || status != ReviewFileStatus::Deleted {
        record.new_path.clone()
    } else {
        record.old_path.clone()
    };
    let old_path = (record.old_path != record.new_path).then(|| record.old_path.clone());
    let modes = [record.old_mode.as_str(), record.new_mode.as_str()];
    ReviewIndexEntry {
        source_id: source_id.to_string(),
        path,
        old_path,
        status,
        old_blob: blob_ref(&record.old_mode, &record.old_oid, false),
        new_blob: blob_ref(&record.new_mode, &record.new_oid, true),
        old_mode: old_present.then(|| record.old_mode.clone()),
        new_mode: new_present.then(|| record.new_mode.clone()),
        similarity: record.similarity,
        additions: added.unwrap_or(0),
        deletions: deleted.unwrap_or(0),
        flags: [
            // Numstat reports `-` for both counts on binary content.
            (
                ReviewIndexFlag::Binary,
                added.is_none() && deleted.is_none(),
            ),
            (
                ReviewIndexFlag::ModeChanged,
                old_present && new_present && record.old_mode != record.new_mode,
            ),
            (ReviewIndexFlag::Symlink, modes.contains(&MODE_SYMLINK)),
            (ReviewIndexFlag::Submodule, modes.contains(&MODE_GITLINK)),
        ]
        .into_iter()
        .filter_map(|(flag, set)| set.then_some(flag))
        .collect(),
    }
}

/// Parse `git ls-files -z --stage` output into browse entries (one per path).
fn parse_ls_files_stage(source_id: &str, output: &[u8]) -> Vec<ReviewIndexEntry> {
    let mut seen = BTreeSet::new();
    git::split_nul(output)
        .into_iter()
        .filter_map(|record| {
            let (meta, path) = record.split_once('\t')?;
            if !seen.insert(path.to_string()) {
                return None;
            }
            let mode = meta.split(' ').next().unwrap_or_default().to_string();
            Some(ReviewIndexEntry {
                source_id: source_id.to_string(),
                path: path.to_string(),
                old_path: None,
                status: ReviewFileStatus::Unknown,
                old_blob: ReviewBlobRef::WorkingTree,
                new_blob: ReviewBlobRef::WorkingTree,
                old_mode: Some(mode.clone()),
                new_mode: Some(mode.clone()),
                similarity: None,
                additions: 0,
                deletions: 0,
                flags: [
                    (ReviewIndexFlag::Symlink, mode == MODE_SYMLINK),
                    (ReviewIndexFlag::Submodule, mode == MODE_GITLINK),
                ]
                .into_iter()
                .filter_map(|(flag, set)| set.then_some(flag))
                .collect(),
            })
        })
        .collect()
}

/// Number of cached listings, for tests that inspect the cache.
#[cfg(test)]
pub fn cached_listing_count(cache: &Mutex<IndexCache>) -> usize {
    lock(cache).entries.len()
}

fn lock(cache: &Mutex<IndexCache>) -> std::sync::MutexGuard<'_, IndexCache> {
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
