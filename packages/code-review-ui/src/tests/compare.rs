//! Comparisons in the app: a stack review reopened after a push compares each layer with what
//! the reviewer last looked at, in place, and switches between the full change and only what
//! changed; looks are recorded as the reviewer works.

use super::app::{create, fixture, settled};
use super::publish::{layer, push_to, resume};
use crate::app::ReviewApp;
use crate::testing::backend::TestBackend;
use crate::testing::fake_provider::{FAKE_PROVIDER, FAKE_REPOSITORY, FakeProvider};
use crate::testing::fixture::{GitFixture, STACK_BRANCHES};
use crate::testing::harness::Scenario;
use bcode_code_review::document::{CompareView, FileKey, ReviewDocument};
use bcode_code_review_models::{
    DeltaKind, FileSince, LayerLook, ListLayerLooksRequest, ListLayerLooksResponse, LookKind,
};
use std::sync::Arc;

fn document(scenario: &Scenario<ReviewApp>) -> &ReviewDocument {
    scenario
        .surface()
        .review()
        .and_then(|review| review.document())
        .expect("a document")
}

/// The source id of the group reviewing layer `number`.
fn source_of(doc: &ReviewDocument, number: u64) -> String {
    doc.groups()
        .find(|group| {
            group
                .stack
                .and_then(|stack| stack.stack.layers.last())
                .is_some_and(|layer| layer.number == number)
        })
        .map(|group| group.source.source_id.clone())
        .expect("a layer source")
}

fn kind_of(doc: &ReviewDocument, number: u64) -> Option<DeltaKind> {
    doc.comparison(&source_of(doc, number))
        .and_then(|comparison| comparison.delta.as_ref())
        .map(|delta| delta.kind)
}

fn looks(backend: &TestBackend, fixture: &GitFixture) -> Vec<LayerLook> {
    let listed: ListLayerLooksResponse = backend.call_now(ListLayerLooksRequest {
        repo_path: fixture.root().to_path_buf(),
        provider: FAKE_PROVIDER.to_owned(),
        repository: FAKE_REPOSITORY.to_owned(),
        numbers: vec![101, 102, 103],
    });
    listed.looks
}

#[test]
fn a_reopened_stack_compares_each_layer_with_what_was_looked_at_in_place() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default().with_provider(FakeProvider::new(fixture.root())));
    let workspace = create(
        &backend,
        &fixture,
        "Compare",
        vec![layer(101), layer(102), layer(103)],
    );
    // First visit: nothing looked at yet, so nothing is compared; completing the pass records
    // a look at every layer.
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    assert!(!scenario.surface().review().unwrap().comparing().active());
    scenario.key("shift+m");
    scenario.key("q");
    settled(&mut scenario);
    let passes = looks(&backend, &fixture);
    assert_eq!(
        passes
            .iter()
            .filter(|look| look.kind == LookKind::Pass)
            .count(),
        3,
        "{passes:?}"
    );

    // The top layer is pushed again; the others stay.
    push_to(&fixture, STACK_BRANCHES[2], "pushed-later.txt");
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    let doc = document(&scenario);
    assert_eq!(kind_of(doc, 101), Some(DeltaKind::Unchanged));
    assert_eq!(kind_of(doc, 102), Some(DeltaKind::Unchanged));
    assert_eq!(kind_of(doc, 103), Some(DeltaKind::Changed));
    let top = source_of(doc, 103);
    let pushed = FileKey::new(top.clone(), None, "pushed-later.txt");
    assert!(matches!(
        doc.file_since(&pushed),
        Some(FileSince::New { .. })
    ));
    let progress = doc.viewed_progress();
    let files: Vec<_> = doc.file_keys().cloned().collect();

    // shift+l shows only what changed, in the same document, and back.
    scenario.key("shift+l");
    settled(&mut scenario);
    let doc = document(&scenario);
    assert_eq!(
        doc.comparison(&top).map(|comparison| comparison.view),
        Some(CompareView::Changes)
    );
    assert!(doc.file_shows_changes(&pushed));
    assert_eq!(doc.file_keys().cloned().collect::<Vec<_>>(), files);
    assert_eq!(doc.viewed_progress(), progress);
    scenario.key("shift+l");
    settled(&mut scenario);
    let doc = document(&scenario);
    assert!(!doc.file_shows_changes(&pushed));
    assert_eq!(doc.viewed_progress(), progress);
}

#[test]
fn marking_a_file_viewed_records_a_look_and_a_reopen_shows_nothing_new() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default().with_provider(FakeProvider::new(fixture.root())));
    let workspace = create(&backend, &fixture, "Looks", vec![layer(101)]);
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    scenario.key("n");
    scenario.key("v");
    scenario.key("q");
    settled(&mut scenario);
    let recorded = looks(&backend, &fixture);
    assert!(
        recorded
            .iter()
            .any(|look| look.number == 101 && look.kind == LookKind::Viewed),
        "{recorded:?}"
    );
    // The layer did not move since: reopened, it is unchanged since the reviewer looked.
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    settled(&mut scenario);
    assert_eq!(
        kind_of(document(&scenario), 101),
        Some(DeltaKind::Unchanged)
    );
}
