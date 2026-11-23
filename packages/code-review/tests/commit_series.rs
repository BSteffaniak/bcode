//! Black-box tests for selected-commit series, using independent repository histories.

use std::{fs, process::Command};

use bcode_code_review::{DataError, ReviewData};
use bcode_code_review_models::{
    CommitSeriesResolution, ReviewSource, ReviewSourceKind, RevisionRef,
};
use tempfile::TempDir;

const SOURCE: &str = "picked-commits";

struct Repo(TempDir);

impl Repo {
    fn new() -> Self {
        let repo = Self(tempfile::tempdir().expect("temporary repository"));
        repo.git(&["init", "-b", "main"]);
        repo.git(&["config", "user.name", "Series Test"]);
        repo.git(&["config", "user.email", "series@example.invalid"]);
        repo.git(&["config", "commit.gpgSign", "false"]);
        repo.git(&["config", "core.hooksPath", "/dev/null"]);
        repo
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(self.0.path())
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .expect("start git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("fixture output")
            .trim_end_matches('\n')
            .to_owned()
    }

    fn write(&self, path: &str, text: &str) {
        fs::write(self.0.path().join(path), text).expect("write fixture file");
    }

    fn commit(&self, path: &str, text: &str) -> String {
        self.write(path, text);
        self.git(&["add", "--", path]);
        self.git(&["commit", "-m", path]);
        self.git(&["rev-parse", "HEAD"])
    }

    fn series(&self, revs: &[&str], combined: bool) -> CommitSeriesResolution {
        self.try_series(revs, combined).expect("plan series")
    }

    fn try_series(
        &self,
        revs: &[&str],
        combined: bool,
    ) -> Result<CommitSeriesResolution, DataError> {
        let source = ReviewSource {
            id: SOURCE.into(),
            label: "Selected commits".into(),
            kind: ReviewSourceKind::Commits {
                revs: revs.iter().map(|s| (*s).to_owned()).collect(),
            },
            included: true,
        };
        ReviewData::default().commit_series(self.0.path(), &source, combined)
    }

    fn dirty(&self) {
        self.write("base", "staged\n");
        self.git(&["add", "base"]);
        self.write("base", "unstaged\n");
        self.write("scratch", "not tracked\n");
    }

    fn checkout_state(&self) -> Vec<String> {
        vec![
            self.git(&["symbolic-ref", "HEAD"]),
            self.git(&["show-ref"]),
            self.git(&["worktree", "list", "--porcelain"]),
            self.git(&["ls-files", "--stage"]),
            self.git(&["diff", "--no-ext-diff", "--binary"]),
            self.git(&["ls-files", "--others", "--exclude-standard"]),
            fs::read_to_string(self.0.path().join("scratch")).expect("untracked contents"),
        ]
    }
}

fn assert_members(series: &CommitSeriesResolution, oids: &[&str]) {
    assert_eq!(series.members.len(), oids.len());
    for (index, (member, oid)) in series.members.iter().zip(oids).enumerate() {
        assert_eq!(member.source_id, SOURCE);
        assert_eq!(member.head.commit_oid(), Some(*oid));
        let part = member.part.as_ref().expect("member position");
        assert_eq!(part.index as usize, index);
        assert_eq!(part.count as usize, oids.len());
    }
}

fn combined_head<'a>(series: &'a CommitSeriesResolution, base: &str) -> &'a str {
    assert_eq!(series.unavailable, None);
    let combined = series.combined.as_ref().expect("exact aggregate");
    assert_eq!(combined.source_id, SOURCE);
    assert_eq!(combined.base.commit_oid(), Some(base));
    assert!(combined.part.is_none());
    combined.head.commit_oid().expect("revision-pinned head")
}

#[test]
fn reversed_contiguous_selection_preserves_first_occurrences_and_real_endpoints() {
    let repo = Repo::new();
    let base = repo.commit("base", "start\n");
    let older = repo.commit("a", "one\n");
    let newer = repo.commit("a", "two\n");
    let selection = ["HEAD", older.as_str(), newer.as_str()];
    for combined in [false, true] {
        let series = repo.series(&selection, combined);
        assert!(series.contiguous);
        assert_members(&series, &[&newer, &older]);
        if combined {
            assert_eq!(combined_head(&series, &base), newer);
        } else {
            assert!(series.combined.is_none());
        }
    }
}

#[test]
fn reversed_sparse_selection_replays_in_ancestry_order_only() {
    let repo = Repo::new();
    let base = repo.commit("base", "start\n");
    let older = repo.commit("a", "one\n");
    repo.commit("excluded", "skip\n");
    let newer = repo.commit("a", "two\n");
    for combined in [false, true] {
        let series = repo.series(&[&newer, &older, "HEAD"], combined);
        assert!(!series.contiguous);
        assert_members(&series, &[&newer, &older]);
        if combined {
            let head = combined_head(&series, &base);
            assert_eq!(repo.git(&["diff", "--name-only", &base, head]), "a");
            assert_eq!(repo.git(&["show", &format!("{head}:a")]), "two");
        } else {
            assert!(series.combined.is_none());
        }
    }
}

#[test]
fn shallow_boundary_and_missing_selection_are_not_partial_or_empty_tree_diffs() {
    let upstream = Repo::new();
    upstream.commit("base", "start\n");
    let boundary = upstream.commit("a", "one\n");
    let tip = upstream.commit("b", "two\n");
    let clone = Repo(tempfile::tempdir().expect("temporary clone"));
    clone.git(&[
        "clone",
        "--no-local",
        "--depth=2",
        upstream.0.path().to_str().unwrap(),
        ".",
    ]);
    for combined in [false, true] {
        let valid = clone.series(&[&tip], combined);
        assert_eq!(valid.members[0].base.commit_oid(), Some(boundary.as_str()));
        assert!(matches!(
            clone.try_series(&[&tip, &boundary], combined),
            Err(DataError::NotFound(_))
        ));
        assert!(matches!(
            clone.try_series(&[&tip, "missing-revision"], combined),
            Err(DataError::NotFound(_))
        ));
    }
}

#[test]
fn contiguous_members_keep_individual_bases_and_combine_at_real_endpoints() {
    let repo = Repo::new();
    let base = repo.commit("base", "start\n");
    let first = repo.commit("a", "one\n");
    let last = repo.commit("a", "two\n");
    let series = repo.series(&[&first, &last], true);
    assert!(series.contiguous);
    assert_members(&series, &[&first, &last]);
    assert_eq!(series.members[0].base.commit_oid(), Some(base.as_str()));
    assert_eq!(series.members[1].base.commit_oid(), Some(first.as_str()));
    assert_eq!(combined_head(&series, &base), last);
}

#[test]
fn single_root_member_uses_the_empty_tree() {
    let repo = Repo::new();
    let root = repo.commit("base", "start\n");
    let series = repo.series(&["HEAD"], false);
    assert_members(&series, &[&root]);
    assert_eq!(series.members[0].base, RevisionRef::EmptyTree);
}

#[test]
fn sparse_replay_excludes_skipped_changes_even_inside_a_selected_file() {
    let repo = Repo::new();
    let base = repo.commit("base", "start\n");
    let first = repo.commit("a", "selected\n");
    let skipped = repo.commit("a", "skipped overwrite\n");
    let last = repo.commit("b", "also selected\n");
    repo.dirty();
    let before = repo.checkout_state();
    let revs = [first.as_str(), last.as_str()];
    let objects = || {
        repo.git(&[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ])
    };
    let original_objects = objects();
    let description = repo.series(&revs, false);
    assert!(!description.contiguous);
    assert_members(&description, &revs);
    assert!(description.combined.is_none());
    assert_eq!(
        objects(),
        original_objects,
        "description materialized replay objects"
    );
    assert_eq!(repo.checkout_state(), before);

    let exact = repo.series(&revs, true);
    assert_eq!(exact.members, description.members);
    assert_eq!(exact.members[1].base.commit_oid(), Some(skipped.as_str()));
    let head = combined_head(&exact, &base);
    assert_eq!(repo.git(&["diff", "--name-only", &base, head]), "a\nb");
    assert_eq!(repo.git(&["show", &format!("{head}:a")]), "selected");
    assert_eq!(repo.git(&["show", &format!("{head}:b")]), "also selected");
    assert_eq!(repo.checkout_state(), before);
}

#[test]
fn omitted_dependency_gives_diagnostic_not_an_inclusive_range() {
    let repo = Repo::new();
    repo.commit("base", "start\n");
    let first = repo.commit("a", "selected\n");
    repo.commit("dependency", "skipped creation\n");
    let last = repo.commit("dependency", "selected modification\n");
    repo.dirty();
    let before = repo.checkout_state();
    let series = repo.series(&[&first, &last], true);
    assert!(!series.contiguous);
    assert_members(&series, &[&first, &last]);
    assert!(series.combined.is_none());
    assert!(
        series
            .unavailable
            .as_deref()
            .is_some_and(|why| why.contains("dependency"))
    );
    assert_eq!(repo.checkout_state(), before);
}

#[test]
fn sparse_revert_has_no_net_change_but_keeps_both_members() {
    let repo = Repo::new();
    let base = repo.commit("base", "start\n");
    let first = repo.commit("a", "temporary\n");
    repo.commit("skip", "excluded\n");
    repo.git(&["revert", "--no-edit", &first]);
    let revert = repo.git(&["rev-parse", "HEAD"]);
    let series = repo.series(&[&first, &revert], true);
    assert_members(&series, &[&first, &revert]);
    let head = combined_head(&series, &base);
    assert_eq!(repo.git(&["diff", "--name-only", &base, head]), "");
}

#[test]
fn merge_member_is_the_delta_from_its_first_parent() {
    let repo = Repo::new();
    let base = repo.commit("base", "start\n");
    repo.git(&["checkout", "-b", "topic"]);
    repo.commit("topic-file", "topic\n");
    repo.git(&["checkout", "main"]);
    let main = repo.commit("main-file", "main\n");
    repo.git(&["merge", "--no-ff", "-m", "merge topic", "topic"]);
    let merge = repo.git(&["rev-parse", "HEAD"]);
    let series = repo.series(&[&main, &merge], true);
    assert_members(&series, &[&main, &merge]);
    assert_eq!(series.members[1].base.commit_oid(), Some(main.as_str()));
    assert!(series.contiguous);
    assert_eq!(combined_head(&series, &base), merge);
}
