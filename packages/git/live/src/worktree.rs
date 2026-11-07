//! One worktree's live status and line counts, updated by the narrowest git queries.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use bcode_git::models::{BranchStatus, LineStats, StatusEntry, StatusSnapshot};
use bcode_git::{CountTarget, GitError, Repository, StatusScope};

use crate::counts::{CountsMode, LineCounts, Modes};
use crate::filter::IgnoreFilter;
use crate::scan::StatusScanner;

/// Longest wait for git to finish writing a file it holds locked.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

/// Most paths one narrowed query names; more are widened to their directories.
const PATHS_PER_QUERY: usize = 256;

/// Limits on counting untracked files, which git does not count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UntrackedLimits {
    /// Files counted; the rest show no count.
    pub files: usize,
    /// Largest file counted, in bytes.
    pub bytes: u64,
}

/// How `HEAD` moved since it was last seen.
pub enum HeadMove {
    /// It did not.
    Same,
    /// Between two commits that differ in these paths.
    Paths(Vec<String>),
    /// To or from an unborn branch: everything is checked again.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CachedCount {
    modified: Option<SystemTime>,
    len: u64,
    stats: LineStats,
}

pub struct WorktreeState {
    pub repo: Repository,
    /// Root as git reports it.
    pub root: PathBuf,
    /// Own git directory, as change events name it.
    pub git_dir: PathBuf,
    /// Path `git worktree list` gives, when listed.
    pub listed: Option<PathBuf>,
    modes: Modes,
    limits: UntrackedLimits,
    pub filter: IgnoreFilter,
    pub loaded: bool,
    branch: BranchStatus,
    entries: BTreeMap<String, StatusEntry>,
    /// `HEAD` to worktree, for [`CountsMode::Total`].
    total: BTreeMap<String, LineStats>,
    /// `HEAD` to index, for [`CountsMode::Split`].
    staged: BTreeMap<String, LineStats>,
    /// Index to worktree, for [`CountsMode::Split`].
    unstaged: BTreeMap<String, LineStats>,
    untracked: BTreeMap<String, CachedCount>,
    /// Index fingerprints, to tell which entries an index rewrite changed.
    index: Option<BTreeMap<String, u64>>,
    head: Option<String>,
}

impl WorktreeState {
    pub fn new(repo: Repository, common_dir: &Path, modes: Modes, limits: UntrackedLimits) -> Self {
        let root = repo.root().to_owned();
        let watch_root = canonical(&root);
        let git_dir = canonical(&repo.paths().git_dir);
        Self {
            filter: IgnoreFilter::new(&watch_root, common_dir),
            repo,
            root,
            git_dir,
            listed: None,
            modes,
            limits,
            loaded: false,
            branch: BranchStatus::default(),
            entries: BTreeMap::new(),
            total: BTreeMap::new(),
            staged: BTreeMap::new(),
            unstaged: BTreeMap::new(),
            untracked: BTreeMap::new(),
            index: None,
            head: None,
        }
    }

    /// Scan everything again.
    pub fn full(&mut self, scanner: &mut dyn StatusScanner) -> Result<(), GitError> {
        let repo = &self.repo;
        let modes = self.modes;
        // Readings that tell what changed later are taken before the status they stand for,
        // so they are never newer than it: a change between the two is seen again, not lost.
        // Each step's independent queries run at once.
        let (index, head) = std::thread::scope(|scope| {
            let head = scope.spawn(|| repo.rev_parse("HEAD").ok());
            let index = repo.index_fingerprints(None).ok();
            (index, head.join().ok().flatten())
        });
        let (status, total, staged, unstaged) = std::thread::scope(|scope| {
            let count = |wanted: bool, target| {
                wanted.then(|| scope.spawn(move || repo.numstat_scoped(target, None)))
            };
            let total = count(modes.total, CountTarget::Worktree);
            let staged = count(modes.split, CountTarget::Staged);
            let unstaged = count(modes.split, CountTarget::Unstaged);
            let joined = |handle: Option<std::thread::ScopedJoinHandle<'_, _>>| {
                handle.map_or_else(
                    || Ok(BTreeMap::new()),
                    |handle| {
                        handle.join().unwrap_or_else(|_| {
                            Err(GitError::Io(std::io::Error::other(
                                "line counting panicked",
                            )))
                        })
                    },
                )
            };
            (
                scanner.full_status(repo),
                joined(total),
                joined(staged),
                joined(unstaged),
            )
        });
        let status = status?;
        self.loaded = true;
        self.index = index;
        self.head = head;
        self.branch = status.branch;
        self.entries = status
            .entries
            .into_iter()
            .map(|entry| (entry.path.clone(), entry))
            .collect();
        self.total = total?;
        self.staged = staged?;
        self.unstaged = unstaged?;
        self.recount_untracked(None);
        Ok(())
    }

    /// Check `paths` (relative; directories cover everything beneath) again. Returns
    /// `false` when they are too widespread to narrow and need a full scan instead.
    pub fn verify(&mut self, paths: &BTreeSet<String>) -> Result<bool, GitError> {
        let Some(mut paths) = narrow(paths) else {
            return Ok(false);
        };
        // Renames pair two paths; check both sides so the pair is seen again.
        for entry in self.entries.values() {
            if let Some(orig) = &entry.orig_path
                && (covers(&paths, &entry.path) || covers(&paths, orig))
            {
                paths.insert(entry.path.clone());
                paths.insert(orig.clone());
            }
        }
        let Some(paths) = narrow(&paths) else {
            return Ok(false);
        };
        let list: Vec<String> = paths.iter().cloned().collect();
        // As in a full scan, the index reading for these paths comes first. `HEAD` is left
        // alone: only a reading of how it moved may advance it, so no move goes unchecked.
        if self.index.is_some() {
            let fresh = self.repo.index_fingerprints(Some(&list))?;
            if let Some(index) = &mut self.index {
                index.retain(|path, _| !covers(&paths, path));
                index.extend(fresh);
            }
        }
        let status = self.repo.status_scoped(StatusScope::Paths(&list))?;
        self.branch = status.branch;
        self.entries.retain(|path, entry| {
            !(covers(&paths, path)
                || entry
                    .orig_path
                    .as_ref()
                    .is_some_and(|orig| covers(&paths, orig)))
        });
        self.entries.extend(
            status
                .entries
                .into_iter()
                .map(|entry| (entry.path.clone(), entry)),
        );
        let recount = |target, map: &mut BTreeMap<String, LineStats>| -> Result<(), GitError> {
            let counts = self.repo.numstat_scoped(target, Some(&list))?;
            map.retain(|path, _| !covers(&paths, path));
            map.extend(counts);
            Ok(())
        };
        if self.modes.total {
            recount(CountTarget::Worktree, &mut self.total)?;
        }
        if self.modes.split {
            recount(CountTarget::Staged, &mut self.staged)?;
            recount(CountTarget::Unstaged, &mut self.unstaged)?;
        }
        self.recount_untracked(Some(&paths));
        Ok(true)
    }

    /// Paths whose index entries changed since last seen; `None` without an earlier
    /// reading to compare with.
    pub fn index_changes(&mut self) -> Result<Option<BTreeSet<String>>, GitError> {
        let current = self.repo.index_fingerprints(None)?;
        let Some(previous) = self.index.replace(current) else {
            return Ok(None);
        };
        let current = self.index.as_ref().unwrap_or(&previous);
        let mut changed: BTreeSet<String> = previous
            .iter()
            .filter(|(path, fingerprint)| current.get(*path) != Some(fingerprint))
            .map(|(path, _)| path.clone())
            .collect();
        changed.extend(
            current
                .keys()
                .filter(|path| !previous.contains_key(*path))
                .cloned(),
        );
        Ok(Some(changed))
    }

    /// How `HEAD` moved since last seen.
    pub fn head_move(&mut self) -> Result<HeadMove, GitError> {
        let current = self.repo.rev_parse("HEAD").ok();
        if current == self.head {
            return Ok(HeadMove::Same);
        }
        let previous = std::mem::replace(&mut self.head, current.clone());
        Ok(match (previous, current) {
            (Some(from), Some(to)) => HeadMove::Paths(self.repo.paths_between(&from, &to)?),
            _ => HeadMove::Unknown,
        })
    }

    /// Wait briefly (at most [`LOCK_WAIT`]) while git holds the lock on `file` of this
    /// worktree's git directory, so a write in progress is read once finished.
    pub fn await_unlocked(&self, file: &str) {
        let lock = self.git_dir.join(format!("{file}.lock"));
        let deadline = std::time::Instant::now() + LOCK_WAIT;
        while lock.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// Read the branch header (upstream and ahead/behind) again.
    pub fn header(&mut self) -> Result<(), GitError> {
        self.branch = self.repo.status_scoped(StatusScope::Header)?.branch;
        Ok(())
    }

    pub fn head(&self) -> Option<&str> {
        self.head.as_deref()
    }

    pub fn status(&self) -> StatusSnapshot {
        StatusSnapshot {
            branch: self.branch.clone(),
            entries: self.entries.values().cloned().collect(),
        }
    }

    /// Count lines as `modes` say (for every client showing this worktree) and untracked
    /// files within `limits`; a change takes a full scan to apply.
    pub fn set_counting(&mut self, modes: Modes, limits: UntrackedLimits) {
        if self.modes != modes || self.limits != limits {
            self.modes = modes;
            self.limits = limits;
            self.loaded = false;
        }
    }

    /// Line counts as `mode` measures them; [`LineCounts::Off`] when not counted that way.
    pub fn counts(&self, mode: CountsMode) -> LineCounts {
        let untracked = self
            .untracked
            .iter()
            .map(|(path, cached)| (path.clone(), cached.stats));
        match mode {
            CountsMode::Total if self.modes.total => {
                let mut total = self.total.clone();
                total.extend(untracked);
                LineCounts::Total(total)
            }
            CountsMode::Split if self.modes.split => {
                let mut unstaged = self.unstaged.clone();
                unstaged.extend(untracked);
                LineCounts::Split {
                    staged: self.staged.clone(),
                    unstaged,
                }
            }
            _ => LineCounts::Off,
        }
    }

    /// Every changed line, staged, unstaged and untracked together.
    pub fn total(&self) -> LineStats {
        let tracked: LineStats = if self.modes.total {
            self.total.values().copied().sum()
        } else {
            self.staged
                .values()
                .chain(self.unstaged.values())
                .copied()
                .sum()
        };
        std::iter::once(tracked)
            .chain(self.untracked.values().map(|cached| cached.stats))
            .sum()
    }

    /// Count untracked files again, within `paths` or everywhere, reusing counts of files
    /// whose size and modification time are unchanged.
    fn recount_untracked(&mut self, paths: Option<&BTreeSet<String>>) {
        if !self.modes.any() {
            self.untracked.clear();
            return;
        }
        let within = |path: &str| paths.is_none_or(|paths| covers(paths, path));
        let mut previous = std::mem::take(&mut self.untracked);
        // Counts outside the checked paths stand; inside, they are rebuilt.
        self.untracked = previous
            .iter()
            .filter(|(path, _)| !within(path))
            .filter(|(path, _)| {
                self.entries
                    .get(*path)
                    .is_some_and(StatusEntry::is_untracked)
            })
            .map(|(path, cached)| (path.clone(), *cached))
            .collect();
        let candidates: Vec<String> = self
            .entries
            .values()
            .filter(|entry| entry.is_untracked() && within(&entry.path))
            .map(|entry| entry.path.clone())
            .collect();
        for path in candidates {
            if self.untracked.len() >= self.limits.files {
                break;
            }
            let Ok(metadata) = std::fs::symlink_metadata(self.root.join(&path)) else {
                continue;
            };
            let (modified, len) = (metadata.modified().ok(), metadata.len());
            if let Some(cached) = previous.remove(&path)
                && cached.modified == modified
                && cached.len == len
                && modified.is_some()
            {
                self.untracked.insert(path, cached);
                continue;
            }
            let counted = self
                .repo
                .untracked_line_stats([path.as_str()], 1, self.limits.bytes);
            if let Some(stats) = counted.into_values().next() {
                self.untracked.insert(
                    path,
                    CachedCount {
                        modified,
                        len,
                        stats,
                    },
                );
            }
        }
    }
}

/// `path` with symbolic links resolved, as change events report it; itself when it cannot
/// be resolved.
pub fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

/// Whether `path` is one of `paths` or beneath one of them. The empty path covers all.
pub fn covers(paths: &BTreeSet<String>, path: &str) -> bool {
    if paths.contains("") || paths.contains(path) {
        return true;
    }
    path.match_indices('/')
        .any(|(index, _)| paths.contains(&path[..index]))
}

/// `paths` without those another covers, widened to parent directories until one query
/// can name them; `None` when that reaches the root.
fn narrow(paths: &BTreeSet<String>) -> Option<BTreeSet<String>> {
    let mut paths = minimal(paths);
    while paths.len() > PATHS_PER_QUERY {
        paths = minimal(
            &paths
                .into_iter()
                .map(|path| {
                    path.rsplit_once('/')
                        .map(|(parent, _)| parent.to_owned())
                        .unwrap_or_default()
                })
                .collect(),
        );
    }
    (!paths.contains("")).then_some(paths)
}

fn minimal(paths: &BTreeSet<String>) -> BTreeSet<String> {
    if paths.contains("") {
        return BTreeSet::from([String::new()]);
    }
    paths
        .iter()
        .filter(|path| {
            !path
                .match_indices('/')
                .any(|(index, _)| paths.contains(&path[..index]))
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }

    #[test]
    fn directories_cover_their_contents() {
        let paths = set(&["src", "README.md"]);
        assert!(covers(&paths, "src/main.rs"));
        assert!(covers(&paths, "src"));
        assert!(!covers(&paths, "srcs/main.rs"));
        assert!(!covers(&paths, "docs/README.md"));
        assert!(covers(&set(&[""]), "anything"));
    }

    #[test]
    fn narrowing_widens_bursts_to_directories() {
        assert_eq!(narrow(&set(&["a/b", "a", "c"])), Some(set(&["a", "c"])));
        let burst: BTreeSet<String> = (0..1000).map(|index| format!("gen/{index}.rs")).collect();
        assert_eq!(narrow(&burst), Some(set(&["gen"])));
        let scattered: BTreeSet<String> = (0..1000).map(|index| format!("{index}.rs")).collect();
        assert_eq!(narrow(&scattered), None);
    }
}
