//! Turns change events into the narrowest git queries and reports what changed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use bcode_git::models::{LineStats, StatusSnapshot};
use bcode_git::{CommandLog, Repository};

use crate::counts::{CountsMode, LineCounts};
use crate::scan::{ScannerKind, StatusScanner, scanner};
use crate::source::{WatchKind, WatchTarget};
use crate::worktree::{HeadMove, UntrackedLimits, WorktreeState, canonical};

/// Untracked files counted in the worktree being shown.
const PRIMARY_UNTRACKED: UntrackedLimits = UntrackedLimits {
    files: 2000,
    bytes: 4 * 1024 * 1024,
};
/// Untracked files counted in each other worktree.
const OTHER_UNTRACKED: UntrackedLimits = UntrackedLimits {
    files: 500,
    bytes: 1024 * 1024,
};

/// What an [`Engine`] keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineConfig {
    /// How the primary worktree's lines are counted.
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

/// Work asked of an engine besides reacting to change events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Scan every worktree fully.
    Full,
    /// Check the index and `HEAD` of every worktree, as after running a git command.
    Check,
    /// Count the primary worktree's lines this way from now on.
    Counts(CountsMode),
}

/// Status and line counts of the primary worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSnapshot {
    /// Status, entries sorted by path.
    pub status: StatusSnapshot,
    /// Line counts.
    pub counts: LineCounts,
    /// `HEAD` commit, `None` while unborn.
    pub head: Option<String>,
}

/// Lines changed in another worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorktreeTotal {
    /// Staged, unstaged and untracked changes together.
    pub total: LineStats,
}

/// What changed since the last update.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveUpdate {
    /// The primary worktree, when its status or counts changed.
    pub primary: Option<WorktreeSnapshot>,
    /// Other worktrees whose totals or `HEAD` changed, by root, with the `HEAD` they were
    /// measured against.
    pub totals: BTreeMap<PathBuf, (Option<String>, WorktreeTotal)>,
    /// Other worktrees no longer kept.
    pub removed: Vec<PathBuf>,
    /// Refs, stashes, tags, configuration, operations in progress or the worktree list
    /// changed; reload what shows them.
    pub metadata: bool,
    /// Files changed in the primary worktree.
    pub files: bool,
    /// Failures, as messages.
    pub errors: Vec<String>,
    /// Work done so far.
    pub stats: EngineStats,
}

/// Work an engine has done since it started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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

/// A change to the set of watched locations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchChange {
    /// Start watching.
    Watch(WatchTarget),
    /// Stop watching this path.
    Unwatch(PathBuf),
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
    counts: Option<CountsMode>,
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
            && self.counts.is_none()
    }

    pub const fn request(&mut self, request: Request) {
        match request {
            Request::Full => {
                self.full = true;
                self.worktree_list = true;
            }
            Request::Check => self.check = true,
            Request::Counts(mode) => self.counts = Some(mode),
        }
    }
}

/// Live state of a repository's worktrees: the primary one in detail, others as totals.
pub struct Engine {
    config: EngineConfig,
    scanner: Box<dyn StatusScanner>,
    log: CommandLog,
    /// Common git directory, as change events name it.
    common_dir: PathBuf,
    primary: PathBuf,
    worktrees: BTreeMap<PathBuf, WorktreeState>,
    watch_changes: Vec<WatchChange>,
    synced: bool,
    stats: EngineStats,
    /// Removals and failures from keeping other worktrees, for the next update.
    carried: LiveUpdate,
    last_primary: Option<WorktreeSnapshot>,
    last_totals: BTreeMap<PathBuf, (Option<String>, WorktreeTotal)>,
}

impl Engine {
    /// An engine for the worktree of `repo`. Nothing runs until the first update.
    #[must_use]
    pub fn new(repo: &Repository, config: EngineConfig) -> Self {
        let common_dir = canonical(&repo.paths().common_dir);
        let primary =
            WorktreeState::new(repo.clone(), &common_dir, config.counts, PRIMARY_UNTRACKED);
        let root = primary.root.clone();
        let watch_changes = vec![
            WatchChange::Watch(WatchTarget {
                path: primary.watch_root.clone(),
                kind: WatchKind::Worktree {
                    common_dir: common_dir.clone(),
                },
            }),
            WatchChange::Watch(WatchTarget {
                path: common_dir.clone(),
                kind: WatchKind::GitDir,
            }),
        ];
        Self {
            config,
            scanner: scanner(config.scanner),
            log: CommandLog::default(),
            common_dir,
            primary: root.clone(),
            worktrees: BTreeMap::from([(root, primary)]),
            watch_changes,
            synced: false,
            stats: EngineStats::default(),
            carried: LiveUpdate::default(),
            last_primary: None,
            last_totals: BTreeMap::new(),
        }
    }

    /// Name of the full-scan strategy in use.
    #[must_use]
    pub fn scanner_name(&self) -> &'static str {
        self.scanner.name()
    }

    /// Locations to start or stop watching since last asked.
    pub fn take_watch_changes(&mut self) -> Vec<WatchChange> {
        std::mem::take(&mut self.watch_changes)
    }

    /// Carry out `request` now.
    pub fn request(&mut self, request: Request) -> LiveUpdate {
        let mut pending = Pending::default();
        pending.request(request);
        self.apply(pending)
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
        let Some(worktree) = self
            .worktrees
            .values_mut()
            .filter(|worktree| path.starts_with(&worktree.watch_root))
            .max_by_key(|worktree| worktree.watch_root.components().count())
        else {
            return;
        };
        let Ok(relative) = path.strip_prefix(&worktree.watch_root) else {
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
                .entry(worktree.root.clone())
                .or_default()
                .insert(parent);
        } else if name == ".gitmodules" {
            pending.metadata = true;
        } else if worktree.filter.is_ignored(relative, path.is_dir()) {
            return;
        }
        pending
            .paths
            .entry(worktree.root.clone())
            .or_default()
            .insert(text);
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
            .values()
            .find(|worktree| worktree.git_dir == git_dir)
            .map(|worktree| worktree.root.clone())
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
            .values()
            .filter(|worktree| path.starts_with(&worktree.watch_root))
            .max_by_key(|worktree| worktree.watch_root.components().count())
        {
            Some(worktree) => {
                pending.full_roots.insert(worktree.root.clone());
            }
            None => pending.full = true,
        }
    }

    /// Bring the set of kept worktrees up to date before `pending` is applied, so their
    /// watches can start before they are scanned. [`Self::apply`] does this itself when
    /// needed.
    pub fn prepare(&mut self, pending: &mut Pending) {
        if pending.worktree_list || !self.synced {
            self.sync_worktrees();
        }
        pending.worktree_list = false;
    }

    /// Carry out `pending`.
    pub fn apply(&mut self, mut pending: Pending) -> LiveUpdate {
        self.prepare(&mut pending);
        let mut update = std::mem::take(&mut self.carried);
        update.metadata |= pending.metadata;
        update.files = pending.paths.contains_key(&self.primary);
        if let Some(mode) = pending.counts
            && let Some(primary) = self.worktrees.get_mut(&self.primary)
            && primary.counts_mode != mode
        {
            primary.counts_mode = mode;
            pending.full_roots.insert(self.primary.clone());
        }
        let roots: Vec<PathBuf> = self.worktrees.keys().cloned().collect();
        for root in roots {
            if let Err(error) = self.apply_to(&root, &mut pending) {
                let message = error.message();
                if root == self.primary {
                    update.errors.push(message);
                }
            }
        }
        self.report(&mut update);
        update
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

    /// Keep the repository's other worktrees (as configured) and watch them.
    fn sync_worktrees(&mut self) {
        self.synced = true;
        let update = &mut self.carried;
        let wanted: Vec<PathBuf> = if self.config.other_worktrees {
            let Some(primary) = self.worktrees.get(&self.primary) else {
                return;
            };
            match primary.repo.worktrees() {
                Ok(list) => list
                    .into_iter()
                    .filter(|worktree| !worktree.bare && !worktree.prunable)
                    .map(|worktree| worktree.path)
                    .collect(),
                Err(error) => {
                    update.errors.push(error.message());
                    return;
                }
            }
        } else {
            Vec::new()
        };
        let primary_root = self
            .worktrees
            .get(&self.primary)
            .map(|primary| primary.watch_root.clone())
            .unwrap_or_default();
        let wanted: Vec<PathBuf> = wanted
            .into_iter()
            .filter(|path| *path != self.primary && canonical(path) != primary_root)
            .take(self.config.max_other_worktrees)
            .collect();
        let stale: Vec<PathBuf> = self
            .worktrees
            .keys()
            .filter(|root| **root != self.primary && !wanted.contains(root))
            .cloned()
            .collect();
        for root in stale {
            if let Some(worktree) = self.worktrees.remove(&root) {
                self.watch_changes
                    .push(WatchChange::Unwatch(worktree.watch_root));
            }
            self.last_totals.remove(&root);
            update.removed.push(root);
        }
        for path in wanted {
            if self.worktrees.contains_key(&path) {
                continue;
            }
            let Ok(repo) = Repository::discover(&path, self.log.clone()) else {
                continue;
            };
            let worktree =
                WorktreeState::new(repo, &self.common_dir, CountsMode::Total, OTHER_UNTRACKED);
            self.watch_changes.push(WatchChange::Watch(WatchTarget {
                path: worktree.watch_root.clone(),
                kind: WatchKind::Worktree {
                    common_dir: self.common_dir.clone(),
                },
            }));
            // Keyed by the path git lists, which is how callers name worktrees.
            self.worktrees.insert(path, worktree);
        }
    }

    /// Fill `update` with what differs from the last report.
    fn report(&mut self, update: &mut LiveUpdate) {
        update.stats = self.stats;
        for (root, worktree) in &self.worktrees {
            if !worktree.loaded {
                continue;
            }
            if *root == self.primary {
                let snapshot = WorktreeSnapshot {
                    status: worktree.status(),
                    counts: worktree.counts(),
                    head: worktree.head().map(str::to_owned),
                };
                if self.last_primary.as_ref() != Some(&snapshot) {
                    self.last_primary = Some(snapshot.clone());
                    update.primary = Some(snapshot);
                }
            } else {
                let measured = (
                    worktree.head().map(str::to_owned),
                    WorktreeTotal {
                        total: worktree.counts().total(),
                    },
                );
                if self.last_totals.get(root) != Some(&measured) {
                    self.last_totals.insert(root.clone(), measured.clone());
                    update.totals.insert(root.clone(), measured);
                }
            }
        }
    }
}

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
