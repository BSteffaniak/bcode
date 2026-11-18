//! Layer deltas and line marks: what changed in a layer between two of its versions, with the
//! rebase taken out, down to which lines of the current diff are new since the baseline.
//!
//! The baseline version's own change (`fork(A) → head(A)`) is replayed on the current version's
//! fork point with one `git merge-tree --stdin` for every layer, and wrapped in a deterministic
//! commit (fixed author, committer, dates, and a message naming its inputs), so the same inputs
//! always make the same commit and every cache key built on it stays stable. The delta is then
//! the diff from that replay commit to the current head. Results are content-addressed and kept
//! in the on-disk cache.

use super::compare::{Replay, replay_trees, trees_of};
use super::present_commits;
use crate::cache::{CacheKind, ReviewCache};
use crate::data::DataError;
use crate::data::git;
use bcode_code_review_models::{
    DeltaFile, DeltaKind, FileSince, LOOK_REF_NAMESPACE, LayerDelta, LayerPair, LineSpan,
    REVIEW_COMPARE_MAX_LAYERS, RemovedSince, ReviewCompareMarksRequest, ReviewCompareMarksResponse,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

/// Most diff lines one file's marks read; past it the file is reported too large to mark.
pub const MAX_MARK_LINES: usize = crate::data::MAX_ATTRIBUTION_LINES;

/// Longest baseline line (in bytes) kept to mark the words of the line that replaced it: longer
/// lines are marked whole.
pub const MAX_WAS_BYTES: usize = 4_096;

/// The fixed identity and date of replay commits.
const REPLAY_ENV: [(&str, &str); 6] = [
    ("GIT_AUTHOR_NAME", "bcode-review"),
    ("GIT_AUTHOR_EMAIL", "bcode-review@localhost"),
    ("GIT_AUTHOR_DATE", "@0 +0000"),
    ("GIT_COMMITTER_NAME", "bcode-review"),
    ("GIT_COMMITTER_EMAIL", "bcode-review@localhost"),
    ("GIT_COMMITTER_DATE", "@0 +0000"),
];

/// The commit wrapping `tree` (the change of `old_head`, from `old_fork`, replayed on
/// `new_fork`), parented on `new_fork`: the same inputs always make the same commit.
///
/// # Errors
///
/// * [`DataError::InvalidRequest`] for a malformed commit id; [`DataError::Git`] when git fails.
pub fn replay_commit(
    root: &Path,
    tree: &str,
    old_fork: &str,
    new_fork: &str,
    old_head: &str,
) -> Result<String, DataError> {
    for oid in [tree, old_fork, new_fork, old_head] {
        git::validate_oid(oid)?;
    }
    let message = format!(
        "bcode-review replay\n\nold-fork {old_fork}\nnew-fork {new_fork}\nold-head {old_head}\n"
    );
    let output = git::output_with_env(
        root,
        &[
            "commit-tree",
            "--no-gpg-sign",
            tree,
            "-p",
            new_fork,
            "-m",
            &message,
        ],
        &REPLAY_ENV,
    )?;
    Ok(String::from_utf8_lossy(&output).trim().to_owned())
}

/// Replay commits and their unresolved paths by (old fork, new fork, old head).
type Replays = BTreeMap<(String, String, String), Option<Replay>>;

/// The replay of `(old fork, new fork, old head)`: its commit, or `None` when it conflicts.
/// Cached; a cached commit git collected since is made again.
fn replays(
    root: &Path,
    disk: &ReviewCache,
    wanted: &[(String, String, String)],
) -> Result<Replays, DataError> {
    let key = |(old_fork, new_fork, old_head): &(String, String, String)| {
        format!("paths-v1 {old_fork} {new_fork} {old_head}")
    };
    let keys: Vec<String> = wanted.iter().map(key).collect();
    let cached = disk.get_many(CacheKind::Replay, &keys);
    let mut found = BTreeMap::new();
    for (triple, key) in wanted.iter().zip(&keys) {
        if let Some(entry) = cached.get(key)
            && let Ok(replay) = serde_json::from_str::<Option<Replay>>(&entry.value)
            && replay
                .as_ref()
                .is_none_or(|r| git::validate_oid(&r.oid).is_ok())
        {
            found.insert(triple.clone(), replay);
        }
    }
    let commits = found.values().flatten().map(|r| r.oid.as_str()).collect();
    let present = present_commits(root, &commits)?;
    found.retain(|_, replay| replay.as_ref().is_none_or(|r| present.contains(&r.oid)));
    let missing: Vec<(String, String, String)> = wanted
        .iter()
        .filter(|triple| !found.contains_key(*triple))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let trees = replay_trees(root, &missing)?;
    let mut stored = Vec::new();
    for (triple, mut replay) in missing.into_iter().zip(trees) {
        if let Some(r) = &mut replay {
            r.oid = replay_commit(root, &r.oid, &triple.0, &triple.1, &triple.2)?;
        }
        if let Ok(value) = serde_json::to_string(&replay) {
            stored.push((key(&triple), value));
        }
        found.insert(triple, replay);
    }
    disk.put_many(CacheKind::Replay, &stored);
    Ok(found)
}

/// How each pair is compared, from what is present and what replayed.
fn plan(pairs: &[LayerPair], present: &BTreeSet<String>, replayed: &Replays) -> Vec<Plan> {
    let has = |oid: &str| present.contains(oid);
    pairs
        .iter()
        .map(|pair| {
            let Some(from) = &pair.from else {
                return Plan::Done(DeltaKind::NewToYou);
            };
            if from.head_oid == pair.to.head_oid {
                return Plan::Done(DeltaKind::Unchanged);
            }
            if !has(&from.head_oid) || !has(&pair.to.head_oid) {
                return Plan::Done(DeltaKind::Unavailable);
            }
            let head_to_head = Plan::From {
                base: from.head_oid.clone(),
                rebase_removed: false,
                conflicts: BTreeSet::new(),
            };
            match (&from.fork_oid, &pair.to.fork_oid) {
                // The same fork point: the old head is its own replay.
                (Some(old_fork), Some(new_fork)) if old_fork == new_fork && has(new_fork) => {
                    Plan::From {
                        base: from.head_oid.clone(),
                        rebase_removed: true,
                        conflicts: BTreeSet::new(),
                    }
                }
                (Some(old_fork), Some(new_fork)) => replayed
                    .get(&(old_fork.clone(), new_fork.clone(), from.head_oid.clone()))
                    .cloned()
                    .flatten()
                    .map_or(head_to_head, |r| Plan::From {
                        base: r.oid,
                        conflicts: r.conflicts,
                        rebase_removed: true,
                    }),
                _ => head_to_head,
            }
        })
        .collect()
}

/// How a pair is compared, before any diff.
enum Plan {
    Done(DeltaKind),
    /// From this commit to the head; `rebase_removed` when it is a replay.
    From {
        base: String,
        rebase_removed: bool,
        conflicts: BTreeSet<String>,
    },
}

/// Each pair's delta, in order: unchanged, rebased only, changed (with its files), new to you,
/// with the rebase (head against head), or unavailable.
///
/// # Errors
///
/// * [`DataError::InvalidRequest`] for too many pairs or a malformed commit id.
/// * [`DataError::Git`] / [`DataError::Io`] when git cannot run.
pub fn layer_deltas(
    root: &Path,
    disk: &ReviewCache,
    pairs: &[LayerPair],
) -> Result<Vec<LayerDelta>, DataError> {
    if pairs.len() > REVIEW_COMPARE_MAX_LAYERS {
        return Err(DataError::InvalidRequest(format!(
            "at most {REVIEW_COMPARE_MAX_LAYERS} layers per comparison"
        )));
    }
    let mut named = BTreeSet::new();
    for pair in pairs {
        let points = pair.from.iter().chain(std::iter::once(&pair.to));
        for point in points {
            git::validate_oid(&point.head_oid)?;
            named.insert(point.head_oid.as_str());
            if let Some(fork) = &point.fork_oid {
                git::validate_oid(fork)?;
                named.insert(fork.as_str());
            }
        }
    }
    let present = present_commits(root, &named)?;
    let has = |oid: &str| present.contains(oid);
    let mut wanted = Vec::new();
    for pair in pairs {
        if let Some(from) = &pair.from
            && let (Some(old_fork), Some(new_fork)) = (&from.fork_oid, &pair.to.fork_oid)
            && old_fork != new_fork
            && has(old_fork)
            && has(new_fork)
            && has(&from.head_oid)
            && from.head_oid != pair.to.head_oid
        {
            wanted.push((old_fork.clone(), new_fork.clone(), from.head_oid.clone()));
        }
    }
    let replayed = replays(root, disk, &wanted)?;
    let plans = plan(pairs, &present, &replayed);
    // Rebased only, when the replay's tree is the head's: one tree lookup for every pair.
    let mut lookups: Vec<&str> = Vec::new();
    for (pair, plan) in pairs.iter().zip(&plans) {
        if let Plan::From {
            base,
            rebase_removed: true,
            ..
        } = plan
        {
            lookups.push(base);
            lookups.push(&pair.to.head_oid);
        }
    }
    let trees = trees_of(root, &lookups)?;
    let mut trees = trees.chunks(2);
    let mut deltas = Vec::with_capacity(pairs.len());
    for (pair, plan) in pairs.iter().zip(plans) {
        let delta = |kind, replay_oid, files| LayerDelta {
            pair: pair.clone(),
            kind,
            replay_oid,
            files,
        };
        deltas.push(match plan {
            Plan::Done(kind) => delta(kind, None, Vec::new()),
            Plan::From {
                base,
                rebase_removed,
                conflicts,
            } => {
                let same_tree = rebase_removed
                    && trees
                        .next()
                        .is_some_and(|pair| matches!(pair, [Some(a), Some(b)] if a == b));
                if same_tree && conflicts.is_empty() {
                    delta(DeltaKind::RebasedOnly, Some(base), Vec::new())
                } else {
                    let fork = pair.to.fork_oid.as_deref().filter(|_| rebase_removed);
                    let mut files = delta_files(root, disk, fork, &base, &pair.to.head_oid)?;
                    if !conflicts.is_empty() {
                        files.retain(|f| !conflicts.contains(&f.path));
                        files.extend(conflicts.into_iter().map(|path| DeltaFile {
                            path,
                            since: FileSince::ReplayConflict,
                        }));
                        files.sort_by(|a, b| a.path.cmp(&b.path));
                    }
                    let kind = if rebase_removed {
                        DeltaKind::Changed
                    } else {
                        DeltaKind::WithRebase
                    };
                    delta(kind, Some(base), files)
                }
            }
        });
    }
    Ok(deltas)
}

/// The files that differ from `base` to `head`, with how; with `fork` (the current version's
/// fork point, `base` being the baseline replayed on it), files new to or dropped from the
/// layer's change are told apart from changed ones. Cached.
fn delta_files(
    root: &Path,
    disk: &ReviewCache,
    fork: Option<&str>,
    base: &str,
    head: &str,
) -> Result<Vec<DeltaFile>, DataError> {
    let key = format!("{} {base} {head}", fork.unwrap_or("-"));
    if let Some(entry) = disk
        .get_many(CacheKind::Delta, std::slice::from_ref(&key))
        .get(&key)
        && let Ok(files) = serde_json::from_str::<Vec<DeltaFile>>(&entry.value)
    {
        return Ok(files);
    }
    let numstat = git::output(
        root,
        &[
            "diff-tree",
            "-r",
            "--no-renames",
            "--numstat",
            "-z",
            base,
            head,
        ],
    )?;
    let names = |from: &str, to: &str| -> Result<BTreeSet<String>, DataError> {
        let output = git::output(
            root,
            &[
                "diff-tree",
                "-r",
                "--no-renames",
                "--name-only",
                "-z",
                from,
                to,
            ],
        )?;
        Ok(git::split_nul(&output).into_iter().collect())
    };
    let (before, now) = match fork {
        Some(fork) => (Some(names(fork, base)?), Some(names(fork, head)?)),
        None => (None, None),
    };
    let mut files: Vec<DeltaFile> = parse_numstat(&numstat)
        .into_iter()
        .map(|(path, added, removed)| {
            let in_before = before.as_ref().is_none_or(|names| names.contains(&path));
            let in_now = now.as_ref().is_none_or(|names| names.contains(&path));
            let since = match (in_before, in_now) {
                (true, false) => FileSince::Dropped,
                (false, true) => FileSince::New { added, removed },
                _ => FileSince::Changed { added, removed },
            };
            DeltaFile { path, since }
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    if let Ok(json) = serde_json::to_string(&files) {
        disk.put_many(CacheKind::Delta, &[(key, json)]);
    }
    Ok(files)
}

/// `git diff-tree --numstat -z` output (no renames) as `(path, added, removed)`; binary files
/// count no lines.
fn parse_numstat(output: &[u8]) -> Vec<(String, u32, u32)> {
    git::split_nul(output)
        .into_iter()
        .filter_map(|record| {
            let mut fields = record.splitn(3, '\t');
            let added = fields.next()?.parse().unwrap_or(0);
            let removed = fields.next()?.parse().unwrap_or(0);
            let path = fields.next()?.trim_start_matches('\n').to_owned();
            (!path.is_empty()).then_some((path, added, removed))
        })
        .collect()
}

/// One hunk of a `git diff -U0`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ZeroHunk {
    pub(crate) old_start: u32,
    pub(crate) old_count: u32,
    pub(crate) new_start: u32,
    pub(crate) new_count: u32,
    pub(crate) removed: Vec<String>,
}

impl ZeroHunk {
    /// The last line before the hunk on a side: with no lines, the start names the line the
    /// change comes after.
    const fn before(start: u32, count: u32) -> u32 {
        if count == 0 {
            start
        } else {
            start.saturating_sub(1)
        }
    }

    pub(crate) const fn new_before(&self) -> u32 {
        Self::before(self.new_start, self.new_count)
    }

    const fn old_before(&self) -> u32 {
        Self::before(self.old_start, self.old_count)
    }
}

/// Parse `"-a,b +c,d"` from a hunk header.
fn parse_range(range: &str) -> Option<(u32, u32)> {
    let (start, count) = range.split_once(',').unwrap_or((range, "1"));
    Some((start.parse().ok()?, count.parse().ok()?))
}

/// The hunks of the file section of `output` (a `git diff -U0` limited to a file's paths) whose
/// new side is one of `paths` (the first that has one); none when no section names them.
fn zero_hunks(output: &str, paths: &[&str]) -> Vec<ZeroHunk> {
    let mut sections = zero_hunk_sections(output);
    paths
        .iter()
        .find_map(|path| {
            sections
                .iter()
                .position(|(name, _)| name == path)
                .map(|index| sections.swap_remove(index).1)
        })
        .unwrap_or_default()
}

/// One file section of a `git diff -U0`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ZeroSection {
    /// The new-side path (empty for a deleted file).
    pub(crate) path: String,
    /// The old-side path of a rename.
    pub(crate) renamed_from: Option<String>,
    /// git said "Binary files … differ": the content changed, without hunks.
    pub(crate) binary: bool,
    pub(crate) hunks: Vec<ZeroHunk>,
}

/// Every file section of `output` (a `git diff -U0`): its new-side path and its hunks.
pub(crate) fn zero_hunk_sections(output: &str) -> Vec<(String, Vec<ZeroHunk>)> {
    zero_sections(output)
        .into_iter()
        .map(|section| (section.path, section.hunks))
        .collect()
}

/// Every file section of `output` (a `git diff -U0`, with or without rename detection).
///
/// Paths are read as git prints them: a name with a space ends its `+++` line with a tab, and a
/// name with a quote, backslash, or control character is C-quoted (`core.quotepath=false` keeps
/// other names as they are).
pub(crate) fn zero_sections(output: &str) -> Vec<ZeroSection> {
    let mut sections: Vec<ZeroSection> = Vec::new();
    let mut in_header = false;
    for line in output.lines() {
        if line.starts_with("diff --git ") {
            sections.push(ZeroSection::default());
            in_header = true;
            continue;
        }
        let Some(section) = sections.last_mut() else {
            continue;
        };
        if in_header {
            if let Some(path) = line.strip_prefix("+++ ") {
                if let Some(path) = header_path(path, "b/") {
                    section.path = path;
                }
            } else if let Some(path) = line.strip_prefix("rename to ") {
                section.path = unquote(path);
            } else if let Some(path) = line.strip_prefix("rename from ") {
                section.renamed_from = Some(unquote(path));
            } else if let Some(pair) = line
                .strip_prefix("Binary files ")
                .and_then(|rest| rest.strip_suffix(" differ"))
            {
                section.binary = true;
                if section.path.is_empty()
                    && let Some(path) = binary_new_path(pair)
                {
                    section.path = path;
                }
            }
            if !line.starts_with("@@") {
                continue;
            }
            in_header = false;
        }
        if let Some(header) = line.strip_prefix("@@ ") {
            let mut parts = header.split(' ');
            let old = parts.next().and_then(|old| old.strip_prefix('-'));
            let new = parts.next().and_then(|new| new.strip_prefix('+'));
            if let (Some((old_start, old_count)), Some((new_start, new_count))) =
                (old.and_then(parse_range), new.and_then(parse_range))
            {
                section.hunks.push(ZeroHunk {
                    old_start,
                    old_count,
                    new_start,
                    new_count,
                    removed: Vec::new(),
                });
            }
        } else if let Some(text) = line.strip_prefix('-')
            && let Some(hunk) = section.hunks.last_mut()
        {
            hunk.removed.push(text.to_owned());
        }
    }
    sections
}

/// The path of a `---`/`+++` header value without its `prefix` (`a/` or `b/`): `None` for
/// `/dev/null`. git ends the value with a tab when the name has a space.
fn header_path(value: &str, prefix: &str) -> Option<String> {
    let value = value.strip_suffix('\t').unwrap_or(value);
    if value == "/dev/null" {
        return None;
    }
    let path = unquote(value);
    Some(
        path.strip_prefix(prefix)
            .map_or_else(|| path.clone(), str::to_owned),
    )
}

/// The new-side path of a `Binary files A and B differ` pair: `B` without `b/`, when `A` names
/// the same path (or `/dev/null`, a new file). Names containing " and " are told apart by
/// trying every split.
fn binary_new_path(pair: &str) -> Option<String> {
    pair.match_indices(" and ").find_map(|(at, separator)| {
        let (old, new) = (&pair[..at], &pair[at + separator.len()..]);
        let new = header_path(new, "b/")?;
        let same = old == "/dev/null" || header_path(old, "a/").is_some_and(|old| old == new);
        same.then_some(new)
    })
}

/// A path as git prints it: C-quoted (`"…"` with backslash escapes and octal bytes) or as is.
fn unquote(text: &str) -> String {
    let Some(inner) = text
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return text.to_owned();
    };
    let mut bytes = Vec::with_capacity(inner.len());
    let mut chars = inner.bytes().peekable();
    while let Some(byte) = chars.next() {
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        let Some(escape) = chars.next() else {
            bytes.push(b'\\');
            break;
        };
        bytes.push(match escape {
            b'a' => 0x07,
            b'b' => 0x08,
            b't' => b'\t',
            b'n' => b'\n',
            b'v' => 0x0b,
            b'f' => 0x0c,
            b'r' => b'\r',
            b'0'..=b'7' => {
                let mut value = u32::from(escape - b'0');
                for _ in 0..2 {
                    match chars.peek() {
                        Some(digit @ b'0'..=b'7') => {
                            value = value * 8 + u32::from(digit - b'0');
                            chars.next();
                        }
                        _ => break,
                    }
                }
                u8::try_from(value).unwrap_or(u8::MAX)
            }
            other => other,
        });
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Where new-side line `line` of `hunks` (old → new) is on the old side: `None` when the hunks
/// added it.
fn back_to_old(hunks: &[ZeroHunk], line: u32) -> Option<u32> {
    let mut shift: i64 = 0;
    for hunk in hunks {
        let before = hunk.new_before();
        if line <= before {
            break;
        }
        if line <= before + hunk.new_count {
            return None;
        }
        shift += i64::from(hunk.new_count) - i64::from(hunk.old_count);
    }
    u32::try_from(i64::from(line) - shift).ok()
}

/// Add `line` to sorted, disjoint `spans`, merging with the last when adjacent.
fn push_line(spans: &mut Vec<LineSpan>, line: u32) {
    match spans.last_mut() {
        Some((_, end)) if *end + 1 == line => *end = line,
        Some((_, end)) if *end >= line => {}
        _ => spans.push((line, line)),
    }
}

/// One file's marks against the baseline replayed as `replay`.
///
/// Which lines of the current diff (`fork → head`) are new since the baseline, which of its
/// removed lines are newly removed, and what of the baseline's own change is gone.
///
/// # Errors
///
/// * [`DataError::InvalidRequest`] for a malformed commit id; [`DataError::PathRejected`] for an
///   unconfined path; [`DataError::Git`] when git fails.
pub fn marks(
    root: &Path,
    disk: &ReviewCache,
    request: &ReviewCompareMarksRequest,
) -> Result<ReviewCompareMarksResponse, DataError> {
    for oid in [&request.fork_oid, &request.replay_oid, &request.head_oid] {
        git::validate_oid(oid)?;
    }
    git::validate_relative_path(&request.path)?;
    if let Some(old) = &request.old_path {
        git::validate_relative_path(old)?;
    }
    // The leading tag names the shape of the cached answer.
    let key = format!(
        "was {} {} {} {}\u{1}{}",
        request.fork_oid,
        request.replay_oid,
        request.head_oid,
        request.path,
        request.old_path.as_deref().unwrap_or_default()
    );
    if let Some(entry) = disk
        .get_many(CacheKind::Marks, std::slice::from_ref(&key))
        .get(&key)
        && let Ok(marks) = serde_json::from_str(&entry.value)
    {
        return Ok(marks);
    }
    let mut paths = vec![request.path.as_str()];
    if let Some(old) = request.old_path.as_deref()
        && old != request.path
    {
        paths.push(old);
    }
    let diff = |from: &str, to: &str| -> Result<String, DataError> {
        let mut args = vec![
            "diff",
            "-U0",
            "-M",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            from,
            to,
            "--",
        ];
        args.extend(paths.iter().copied());
        Ok(String::from_utf8_lossy(&git::output(root, &args)?).into_owned())
    };
    let since = diff(&request.replay_oid, &request.head_oid)?;
    let before = diff(&request.fork_oid, &request.replay_oid)?;
    let answer = if since.lines().count() + before.lines().count() > MAX_MARK_LINES {
        ReviewCompareMarksResponse {
            too_large: true,
            ..ReviewCompareMarksResponse::default()
        }
    } else {
        let since = zero_hunks(&since, &[request.path.as_str()]);
        let before = zero_hunks(&before, &paths);
        marks_from(&since, &before)
    };
    if let Ok(json) = serde_json::to_string(&answer) {
        disk.put_many(CacheKind::Marks, &[(key, json)]);
    }
    Ok(answer)
}

/// Marks from the hunks of `replay → head` (`since`) and `fork → replay` (`before`).
fn marks_from(since: &[ZeroHunk], before: &[ZeroHunk]) -> ReviewCompareMarksResponse {
    let mut answer = ReviewCompareMarksResponse::default();
    for hunk in since {
        for line in hunk.new_start..hunk.new_start + hunk.new_count {
            push_line(&mut answer.new_lines, line);
        }
        let at = hunk.new_before() + 1;
        // Lines that replaced lines of the baseline pair with them in order, as a line diff
        // pairs a removed run with the added run after it.
        for (offset, text) in (0..hunk.new_count).zip(&hunk.removed) {
            if text.len() <= MAX_WAS_BYTES {
                answer.was.push((hunk.new_start + offset, text.clone()));
            }
        }
        let mut gone: Vec<String> = Vec::new();
        for (offset, text) in (0..hunk.old_count).zip(&hunk.removed) {
            let replay_line = hunk.old_before() + 1 + offset;
            match back_to_old(before, replay_line) {
                Some(fork_line) => push_line(&mut answer.newly_removed, fork_line),
                None => gone.push(text.clone()),
            }
        }
        if !gone.is_empty() {
            answer.removed_since.push(RemovedSince {
                before_new_line: at,
                lines: gone,
            });
        }
    }
    answer.newly_removed.sort_unstable();
    answer
}

/// Keep `keep`'s commits (those present) under [`LOOK_REF_NAMESPACE`] and drop the refs of
/// `release` there, in one `git update-ref --stdin`.
///
/// Refs of fetched versions (another namespace) are never touched. Returns how many refs were written and removed.
///
/// # Errors
///
/// * [`DataError::InvalidRequest`] for a malformed commit id; [`DataError::Git`] when git fails.
pub fn keep_versions(
    root: &Path,
    keep: &[String],
    release: &[String],
) -> Result<(usize, usize), DataError> {
    for oid in keep.iter().chain(release) {
        git::validate_oid(oid)?;
    }
    let named: BTreeSet<&str> = keep.iter().map(String::as_str).collect();
    let present = present_commits(root, &named)?;
    let release: BTreeSet<&str> = release
        .iter()
        .map(String::as_str)
        .filter(|oid| !named.contains(oid))
        .collect();
    let mut input = String::new();
    for oid in &present {
        let _ = writeln!(input, "update {LOOK_REF_NAMESPACE}/{oid} {oid}");
    }
    for oid in &release {
        let _ = writeln!(input, "delete {LOOK_REF_NAMESPACE}/{oid}");
    }
    if input.is_empty() {
        return Ok((0, 0));
    }
    git::output_with_input(root, &["update-ref", "--stdin"], &input)?;
    Ok((present.len(), release.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(old: (u32, u32), new: (u32, u32), removed: &[&str]) -> ZeroHunk {
        ZeroHunk {
            old_start: old.0,
            old_count: old.1,
            new_start: new.0,
            new_count: new.1,
            removed: removed.iter().map(|text| (*text).to_owned()).collect(),
        }
    }

    #[test]
    fn sections_read_paths_with_spaces_quotes_renames_and_binary_changes() {
        let output = "diff --git a/my file.txt b/my file.txt\n\
            index 1..2 100644\n\
            --- a/my file.txt\t\n\
            +++ b/my file.txt\t\n\
            @@ -1 +1 @@\n\
            -old\n\
            +new\n\
            diff --git \"a/tab\\there.txt\" \"b/tab\\there.txt\"\n\
            --- \"a/tab\\there.txt\"\n\
            +++ \"b/tab\\there.txt\"\n\
            @@ -0,0 +1 @@\n\
            +x\n\
            diff --git a/caf\u{e9}.txt b/caf\u{e9}.txt\n\
            --- \"a/caf\\303\\251.txt\"\n\
            +++ \"b/caf\\303\\251.txt\"\n\
            @@ -2,0 +3 @@\n\
            +y\n\
            diff --git a/old name.rs b/new name.rs\n\
            similarity index 100%\n\
            rename from old name.rs\n\
            rename to new name.rs\n\
            diff --git a/img and pic.png b/img and pic.png\n\
            index 1..2 100644\n\
            Binary files a/img and pic.png and b/img and pic.png differ\n\
            diff --git a/new.bin b/new.bin\n\
            new file mode 100644\n\
            Binary files /dev/null and b/new.bin differ\n\
            diff --git a/gone.rs b/gone.rs\n\
            deleted file mode 100644\n\
            --- a/gone.rs\n\
            +++ /dev/null\n\
            @@ -1 +0,0 @@\n\
            -z\n";
        let sections = zero_sections(output);
        let summary: Vec<(&str, Option<&str>, bool, usize)> = sections
            .iter()
            .map(|section| {
                (
                    section.path.as_str(),
                    section.renamed_from.as_deref(),
                    section.binary,
                    section.hunks.len(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("my file.txt", None, false, 1),
                ("tab\there.txt", None, false, 1),
                ("caf\u{e9}.txt", None, false, 1),
                ("new name.rs", Some("old name.rs"), false, 0),
                ("img and pic.png", None, true, 0),
                ("new.bin", None, true, 0),
                ("", None, false, 1),
            ]
        );
        assert_eq!(
            zero_hunks(output, &["my file.txt"]),
            [hunk((1, 1), (1, 1), &["old"])]
        );
    }

    #[test]
    fn lines_map_back_across_insertions_and_deletions() {
        // Two lines inserted after line 2, one line deleted at old line 6.
        let hunks = [hunk((2, 0), (3, 2), &[]), hunk((6, 1), (7, 0), &["x"])];
        assert_eq!(back_to_old(&hunks, 1), Some(1));
        assert_eq!(back_to_old(&hunks, 2), Some(2));
        assert_eq!(back_to_old(&hunks, 3), None);
        assert_eq!(back_to_old(&hunks, 4), None);
        assert_eq!(back_to_old(&hunks, 5), Some(3));
        assert_eq!(back_to_old(&hunks, 7), Some(5));
        assert_eq!(back_to_old(&hunks, 8), Some(7));
    }

    #[test]
    fn marks_tell_new_lines_newly_removed_lines_and_lines_removed_since() {
        // The baseline added lines 3–4 to the fork's file; since, line 3 (its own) and fork
        // line 6 (replay line 8) were removed, and one line was added after replay line 9.
        let before = [hunk((2, 0), (3, 2), &[])];
        let since = [
            hunk((3, 1), (2, 0), &["mine"]),
            hunk((8, 1), (6, 0), &["theirs"]),
            hunk((9, 0), (8, 1), &[]),
        ];
        let marks = marks_from(&since, &before);
        assert_eq!(marks.new_lines, [(8, 8)]);
        assert_eq!(marks.newly_removed, [(6, 6)]);
        assert_eq!(
            marks.removed_since,
            [RemovedSince {
                before_new_line: 3,
                lines: vec!["mine".into()],
            }]
        );
    }

    #[test]
    fn lines_that_replaced_baseline_lines_carry_the_baseline_text_in_order() {
        // Two replay lines replaced by three: the first two pair, the third is only new; a
        // pure insertion pairs with nothing; an overlong baseline line is not kept.
        let long = "x".repeat(MAX_WAS_BYTES + 1);
        let since = [
            hunk((3, 2), (3, 3), &["let a = 1;", "let b = 2;"]),
            hunk((9, 0), (10, 1), &[]),
            hunk((12, 1), (13, 1), &[long.as_str()]),
        ];
        let marks = marks_from(&since, &[]);
        assert_eq!(
            marks.was,
            [(3, "let a = 1;".to_owned()), (4, "let b = 2;".to_owned())]
        );
        assert_eq!(marks.new_lines, [(3, 5), (10, 10), (13, 13)]);
    }

    #[test]
    fn diff_sections_are_picked_by_new_path() {
        let output = "diff --git a/a.rs b/a.rs\nindex 1..2 100644\n--- a/a.rs\n+++ b/a.rs\n\
                      @@ -1 +1 @@\n-old\n+new\ndiff --git a/b.rs b/b.rs\n--- a/b.rs\n+++ b/b.rs\n\
                      @@ -4,0 +5,2 @@\n+x\n+y\n";
        assert_eq!(zero_hunks(output, &["b.rs"]), [hunk((4, 0), (5, 2), &[])]);
        assert_eq!(
            zero_hunks(output, &["c.rs", "a.rs"]),
            [hunk((1, 1), (1, 1), &["old"])]
        );
        assert_eq!(zero_hunks(output, &["c.rs"]), []);
        assert_eq!(
            parse_numstat(b"1\t2\ta.rs\0-\t-\tbin\0"),
            [("a.rs".into(), 1, 2), ("bin".into(), 0, 0)]
        );
    }
}
