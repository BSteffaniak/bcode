//! Reviews of code as it is now: path selectors, browse selections, and the path operations
//! behind the path picker, `find_paths`, and uncommitted-change marks.
//!
//! One path syntax is used everywhere (the CLI, the picker's filter, and the assistant's
//! `create_review`):
//!
//! * `path`: a file or a folder, decided by the working tree ([`ParsedSelector::Path`]);
//! * `folder/`: every file under a folder;
//! * a glob such as `src/**/*.rs` (`*`, `?` or `[`; `\` escapes them);
//! * `path:START-END` (or `path:LINE`): lines of one file.
//!
//! Paths are repository-relative and `/`-separated. Parsing is purely syntactic; the service
//! confines paths to the repository.

use crate::{LineSpan, RemovedSince};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

/// Operation that lists one folder's children in the repository's path index.
pub const OP_REVIEW_PATHS_LIST: &str = "review.paths.list";
/// Operation that searches the path index by fuzzy name or glob.
pub const OP_REVIEW_PATHS_SEARCH: &str = "review.paths.search";
/// Operation that counts the files path selectors select now.
pub const OP_REVIEW_PATHS_RESOLVE: &str = "review.paths.resolve";
/// Operation that resolves a natural name ("the hello plugin") to repository paths.
pub const OP_REVIEW_PATHS_FIND: &str = "review.paths.find";
/// Operation that marks files' lines changed since `HEAD` (uncommitted changes).
pub const OP_REVIEW_WORKTREE_MARKS: &str = "review.worktree.marks";

/// Files one browse source lists by default before it is cut.
pub const REVIEW_FILES_DEFAULT_MAX_FILES: u32 = 5_000;
/// Upper bound on a configured per-source file limit.
pub const REVIEW_FILES_MAX_FILES_LIMIT: u32 = 100_000;
/// Include (and, separately, exclude) selectors one source may have.
pub const REVIEW_MAX_SELECTORS: usize = 200;
/// Paths the path index holds at most; beyond it the index is marked truncated.
pub const REVIEW_PATH_INDEX_MAX_PATHS: usize = 200_000;
/// Paths one `review.worktree.marks` request may name.
pub const REVIEW_WORKTREE_MARKS_MAX_PATHS: usize = 100;
/// Files over this size are not marked (`too_large`).
pub const REVIEW_WORKTREE_MARKS_MAX_BYTES: u64 = 4 * 1024 * 1024;
/// Default and maximum matches of `review.paths.find`.
pub const REVIEW_PATHS_FIND_DEFAULT_LIMIT: u32 = 10;
/// Maximum matches of `review.paths.find`.
pub const REVIEW_PATHS_FIND_MAX_LIMIT: u32 = 50;
/// Diagnostic code: a browse listing was cut to its file limit.
pub const REVIEW_DIAGNOSTIC_LISTING_TRUNCATED: &str = "listing_truncated";

/// What a files review selects: a file, lines of a file, a folder, or a glob.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PathSelector {
    /// One file (`path`).
    File {
        /// Repository-relative file path.
        path: String,
    },
    /// Lines `start..=end` of one file (`path:START-END`).
    FileRange {
        /// Repository-relative file path.
        path: String,
        /// First line (1-based).
        start: u32,
        /// Last line (inclusive).
        end: u32,
    },
    /// Every file under a folder, listed when the review resolves (`folder/`).
    Folder {
        /// Repository-relative folder path, without a trailing `/`.
        path: String,
    },
    /// Every file matching a git glob, listed when the review resolves (`src/**/*.rs`).
    Glob {
        /// The glob, relative to the repository root.
        pattern: String,
    },
}

impl PathSelector {
    /// Parse the selector syntax (see the module documentation).
    ///
    /// # Errors
    ///
    /// * [`SelectorError`] for an empty selector, one naming the repository root, or a
    ///   malformed line range.
    pub fn parse(text: &str) -> Result<ParsedSelector, SelectorError> {
        let trimmed = text.trim();
        let stripped = strip_dot_slash(trimmed);
        if stripped.is_empty() || stripped == "." {
            return Err(SelectorError::new(
                text,
                "names the whole repository; review the repository instead",
            ));
        }
        if has_glob_characters(stripped) {
            return Ok(ParsedSelector::Selector(Self::Glob {
                pattern: stripped.to_owned(),
            }));
        }
        let unescaped = unescape(stripped);
        if let Some((path, start, end)) = split_range(&unescaped, text)? {
            return Ok(ParsedSelector::Selector(Self::FileRange {
                path,
                start,
                end,
            }));
        }
        if let Some(folder) = unescaped.strip_suffix('/') {
            let folder = folder.trim_end_matches('/');
            if folder.is_empty() || folder == "." {
                return Err(SelectorError::new(
                    text,
                    "names the whole repository; review the repository instead",
                ));
            }
            return Ok(ParsedSelector::Selector(Self::Folder {
                path: folder.to_owned(),
            }));
        }
        Ok(ParsedSelector::Path(unescaped))
    }

    /// The repository-relative path a file, range, or folder selector names; `None` for a glob.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        match self {
            Self::File { path } | Self::FileRange { path, .. } | Self::Folder { path } => {
                Some(path)
            }
            Self::Glob { .. } => None,
        }
    }

    /// Whether the selector names one file explicitly (a file or a range).
    #[must_use]
    pub const fn is_explicit_file(&self) -> bool {
        matches!(self, Self::File { .. } | Self::FileRange { .. })
    }

    /// The git pathspec selecting the selector's files, with explicit magic so a path is never
    /// read as a pattern: `:(literal)path`, `:(literal)folder/`, `:(glob)pattern`.
    #[must_use]
    pub fn pathspec(&self) -> String {
        self.pathspec_with("")
    }

    /// The git pathspec excluding the selector's files: `:(exclude,literal)path`.
    #[must_use]
    pub fn exclude_pathspec(&self) -> String {
        self.pathspec_with("exclude,")
    }

    fn pathspec_with(&self, extra: &str) -> String {
        match self {
            Self::File { path } | Self::FileRange { path, .. } => {
                format!(":({extra}literal){path}")
            }
            Self::Folder { path } => format!(":({extra}literal){path}/"),
            Self::Glob { pattern } => format!(":({extra}glob){pattern}"),
        }
    }
}

/// The selector syntax: `path`, `folder/`, a glob, or `path:START-END`.
impl fmt::Display for PathSelector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File { path } => formatter.write_str(&escape(path)),
            Self::FileRange { path, start, end } => {
                write!(formatter, "{}:{start}-{end}", escape(path))
            }
            Self::Folder { path } => write!(formatter, "{}/", escape(path)),
            Self::Glob { pattern } => formatter.write_str(pattern),
        }
    }
}

/// A parsed selector: a complete [`PathSelector`], or a plain path that is a file or a folder
/// depending on the working tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedSelector {
    /// Fully determined by the syntax.
    Selector(PathSelector),
    /// A plain path: a [`PathSelector::File`] or a [`PathSelector::Folder`], by what it is.
    Path(String),
}

impl ParsedSelector {
    /// The selector, deciding a plain path with `is_dir` (whether the path is a folder).
    #[must_use]
    pub fn classify(self, is_dir: impl FnOnce(&str) -> bool) -> PathSelector {
        match self {
            Self::Selector(selector) => selector,
            Self::Path(path) if is_dir(&path) => PathSelector::Folder { path },
            Self::Path(path) => PathSelector::File { path },
        }
    }
}

/// A selector string that does not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorError {
    /// The selector as written.
    pub selector: String,
    /// What is wrong with it.
    pub reason: String,
}

impl SelectorError {
    fn new(selector: &str, reason: &str) -> Self {
        Self {
            selector: selector.to_owned(),
            reason: reason.to_owned(),
        }
    }
}

impl fmt::Display for SelectorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}: {}", self.selector, self.reason)
    }
}

impl std::error::Error for SelectorError {}

/// `text` without leading `./` segments.
fn strip_dot_slash(mut text: &str) -> &str {
    while let Some(rest) = text.strip_prefix("./") {
        text = rest.trim_start_matches('/');
    }
    text
}

/// Whether `text` has an unescaped `*`, `?` or `[`.
fn has_glob_characters(text: &str) -> bool {
    let mut escaped = false;
    for character in text.chars() {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if matches!(character, '*' | '?' | '[') {
            return true;
        }
    }
    false
}

/// `text` with `\` escapes removed.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut escaped = false;
    for character in text.chars() {
        if escaped || character != '\\' {
            out.push(character);
            escaped = false;
        } else {
            escaped = true;
        }
    }
    out
}

/// `path` with its glob characters and backslashes escaped, so it parses back as itself.
fn escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for character in path.chars() {
        if matches!(character, '*' | '?' | '[' | '\\') {
            out.push('\\');
        }
        out.push(character);
    }
    out
}

/// A trailing `:N-M` or `:N` range: the path and the ordered lines.
fn split_range(text: &str, original: &str) -> Result<Option<(String, u32, u32)>, SelectorError> {
    let Some((path, range)) = text.rsplit_once(':') else {
        return Ok(None);
    };
    if range.is_empty() || !range.chars().all(|c| c.is_ascii_digit() || c == '-') {
        return Ok(None);
    }
    let parse = |value: &str| {
        value
            .parse::<u32>()
            .ok()
            .filter(|line| *line >= 1)
            .ok_or_else(|| SelectorError::new(original, "line numbers start at 1"))
    };
    let (start, end) = if let Some((start, end)) = range.split_once('-') {
        (parse(start)?, parse(end)?)
    } else {
        let line = parse(range)?;
        (line, line)
    };
    if path.is_empty() || path.ends_with('/') {
        return Err(SelectorError::new(original, "a line range needs a file"));
    }
    Ok(Some((path.to_owned(), start.min(end), start.max(end))))
}

/// Which files a browse listing includes besides the tracked ones.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum BrowseUntracked {
    /// Tracked files only.
    #[default]
    Tracked,
    /// Tracked files and untracked files that are not ignored.
    Unignored,
    /// Tracked, untracked, and ignored files.
    All,
}

impl BrowseUntracked {
    /// The listing for an ignored-files toggle.
    #[must_use]
    pub const fn with_ignored(include_ignored: bool) -> Self {
        if include_ignored {
            Self::All
        } else {
            Self::Unignored
        }
    }
}

/// What a browse source lists: selectors instead of [`crate::ResolvedSource::paths`], which
/// untracked files, and the file limit.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BrowseSelection {
    /// What is listed; empty lists the whole repository.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<PathSelector>,
    /// Files and folders left out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<PathSelector>,
    /// Which untracked files are listed.
    #[serde(default)]
    pub untracked: BrowseUntracked,
    /// Files listed at most; the listing keeps the first ones in path order.
    #[serde(default = "default_max_files")]
    pub max_files: u32,
}

const fn default_max_files() -> u32 {
    REVIEW_FILES_DEFAULT_MAX_FILES
}

/// How browse sources resolve (`review.sources.resolve`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BrowseOptions {
    /// List ignored files in folder, glob, and repository sources.
    #[serde(default)]
    pub include_ignored: bool,
    /// Files one source lists at most ([`REVIEW_FILES_DEFAULT_MAX_FILES`] when unset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_files: Option<u32>,
}

impl BrowseOptions {
    /// Whether every option has its default.
    #[must_use]
    pub const fn is_default(&self) -> bool {
        !self.include_ignored && self.max_files.is_none()
    }

    /// The effective file limit.
    #[must_use]
    pub fn max_files(&self) -> u32 {
        self.max_files
            .unwrap_or(REVIEW_FILES_DEFAULT_MAX_FILES)
            .clamp(1, REVIEW_FILES_MAX_FILES_LIMIT)
    }
}

/// The message of a [`REVIEW_DIAGNOSTIC_LISTING_TRUNCATED`] diagnostic.
#[must_use]
pub fn listing_truncated_message(label: &str, total: u32, shown: u32) -> String {
    format!(
        "{label} resolves to {total} files; showing the first {shown} (files.max_files). \
         Choose narrower folders or raise the limit."
    )
}

/// Whether a path index entry is a file or a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewPathKind {
    /// A file.
    File,
    /// A folder.
    Folder,
}

/// Whether git tracks, ignores, or neither a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewPathStatus {
    /// In the index.
    #[default]
    Tracked,
    /// Neither tracked nor ignored.
    Untracked,
    /// Ignored (`.gitignore` and the other exclude sources).
    Ignored,
}

/// One file or folder of the path index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPathEntry {
    /// Base name.
    pub name: String,
    /// Repository-relative path; folders end with `/` (the selector syntax).
    pub path: String,
    /// File or folder.
    pub kind: ReviewPathKind,
    /// Files under a folder now (1 for a file); `None` for an ignored folder, which is not
    /// listed.
    #[serde(default)]
    pub files: Option<u32>,
    /// Files changed since `HEAD` (a file: 1 when it changed), untracked files included.
    #[serde(default)]
    pub changed: u32,
    /// Tracked, untracked, or ignored.
    #[serde(default)]
    pub status: ReviewPathStatus,
}

/// Request payload for [`OP_REVIEW_PATHS_LIST`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPathsListRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Folder whose children are listed (`""` for the root).
    #[serde(default)]
    pub dir: String,
    /// Also list ignored files and folders.
    #[serde(default)]
    pub include_ignored: bool,
    /// Build the path index again instead of reusing a recent one.
    #[serde(default)]
    pub refresh: bool,
    /// Cursor from the previous page.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Page size; clamped to the search maximum.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Response payload for [`OP_REVIEW_PATHS_LIST`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPathsListResponse {
    /// Children, folders first, then by name.
    pub entries: Vec<ReviewPathEntry>,
    /// Cursor for the next page.
    #[serde(default)]
    pub next_cursor: Option<String>,
    /// Children across all pages.
    pub total_entries: u32,
    /// The path index stopped at [`REVIEW_PATH_INDEX_MAX_PATHS`]; counts are lower bounds.
    #[serde(default)]
    pub truncated: bool,
}

/// Request payload for [`OP_REVIEW_PATHS_SEARCH`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPathsSearchRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Fuzzy text, or a glob when it has `*`, `?` or `[`.
    pub query: String,
    /// Also search ignored files and folders.
    #[serde(default)]
    pub include_ignored: bool,
    /// Build the path index again instead of reusing a recent one.
    #[serde(default)]
    pub refresh: bool,
    /// Matches returned at most; clamped to the search maximum.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Response payload for [`OP_REVIEW_PATHS_SEARCH`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPathsSearchResponse {
    /// Matching files and folders, best first (a glob's matches are files, in path order).
    pub entries: Vec<ReviewPathEntry>,
    /// Matches in all.
    pub total_matches: u32,
    /// The query was a glob.
    #[serde(default)]
    pub glob: bool,
    /// The path index stopped at [`REVIEW_PATH_INDEX_MAX_PATHS`].
    #[serde(default)]
    pub truncated: bool,
}

/// Request payload for [`OP_REVIEW_PATHS_RESOLVE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPathsResolveRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Selected files, folders, globs, and ranges.
    pub include: Vec<PathSelector>,
    /// Files and folders left out.
    #[serde(default)]
    pub exclude: Vec<PathSelector>,
    /// Count ignored files too.
    #[serde(default)]
    pub include_ignored: bool,
    /// Build the path index again instead of reusing a recent one.
    #[serde(default)]
    pub refresh: bool,
}

/// What one selector selects now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSelectorCount {
    /// The selector.
    pub selector: PathSelector,
    /// Files it selects now.
    pub files: u32,
    /// What is wrong with it, such as "no such file or folder".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Response payload for [`OP_REVIEW_PATHS_RESOLVE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPathsResolveResponse {
    /// Per include selector, in request order.
    pub include: Vec<ReviewSelectorCount>,
    /// Per exclude selector, in request order (files of the includes it removes).
    pub exclude: Vec<ReviewSelectorCount>,
    /// Files selected after exclusions.
    pub total: u32,
    /// The path index stopped at [`REVIEW_PATH_INDEX_MAX_PATHS`]; counts are lower bounds.
    #[serde(default)]
    pub truncated: bool,
}

/// What `review.paths.find` looks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewPathMatchKind {
    /// A folder, by its name.
    Folder,
    /// A package (`Cargo.toml`, `package.json`, or `pyproject.toml`), by its name.
    Package,
    /// A file, by its name.
    File,
}

/// Request payload for [`OP_REVIEW_PATHS_FIND`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPathsFindRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// What the user called it: "the hello plugin", "bcode-git", "Cargo.toml".
    pub query: String,
    /// What to look for; every kind when empty.
    #[serde(default)]
    pub kinds: Vec<ReviewPathMatchKind>,
    /// Also search ignored paths.
    #[serde(default)]
    pub include_ignored: bool,
    /// Matches returned at most ([`REVIEW_PATHS_FIND_DEFAULT_LIMIT`] when unset).
    #[serde(default)]
    pub limit: Option<u32>,
}

/// One `review.paths.find` match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPathMatch {
    /// Repository-relative path; folders and packages end with `/`.
    pub path: String,
    /// What matched.
    pub kind: ReviewPathMatchKind,
    /// The package name, for a package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// The package manifest, for a package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<String>,
    /// Files under a folder or package now (1 for a file).
    pub files: u32,
    /// How it matched: "package name", "folder name", "file name", "path".
    pub matched: String,
    /// Score out of 100; higher is better.
    pub score: u32,
}

/// Response payload for [`OP_REVIEW_PATHS_FIND`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPathsFindResponse {
    /// Matches, best first.
    pub matches: Vec<ReviewPathMatch>,
    /// Matches in all.
    pub total: u32,
    /// The two best matches name different paths and score alike: ask the user which.
    pub ambiguous: bool,
    /// The path index stopped at [`REVIEW_PATH_INDEX_MAX_PATHS`].
    #[serde(default)]
    pub truncated: bool,
}

/// Request payload for [`OP_REVIEW_WORKTREE_MARKS`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewWorktreeMarksRequest {
    /// Absolute repository path.
    pub repo_path: PathBuf,
    /// Repository-relative file paths (at most [`REVIEW_WORKTREE_MARKS_MAX_PATHS`]).
    pub paths: Vec<String>,
}

/// One file's lines changed since `HEAD`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewWorktreeFileMarks {
    /// Repository-relative path.
    pub path: String,
    /// The file is not tracked: all of it is new, and nothing else is marked.
    #[serde(default)]
    pub untracked: bool,
    /// The file is not in the working tree.
    #[serde(default)]
    pub missing: bool,
    /// Lines added or changed since `HEAD`, sorted, disjoint.
    #[serde(default)]
    pub added: Vec<LineSpan>,
    /// Lines removed since `HEAD`, by position.
    #[serde(default)]
    pub removed: Vec<RemovedSince>,
    /// Changed lines with their text at `HEAD`, sorted by line.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub was: Vec<(u32, String)>,
    /// Lines added since `HEAD`.
    #[serde(default)]
    pub additions: u32,
    /// Lines removed since `HEAD`.
    #[serde(default)]
    pub deletions: u32,
    /// The file is over [`REVIEW_WORKTREE_MARKS_MAX_BYTES`]; nothing is marked.
    #[serde(default)]
    pub too_large: bool,
}

impl ReviewWorktreeFileMarks {
    /// Whether anything is marked.
    #[must_use]
    pub const fn is_changed(&self) -> bool {
        self.untracked || !self.added.is_empty() || !self.removed.is_empty()
    }
}

/// Response payload for [`OP_REVIEW_WORKTREE_MARKS`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewWorktreeMarksResponse {
    /// One entry per requested path, in request order.
    pub files: Vec<ReviewWorktreeFileMarks>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> ParsedSelector {
        PathSelector::parse(text).expect("parses")
    }

    fn selector(text: &str) -> PathSelector {
        match parse(text) {
            ParsedSelector::Selector(selector) => selector,
            ParsedSelector::Path(path) => panic!("{text} is a plain path {path}"),
        }
    }

    #[test]
    fn selector_syntax_parses_files_folders_globs_and_ranges() {
        assert_eq!(
            parse("src/lib.rs"),
            ParsedSelector::Path("src/lib.rs".into())
        );
        assert_eq!(
            parse("./src/lib.rs"),
            ParsedSelector::Path("src/lib.rs".into())
        );
        assert_eq!(
            selector("examples/hello-plugin/"),
            PathSelector::Folder {
                path: "examples/hello-plugin".into()
            }
        );
        assert_eq!(
            selector("src/**/*.rs"),
            PathSelector::Glob {
                pattern: "src/**/*.rs".into()
            }
        );
        assert_eq!(
            selector("src/lib.rs:40-10"),
            PathSelector::FileRange {
                path: "src/lib.rs".into(),
                start: 10,
                end: 40
            }
        );
        assert_eq!(
            selector("src/lib.rs:7"),
            PathSelector::FileRange {
                path: "src/lib.rs".into(),
                start: 7,
                end: 7
            }
        );
        // A colon not followed by a range is part of the name.
        assert_eq!(parse("a:b.txt"), ParsedSelector::Path("a:b.txt".into()));
        // Escaped glob characters are literal.
        assert_eq!(
            parse(r"a/\[x\].txt"),
            ParsedSelector::Path("a/[x].txt".into())
        );
        assert!(PathSelector::parse("src/lib.rs:0-3").is_err());
        assert!(PathSelector::parse("src/:1-2").is_err());
        assert!(PathSelector::parse("").is_err());
        assert!(PathSelector::parse("./").is_err());
        assert!(PathSelector::parse(".").is_err());
    }

    #[test]
    fn selectors_print_back_in_the_syntax_and_map_to_literal_or_glob_pathspecs() {
        for text in [
            "src/lib.rs",
            "examples/hello-plugin/",
            "src/**/*.rs",
            "src/lib.rs:10-40",
            r"a/\[x].txt",
        ] {
            let parsed = parse(text).classify(|path| path.ends_with('/'));
            assert_eq!(parsed.to_string(), text);
        }
        let folder = PathSelector::Folder { path: "a/b".into() };
        assert_eq!(folder.pathspec(), ":(literal)a/b/");
        assert_eq!(folder.exclude_pathspec(), ":(exclude,literal)a/b/");
        let file = parse(r"a/\[x].txt").classify(|_| false);
        assert_eq!(
            file,
            PathSelector::File {
                path: "a/[x].txt".into()
            }
        );
        assert_eq!(file.pathspec(), ":(literal)a/[x].txt");
        assert_eq!(
            PathSelector::Glob {
                pattern: "*.rs".into()
            }
            .pathspec(),
            ":(glob)*.rs"
        );
    }

    #[test]
    fn a_plain_path_is_classified_by_the_working_tree() {
        assert_eq!(
            parse("examples").classify(|_| true),
            PathSelector::Folder {
                path: "examples".into()
            }
        );
        assert_eq!(
            parse("README.md").classify(|_| false),
            PathSelector::File {
                path: "README.md".into()
            }
        );
    }

    #[test]
    fn selections_and_options_default_for_older_payloads() {
        let selection: BrowseSelection = serde_json::from_str("{}").expect("selection");
        assert_eq!(selection.untracked, BrowseUntracked::Tracked);
        assert_eq!(selection.max_files, REVIEW_FILES_DEFAULT_MAX_FILES);
        let options = BrowseOptions::default();
        assert!(options.is_default());
        assert_eq!(options.max_files(), REVIEW_FILES_DEFAULT_MAX_FILES);
        let json = serde_json::to_value(PathSelector::Folder { path: "a".into() }).expect("json");
        assert_eq!(json, serde_json::json!({"kind": "folder", "path": "a"}));
    }
}
