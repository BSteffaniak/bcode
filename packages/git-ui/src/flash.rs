//! Brief row highlights when changes land.
//!
//! A row whose content grew between refreshes flashes green, one whose content shrank
//! flashes red, and either fades back to the row's own background. Rows that newly appear
//! flash green.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bcode_git::models::LineStats;
use bmux_tui::style::{Color, Easing};

use crate::config::FlashConfig;

/// Which way a change went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashKind {
    /// Content added (or a row appeared).
    Added,
    /// Content removed.
    Removed,
}

/// A row that can flash.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum FlashKey {
    /// A working tree file or folder, by path (folders without a trailing slash).
    Path(String),
    /// A local branch, by name.
    Branch(String),
    /// A worktree, by root.
    Worktree(PathBuf),
}

/// How a row's line counts changed between two refreshes; `before` is `None` for a row that
/// just appeared, which counts as added.
///
/// More `+` lines or fewer `-` lines add content; more `-` lines or fewer `+` lines remove
/// it. The change is [`FlashKind::Removed`] when removal makes up at least
/// `removed_percent` of it, otherwise [`FlashKind::Added`]; unchanged counts give `None`.
#[must_use]
pub fn classify(
    before: Option<LineStats>,
    after: LineStats,
    removed_percent: u8,
) -> Option<FlashKind> {
    let Some(before) = before else {
        return Some(FlashKind::Added);
    };
    let grew = |from: u64, to: u64| u128::from(to.saturating_sub(from));
    let added = grew(before.added, after.added) + grew(after.removed, before.removed);
    let removed = grew(before.removed, after.removed) + grew(after.added, before.added);
    let total = added + removed;
    if total == 0 {
        return None;
    }
    Some(
        if removed * 100 >= u128::from(removed_percent.min(100)) * total {
            FlashKind::Removed
        } else {
            FlashKind::Added
        },
    )
}

/// Background of a row `elapsed` into a flash lasting `duration`, or `None` once it is over.
///
/// The row shows `color` for the first fifth, then blends toward `base`. When `fade` is off,
/// or `base` has no known color (such as the terminal's default background), it keeps
/// `color` until the end instead.
#[must_use]
pub fn background(
    color: Color,
    base: Color,
    elapsed: Duration,
    duration: Duration,
    fade: bool,
) -> Option<Color> {
    if elapsed >= duration {
        return None;
    }
    let hold = duration / 5;
    if !fade || elapsed < hold {
        return Some(color);
    }
    let amount =
        Easing::EaseInOut.progress(elapsed.saturating_sub(hold), duration.saturating_sub(hold));
    Some(color.mix(base, amount).unwrap_or(color))
}

/// Line counts of every folder above `files`, by folder path.
fn folders(files: &BTreeMap<String, LineStats>) -> BTreeMap<String, LineStats> {
    let mut folders: BTreeMap<String, LineStats> = BTreeMap::new();
    for (path, stats) in files {
        for (index, _) in path.match_indices('/') {
            let folder = folders.entry(path[..index].to_owned()).or_default();
            *folder = folder.combined(*stats);
        }
    }
    folders
}

/// Measured state of one refresh, compared with the next.
#[derive(Debug, Clone)]
struct Measured<T> {
    /// `HEAD` commit it was measured against; a different `HEAD` starts over without flashes.
    head: Option<String>,
    value: T,
}

/// Live flashes and what the next refresh is compared with.
#[derive(Debug, Default)]
pub struct Flashes {
    live: BTreeMap<FlashKey, (FlashKind, Instant)>,
    files: Option<Measured<BTreeMap<String, LineStats>>>,
    worktrees: BTreeMap<PathBuf, Measured<LineStats>>,
    listed: Option<(BTreeSet<String>, BTreeSet<PathBuf>)>,
}

impl Flashes {
    /// Compare the working tree's changed files (zero counts for those without any) with
    /// the last refresh, flashing files and the folders above them.
    pub fn files(
        &mut self,
        head: Option<&str>,
        files: BTreeMap<String, LineStats>,
        config: &FlashConfig,
        now: Instant,
    ) {
        let previous = self.files.take();
        if config.files()
            && let Some(previous) = previous.filter(|previous| previous.head.as_deref() == head)
        {
            let (before, before_folders) = (&previous.value, folders(&previous.value));
            for (path, stats) in files.iter().chain(&folders(&files)) {
                let old = before.get(path).or_else(|| before_folders.get(path));
                if let Some(kind) = classify(old.copied(), *stats, config.removed_percent) {
                    self.live.insert(FlashKey::Path(path.clone()), (kind, now));
                }
            }
        }
        self.files = Some(Measured {
            head: head.map(str::to_owned),
            value: files,
        });
    }

    /// Compare a worktree's total with its last one, flashing its row and the row of
    /// `branch`, the branch checked out there.
    pub fn worktree(
        &mut self,
        path: &Path,
        head: Option<&str>,
        total: LineStats,
        branch: Option<&str>,
        config: &FlashConfig,
        now: Instant,
    ) {
        let previous = self.worktrees.insert(
            path.to_owned(),
            Measured {
                head: head.map(str::to_owned),
                value: total,
            },
        );
        let Some(previous) = previous.filter(|previous| previous.head.as_deref() == head) else {
            return;
        };
        if !config.branches() {
            return;
        }
        if let Some(kind) = classify(Some(previous.value), total, config.removed_percent) {
            self.live
                .insert(FlashKey::Worktree(path.to_owned()), (kind, now));
            if let Some(branch) = branch {
                self.live
                    .insert(FlashKey::Branch(branch.to_owned()), (kind, now));
            }
        }
    }

    /// Flash branches and worktrees that were not listed by the last refresh, and forget
    /// the totals of worktrees that are gone.
    pub fn listed(
        &mut self,
        branches: BTreeSet<String>,
        worktrees: BTreeSet<PathBuf>,
        config: &FlashConfig,
        now: Instant,
    ) {
        self.worktrees.retain(|path, _| worktrees.contains(path));
        if config.branches()
            && let Some((old_branches, old_worktrees)) = &self.listed
        {
            for branch in branches.difference(old_branches) {
                self.live
                    .insert(FlashKey::Branch(branch.clone()), (FlashKind::Added, now));
            }
            for worktree in worktrees.difference(old_worktrees) {
                self.live.insert(
                    FlashKey::Worktree(worktree.clone()),
                    (FlashKind::Added, now),
                );
            }
        }
        self.listed = Some((branches, worktrees));
    }

    /// The flash of `key` and when it started, if it is live.
    #[must_use]
    pub fn get(&self, key: &FlashKey) -> Option<(FlashKind, Instant)> {
        self.live.get(key).copied()
    }

    /// Whether any flash is live.
    #[must_use]
    pub fn active(&self) -> bool {
        !self.live.is_empty()
    }

    /// Drop flashes older than `duration`. Returns whether any were dropped.
    pub fn expire(&mut self, now: Instant, duration: Duration) -> bool {
        let before = self.live.len();
        self.live
            .retain(|_, (_, started)| now.saturating_duration_since(*started) < duration);
        self.live.len() != before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(added: u64, removed: u64) -> LineStats {
        LineStats::text(added, removed)
    }

    #[test]
    fn changes_classify_by_which_way_content_moved() {
        let classify = |before, after| classify(before, after, 90);
        // New `+` lines, or `-` lines restored, add content.
        assert_eq!(
            classify(Some(stats(1, 0)), stats(3, 0)),
            Some(FlashKind::Added)
        );
        assert_eq!(
            classify(Some(stats(0, 4)), stats(0, 1)),
            Some(FlashKind::Added)
        );
        // New `-` lines, or typed lines taken back, remove it.
        assert_eq!(
            classify(Some(stats(0, 0)), stats(0, 2)),
            Some(FlashKind::Removed)
        );
        assert_eq!(
            classify(Some(stats(5, 0)), stats(2, 0)),
            Some(FlashKind::Removed)
        );
        // Mostly removal, but not almost all of it, is still green.
        assert_eq!(
            classify(Some(stats(0, 0)), stats(2, 8)),
            Some(FlashKind::Added)
        );
        assert_eq!(
            classify(Some(stats(0, 0)), stats(1, 9)),
            Some(FlashKind::Removed)
        );
        // A replaced line is half and half.
        assert_eq!(
            classify(Some(stats(0, 0)), stats(1, 1)),
            Some(FlashKind::Added)
        );
        // Nothing moved, nothing flashes; a new row flashes green even when empty.
        assert_eq!(classify(Some(stats(2, 3)), stats(2, 3)), None);
        assert_eq!(classify(None, stats(0, 0)), Some(FlashKind::Added));
        // The threshold is configurable and clamped.
        assert_eq!(
            super::classify(Some(stats(0, 0)), stats(1, 1), 50),
            Some(FlashKind::Removed)
        );
        assert_eq!(
            super::classify(Some(stats(0, 0)), stats(0, 1), 200),
            Some(FlashKind::Removed)
        );
    }

    #[test]
    fn backgrounds_hold_then_fade_to_the_base() {
        let flash = Color::Rgb(200, 0, 0);
        let base = Color::Rgb(0, 0, 0);
        let duration = Duration::from_millis(1000);
        let at = |ms| background(flash, base, Duration::from_millis(ms), duration, true);
        assert_eq!(at(0), Some(flash));
        assert_eq!(at(199), Some(flash));
        // Halfway through the fade the color sits halfway between.
        assert_eq!(at(600), flash.mix(base, 0.5));
        let Some(Color::Rgb(late, ..)) = at(990) else {
            panic!("fading colors blend through RGB");
        };
        assert!(late < 5, "{late}");
        assert_eq!(at(1000), None);
        // Without a fade, or over the terminal's default background, the color holds.
        assert_eq!(
            background(flash, base, Duration::from_millis(900), duration, false),
            Some(flash)
        );
        assert_eq!(
            background(
                flash,
                Color::Default,
                Duration::from_millis(900),
                duration,
                true
            ),
            Some(flash)
        );
    }

    #[test]
    fn files_and_folders_flash_against_the_last_refresh() {
        let config = FlashConfig::default();
        let now = Instant::now();
        let mut flashes = Flashes::default();
        let files = |entries: &[(&str, LineStats)]| {
            entries
                .iter()
                .map(|(path, stats)| ((*path).to_owned(), *stats))
                .collect::<BTreeMap<_, _>>()
        };
        // The first refresh only sets the baseline.
        flashes.files(
            Some("a"),
            files(&[("src/ui/app.rs", stats(1, 0)), ("README", stats(0, 0))]),
            &config,
            now,
        );
        assert!(!flashes.active());
        flashes.files(
            Some("a"),
            files(&[
                ("src/ui/app.rs", stats(1, 3)),
                ("README", stats(0, 0)),
                ("new/file", stats(0, 0)),
            ]),
            &config,
            now,
        );
        let kind = |path: &str| {
            flashes
                .get(&FlashKey::Path(path.to_owned()))
                .map(|(kind, _)| kind)
        };
        assert_eq!(kind("src/ui/app.rs"), Some(FlashKind::Removed));
        assert_eq!(kind("src/ui"), Some(FlashKind::Removed));
        assert_eq!(kind("src"), Some(FlashKind::Removed));
        assert_eq!(kind("README"), None);
        assert_eq!(kind("new/file"), Some(FlashKind::Added));
        assert_eq!(kind("new"), Some(FlashKind::Added));
        // A moved `HEAD` starts over without flashing.
        let mut flashes = Flashes::default();
        flashes.files(Some("a"), files(&[("x", stats(1, 0))]), &config, now);
        flashes.files(Some("b"), files(&[("x", stats(9, 0))]), &config, now);
        assert!(!flashes.active());
        // Expired flashes are dropped.
        let mut flashes = Flashes::default();
        flashes.files(Some("a"), files(&[]), &config, now);
        flashes.files(Some("a"), files(&[("x", stats(1, 0))]), &config, now);
        assert!(!flashes.expire(now + Duration::from_millis(599), config.duration()));
        assert!(flashes.expire(now + Duration::from_millis(600), config.duration()));
        assert!(!flashes.active());
    }

    #[test]
    fn worktrees_flash_with_their_branch_and_new_rows_flash_green() {
        let config = FlashConfig::default();
        let now = Instant::now();
        let mut flashes = Flashes::default();
        let root = Path::new("/repo");
        let names = |names: &[&str]| names.iter().map(|name| (*name).to_owned()).collect();
        let roots = |paths: &[&str]| paths.iter().map(PathBuf::from).collect();
        flashes.listed(names(&["main"]), roots(&["/repo"]), &config, now);
        flashes.worktree(root, Some("a"), stats(2, 0), Some("main"), &config, now);
        assert!(!flashes.active());
        flashes.worktree(root, Some("a"), stats(2, 5), Some("main"), &config, now);
        assert_eq!(
            flashes
                .get(&FlashKey::Branch("main".into()))
                .map(|(kind, _)| kind),
            Some(FlashKind::Removed)
        );
        assert_eq!(
            flashes
                .get(&FlashKey::Worktree(root.to_owned()))
                .map(|(kind, _)| kind),
            Some(FlashKind::Removed)
        );
        flashes.listed(
            names(&["main", "topic"]),
            roots(&["/repo", "/wt"]),
            &config,
            now,
        );
        assert_eq!(
            flashes
                .get(&FlashKey::Branch("topic".into()))
                .map(|(kind, _)| kind),
            Some(FlashKind::Added)
        );
        assert_eq!(
            flashes
                .get(&FlashKey::Worktree("/wt".into()))
                .map(|(kind, _)| kind),
            Some(FlashKind::Added)
        );
        // Turned off, nothing flashes but baselines still follow along.
        let off = FlashConfig {
            enabled: false,
            ..FlashConfig::default()
        };
        let mut flashes = Flashes::default();
        flashes.listed(names(&["main"]), roots(&[]), &off, now);
        flashes.worktree(root, Some("a"), stats(0, 0), None, &off, now);
        flashes.worktree(root, Some("a"), stats(1, 0), None, &off, now);
        flashes.listed(names(&["main", "new"]), roots(&[]), &off, now);
        assert!(!flashes.active());
    }
}
