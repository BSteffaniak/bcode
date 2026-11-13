//! Placing a pull request's threads on a review whose head differs from the forge's, and judging
//! reviews against a layer's history.

use super::*;
use bcode_code_review_models::{
    LayerVersion, Participant, PullState, RemoteAnchor, RemoteComment, TimelineVersion,
};
use std::fmt::Write as _;
use std::process::Command;

struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let repo = Self { _dir: dir, root };
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_AUTHOR_DATE", "@1700000000 +0000")
            .env("GIT_COMMITTER_DATE", "@1700000000 +0000")
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn commit(&self, path: &str, text: &str) -> String {
        std::fs::write(self.root.join(path), text).expect("write");
        self.git(&["add", path]);
        self.git(&["commit", "-q", "-m", path]);
        self.git(&["rev-parse", "HEAD"])
    }
}

fn lines(range: std::ops::RangeInclusive<u32>) -> String {
    range.fold(String::new(), |mut text, line| {
        let _ = writeln!(text, "line {line}");
        text
    })
}

fn thread(id: &str, anchor: RemoteAnchor) -> RemoteThread {
    RemoteThread {
        id: id.to_owned(),
        anchor,
        resolved: false,
        outdated: false,
        can_reply: true,
        can_resolve: true,
        comments: vec![comment("ana", "look")],
    }
}

fn comment(login: &str, body: &str) -> RemoteComment {
    RemoteComment {
        id: format!("{login}:{body}"),
        author: Participant {
            login: login.to_owned(),
            is_viewer: login == "me",
            ..Participant::default()
        },
        body: body.to_owned(),
        created_at_ms: Some(1),
        updated_at_ms: None,
        url: None,
        review_id: None,
    }
}

fn activity(head: &str, threads: Vec<RemoteThread>) -> PullActivity {
    PullActivity {
        number: 7,
        title: "t".to_owned(),
        head_oid: head.to_owned(),
        state: PullState::Open,
        author: None,
        description: String::new(),
        url: None,
        decision: None,
        reviews: Vec::new(),
        requested: Vec::new(),
        threads,
        comments: Vec::new(),
        checks: bcode_code_review_models::Checks::default(),
        viewer: bcode_code_review_models::ViewerAbilities::default(),
        updated_at_ms: None,
    }
}

fn at_line(path: &str, line: Option<u32>, original: u32, commit: &str) -> RemoteAnchor {
    RemoteAnchor {
        path: path.to_owned(),
        side: DiffSide::New,
        line,
        original_line: Some(original),
        original_commit_oid: Some(commit.to_owned()),
        ..RemoteAnchor::default()
    }
}

#[test]
fn threads_follow_their_code_to_the_head_the_review_shows() {
    let repo = Repo::new();
    let written = repo.commit("a.rs", &lines(1..=20));
    // Three lines inserted at the top, and line 10 rewritten.
    let mut text = String::from("new 1\nnew 2\nnew 3\n");
    for line in 1..=20 {
        if line == 10 {
            text.push_str("changed\n");
        } else {
            let _ = writeln!(text, "line {line}");
        }
    }
    let head = repo.commit("a.rs", &text);
    let target = LayerTarget {
        source_id: "s".to_owned(),
        head_oid: head,
        paths: BTreeSet::from(["a.rs".to_owned()]),
    };
    let placed = place_threads(
        &repo.root,
        &activity(
            &written,
            vec![
                // The forge still places it, on its own (older) head.
                thread("kept", at_line("a.rs", Some(5), 5, &written)),
                // Outdated on the forge: only where it was written is known.
                thread("outdated", at_line("a.rs", None, 15, &written)),
                // Its line was rewritten: on the line that replaced it.
                thread("rewritten", at_line("a.rs", Some(10), 10, &written)),
                thread("elsewhere", at_line("b.rs", Some(1), 1, &written)),
            ],
        ),
        &target,
    );
    let summary: Vec<(&str, Placement, Option<u32>)> = placed
        .iter()
        .map(|placed| {
            (
                placed.thread.id.as_str(),
                placed.placement,
                placed.anchor.new_line,
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            ("kept", Placement::Moved, Some(8)),
            ("outdated", Placement::Moved, Some(18)),
            ("rewritten", Placement::Near, Some(13)),
            ("elsewhere", Placement::OnLayer, None),
        ]
    );
    assert!(
        placed
            .iter()
            .all(|placed| placed.anchor.source_id.as_deref() == Some("s"))
    );
    assert_eq!(placed[2].anchor.kind, ReviewAnchorKind::Range);
    assert_eq!(placed[3].anchor.kind, ReviewAnchorKind::Review);
}

#[test]
fn threads_on_the_same_head_are_placed_exactly_without_git() {
    let target = LayerTarget {
        source_id: "s".to_owned(),
        head_oid: "a".repeat(40),
        paths: BTreeSet::from(["a.rs".to_owned()]),
    };
    let mut old_side = at_line("a.rs", Some(3), 3, &"b".repeat(40));
    old_side.side = DiffSide::Old;
    old_side.start_line = Some(2);
    // The root does not exist: nothing here may need git.
    let placed = place_threads(
        Path::new("/nonexistent"),
        &activity(
            &"a".repeat(40),
            vec![
                thread("old", old_side),
                thread("new", at_line("a.rs", Some(4), 9, "x")),
            ],
        ),
        &target,
    );
    assert_eq!(placed[0].placement, Placement::Exact);
    assert_eq!(
        (placed[0].anchor.old_start, placed[0].anchor.old_end),
        (Some(2), Some(3))
    );
    assert_eq!(placed[1].anchor.new_line, Some(4));
}

#[test]
fn carrying_shifts_past_insertions_and_drops_changed_lines() {
    let hunks = parse_line_changes(
        "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -0,0 +1,2 @@\n+x\n+y\n@@ -5 +7,3 @@ fn\n-a\n+b\n+c\n+d\n@@ -9,2 +11,0 @@\n-e\n-f\n",
    );
    let hunks = &hunks["f"];
    assert_eq!(carry_line(hunks, 1), Some(3));
    assert_eq!(carry_line(hunks, 4), Some(6));
    assert_eq!(carry_line(hunks, 5), None);
    assert_eq!(carry_line(hunks, 6), Some(10));
    assert_eq!(carry_line(hunks, 9), None);
    assert_eq!(carry_line(hunks, 11), Some(13));
}

fn version(head: &str, kind: ChangeKind) -> TimelineVersion {
    TimelineVersion {
        version: LayerVersion {
            provider: "p".to_owned(),
            repository: "r".to_owned(),
            number: 7,
            head_oid: head.to_owned(),
            base_oid: None,
            base_branch: None,
            seen_at_ms: 0,
            actor: None,
            sources: BTreeSet::new(),
        },
        kind,
        fork_oid: None,
        reviewed: false,
    }
}

#[test]
fn a_rebase_keeps_a_review_current_and_a_content_change_does_not() {
    let layer = LayerTimeline {
        number: 7,
        title: String::new(),
        head_branch: String::new(),
        versions: vec![
            version("a", ChangeKind::First),
            version("b", ChangeKind::RebaseOnly),
            version("c", ChangeKind::Content),
            version("d", ChangeKind::RebaseOnly),
        ],
    };
    assert_eq!(review_currency(&layer, "d", "d"), Currency::Current);
    assert_eq!(review_currency(&layer, "c", "d"), Currency::Rebased);
    assert_eq!(review_currency(&layer, "a", "d"), Currency::Outdated);
    assert_eq!(review_currency(&layer, "z", "d"), Currency::Unknown);
}

#[test]
fn a_thread_waits_on_the_viewer_when_someone_else_spoke_last() {
    let mut pull = activity("h", Vec::new());
    let mut asked = thread("t", RemoteAnchor::default());
    asked.comments = vec![comment("me", "why?"), comment("ana", "because")];
    assert!(waiting_on_viewer(&pull, &asked));
    asked.comments.push(comment("me", "ok"));
    assert!(!waiting_on_viewer(&pull, &asked));
    let stranger = thread("s", RemoteAnchor::default());
    assert!(!waiting_on_viewer(&pull, &stranger));
    pull.viewer.is_author = true;
    assert!(waiting_on_viewer(&pull, &stranger));
}

#[test]
fn a_range_whose_code_was_partly_removed_stays_by_what_is_left_of_it() {
    let repo = Repo::new();
    let written = repo.commit("a.rs", &lines(1..=30));
    // Three lines inserted at the top; lines 21 to 25 removed (the end of the commented range).
    let mut text = String::from("new 1\nnew 2\nnew 3\n");
    for line in (1..=30).filter(|line| !(21..=25).contains(line)) {
        let _ = writeln!(text, "line {line}");
    }
    let head = repo.commit("a.rs", &text);
    let target = LayerTarget {
        source_id: "s".to_owned(),
        head_oid: head,
        paths: BTreeSet::from(["a.rs".to_owned()]),
    };
    // Outdated on the forge, written on lines 12 to 24.
    let mut range = at_line("a.rs", None, 24, &written);
    range.original_start_line = Some(12);
    let placed = place_threads(
        &repo.root,
        &activity(&written, vec![thread("range", range)]),
        &target,
    );
    assert_eq!(placed[0].placement, Placement::Near);
    // From line 12 (now 15) to where the removed lines were: after line 20 (now 23).
    assert_eq!(
        (placed[0].anchor.new_start, placed[0].anchor.new_end),
        (Some(15), Some(23))
    );
    assert_eq!(placed[0].anchor.kind, ReviewAnchorKind::Range);
}
