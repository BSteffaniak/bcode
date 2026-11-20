use std::collections::BTreeSet;

use bcode_feedback_models::{
    Actor, ClaimIntakeSessionRequest, Facet, FeedbackId, FeedbackQuery, FeedbackStatus,
    LinkFeedbackRequest, LinkKind, RecordFeedbackRequest, SetFeedbackStatusRequest, Triage,
    TriageFeedbackRequest, TriageState,
};

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
