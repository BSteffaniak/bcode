//! Re-review (spec 7.4) over small fixture branches: passes are recorded only when marked
//! complete; a fast-forward offers the diff from the reviewed head to the new head; after a
//! rebase, files whose patch is unchanged stay viewed and are de-emphasized.

use super::app::{create, open, settled, stored, update};
use crate::v2::document::{FileKey, ReviewItem};
use crate::v2::testing::fixture::GitFixture;
use crate::v2::testing::harness::Scenario;
use crate::v2::testing::local_backend::LocalBackend;
use crate::v2::tui::app::ReviewAppV2;
use crate::v2::tui::entry::Entry;
use bcode_code_review_models::{ReviewSourceKind, ReviewedRevision};
use std::collections::BTreeMap;
use std::sync::Arc;

/// A small repository: `main` with `a.txt` and `b.txt`, and `feature` changing both.
struct Repo {
    fixture: GitFixture,
}

impl Repo {
    fn new() -> Self {
        let repo = Self {
            fixture: GitFixture::empty(),
        };
        repo.git(&["config", "user.email", "test@example.com"]);
        repo.git(&["config", "user.name", "Test"]);
        repo.write("a.txt", &lines("a", &[]));
        repo.write("b.txt", &lines("b", &[]));
        repo.commit("base");
        repo.git(&["checkout", "-q", "-b", "feature"]);
        repo.write("a.txt", &lines("a", &[(15, "feature a")]));
        repo.write("b.txt", &lines("b", &[(5, "feature b")]));
        repo.commit("feature");
        repo
    }

    fn git(&self, args: &[&str]) -> String {
        self.fixture.git(args).trim().to_owned()
    }

    fn write(&self, path: &str, content: &str) {
        std::fs::write(self.fixture.root().join(path), content).expect("write");
    }

    fn commit(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", message]);
    }
}

/// Twenty lines named after `prefix`, with some replaced.
fn lines(prefix: &str, replaced: &[(usize, &str)]) -> String {
    (1..=20)
        .map(|line| {
            replaced.iter().find(|(at, _)| *at == line).map_or_else(
                || format!("{prefix} line {line}"),
                |(_, text)| (*text).to_owned(),
            )
        })
        .map(|line| line + "\n")
        .collect()
}

const fn fixture_root(repo: &Repo) -> &GitFixture {
    &repo.fixture
}

fn resume(repo: &Repo, backend: &Arc<LocalBackend>, id: &str) -> Scenario<ReviewAppV2> {
    let entry = Entry::Resume {
        workspace_id: Some(id.to_owned()),
    };
    let (scenario, _) = open(fixture_root(repo), backend, entry, 140, 40);
    scenario
}

fn compare() -> ReviewSourceKind {
    ReviewSourceKind::BranchCompare {
        base_branch: "main".to_owned(),
        head_branch: "feature".to_owned(),
        merge_base: true,
    }
}

fn doc_paths(scenario: &Scenario<ReviewAppV2>) -> Vec<String> {
    let doc = scenario.surface().review().unwrap().document().unwrap();
    doc.file_keys().map(|file| file.path.clone()).collect()
}

#[test]
fn fast_forward_offers_the_changes_since_the_marked_pass() {
    let repo = Repo::new();
    let fixture = fixture_root(&repo);
    let backend = Arc::new(LocalBackend::default());
    let workspace = create(&backend, fixture, "Feature", vec![compare()]);
    // Leaving without marking the pass records nothing.
    let mut scenario = resume(&repo, &backend, &workspace.id);
    scenario.key("q");
    settled(&mut scenario);
    assert!(
        stored(&backend, fixture, &workspace.id)
            .reviewed_revisions
            .is_empty()
    );
    // Mark the pass complete.
    let mut scenario = resume(&repo, &backend, &workspace.id);
    assert!(scenario.surface().review().unwrap().rereview().is_none());
    scenario.key("shift+m");
    scenario.key("q");
    settled(&mut scenario);
    let reviewed_head = repo.git(&["rev-parse", "feature"]);
    assert_eq!(
        stored(&backend, fixture, &workspace.id).reviewed_revisions["source-1"]
            .reviewed_head_oid
            .as_deref(),
        Some(reviewed_head.as_str())
    );

    // One new commit touching only b.txt.
    repo.write("b.txt", &lines("b", &[(5, "feature b"), (12, "more b")]));
    repo.commit("more");
    let new_head = repo.git(&["rev-parse", "feature"]);
    let mut scenario = resume(&repo, &backend, &workspace.id);
    let review = scenario.surface().review().unwrap();
    let plan = review.rereview().expect("re-review plan");
    assert_eq!(plan.new_commits(), 1);
    assert!(!plan.rebased());
    assert!(
        review
            .toasts()
            .contains(&"1 new commit since your last review"),
        "{:?}",
        review.toasts()
    );
    assert_eq!(doc_paths(&scenario), ["a.txt", "b.txt"]);

    scenario.key("shift+l");
    settled(&mut scenario);
    let review = scenario.surface().review().unwrap();
    assert!(review.showing_since_last());
    assert_eq!(
        review.sources()[0].kind,
        ReviewSourceKind::CommitRange {
            base: reviewed_head,
            head: new_head.clone(),
            merge_base: false,
        }
    );
    assert_eq!(doc_paths(&scenario), ["b.txt"]);
    let screen = scenario.frame();
    assert!(screen.contains("more b"), "{}", screen.text());
    // Back to the whole review; marking the pass complete moves the baseline.
    scenario.key("shift+l");
    settled(&mut scenario);
    assert!(!scenario.surface().review().unwrap().showing_since_last());
    assert_eq!(doc_paths(&scenario), ["a.txt", "b.txt"]);
    scenario.key("shift+m");
    scenario.key("q");
    settled(&mut scenario);
    assert_eq!(
        stored(&backend, fixture, &workspace.id).reviewed_revisions["source-1"]
            .reviewed_head_oid
            .as_deref(),
        Some(new_head.as_str())
    );
    let scenario = resume(&repo, &backend, &workspace.id);
    assert!(scenario.surface().review().unwrap().rereview().is_none());
}

#[test]
fn after_a_rebase_unchanged_patches_stay_viewed_and_are_de_emphasized() {
    let repo = Repo::new();
    let fixture = fixture_root(&repo);
    let backend = Arc::new(LocalBackend::default());
    let mut workspace = create(&backend, fixture, "Feature", vec![compare()]);
    workspace.reviewed_revisions = BTreeMap::from([(
        "source-1".to_owned(),
        ReviewedRevision {
            reviewed_base_oid: Some(repo.git(&["rev-parse", "main"])),
            reviewed_head_oid: Some(repo.git(&["rev-parse", "feature"])),
            recorded_at_ms: Some(1),
        },
    )]);
    update(&backend, &workspace);

    // `main` moves (changing a.txt far from the feature's line), the feature is rebased onto
    // it, and its b.txt change is rewritten.
    repo.git(&["checkout", "-q", "main"]);
    repo.write("a.txt", &lines("a", &[(1, "main a")]));
    repo.commit("main moves");
    repo.git(&["checkout", "-q", "feature"]);
    repo.git(&["rebase", "-q", "main"]);
    repo.write("b.txt", &lines("b", &[(5, "rewritten b")]));
    repo.git(&["commit", "-q", "-a", "--amend", "-m", "feature"]);

    let mut scenario = resume(&repo, &backend, &workspace.id);
    settled(&mut scenario);
    let review = scenario.surface().review().unwrap();
    let plan = review.rereview().expect("re-review plan");
    assert!(plan.rebased());
    assert!(
        review.toasts().contains(&"rebased since your last review"),
        "{:?}",
        review.toasts()
    );
    assert!(
        review
            .toasts()
            .contains(&"1 file unchanged since your last review (marked viewed)"),
        "{:?}",
        review.toasts()
    );
    let doc = review.document().unwrap();
    let a = FileKey::new("source-1", None, "a.txt");
    let b = FileKey::new("source-1", None, "b.txt");
    assert!(doc.carried_over(&a) && !doc.carried_over(&b));
    assert!(
        doc.viewed_state(&a)
            .is_some_and(crate::v2::viewed::ViewedState::is_viewed)
    );
    assert!(
        !doc.viewed_state(&b)
            .is_some_and(crate::v2::viewed::ViewedState::is_viewed)
    );
    let header = (0..doc.len()).find_map(|index| match doc.item(index)? {
        ReviewItem::FileHeader(header) if header.file.path == "a.txt" => Some(header.clone()),
        _ => None,
    });
    assert!(header.is_some_and(|header| header.carried_over && header.viewed));
    let screen = scenario.frame();
    assert!(
        screen.contains("unchanged since last review"),
        "{}",
        screen.text()
    );
    // A rebased source stays whole in the changes-since view.
    scenario.key("shift+l");
    settled(&mut scenario);
    assert_eq!(doc_paths(&scenario), ["a.txt", "b.txt"]);
    // The viewed mark is saved with the new identity.
    scenario.key("q");
    settled(&mut scenario);
    let saved = stored(&backend, fixture, &workspace.id);
    assert!(saved.viewed_files.contains("a.txt"));
}
