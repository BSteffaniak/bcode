use std::collections::BTreeSet;

use bcode_feedback_models::{
    Actor, ClaimIntakeSessionRequest, Facet, FeedbackId, FeedbackQuery, FeedbackStatus,
    IntakeTurnEnding, LeaseDueTriageRequest, LeaseTriageRequest, LinkFeedbackRequest, LinkKind,
    MAX_TRIAGE_GENERATIONS, RecordFeedbackRequest, RecordForTriageRequest,
    SetFeedbackStatusRequest, StartedTriageQuery, Triage, TriageDispatch, TriageDispatchState,
    TriageFailedRequest, TriageFeedbackRequest, TriageIntent, TriageStartedRequest, TriageState,
    TriageTurnEndedRequest,
};

use crate::dispatch::retry_delay_ms;
use crate::{FeedbackStore, FeedbackStoreError};

fn store() -> (tempfile::TempDir, FeedbackStore) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = FeedbackStore::open_in_state_dir(dir.path()).expect("open store");
    (dir, store)
}

fn record(store: &FeedbackStore, app: &str, text: &str, facets: Vec<Facet>) -> FeedbackId {
    store
        .record(
            &RecordFeedbackRequest {
                app: app.into(),
                text: text.into(),
                facets,
            },
            1_000,
        )
        .expect("record")
        .id
}

fn status(id: FeedbackId, status: FeedbackStatus, actor: Actor) -> SetFeedbackStatusRequest {
    SetFeedbackStatusRequest {
        id,
        status,
        actor,
        note: None,
        session_id: None,
        commits: Vec::new(),
    }
}

#[test]
fn recorded_items_read_back_with_facets_and_survive_reopening() {
    let dir = tempfile::tempdir().unwrap();
    let id = {
        let store = FeedbackStore::open_in_state_dir(dir.path()).unwrap();
        record(
            &store,
            "bdata",
            "copy failed with an error",
            vec![
                Facet::new("bdata.dialect", "postgres"),
                Facet {
                    key: "bdata.connection".into(),
                    value: "postgres://u:p@h".into(),
                    sensitive: true,
                },
            ],
        )
    };
    let store = FeedbackStore::open_in_state_dir(dir.path()).unwrap();
    let item = store.get(id).unwrap();
    assert_eq!(item.app, "bdata");
    assert_eq!(item.text, "copy failed with an error");
    assert_eq!(item.status, FeedbackStatus::Open);
    assert_eq!(item.triage_state, TriageState::Pending);
    assert_eq!(item.facets.len(), 2);
    assert!(
        item.facets
            .iter()
            .any(|facet| facet.key == "bdata.connection" && facet.sensitive)
    );
}

#[test]
fn ids_are_distinct_and_increasing() {
    let (_dir, store) = store();
    let first = record(&store, "bmux", "a", Vec::new());
    let second = record(&store, "bmux", "b", Vec::new());
    assert!(second > first);
    assert!(matches!(
        store.get(FeedbackId(999)),
        Err(FeedbackStoreError::NotFound(_))
    ));
}

#[test]
fn listing_filters_by_app_status_facet_tag_and_text_newest_first() {
    let (_dir, store) = store();
    let a = record(
        &store,
        "bdata",
        "Query editor loses cursor",
        vec![Facet::new("bdata.connection", "prod")],
    );
    let b = record(
        &store,
        "bdata",
        "Slow schema browser",
        vec![Facet::new("bdata.connection", "dev")],
    );
    let c = record(&store, "bmux", "Pane split flickers", Vec::new());
    store
        .triage(
            &TriageFeedbackRequest {
                id: b,
                triage: Triage {
                    title: "Schema browser is slow on large catalogs".into(),
                    category: Some("performance".into()),
                    tags: BTreeSet::from(["schema-browser".to_owned()]),
                    ..Triage::default()
                },
            },
            2_000,
        )
        .unwrap();

    let ids = |query: FeedbackQuery| -> Vec<FeedbackId> {
        store
            .list(&query)
            .unwrap()
            .items
            .into_iter()
            .map(|item| item.id)
            .collect()
    };
    assert_eq!(ids(FeedbackQuery::default()), vec![c, b, a]);
    assert_eq!(
        ids(FeedbackQuery {
            app: Some("bdata".into()),
            ..FeedbackQuery::default()
        }),
        vec![b, a]
    );
    assert_eq!(
        ids(FeedbackQuery {
            facets: vec![("bdata.connection".into(), "prod".into())],
            ..FeedbackQuery::default()
        }),
        vec![a]
    );
    assert_eq!(
        ids(FeedbackQuery {
            tags: BTreeSet::from(["schema-browser".to_owned()]),
            ..FeedbackQuery::default()
        }),
        vec![b]
    );
    assert_eq!(
        ids(FeedbackQuery {
            text: Some("LARGE CATALOG".into()),
            ..FeedbackQuery::default()
        }),
        vec![b],
        "text search covers the triage title, case-insensitively"
    );
    assert_eq!(
        ids(FeedbackQuery {
            triage_state: Some(TriageState::Pending),
            ..FeedbackQuery::default()
        }),
        vec![c, a]
    );
    assert_eq!(
        ids(FeedbackQuery {
            statuses: BTreeSet::from([FeedbackStatus::Verified]),
            ..FeedbackQuery::default()
        }),
        Vec::<FeedbackId>::new()
    );
}

#[test]
fn pages_continue_without_repeating_or_skipping() {
    let (_dir, store) = store();
    let ids: Vec<FeedbackId> = (0..7)
        .map(|n| record(&store, "bmux", &format!("item {n}"), Vec::new()))
        .collect();
    let mut seen = Vec::new();
    let mut query = FeedbackQuery {
        limit: 3,
        ..FeedbackQuery::default()
    };
    loop {
        let page = store.list(&query).unwrap();
        seen.extend(page.items.iter().map(|item| item.id));
        match page.next_before {
            Some(before) => query.before = Some(before),
            None => break,
        }
    }
    let mut expected = ids;
    expected.reverse();
    assert_eq!(seen, expected);
}

#[test]
fn agents_resolve_but_only_people_verify_or_dismiss() {
    let (_dir, store) = store();
    let id = record(&store, "bmux", "copy fails", Vec::new());

    let resolved = store
        .set_status(
            &SetFeedbackStatusRequest {
                note: Some("Fixed by handling symlinks".into()),
                commits: vec!["abc123".into()],
                ..status(id, FeedbackStatus::AgentResolved, Actor::Agent)
            },
            5,
        )
        .unwrap();
    assert_eq!(resolved.status, FeedbackStatus::AgentResolved);
    let change = resolved.status_change.unwrap();
    assert_eq!(change.by, Actor::Agent);
    assert_eq!(change.commits, vec!["abc123".to_owned()]);

    assert!(matches!(
        store.set_status(&status(id, FeedbackStatus::Verified, Actor::Agent), 6),
        Err(FeedbackStoreError::Forbidden(_))
    ));
    assert!(matches!(
        store.set_status(&status(id, FeedbackStatus::Dismissed, Actor::Agent), 6),
        Err(FeedbackStoreError::Forbidden(_))
    ));

    let verified = store
        .set_status(&status(id, FeedbackStatus::Verified, Actor::User), 7)
        .unwrap();
    assert_eq!(verified.status, FeedbackStatus::Verified);
    assert!(
        matches!(
            store.set_status(&status(id, FeedbackStatus::Open, Actor::Agent), 8),
            Err(FeedbackStoreError::Forbidden(_))
        ),
        "agents cannot reopen what a person verified"
    );
    let reopened = store
        .set_status(&status(id, FeedbackStatus::Open, Actor::User), 9)
        .unwrap();
    assert_eq!(reopened.status, FeedbackStatus::Open);
}

#[test]
fn links_are_idempotent_and_reject_missing_or_self_targets() {
    let (_dir, store) = store();
    let a = record(&store, "bmux", "a", Vec::new());
    let b = record(&store, "bmux", "b", Vec::new());
    let link = LinkFeedbackRequest {
        from: b,
        to: a,
        kind: LinkKind::Duplicate,
    };
    store.link(&link, 1).unwrap();
    let item = store.link(&link, 2).unwrap();
    assert_eq!(item.links.len(), 1);
    assert_eq!(item.links[0].target, a);
    assert!(
        store
            .link(&LinkFeedbackRequest { to: b, ..link }, 3)
            .is_err()
    );
    assert!(matches!(
        store.link(
            &LinkFeedbackRequest {
                to: FeedbackId(404),
                ..link
            },
            3
        ),
        Err(FeedbackStoreError::NotFound(_))
    ));
}

#[test]
fn intake_session_claims_are_compare_and_set() {
    let (_dir, store) = store();
    assert_eq!(store.intake_session().unwrap().session_id, None);
    let first = store
        .claim_intake_session(&ClaimIntakeSessionRequest {
            expected: None,
            session_id: "s1".into(),
        })
        .unwrap();
    assert_eq!(first.session_id.as_deref(), Some("s1"));
    let lost = store
        .claim_intake_session(&ClaimIntakeSessionRequest {
            expected: None,
            session_id: "s2".into(),
        })
        .unwrap();
    assert_eq!(
        lost.session_id.as_deref(),
        Some("s1"),
        "a stale claim adopts the winner"
    );
    let replaced = store
        .claim_intake_session(&ClaimIntakeSessionRequest {
            expected: Some("s1".into()),
            session_id: "s3".into(),
        })
        .unwrap();
    assert_eq!(replaced.session_id.as_deref(), Some("s3"));
}

#[test]
fn unknown_schema_versions_are_refused_and_left_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = crate::database_path(dir.path());
    drop(FeedbackStore::open_at(&path).unwrap());
    {
        let database = crate::database::open(&path, std::time::Duration::from_secs(1)).unwrap();
        crate::database::block_on(async {
            use switchy_database::{Database as _, query::FilterableQuery as _};
            database
                .update("feedback_meta")
                .where_eq("id", 1)
                .value("version", 99)
                .execute(&database)
                .await
        })
        .unwrap();
    }
    assert!(matches!(
        FeedbackStore::open_at(&path),
        Err(FeedbackStoreError::UnsupportedSchema(99))
    ));
}

fn attach_request(
    id: FeedbackId,
    source: &std::path::Path,
    name: &str,
) -> bcode_feedback_models::AttachFeedbackRequest {
    bcode_feedback_models::AttachFeedbackRequest {
        id,
        source: source.to_string_lossy().into_owned(),
        name: name.into(),
        media_type: "application/x-bmux-recording".into(),
        description: Some("the pane around the feedback".into()),
        sensitive: false,
    }
}

#[test]
fn files_and_directory_trees_are_copied_into_the_store_and_read_back() {
    use bcode_feedback_models::AttachmentKind;
    let (dir, store) = store();
    let id = record(&store, "bmux", "flicker", Vec::new());
    let sources = tempfile::tempdir().unwrap();
    let file = sources.path().join("clip.txt");
    std::fs::write(&file, b"MARKER-B\n").unwrap();
    let tree = sources.path().join("cut");
    std::fs::create_dir_all(tree.join("nested")).unwrap();
    std::fs::write(tree.join("manifest.json"), b"{}").unwrap();
    std::fs::write(tree.join("nested").join("events_0.bin"), b"12345").unwrap();

    store
        .attach(&attach_request(id, &file, "clip.txt"), 10)
        .unwrap();
    let item = store.attach(&attach_request(id, &tree, "cut"), 11).unwrap();

    assert_eq!(item.attachments.len(), 2);
    let (text, cut) = (&item.attachments[0], &item.attachments[1]);
    assert_eq!(
        (text.ordinal, text.kind, text.bytes),
        (1, AttachmentKind::File, 9)
    );
    assert_eq!(
        (cut.ordinal, cut.kind, cut.bytes),
        (2, AttachmentKind::Directory, 7)
    );
    assert!(
        cut.path
            .starts_with(&dir.path().to_string_lossy().into_owned())
    );
    assert_eq!(std::fs::read(&text.path).unwrap(), b"MARKER-B\n");
    assert_eq!(
        std::fs::read(std::path::Path::new(&cut.path).join("nested/events_0.bin")).unwrap(),
        b"12345"
    );
    drop(sources);
    assert!(
        std::path::Path::new(&cut.path)
            .join("manifest.json")
            .exists(),
        "the store keeps its own copy after the source is gone"
    );
    assert_eq!(store.get(id).unwrap().attachments, item.attachments);
}

#[cfg(unix)]
#[test]
fn symbolic_links_and_missing_sources_are_refused_without_leaving_copies() {
    let (dir, store) = store();
    let id = record(&store, "bmux", "flicker", Vec::new());
    let sources = tempfile::tempdir().unwrap();
    let secret = sources.path().join("secret");
    std::fs::write(&secret, b"key").unwrap();
    let tree = sources.path().join("cut");
    std::fs::create_dir(&tree).unwrap();
    std::os::unix::fs::symlink(&secret, tree.join("link")).unwrap();
    let link = sources.path().join("direct-link");
    std::os::unix::fs::symlink(&secret, &link).unwrap();

    for (source, name) in [
        (tree.as_path(), "cut"),
        (link.as_path(), "link"),
        (sources.path().join("missing").as_path(), "missing"),
    ] {
        assert!(matches!(
            store.attach(&attach_request(id, source, name), 1),
            Err(FeedbackStoreError::AttachmentRejected(_))
        ));
    }
    assert_eq!(store.get(id).unwrap().attachments, Vec::new());
    let stored = dir.path().join("feedback/attachments").join(id.to_string());
    assert!(
        !stored.exists() || std::fs::read_dir(&stored).unwrap().next().is_none(),
        "nothing is left behind"
    );
}

#[test]
fn items_hold_a_bounded_number_of_attachments() {
    let (_dir, store) = store();
    let id = record(&store, "bmux", "flicker", Vec::new());
    let sources = tempfile::tempdir().unwrap();
    let file = sources.path().join("clip.txt");
    std::fs::write(&file, b"x").unwrap();
    for n in 0..bcode_feedback_models::MAX_ATTACHMENTS {
        store
            .attach(&attach_request(id, &file, &format!("clip-{n}.txt")), 1)
            .unwrap();
    }
    assert!(matches!(
        store.attach(&attach_request(id, &file, "one-more.txt"), 1),
        Err(FeedbackStoreError::AttachmentRejected(_))
    ));
}

fn submit(store: &FeedbackStore, text: &str, triage: TriageIntent, now: u64) -> FeedbackId {
    store
        .record_for_triage(
            &RecordForTriageRequest {
                item: RecordFeedbackRequest {
                    app: "bmux".into(),
                    text: text.into(),
                    facets: Vec::new(),
                },
                triage,
            },
            now,
        )
        .expect("record for triage")
        .id
}

fn requested() -> TriageIntent {
    TriageIntent::Requested {
        instructions: Some("bmux.capture_recording cuts the pane recording.".into()),
    }
}

fn due(holder: &str, ignore_retry_delay: bool) -> LeaseDueTriageRequest {
    LeaseDueTriageRequest {
        holder: holder.into(),
        lease_ms: 60_000,
        limit: 8,
        ignore_retry_delay,
    }
}

fn dispatch(store: &FeedbackStore, id: FeedbackId) -> TriageDispatch {
    store
        .get(id)
        .unwrap()
        .triage_dispatch
        .expect("triage request")
}

fn started(id: FeedbackId, holder: &str, generation: u32, turn: &str) -> TriageStartedRequest {
    TriageStartedRequest {
        id,
        holder: holder.into(),
        generation,
        session_id: "intake".into(),
        turn_id: turn.into(),
    }
}

fn failed(id: FeedbackId, holder: &str, generation: u32, retry: bool) -> TriageFailedRequest {
    TriageFailedRequest {
        id,
        holder: holder.into(),
        generation,
        reason: "intake session unavailable".into(),
        retry,
    }
}

fn ended(
    id: FeedbackId,
    generation: u32,
    turn: &str,
    ending: IntakeTurnEnding,
) -> TriageTurnEndedRequest {
    TriageTurnEndedRequest {
        id,
        generation,
        turn_id: turn.into(),
        ending,
        detail: "Completed".into(),
    }
}

#[test]
fn requested_triage_is_saved_with_the_item_and_skipped_triage_is_never_delivered() {
    let (_dir, store) = store();
    let queued = submit(&store, "the pane froze", requested(), 1_000);
    let skipped = submit(&store, "just noting", TriageIntent::Skipped, 1_000);
    let plain = record(&store, "bmux", "agent recorded", Vec::new());

    let request = dispatch(&store, queued);
    assert_eq!(request.state, TriageDispatchState::Queued);
    assert_eq!((request.generation, request.attempts), (1, 0));
    assert_eq!(request.next_attempt_at_ms, Some(1_000));
    assert_eq!(
        dispatch(&store, skipped).state,
        TriageDispatchState::Skipped
    );
    assert_eq!(store.get(plain).unwrap().triage_dispatch, None);

    let leases = store.lease_due_triage(&due("a", true), 1_000).unwrap();
    assert_eq!(
        leases.iter().map(|lease| lease.item.id).collect::<Vec<_>>(),
        vec![queued],
        "only the requested item is delivered"
    );
    assert_eq!(
        leases[0].instructions.as_deref(),
        Some("bmux.capture_recording cuts the pane recording.")
    );
    let lease = LeaseTriageRequest {
        id: skipped,
        holder: "a".into(),
        lease_ms: 60_000,
    };
    assert_eq!(store.lease_triage(&lease, 1_000).unwrap(), None);
}

#[test]
fn one_dispatcher_at_a_time_holds_an_item_until_its_lease_ends() {
    let (_dir, store) = store();
    let id = submit(&store, "flicker", requested(), 1_000);
    let lease = |holder: &str| LeaseTriageRequest {
        id,
        holder: holder.into(),
        lease_ms: 60_000,
    };
    assert!(
        store
            .lease_triage(&lease("bridge"), 1_000)
            .unwrap()
            .is_some()
    );
    assert_eq!(store.lease_triage(&lease("daemon"), 2_000).unwrap(), None);
    assert_eq!(
        store
            .lease_due_triage(&due("daemon", true), 2_000)
            .unwrap()
            .len(),
        0
    );
    // A dispatcher that died mid-delivery frees the item when its lease ends.
    let taken = store
        .lease_triage(&lease("daemon"), 61_001)
        .unwrap()
        .unwrap();
    assert_eq!(taken.generation, 1);
    // The previous holder can no longer record an outcome for it.
    assert!(matches!(
        store.triage_failed(&failed(id, "bridge", 1, true), 61_002),
        Err(FeedbackStoreError::Conflict(_))
    ));
}

#[test]
fn transient_failures_back_off_and_permanent_ones_block_with_their_reason() {
    let (_dir, store) = store();
    let id = submit(&store, "flicker", requested(), 1_000);
    assert_eq!(
        store
            .lease_due_triage(&due("a", false), 1_000)
            .unwrap()
            .len(),
        1
    );
    store
        .triage_failed(&failed(id, "a", 1, true), 1_000)
        .unwrap();
    let request = dispatch(&store, id);
    assert_eq!(request.state, TriageDispatchState::Queued);
    assert_eq!(request.attempts, 1);
    assert!(request.is_retrying());
    assert_eq!(request.next_attempt_at_ms, Some(1_000 + retry_delay_ms(1)));
    assert_eq!(
        request.last_error.as_deref(),
        Some("intake session unavailable")
    );

    // Not due before the delay, unless a just-started dispatcher ignores it.
    assert_eq!(
        store
            .lease_due_triage(&due("a", false), 2_000)
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        store
            .lease_due_triage(&due("a", true), 2_000)
            .unwrap()
            .len(),
        1
    );
    store
        .triage_failed(&failed(id, "a", 1, true), 2_000)
        .unwrap();
    assert_eq!(
        dispatch(&store, id).next_attempt_at_ms,
        Some(2_000 + retry_delay_ms(2))
    );
    assert!(retry_delay_ms(2) > retry_delay_ms(1));
    assert_eq!(
        retry_delay_ms(100),
        retry_delay_ms(30),
        "the delay is capped"
    );

    let later = 2_000 + retry_delay_ms(2);
    assert_eq!(
        store
            .lease_due_triage(&due("a", false), later)
            .unwrap()
            .len(),
        1
    );
    store
        .triage_failed(&failed(id, "a", 1, false), later)
        .unwrap();
    let request = dispatch(&store, id);
    assert_eq!(request.state, TriageDispatchState::Blocked);
    assert_eq!(
        request.blocked_reason.as_deref(),
        Some("intake session unavailable")
    );
    assert_eq!(
        store
            .lease_due_triage(&due("a", true), later)
            .unwrap()
            .len(),
        0
    );

    // A person asks again: delivery resumes with a fresh generation.
    store.request_triage(id, later + 1).unwrap();
    let request = dispatch(&store, id);
    assert_eq!(request.state, TriageDispatchState::Queued);
    assert_eq!((request.generation, request.attempts), (2, 0));
    assert_eq!(request.blocked_reason, None);
}

#[test]
fn repeated_starts_of_one_generation_are_idempotent_and_triage_completes_the_request() {
    let (_dir, store) = store();
    let id = submit(&store, "flicker", requested(), 1_000);
    let lease = store
        .lease_due_triage(&due("a", false), 1_000)
        .unwrap()
        .remove(0);
    assert_eq!(lease.idempotency_key(), "feedback:fb-1");
    store
        .triage_started(&started(id, "a", 1, "t1"), 1_000)
        .unwrap();
    // A second dispatcher that delivered the same generation (and got the same turn back)
    // records nothing new.
    store
        .triage_started(&started(id, "b", 1, "t1"), 1_100)
        .unwrap();
    let request = dispatch(&store, id);
    assert_eq!(request.state, TriageDispatchState::Started);
    assert_eq!(request.turn_id.as_deref(), Some("t1"));
    assert_eq!(
        store
            .started_triages(StartedTriageQuery { limit: 8 })
            .unwrap()
            .len(),
        1
    );
    assert!(matches!(
        store.triage_started(&started(id, "a", 2, "t2"), 1_200),
        Err(FeedbackStoreError::Conflict(_))
    ));

    store
        .triage(
            &TriageFeedbackRequest {
                id,
                triage: Triage {
                    title: "Pane flickers".into(),
                    ..Triage::default()
                },
            },
            2_000,
        )
        .unwrap();
    assert_eq!(dispatch(&store, id).state, TriageDispatchState::Completed);
    assert_eq!(
        store
            .started_triages(StartedTriageQuery { limit: 8 })
            .unwrap()
            .len(),
        0
    );
    // A late report that the turn ended changes nothing.
    store
        .triage_turn_ended(&ended(id, 1, "t1", IntakeTurnEnding::Untriaged), 3_000)
        .unwrap();
    assert_eq!(dispatch(&store, id).state, TriageDispatchState::Completed);
    assert!(matches!(
        store.request_triage(id, 3_000),
        Err(FeedbackStoreError::Forbidden(_))
    ));
}

#[test]
fn turns_that_end_without_triage_are_delivered_again_then_block() {
    let (_dir, store) = store();
    let id = submit(&store, "flicker", requested(), 1_000);
    let mut now = 1_000;
    // A failed turn (such as an unavailable provider) is delivered again as a new turn.
    store.lease_due_triage(&due("a", false), now).unwrap();
    store
        .triage_started(&started(id, "a", 1, "t1"), now)
        .unwrap();
    store
        .triage_turn_ended(&ended(id, 1, "t1", IntakeTurnEnding::Failed), now)
        .unwrap();
    let request = dispatch(&store, id);
    assert_eq!(request.state, TriageDispatchState::Queued);
    assert_eq!(request.generation, 2);
    // An end report for a turn that is no longer current is ignored.
    store
        .triage_turn_ended(&ended(id, 1, "t1", IntakeTurnEnding::Cancelled), now)
        .unwrap();
    assert_eq!(dispatch(&store, id).state, TriageDispatchState::Queued);

    for generation in 2..=MAX_TRIAGE_GENERATIONS {
        now += retry_delay_ms(10);
        let lease = store
            .lease_due_triage(&due("a", false), now)
            .unwrap()
            .remove(0);
        assert_eq!(lease.generation, generation);
        assert_eq!(
            lease.idempotency_key(),
            format!("feedback:fb-1:{generation}")
        );
        let turn = format!("t{generation}");
        store
            .triage_started(&started(id, "a", generation, &turn), now)
            .unwrap();
        store
            .triage_turn_ended(
                &ended(id, generation, &turn, IntakeTurnEnding::Untriaged),
                now,
            )
            .unwrap();
    }
    let request = dispatch(&store, id);
    assert_eq!(request.state, TriageDispatchState::Blocked);
    assert!(
        request
            .blocked_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("without triaging")),
        "{request:?}"
    );
}

#[test]
fn a_cancelled_intake_turn_blocks_delivery() {
    let (_dir, store) = store();
    let id = submit(&store, "flicker", requested(), 1_000);
    store.lease_due_triage(&due("a", false), 1_000).unwrap();
    store
        .triage_started(&started(id, "a", 1, "t1"), 1_000)
        .unwrap();
    store
        .triage_turn_ended(&ended(id, 1, "t1", IntakeTurnEnding::Cancelled), 2_000)
        .unwrap();
    assert_eq!(dispatch(&store, id).state, TriageDispatchState::Blocked);
}

#[test]
fn stores_written_before_triage_requests_gain_them_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = crate::database_path(dir.path());
    let legacy = {
        let store = FeedbackStore::open_at(&path).unwrap();
        record(&store, "bmux", "saved before triage requests", Vec::new())
    };
    {
        let database = crate::database::open(&path, std::time::Duration::from_secs(1)).unwrap();
        crate::database::block_on(
            switchy_database::schema::drop_table(crate::dispatch::TABLE).execute(&database),
        )
        .unwrap();
    }
    let store = FeedbackStore::open_at(&path).unwrap();
    assert_eq!(store.get(legacy).unwrap().triage_dispatch, None);
    let queued = store.request_triage(legacy, 5_000).unwrap();
    assert_eq!(
        queued.triage_dispatch.map(|dispatch| dispatch.state),
        Some(TriageDispatchState::Queued)
    );
}
