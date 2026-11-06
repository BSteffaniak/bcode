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

#[test]
fn failed_commands_keep_git_message_in_the_log() {
    let repo = TempRepo::init();
    repo.write("f", b"1\n");
    repo.commit_all("one");
    assert!(repo.repo.delete_branch("missing", false).is_err());
    let record = repo
        .repo
        .command_log()
        .into_iter()
        .rev()
        .find(|record| record.args.first().is_some_and(|arg| arg == "branch"))
        .unwrap();
    assert!(!record.success());
    let error = record.error.unwrap();
    assert!(
        error.contains("missing") && error.contains("not found"),
        "{error}"
    );
    // Successful commands carry no error.
    assert!(
        repo.repo
            .command_log()
            .iter()
            .filter(|record| record.success())
            .all(|record| record.error.is_none())
    );
}

#[test]
fn fast_forwards_branches_here_elsewhere_and_in_other_worktrees() {
    let remote_dir = tempfile::tempdir().unwrap();
    git(
        remote_dir.path(),
        &["init", "--quiet", "--bare", "--initial-branch", "main"],
    );
    let url = remote_dir.path().to_string_lossy().into_owned();
    let repo = TempRepo::init();
    repo.write("f", b"1\n");
    repo.commit_all("one");
    git(repo.path(), &["remote", "add", "origin", &url]);
    git(repo.path(), &["push", "--quiet", "-u", "origin", "main"]);
    git(repo.path(), &["branch", "--track", "loose", "origin/main"]);
    git(
        repo.path(),
        &["branch", "--track", "elsewhere", "origin/main"],
    );
    let worktrees = tempfile::tempdir().unwrap();
    let worktree = worktrees.path().join("elsewhere");
    git(
        repo.path(),
        &[
            "worktree",
            "add",
            "--quiet",
            worktree.to_str().unwrap(),
            "elsewhere",
        ],
    );

    // Someone else pushes a commit; nothing has been fetched yet.
    let other = tempfile::tempdir().unwrap();
    git(other.path(), &["clone", "--quiet", &url, "."]);
    let push = |content: &str, message: &str| {
        std::fs::write(other.path().join("f"), content).unwrap();
        git(other.path(), &["commit", "--quiet", "-am", message]);
        git(other.path(), &["push", "--quiet", "origin", "main"]);
        git(other.path(), &["rev-parse", "HEAD"]).trim().to_owned()
    };
    let tip = push("2\n", "two");
    let branch = |name: &str| {
        repo.repo
            .branches()
            .unwrap()
            .into_iter()
            .find(|branch| branch.name == name)
            .unwrap()
    };

    // A branch checked out nowhere only has its ref moved.
    repo.repo.fast_forward(&branch("loose")).unwrap();
    assert_eq!(git(repo.path(), &["rev-parse", "loose"]).trim(), tip);
    // A branch checked out in another worktree moves with that worktree's files.
    repo.repo.fast_forward(&branch("elsewhere")).unwrap();
    assert_eq!(git(&worktree, &["rev-parse", "HEAD"]).trim(), tip);
    assert_eq!(std::fs::read(worktree.join("f")).unwrap(), b"2\n");
    assert!(git(&worktree, &["status", "--porcelain"]).trim().is_empty());
    // The current branch is merged here.
    repo.repo.fast_forward(&branch("main")).unwrap();
    assert_eq!(repo.read("f"), b"2\n");

    // Diverged: the worktree has a commit of its own, so it cannot fast-forward.
    std::fs::write(worktree.join("g"), "mine\n").unwrap();
    git(&worktree, &["add", "g"]);
    git(&worktree, &["commit", "--quiet", "-m", "mine"]);
    let mine = git(&worktree, &["rev-parse", "HEAD"]).trim().to_owned();
    push("3\n", "three");
    let error = repo.repo.fast_forward(&branch("elsewhere")).unwrap_err();
    assert!(
        error.message().contains("fast-forward"),
        "{}",
        error.message()
    );
    assert_eq!(git(&worktree, &["rev-parse", "HEAD"]).trim(), mine);
}

/// Terminal output recorded for the last logged command whose arguments start with `first`.
fn recorded_output(repo: &TempRepo, first: &str) -> (bcode_git::models::GitCommandRecord, String) {
    let log = repo.repo.runner().log();
    let record = log
        .records()
        .into_iter()
        .rev()
        .find(|record| record.args.first().is_some_and(|arg| arg == first))
        .unwrap();
    let output = String::from_utf8_lossy(&log.output(record.id, 0)).into_owned();
    (record, output)
}

#[cfg(unix)]
#[test]
fn logged_commands_record_their_terminal_output_and_keep_stdout_exact() {
    use std::os::unix::fs::PermissionsExt as _;

    let repo = TempRepo::init();
    repo.write("f", b"1\n");
    repo.commit_all("one");
    // A hook that prints in color only on a terminal, as hook tools do.
    // Use this repository's hooks, whatever the user's configuration says.
    git(repo.path(), &["config", "core.hooksPath", ".git/hooks"]);
    let hook = repo.path().join(".git/hooks/pre-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\nif [ -t 1 ]; then printf '\\033[32mchecks passed\\033[0m\\n'; else echo 'no terminal'; fi\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    repo.write("f", b"2\n");
    git(repo.path(), &["add", "f"]);
    repo.repo
        .commit(&CommitOptions {
            message: "two".into(),
            ..CommitOptions::default()
        })
        .unwrap();
    let (record, output) = recorded_output(&repo, "commit");
    assert!(record.success() && !record.running() && !record.output_truncated);
    // The hook saw a terminal (git sends hook output to standard error).
    assert!(
        output.contains("\u{1b}[32mchecks passed\u{1b}[0m\r\n"),
        "{output:?}"
    );

    // Parsed output stays exact while its terminal copy is recorded.
    let runner = repo.repo.runner();
    let head = runner
        .run(&bcode_git::GitInvocation::read(["rev-parse", "HEAD"]).logged())
        .unwrap();
    let head = head.stdout_lossy();
    assert!(head.ends_with('\n') && !head.contains('\r'), "{head:?}");
    let (_, output) = recorded_output(&repo, "rev-parse");
    assert_eq!(output, format!("{}\r\n", head.trim_end()));
}

#[cfg(unix)]
#[test]
fn push_progress_is_recorded_on_a_terminal() {
    let remote_dir = tempfile::tempdir().unwrap();
    git(
        remote_dir.path(),
        &["init", "--quiet", "--bare", "--initial-branch", "main"],
    );
    let repo = TempRepo::init();
    // Enough objects for git to report progress.
    for index in 0..30 {
        repo.write(&format!("file-{index}"), format!("{index}\n").as_bytes());
    }
    repo.commit_all("many");
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
    let (record, output) = recorded_output(&repo, "push");
    assert!(record.success());
    // Progress is only written to a terminal.
    assert!(output.contains("Writing objects"), "{output:?}");
}

#[cfg(unix)]
#[test]
fn terminal_output_is_capped_and_marked_truncated() {
    use std::os::unix::fs::PermissionsExt as _;

    let repo = TempRepo::init();
    // Use this repository's hooks, whatever the user's configuration says.
    git(repo.path(), &["config", "core.hooksPath", ".git/hooks"]);
    let hook = repo.path().join(".git/hooks/pre-commit");
    // About 2 MiB, over the 1 MiB kept per command.
    std::fs::write(
        &hook,
        "#!/bin/sh\nawk 'BEGIN { for (i = 0; i < 32768; i++) printf \"%063d\\n\", i }' >&2\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    repo.write("f", b"1\n");
    git(repo.path(), &["add", "f"]);
    repo.repo
        .commit(&CommitOptions {
            message: "big".into(),
            ..CommitOptions::default()
        })
        .unwrap();
    let (record, _) = recorded_output(&repo, "commit");
    assert!(record.output_truncated);
    let kept = repo.repo.runner().log().output(record.id, 0).len();
    assert_eq!(kept, 1024 * 1024);
    // Reading from an offset returns only what follows it.
    assert_eq!(
        repo.repo.runner().log().output(record.id, kept - 10).len(),
        10
    );
}

#[test]
fn merged_branches_are_those_already_in_the_base_branch() {
    let repo = TempRepo::init();
    // No base branch yet: nothing is reported as merged.
    git(repo.path(), &["checkout", "--quiet", "-b", "work"]);
    repo.write("f", b"1\n");
    repo.commit_all("one");
    assert_eq!(repo.repo.base_branch().unwrap(), None);
    assert!(repo.repo.merged_branches().unwrap().is_empty());

    git(repo.path(), &["branch", "main"]);
    git(repo.path(), &["checkout", "--quiet", "main"]);
    git(repo.path(), &["branch", "done"]);
    git(repo.path(), &["checkout", "--quiet", "-b", "open"]);
    repo.write("f", b"2\n");
    repo.commit_all("unmerged work");
    git(repo.path(), &["checkout", "--quiet", "main"]);
    assert_eq!(repo.repo.base_branch().unwrap().as_deref(), Some("main"));
    let merged = repo.repo.merged_branches().unwrap();
    // `done` and `work` point into main's history; `open` has its own commit; the base
    // itself is never reported.
    assert_eq!(
        merged,
        BTreeSet::from(["done".to_owned(), "work".to_owned()])
    );

    // A remote's default branch takes precedence, and its local counterpart is excluded.
    let remote_dir = tempfile::tempdir().unwrap();
    git(
        remote_dir.path(),
        &["init", "--quiet", "--bare", "--initial-branch", "main"],
    );
    let url = remote_dir.path().to_string_lossy().into_owned();
    git(repo.path(), &["remote", "add", "origin", &url]);
    git(repo.path(), &["push", "--quiet", "origin", "main"]);
    git(repo.path(), &["remote", "set-head", "origin", "main"]);
    assert_eq!(
        repo.repo.base_branch().unwrap().as_deref(),
        Some("origin/main")
    );
    let merged = repo.repo.merged_branches().unwrap();
    assert!(
        !merged.contains("main") && merged.contains("done"),
        "{merged:?}"
    );
}
