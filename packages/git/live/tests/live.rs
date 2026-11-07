//! Live state follows real repositories through real filesystem events.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use bcode_git::models::{ChangeKind, LineStats, StatusEntry};
use bcode_git::{CommandLog, Repository};
use bcode_git_live::source::SourceKind;
use bcode_git_live::{
    CountsMode, EngineConfig, EngineStats, LineCounts, LiveUpdate, LocalSession, WorktreeSnapshot,
};
use tempfile::TempDir;

const SOURCES: [SourceKind; 2] = [SourceKind::Recursive, SourceKind::Pruned];

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repository() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    git(path, &["init", "--quiet", "--initial-branch", "main"]);
    git(path, &["config", "user.email", "test@example.com"]);
    git(path, &["config", "user.name", "Test"]);
    git(path, &["config", "commit.gpgsign", "false"]);
    std::fs::write(path.join("a.txt"), "1\n").unwrap();
    git(path, &["add", "--all"]);
    git(path, &["commit", "--quiet", "-m", "init"]);
    dir
}

/// A running session and everything it has reported.
struct Live {
    _session: LocalSession,
    updates: mpsc::Receiver<LiveUpdate>,
    primary: Option<WorktreeSnapshot>,
    totals: BTreeMap<PathBuf, (Option<String>, LineStats)>,
    stats: EngineStats,
    received: usize,
}

impl Live {
    fn start(path: &Path, config: EngineConfig, source: SourceKind) -> Self {
        let repo = Repository::discover(path, CommandLog::default()).unwrap();
        let (sender, updates) = mpsc::channel();
        let session = LocalSession::start(
            &repo,
            config,
            Some(source),
            std::sync::Arc::new(move |update| {
                let _ = sender.send(update);
            }),
        );
        let mut live = Self {
            _session: session,
            updates,
            primary: None,
            totals: BTreeMap::new(),
            stats: EngineStats::default(),
            received: 0,
        };
        live.wait("the first scan", |live| live.primary.is_some());
        live
    }

    fn take(&mut self, update: LiveUpdate) {
        assert!(update.errors.is_empty(), "{:?}", update.errors);
        self.received += 1;
        self.stats = update.stats;
        if update.primary.is_some() {
            self.primary = update.primary;
        }
        for (root, (head, total)) in update.totals {
            self.totals.insert(canonical(&root), (head, total.total));
        }
    }

    /// Apply updates until `done` holds (at most ten seconds).
    fn wait(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(self) {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "timed out waiting for {what}");
            if let Ok(update) = self.updates.recv_timeout(left) {
                self.take(update);
            }
        }
    }

    /// Apply whatever arrives within `period`; how many updates that was.
    fn quiet(&mut self, period: Duration) -> usize {
        let before = self.received;
        let deadline = Instant::now() + period;
        while let Ok(update) = self
            .updates
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            self.take(update);
        }
        self.received - before
    }

    fn entry(&self, path: &str) -> Option<&StatusEntry> {
        self.primary
            .as_ref()?
            .status
            .entries
            .iter()
            .find(|entry| entry.path == path)
    }

    fn count(&self, path: &str) -> Option<LineStats> {
        match &self.primary.as_ref()?.counts {
            LineCounts::Total(total) => total.get(path).copied(),
            _ => None,
        }
    }

    fn total(&self, root: &Path) -> Option<LineStats> {
        self.totals.get(&canonical(root)).map(|(_, total)| *total)
    }
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

fn counting() -> EngineConfig {
    EngineConfig {
        counts: CountsMode::Total,
        ..EngineConfig::default()
    }
}

#[test]
fn edits_are_checked_by_path_without_rescanning() {
    for source in SOURCES {
        let repo = repository();
        let mut live = Live::start(repo.path(), counting(), source);
        assert_eq!(live.stats.full_scans, 1, "{source:?}");

        std::fs::write(repo.path().join("a.txt"), "1\n2\n").unwrap();
        live.wait("the edit", |live| {
            live.count("a.txt") == Some(LineStats::text(1, 0))
        });
        std::fs::write(repo.path().join("new.txt"), "x\ny\n").unwrap();
        live.wait("the new file", |live| {
            live.count("new.txt") == Some(LineStats::text(2, 0))
        });
        assert!(live.entry("new.txt").is_some_and(StatusEntry::is_untracked));
        // Lock files are only git's own business inside its directory.
        std::fs::write(repo.path().join("Cargo.lock"), "lock\n").unwrap();
        live.wait("the lock file", |live| live.entry("Cargo.lock").is_some());
        assert_eq!(live.stats.full_scans, 1, "{source:?}");
        assert!(live.stats.path_checks >= 3, "{source:?}");
    }
}

#[test]
fn ignored_paths_cost_nothing() {
    for source in SOURCES {
        let repo = repository();
        std::fs::write(repo.path().join(".gitignore"), "target/\n").unwrap();
        let mut live = Live::start(repo.path(), counting(), source);
        let checks = live.stats.path_checks;

        std::fs::create_dir_all(repo.path().join("target/debug")).unwrap();
        for index in 0..20 {
            std::fs::write(repo.path().join(format!("target/debug/{index}.o")), "x").unwrap();
        }
        assert_eq!(live.quiet(Duration::from_millis(400)), 0, "{source:?}");

        std::fs::write(repo.path().join("a.txt"), "2\n").unwrap();
        live.wait("the edit", |live| live.entry("a.txt").is_some());
        assert_eq!(live.stats.path_checks, checks + 1, "{source:?}");
        assert_eq!(live.stats.full_scans, 1, "{source:?}");
    }
}

#[test]
fn reading_files_is_no_change() {
    for source in SOURCES {
        let repo = repository();
        let path = repo.path();
        std::fs::write(path.join(".gitignore"), "target/\n").unwrap();
        std::fs::create_dir_all(path.join("src/nested")).unwrap();
        std::fs::write(path.join("src/nested/lib.rs"), "fn main() {}\n").unwrap();
        git(path, &["add", "--all"]);
        git(path, &["commit", "--quiet", "-m", "tree"]);
        let mut live = Live::start(path, counting(), source);
        let checks = live.stats.path_checks;
        let walks = live.stats.watch_walks;
        // Reads (by editors, git, or the watcher's own ignore rules) must not turn into work.
        for _ in 0..200 {
            let _ = std::fs::read_to_string(path.join(".gitignore")).unwrap();
            let _ = std::fs::read_dir(path.join("src/nested")).unwrap().count();
            let _ = std::fs::read_to_string(path.join("src/nested/lib.rs")).unwrap();
        }
        git(path, &["status", "--porcelain"]);
        assert_eq!(live.quiet(Duration::from_millis(400)), 0, "{source:?}");
        std::fs::write(
            path.join("src/nested/lib.rs"),
            "fn main() {}\nfn more() {}\n",
        )
        .unwrap();
        live.wait("the edit", |live| {
            live.count("src/nested/lib.rs") == Some(LineStats::text(1, 0))
        });
        assert_eq!(live.stats.path_checks, checks + 1, "{source:?}");
        assert_eq!(live.stats.watch_walks, walks, "{source:?}");
    }
}

#[test]
fn staging_and_committing_follow_the_index_and_head() {
    for source in SOURCES {
        let repo = repository();
        let path = repo.path();
        let mut live = Live::start(path, counting(), source);

        std::fs::write(path.join("a.txt"), "1\n2\n").unwrap();
        live.wait("the edit", |live| {
            live.entry("a.txt")
                .is_some_and(|entry| entry.worktree == ChangeKind::Modified)
        });
        git(path, &["add", "a.txt"]);
        live.wait("staging", |live| {
            live.entry("a.txt").is_some_and(|entry| {
                entry.index == ChangeKind::Modified && entry.worktree == ChangeKind::Unmodified
            })
        });
        let before = live.primary.as_ref().unwrap().head.clone();
        git(path, &["commit", "--quiet", "-m", "second"]);
        live.wait("the commit", |live| {
            let primary = live.primary.as_ref().unwrap();
            primary.status.entries.is_empty() && primary.head != before
        });
        // Unstaging through the index alone is seen too.
        std::fs::write(path.join("b.txt"), "b\n").unwrap();
        git(path, &["add", "b.txt"]);
        live.wait("the staged file", |live| {
            live.entry("b.txt")
                .is_some_and(|entry| entry.index == ChangeKind::Added)
        });
        git(path, &["rm", "--cached", "--quiet", "b.txt"]);
        live.wait("the unstaged file", |live| {
            live.entry("b.txt").is_some_and(StatusEntry::is_untracked)
        });
        assert_eq!(live.stats.full_scans, 1, "{source:?}");
    }
}

#[test]
fn bursts_coalesce_into_few_narrowed_checks() {
    for source in SOURCES {
        let repo = repository();
        let mut live = Live::start(repo.path(), counting(), source);
        let generated = repo.path().join("generated");
        std::fs::create_dir(&generated).unwrap();
        for index in 0..400 {
            std::fs::write(generated.join(format!("{index}.txt")), "x\n").unwrap();
        }
        live.wait("every file", |live| {
            live.primary.as_ref().unwrap().status.entries.len() == 400
        });
        assert!(live.received < 40, "{source:?}: {} updates", live.received);
        assert_eq!(live.stats.full_scans, 1, "{source:?}");
    }
}

#[test]
fn new_directories_are_followed() {
    for source in SOURCES {
        let repo = repository();
        let mut live = Live::start(repo.path(), counting(), source);
        let nested = repo.path().join("one/two");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("first.txt"), "1\n").unwrap();
        live.wait("the first file", |live| {
            live.entry("one/two/first.txt").is_some()
        });
        // Later files in the new directory are seen through its own watch.
        live.quiet(Duration::from_millis(100));
        std::fs::write(nested.join("second.txt"), "1\n").unwrap();
        live.wait("the second file", |live| {
            live.entry("one/two/second.txt").is_some()
        });
    }
}

#[test]
fn ignore_rule_changes_update_untracked_files() {
    for source in SOURCES {
        let repo = repository();
        std::fs::write(repo.path().join("notes.tmp"), "x\n").unwrap();
        let mut live = Live::start(repo.path(), counting(), source);
        assert!(live.entry("notes.tmp").is_some());

        std::fs::write(repo.path().join(".gitignore"), "*.tmp\n").unwrap();
        live.wait("the ignored file to go", |live| {
            live.entry("notes.tmp").is_none() && live.entry(".gitignore").is_some()
        });
        std::fs::write(repo.path().join(".gitignore"), "").unwrap();
        live.wait("the file to return", |live| {
            live.entry("notes.tmp").is_some()
        });
    }
}

#[test]
fn other_worktrees_report_their_totals() {
    for source in SOURCES {
        let repo = repository();
        let others = tempfile::tempdir().unwrap();
        let topic = others.path().join("topic");
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "topic",
                topic.to_str().unwrap(),
            ],
        );
        let config = EngineConfig {
            other_worktrees: true,
            ..counting()
        };
        let mut live = Live::start(repo.path(), config, source);
        live.wait("the other worktree", |live| live.total(&topic).is_some());

        std::fs::write(topic.join("a.txt"), "1\n2\n3\n").unwrap();
        live.wait("its edit", |live| {
            live.total(&topic) == Some(LineStats::text(2, 0))
        });
        std::fs::write(topic.join("a.txt"), "").unwrap();
        live.wait("its removal", |live| {
            live.total(&topic) == Some(LineStats::text(0, 1))
        });
        // Committing there moves its `HEAD` and empties its changes.
        git(&topic, &["commit", "--quiet", "-am", "empty"]);
        live.wait("its commit", |live| {
            live.total(&topic) == Some(LineStats::default())
        });
        assert_eq!(live.stats.full_scans, 2, "{source:?}");
    }
}

#[test]
fn a_clients_own_worktree_comes_before_the_others_are_scanned() {
    let repo = repository();
    let others = tempfile::tempdir().unwrap();
    let topics: Vec<PathBuf> = (0..3)
        .map(|index| {
            let topic = others.path().join(format!("topic-{index}"));
            git(
                repo.path(),
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "-b",
                    &format!("topic-{index}"),
                    topic.to_str().unwrap(),
                ],
            );
            topic
        })
        .collect();
    let config = EngineConfig {
        other_worktrees: true,
        ..counting()
    };
    let mut live = Live::start(repo.path(), config, SourceKind::Auto);
    // The first update carries the client's own worktree, scanned alone.
    assert_eq!(live.stats.full_scans, 1);
    assert!(live.totals.is_empty(), "{:?}", live.totals);
    live.wait("the other worktrees", |live| {
        topics.iter().all(|topic| live.total(topic).is_some())
    });
    assert_eq!(live.stats.full_scans, 4);
}

#[test]
fn a_departed_clients_worktree_is_kept_while_retained() {
    let repo = repository();
    let main_repo = Repository::discover(repo.path(), CommandLog::default()).unwrap();
    let hub = bcode_git_live::Hub::start_retaining(
        &main_repo,
        counting().scanner,
        Some(SourceKind::Auto),
        Duration::from_millis(1500),
    );
    let subscribe = || {
        let (sender, updates) = mpsc::channel();
        let id = hub.subscribe(
            &main_repo,
            counting(),
            std::sync::Arc::new(move |update| {
                let _ = sender.send(update);
            }),
        );
        (id, Observer::new(updates))
    };
    let (id, mut first) = subscribe();
    first.wait("the first scan", |seen| seen.primary.is_some());
    hub.unsubscribe(id);
    // Changes while nobody is subscribed are still followed.
    std::fs::write(repo.path().join("a.txt"), "1\n2\n").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let (id, mut again) = subscribe();
    again.wait("everything known", |seen| seen.primary.is_some());
    assert_eq!(again.count("a.txt"), Some(LineStats::text(1, 0)));
    assert_eq!(again.stats.full_scans, 1, "served without scanning again");
    hub.unsubscribe(id);

    // Once retention runs out, the worktree is let go and scanned again when shown.
    std::thread::sleep(Duration::from_millis(2000));
    let (_, mut late) = subscribe();
    late.wait("everything known", |seen| seen.primary.is_some());
    assert_eq!(late.stats.full_scans, 2);
}

#[test]
fn clients_of_one_repository_share_its_work() {
    let repo = repository();
    let others = tempfile::tempdir().unwrap();
    let topic = others.path().join("topic");
    git(
        repo.path(),
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "topic",
            topic.to_str().unwrap(),
        ],
    );
    let config = EngineConfig {
        other_worktrees: true,
        ..counting()
    };
    let main_repo = Repository::discover(repo.path(), CommandLog::default()).unwrap();
    let topic_repo = Repository::discover(&topic, CommandLog::default()).unwrap();
    let hub = bcode_git_live::Hub::start(&main_repo, config.scanner, Some(SourceKind::Auto));
    let subscribe = |repo: &Repository| {
        let (sender, updates) = mpsc::channel();
        hub.subscribe(
            repo,
            config,
            std::sync::Arc::new(move |update| {
                let _ = sender.send(update);
            }),
        );
        updates
    };
    let mut main = Observer::new(subscribe(&main_repo));
    let mut other = Observer::new(subscribe(&topic_repo));
    main.wait("main's scan", |seen| {
        seen.primary.is_some() && seen.total(&topic).is_some()
    });
    other.wait("topic's scan", |seen| {
        seen.primary.is_some() && seen.total(repo.path()).is_some()
    });
    // Each worktree is scanned once, however many clients show it.
    assert_eq!(main.stats.full_scans, 2);

    std::fs::write(repo.path().join("a.txt"), "1\n2\n").unwrap();
    main.wait("its own edit", |seen| {
        seen.count("a.txt") == Some(LineStats::text(1, 0))
    });
    other.wait("main's total", |seen| {
        seen.total(repo.path()) == Some(LineStats::text(1, 0))
    });
    std::fs::write(topic.join("a.txt"), "").unwrap();
    other.wait("its own edit", |seen| {
        seen.count("a.txt") == Some(LineStats::text(0, 1))
    });
    main.wait("topic's total", |seen| {
        seen.total(&topic) == Some(LineStats::text(0, 1))
    });
    // One check per edit, for both clients together.
    assert_eq!(other.stats.full_scans, 2);
    assert_eq!(other.stats.path_checks, 2);
}

/// Updates one subscriber received.
struct Observer {
    updates: mpsc::Receiver<LiveUpdate>,
    primary: Option<WorktreeSnapshot>,
    totals: BTreeMap<PathBuf, LineStats>,
    stats: EngineStats,
}

impl Observer {
    fn new(updates: mpsc::Receiver<LiveUpdate>) -> Self {
        Self {
            updates,
            primary: None,
            totals: BTreeMap::new(),
            stats: EngineStats::default(),
        }
    }

    fn wait(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(self) {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "timed out waiting for {what}");
            if let Ok(update) = self.updates.recv_timeout(left) {
                assert!(update.errors.is_empty(), "{:?}", update.errors);
                self.stats = update.stats;
                if update.primary.is_some() {
                    self.primary = update.primary;
                }
                for (root, (_, total)) in update.totals {
                    self.totals.insert(canonical(&root), total.total);
                }
            }
        }
    }

    fn count(&self, path: &str) -> Option<LineStats> {
        match &self.primary.as_ref()?.counts {
            LineCounts::Total(total) => total.get(path).copied(),
            _ => None,
        }
    }

    fn total(&self, root: &Path) -> Option<LineStats> {
        self.totals.get(&canonical(root)).copied()
    }
}
