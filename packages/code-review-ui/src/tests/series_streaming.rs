//! A sparse pair of commits and an ordinary source all edit the same path. Use actual
//! listings and hunk answers, but control delivery order without sleeps or task scheduling.

use crate::loader::{Inbox, Message, OpenOptions, open};
use crate::testing::backend::TestBackend;
use crate::testing::fixture::{FixtureSpec, GitFixture, STACK_BRANCHES, STACK_SHARED_FILE};
use bcode_code_review::document::{DocumentConfig, DocumentNeed, ReviewDocument, SourceInput};
use bcode_code_review_models::{ReviewFileHunksResponse, ReviewSource, ReviewSourceKind};
use std::collections::BTreeSet;

fn sources(fixture: &GitFixture) -> ([String; 3], Vec<ReviewSource>) {
    let heads = STACK_BRANCHES.map(|branch| fixture.git(&["rev-parse", branch]).trim().to_owned());
    let source = |id: &str, kind| ReviewSource {
        id: id.to_owned(),
        label: id.to_owned(),
        kind,
        included: true,
    };
    let sources = vec![
        source(
            "picked",
            ReviewSourceKind::Commits {
                revs: vec![heads[0].clone(), heads[2].clone()],
            },
        ),
        source(
            "ordinary",
            ReviewSourceKind::Commit {
                rev: heads[1].clone(),
            },
        ),
    ];
    (heads, sources)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

#[test]
fn streaming_lists_each_member_even_with_another_source_ready_first() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let (heads, sources) = sources(&fixture);
    let inbox = Inbox::default();
    let answer = runtime()
        .block_on(open(
            TestBackend::default().services(),
            fixture.root().to_owned(),
            sources,
            inbox.clone(),
            OpenOptions {
                first: Some("ordinary".to_owned()),
                stream: true,
                ..OpenOptions::default()
            },
        ))
        .expect("streamed open");
    assert!(answer.is_none());

    let mut ready = BTreeSet::new();
    let mut note_ready = |input: &SourceInput| {
        if !input.loading {
            assert!(
                input
                    .entries
                    .iter()
                    .any(|entry| entry.path == STACK_SHARED_FILE)
            );
            ready.insert((
                input.source.source_id.clone(),
                input.source.part.as_ref().map(|part| part.index),
                input
                    .source
                    .head
                    .commit_oid()
                    .expect("commit head")
                    .to_owned(),
            ));
        }
    };
    let messages = inbox.drain();
    assert!(matches!(messages.last(), Some(Message::Filled { .. })));
    for message in messages {
        match message {
            Message::Opened(opened) => {
                assert!(opened.diagnostics.is_empty(), "{:?}", opened.diagnostics);
                assert!(
                    opened
                        .inputs
                        .iter()
                        .any(|input| { input.source.source_id == "ordinary" && !input.loading })
                );
                opened.inputs.iter().for_each(&mut note_ready);
            }
            Message::Arrived(arrival) => note_ready(&arrival.input),
            Message::SourceFailed(error) => panic!("unexpected source failure: {error:?}"),
            _ => {}
        }
    }
    assert_eq!(
        ready,
        BTreeSet::from([
            ("picked".to_owned(), Some(0), heads[0].clone()),
            ("picked".to_owned(), Some(1), heads[2].clone()),
            ("ordinary".to_owned(), None, heads[1].clone()),
        ])
    );
}

#[test]
fn reversed_part_listings_and_late_hunk_answers_keep_duplicate_paths_independent() {
    let fixture = GitFixture::generate(FixtureSpec::small());
    let backend = TestBackend::default();
    let (heads, sources) = sources(&fixture);
    let mut inputs = runtime()
        .block_on(open(
            backend.services(),
            fixture.root().to_owned(),
            sources,
            Inbox::default(),
            OpenOptions::default(),
        ))
        .expect("atomic open")
        .expect("complete listing")
        .inputs;
    assert_eq!(inputs.len(), 3);
    // Keep just the shared path so all three requests fit in one viewport/inflight batch.
    for input in &mut inputs {
        input
            .entries
            .retain(|entry| entry.path == STACK_SHARED_FILE);
        assert_eq!(input.entries.len(), 1);
    }
    let pending = inputs
        .iter()
        .cloned()
        .map(|mut input| {
            input.loading = true;
            input.entries.clear();
            input
        })
        .collect();
    let config = DocumentConfig::default();
    let mut doc = ReviewDocument::new(fixture.root().to_owned(), pending, config);
    // Slots are known before listings finish. Completing a later slot must not fill its sibling.
    for (index, input) in inputs.into_iter().enumerate().rev() {
        assert!(doc.upsert_group(input, index));
        assert_eq!(doc.groups().filter(|group| group.loading).count(), index);
    }
    let keys: Vec<_> = doc.file_keys().cloned().collect();
    assert_eq!(keys.len(), 3);
    for (file, oid) in keys.iter().zip([&heads[0], &heads[2], &heads[1]]) {
        assert_eq!(doc.head_oid(file).as_ref(), Some(oid));
    }

    let requests = |doc: &mut ReviewDocument| {
        doc.set_viewport(0, doc.len() - 1);
        doc.take_needs()
            .into_iter()
            .filter_map(|need| match need {
                DocumentNeed::Hunks {
                    ticket,
                    file,
                    request,
                } => {
                    let answer: ReviewFileHunksResponse = backend.call_now(request);
                    Some((ticket, file, answer))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let stale = requests(&mut doc);
    assert_eq!(stale.len(), 3);
    doc.set_ignore_whitespace(true);
    doc.set_ignore_whitespace(false);
    let current = requests(&mut doc);
    assert_eq!(current.len(), 3);
    for (ticket, _, answer) in stale {
        assert!(!doc.accept_hunks(ticket, &answer));
        assert!(!doc.fail(ticket, "late failure"));
    }
    let mut patches = BTreeSet::new();
    for (ticket, file, answer) in current.into_iter().rev() {
        assert!(doc.accept_hunks(ticket, &answer));
        let patch = answer.patch_id.expect("text diff fingerprint");
        assert_eq!(doc.patch_id(&file), Some(patch.as_str()));
        assert!(
            patches.insert(patch),
            "each commit edited a different region"
        );
    }
}
