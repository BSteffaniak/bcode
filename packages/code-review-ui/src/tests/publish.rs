//! Stacks, publishing, and importing in the app: the stack rail and
//! layer stepping with remembered positions, whole-stack publishing split per pull request
//! through a fake external publisher, layer and local publishing, and imported threads shown
//! read-only.

use super::app::{app, create, fixture, open, settled, stored};
use crate::app::ReviewApp;
use crate::entry::Entry;
use crate::notes::NoteScope;
use crate::publish::Step;
use crate::surface::Modal;
use crate::testing::backend::TestBackend;
use crate::testing::fake_provider::{FAKE_PROVIDER, FAKE_REPOSITORY, FakeProvider};
use crate::testing::fake_publisher::{FAKE_IMPORTER, FakePublisher};
use crate::testing::fixture::{FixtureSpec, GitFixture, HEAD_BRANCH, STACK_BRANCHES};
use crate::testing::harness::Scenario;
use bcode_code_review::document::{ItemKey, ReviewItem};
use bcode_code_review_models::{
    DraftAnchor, ExternalAnchorMappingStatus, ExternalReviewAnchorMapping, ExternalReviewComment,
    ExternalReviewIdentity, ExternalReviewThread, ExternalReviewThreadState,
    ListExternalReviewImportsRequest, ListExternalReviewImportsResponse, ListPublishRecordsRequest,
    ListPublishRecordsResponse, ReviewAnchorKind, ReviewLineKind, ReviewSourceKind,
    ReviewWorkspace, SaveDraftRequest, SaveDraftResponse,
};
use std::sync::Arc;

fn layer(number: u64) -> ReviewSourceKind {
    ReviewSourceKind::StackLayer {
        provider: FAKE_PROVIDER.to_owned(),
        repository: FAKE_REPOSITORY.to_owned(),
        number,
    }
}

fn line_anchor(source: &str, path: &str, line: u32, kind: ReviewLineKind) -> DraftAnchor {
    DraftAnchor {
        kind: ReviewAnchorKind::Range,
        file_path: path.to_owned(),
        diff_row: u64::from(line),
        start_diff_row: None,
        end_diff_row: None,
        old_start: None,
        old_end: None,
        new_start: Some(line),
        new_end: Some(line),
        old_line: None,
        new_line: Some(line),
        line_kind: kind,
        is_file_anchor: false,
        surface_id: None,
        source_id: Some(source.to_owned()),
        head_oid: None,
        blob_oid: None,
    }
}

fn scoped_anchor(kind: ReviewAnchorKind, path: &str, source: Option<&str>) -> DraftAnchor {
    DraftAnchor {
        kind,
        line_kind: ReviewLineKind::Context,
        new_start: None,
        new_end: None,
        new_line: None,
        diff_row: 0,
        source_id: source.map(str::to_owned),
        ..line_anchor("x", path, 1, ReviewLineKind::Context)
    }
}

fn save_draft(backend: &TestBackend, workspace: &ReviewWorkspace, anchor: DraftAnchor, body: &str) {
    let scope = NoteScope::for_workspace(workspace);
    let _: SaveDraftResponse = backend.call_now(SaveDraftRequest {
        repo_path: scope.repo_path,
        target: scope.target,
        scope: scope.scope,
        anchor,
        body: body.to_owned(),
    });
}

fn resume(fixture: &GitFixture, backend: &Arc<TestBackend>, id: &str) -> Scenario<ReviewApp> {
    let entry = Entry::Resume {
        workspace_id: Some(id.to_owned()),
    };
    let (scenario, _) = open(fixture, backend, entry, 160, 48);
    scenario
}

fn cursor(scenario: &Scenario<ReviewApp>) -> Option<ItemKey> {
    scenario.surface().review()?.cursor().cloned()
}

fn publish_step(scenario: &Scenario<ReviewApp>) -> Option<Step> {
    match scenario.surface().review()?.modal()? {
        Modal::Publish(flow) => Some(flow.step().clone()),
        Modal::Import(_) => None,
    }
}

#[test]
fn stack_rail_steps_between_layers_keeping_each_layers_position() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default().with_provider(FakeProvider::new(fixture.root())));
    let workspace = create(
        &backend,
        &fixture,
        "Step through",
        vec![layer(101), layer(102), layer(103)],
    );
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    let review = scenario.surface().review().expect("review");
    let (labels, current) = review.rail();
    assert_eq!(labels.len(), 3, "{labels:?}");
    assert!(labels[0].starts_with("#101 Layer 101 2/4"), "{labels:?}");
    assert!(labels[2].starts_with("#103 Layer 103 4/4"), "{labels:?}");
    assert_eq!(current, Some(0));
    let screen = scenario.frame();
    assert!(
        screen.row_text(1).contains("#101 Layer 101"),
        "{}",
        screen.text()
    );

    // Next layer lands on its header; moving inside it is remembered when stepping away.
    scenario.key(".");
    settled(&mut scenario);
    assert_eq!(scenario.surface().review().unwrap().rail().1, Some(1));
    assert!(matches!(
        cursor(&scenario),
        Some(ItemKey::SourceHeader { ref source_id, .. }) if source_id == "source-2"
    ));
    scenario.keys(&["j", "j", "j"]);
    settled(&mut scenario);
    let inside = cursor(&scenario).expect("cursor");
    assert_eq!(
        inside.file().map(|file| file.source_id.as_str()),
        Some("source-2")
    );
    scenario.key(".");
    settled(&mut scenario);
    assert_eq!(scenario.surface().review().unwrap().rail().1, Some(2));
    scenario.key(",");
    settled(&mut scenario);
    assert_eq!(cursor(&scenario), Some(inside.clone()));
    // Stepping wraps; a click on a tab enters that layer.
    scenario.key(",");
    scenario.key(",");
    settled(&mut scenario);
    assert_eq!(scenario.surface().review().unwrap().rail().1, Some(2));
    let screen = scenario.frame();
    let x = screen.column_of(1, "#101").expect("first tab");
    scenario.click(x, 1);
    settled(&mut scenario);
    assert_eq!(scenario.surface().review().unwrap().rail().1, Some(0));

    // Narrow screens keep every layer by shortening the labels.
    scenario.resize(44, 30);
    let screen = settled(&mut scenario);
    let rail = screen.row_text(1);
    assert!(rail.contains("#101") && rail.contains("#103"), "{rail}");
    assert!(!rail.contains("Layer 101"), "{rail}");

    // Positions persist with the workspace.
    scenario.key("q");
    settled(&mut scenario);
    let saved = stored(&backend, &fixture, &workspace.id);
    let positions = saved.presentation.expect("presentation").layer_positions;
    assert_eq!(positions.get("source-2"), inside.to_persisted().as_ref());
}

#[test]
fn step_through_reviews_show_one_pull_request_at_a_time_or_every_one() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default().with_provider(FakeProvider::new(fixture.root())));
    let workspace = create(
        &backend,
        &fixture,
        "Step through",
        vec![layer(101), layer(102), layer(103)],
    );
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    let focus = |scenario: &Scenario<ReviewApp>| {
        scenario
            .surface()
            .review()
            .and_then(|review| review.document())
            .and_then(|doc| doc.focus().map(str::to_owned))
    };
    let sources_shown = |scenario: &Scenario<ReviewApp>| {
        let review = scenario.surface().review().expect("review");
        let doc = review.document().expect("document");
        (0..doc.len())
            .filter_map(|index| match doc.item(index)? {
                ReviewItem::SourceHeader(header) => Some(header.source_id.clone()),
                item => item.file().map(|file| file.source_id.clone()),
            })
            .collect::<std::collections::BTreeSet<String>>()
    };
    // By default one pull request at a time: its banner (its position in the whole stack, which
    // starts at #100), its branches, and only its files.
    assert_eq!(focus(&scenario).as_deref(), Some("source-1"));
    assert_eq!(sources_shown(&scenario), ["source-1".to_owned()].into());
    let screen = settled(&mut scenario);
    assert!(screen.contains("PR 2 of 4"), "{}", screen.text());
    assert!(
        screen.contains(&format!("{HEAD_BRANCH} ← {}", STACK_BRANCHES[0])),
        "{}",
        screen.text()
    );
    // The next pull request replaces it.
    scenario.key(".");
    settled(&mut scenario);
    assert_eq!(focus(&scenario).as_deref(), Some("source-2"));
    assert_eq!(sources_shown(&scenario), ["source-2".to_owned()].into());

    // shift+o shows every pull request, each under its banner, with a strip naming the current
    // one under the rail.
    scenario.key("shift+o");
    let screen = settled(&mut scenario);
    assert_eq!(focus(&scenario), None);
    assert_eq!(sources_shown(&scenario).len(), 3);
    assert!(
        screen.row_text(2).contains("PR 3 of 4"),
        "{}",
        screen.text()
    );
    assert!(screen.row_text(2).contains("#102"), "{}", screen.text());
    // The file panel groups files by pull request, only the current one open.
    assert!(screen.contains("▸ #101 Layer 101"), "{}", screen.text());
    assert!(screen.contains("▾ #102 Layer 102"), "{}", screen.text());
    // And back.
    scenario.key("shift+o");
    settled(&mut scenario);
    assert_eq!(focus(&scenario).as_deref(), Some("source-2"));
    scenario.key("shift+o");
    settled(&mut scenario);

    // The choice persists with the review, and reopening keeps it.
    scenario.key("q");
    settled(&mut scenario);
    let saved = stored(&backend, &fixture, &workspace.id);
    assert!(saved.presentation.expect("presentation").all_stack_layers);
    let scenario = resume(&fixture, &backend, &workspace.id);
    assert_eq!(focus(&scenario), None);
}

#[test]
fn finishing_a_pull_requests_files_points_to_the_next_one() {
    let fixture = fixture();
    let backend = Arc::new(TestBackend::default().with_provider(FakeProvider::new(fixture.root())));
    let workspace = create(
        &backend,
        &fixture,
        "Step through",
        vec![layer(101), layer(102)],
    );
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    scenario.key("j");
    settled(&mut scenario);
    let mut status = String::new();
    for _ in 0..30 {
        scenario.key("v");
        settled(&mut scenario);
        status = scenario
            .surface()
            .review()
            .and_then(|review| review.status().map(str::to_owned))
            .unwrap_or_default();
        if status.contains("every file of this pull request is viewed") {
            break;
        }
    }
    assert!(status.contains(". for the next"), "{status}");
}

/// A whole-stack review of the fake stack with comments on lines of layers #102 and #103, a
/// file comment, and a review comment.
fn whole_stack_review() -> (GitFixture, Arc<TestBackend>, ReviewWorkspace) {
    let fixture = fixture();
    let backend = Arc::new(
        TestBackend::default()
            .with_provider(FakeProvider::new(fixture.root()))
            .with_publisher(FakePublisher::default()),
    );
    let workspace = create(
        &backend,
        &fixture,
        "Whole stack",
        vec![ReviewSourceKind::Stack {
            provider: FAKE_PROVIDER.to_owned(),
            repository: FAKE_REPOSITORY.to_owned(),
            top: 103,
        }],
    );
    let drafts = [
        (
            line_anchor(
                "source-1",
                &GitFixture::stack_layer_path(2),
                3,
                ReviewLineKind::Added,
            ),
            "layer two",
        ),
        (
            line_anchor(
                "source-1",
                &GitFixture::stack_layer_path(3),
                5,
                ReviewLineKind::Added,
            ),
            "layer three",
        ),
        (
            scoped_anchor(
                ReviewAnchorKind::File,
                &GitFixture::modified_path(0),
                Some("source-1"),
            ),
            "about this file",
        ),
        (
            scoped_anchor(
                ReviewAnchorKind::Review,
                bcode_code_review::document::REVIEW_LEVEL_PATH,
                None,
            ),
            "overall",
        ),
    ];
    for (anchor, body) in drafts {
        save_draft(&backend, &workspace, anchor, body);
    }
    (fixture, backend, workspace)
}

#[test]
fn whole_stack_publishes_one_review_per_layer_with_summary_fallback() {
    let (fixture, backend, workspace) = whole_stack_review();
    let layer_2 = GitFixture::stack_layer_path(2);
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    // Whole-stack files and hunks carry the layer that introduced them.
    let screen = settled(&mut scenario);
    assert!(
        screen.contains("Whole stack #100–#103"),
        "{}",
        screen.text()
    );

    scenario.key("shift+p");
    assert_eq!(publish_step(&scenario), Some(Step::Checklist));
    let Some(Modal::Publish(flow)) = scenario.surface().review().unwrap().modal() else {
        panic!("publish flow");
    };
    let labels = flow.menu_labels();
    assert!(
        labels
            .iter()
            .any(|label| label.contains("[x]") && label.contains("layer two")),
        "{labels:?}"
    );
    scenario.key("enter");
    settled(&mut scenario);
    assert_eq!(publish_step(&scenario), Some(Step::Publishers));
    // The publisher of the review's provider is preselected.
    scenario.key("enter");
    assert_eq!(publish_step(&scenario), Some(Step::Options));
    let screen = scenario.frame();
    assert!(
        screen.contains("octo/fixture#100, octo/fixture#101"),
        "{}",
        screen.text()
    );
    scenario.key("enter");
    settled(&mut scenario);
    assert_eq!(publish_step(&scenario), Some(Step::Preview));
    let Some(Modal::Publish(flow)) = scenario.surface().review().unwrap().modal() else {
        panic!("publish flow");
    };
    let bundles: Vec<(&str, usize)> = flow
        .prepared()
        .iter()
        .map(|bundle| (bundle.label.as_str(), bundle.bundle.threads.len()))
        .collect();
    assert_eq!(bundles, [("#102 Layer 102", 1), ("#103 Layer 103", 3)]);
    let screen = scenario.frame();
    assert!(
        screen.contains("Fake review of octo/fixture#102"),
        "{}",
        screen.text()
    );
    scenario.key("enter");
    assert_eq!(publish_step(&scenario), Some(Step::Confirm));
    scenario.key("enter");
    settled(&mut scenario);
    assert!(scenario.surface().review().unwrap().modal().is_none());

    let submitted = backend.publisher().unwrap().submitted();
    let targets: Vec<u64> = submitted
        .iter()
        .map(|bundle| bundle.pull_request.as_ref().unwrap().number)
        .collect();
    assert_eq!(targets, [102, 103]);
    // The layer comment reads in the layer's own diff, with its selected line.
    let two = &submitted[0].threads[0];
    assert_eq!(
        (two.anchor.file_path.as_str(), two.anchor.new_line),
        (layer_2.as_str(), Some(3))
    );
    assert!(!two.summary_only);
    assert_eq!(two.selected_lines.len(), 1);
    assert_eq!(two.selected_lines[0].kind, ReviewLineKind::Added);
    let summaries: Vec<(&str, bool)> = submitted[1]
        .threads
        .iter()
        .map(|thread| (thread.comments[0].body.as_str(), thread.summary_only))
        .collect();
    assert_eq!(
        summaries,
        [
            ("layer three", false),
            ("about this file", true),
            ("overall", true)
        ]
    );
    assert_recorded(&mut scenario, &fixture, &backend, workspace.id);
}

#[test]
fn a_failed_pull_request_returns_to_the_preview_and_retry_skips_published_ones() {
    let fixture = fixture();
    let backend = Arc::new(
        TestBackend::default()
            .with_provider(FakeProvider::new(fixture.root()))
            .with_publisher(FakePublisher::default().failing_submit_of(102)),
    );
    let workspace = create(&backend, &fixture, "Layers", vec![layer(101), layer(102)]);
    save_draft(
        &backend,
        &workspace,
        line_anchor(
            "source-1",
            &GitFixture::stack_layer_path(1),
            2,
            ReviewLineKind::Added,
        ),
        "one",
    );
    save_draft(
        &backend,
        &workspace,
        line_anchor(
            "source-2",
            &GitFixture::stack_layer_path(2),
            2,
            ReviewLineKind::Added,
        ),
        "two",
    );
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    scenario.key("shift+p");
    scenario.key("enter");
    settled(&mut scenario);
    scenario.keys(&["enter", "enter"]);
    settled(&mut scenario);
    scenario.keys(&["enter", "enter"]);
    settled(&mut scenario);
    // #101 published, #102 failed: back at the preview with the error.
    assert_eq!(publish_step(&scenario), Some(Step::Preview));
    let Some(Modal::Publish(flow)) = scenario.surface().review().unwrap().modal() else {
        panic!("publish flow");
    };
    assert!(flow.error().is_some_and(|error| error.contains("#102")));
    assert_eq!(backend.publisher().unwrap().submitted().len(), 1);
    // Retrying submits only #102 (it fails again; #101 is not published twice).
    scenario.keys(&["enter", "enter"]);
    settled(&mut scenario);
    assert_eq!(backend.publisher().unwrap().submitted().len(), 1);
    assert!(
        scenario
            .surface()
            .review()
            .unwrap()
            .toasts()
            .contains(&"Publishing failed")
    );
}

#[test]
fn local_reviews_publish_one_bundle_with_built_in_publishers() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let backend = Arc::new(TestBackend::default());
    let workspace = create(
        &backend,
        &fixture,
        "Local",
        vec![ReviewSourceKind::LastCommit],
    );
    save_draft(
        &backend,
        &workspace,
        scoped_anchor(
            ReviewAnchorKind::Review,
            bcode_code_review::document::REVIEW_LEVEL_PATH,
            None,
        ),
        "looks good",
    );
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    scenario.key("shift+p");
    scenario.key("enter");
    settled(&mut scenario);
    let Some(Modal::Publish(flow)) = scenario.surface().review().unwrap().modal() else {
        panic!("publish flow");
    };
    // Built-in publishers only; markdown_file is preselected.
    let labels = flow.menu_labels();
    assert!(
        labels
            .iter()
            .any(|label| label.starts_with("Markdown file")),
        "{labels:?}"
    );
    scenario.key("enter");
    let output = fixture.root().join("review-export.md");
    // Options: the output path field is typed (any letter, `j`/`k` included).
    scenario.paste(&output.display().to_string());
    scenario.key("enter");
    settled(&mut scenario);
    let screen = scenario.frame();
    assert!(screen.contains("looks good"), "{}", screen.text());
    scenario.keys(&["enter", "enter"]);
    settled(&mut scenario);
    let written = std::fs::read_to_string(&output).expect("export written");
    assert!(written.contains("looks good"));
    assert!(scenario.surface().review().unwrap().modal().is_none());
}

/// Two imported threads: one mapped exactly onto line 2 of `added`, one not mappable.
fn imported_threads(added: &str) -> Vec<ExternalReviewThread> {
    let identity = |id: &str| ExternalReviewIdentity {
        provider_id: "fake".to_owned(),
        external_id: id.to_owned(),
        url: None,
    };
    let comment = |id: &str, author: &str, body: &str| ExternalReviewComment {
        identity: identity(id),
        author: None,
        author_label: Some(author.to_owned()),
        body: body.to_owned(),
        created_at_ms: None,
        updated_at_ms: None,
    };
    vec![
        ExternalReviewThread {
            identity: identity("t1"),
            state: ExternalReviewThreadState::Open,
            mapping: ExternalReviewAnchorMapping {
                status: ExternalAnchorMappingStatus::Exact,
                anchor: Some(line_anchor("source-1", added, 2, ReviewLineKind::Added)),
                message: None,
            },
            comments: vec![
                comment("c1", "octocat", "Why this line?"),
                comment("c2", "hubot", "Because."),
            ],
        },
        ExternalReviewThread {
            identity: identity("t2"),
            state: ExternalReviewThreadState::Outdated,
            mapping: ExternalReviewAnchorMapping {
                status: ExternalAnchorMappingStatus::Unmappable,
                anchor: None,
                message: Some("file is not present in the current review".to_owned()),
            },
            comments: vec![comment("c3", "octocat", "Old remark")],
        },
    ]
}

#[test]
fn imported_threads_show_read_only_inline_and_in_the_threads_panel() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let added = GitFixture::added_path(0);
    let threads = imported_threads(&added);
    let backend = Arc::new(
        TestBackend::default().with_publisher(FakePublisher::default().with_threads(threads)),
    );
    let workspace = create(
        &backend,
        &fixture,
        "Imported",
        vec![ReviewSourceKind::BranchCompare {
            base_branch: crate::testing::fixture::BASE_BRANCH.to_owned(),
            head_branch: crate::testing::fixture::HEAD_BRANCH.to_owned(),
            merge_base: true,
        }],
    );
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    scenario.key("shift+i");
    settled(&mut scenario);
    scenario.key("enter");
    scenario.key("enter");
    settled(&mut scenario);
    let review = scenario.surface().review().unwrap();
    assert!(review.modal().is_none());
    assert!(
        review
            .toasts()
            .contains(&"Imported 2 external review threads")
    );
    assert_eq!(review.notes().external_count(), 2);
    let request = &backend.publisher().unwrap().imports()[0];
    assert!(
        request
            .files
            .iter()
            .any(|file| file.new_path.as_deref() == Some(added.as_str()))
    );
    // The unmappable thread is review-level; the mapped one sits under its line.
    let doc = review.document().unwrap();
    let threads: Vec<&str> = (0..doc.len())
        .filter_map(|index| match doc.item(index)? {
            ReviewItem::Thread(thread) => Some(thread.thread_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(threads.len(), 2, "{threads:?}");
    assert!(threads.iter().all(|id| id.starts_with("external:")));
    // Read-only: editing an imported thread is refused.
    scenario.key(")");
    settled(&mut scenario);
    assert!(matches!(cursor(&scenario), Some(ItemKey::Thread { .. })));
    scenario.key("i");
    assert!(
        scenario
            .surface()
            .review()
            .unwrap()
            .status()
            .is_some_and(|status| status.contains("read-only"))
    );
    let screen = scenario.frame();
    assert!(screen.contains("imported"), "{}", screen.text());
    assert!(screen.contains("not in this review"), "{}", screen.text());
    // The threads panel lists them.
    scenario.key("shift+t");
    let screen = settled(&mut scenario);
    assert!(screen.contains("imported fake"), "{}", screen.text());
    // The snapshot is saved and shows again on reopen.
    let imports: ListExternalReviewImportsResponse =
        backend.call_now(ListExternalReviewImportsRequest {
            repo_path: fixture.root().to_path_buf(),
            workspace_id: workspace.id.clone(),
        });
    assert_eq!(imports.records.len(), 1);
    assert_eq!(imports.records[0].importer_id, FAKE_IMPORTER);
    let mut scenario = Scenario::new(
        app(
            &fixture,
            &backend,
            Entry::Resume {
                workspace_id: Some(workspace.id),
            },
        ),
        160,
        48,
    );
    settled(&mut scenario);
    assert_eq!(
        scenario
            .surface()
            .review()
            .unwrap()
            .notes()
            .external_count(),
        2
    );
}

/// One publish record per pull request; publishing completes the pass and marks the threads.
fn assert_recorded(
    scenario: &mut Scenario<ReviewApp>,
    fixture: &GitFixture,
    backend: &TestBackend,
    workspace_id: String,
) {
    // One record per pull request; publishing completes the pass.
    let records: ListPublishRecordsResponse = backend.call_now(ListPublishRecordsRequest {
        repo_path: fixture.root().to_path_buf(),
        workspace_id,
    });
    assert_eq!(records.records.len(), 2);
    assert_eq!(records.records[1].published_thread_anchors.len(), 3);
    let review = scenario.surface().review().unwrap();
    assert!(review.toasts().contains(&"Review published"));
    assert!(
        review
            .workspace()
            .unwrap()
            .reviewed_revisions
            .contains_key("source-1")
    );
    let screen = settled(scenario);
    assert!(screen.contains("published"), "{}", screen.text());
}

#[test]
fn excluded_threads_stay_out_and_option_fields_take_every_letter() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let backend = Arc::new(TestBackend::default());
    let workspace = create(
        &backend,
        &fixture,
        "Local",
        vec![ReviewSourceKind::LastCommit],
    );
    // Two threads: one about the review, one about a file.
    save_draft(
        &backend,
        &workspace,
        scoped_anchor(
            ReviewAnchorKind::Review,
            bcode_code_review::document::REVIEW_LEVEL_PATH,
            None,
        ),
        "keep this",
    );
    save_draft(
        &backend,
        &workspace,
        scoped_anchor(ReviewAnchorKind::File, &GitFixture::modified_path(0), None),
        "drop this",
    );
    let mut scenario = resume(&fixture, &backend, &workspace.id);
    scenario.key("shift+p");
    settled(&mut scenario);
    assert_eq!(publish_step(&scenario), Some(Step::Checklist));
    let toggled = |scenario: &Scenario<ReviewApp>| {
        let Some(Modal::Publish(flow)) = scenario.surface().review().unwrap().modal() else {
            panic!("publish flow");
        };
        flow.menu_labels()
            .into_iter()
            .filter(|label| label.starts_with("[ ]"))
            .collect::<Vec<_>>()
    };
    // `space` toggles the focused thread; move down until "drop this" is excluded.
    for _ in 0..4 {
        scenario.key("space");
        if toggled(&scenario)
            .iter()
            .any(|label| label.contains("drop this"))
        {
            break;
        }
        scenario.key("space");
        scenario.key("down");
    }
    let excluded = toggled(&scenario);
    assert_eq!(excluded.len(), 1, "{excluded:?}");
    assert!(excluded[0].contains("drop this"), "{excluded:?}");
    scenario.key("enter");
    settled(&mut scenario);
    assert_eq!(publish_step(&scenario), Some(Step::Publishers));
    scenario.key("enter");
    assert_eq!(publish_step(&scenario), Some(Step::Options));
    // Letters that move selections elsewhere (`j`, `k`, `q`) type into the path field.
    scenario.paste(&format!("{}/", fixture.root().display()));
    scenario.keys(&["j", "k", "q", ".", "m", "d"]);
    scenario.key("enter");
    settled(&mut scenario);
    assert_eq!(publish_step(&scenario), Some(Step::Preview));
    scenario.keys(&["enter", "enter"]);
    settled(&mut scenario);
    let written = std::fs::read_to_string(fixture.root().join("jkq.md")).expect("export written");
    assert!(written.contains("keep this"), "{written}");
    assert!(!written.contains("drop this"), "{written}");
}
