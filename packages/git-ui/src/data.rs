//! Repository data loading for panels and the main view. Runs off the input thread.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use bcode_git::models::{
    Branch, ChangedFile, Commit, DiffFile, HeadState, ReflogEntry, Remote, RemoteBranch,
    RepoOperation, StashEntry, StatusEntry, StatusSnapshot, Submodule, Tag, Worktree,
};
use bcode_git::{DiffTarget, LogQuery, Repository};
use bcode_syntax_render::SyntaxHighlighter;
use bmux_tui::style::{Modifier, Style};
use bmux_tui::text::{Line, Span};

use crate::diffview::{DiffOptions, DiffRow, SideTexts, present_file};
use crate::graph;
use crate::theme::GitTheme;

/// Everything shown in the side panels.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// `HEAD`.
    pub head: Option<HeadState>,
    /// Operation in progress.
    pub operation: RepoOperation,
    /// Working tree status.
    pub status: StatusSnapshot,
    /// Local branches.
    pub branches: Vec<Branch>,
    /// Remotes.
    pub remotes: Vec<Remote>,
    /// Remote-tracking branches.
    pub remote_branches: Vec<RemoteBranch>,
    /// Tags.
    pub tags: Vec<Tag>,
    /// History of `HEAD`.
    pub commits: Vec<Commit>,
    /// Whether more history exists beyond `commits`.
    pub more_commits: bool,
    /// Commits not yet pushed to upstream.
    pub unpushed: BTreeSet<String>,
    /// `HEAD` reflog.
    pub reflog: Vec<ReflogEntry>,
    /// Stash entries.
    pub stashes: Vec<StashEntry>,
    /// Worktrees.
    pub worktrees: Vec<Worktree>,
    /// Submodules.
    pub submodules: Vec<Submodule>,
    /// Non-fatal load failures.
    pub errors: Vec<String>,
}

/// Load all panel data, running independent queries in parallel.
#[must_use]
pub fn load_snapshot(repo: &Repository, commit_limit: usize) -> Snapshot {
    fn join<T>(
        handle: std::thread::ScopedJoinHandle<'_, Result<T, bcode_git::GitError>>,
    ) -> Result<T, bcode_git::GitError> {
        handle
            .join()
            .unwrap_or_else(|_| Err(bcode_git::GitError::Parse("loader panicked".into())))
    }
    fn take<T: Default>(result: Result<T, bcode_git::GitError>, errors: &mut Vec<String>) -> T {
        result.unwrap_or_else(|error| {
            errors.push(error.message());
            T::default()
        })
    }
    let mut errors = Vec::new();
    let query = LogQuery {
        limit: commit_limit + 1,
        ..LogQuery::default()
    };
    let (
        head,
        status,
        branches,
        remotes,
        remote_branches,
        tags,
        commits,
        unpushed,
        reflog,
        stashes,
        worktrees,
        submodules,
    ) = std::thread::scope(|scope| {
        let head = scope.spawn(|| repo.head());
        let status = scope.spawn(|| repo.status());
        let branches = scope.spawn(|| repo.branches());
        let remotes = scope.spawn(|| repo.remotes());
        let remote_branches = scope.spawn(|| repo.remote_branches());
        let tags = scope.spawn(|| repo.tags());
        let commits = scope.spawn(|| repo.log(&query));
        let unpushed = scope.spawn(|| repo.unpushed());
        let reflog = scope.spawn(|| repo.reflog(200));
        let stashes = scope.spawn(|| repo.stashes());
        let worktrees = scope.spawn(|| repo.worktrees());
        let submodules = scope.spawn(|| repo.submodules());
        (
            join(head).map(Some),
            join(status),
            join(branches),
            join(remotes),
            join(remote_branches),
            join(tags),
            join(commits),
            join(unpushed),
            join(reflog),
            join(stashes),
            join(worktrees),
            join(submodules),
        )
    });
    let mut commits: Vec<Commit> = take(commits, &mut errors);
    let more_commits = commits.len() > commit_limit;
    commits.truncate(commit_limit);
    Snapshot {
        head: take(head, &mut errors),
        operation: repo.operation(),
        status: take(status, &mut errors),
        branches: take(branches, &mut errors),
        remotes: take(remotes, &mut errors),
        remote_branches: take(remote_branches, &mut errors),
        tags: take(tags, &mut errors),
        commits,
        more_commits,
        unpushed: take(unpushed, &mut errors),
        reflog: take(reflog, &mut errors),
        stashes: take(stashes, &mut errors),
        worktrees: take(worktrees, &mut errors),
        submodules: take(submodules, &mut errors),
        errors,
    }
}

/// One row of the files panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileNode {
    /// A directory (possibly a compressed chain such as `src/ui`).
    Dir {
        /// Full directory path.
        path: String,
        /// Displayed label.
        label: String,
        /// Nesting depth.
        depth: usize,
        /// Whether children are hidden.
        collapsed: bool,
        /// Status entry indices beneath this directory.
        entries: Vec<usize>,
    },
    /// A file.
    File {
        /// Status entry index.
        entry: usize,
        /// Displayed label.
        label: String,
        /// Nesting depth.
        depth: usize,
    },
}

impl FileNode {
    /// Status entries this row covers.
    #[must_use]
    pub fn entries(&self) -> Vec<usize> {
        match self {
            Self::Dir { entries, .. } => entries.clone(),
            Self::File { entry, .. } => vec![*entry],
        }
    }

    /// Stable key for identity across refreshes.
    #[must_use]
    pub fn key(&self, status: &StatusSnapshot) -> String {
        match self {
            Self::Dir { path, .. } => format!("dir:{path}"),
            Self::File { entry, .. } => format!("file:{}", status.entries[*entry].path),
        }
    }
}

#[derive(Default)]
struct TreeNode {
    dirs: BTreeMap<String, Self>,
    files: Vec<(String, usize)>,
}

/// Rows for the files panel, as a flat list or a compressed directory tree.
#[must_use]
pub fn file_rows(
    status: &StatusSnapshot,
    tree: bool,
    collapsed: &BTreeSet<String>,
) -> Vec<FileNode> {
    let mut order: Vec<usize> = (0..status.entries.len()).collect();
    order.sort_by(|a, b| status.entries[*a].path.cmp(&status.entries[*b].path));
    if !tree {
        return order
            .into_iter()
            .map(|entry| FileNode::File {
                entry,
                label: status.entries[entry].path.clone(),
                depth: 0,
            })
            .collect();
    }
    let mut root = TreeNode::default();
    for entry in order {
        let path = &status.entries[entry].path;
        let mut parts: Vec<&str> = path.trim_end_matches('/').split('/').collect();
        let name = parts.pop().unwrap_or_default().to_owned();
        let mut node = &mut root;
        for part in parts {
            node = node.dirs.entry(part.to_owned()).or_default();
        }
        node.files.push((name, entry));
    }
    let mut rows = Vec::new();
    flatten(&root, "", 0, collapsed, &mut rows);
    rows
}

fn flatten(
    node: &TreeNode,
    prefix: &str,
    depth: usize,
    collapsed: &BTreeSet<String>,
    rows: &mut Vec<FileNode>,
) {
    for (name, child) in &node.dirs {
        let mut label = name.clone();
        let mut current = child;
        while current.files.is_empty() && current.dirs.len() == 1 {
            let (next_name, next) = current.dirs.iter().next().unwrap_or_else(|| unreachable!());
            label = format!("{label}/{next_name}");
            current = next;
        }
        let path = format!("{prefix}{label}");
        let is_collapsed = collapsed.contains(&path);
        let mut entries = Vec::new();
        collect_entries(current, &mut entries);
        rows.push(FileNode::Dir {
            path: path.clone(),
            label,
            depth,
            collapsed: is_collapsed,
            entries,
        });
        if !is_collapsed {
            flatten(current, &format!("{path}/"), depth + 1, collapsed, rows);
        }
    }
    for (name, entry) in &node.files {
        rows.push(FileNode::File {
            entry: *entry,
            label: name.clone(),
            depth,
        });
    }
}

fn collect_entries(node: &TreeNode, entries: &mut Vec<usize>) {
    for child in node.dirs.values() {
        collect_entries(child, entries);
    }
    entries.extend(node.files.iter().map(|(_, entry)| *entry));
}

/// What the main view shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MainRequest {
    /// A message.
    Message(String),
    /// Repository overview.
    Status,
    /// Changes of a working tree file.
    File(StatusEntry),
    /// A commit, optionally limited to one path.
    Commit {
        /// Commit.
        oid: String,
        /// Path filter.
        path: Option<String>,
    },
    /// A stash entry, optionally limited to one path.
    Stash {
        /// Selector such as `stash@{0}`.
        selector: String,
        /// Path filter.
        path: Option<String>,
    },
    /// History of a revision.
    Log {
        /// Revision.
        revision: String,
    },
    /// Difference between two revisions.
    Range {
        /// Old side.
        from: String,
        /// New side.
        to: String,
        /// Path filter.
        path: Option<String>,
    },
}

/// Which index operation a main section supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionKind {
    /// Read-only.
    Plain,
    /// Unstaged changes (stage / discard lines).
    Unstaged,
    /// Staged changes (unstage lines).
    Staged,
}

/// One titled block of the main view.
#[derive(Debug, Clone)]
pub struct MainSection {
    /// Title.
    pub title: String,
    /// Supported operations.
    pub kind: SectionKind,
    /// Parsed diffs, indexed by [`DiffRow::file`].
    pub files: Vec<DiffFile>,
    /// Presented rows.
    pub rows: Vec<DiffRow>,
}

/// Loaded main view.
#[derive(Debug, Clone)]
pub struct MainContent {
    /// Request that produced this content.
    pub request: MainRequest,
    /// Sections, top to bottom.
    pub sections: Vec<MainSection>,
}

/// Shared inputs for building presentations.
#[derive(Debug, Clone)]
pub struct PresentContext {
    /// Styles.
    pub theme: GitTheme,
    /// Diff context lines.
    pub context: u32,
    /// Language-aware highlighting.
    pub highlight: bool,
    /// Whole-file highlighting size bound.
    pub max_file_bytes: u64,
    /// Relative dates.
    pub relative_dates: bool,
    /// Show the commit graph in logs.
    pub show_graph: bool,
}

/// Build the main view for a request.
#[must_use]
pub fn load_main(repo: &Repository, request: MainRequest, cx: &PresentContext) -> MainContent {
    let highlighter = SyntaxHighlighter::new();
    let sections = match &request {
        MainRequest::Message(message) => vec![plain(
            "",
            vec![DiffRow::info(Line::from_spans(vec![Span::styled(
                message.clone(),
                cx.theme.muted,
            )]))],
        )],
        MainRequest::Status => vec![plain("bcode git", status_rows(repo, cx))],
        MainRequest::File(entry) => file_sections(repo, entry, cx, &highlighter),
        MainRequest::Commit { oid, path } => {
            let mut rows = commit_header(repo, oid, cx);
            let files = repo
                .diff(
                    &DiffTarget::Commit(oid.clone()),
                    &path_filter(path.as_ref()),
                    cx.context,
                )
                .unwrap_or_else(|error| {
                    rows.push(error_row(&error.message(), cx));
                    Vec::new()
                });
            let parent = format!("{oid}^");
            vec![files_section(
                repo,
                "Commit",
                files,
                rows,
                |file| side_spec(file, Some(&parent), Some(oid)),
                cx,
                &highlighter,
            )]
        }
        MainRequest::Stash { selector, path } => {
            let files = repo
                .diff(
                    &DiffTarget::Stash(selector.clone()),
                    &path_filter(path.as_ref()),
                    cx.context,
                )
                .unwrap_or_default();
            let base = format!("{selector}^1");
            vec![files_section(
                repo,
                "Stash",
                files,
                Vec::new(),
                |file| side_spec(file, Some(&base), Some(selector)),
                cx,
                &highlighter,
            )]
        }
        MainRequest::Range { from, to, path } => {
            let files = repo
                .diff(
                    &DiffTarget::Range {
                        from: from.clone(),
                        to: to.clone(),
                    },
                    &path_filter(path.as_ref()),
                    cx.context,
                )
                .unwrap_or_default();
            vec![files_section(
                repo,
                &format!("{from} → {to}"),
                files,
                Vec::new(),
                |file| side_spec(file, Some(from), Some(to)),
                cx,
                &highlighter,
            )]
        }
        MainRequest::Log { revision } => {
            let commits = repo
                .log(&LogQuery {
                    revision: Some(revision.clone()),
                    limit: 200,
                    ..LogQuery::default()
                })
                .unwrap_or_default();
            vec![plain(
                &format!("Log · {revision}"),
                log_rows(&commits, &BTreeSet::new(), cx),
            )]
        }
    };
    MainContent { request, sections }
}

fn path_filter(path: Option<&String>) -> Vec<&str> {
    path.map(String::as_str).into_iter().collect()
}

fn plain(title: &str, rows: Vec<DiffRow>) -> MainSection {
    MainSection {
        title: title.to_owned(),
        kind: SectionKind::Plain,
        files: Vec::new(),
        rows,
    }
}

fn error_row(message: &str, cx: &PresentContext) -> DiffRow {
    DiffRow::info(Line::from_spans(vec![Span::styled(
        message.to_owned(),
        cx.theme.error,
    )]))
}

/// Revision specs for each side of a file; `None` means the worktree.
struct SideSpec {
    old: Option<String>,
    new: Option<String>,
    worktree_new: bool,
}

fn side_spec(file: &DiffFile, old_rev: Option<&str>, new_rev: Option<&str>) -> SideSpec {
    SideSpec {
        old: file
            .old_path
            .as_ref()
            .zip(old_rev)
            .map(|(path, rev)| format!("{rev}:{path}")),
        new: file
            .new_path
            .as_ref()
            .zip(new_rev)
            .map(|(path, rev)| format!("{rev}:{path}")),
        worktree_new: false,
    }
}

fn read_sides(
    repo: &Repository,
    file: &DiffFile,
    spec: &SideSpec,
    cx: &PresentContext,
) -> SideTexts {
    if !cx.highlight || file.is_binary {
        return SideTexts::default();
    }
    let limit = usize::try_from(cx.max_file_bytes).unwrap_or(usize::MAX);
    let decode = |bytes: Vec<u8>| {
        (bytes.len() <= limit)
            .then(|| String::from_utf8(bytes).ok())
            .flatten()
    };
    let old = spec
        .old
        .as_ref()
        .and_then(|spec| repo.show_file(spec).ok().flatten())
        .and_then(decode);
    let new = if spec.worktree_new {
        file.new_path
            .as_ref()
            .and_then(|path| std::fs::read(repo.root().join(path)).ok())
            .and_then(decode)
    } else {
        spec.new
            .as_ref()
            .and_then(|spec| repo.show_file(spec).ok().flatten())
            .and_then(decode)
    };
    SideTexts { old, new }
}

const MAX_HIGHLIGHTED_FILES: usize = 40;

fn files_section(
    repo: &Repository,
    title: &str,
    files: Vec<DiffFile>,
    mut rows: Vec<DiffRow>,
    spec: impl Fn(&DiffFile) -> SideSpec,
    cx: &PresentContext,
    highlighter: &SyntaxHighlighter,
) -> MainSection {
    let options = DiffOptions {
        highlight: cx.highlight,
        file_headers: true,
    };
    let fetch_sides = files.len() <= MAX_HIGHLIGHTED_FILES;
    for (index, file) in files.iter().enumerate() {
        let sides = if fetch_sides {
            read_sides(repo, file, &spec(file), cx)
        } else {
            SideTexts::default()
        };
        rows.extend(present_file(
            index,
            file,
            &sides,
            &cx.theme,
            highlighter,
            options,
        ));
    }
    if files.is_empty() && rows.is_empty() {
        rows.push(DiffRow::info(Line::from_spans(vec![Span::styled(
            "No changes",
            cx.theme.muted,
        )])));
    }
    MainSection {
        title: title.to_owned(),
        kind: SectionKind::Plain,
        files,
        rows,
    }
}

fn file_sections(
    repo: &Repository,
    entry: &StatusEntry,
    cx: &PresentContext,
    highlighter: &SyntaxHighlighter,
) -> Vec<MainSection> {
    let options = DiffOptions {
        highlight: cx.highlight,
        file_headers: false,
    };
    let path = entry.path.as_str();
    let mut sections = Vec::new();
    let mut section = |title: &str, kind: SectionKind, target: DiffTarget, spec: SideSpec| {
        let files = match repo.diff(&target, &[path], cx.context) {
            Ok(files) => files,
            Err(error) => {
                sections.push(plain(title, vec![error_row(&error.message(), cx)]));
                return;
            }
        };
        let mut rows = Vec::new();
        for (index, file) in files.iter().enumerate() {
            let sides = read_sides(repo, file, &spec, cx);
            rows.extend(present_file(
                index,
                file,
                &sides,
                &cx.theme,
                highlighter,
                options,
            ));
        }
        if rows.is_empty() {
            return;
        }
        sections.push(MainSection {
            title: title.to_owned(),
            kind,
            files,
            rows,
        });
    };
    if entry.is_untracked() {
        section(
            "Untracked file",
            SectionKind::Unstaged,
            DiffTarget::Untracked,
            SideSpec {
                old: None,
                new: None,
                worktree_new: true,
            },
        );
    } else {
        if entry.has_unstaged() {
            section(
                if entry.is_conflicted() {
                    "Merge conflict"
                } else {
                    "Unstaged changes"
                },
                if entry.is_conflicted() {
                    SectionKind::Plain
                } else {
                    SectionKind::Unstaged
                },
                DiffTarget::Unstaged,
                SideSpec {
                    old: Some(format!(":{path}")),
                    new: None,
                    worktree_new: true,
                },
            );
        }
        if entry.has_staged() {
            let old_path = entry.orig_path.as_deref().unwrap_or(path);
            section(
                "Staged changes",
                SectionKind::Staged,
                DiffTarget::Staged,
                SideSpec {
                    old: Some(format!("HEAD:{old_path}")),
                    new: Some(format!(":{path}")),
                    worktree_new: false,
                },
            );
        }
    }
    if sections.is_empty() {
        sections.push(plain(
            "",
            vec![DiffRow::info(Line::from_spans(vec![Span::styled(
                "No textual changes",
                cx.theme.muted,
            )]))],
        ));
    }
    sections
}

fn commit_header(repo: &Repository, oid: &str, cx: &PresentContext) -> Vec<DiffRow> {
    let theme = &cx.theme;
    let Ok(output) = repo.runner().read([
        "show",
        "--no-patch",
        "--format=%H%x1f%an%x1f%ae%x1f%at%x1f%D%x1f%B",
        oid,
    ]) else {
        return Vec::new();
    };
    let fields: Vec<&str> = output.splitn(6, '\u{1f}').collect();
    let [sha, name, email, time, refs, message] = fields[..] else {
        return Vec::new();
    };
    let mut rows = vec![DiffRow::info(Line::from_spans(vec![
        Span::styled("commit ", theme.muted),
        Span::styled(sha.to_owned(), theme.sha.add_modifier(Modifier::BOLD)),
        Span::styled(
            if refs.is_empty() {
                String::new()
            } else {
                format!("  ({refs})")
            },
            theme.branch_local,
        ),
    ]))];
    rows.push(DiffRow::info(Line::from_spans(vec![
        Span::styled("Author ", theme.muted),
        Span::styled(
            name.to_owned(),
            theme.hashed(name).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" <{email}>"), theme.muted),
    ])));
    rows.push(DiffRow::info(Line::from_spans(vec![
        Span::styled("Date   ", theme.muted),
        Span::styled(
            format_time(time.parse().unwrap_or_default(), cx.relative_dates),
            theme.date,
        ),
    ])));
    rows.push(DiffRow::info(Line::new()));
    for (index, line) in message.trim_end().lines().enumerate() {
        let style = if index == 0 {
            theme.text.add_modifier(Modifier::BOLD)
        } else {
            theme.text
        };
        rows.push(DiffRow::info(Line::from_spans(vec![Span::styled(
            format!("    {line}"),
            style,
        )])));
    }
    rows.push(DiffRow::info(Line::new()));
    rows
}

/// Rows for a commit log with optional graph.
#[must_use]
pub fn log_rows(
    commits: &[Commit],
    unpushed: &BTreeSet<String>,
    cx: &PresentContext,
) -> Vec<DiffRow> {
    let graph = cx.show_graph.then(|| graph::layout(commits));
    commits
        .iter()
        .enumerate()
        .map(|(index, commit)| {
            DiffRow::info(commit_line(
                commit,
                graph
                    .as_ref()
                    .and_then(|rows| rows.get(index))
                    .map(Vec::as_slice),
                unpushed.contains(&commit.oid),
                &cx.theme,
                cx.relative_dates,
            ))
        })
        .collect()
}

/// One commit row: sha, graph, refs, summary, author and date.
#[must_use]
pub fn commit_line(
    commit: &Commit,
    graph: Option<&[graph::Cell]>,
    unpushed: bool,
    theme: &GitTheme,
    relative_dates: bool,
) -> Line {
    let mut spans = vec![Span::styled(
        format!("{} ", commit.short_oid),
        if unpushed { theme.unstaged } else { theme.sha },
    )];
    if let Some(cells) = graph {
        for cell in cells {
            let style = graph::color(*cell).map_or(theme.muted, |lane| theme.lane(lane));
            spans.push(Span::styled(graph::glyph(*cell), style));
        }
    }
    for reference in &commit.refs {
        let (text, style) = ref_label(reference, theme);
        spans.push(Span::styled(text, style.add_modifier(Modifier::BOLD)));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(commit.summary.clone(), theme.text));
    spans.push(Span::styled(
        format!("  {} ", initials(&commit.author_name)),
        theme.hashed(&commit.author_name),
    ));
    spans.push(Span::styled(
        format_time(commit.author_time, relative_dates),
        theme.date,
    ));
    Line::from_spans(spans)
}

fn ref_label(reference: &str, theme: &GitTheme) -> (String, Style) {
    if let Some(tag) = reference.strip_prefix("tag: ") {
        return (format!("◆ {tag}"), theme.tag);
    }
    if let Some(branch) = reference.strip_prefix("HEAD -> ") {
        return (format!("✱ {branch}"), theme.branch_head);
    }
    let style = match reference {
        "HEAD" => theme.branch_head,
        remote if remote.contains('/') => theme.branch_remote,
        _ => theme.branch_local,
    };
    let text = if reference == "HEAD" {
        "✱ HEAD".to_owned()
    } else {
        reference.to_owned()
    };
    (text, style)
}

fn initials(name: &str) -> String {
    let initials: String = name
        .split_whitespace()
        .filter_map(|part| part.chars().next())
        .take(2)
        .collect();
    if initials.is_empty() {
        "?".to_owned()
    } else {
        initials.to_uppercase()
    }
}

/// Format a Unix timestamp as a compact relative age or an ISO date.
#[must_use]
pub fn format_time(seconds: i64, relative: bool) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        });
    if relative {
        let age = (now - seconds).max(0);
        return match age {
            0..60 => format!("{age}s"),
            60..3_600 => format!("{}m", age / 60),
            3_600..86_400 => format!("{}h", age / 3_600),
            86_400..2_592_000 => format!("{}d", age / 86_400),
            2_592_000..31_536_000 => format!("{}mo", age / 2_592_000),
            _ => format!("{}y", age / 31_536_000),
        };
    }
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    format!("{year:04}-{month:02}-{day:02}")
}

/// Days since 1970-01-01 to a proleptic Gregorian date.
const fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

/// Wordmark glyphs drawn with block elements: one-cell vertical strokes, half-cell horizontal
/// strokes, four rows with b, d, i and t rising into the top row. Rows of a glyph share a width.
const WORDMARK_GLYPHS: [(char, [&str; 4]); 8] = [
    ('b', ["█   ", "█▀▀█", "█  █", "▀▀▀▀"]),
    ('c', ["    ", "█▀▀▀", "█   ", "▀▀▀▀"]),
    ('o', ["    ", "█▀▀█", "█  █", "▀▀▀▀"]),
    ('d', ["   █", "█▀▀█", "█  █", "▀▀▀▀"]),
    ('e', ["    ", "█▀▀█", "█▀▀▀", "▀▀▀▀"]),
    ('g', ["    ", "█▀▀▀", "█ ▀█", "▀▀▀▀"]),
    ('i', ["▀", "█", "█", "▀"]),
    ('t', ["█  ", "█▀▀", "█  ", "▀▀▀"]),
];

/// The overview wordmark: "bcode" in the text color and "git" in the accent color, as block
/// letters 37 cells wide. Rows are fixed: never wrapped; narrow views drop the indent, and
/// only views narrower than the letters clip them.
fn logo_rows(theme: &crate::theme::GitTheme) -> Vec<DiffRow> {
    const INDENT: &str = "  ";
    let words = [
        ("bcode", theme.text.add_modifier(Modifier::BOLD)),
        ("git", theme.accent.add_modifier(Modifier::BOLD)),
    ];
    let mut rows = Vec::new();
    for row in 0..4 {
        let mut spans = vec![Span::raw(INDENT)];
        for (word_index, (word, style)) in words.iter().enumerate() {
            if word_index > 0 {
                spans.push(Span::raw("   "));
            }
            for (index, letter) in word.chars().enumerate() {
                if index > 0 {
                    spans.push(Span::raw(" "));
                }
                if let Some((_, glyph)) = WORDMARK_GLYPHS.iter().find(|(name, _)| *name == letter) {
                    spans.push(Span::styled(glyph[row], *style));
                }
            }
        }
        // Narrow views drop the indent rather than clipping the letters.
        let compact = Line::from_spans(spans[1..].to_vec());
        let mut logo = DiffRow::fixed(Line::from_spans(spans));
        logo.compact = Some(compact);
        rows.push(logo);
    }
    rows
}

fn status_rows(repo: &Repository, cx: &PresentContext) -> Vec<DiffRow> {
    let theme = &cx.theme;
    let mut rows = vec![DiffRow::info(Line::new())];
    rows.extend(logo_rows(theme));
    rows.push(DiffRow::info(Line::new()));
    rows.extend(summary_rows(repo, cx));
    rows.push(DiffRow::info(Line::new()));
    rows.push(DiffRow::info(Line::from_spans(vec![Span::styled(
        "  Press ? for keybindings in any panel, ctrl+p for the command palette.",
        theme.muted,
    )])));
    rows
}

/// Repository, branch, changes, last commit and any operation in progress.
/// Branch and change counts of the overview.
fn status_summary(
    status: &bcode_git::models::StatusSnapshot,
    theme: &crate::theme::GitTheme,
) -> Vec<DiffRow> {
    let label = |icon: &str, name: &str| summary_label(theme, icon, name);
    let mut rows = Vec::new();
    let branch = status
        .branch
        .head
        .clone()
        .unwrap_or_else(|| "(detached)".into());
    rows.push(labeled(vec![
        label("⎇", "Branch"),
        Span::styled(branch, theme.branch_head),
        Span::styled(
            status.branch.upstream.as_ref().map_or_else(
                || "  no upstream".to_owned(),
                |upstream| format!("  → {upstream}"),
            ),
            theme.branch_remote,
        ),
        Span::styled(format!("  ↑{}", status.branch.ahead), theme.success),
        Span::styled(format!(" ↓{}", status.branch.behind), theme.warning),
    ]));
    let staged = status
        .entries
        .iter()
        .filter(|entry| entry.has_staged())
        .count();
    let unstaged = status
        .entries
        .iter()
        .filter(|entry| entry.has_unstaged() && !entry.is_untracked())
        .count();
    let untracked = status
        .entries
        .iter()
        .filter(|entry| entry.is_untracked())
        .count();
    rows.push(labeled(vec![
        label("±", "Changes"),
        Span::styled(format!("{staged} staged"), theme.staged),
        Span::styled("  ·  ", theme.muted),
        Span::styled(format!("{unstaged} unstaged"), theme.unstaged),
        Span::styled("  ·  ", theme.muted),
        Span::styled(format!("{untracked} untracked"), theme.untracked),
    ]));
    rows
}

/// Labeled overview row; wrapped text continues under the value, not under the label.
fn labeled(spans: Vec<Span>) -> DiffRow {
    let mut row = DiffRow::info(Line::from_spans(spans));
    row.gutter_spans = 1;
    row
}

/// Overview label, padded to a shared column measured in terminal cells.
fn summary_label(theme: &crate::theme::GitTheme, icon: &str, name: &str) -> Span {
    let text = format!("  {icon} {name}");
    let pad = 16usize.saturating_sub(bmux_tui::text_width::display_width(&text));
    Span::styled(format!("{text}{}", " ".repeat(pad)), theme.muted)
}

fn summary_rows(repo: &Repository, cx: &PresentContext) -> Vec<DiffRow> {
    let theme = &cx.theme;
    let label = |icon: &str, name: &str| summary_label(theme, icon, name);
    let mut rows = Vec::new();
    let root = repo.root();
    let name = root.file_name().map_or_else(
        || root.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let location = root.parent().map(|parent| {
        let parent = parent.display().to_string();
        std::env::var("HOME")
            .ok()
            .filter(|home| !home.is_empty())
            .and_then(|home| parent.strip_prefix(&home).map(|rest| format!("~{rest}")))
            .unwrap_or(parent)
    });
    let mut repository = vec![
        label("⌂", "Repository"),
        Span::styled(name, theme.text.add_modifier(Modifier::BOLD)),
    ];
    if let Some(location) = location {
        repository.push(Span::styled(format!("  {location}"), theme.muted));
    }
    rows.push(labeled(repository));
    if let Ok(status) = repo.status() {
        rows.extend(status_summary(&status, theme));
    }
    if let Some(last) = repo
        .log(&LogQuery {
            limit: 1,
            ..LogQuery::default()
        })
        .ok()
        .and_then(|commits| commits.into_iter().next())
    {
        rows.push(labeled(vec![
            label("●", "Last commit"),
            Span::styled(last.short_oid.clone(), theme.sha),
            Span::styled(format!(" {}", last.summary), theme.text),
            Span::styled(
                format!(
                    "  {} · {}",
                    last.author_name,
                    format_time(last.commit_time, cx.relative_dates)
                ),
                theme.muted,
            ),
        ]));
    }
    let operation = repo.operation();
    if operation != RepoOperation::None {
        rows.push(labeled(vec![
            label("⚠", "In progress"),
            Span::styled(
                operation.label().to_uppercase(),
                theme.warning.add_modifier(Modifier::BOLD),
            ),
        ]));
    }
    rows
}

/// Paths of a commit or stash, for the commit files view.
///
/// # Errors
/// Returns git's message when the listing fails.
pub fn load_changed_files(
    repo: &Repository,
    target: &DiffTarget,
) -> Result<Vec<ChangedFile>, String> {
    repo.changed_files(target).map_err(|error| error.message())
}

/// Style for a status entry's two-letter code.
#[must_use]
pub const fn status_style(entry: &StatusEntry, theme: &GitTheme) -> (Style, Style) {
    if entry.is_conflicted() {
        return (theme.conflict, theme.conflict);
    }
    if entry.is_untracked() {
        return (theme.untracked, theme.untracked);
    }
    (theme.staged, theme.unstaged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_git::models::{ChangeKind, StatusEntryKind};

    fn status(paths: &[&str]) -> StatusSnapshot {
        StatusSnapshot {
            branch: bcode_git::models::BranchStatus::default(),
            entries: paths
                .iter()
                .map(|path| StatusEntry {
                    path: (*path).into(),
                    orig_path: None,
                    kind: StatusEntryKind::Ordinary,
                    index: ChangeKind::Modified,
                    worktree: ChangeKind::Unmodified,
                    submodule: false,
                })
                .collect(),
        }
    }

    #[test]
    fn tree_compresses_single_child_directories_and_collapses() {
        let status = status(&["src/ui/a.rs", "src/ui/b.rs", "README.md", "docs/x/y/z.md"]);
        let rows = file_rows(&status, true, &BTreeSet::new());
        let labels: Vec<String> = rows
            .iter()
            .map(|row| match row {
                FileNode::Dir { label, depth, .. } | FileNode::File { label, depth, .. } => {
                    format!("{depth}:{label}")
                }
            })
            .collect();
        assert_eq!(
            labels,
            [
                "0:docs/x/y",
                "1:z.md",
                "0:src/ui",
                "1:a.rs",
                "1:b.rs",
                "0:README.md"
            ]
        );
        let collapsed = file_rows(&status, true, &BTreeSet::from(["src/ui".to_owned()]));
        assert_eq!(collapsed.len(), 4);
        assert!(matches!(&collapsed[2], FileNode::Dir { entries, .. } if entries.len() == 2));
    }

    #[test]
    fn flat_list_is_sorted() {
        let rows = file_rows(&status(&["b", "a"]), false, &BTreeSet::new());
        assert!(matches!(&rows[0], FileNode::File { label, .. } if label == "a"));
    }

    #[test]
    fn dates_format_relative_and_absolute() {
        assert_eq!(format_time(0, false), "1970-01-01");
        assert_eq!(format_time(951_782_400, false), "2000-02-29");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(format_time(i64::try_from(now).unwrap() - 7_200, true), "2h");
    }
}
