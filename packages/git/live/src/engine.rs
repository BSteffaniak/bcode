//! Turns change events into the narrowest git queries, for every worktree some client cares
//! about.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use bcode_git::models::{LineStats, StatusSnapshot};
use bcode_git::{CommandLog, Repository};
use serde::{Deserialize, Serialize};

use crate::counts::{CountsMode, LineCounts, Modes};
use crate::scan::{ScannerKind, StatusScanner, scanner};
use crate::source::{WatchKind, WatchTarget};
use crate::worktree::{HeadMove, UntrackedLimits, WorktreeState, canonical};

/// Untracked files counted in a worktree a client shows.
const SHOWN_UNTRACKED: UntrackedLimits = UntrackedLimits {
    files: 2000,
    bytes: 4 * 1024 * 1024,
};
/// Untracked files counted in a worktree measured only for its total.
const OTHER_UNTRACKED: UntrackedLimits = UntrackedLimits {
    files: 500,
    bytes: 1024 * 1024,
};

/// What one client wants kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    /// How the client's own worktree's lines are counted.
    pub counts: CountsMode,
    /// Also keep the line totals of the repository's other worktrees.
    pub other_worktrees: bool,
    /// Most other worktrees kept.
    pub max_other_worktrees: usize,
    /// How full scans run.
    pub scanner: ScannerKind,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            counts: CountsMode::Off,
            other_worktrees: false,
            max_other_worktrees: 16,
            scanner: ScannerKind::Plain,
        }
    }
}

/// Work asked of live state besides reacting to change events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Request {
    /// Scan every worktree fully.
    Full,
    /// Check the index and `HEAD` of every worktree, as after running a git command.
    Check,
    /// Count the client's own worktree's lines this way from now on.
    Counts(CountsMode),
}

/// Status and line counts of a client's own worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeSnapshot {
    /// Status, entries sorted by path.
    pub status: StatusSnapshot,
    /// Line counts.
    pub counts: LineCounts,
    /// `HEAD` commit the counts were measured against, `None` while unborn.
    pub head: Option<String>,
}

/// Lines changed in another worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeTotal {
    /// Staged, unstaged and untracked changes together.
    pub total: LineStats,
}

/// What changed for one client since its last update.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveUpdate {
    /// The client's own worktree, when its status or counts changed.
    pub primary: Option<WorktreeSnapshot>,
    /// Other worktrees whose totals or `HEAD` changed, by the path git lists them under,
    /// with the `HEAD` they were measured against.
    pub totals: BTreeMap<PathBuf, (Option<String>, WorktreeTotal)>,
    /// Other worktrees no longer kept.
    pub removed: Vec<PathBuf>,
    /// Refs, stashes, tags, configuration, operations in progress or the worktree list
    /// changed; reload what shows them.
    pub metadata: bool,
    /// Files changed in the client's own worktree.
    pub files: bool,
    /// Failures, as messages.
    pub errors: Vec<String>,
    /// Work done so far by the engine serving the client.
    pub stats: EngineStats,
}

impl LiveUpdate {
    /// Whether nothing changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.primary.is_none()
            && self.totals.is_empty()
            && self.removed.is_empty()
            && !self.metadata
            && !self.files
            && self.errors.is_empty()
    }
}

/// Work an engine has done since it started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineStats {
    /// Change event paths received.
    pub events: u64,
    /// Full status scans of a worktree.
    pub full_scans: u64,
    /// Status checks narrowed to changed paths.
    pub path_checks: u64,
    /// Paths named by those checks.
    pub paths_checked: u64,
    /// Readings of index entries.
    pub index_reads: u64,
    /// Readings of `HEAD`.
    pub head_reads: u64,
    /// Readings of the branch header alone.
    pub header_reads: u64,
}

/// A change to the set of watched locations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchChange {
    /// Start watching.
    Watch(WatchTarget),
    /// Stop watching this path.
    Unwatch(PathBuf),
}

/// One client's interest in a repository.
#[derive(Debug, Clone)]
pub struct Interest {
    /// The client's own worktree.
    pub repo: Repository,
    /// What it wants kept.
    pub config: EngineConfig,
}

/// What one round of work changed, for [`crate::Subscriber`]s to report.
#[derive(Debug, Clone, Default)]
pub struct Applied {
    /// Worktrees (as change events name their roots) with changed files.
    pub files: BTreeSet<PathBuf>,
    /// Git metadata changed.
    pub metadata: bool,
    /// Failures, with the worktree they concern (`None`: the whole repository).
    pub errors: Vec<(Option<PathBuf>, String)>,
    /// Worktrees no longer kept, by the path git lists them under.
    pub removed: Vec<PathBuf>,
}

/// Work gathered from change events and requests, applied together.
#[derive(Debug, Default)]
// Each flag is an independent kind of pending work.
#[allow(clippy::struct_excessive_bools)]
pub struct Pending {
    full: bool,
    full_roots: BTreeSet<PathBuf>,
    paths: BTreeMap<PathBuf, BTreeSet<String>>,
    ignore_dirs: BTreeMap<PathBuf, BTreeSet<String>>,
    index: BTreeSet<PathBuf>,
    heads: BTreeSet<PathBuf>,
    /// Local branches moved: check every `HEAD`.
    refs: bool,
    /// Upstream refs moved: read every branch header.
    remote: bool,
    metadata: bool,
    worktree_list: bool,
    check: bool,
}

impl Pending {
    /// Whether nothing is pending.
    pub fn is_empty(&self) -> bool {
        !self.full
            && self.full_roots.is_empty()
            && self.paths.is_empty()
            && self.ignore_dirs.is_empty()
            && self.index.is_empty()
            && self.heads.is_empty()
            && !self.refs
            && !self.remote
            && !self.metadata
            && !self.worktree_list
            && !self.check
    }

    /// Add a request that concerns every worktree. Per-client requests
    /// ([`Request::Counts`]) change interests instead and are ignored here.
    pub const fn request(&mut self, request: Request) {
        match request {
            Request::Full => {
                self.full = true;
                self.worktree_list = true;
            }
            Request::Check => self.check = true,
            Request::Counts(_) => {}
        }
    }
}

/// Live state of one repository's worktrees: those clients show in detail, others as
/// totals. Worktrees are keyed by their root as change events name it.
pub struct Engine {
    scanner: Box<dyn StatusScanner>,
    log: CommandLog,
    /// A worktree of the repository, to list the others.
    repo: Repository,
    /// Common git directory, as change events name it.
    common_dir: PathBuf,
    worktrees: BTreeMap<PathBuf, WorktreeState>,
    interests: Vec<Interest>,
    watch_changes: Vec<WatchChange>,
    /// The kept set needs working out again before the next round.
    unsynced: bool,
    stats: EngineStats,
    /// Results of working out the kept set, for the next round's report.
    carried: Applied,
}

impl Engine {
    /// An engine for the repository `repo` belongs to, keeping nothing until interests are
    /// set.
    #[must_use]
    pub fn new(repo: &Repository, scanner_kind: ScannerKind) -> Self {
        let common_dir = canonical(&repo.paths().common_dir);
        Self {
            scanner: scanner(scanner_kind),
            log: CommandLog::default(),
            repo: repo.clone(),
            watch_changes: vec![WatchChange::Watch(WatchTarget {
                path: common_dir.clone(),
                kind: WatchKind::GitDir,
            })],
            common_dir,
            worktrees: BTreeMap::new(),
            interests: Vec::new(),
            unsynced: true,
            stats: EngineStats::default(),
            carried: Applied::default(),
        }
    }

    /// Name of the full-scan strategy in use.
    #[must_use]
    pub fn scanner_name(&self) -> &'static str {
        self.scanner.name()
    }

    /// Use this full-scan strategy from now on.
    pub fn set_scanner(&mut self, kind: ScannerKind) {
        self.scanner = scanner(kind);
    }

    /// Work done so far.
    #[must_use]
    pub const fn stats(&self) -> EngineStats {
        self.stats
    }

    /// Keep what `interests` ask for (all clients' interests together). Worktrees newly
    /// kept, or counted differently, are scanned in the next round.
    pub fn set_interests(&mut self, interests: Vec<Interest>) {
        self.interests = interests;
        self.unsynced = true;
    }

    /// Locations to start or stop watching since last asked.
    pub fn take_watch_changes(&mut self) -> Vec<WatchChange> {
        std::mem::take(&mut self.watch_changes)
    }

    /// The root (as change events name it) of the worktree containing `repo`.
    #[must_use]
    pub fn root_of(repo: &Repository) -> PathBuf {
        canonical(repo.root())
    }

    /// Status and counts of the worktree at `root`, counted as `counts`; `None` until
    /// scanned.
    #[must_use]
    pub fn snapshot(&self, root: &Path, counts: CountsMode) -> Option<WorktreeSnapshot> {
        let worktree = self
            .worktrees
            .get(root)
            .filter(|worktree| worktree.loaded)?;
        Some(WorktreeSnapshot {
            status: worktree.status(),
            counts: worktree.counts(counts),
            head: worktree.head().map(str::to_owned),
        })
    }

    /// Totals of every scanned worktree: the path git lists it under, its root, its `HEAD`
    /// and its total.
    pub fn totals(&self) -> impl Iterator<Item = (&Path, &Path, WorktreeTotalAt)> {
        self.worktrees
            .iter()
            .filter(|(_, worktree)| worktree.loaded)
            .map(|(root, worktree)| {
                (
                    worktree.listed.as_deref().unwrap_or(&worktree.root),
                    root.as_path(),
                    (
                        worktree.head().map(str::to_owned),
                        WorktreeTotal {
                            total: worktree.total(),
                        },
                    ),
                )
            })
    }

    /// Record what a change at `path` (as change events name it) needs.
    pub fn route(&mut self, path: &Path, pending: &mut Pending) {
        self.stats.events += 1;
        if let Ok(relative) = path.strip_prefix(&self.common_dir) {
            let mut parts: Vec<String> = relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect();
            // git writes `X` by renaming `X.lock` over it, and a filesystem may report only
            // the lock's name for that; either way `X` changed.
            if let Some(last) = parts.last_mut()
                && let Some(base) = last.strip_suffix(".lock")
            {
                *last = base.to_owned();
            }
            self.route_git(&parts, pending);
            return;
        }
        let Some((root, worktree)) = self
            .worktrees
            .iter_mut()
            .filter(|(root, _)| path.starts_with(root))
            .max_by_key(|(root, _)| root.components().count())
        else {
            return;
        };
        let Ok(relative) = path.strip_prefix(root) else {
            return;
        };
        let Some(first) = relative.components().next() else {
            return;
        };
        if first.as_os_str() == ".git" {
            return;
        }
        let text = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let name = relative.file_name().unwrap_or_default();
        if name == ".gitignore" {
            let parent = text
                .rsplit_once('/')
                .map(|(parent, _)| parent.to_owned())
                .unwrap_or_default();
            pending
                .ignore_dirs
                .entry(root.clone())
                .or_default()
                .insert(parent);
        } else if name == ".gitmodules" {
            pending.metadata = true;
        } else if worktree.filter.is_ignored(relative, path.is_dir()) {
            return;
        }
        pending.paths.entry(root.clone()).or_default().insert(text);
    }

    fn route_git(&self, parts: &[String], pending: &mut Pending) {
        let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
        match parts.as_slice() {
            ["worktrees"] | ["worktrees", _] => {
                pending.worktree_list = true;
                pending.metadata = true;
            }
            ["worktrees", name, rest @ ..] => {
                let git_dir = self.common_dir.join("worktrees").join(name);
                if let Some(root) = self.root_of_git_dir(&git_dir) {
                    route_own(&root, rest, pending);
                } else if matches!(rest, ["gitdir" | "commondir" | "HEAD"]) {
                    // A worktree being added.
                    pending.worktree_list = true;
                    pending.metadata = true;
                }
            }
            ["refs", "heads", ..] | ["packed-refs"] => {
                pending.refs = true;
                pending.metadata = true;
            }
            ["refs", "remotes", ..] | ["FETCH_HEAD"] => {
                pending.remote = true;
                pending.metadata = true;
            }
            ["refs" | "modules", ..] | ["config"] | ["logs", "refs", "stash"] => {
                pending.metadata = true;
            }
            ["info", "exclude"] => {
                for root in self.worktrees.keys() {
                    pending
                        .ignore_dirs
                        .entry(root.clone())
                        .or_default()
                        .insert(String::new());
                }
            }
            [] | ["objects" | "logs" | "hooks" | "lfs" | "info", ..] => {}
            rest => {
                if let Some(root) = self.root_of_git_dir(&self.common_dir) {
                    route_own(&root, rest, pending);
                } else if matches!(rest, ["HEAD" | "index"]) {
                    pending.metadata = true;
                }
            }
        }
    }

    fn root_of_git_dir(&self, git_dir: &Path) -> Option<PathBuf> {
        self.worktrees
            .iter()
            .find(|(_, worktree)| worktree.git_dir == git_dir)
            .map(|(root, _)| root.clone())
    }

    /// Record that events under `path` (everywhere when `None`) were lost.
    pub fn rescan(&self, path: Option<&Path>, pending: &mut Pending) {
        let Some(path) = path else {
            pending.full = true;
            pending.metadata = true;
            pending.worktree_list = true;
            return;
        };
        if path.starts_with(&self.common_dir) {
            pending.check = true;
            pending.remote = true;
            pending.metadata = true;
            pending.worktree_list = true;
            return;
        }
        match self
            .worktrees
            .keys()
            .filter(|root| path.starts_with(root))
            .max_by_key(|root| root.components().count())
        {
            Some(root) => {
                pending.full_roots.insert(root.clone());
            }
            None => pending.full = true,
        }
    }

    /// Bring the set of kept worktrees up to date before `pending` is applied, so their
    /// watches can start before they are scanned. [`Self::apply`] does this itself when
    /// needed.
    pub fn prepare(&mut self, pending: &mut Pending) {
        if pending.worktree_list || self.unsynced {
            self.sync_worktrees();
        }
        pending.worktree_list = false;
    }

    /// Carry out `pending`.
    pub fn apply(&mut self, mut pending: Pending) -> Applied {
        self.prepare(&mut pending);
        let mut applied = std::mem::take(&mut self.carried);
        applied.metadata |= pending.metadata;
        applied.files = pending.paths.keys().cloned().collect();
        let roots: Vec<PathBuf> = self.worktrees.keys().cloned().collect();
        for root in roots {
            if let Err(error) = self.apply_to(&root, &mut pending) {
                applied.errors.push((Some(root), error.message()));
            }
        }
        applied
    }

    fn apply_to(
        &mut self,
        root: &PathBuf,
        pending: &mut Pending,
    ) -> Result<(), bcode_git::GitError> {
        let Some(worktree) = self.worktrees.get_mut(root) else {
            return Ok(());
        };
        let narrowed = !(pending.full || pending.full_roots.contains(root) || !worktree.loaded)
            && narrowed(worktree, root, pending, &mut self.stats)?;
        if !narrowed {
            self.stats.full_scans += 1;
            worktree.full(self.scanner.as_mut())?;
        }
        Ok(())
    }

    /// Keep the worktrees interests ask for, and watch them.
    fn sync_worktrees(&mut self) {
        self.unsynced = false;
        // Worktrees shown in detail, with how their lines are counted.
        let mut wanted: BTreeMap<PathBuf, (Repository, Modes, UntrackedLimits)> = BTreeMap::new();
        for interest in &self.interests {
            let root = Self::root_of(&interest.repo);
            let entry = wanted
                .entry(root)
                .or_insert_with(|| (interest.repo.clone(), Modes::default(), SHOWN_UNTRACKED));
            entry.1.add(interest.config.counts);
        }
        // The rest of the repository's worktrees, measured for their totals.
        let others = self
            .interests
            .iter()
            .filter(|interest| interest.config.other_worktrees)
            .map(|interest| interest.config.max_other_worktrees)
            .max();
        let mut listed: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
        if let Some(max) = others {
            match self.repo.worktrees() {
                Ok(list) => {
                    let mut added = 0;
                    for worktree in list {
                        if worktree.bare || worktree.prunable {
                            continue;
                        }
                        let root = canonical(&worktree.path);
                        listed.insert(root.clone(), worktree.path.clone());
                        if wanted.contains_key(&root) || added >= max {
                            continue;
                        }
                        let repo = match self.worktrees.get(&root) {
                            Some(existing) => existing.repo.clone(),
                            None => match Repository::discover(&worktree.path, self.log.clone()) {
                                Ok(repo) => repo,
                                Err(_) => continue,
                            },
                        };
                        added += 1;
                        wanted.insert(root, (repo, Modes::total(), OTHER_UNTRACKED));
                    }
                }
                Err(error) => self.carried.errors.push((None, error.message())),
            }
        }
        if others.is_some() {
            // Every kept worktree may be another client's "other": measure its total.
            for (_, modes, _) in wanted.values_mut() {
                modes.total = true;
            }
        }
        let stale: Vec<PathBuf> = self
            .worktrees
            .keys()
            .filter(|root| !wanted.contains_key(*root))
            .cloned()
            .collect();
        for root in stale {
            if let Some(worktree) = self.worktrees.remove(&root) {
                self.watch_changes.push(WatchChange::Unwatch(root));
                self.carried
                    .removed
                    .push(worktree.listed.unwrap_or(worktree.root));
            }
        }
        for (root, (repo, modes, limits)) in wanted {
            let worktree = self.worktrees.entry(root.clone()).or_insert_with(|| {
                self.watch_changes.push(WatchChange::Watch(WatchTarget {
                    path: root.clone(),
                    kind: WatchKind::Worktree {
                        common_dir: self.common_dir.clone(),
                    },
                }));
                WorktreeState::new(repo, &self.common_dir, modes, limits)
            });
            worktree.set_counting(modes, limits);
            worktree.listed = listed.get(&root).cloned();
        }
    }
}

/// A worktree's `HEAD` and total.
pub type WorktreeTotalAt = (Option<String>, WorktreeTotal);

/// Bring `worktree` up to date with narrowed queries; `false` when it needs a full scan.
fn narrowed(
    worktree: &mut WorktreeState,
    root: &PathBuf,
    pending: &mut Pending,
    stats: &mut EngineStats,
) -> Result<bool, bcode_git::GitError> {
    let mut paths = pending.paths.remove(root).unwrap_or_default();
    if let Some(directories) = pending.ignore_dirs.remove(root) {
        worktree.filter.reset();
        paths.extend(directories);
    }
    if pending.check || pending.index.contains(root) {
        stats.index_reads += 1;
        worktree.await_unlocked("index");
        match worktree.index_changes()? {
            Some(changed) => paths.extend(changed),
            None => return Ok(false),
        }
    }
    let mut header = pending.remote;
    if pending.check || pending.refs || pending.heads.contains(root) {
        header = true;
        stats.head_reads += 1;
        worktree.await_unlocked("HEAD");
        match worktree.head_move()? {
            HeadMove::Same => {}
            HeadMove::Paths(changed) => paths.extend(changed),
            HeadMove::Unknown => return Ok(false),
        }
    }
    if !paths.is_empty() {
        stats.path_checks += 1;
        stats.paths_checked += paths.len() as u64;
        return worktree.verify(&paths);
    }
    if header {
        stats.header_reads += 1;
        worktree.header()?;
    }
    Ok(true)
}

/// Route a change inside one worktree's own git directory.
fn route_own(root: &Path, parts: &[&str], pending: &mut Pending) {
    match parts {
        ["index"] => {
            pending.index.insert(root.to_owned());
        }
        ["HEAD"] => {
            pending.heads.insert(root.to_owned());
            pending.metadata = true;
        }
        ["MERGE_HEAD" | "CHERRY_PICK_HEAD" | "REVERT_HEAD" | "REBASE_HEAD" | "BISECT_LOG"]
        | ["rebase-merge" | "rebase-apply" | "sequencer", ..]
        | ["logs", "HEAD"] => pending.metadata = true,
        _ => {}
    }
}
