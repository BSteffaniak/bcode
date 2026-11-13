//! A three-layer stack pushed three times, read back from its reflogs: the first push, then the
//! bottom layer edited with the two above restacked onto it, then a commit added on top.

use super::compare::{changes_since, layer_changes, stack_at, whole_stack_since};
use super::reflog::reflog_versions;
use super::*;
use bcode_code_review_models::{PullRequestSummary, ReviewSourceKind, ReviewStackLayer};
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

    /// The files `base..head` changes.
    fn changed(&self, source: &ReviewSourceKind) -> Vec<String> {
        let ReviewSourceKind::CommitRange { base, head, .. } = source else {
            panic!("{source:?}");
        };
        let listed = self.git(0, &["diff", "--name-only", base, head]);
        listed.lines().map(str::to_owned).collect()
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
    let (repo, heads) = restacked();
    let timeline = history(&repo);
    let [first, second, _] = PUSHES;
    let after_first = first * 1000 + 1;

    // A rebase-only version compares as no change at all.
    let rebased = layer_changes(&repo.root, &timeline.layers[1], 0, 1)
        .expect("compare")
        .expect("different heads");
    assert!(rebased.rebase_removed);
    assert_eq!(repo.changed(&rebased.source), Vec::<String>::new());

    // Since the first push: only the edited layer and the commit on top, nothing from rebases.
    let since = changes_since(&repo.root, &timeline, after_first, false).expect("since");
    let changed: Vec<(u64, Vec<String>)> = since
        .iter()
        .map(|comparison| (comparison.number, repo.changed(&comparison.source)))
        .collect();
    assert_eq!(
        changed,
        [
            (1, vec!["l1.txt".to_owned()]),
            (3, vec!["l3-more.txt".to_owned()]),
        ]
    );
    // Asked for, the rebased layer is there too, with nothing in it.
    let all = changes_since(&repo.root, &timeline, after_first, true).expect("since");
    assert_eq!(all.len(), 3);
    assert_eq!(repo.changed(&all[1].source), Vec::<String>::new());

    // The stack as it was after the first push: each layer's own change then.
    let then = stack_at(&timeline, after_first);
    let heads_then: Vec<&str> = then
        .iter()
        .map(|comparison| match &comparison.source {
            ReviewSourceKind::CommitRange { head, .. } => head.as_str(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        heads_then,
        heads[0].iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert_eq!(repo.changed(&then[1].source), ["l2.txt"]);

    // The whole stack since the first push: the edit and the new commit, together.
    let whole = whole_stack_since(&repo.root, &timeline, after_first)
        .expect("whole")
        .expect("a stack then and now");
    assert_eq!(repo.changed(&whole.source), ["l1.txt", "l3-more.txt"]);
    // Nothing happened since the last push but the commit on top.
    let latest = changes_since(&repo.root, &timeline, second * 1000 + 1, false).expect("since");
    assert_eq!(latest.len(), 1);
    assert_eq!(latest[0].number, 3);
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
