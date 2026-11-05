//! Behavior tests against real temporary repositories.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use bcode_git::models::{
    CommitMessageSource, DiffLineId, DiffLineKind, HeadState, RebaseAction, RebaseTodoItem,
    RepoOperation, ResetMode, StatusEntryKind,
};
use bcode_git::{
    CommandLog, CommitOptions, ConflictSide, DiffTarget, LogQuery, Repository, StashOptions,
};
use tempfile::TempDir;

struct TempRepo {
    dir: TempDir,
    repo: Repository,
}

impl TempRepo {
    fn init() -> Self {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "--quiet", "--initial-branch", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test"]);
        git(dir.path(), &["config", "commit.gpgsign", "false"]);
        git(dir.path(), &["config", "core.autocrlf", "false"]);
        let repo = Repository::discover(dir.path(), CommandLog::default()).unwrap();
        Self { dir, repo }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn write(&self, name: &str, content: &[u8]) {
        let path = self.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn read(&self, name: &str) -> Vec<u8> {
        std::fs::read(self.path().join(name)).unwrap()
    }

    fn commit_all(&self, message: &str) {
        git(self.path(), &["add", "--all"]);
        git(self.path(), &["commit", "--quiet", "-m", message]);
    }

    fn index_content(&self, name: &str) -> Vec<u8> {
        self.repo.show_file(&format!(":{name}")).unwrap().unwrap()
    }

    fn unstaged(&self, name: &str) -> bcode_git::models::DiffFile {
        self.repo
            .diff(&DiffTarget::Unstaged, &[name], 3)
            .unwrap()
            .remove(0)
    }

    fn staged(&self, name: &str) -> bcode_git::models::DiffFile {
        self.repo
            .diff(&DiffTarget::Staged, &[name], 3)
            .unwrap()
            .remove(0)
    }
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn select(
    file: &bcode_git::models::DiffFile,
    predicate: impl Fn(&str) -> bool,
) -> BTreeSet<DiffLineId> {
    let mut ids = BTreeSet::new();
    for (hunk_index, hunk) in file.hunks.iter().enumerate() {
        for (line_index, line) in hunk.lines.iter().enumerate() {
            if line.kind != DiffLineKind::Context && predicate(&line.content) {
                ids.insert(DiffLineId {
                    hunk: hunk_index,
                    line: line_index,
                });
            }
        }
    }
    ids
}

#[test]
fn discovers_unborn_repository_and_status() {
    let repo = TempRepo::init();
    assert_eq!(
        repo.repo.head().unwrap(),
        HeadState::Unborn {
            name: "main".into()
        }
    );
    repo.write("a.txt", b"a\n");
    let status = repo.repo.status().unwrap();
    assert_eq!(status.entries.len(), 1);
    assert_eq!(status.entries[0].kind, StatusEntryKind::Untracked);
    assert!(repo.repo.log(&LogQuery::default()).unwrap().is_empty());
    repo.repo.stage(&["a.txt"]).unwrap();
    assert!(repo.repo.status().unwrap().entries[0].has_staged());
    repo.repo.unstage(&["a.txt"]).unwrap();
    assert!(repo.repo.status().unwrap().entries[0].is_untracked());
}

#[test]
fn stages_and_unstages_individual_lines() {
    let repo = TempRepo::init();
    repo.write("f.txt", b"one\ntwo\nthree\nfour\n");
    repo.commit_all("init");
    repo.write("f.txt", b"one\nTWO\nthree\nfour\nfive\n");

    let file = repo.unstaged("f.txt");
    repo.repo
        .stage_lines(&file, &select(&file, |line| line == "TWO"))
        .unwrap();
    assert_eq!(repo.index_content("f.txt"), b"one\ntwo\nTWO\nthree\nfour\n");

    let file = repo.unstaged("f.txt");
    repo.repo
        .stage_lines(
            &file,
            &select(&file, |line| line == "two" || line == "five"),
        )
        .unwrap();
    assert_eq!(
        repo.index_content("f.txt"),
        b"one\nTWO\nthree\nfour\nfive\n"
    );

    let staged = repo.staged("f.txt");
    repo.repo
        .unstage_lines(&staged, &select(&staged, |line| line == "five"))
        .unwrap();
    assert_eq!(repo.index_content("f.txt"), b"one\nTWO\nthree\nfour\n");
    assert_eq!(repo.read("f.txt"), b"one\nTWO\nthree\nfour\nfive\n");
}

#[test]
fn discards_individual_lines_from_worktree() {
    let repo = TempRepo::init();
    repo.write("f.txt", b"a\nb\nc\n");
    repo.commit_all("init");
    repo.write("f.txt", b"a\nB\nc\nd\n");
    let file = repo.unstaged("f.txt");
    repo.repo
        .discard_lines(&file, &select(&file, |line| line == "d"))
        .unwrap();
    assert_eq!(repo.read("f.txt"), b"a\nB\nc\n");
    let file = repo.unstaged("f.txt");
    repo.repo
        .discard_lines(&file, &select(&file, |line| line == "b"))
        .unwrap();
    assert_eq!(repo.read("f.txt"), b"a\nb\nB\nc\n");
}

#[test]
fn partial_staging_preserves_crlf_and_missing_final_newline() {
    let repo = TempRepo::init();
    repo.write("crlf.txt", b"one\r\ntwo\r\n");
    repo.write("eol.txt", b"x\ny");
    repo.commit_all("init");
    repo.write("crlf.txt", b"one\r\nmid\r\ntwo\r\nend\r\n");
    repo.write("eol.txt", b"x\nz\nw");

    let file = repo.unstaged("crlf.txt");
    repo.repo
        .stage_lines(&file, &select(&file, |line| line == "end\r"))
        .unwrap();
    assert_eq!(repo.index_content("crlf.txt"), b"one\r\ntwo\r\nend\r\n");

    let file = repo.unstaged("eol.txt");
    repo.repo
        .stage_lines(&file, &select(&file, |line| line == "w"))
        .unwrap();
    assert_eq!(repo.index_content("eol.txt"), b"x\ny\nw");
    let file = repo.unstaged("eol.txt");
    repo.repo
        .stage_lines(&file, &select(&file, |_| true))
        .unwrap();
    assert_eq!(repo.index_content("eol.txt"), b"x\nz\nw");
}

#[test]
fn untracked_files_can_be_staged_line_by_line_after_intent_to_add() {
    let repo = TempRepo::init();
    repo.write("base.txt", b"base\n");
    repo.commit_all("init");
    repo.write("new.txt", b"keep\nskip\n");
    let untracked = repo
        .repo
        .diff(&DiffTarget::Untracked, &["new.txt"], 3)
        .unwrap();
    assert_eq!(untracked[0].additions, 2);
    repo.repo.intent_to_add(&["new.txt"]).unwrap();
    let file = repo.unstaged("new.txt");
    repo.repo
        .stage_lines(&file, &select(&file, |line| line == "keep"))
        .unwrap();
    assert_eq!(repo.index_content("new.txt"), b"keep\n");
}

#[test]
fn commits_amends_and_rewords() {
    let repo = TempRepo::init();
    repo.write("a", b"1\n");
    repo.repo.stage_all().unwrap();
    repo.repo
        .commit(&CommitOptions {
            message: "first\n\nbody".into(),
            ..CommitOptions::default()
        })
        .unwrap();
    repo.write("a", b"2\n");
    repo.repo.stage_all().unwrap();
    repo.repo
        .commit(&CommitOptions {
            message: "second".into(),
            ..CommitOptions::default()
        })
        .unwrap();
    let log = repo.repo.log(&LogQuery::default()).unwrap();
    assert_eq!(log.len(), 2);
    assert_eq!(
        repo.repo.commit_message(&log[1].oid).unwrap(),
        "first\n\nbody"
    );

    repo.repo.reword(&log[1].oid, "renamed first").unwrap();
    let log = repo.repo.log(&LogQuery::default()).unwrap();
    assert_eq!(log[1].summary, "renamed first");
    assert_eq!(log[0].summary, "second");

    repo.repo.reword(&log[0].oid, "renamed second").unwrap();
    assert_eq!(
        repo.repo.log(&LogQuery::default()).unwrap()[0].summary,
        "renamed second"
    );
}

#[test]
fn interactive_rebase_squashes_drops_and_reorders() {
    let repo = TempRepo::init();
    repo.write("base", b"base\n");
    repo.commit_all("base");
    for name in ["a", "b", "c"] {
        repo.write(name, name.as_bytes());
        repo.commit_all(name);
    }
    let log = repo.repo.log(&LogQuery::default()).unwrap();
    let oid = |summary: &str| {
        log.iter()
            .find(|commit| commit.summary == summary)
            .unwrap()
            .oid
            .clone()
    };
    let base = oid("base");
    repo.repo
        .rebase_interactive(
            Some(&base),
            &[
                RebaseTodoItem {
                    action: RebaseAction::Pick,
                    oid: oid("c"),
                    summary: "c".into(),
                },
                RebaseTodoItem {
                    action: RebaseAction::Fixup,
                    oid: oid("a"),
                    summary: "a".into(),
                },
                RebaseTodoItem {
                    action: RebaseAction::Drop,
                    oid: oid("b"),
                    summary: "b".into(),
                },
            ],
        )
        .unwrap();
    let summaries: Vec<String> = repo
        .repo
        .log(&LogQuery::default())
        .unwrap()
        .into_iter()
        .map(|commit| commit.summary)
        .collect();
    assert_eq!(summaries, ["c", "base"]);
    assert!(repo.path().join("a").exists());
    assert!(!repo.path().join("b").exists());
    assert_eq!(repo.repo.operation(), RepoOperation::None);
}

#[test]
fn branches_merge_conflicts_and_resolution() {
    let repo = TempRepo::init();
    repo.write("f", b"base\n");
    repo.commit_all("base");
    repo.repo.create_branch("feature", None, true).unwrap();
    repo.write("f", b"feature\n");
    repo.commit_all("feature");
    repo.repo.checkout("main").unwrap();
    repo.write("f", b"main\n");
    repo.commit_all("main");

    let branches = repo.repo.branches().unwrap();
    assert_eq!(branches.len(), 2);
    assert!(
        branches
            .iter()
            .any(|branch| branch.is_head && branch.name == "main")
    );

    assert!(repo.repo.merge("feature", false, false).is_err());
    assert_eq!(repo.repo.operation(), RepoOperation::Merging);
    let status = repo.repo.status().unwrap();
    assert!(status.entries[0].is_conflicted());
    repo.repo
        .resolve_conflict("f", ConflictSide::Theirs)
        .unwrap();
    assert_eq!(repo.read("f"), b"feature\n");
    repo.repo.continue_operation().unwrap();
    assert_eq!(repo.repo.operation(), RepoOperation::None);
    assert!(repo.repo.log(&LogQuery::default()).unwrap()[0].is_merge());

    repo.repo.rename_branch("feature", "done").unwrap();
    repo.repo.delete_branch("done", false).unwrap();
    assert_eq!(repo.repo.branches().unwrap().len(), 1);
}

#[test]
fn stash_tag_reset_and_reflog() {
    let repo = TempRepo::init();
    repo.write("f", b"1\n");
    repo.commit_all("one");
    repo.write("f", b"2\n");
    repo.commit_all("two");
    repo.write("f", b"dirty\n");
    repo.write("u", b"untracked\n");
    repo.repo
        .stash_push(&StashOptions {
            message: Some("wip".into()),
            include_untracked: true,
            ..StashOptions::default()
        })
        .unwrap();
    assert!(repo.repo.status().unwrap().entries.is_empty());
    let stashes = repo.repo.stashes().unwrap();
    assert_eq!(stashes.len(), 1);
    assert!(stashes[0].message.contains("wip"));
    let stash_diff = repo
        .repo
        .diff(&DiffTarget::Stash(stashes[0].selector()), &[], 3)
        .unwrap();
    assert!(!stash_diff.is_empty());
    repo.repo.stash_rename(&stashes[0], "renamed").unwrap();
    assert_eq!(repo.repo.stashes().unwrap()[0].message, "renamed");
    repo.repo.stash_apply("stash@{0}", true).unwrap();
    assert_eq!(repo.read("f"), b"dirty\n");
    assert!(repo.repo.stashes().unwrap().is_empty());

    repo.repo.create_tag("v1", "HEAD", Some("release")).unwrap();
    repo.repo.create_tag("light", "HEAD~1", None).unwrap();
    let tags = repo.repo.tags().unwrap();
    assert_eq!(tags.len(), 2);
    assert!(tags.iter().any(|tag| tag.name == "v1" && tag.annotated));

    repo.repo.discard(&["f"], &["u"]).unwrap();
    assert!(repo.repo.status().unwrap().entries.is_empty());
    repo.repo.reset("HEAD~1", ResetMode::Hard).unwrap();
    assert_eq!(repo.read("f"), b"1\n");
    let reflog = repo.repo.reflog(10).unwrap();
    assert!(reflog[0].message.starts_with("reset"));
    assert!(
        repo.repo
            .command_log()
            .iter()
            .any(|record| record.args[0] == "reset")
    );
}

#[test]
fn pushes_fetches_and_tracks_a_local_remote() {
    let remote_dir = tempfile::tempdir().unwrap();
    git(
        remote_dir.path(),
        &["init", "--quiet", "--bare", "--initial-branch", "main"],
    );
    let repo = TempRepo::init();
    repo.write("f", b"1\n");
    repo.commit_all("one");
    let url = remote_dir.path().to_string_lossy().into_owned();
    repo.repo.add_remote("origin", &url).unwrap();
    repo.repo
        .push(&bcode_git::PushOptions {
            remote: Some("origin".into()),
            refspec: Some("main".into()),
            set_upstream: true,
            ..bcode_git::PushOptions::default()
        })
        .unwrap();
    repo.repo.fetch(None).unwrap();
    assert_eq!(
        repo.repo.remote_branches().unwrap()[0].full_name(),
        "origin/main"
    );
    let status = repo.repo.status().unwrap();
    assert_eq!(status.branch.upstream.as_deref(), Some("origin/main"));
    repo.write("f", b"2\n");
    repo.commit_all("two");
    let branch = repo.repo.branches().unwrap().remove(0);
    assert_eq!(branch.ahead, 1);
    assert_eq!(repo.repo.remotes().unwrap()[0].name, "origin");
}

#[test]
fn renamed_files_report_both_paths_in_staged_diff() {
    let repo = TempRepo::init();
    repo.write("old name.txt", b"same content here\nline2\nline3\n");
    repo.commit_all("init");
    git(repo.path(), &["mv", "old name.txt", "new name.txt"]);
    let staged = repo.repo.diff(&DiffTarget::Staged, &[], 3).unwrap();
    assert_eq!(staged.len(), 1);
    assert_eq!(staged[0].old_path.as_deref(), Some("old name.txt"));
    assert_eq!(staged[0].path(), "new name.txt");
    let status = repo.repo.status().unwrap();
    assert_eq!(status.entries[0].kind, StatusEntryKind::RenamedOrCopied);
}

#[test]
fn lists_changed_files_and_amends_into_older_commit() {
    let repo = TempRepo::init();
    repo.write("a", b"1\n");
    repo.commit_all("first");
    repo.write("b", b"2\n");
    repo.commit_all("second");
    let log = repo.repo.log(&LogQuery::default()).unwrap();
    let files = repo
        .repo
        .changed_files(&DiffTarget::Commit(log[1].oid.clone()))
        .unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "a");

    repo.write("a", b"1\nfixed\n");
    repo.repo.stage(&["a"]).unwrap();
    repo.repo.amend_into(&log[1].oid).unwrap();
    let log = repo.repo.log(&LogQuery::default()).unwrap();
    assert_eq!(log.len(), 2);
    assert_eq!(log[1].summary, "first");
    assert_eq!(
        repo.repo
            .show_file(&format!("{}:a", log[1].oid))
            .unwrap()
            .unwrap(),
        b"1\nfixed\n"
    );
    assert!(repo.repo.unpushed().unwrap().is_empty());
}

fn message(repo: &TempRepo, source: CommitMessageSource) -> Option<String> {
    repo.repo.commit_message_from(source).unwrap()
}

#[test]
fn commit_message_sources_and_drafts() {
    let repo = TempRepo::init();
    let git_dir = repo.path().join(".git");
    repo.write("a", b"a\n");
    repo.commit_all("first");

    // A successful commit leaves COMMIT_EDITMSG behind; it must not be offered again.
    std::fs::write(git_dir.join("COMMIT_EDITMSG"), "first\n# Please enter...\n").unwrap();
    assert_eq!(message(&repo, CommitMessageSource::EditMsg), None);
    // A rejected commit's message differs from HEAD's and is offered without comments.
    std::fs::write(
        git_dir.join("COMMIT_EDITMSG"),
        "attempt\n\n# comment\nbody\n",
    )
    .unwrap();
    assert_eq!(
        message(&repo, CommitMessageSource::EditMsg).as_deref(),
        Some("attempt\n\nbody")
    );

    // Templates resolve relative to the worktree and lose comment lines.
    repo.write("template.txt", b"feat: \n# describe the change\n");
    git(repo.path(), &["config", "commit.template", "template.txt"]);
    assert_eq!(
        message(&repo, CommitMessageSource::Template).as_deref(),
        Some("feat:")
    );

    // Drafts keep `#` lines, which are message content.
    std::fs::write(git_dir.join("LAZYGIT_PENDING_COMMIT"), "#42 fix\n").unwrap();
    assert_eq!(
        message(&repo, CommitMessageSource::LazygitDraft).as_deref(),
        Some("#42 fix")
    );
    repo.repo.save_commit_draft("Mine\n\nDetails").unwrap();
    assert_eq!(
        message(&repo, CommitMessageSource::BcodeDraft).as_deref(),
        Some("Mine\n\nDetails")
    );
    repo.repo.save_commit_draft("  ").unwrap();
    assert_eq!(message(&repo, CommitMessageSource::BcodeDraft), None);

    repo.repo.save_commit_draft("again").unwrap();
    repo.repo.clear_commit_drafts(true, false).unwrap();
    assert_eq!(message(&repo, CommitMessageSource::BcodeDraft), None);
    assert!(message(&repo, CommitMessageSource::LazygitDraft).is_some());
    repo.repo.clear_commit_drafts(false, true).unwrap();
    assert_eq!(message(&repo, CommitMessageSource::LazygitDraft), None);
    assert_eq!(message(&repo, CommitMessageSource::Env), None);
}

#[test]
fn conflicted_cherry_pick_offers_merge_msg() {
    let repo = TempRepo::init();
    repo.write("f", b"base\n");
    repo.commit_all("base");
    git(repo.path(), &["checkout", "--quiet", "-b", "other"]);
    repo.write("f", b"other\n");
    repo.commit_all("Other change");
    git(repo.path(), &["checkout", "--quiet", "main"]);
    repo.write("f", b"main\n");
    repo.commit_all("Main change");
    let failed = Command::new("git")
        .args(["cherry-pick", "other"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(!failed.status.success());
    let text = message(&repo, CommitMessageSource::Merge).unwrap();
    assert!(text.starts_with("Other change"), "{text}");
    assert!(!text.contains('#'), "{text}");
}

#[test]
fn commit_messages_keep_lines_starting_with_hash() {
    let repo = TempRepo::init();
    repo.write("a", b"a\n");
    git(repo.path(), &["add", "a"]);
    repo.repo
        .commit(&CommitOptions {
            message: "#12 fix the thing\n\n# not a comment".into(),
            ..CommitOptions::default()
        })
        .unwrap();
    assert_eq!(
        git(repo.path(), &["log", "-1", "--format=%B"]),
        "#12 fix the thing\n\n# not a comment\n\n"
    );
}

#[test]
fn worktrees_keep_their_own_draft() {
    let repo = TempRepo::init();
    repo.write("a", b"a\n");
    repo.commit_all("first");
    let other = repo.path().join("wt");
    git(
        repo.path(),
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "side",
            other.to_str().unwrap(),
        ],
    );
    let linked = Repository::discover(&other, CommandLog::default()).unwrap();
    linked.save_commit_draft("linked draft").unwrap();
    assert_eq!(message(&repo, CommitMessageSource::BcodeDraft), None);
    assert_eq!(
        linked
            .commit_message_from(CommitMessageSource::BcodeDraft)
            .unwrap()
            .as_deref(),
        Some("linked draft")
    );
}
