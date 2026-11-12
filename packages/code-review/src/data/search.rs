//! Paged reference search, commit log, and path search.

use super::{DataError, decode_cursor, fingerprint, git, page, page_size};
use bcode_code_review_models::{
    REVIEW_SEARCH_DEFAULT_PAGE_SIZE, REVIEW_SEARCH_MAX_PAGE_SIZE, ReviewCommitEntry,
    ReviewCommitsPageRequest, ReviewCommitsPageResponse, ReviewFileSearchEntry,
    ReviewFilesSearchRequest, ReviewFilesSearchResponse, ReviewRefEntry, ReviewRefKind,
    ReviewRefsSearchRequest, ReviewRefsSearchResponse, RevisionRef,
};
use std::collections::BTreeSet;
use std::path::Path;

/// Fuzzy match score; lower is better. `None` means no match.
///
/// Exact matches rank first, then prefixes, then substrings (earlier and at word boundaries
/// first), then subsequences (tighter first). Matching is case-insensitive.
#[must_use]
pub fn fuzzy_score(candidate: &str, query: &str) -> Option<u64> {
    if query.is_empty() {
        return Some(0);
    }
    let candidate = candidate.to_lowercase();
    let query = query.to_lowercase();
    if candidate == query {
        return Some(0);
    }
    if candidate.starts_with(&query) {
        return Some(1);
    }
    if let Some(position) = candidate.find(&query) {
        let boundary = candidate[..position]
            .chars()
            .next_back()
            .is_some_and(|before| matches!(before, '/' | '-' | '_' | '.' | ' '));
        let position = u64::try_from(position).unwrap_or(u64::MAX);
        let base: u64 = if boundary { 2 } else { 100 };
        return Some(base.saturating_add(position.min(899)));
    }
    let mut chars = candidate.char_indices();
    let mut first = None;
    let mut last = 0;
    for wanted in query.chars() {
        let (index, _) = chars.find(|(_, character)| *character == wanted)?;
        first.get_or_insert(index);
        last = index;
    }
    let spread = u64::try_from(last - first.unwrap_or(0)).unwrap_or(u64::MAX);
    Some(1_000_u64.saturating_add(spread))
}

/// Filter and rank `items` by `fuzzy_score` of `key`, keeping ties in input order.
fn ranked<T>(items: Vec<T>, query: &str, key: impl Fn(&T) -> &str) -> Vec<T> {
    let mut scored: Vec<(u64, usize, T)> = items
        .into_iter()
        .filter_map(|item| {
            let score = fuzzy_score(key(&item), query)?;
            Some((score, key(&item).len(), item))
        })
        .collect();
    scored.sort_by_key(|(score, length, _)| (*score, *length));
    scored.into_iter().map(|(_, _, item)| item).collect()
}

/// Search branches, remote branches, and tags.
pub fn refs_search(
    root: &Path,
    request: &ReviewRefsSearchRequest,
) -> Result<ReviewRefsSearchResponse, DataError> {
    let output = git::output(
        root,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(*objectname)%00%(committerdate:unix)%00%(*committerdate:unix)%00%(HEAD)%00%(contents:subject)",
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ],
    )?;
    let kinds: BTreeSet<ReviewRefKind> = request.kinds.iter().copied().collect();
    let refs: Vec<ReviewRefEntry> = String::from_utf8_lossy(&output)
        .lines()
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('\0').collect();
            let [full_name, oid, peeled, date, peeled_date, head, subject] = fields.as_slice()
            else {
                return None;
            };
            let (kind, name) = if let Some(name) = full_name.strip_prefix("refs/heads/") {
                (ReviewRefKind::Branch, name)
            } else if let Some(name) = full_name.strip_prefix("refs/remotes/") {
                if name.ends_with("/HEAD") {
                    return None;
                }
                (ReviewRefKind::RemoteBranch, name)
            } else {
                (ReviewRefKind::Tag, full_name.strip_prefix("refs/tags/")?)
            };
            if !kinds.is_empty() && !kinds.contains(&kind) {
                return None;
            }
            let oid = if peeled.is_empty() { oid } else { peeled };
            let date = if peeled_date.is_empty() {
                date
            } else {
                peeled_date
            };
            Some(ReviewRefEntry {
                name: name.to_string(),
                full_name: (*full_name).to_string(),
                kind,
                oid: (*oid).to_string(),
                subject: (*subject).to_string(),
                committed_at_s: date.parse().ok(),
                is_head: *head == "*",
            })
        })
        .collect();
    let mut matches = ranked(refs, &request.query, |entry| entry.name.as_str());
    // Stable secondary order: kind, so local branches precede remotes and tags on ties.
    matches.sort_by_key(|entry| {
        (
            fuzzy_score(&entry.name, &request.query).unwrap_or(u64::MAX),
            entry.kind,
        )
    });
    let kinds_key = format!("{kinds:?}");
    let fingerprint = fingerprint(&[request.query.as_bytes(), kinds_key.as_bytes(), &output]);
    let offset = decode_cursor(request.cursor.as_deref(), &fingerprint)?;
    let size = page_size(
        request.limit,
        REVIEW_SEARCH_DEFAULT_PAGE_SIZE,
        REVIEW_SEARCH_MAX_PAGE_SIZE,
    );
    let (refs, next_cursor) = page(&matches, offset, size, &fingerprint)?;
    Ok(ReviewRefsSearchResponse {
        refs,
        next_cursor,
        total_matches: u32::try_from(matches.len()).unwrap_or(u32::MAX),
    })
}

/// One page of `git log`, anchored to the commit the first page started from.
///
/// The cursor is `START_OID.SKIP.QUERY_FINGERPRINT`, so later pages are stable even when the
/// branch moves.
pub fn commits_page(
    root: &Path,
    request: &ReviewCommitsPageRequest,
) -> Result<ReviewCommitsPageResponse, DataError> {
    let query = request.query.clone().unwrap_or_default();
    let query_fingerprint = fingerprint(&[query.as_bytes()]);
    let (start, skip) = if let Some(cursor) = &request.cursor {
        parse_commit_cursor(cursor, &query_fingerprint)?
    } else {
        let rev = request.rev.as_deref().unwrap_or("HEAD");
        match git::resolve_commit(root, rev)? {
            Some(oid) => (oid, 0),
            None if request.rev.is_none() => {
                // Unborn branch: no history yet.
                return Ok(ReviewCommitsPageResponse {
                    commits: Vec::new(),
                    next_cursor: None,
                });
            }
            None => return Err(DataError::NotFound(format!("revision {rev} not found"))),
        }
    };
    let size = page_size(
        request.limit,
        REVIEW_SEARCH_DEFAULT_PAGE_SIZE,
        REVIEW_SEARCH_MAX_PAGE_SIZE,
    );
    let skip_arg = format!("--skip={skip}");
    let count_arg = format!("--max-count={}", size + 1);
    let grep_arg = format!("--grep={query}");
    let mut args = vec![
        "log",
        "-z",
        "--format=%H%x1f%h%x1f%an%x1f%ae%x1f%at%x1f%D%x1f%P%x1f%s",
        skip_arg.as_str(),
        count_arg.as_str(),
    ];
    if !query.is_empty() {
        args.extend(["--regexp-ignore-case", "--fixed-strings", grep_arg.as_str()]);
    }
    args.extend(["--end-of-options", start.as_str(), "--"]);
    let output = git::output(root, &args)?;
    let mut commits: Vec<ReviewCommitEntry> = git::split_nul(&output)
        .into_iter()
        .filter_map(|record| {
            let fields: Vec<&str> = record.trim_start_matches('\n').split('\x1f').collect();
            let [oid, short, name, email, time, refs, parents, subject] = fields.as_slice() else {
                return None;
            };
            Some(ReviewCommitEntry {
                oid: (*oid).to_string(),
                short_oid: (*short).to_string(),
                subject: (*subject).to_string(),
                author_name: (*name).to_string(),
                author_email: (*email).to_string(),
                authored_at_s: time.parse().unwrap_or_default(),
                refs: refs
                    .split(", ")
                    .filter(|decoration| !decoration.is_empty())
                    .map(str::to_string)
                    .collect(),
                parents: parents.split_whitespace().map(str::to_string).collect(),
            })
        })
        .collect();
    let next_cursor = (commits.len() > size).then(|| {
        commits.truncate(size);
        format!("{start}.{}.{query_fingerprint}", skip + size)
    });
    Ok(ReviewCommitsPageResponse {
        commits,
        next_cursor,
    })
}

/// Decode a `START_OID.SKIP.QUERY_FINGERPRINT` commit cursor.
fn parse_commit_cursor(
    cursor: &str,
    query_fingerprint: &str,
) -> Result<(String, usize), DataError> {
    let malformed = || DataError::InvalidRequest(format!("malformed cursor: {cursor:?}"));
    let mut parts = cursor.splitn(3, '.');
    let (Some(start), Some(skip), Some(expected)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(malformed());
    };
    git::validate_oid(start)?;
    if expected != query_fingerprint {
        return Err(DataError::CursorStale);
    }
    let skip = skip.parse::<usize>().map_err(|_| malformed())?;
    Ok((start.to_string(), skip))
}

/// Search repository paths at a revision (default: tracked files in the index).
pub fn files_search(
    root: &Path,
    request: &ReviewFilesSearchRequest,
) -> Result<ReviewFilesSearchResponse, DataError> {
    let output = match request.revision.as_ref().unwrap_or(&RevisionRef::Index) {
        RevisionRef::Index => git::output(root, &["ls-files", "-z", "--cached"])?,
        RevisionRef::WorkingTree => git::output(
            root,
            &[
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ],
        )?,
        RevisionRef::Commit { oid } => {
            git::validate_oid(oid)?;
            git::output(
                root,
                &[
                    "ls-tree",
                    "-r",
                    "-z",
                    "--name-only",
                    "--full-tree",
                    oid.as_str(),
                ],
            )?
        }
        RevisionRef::EmptyTree => Vec::new(),
    };
    let mut seen = BTreeSet::new();
    let paths: Vec<String> = git::split_nul(&output)
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .collect();
    let matches = ranked(paths, &request.query, String::as_str);
    let revision_key = format!("{:?}", request.revision);
    let fingerprint = fingerprint(&[request.query.as_bytes(), revision_key.as_bytes(), &output]);
    let offset = decode_cursor(request.cursor.as_deref(), &fingerprint)?;
    let size = page_size(
        request.limit,
        REVIEW_SEARCH_DEFAULT_PAGE_SIZE,
        REVIEW_SEARCH_MAX_PAGE_SIZE,
    );
    let (paths, next_cursor) = page(&matches, offset, size, &fingerprint)?;
    Ok(ReviewFilesSearchResponse {
        files: paths
            .into_iter()
            .map(|path| ReviewFileSearchEntry { path })
            .collect(),
        next_cursor,
        total_matches: u32::try_from(matches.len()).unwrap_or(u32::MAX),
    })
}
