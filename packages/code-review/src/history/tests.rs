//! A three-layer stack pushed three times, read back from its reflogs: the first push, then the
//! bottom layer edited with the two above restacked onto it, then a commit added on top.

use super::reflog::reflog_versions;
use super::*;
use bcode_code_review_models::{PullRequestSummary, ReviewStackLayer};
use std::path::PathBuf;
use std::process::Command;

/// Seconds of each push.
const PUSHES: [u64; 3] = [1_700_000_000, 1_700_010_000, 1_700_020_000];

struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Repo {
    fn git(&self, at: u64, args: &[&str]) -> String {
        let date = format!("@{at} +0000");
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            // Away from the user's configuration (and its hooks), as the shared fixture is.
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn write(&self, path: &str, text: &str) {
        std::fs::write(self.root.join(path), text).expect("write");
    }

    fn commit(&self, at: u64, path: &str, text: &str) {
        self.write(path, text);
        self.git(at, &["add", path]);
        self.git(at, &["commit", "-q", "-m", path]);
    }

    fn head(&self, branch: &str) -> String {
        self.git(0, &["rev-parse", branch])
    }
}

/// The stack after three pushes, with each layer's head after each push.
fn restacked() -> (Repo, [[String; 3]; 3]) {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repo {
        root: dir.path().to_path_buf(),
        _dir: dir,
    };
    let [first, second, third] = PUSHES;
    repo.git(first, &["init", "-q", "-b", "main"]);
    repo.commit(first, "base.txt", "base\n");
    for (layer, below) in [("l1", "main"), ("l2", "l1"), ("l3", "l2")] {
        repo.git(first, &["checkout", "-q", "-b", layer, below]);
        repo.commit(first, &format!("{layer}.txt"), &format!("{layer}\n"));
    }
    let at_first = ["l1", "l2", "l3"].map(|layer| repo.head(layer));
    // The bottom layer is edited, and the layers above are restacked onto it.
    repo.git(second, &["checkout", "-q", "l1"]);
    repo.write("l1.txt", "l1 edited\n");
    repo.git(second, &["commit", "-q", "-a", "--amend", "--no-edit"]);
    repo.git(
        second,
        &["rebase", "-q", "--onto", "l1", &at_first[0], "l2"],
    );
    repo.git(
        second,
        &["rebase", "-q", "--onto", "l2", &at_first[1], "l3"],
    );
    let at_second = ["l1", "l2", "l3"].map(|layer| repo.head(layer));
    // A commit is added on top.
    repo.commit(third, "l3-more.txt", "more\n");
    let at_third = ["l1", "l2", "l3"].map(|layer| repo.head(layer));
    (repo, [at_first, at_second, at_third])
}

fn stack() -> ReviewStack {
    let layer = |number: u64, base: &str, head: &str| ReviewStackLayer {
        pull: PullRequestSummary {
            number,
            title: format!("Layer {number}"),
            author: None,
            base_branch: base.into(),
            head_branch: head.into(),
            head_oid: String::new(),
            base_oid: None,
            review_requested_from_me: false,
            draft: false,
            updated: None,
            url: None,
        },
        parent: number.checked_sub(1).filter(|below| *below > 0),
    };
    ReviewStack {
        trunk: "main".into(),
        rooted_at_default_branch: true,
        layers: vec![
            layer(1, "main", "l1"),
            layer(2, "l1", "l2"),
            layer(3, "l2", "l3"),
        ],
        requested: 3,
        siblings: Vec::new(),
        warnings: Vec::new(),
    }
}

fn history(repo: &Repo) -> StackTimeline {
    let layers: Vec<(u64, String)> = (1..=3)
        .map(|number| (number, format!("l{number}")))
        .collect();
    let versions = reflog_versions(&repo.root, "fake", "o/r", &layers).expect("reflogs");
    timeline(&repo.root, &stack(), &versions, &TimelineOptions::default()).expect("timeline")
}

fn kinds(timeline: &StackTimeline) -> Vec<Vec<ChangeKind>> {
    timeline
        .layers
        .iter()
        .map(|layer| layer.versions.iter().map(|version| version.kind).collect())
        .collect()
}

#[test]
fn reflogs_show_which_push_changed_which_layer_and_which_only_moved() {
    let (repo, heads) = restacked();
    let timeline = history(&repo);
    // Branch creations (a branch pointing at the one below, with no change yet) are not versions.
    let layer_heads: Vec<Vec<&str>> = timeline
        .layers
        .iter()
        .map(|layer| {
            layer
                .versions
                .iter()
                .map(|version| version.version.head_oid.as_str())
                .collect()
        })
        .collect();
    assert_eq!(
        layer_heads,
        [
            vec![heads[0][0].as_str(), heads[1][0].as_str()],
            vec![heads[0][1].as_str(), heads[1][1].as_str()],
            vec![
                heads[0][2].as_str(),
                heads[1][2].as_str(),
                heads[2][2].as_str()
            ],
        ]
    );
    assert_eq!(
        kinds(&timeline),
        [
            vec![ChangeKind::First, ChangeKind::Content],
            vec![ChangeKind::First, ChangeKind::RebaseOnly],
            vec![
                ChangeKind::First,
                ChangeKind::RebaseOnly,
                ChangeKind::CommitsAdded
            ],
        ]
    );
    // Each layer's change starts at the layer below it at the time.
    assert_eq!(
        timeline.layers[2].versions[1].fork_oid.as_deref(),
        Some(heads[1][1].as_str())
    );
    let summaries: Vec<String> = timeline.pushes.iter().map(push_summary).collect();
    assert_eq!(summaries, ["#1 changed, 2 rebased", "#3 changed"]);
}

#[test]
fn comparisons_show_what_the_author_changed_without_what_moved_underneath() {
    use bcode_code_review_models::{DeltaKind, LayerPair, VersionPoint};
    let (repo, _) = restacked();
    let timeline = history(&repo);
    let [first, second, _] = PUSHES;
    let after_first = first * 1000 + 1;
    let point = |version: &TimelineVersion| VersionPoint {
        head_oid: version.version.head_oid.clone(),
        fork_oid: version.fork_oid.clone(),
    };
    // Each layer from its version at `at_ms` to its newest.
    let since = |at_ms: u64| -> Vec<LayerPair> {
        timeline
            .layers
            .iter()
            .map(|layer| LayerPair {
                number: layer.number,
                from: version_at(layer, at_ms).map(|index| point(&layer.versions[index])),
                to: point(layer.versions.last().expect("a version")),
            })
            .collect()
    };
    let disk = crate::cache::ReviewCache::disabled();
    let deltas =
        super::delta::layer_deltas(&repo.root, &disk, &since(after_first)).expect("compare");
    let found: Vec<(u64, DeltaKind, Vec<String>)> = deltas
        .iter()
        .map(|delta| {
            (
                delta.pair.number,
                delta.kind,
                delta.files.iter().map(|file| file.path.clone()).collect(),
            )
        })
        .collect();
    // Since the first push: the edited layer and the commit on top; the layer between was only
    // rebased.
    assert_eq!(
        found,
        [
            (1, DeltaKind::Changed, vec!["l1.txt".to_owned()]),
            (2, DeltaKind::RebasedOnly, Vec::new()),
            (3, DeltaKind::Changed, vec!["l3-more.txt".to_owned()]),
        ]
    );
    // Since the second push only the commit on top changed anything.
    let latest =
        super::delta::layer_deltas(&repo.root, &disk, &since(second * 1000 + 1)).expect("compare");
    let kinds: Vec<DeltaKind> = latest.iter().map(|delta| delta.kind).collect();
    assert_eq!(
        kinds,
        [
            DeltaKind::Unchanged,
            DeltaKind::Unchanged,
            DeltaKind::Changed
        ]
    );
}

#[test]
fn forge_pushes_become_versions_on_the_base_branch_of_their_time() {
    use bcode_code_review_models::{PullHistory, PullHistoryEvent};
    let histories = [PullHistory {
        number: 7,
        events: vec![
            // Two commits pushed together, then a third later: two versions.
            PullHistoryEvent::CommitsPushed {
                at_ms: 1_000,
                head_oid: "a1".into(),
            },
            PullHistoryEvent::CommitsPushed {
                at_ms: 2_000,
                head_oid: "a2".into(),
            },
            PullHistoryEvent::BaseChanged {
                at_ms: 100_000,
                from: Some("main".into()),
                to: Some("feature".into()),
            },
            PullHistoryEvent::ForcePushed {
                at_ms: 200_000,
                actor: Some("octocat".into()),
                before_oid: Some("a2".into()),
                after_oid: Some("b1".into()),
            },
        ],
    }];
    let versions = super::forge::forge_versions(
        "github",
        "o/r",
        &histories,
        &BTreeMap::from([(7, "feature".to_owned())]),
    );
    let seen: Vec<(&str, u64, Option<&str>, Option<&str>)> = versions
        .iter()
        .map(|version| {
            (
                version.head_oid.as_str(),
                version.seen_at_ms,
                version.base_branch.as_deref(),
                version.actor.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("a2", 2_000, Some("main"), None),
            ("a2", 199_999, Some("feature"), None),
            ("b1", 200_000, Some("feature"), Some("octocat")),
        ]
    );
}

mod deltas {
    use super::super::delta::{keep_versions, layer_deltas, marks};
    use super::*;
    use crate::cache::ReviewCache;
    use bcode_code_review_models::{
        DeltaFile, DeltaKind, FileSince, LayerPair, RemovedSince, ReviewCompareMarksRequest,
        VersionPoint,
    };

    fn point(head: &str, fork: Option<&str>) -> VersionPoint {
        VersionPoint {
            head_oid: head.to_owned(),
            fork_oid: fork.map(str::to_owned),
        }
    }

    fn pair(number: u64, from: Option<VersionPoint>, to: VersionPoint) -> LayerPair {
        LayerPair { number, from, to }
    }

    fn empty_repo() -> Repo {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repo {
            root: dir.path().to_path_buf(),
            _dir: dir,
        };
        repo.git(PUSHES[0], &["init", "-q", "-b", "main"]);
        repo
    }

    #[test]
    fn one_edited_layer_reads_changed_and_the_restacked_ones_rebased_only() {
        let (repo, heads) = restacked();
        let base = repo.git(0, &["rev-parse", "main"]);
        let disk = ReviewCache::disabled();
        // Each layer from the first push to the second, forks as the stack had them.
        let pairs = [
            pair(
                1,
                Some(point(&heads[0][0], Some(&base))),
                point(&heads[1][0], Some(&base)),
            ),
            pair(
                2,
                Some(point(&heads[0][1], Some(&heads[0][0]))),
                point(&heads[1][1], Some(&heads[1][0])),
            ),
            pair(
                3,
                Some(point(&heads[1][2], Some(&heads[1][1]))),
                point(&heads[2][2], Some(&heads[2][1])),
            ),
            pair(
                3,
                Some(point(&heads[2][2], Some(&heads[2][1]))),
                point(&heads[2][2], Some(&heads[2][1])),
            ),
            pair(2, None, point(&heads[1][1], Some(&heads[1][0]))),
        ];
        let deltas = layer_deltas(&repo.root, &disk, &pairs).expect("deltas");
        let kinds: Vec<DeltaKind> = deltas.iter().map(|delta| delta.kind).collect();
        assert_eq!(
            kinds,
            [
                DeltaKind::Changed,
                DeltaKind::RebasedOnly,
                DeltaKind::Changed,
                DeltaKind::Unchanged,
                DeltaKind::NewToYou,
            ]
        );
        assert_eq!(
            deltas[0].files,
            [DeltaFile {
                path: "l1.txt".into(),
                since: FileSince::Changed {
                    added: 1,
                    removed: 1
                },
            }]
        );
        // A fast-forward: the commit on top is a file new to the layer's change.
        assert_eq!(
            deltas[2].files,
            [DeltaFile {
                path: "l3-more.txt".into(),
                since: FileSince::New {
                    added: 1,
                    removed: 0
                },
            }]
        );
        // The same fork point needs no replay: the old head is its own.
        assert_eq!(deltas[2].replay_oid.as_deref(), Some(heads[1][2].as_str()));

        // Replay commits are deterministic: computed again from scratch, the same ids.
        let again = layer_deltas(&repo.root, &disk, &pairs[1..2]).expect("deltas");
        assert_eq!(again[0].replay_oid, deltas[1].replay_oid);
        assert_ne!(deltas[1].replay_oid.as_deref(), Some(heads[0][1].as_str()));
    }

    #[test]
    fn cached_deltas_and_marks_answer_as_fresh_ones_do() {
        let (repo, heads) = restacked();
        let base = repo.git(0, &["rev-parse", "main"]);
        let dir = tempfile::tempdir().expect("tempdir");
        let disk = ReviewCache::open(&dir.path().join("cache.db"));
        let pairs = [pair(
            2,
            Some(point(&heads[0][1], Some(&heads[0][0]))),
            point(&heads[2][1], Some(&heads[2][0])),
        )];
        let fresh = layer_deltas(&repo.root, &ReviewCache::disabled(), &pairs).expect("fresh");
        let first = layer_deltas(&repo.root, &disk, &pairs).expect("first");
        let cached = layer_deltas(&repo.root, &disk, &pairs).expect("cached");
        assert_eq!(fresh, first);
        assert_eq!(first, cached);
        let request = ReviewCompareMarksRequest {
            repo_path: repo.root.clone(),
            fork_oid: base,
            replay_oid: heads[0][0].clone(),
            head_oid: heads[1][0].clone(),
            path: "l1.txt".into(),
            old_path: None,
        };
        let first = marks(&repo.root, &disk, &request).expect("marks");
        assert_eq!(first.new_lines, [(1, 1)]);
        assert_eq!(marks(&repo.root, &disk, &request).expect("cached"), first);
    }

    #[test]
    fn a_retargeted_layer_with_the_same_change_is_rebased_only() {
        let (repo, heads) = restacked();
        let base = repo.git(0, &["rev-parse", "main"]);
        // Layer 2 moves off layer 1, straight onto the trunk.
        repo.git(
            PUSHES[2],
            &["rebase", "-q", "--onto", "main", &heads[2][0], "l2"],
        );
        let retargeted = repo.head("l2");
        let deltas = layer_deltas(
            &repo.root,
            &ReviewCache::disabled(),
            &[pair(
                2,
                Some(point(&heads[1][1], Some(&heads[1][0]))),
                point(&retargeted, Some(&base)),
            )],
        )
        .expect("deltas");
        assert_eq!(deltas[0].kind, DeltaKind::RebasedOnly);
    }

    #[test]
    fn a_conflicting_replay_compares_with_the_rebase_and_missing_commits_are_unavailable() {
        let repo = empty_repo();
        let at = PUSHES[0];
        repo.commit(at, "f.txt", "a\n");
        let base = repo.head("main");
        repo.git(at, &["checkout", "-q", "-b", "l1"]);
        repo.commit(at, "f.txt", "b\n");
        let l1_old = repo.head("l1");
        repo.git(at, &["checkout", "-q", "-b", "l2"]);
        repo.commit(at, "f.txt", "c\n");
        let l2_old = repo.head("l2");
        // Layer 1 rewrites the line; layer 2 is redone on top of it by hand.
        repo.git(at, &["checkout", "-q", "l1"]);
        repo.write("f.txt", "x\n");
        repo.git(at, &["commit", "-q", "-a", "--amend", "--no-edit"]);
        let l1_new = repo.head("l1");
        repo.git(at, &["checkout", "-q", "-b", "l2-new"]);
        repo.commit(at, "f.txt", "y\n");
        let l2_new = repo.head("l2-new");
        let missing = "0123456789abcdef0123456789abcdef01234567";
        let deltas = layer_deltas(
            &repo.root,
            &ReviewCache::disabled(),
            &[
                pair(
                    2,
                    Some(point(&l2_old, Some(&l1_old))),
                    point(&l2_new, Some(&l1_new)),
                ),
                pair(2, Some(point(missing, Some(&base))), point(&l2_new, None)),
            ],
        )
        .expect("deltas");
        assert_eq!(deltas[0].kind, DeltaKind::WithRebase);
        assert_eq!(deltas[0].replay_oid.as_deref(), Some(l2_old.as_str()));
        assert_eq!(deltas[0].files.len(), 1);
        assert_eq!(deltas[1].kind, DeltaKind::Unavailable);
    }

    #[test]
    fn marks_tell_new_lines_from_newly_removed_and_removed_since_lines() {
        let repo = empty_repo();
        let at = PUSHES[0];
        repo.commit(at, "f.txt", "a\nb\nc\nd\ne\n");
        let fork = repo.head("main");
        repo.git(at, &["checkout", "-q", "-b", "l1"]);
        // The baseline adds `x`; the current version drops it again, removes the fork's `d`,
        // and adds `y`.
        repo.commit(at, "f.txt", "a\nb\nx\nc\nd\ne\n");
        let baseline = repo.head("l1");
        repo.write("f.txt", "a\nb\nc\ne\ny\n");
        repo.git(at, &["commit", "-q", "-a", "--amend", "--no-edit"]);
        let head = repo.head("l1");
        let request = ReviewCompareMarksRequest {
            repo_path: repo.root.clone(),
            fork_oid: fork,
            replay_oid: baseline,
            head_oid: head,
            path: "f.txt".into(),
            old_path: None,
        };
        let marked = marks(&repo.root, &ReviewCache::disabled(), &request).expect("marks");
        assert_eq!(marked.new_lines, [(5, 5)]);
        assert_eq!(marked.newly_removed, [(4, 4)]);
        assert_eq!(
            marked.removed_since,
            [RemovedSince {
                before_new_line: 3,
                lines: vec!["x".into()],
            }]
        );
        assert!(!marked.too_large);
    }

    #[test]
    fn looked_heads_are_kept_with_refs_until_released() {
        let (repo, heads) = restacked();
        let missing = "0123456789abcdef0123456789abcdef01234567".to_owned();
        let kept = keep_versions(&repo.root, &[heads[0][0].clone(), missing], &[]).expect("keep");
        assert_eq!(kept, (1, 0));
        let refs = repo.git(
            0,
            &["for-each-ref", "--format=%(refname)", "refs/bcode-review"],
        );
        assert_eq!(refs, format!("refs/bcode-review/looked/{}", heads[0][0]));
        assert_eq!(
            keep_versions(&repo.root, &[], &[heads[0][0].clone()]).expect("release"),
            (0, 1)
        );
        let refs = repo.git(
            0,
            &["for-each-ref", "--format=%(refname)", "refs/bcode-review"],
        );
        assert_eq!(refs, "");
    }

    #[test]
    fn releasing_a_looked_head_keeps_the_ref_a_fetched_version_has() {
        let (repo, heads) = restacked();
        let head = heads[0][0].clone();
        // A fetched version's ref, as the forge history writes it.
        let fetched = format!("refs/bcode-review/version/{head}");
        repo.git(0, &["update-ref", &fetched, &head]);
        keep_versions(&repo.root, std::slice::from_ref(&head), &[]).expect("keep");
        keep_versions(&repo.root, &[], &[head]).expect("release");
        let refs = repo.git(
            0,
            &["for-each-ref", "--format=%(refname)", "refs/bcode-review"],
        );
        assert_eq!(refs, fetched);
    }
}
