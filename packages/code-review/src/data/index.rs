//! Per-source file index (`review.index.get`) from `git diff --raw --numstat -z`.

use super::{DataError, decode_cursor, fingerprint, git, page, page_size};
use crate::cache::{CacheKind, ReviewCache};
use bcode_code_review_models::{
    REVIEW_INDEX_DEFAULT_PAGE_SIZE, REVIEW_INDEX_MAX_PAGE_SIZE, ResolvedSource, ResolvedSourceMode,
    ReviewBlobRef, ReviewFileStatus, ReviewIndexEntry, ReviewIndexFlag, ReviewIndexRequest,
    ReviewIndexResponse, RevisionRef,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Immutable (commit-to-commit) listings kept in memory: every layer of a large stack, so a
/// reopened step-through review lists nothing again. Listings are small (one entry per file).
const MAX_CACHED_LISTINGS: usize = 256;

const MODE_SYMLINK: &str = "120000";
const MODE_GITLINK: &str = "160000";

/// A complete listing of one resolved source.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct IndexListing {
    fingerprint: String,
    entries: Vec<ReviewIndexEntry>,
    additions: u64,
    deletions: u64,
}

/// Bounded cache of listings whose sides are both immutable commits, backed by the on-disk
/// cache.
#[derive(Debug, Default)]
pub struct IndexCache {
    entries: VecDeque<(String, Arc<IndexListing>)>,
    disk: Arc<ReviewCache>,
}

impl IndexCache {
    /// An empty cache backed by `disk`.
    pub const fn with_disk(disk: Arc<ReviewCache>) -> Self {
        Self {
            entries: VecDeque::new(),
            disk,
        }
    }

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

/// The cache key of `source`'s listing, and whether it can be cached (both sides immutable).
fn listing_key(root: &Path, source: &ResolvedSource) -> Result<(String, bool), DataError> {
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
    Ok((key, immutable))
}

/// The on-disk cache key of a listing key: its digest, so keys stay short.
fn disk_key(key: &str) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(key.as_bytes()))
}

/// Load the listings of `sources` the on-disk cache holds into `cache`, in one read, so listing
/// them runs no git.
pub fn prefetch(cache: &Mutex<IndexCache>, root: &Path, sources: &[ResolvedSource]) {
    let disk = Arc::clone(&lock(cache).disk);
    if !disk.enabled() {
        return;
    }
    let wanted: Vec<String> = {
        let mut memory = lock(cache);
        sources
            .iter()
            .filter_map(|source| listing_key(root, source).ok())
            .filter(|(key, immutable)| *immutable && memory.get(key).is_none())
            .map(|(key, _)| key)
            .collect()
    };
    let by_disk_key: BTreeMap<String, String> = wanted
        .into_iter()
        .map(|key| (disk_key(&key), key))
        .collect();
    let keys: Vec<String> = by_disk_key.keys().cloned().collect();
    let found = disk.get_many(CacheKind::Index, &keys);
    let mut memory = lock(cache);
    for (stored, entry) in found {
        if let (Some(key), Ok(listing)) = (
            by_disk_key.get(&stored),
            serde_json::from_str::<IndexListing>(&entry.value),
        ) {
            memory.insert(key.clone(), Arc::new(listing));
        }
    }
}

/// List every commit-to-commit diff of `sources` that `cache` does not hold yet with one
/// `git diff-tree --stdin` (instead of a `git diff` each), keeping the listings in `cache` and on
/// disk. Sources it cannot cover (paths, live sides, a head shared with another source) and
/// everything when git or its output is unexpected are left to [`index_page`], one at a time.
pub fn list_together(cache: &Mutex<IndexCache>, root: &Path, sources: &[ResolvedSource]) {
    let mut heads: BTreeMap<&str, (&str, String)> = BTreeMap::new();
    {
        let mut memory = lock(cache);
        for source in sources {
            let (
                ResolvedSourceMode::Diff,
                RevisionRef::Commit { oid: base },
                RevisionRef::Commit { oid: head },
            ) = (&source.mode, &source.base, &source.head)
            else {
                continue;
            };
            if !source.paths.is_empty()
                || git::validate_oid(base).is_err()
                || git::validate_oid(head).is_err()
            {
                continue;
            }
            let Ok((key, true)) = listing_key(root, source) else {
                continue;
            };
            if memory.get(&key).is_none() && !heads.contains_key(head.as_str()) {
                heads.insert(head, (base, key));
            }
        }
    }
    // One diff is as quick alone; the batch pays off from two.
    if heads.len() < 2 {
        return;
    }
    let input = heads
        .iter()
        .fold(String::new(), |mut input, (head, (base, _))| {
            let _ = writeln!(input, "{head} {base}");
            input
        });
    let mut args = vec![
        "diff-tree",
        "--stdin",
        "-r",
        "--raw",
        "--numstat",
        "-z",
        "--no-abbrev",
    ];
    args.extend(DIFF_COMMON);
    let Ok(output) = git::output_with_input(root, &args, &input) else {
        return;
    };
    let tokens = git::split_nul(&output);
    let Some(segments) = diff_tree_segments(&tokens, &heads) else {
        return;
    };
    let disk = Arc::clone(&lock(cache).disk);
    let mut stored = Vec::new();
    let mut listings = Vec::new();
    for (head, (_, key)) in &heads {
        // A pair without changes prints nothing.
        let segment = segments.get(head).map_or_else(Vec::new, |tokens| {
            let mut bytes = tokens.join("\0").into_bytes();
            bytes.push(0);
            bytes
        });
        let Ok(entries) = parse_raw_numstat("", &segment) else {
            return;
        };
        let (additions, deletions) = totals(&entries);
        let listing = IndexListing {
            fingerprint: fingerprint(&[key.as_bytes(), &segment]),
            entries,
            additions,
            deletions,
        };
        if disk.enabled()
            && let Ok(text) = serde_json::to_string(&listing)
        {
            stored.push((disk_key(key), text));
        }
        listings.push((key.clone(), Arc::new(listing)));
    }
    disk.put_many(CacheKind::Index, &stored);
    let mut memory = lock(cache);
    for (key, listing) in listings {
        memory.insert(key, listing);
    }
}

/// `git diff-tree --stdin -z` output split per commit: each commit's header (its id), then its
/// raw records (each with one path, or two for a rename or copy), then its numstat records (one
/// token, or a token and two paths for a rename or copy). `None` for anything else.
fn diff_tree_segments<'a>(
    tokens: &'a [String],
    heads: &BTreeMap<&str, (&str, String)>,
) -> Option<BTreeMap<&'a str, Vec<String>>> {
    let mut segments: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut current: Option<&str> = None;
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        let paths = if let Some(record) = token.strip_prefix(':') {
            let status = record.rsplit(' ').next()?;
            if status.starts_with(['R', 'C']) { 2 } else { 1 }
        } else if token.matches('\t').count() >= 2 {
            usize::from(token.ends_with('\t')) * 2
        } else if heads.contains_key(token.as_str()) && !segments.contains_key(token.as_str()) {
            current = Some(token);
            segments.insert(token, Vec::new());
            index += 1;
            continue;
        } else {
            return None;
        };
        let segment = segments.get_mut(current?)?;
        segment.extend(tokens.get(index..=index + paths)?.iter().cloned());
        index += paths + 1;
    }
    Some(segments)
}

/// Lines added and removed across `entries`.
fn totals(entries: &[ReviewIndexEntry]) -> (u64, u64) {
    entries.iter().fold((0_u64, 0_u64), |(add, del), entry| {
        (
            add.saturating_add(u64::from(entry.additions)),
            del.saturating_add(u64::from(entry.deletions)),
        )
    })
}

/// Listings being computed, so overlapping requests for one share its git process.
static LISTINGS: std::sync::LazyLock<super::flight::SingleFlight<String, Arc<IndexListing>>> =
    std::sync::LazyLock::new(super::flight::SingleFlight::default);

fn listing(
    cache: &Mutex<IndexCache>,
    root: &Path,
    source: &ResolvedSource,
) -> Result<Arc<IndexListing>, DataError> {
    let (key, immutable) = listing_key(root, source)?;
    if !immutable {
        // A live side (the index or working tree) is listed every time.
        return compute_listing(root, source, &key);
    }
    // The cache lock is held only to look up and to store, never while git or the disk runs.
    let kept = lock(cache).get(&key);
    if let Some(listing) = kept {
        return Ok(listing);
    }
    LISTINGS.run(&key, || {
        // Another flight may have landed between the lookup and this one.
        let kept = lock(cache).get(&key);
        if let Some(listing) = kept {
            return Ok(listing);
        }
        let disk = Arc::clone(&lock(cache).disk);
        if let Some(entry) = disk
            .get_many(CacheKind::Index, &[disk_key(&key)])
            .into_values()
            .next()
            && let Ok(listing) = serde_json::from_str::<IndexListing>(&entry.value)
        {
            let listing = Arc::new(listing);
            lock(cache).insert(key.clone(), Arc::clone(&listing));
            return Ok(listing);
        }
        let listing = compute_listing(root, source, &key)?;
        if disk.enabled()
            && let Ok(text) = serde_json::to_string(listing.as_ref())
        {
            disk.put_many(CacheKind::Index, &[(disk_key(&key), text)]);
        }
        lock(cache).insert(key.clone(), Arc::clone(&listing));
        Ok(listing)
    })
}

/// List `source` with git.
fn compute_listing(
    root: &Path,
    source: &ResolvedSource,
    key: &str,
) -> Result<Arc<IndexListing>, DataError> {
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
    let (additions, deletions) = totals(&entries);
    Ok(Arc::new(IndexListing {
        fingerprint: fingerprint(&[key.as_bytes(), &output]),
        entries,
        additions,
        deletions,
    }))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::fixture::{
        BASE_BRANCH, FixtureSpec, GitFixture, HEAD_BRANCH, STACK_BRANCHES,
    };

    fn commit(root: &Path, rev: &str) -> String {
        let output = std::process::Command::new("git")
            .args(["rev-parse", rev])
            .current_dir(root)
            .output()
            .expect("git");
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    #[test]
    fn listings_from_one_diff_tree_match_one_diff_each() {
        // Renames, mode changes, binary and special entries, deletions, and an empty diff.
        let fixture = GitFixture::generate(FixtureSpec::small());
        let root = fixture.root();
        let chain = [
            BASE_BRANCH,
            HEAD_BRANCH,
            STACK_BRANCHES[0],
            STACK_BRANCHES[1],
        ];
        let mut sources: Vec<ResolvedSource> = chain
            .windows(2)
            .enumerate()
            .map(|(index, pair)| ResolvedSource {
                source_id: format!("s{index}"),
                base: RevisionRef::commit(commit(root, pair[0])),
                head: RevisionRef::commit(commit(root, pair[1])),
                mode: ResolvedSourceMode::Diff,
                paths: Vec::new(),
                line_range: None,
                part: None,
            })
            .collect();
        let tip = commit(root, STACK_BRANCHES[2]);
        sources.push(ResolvedSource {
            source_id: "empty".into(),
            base: RevisionRef::commit(tip.clone()),
            head: RevisionRef::commit(tip),
            ..sources[0].clone()
        });
        let together = Mutex::new(IndexCache::default());
        list_together(&together, root, &sources);
        assert_eq!(cached_listing_count(&together), sources.len());
        let alone = Mutex::new(IndexCache::default());
        for source in &sources {
            let batched = listing(&together, root, source).expect("batched");
            let single = listing(&alone, root, source).expect("single");
            let paths = |listing: &IndexListing| {
                listing
                    .entries
                    .iter()
                    .map(|entry| {
                        let mut entry = entry.clone();
                        entry.source_id.clear();
                        entry
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(paths(&batched), paths(&single), "{}", source.source_id);
            assert_eq!(
                (batched.additions, batched.deletions),
                (single.additions, single.deletions)
            );
        }
        assert_eq!(
            listing(&together, root, &sources[3])
                .expect("empty")
                .entries
                .len(),
            0
        );
    }
}
