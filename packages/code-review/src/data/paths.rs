//! The repository's path index: what the path picker, `find_paths`, and selector counts read.
//!
//! One index lists the repository as it is now: tracked files, untracked files that are not
//! ignored, and, when asked, ignored files with ignored folders collapsed to one entry each (an
//! ignored `target/` costs one entry, not millions). It holds at most
//! [`REVIEW_PATH_INDEX_MAX_PATHS`] paths; beyond that it is marked truncated.
//!
//! Indexes are kept per repository and ignored-files choice, at most [`MAX_CACHED_INDEXES`],
//! and reused for at most [`REUSE_FOR`] and only while the git index file is unchanged (a
//! commit, checkout, or staging rewrites it). Callers that must see the working tree as it is
//! now (the picker opening, a relist) pass `refresh`.

use super::{DataError, decode_cursor, fingerprint, git, page, page_size, search::fuzzy_score};
use bcode_code_review_models::{
    PathSelector, REVIEW_PATH_INDEX_MAX_PATHS, REVIEW_PATHS_FIND_DEFAULT_LIMIT,
    REVIEW_PATHS_FIND_MAX_LIMIT, REVIEW_SEARCH_DEFAULT_PAGE_SIZE, REVIEW_SEARCH_MAX_PAGE_SIZE,
    ReviewPathEntry, ReviewPathKind, ReviewPathMatch, ReviewPathMatchKind, ReviewPathStatus,
    ReviewPathsFindRequest, ReviewPathsFindResponse, ReviewPathsListRequest,
    ReviewPathsListResponse, ReviewPathsResolveRequest, ReviewPathsResolveResponse,
    ReviewPathsSearchRequest, ReviewPathsSearchResponse, ReviewSelectorCount,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

/// Repositories (times the ignored-files choice) whose index is kept.
const MAX_CACHED_INDEXES: usize = 8;
/// How long an index is reused at most.
const REUSE_FOR: Duration = Duration::from_secs(2);
/// Package manifests read at most for `find_paths`.
const MAX_MANIFESTS: usize = 2_000;
/// Manifests larger than this are skipped.
const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
/// Manifest file names, each with how its package name is read.
const MANIFESTS: [&str; 3] = ["Cargo.toml", "package.json", "pyproject.toml"];
/// Leading query words that name nothing.
const STOP_WORDS: [&str; 7] = ["the", "a", "an", "my", "our", "this", "that"];
/// Trailing query words that say what kind of thing is meant.
const KIND_WORDS: [&str; 7] = [
    "folder",
    "directory",
    "dir",
    "package",
    "crate",
    "module",
    "file",
];
/// Score of an exact match.
const SCORE_EXACT: u32 = 100;
/// How much a folder match ranks below a file or package match of the same quality.
const FOLDER_PENALTY: u32 = 5;
/// Matches scoring at least this, within [`AMBIGUOUS_WITHIN`] of each other, are ambiguous.
const AMBIGUOUS_FROM: u32 = 80;
/// See [`AMBIGUOUS_FROM`].
const AMBIGUOUS_WITHIN: u32 = 5;

/// One indexed file.
#[derive(Debug, Clone)]
struct IndexedFile {
    path: String,
    status: ReviewPathStatus,
    changed: bool,
}

/// Counts of the files under one folder.
#[derive(Debug, Clone, Copy, Default)]
struct FolderCount {
    files: u32,
    tracked: u32,
    changed: u32,
}

/// A package found by its manifest.
#[derive(Debug, Clone)]
struct Package {
    /// The package's folder, without a trailing `/`.
    folder: String,
    name: String,
    manifest: String,
}

/// The repository's paths as they are now.
#[derive(Debug, Default)]
pub struct PathIndex {
    /// Files, sorted by path.
    files: Vec<IndexedFile>,
    /// Ignored folders listed as one entry each, sorted, without a trailing `/`.
    ignored_folders: Vec<String>,
    /// Every folder with files or ignored folders under it, the root as `""`.
    folders: BTreeMap<String, FolderCount>,
    truncated: bool,
    /// Changes whenever the listing does: binds list cursors to it.
    generation: String,
    packages: OnceLock<Vec<Package>>,
}

impl PathIndex {
    /// List the repository at `root`.
    fn build(root: &Path, include_ignored: bool) -> Result<Self, DataError> {
        let mut budget = REVIEW_PATH_INDEX_MAX_PATHS;
        let mut truncated = false;
        let mut run = |args: &[&str]| -> Result<git::LimitedRecords, DataError> {
            let listed = git::limited_records(git::command(root, args), budget)?;
            budget = budget.saturating_sub(listed.records.len());
            truncated |= listed.total > listed.records.len() as u64;
            Ok(listed)
        };
        let tracked = run(&["ls-files", "-z", "--cached"])?;
        let untracked = run(&["ls-files", "-z", "--others", "--exclude-standard"])?;
        let ignored = if include_ignored {
            run(&[
                "ls-files",
                "-z",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
            ])?
        } else {
            git::LimitedRecords::default()
        };
        let base = match git::resolve_commit(root, "HEAD")? {
            Some(head) => head,
            None => git::empty_tree_oid(root)?,
        };
        let changed: BTreeSet<String> = git::split_nul(&git::output(
            root,
            &["diff", "--name-only", "-z", "--no-renames", base.as_str()],
        )?)
        .into_iter()
        .collect();
        let mut files: BTreeMap<String, IndexedFile> = BTreeMap::new();
        for (records, status) in [
            (&tracked.records, ReviewPathStatus::Tracked),
            (&untracked.records, ReviewPathStatus::Untracked),
            (&ignored.records, ReviewPathStatus::Ignored),
        ] {
            for path in records {
                if path.ends_with('/') || files.contains_key(path) {
                    continue;
                }
                files.insert(
                    path.clone(),
                    IndexedFile {
                        path: path.clone(),
                        status,
                        changed: status == ReviewPathStatus::Untracked || changed.contains(path),
                    },
                );
            }
        }
        let ignored_folders: Vec<String> = ignored
            .records
            .iter()
            .filter_map(|path| path.strip_suffix('/'))
            .map(str::to_owned)
            .collect();
        let mut folders: BTreeMap<String, FolderCount> = BTreeMap::new();
        folders.insert(String::new(), FolderCount::default());
        for file in files.values() {
            for folder in ancestors(&file.path) {
                let count = folders.entry(folder.to_owned()).or_default();
                count.files = count.files.saturating_add(1);
                count.tracked += u32::from(file.status == ReviewPathStatus::Tracked);
                count.changed += u32::from(file.changed);
            }
        }
        for folder in &ignored_folders {
            folders.entry(folder.clone()).or_default();
            for ancestor in ancestors(folder) {
                folders.entry(ancestor.to_owned()).or_default();
            }
        }
        Ok(Self {
            files: files.into_values().collect(),
            ignored_folders,
            folders,
            truncated,
            generation: fingerprint(&[&tracked.digest, &untracked.digest, &ignored.digest]),
            packages: OnceLock::new(),
        })
    }

    fn file(&self, path: &str) -> Option<&IndexedFile> {
        self.files
            .binary_search_by(|file| file.path.as_str().cmp(path))
            .ok()
            .map(|index| &self.files[index])
    }

    /// Files under `folder` (`""` for every file), in path order.
    fn files_under<'a>(&'a self, folder: &str) -> impl Iterator<Item = &'a IndexedFile> + 'a {
        let prefix = if folder.is_empty() {
            String::new()
        } else {
            format!("{folder}/")
        };
        let start = self
            .files
            .partition_point(|file| file.path.as_str() < prefix.as_str());
        self.files[start..]
            .iter()
            .take_while(move |file| file.path.starts_with(&prefix))
    }

    /// Whether `folder` is an ignored folder or inside one.
    fn in_ignored_folder(&self, folder: &str) -> bool {
        std::iter::once(folder)
            .chain(ancestors(folder))
            .any(|candidate| {
                self.ignored_folders
                    .iter()
                    .any(|ignored| ignored == candidate)
            })
    }

    fn folder_entry(&self, path: &str) -> ReviewPathEntry {
        let count = self.folders.get(path).copied().unwrap_or_default();
        let ignored = self.in_ignored_folder(path);
        ReviewPathEntry {
            name: base_name(path).to_owned(),
            path: format!("{path}/"),
            kind: ReviewPathKind::Folder,
            files: (!ignored).then_some(count.files),
            changed: count.changed,
            status: if ignored {
                ReviewPathStatus::Ignored
            } else if count.tracked == 0 && count.files > 0 {
                ReviewPathStatus::Untracked
            } else {
                ReviewPathStatus::Tracked
            },
        }
    }

    fn file_entry(file: &IndexedFile) -> ReviewPathEntry {
        ReviewPathEntry {
            name: base_name(&file.path).to_owned(),
            path: file.path.clone(),
            kind: ReviewPathKind::File,
            files: Some(1),
            changed: u32::from(file.changed),
            status: file.status,
        }
    }

    /// The children of `dir`: folders first, then files, each by name.
    fn children(&self, root: &Path, dir: &str) -> Vec<ReviewPathEntry> {
        if !dir.is_empty() && self.in_ignored_folder(dir) {
            return ignored_children(root, dir);
        }
        let mut folders: BTreeSet<String> = BTreeSet::new();
        let mut files = Vec::new();
        let prefix_len = if dir.is_empty() { 0 } else { dir.len() + 1 };
        for file in self.files_under(dir) {
            let rest = &file.path[prefix_len..];
            match rest.split_once('/') {
                Some((folder, _)) => {
                    folders.insert(join(dir, folder));
                }
                None => files.push(Self::file_entry(file)),
            }
        }
        for folder in &self.ignored_folders {
            if parent(folder) == dir {
                folders.insert(folder.clone());
            }
        }
        folders
            .iter()
            .map(|folder| self.folder_entry(folder))
            .chain(files)
            .collect()
    }

    /// Whether `selector` selects the indexed file `path`.
    fn selects(selector: &PathSelector, glob: Option<&globset::GlobMatcher>, path: &str) -> bool {
        crate::selectors::selects(selector, glob, path)
    }

    fn packages(&self, root: &Path) -> &[Package] {
        self.packages.get_or_init(|| {
            self.files
                .iter()
                .filter(|file| MANIFESTS.contains(&base_name(&file.path)))
                .take(MAX_MANIFESTS)
                .filter_map(|file| {
                    let folder = parent(&file.path);
                    // The root package is the whole repository, not a folder of it.
                    if folder.is_empty() {
                        return None;
                    }
                    let name = package_name(root, &file.path)?;
                    Some(Package {
                        folder: folder.to_owned(),
                        name,
                        manifest: file.path.clone(),
                    })
                })
                .collect()
        })
    }
}

/// `path`'s folders, nearest first, ending with the root `""`.
fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    let mut current = Some(path);
    std::iter::from_fn(move || {
        let path = current.take().filter(|path| !path.is_empty())?;
        let folder = parent(path);
        current = Some(folder);
        Some(folder)
    })
}

/// The folder of `path` (`""` at the root).
fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(folder, _)| folder)
}

fn base_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// The children of a folder inside an ignored folder, read from the file system (git does not
/// list them), in the same order as indexed children.
fn ignored_children(root: &Path, dir: &str) -> Vec<ReviewPathEntry> {
    let Some(full) = working_tree_dir(root, dir) else {
        return Vec::new();
    };
    let Ok(read) = std::fs::read_dir(full) else {
        return Vec::new();
    };
    let mut folders = Vec::new();
    let mut files = Vec::new();
    for entry in read.flatten().take(REVIEW_PATH_INDEX_MAX_PATHS) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        let path = join(dir, &name);
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        let item = ReviewPathEntry {
            name,
            path: if is_dir { format!("{path}/") } else { path },
            kind: if is_dir {
                ReviewPathKind::Folder
            } else {
                ReviewPathKind::File
            },
            files: (!is_dir).then_some(1),
            changed: 0,
            status: ReviewPathStatus::Ignored,
        };
        if is_dir {
            folders.push(item);
        } else {
            files.push(item);
        }
    }
    folders.sort_by(|a, b| a.name.cmp(&b.name));
    files.sort_by(|a, b| a.name.cmp(&b.name));
    folders.extend(files);
    folders
}

/// The metadata of the working-tree file `path` when it is a regular file or a link whose
/// folder stays inside `root`.
pub fn working_tree_file(root: &Path, path: &str) -> Option<std::fs::Metadata> {
    let full = root.join(path);
    let metadata = std::fs::symlink_metadata(&full).ok()?;
    if !(metadata.is_file() || metadata.is_symlink()) {
        return None;
    }
    inside(root, full.parent()?).then_some(metadata)
}

/// The working-tree folder `path` when it is a folder that stays inside `root`.
fn working_tree_dir(root: &Path, path: &str) -> Option<PathBuf> {
    let full = root.join(path);
    (full.is_dir() && inside(root, &full)).then_some(full)
}

fn inside(root: &Path, path: &Path) -> bool {
    match (std::fs::canonicalize(path), std::fs::canonicalize(root)) {
        (Ok(path), Ok(root)) => path.starts_with(root),
        _ => false,
    }
}

/// The size and modification time of the repository's git index file, when it can be found
/// without running git: what an index is reused under.
pub fn index_stamp(root: &Path) -> Option<(u64, SystemTime)> {
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let text = std::fs::read_to_string(&dot_git).ok()?;
        let dir = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
        if dir.is_absolute() {
            dir
        } else {
            root.join(dir)
        }
    };
    let metadata = std::fs::metadata(git_dir.join("index")).ok()?;
    Some((metadata.len(), metadata.modified().ok()?))
}

/// A package name from a manifest: `[package] name` of a `Cargo.toml`, `name` of a
/// `package.json`, `[project] name` (or `[tool.poetry] name`) of a `pyproject.toml`.
fn package_name(root: &Path, manifest: &str) -> Option<String> {
    let metadata = working_tree_file(root, manifest)?;
    if metadata.len() > MAX_MANIFEST_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(root.join(manifest)).ok()?;
    let name = match base_name(manifest) {
        "package.json" => serde_json::from_str::<serde_json::Value>(&text)
            .ok()?
            .get("name")?
            .as_str()?
            .to_owned(),
        "Cargo.toml" => text
            .parse::<toml::Table>()
            .ok()?
            .get("package")?
            .get("name")?
            .as_str()?
            .to_owned(),
        _ => {
            let table = text.parse::<toml::Table>().ok()?;
            table
                .get("project")
                .and_then(|project| project.get("name"))
                .or_else(|| {
                    table
                        .get("tool")
                        .and_then(|tool| tool.get("poetry"))
                        .and_then(|poetry| poetry.get("name"))
                })?
                .as_str()?
                .to_owned()
        }
    };
    (!name.trim().is_empty()).then(|| name.trim().to_owned())
}

/// Path indexes kept between calls.
#[derive(Debug, Default)]
pub struct PathIndexCache {
    entries: Mutex<VecDeque<CachedIndex>>,
}

#[derive(Debug)]
struct CachedIndex {
    root: PathBuf,
    include_ignored: bool,
    built: Instant,
    stamp: Option<(u64, SystemTime)>,
    index: Arc<PathIndex>,
}

impl PathIndexCache {
    /// The index of `root`: a recent one, unless `refresh`, else a new one.
    fn get(
        &self,
        root: &Path,
        include_ignored: bool,
        refresh: bool,
    ) -> Result<Arc<PathIndex>, DataError> {
        let stamp = index_stamp(root);
        let recent = (!refresh)
            .then(|| {
                lock(&self.entries)
                    .iter()
                    .find(|cached| {
                        cached.root == root
                            && cached.include_ignored == include_ignored
                            && cached.built.elapsed() < REUSE_FOR
                            && cached.stamp.is_some()
                            && cached.stamp == stamp
                    })
                    .map(|cached| Arc::clone(&cached.index))
            })
            .flatten();
        if let Some(index) = recent {
            return Ok(index);
        }
        let index = Arc::new(PathIndex::build(root, include_ignored)?);
        self.keep(CachedIndex {
            root: root.to_path_buf(),
            include_ignored,
            built: Instant::now(),
            stamp,
            index: Arc::clone(&index),
        });
        Ok(index)
    }

    fn keep(&self, index: CachedIndex) {
        let mut entries = lock(&self.entries);
        entries.retain(|cached| {
            cached.root != index.root || cached.include_ignored != index.include_ignored
        });
        if entries.len() >= MAX_CACHED_INDEXES {
            entries.pop_front();
        }
        entries.push_back(index);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One folder's children (`review.paths.list`).
pub fn list(
    cache: &PathIndexCache,
    root: &Path,
    request: &ReviewPathsListRequest,
) -> Result<ReviewPathsListResponse, DataError> {
    let dir = request.dir.trim_matches('/');
    if !dir.is_empty() {
        git::validate_relative_path(dir)?;
    }
    let index = cache.get(root, request.include_ignored, request.refresh)?;
    if !dir.is_empty() && !index.folders.contains_key(dir) && !index.in_ignored_folder(dir) {
        return Err(DataError::NotFound(format!("{dir}/ has no files")));
    }
    let entries = index.children(root, dir);
    let generation = fingerprint(&[index.generation.as_bytes(), dir.as_bytes()]);
    let offset = decode_cursor(request.cursor.as_deref(), &generation)?;
    let size = page_size(
        request.limit,
        REVIEW_SEARCH_MAX_PAGE_SIZE,
        REVIEW_SEARCH_MAX_PAGE_SIZE,
    );
    let (page, next_cursor) = page(&entries, offset, size, &generation)?;
    Ok(ReviewPathsListResponse {
        entries: page,
        next_cursor,
        total_entries: u32::try_from(entries.len()).unwrap_or(u32::MAX),
        truncated: index.truncated,
    })
}

/// A glob matcher with git's `:(glob)` rules (`*` stays within a folder, `**` crosses them).
fn glob_matcher(pattern: &str) -> Result<globset::GlobMatcher, DataError> {
    git::validate_glob(pattern)?;
    crate::selectors::glob_matcher(pattern).map_err(DataError::InvalidRequest)
}

/// Files and folders matching a fuzzy name or a glob (`review.paths.search`).
pub fn search(
    cache: &PathIndexCache,
    root: &Path,
    request: &ReviewPathsSearchRequest,
) -> Result<ReviewPathsSearchResponse, DataError> {
    let index = cache.get(root, request.include_ignored, request.refresh)?;
    let size = page_size(
        request.limit,
        REVIEW_SEARCH_DEFAULT_PAGE_SIZE,
        REVIEW_SEARCH_MAX_PAGE_SIZE,
    );
    let query = request.query.trim();
    let glob = matches!(
        PathSelector::parse(query),
        Ok(bcode_code_review_models::ParsedSelector::Selector(
            PathSelector::Glob { .. }
        ))
    );
    let (entries, total) = if glob {
        let matcher = glob_matcher(query)?;
        let mut total = 0_u32;
        let mut entries = Vec::new();
        for file in index
            .files
            .iter()
            .filter(|file| matcher.is_match(&file.path))
        {
            total = total.saturating_add(1);
            if entries.len() < size {
                entries.push(PathIndex::file_entry(file));
            }
        }
        (entries, total)
    } else {
        let mut scored: Vec<(u64, usize, ReviewPathEntry)> = Vec::new();
        let folders = index
            .folders
            .keys()
            .filter(|folder| !folder.is_empty())
            .map(|folder| (format!("{folder}/"), None));
        let files = index
            .files
            .iter()
            .map(|file| (file.path.clone(), Some(file)));
        for (path, file) in folders.chain(files) {
            let Some(score) = fuzzy_score(&path, query) else {
                continue;
            };
            let entry = file.map_or_else(
                || index.folder_entry(path.trim_end_matches('/')),
                PathIndex::file_entry,
            );
            scored.push((score, path.len(), entry));
        }
        scored.sort_by_key(|(score, length, _)| (*score, *length));
        let total = u32::try_from(scored.len()).unwrap_or(u32::MAX);
        let entries = scored
            .into_iter()
            .take(size)
            .map(|(_, _, entry)| entry)
            .collect();
        (entries, total)
    };
    Ok(ReviewPathsSearchResponse {
        entries,
        total_matches: total,
        glob,
        truncated: index.truncated,
    })
}

/// How many files selectors select now (`review.paths.resolve`).
pub fn resolve(
    cache: &PathIndexCache,
    root: &Path,
    request: &ReviewPathsResolveRequest,
) -> Result<ReviewPathsResolveResponse, DataError> {
    super::resolve::validate_selectors(&request.include, &request.exclude)?;
    let index = cache.get(root, request.include_ignored, request.refresh)?;
    let globs =
        |selectors: &[PathSelector]| -> Result<Vec<Option<globset::GlobMatcher>>, DataError> {
            selectors
                .iter()
                .map(|selector| match selector {
                    PathSelector::Glob { pattern } => glob_matcher(pattern).map(Some),
                    _ => Ok(None),
                })
                .collect()
        };
    let include_globs = globs(&request.include)?;
    let exclude_globs = globs(&request.exclude)?;
    // Files a selector names directly that the index does not hold (an ignored file).
    let unindexed: BTreeSet<&str> = request
        .include
        .iter()
        .filter(|selector| selector.is_explicit_file())
        .filter_map(PathSelector::path)
        .filter(|path| index.file(path).is_none() && working_tree_file(root, path).is_some())
        .collect();
    let mut include: Vec<ReviewSelectorCount> = request
        .include
        .iter()
        .map(|selector| ReviewSelectorCount {
            selector: selector.clone(),
            files: 0,
            error: None,
        })
        .collect();
    let mut exclude: Vec<ReviewSelectorCount> = request
        .exclude
        .iter()
        .map(|selector| ReviewSelectorCount {
            selector: selector.clone(),
            files: 0,
            error: None,
        })
        .collect();
    let mut total = 0_u32;
    let paths = index
        .files
        .iter()
        .map(|file| (file.path.as_str(), true))
        .chain(unindexed.iter().map(|path| (*path, false)));
    for (path, indexed) in paths {
        let mut chosen = false;
        for ((selector, glob), count) in
            request.include.iter().zip(&include_globs).zip(&mut include)
        {
            // A file the index does not hold is selected only by naming it.
            if (indexed || selector.is_explicit_file())
                && PathIndex::selects(selector, glob.as_ref(), path)
            {
                count.files += 1;
                chosen = true;
            }
        }
        if !chosen {
            continue;
        }
        let mut removed = false;
        for ((selector, glob), count) in
            request.exclude.iter().zip(&exclude_globs).zip(&mut exclude)
        {
            if PathIndex::selects(selector, glob.as_ref(), path) {
                count.files += 1;
                removed = true;
            }
        }
        if !removed {
            total = total.saturating_add(1);
        }
    }
    for count in include.iter_mut().chain(exclude.iter_mut()) {
        if count.files == 0 {
            count.error = selector_error(&index, root, &count.selector);
        }
    }
    Ok(ReviewPathsResolveResponse {
        include,
        exclude,
        total,
        truncated: index.truncated,
    })
}

/// Why `selector` selects no files now; `None` for an ignored folder (not listed).
fn selector_error(index: &PathIndex, root: &Path, selector: &PathSelector) -> Option<String> {
    match selector {
        PathSelector::File { path } | PathSelector::FileRange { path, .. } => {
            Some(if working_tree_dir(root, path).is_some() {
                format!("{path} is a folder; select {path}/")
            } else {
                format!("{path}: no such file")
            })
        }
        PathSelector::Folder { path } => {
            if index.in_ignored_folder(path) {
                None
            } else if working_tree_dir(root, path).is_some() {
                Some(format!("{path}/ has no files that are not ignored"))
            } else {
                Some(format!("{path}/: no such folder"))
            }
        }
        PathSelector::Glob { pattern } => Some(format!("{pattern} matches no files")),
    }
}

/// Lowercase words of a name: split on anything not alphanumeric and on camel-case boundaries.
fn words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    for token in text.split(|c: char| !c.is_alphanumeric()) {
        let chars: Vec<char> = token.chars().collect();
        let mut current = String::new();
        for (index, character) in chars.iter().enumerate() {
            let boundary = index > 0
                && character.is_uppercase()
                && (chars[index - 1].is_lowercase()
                    || chars.get(index + 1).is_some_and(|next| {
                        next.is_lowercase() && chars[index - 1].is_uppercase()
                    }));
            if boundary && !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            current.extend(character.to_lowercase());
        }
        if !current.is_empty() {
            words.push(current);
        }
    }
    words
}

/// The query's words without leading stop words and trailing kind words (unless nothing else
/// would remain).
fn query_words(query: &str) -> Vec<String> {
    let all = words(query);
    let mut start = 0;
    while start < all.len() && STOP_WORDS.contains(&all[start].as_str()) {
        start += 1;
    }
    let mut end = all.len();
    while end > start + 1 && KIND_WORDS.contains(&all[end - 1].as_str()) {
        end -= 1;
    }
    if start >= end {
        return all;
    }
    all[start..end].to_vec()
}

/// How well `query` (its words) names `name` (its words), out of 100; `None` for no match.
fn name_score(query: &[String], name: &[String]) -> Option<u32> {
    if query.is_empty() || name.is_empty() {
        return None;
    }
    if query.concat() == name.concat() {
        return Some(SCORE_EXACT);
    }
    let mut remaining = name.iter();
    if query
        .iter()
        .all(|word| remaining.any(|candidate| candidate == word))
    {
        return Some(90);
    }
    if query.iter().all(|word| {
        name.iter()
            .any(|candidate| candidate.starts_with(word.as_str()))
    }) {
        return Some(80);
    }
    if name.iter().all(|word| query.contains(word)) {
        return Some(60);
    }
    let score = fuzzy_score(&name.concat(), &query.concat())?;
    Some(if score < 1_000 {
        40 - u32::try_from(score.min(900) / 45).unwrap_or(20)
    } else {
        20 - u32::try_from((score - 1_000).min(190) / 10).unwrap_or(19)
    })
}

/// A `find_paths` query: its words, and the path ending it names when it has a `/`.
struct Query {
    words: Vec<String>,
    suffix: Option<String>,
}

impl Query {
    fn new(text: &str) -> Self {
        let suffix = text
            .trim()
            .trim_start_matches("./")
            .trim_matches('/')
            .to_lowercase();
        Self {
            words: query_words(text),
            suffix: suffix.contains('/').then_some(suffix),
        }
    }

    /// The score of a candidate at `path` called `name`, and how it matched.
    fn score(&self, path: &str, name: &str, by_name: &'static str) -> Option<(u32, &'static str)> {
        if let Some(suffix) = &self.suffix {
            let path = path.to_lowercase();
            if path == *suffix || path.ends_with(&format!("/{suffix}")) {
                return Some((SCORE_EXACT, "path"));
            }
        }
        name_score(&self.words, &words(name)).map(|score| (score, by_name))
    }
}

/// Every candidate of `query`, the best match per path.
fn candidates(
    index: &PathIndex,
    root: &Path,
    query: &Query,
    wants: impl Fn(ReviewPathMatchKind) -> bool,
) -> BTreeMap<String, ReviewPathMatch> {
    let mut best: BTreeMap<String, ReviewPathMatch> = BTreeMap::new();
    let mut offer = |candidate: ReviewPathMatch| {
        let rank = |found: &ReviewPathMatch| {
            let kind = match found.kind {
                ReviewPathMatchKind::Package => 2,
                ReviewPathMatchKind::File => 1,
                ReviewPathMatchKind::Folder => 0,
            };
            (found.score, kind)
        };
        if best
            .get(&candidate.path)
            .is_none_or(|kept| rank(kept) < rank(&candidate))
        {
            best.insert(candidate.path.clone(), candidate);
        }
    };
    let folder_files = |folder: &str| index.folders.get(folder).map_or(0, |count| count.files);
    let found = |path: String, kind, files, (score, matched): (u32, &str)| ReviewPathMatch {
        path,
        kind,
        package: None,
        manifest: None,
        files,
        matched: matched.to_owned(),
        score,
    };
    if wants(ReviewPathMatchKind::Package) {
        for package in index.packages(root) {
            if let Some(score) = query.score(&package.folder, &package.name, "package name") {
                offer(ReviewPathMatch {
                    package: Some(package.name.clone()),
                    manifest: Some(package.manifest.clone()),
                    ..found(
                        format!("{}/", package.folder),
                        ReviewPathMatchKind::Package,
                        folder_files(&package.folder),
                        score,
                    )
                });
            }
        }
    }
    if wants(ReviewPathMatchKind::Folder) {
        for folder in index.folders.keys().filter(|folder| !folder.is_empty()) {
            if let Some((score, matched)) = query.score(folder, base_name(folder), "folder name") {
                let score = score.saturating_sub(FOLDER_PENALTY).max(1);
                offer(found(
                    format!("{folder}/"),
                    ReviewPathMatchKind::Folder,
                    folder_files(folder),
                    (score, matched),
                ));
            }
        }
    }
    if wants(ReviewPathMatchKind::File) {
        for file in &index.files {
            if let Some(score) = query.score(&file.path, base_name(&file.path), "file name") {
                offer(found(
                    file.path.clone(),
                    ReviewPathMatchKind::File,
                    1,
                    score,
                ));
            }
        }
    }
    best
}

/// Resolve a natural name to repository paths (`review.paths.find`).
pub fn find(
    cache: &PathIndexCache,
    root: &Path,
    request: &ReviewPathsFindRequest,
) -> Result<ReviewPathsFindResponse, DataError> {
    let index = cache.get(root, request.include_ignored, false)?;
    let query = Query::new(&request.query);
    let wants = |kind| request.kinds.is_empty() || request.kinds.contains(&kind);
    let mut matches: Vec<ReviewPathMatch> = candidates(&index, root, &query, wants)
        .into_values()
        .collect();
    matches.sort_by(|a, b| {
        let depth = |found: &ReviewPathMatch| found.path.trim_end_matches('/').matches('/').count();
        b.score
            .cmp(&a.score)
            .then_with(|| depth(a).cmp(&depth(b)))
            .then_with(|| a.path.len().cmp(&b.path.len()))
            .then_with(|| a.path.cmp(&b.path))
    });
    let ambiguous = matches!(
        matches.as_slice(),
        [first, second, ..]
            if second.score >= AMBIGUOUS_FROM
                && first.score - second.score <= AMBIGUOUS_WITHIN
    );
    let total = u32::try_from(matches.len()).unwrap_or(u32::MAX);
    let limit = request
        .limit
        .unwrap_or(REVIEW_PATHS_FIND_DEFAULT_LIMIT)
        .clamp(1, REVIEW_PATHS_FIND_MAX_LIMIT);
    matches.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(ReviewPathsFindResponse {
        matches,
        total,
        ambiguous,
        truncated: index.truncated,
    })
}
