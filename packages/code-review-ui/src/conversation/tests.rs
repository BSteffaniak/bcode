//! Signals, reads, what is new, and what makes a toast.

use super::*;
use crate::config::conversation::OverviewMode;
use bcode_code_review_models::{Participant, PullState, RemoteThread, WatchMechanism};

fn key(number: u64) -> PullKey {
    PullKey {
        provider: "forge".to_owned(),
        repository: "octo/repo".to_owned(),
        number,
    }
}

fn person(login: &str) -> Participant {
    Participant {
        login: login.to_owned(),
        is_viewer: login == "me",
        ..Participant::default()
    }
}

fn comment(id: &str, login: &str, at: u64) -> RemoteComment {
    RemoteComment {
        id: id.to_owned(),
        author: person(login),
        body: format!("{login} says"),
        created_at_ms: Some(at),
        updated_at_ms: None,
        url: None,
        review_id: None,
    }
}

fn activity(number: u64) -> PullActivity {
    PullActivity {
        number,
        title: format!("#{number}"),
        head_oid: "h".to_owned(),
        state: PullState::Open,
        author: Some(person("me")),
        description: String::new(),
        url: None,
        decision: None,
        reviews: Vec::new(),
        requested: Vec::new(),
        threads: Vec::new(),
        comments: Vec::new(),
        checks: bcode_code_review_models::Checks::default(),
        viewer: bcode_code_review_models::ViewerAbilities {
            is_author: true,
            ..Default::default()
        },
        updated_at_ms: None,
    }
}

fn conversations(numbers: &[u64], watch: bool) -> Conversations {
    let mut conversations = Conversations {
        links: numbers
            .iter()
            .map(|number| LayerLink {
                source_id: format!("s{number}"),
                pull: key(*number),
                head_oid: "h".to_owned(),
                paths: BTreeSet::new(),
                title: String::new(),
            })
            .collect(),
        ..Conversations::default()
    };
    conversations.providers.insert(
        "forge".to_owned(),
        ProviderInfo {
            label: "Forge".to_owned(),
            capabilities: ConversationCapabilities {
                activity: true,
                watch: watch.then_some(WatchMechanism::Conditional),
                ..ConversationCapabilities::default()
            },
        },
    );
    conversations
}

#[test]
fn signals_within_the_gap_become_one_later_read() {
    let mut talk = conversations(&[1, 2], false);
    let gap = Duration::from_secs(60);
    assert_eq!(talk.signal([key(1)], gap), Next::Read(vec![key(1)]));
    // A read is in flight: later signals wait for it.
    assert_eq!(talk.signal([key(2)], gap), Next::Wait);
    talk.read(&[key(1)], Ok(vec![activity(1)]), false);
    // Within the gap of the last read: one delayed read, however many signals.
    assert!(matches!(talk.signal([key(1)], gap), Next::After(_)));
    assert_eq!(talk.signal([key(2)], gap), Next::Wait);
    talk.due();
    // With no gap the dirty pull requests are read together.
    match talk.next(Duration::ZERO) {
        Next::Read(keys) => assert_eq!(keys, vec![key(1), key(2)]),
        other => panic!("expected a read, got {other:?}"),
    }
}

#[test]
fn pull_requests_of_providers_without_conversations_are_never_read() {
    let mut talk = conversations(&[1], false);
    talk.providers
        .get_mut("forge")
        .expect("provider")
        .capabilities
        .activity = false;
    assert_eq!(talk.signal([key(1)], Duration::ZERO), Next::Wait);
    assert_eq!(talk.readable(), Vec::<PullKey>::new());
}

#[test]
fn a_second_read_reports_what_arrived_from_others() {
    let mut talk = conversations(&[1], false);
    let mut first = activity(1);
    first.comments = vec![comment("c1", "ana", 10)];
    assert_eq!(
        talk.read(&[key(1)], Ok(vec![first.clone()]), false),
        Vec::new()
    );
    let mut second = first;
    second.comments.push(comment("c2", "me", 20));
    second.comments.push(comment("c3", "raj", 30));
    second.threads.push(RemoteThread {
        id: "t".to_owned(),
        anchor: bcode_code_review_models::RemoteAnchor::default(),
        resolved: false,
        outdated: false,
        can_reply: true,
        can_resolve: true,
        comments: vec![comment("t1", "ana", 40)],
    });
    second.reviews.push(bcode_code_review_models::PullReview {
        id: "r".to_owned(),
        author: person("li"),
        state: ReviewState::Approved,
        body: String::new(),
        at_ms: Some(50),
        commit_oid: None,
        url: None,
    });
    let arrivals: Vec<String> = talk
        .read(&[key(1)], Ok(vec![second]), false)
        .iter()
        .map(|(key, arrival)| arrival_text(key, arrival))
        .collect();
    assert_eq!(
        arrivals,
        vec![
            "li approved #1".to_owned(),
            "ana replied in a thread on #1".to_owned(),
            "raj commented on #1".to_owned(),
        ]
    );
}

#[test]
fn what_is_new_is_what_others_did_after_the_viewer_looked() {
    let mut pull = activity(1);
    pull.comments = vec![comment("a", "ana", 10), comment("b", "me", 30)];
    // Never looked: the starting point, not news.
    assert!(!is_new(&pull, None));
    assert!(!is_new(&pull, Some(10)));
    assert!(is_new(&pull, Some(5)));
    assert_eq!(newest_from_others(&pull), 10);
    let mut talk = conversations(&[1], false);
    talk.read(&[key(1)], Ok(vec![pull]), false);
    assert!(talk.overview_open(&key(1), OverviewMode::Auto, Some(5)));
    assert!(!talk.overview_open(&key(1), OverviewMode::Auto, Some(10)));
    assert!(!talk.overview_open(&key(1), OverviewMode::Collapsed, Some(5)));
    assert!(talk.overview_open(&key(1), OverviewMode::Expanded, Some(10)));
    // The viewer's own choice wins.
    talk.opened.insert(key(1), false);
    assert!(!talk.overview_open(&key(1), OverviewMode::Expanded, Some(10)));
}

#[test]
fn the_notify_setting_chooses_which_arrivals_make_a_toast() {
    let review = Arrival::Review(bcode_code_review_models::PullReview {
        id: "r".to_owned(),
        author: person("ana"),
        state: ReviewState::ChangesRequested,
        body: String::new(),
        at_ms: None,
        commit_oid: None,
        url: None,
    });
    let mine = Arrival::Comment {
        comment: comment("c", "ana", 1),
        thread: Some("t".to_owned()),
        involves_viewer: true,
    };
    let others = Arrival::Comment {
        comment: comment("d", "ana", 1),
        thread: None,
        involves_viewer: false,
    };
    let shown = |mode| {
        [&review, &mine, &others]
            .iter()
            .map(|arrival| notifies(mode, arrival))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        shown(NotifyMode::ReviewsAndReplies),
        vec![true, true, false]
    );
    assert_eq!(shown(NotifyMode::All), vec![true, true, true]);
    assert_eq!(shown(NotifyMode::Reviews), vec![true, false, false]);
    assert_eq!(shown(NotifyMode::None), vec![false, false, false]);
}

#[test]
fn watches_start_once_per_repository_and_not_while_unfocused() {
    let mut talk = conversations(&[1, 2], true);
    talk.unfocused = true;
    assert_eq!(talk.watches_to_start(true), Vec::new());
    talk.unfocused = false;
    let started = talk.watches_to_start(true);
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].numbers, vec![1, 2]);
    // Running: not started again until it answers.
    assert_eq!(talk.watches_to_start(true), Vec::new());
    let changed = talk.watched(
        "forge",
        "octo/repo",
        Ok(bcode_code_review_models::ReviewSourcePullsWatchResponse {
            changed: vec![2],
            cursor: "c1".to_owned(),
        }),
    );
    assert_eq!(changed, vec![key(2)]);
    let again = talk.watches_to_start(true);
    assert_eq!(again[0].cursor.as_deref(), Some("c1"));
    assert_eq!(
        talk.watched("forge", "octo/repo", Err("down".to_owned())),
        Vec::new()
    );
    assert_eq!(talk.watches_to_start(true)[0].failures, 1);
    assert!(watch_backoff(1) > watch_backoff(0));
}

fn snapshot(threads: &[&str], read_at_ms: u64) -> ThreadsSnapshot {
    let template = bcode_code_review_models::PullThreadRecord {
        id: String::new(),
        provider: "forge".to_owned(),
        provider_label: "Forge".to_owned(),
        repository: "octo/repo".to_owned(),
        number: 1,
        location: "the review".to_owned(),
        placement: bcode_code_review_models::ThreadPlacement::Exact,
        thread: RemoteThread {
            id: "t".to_owned(),
            anchor: bcode_code_review_models::RemoteAnchor::default(),
            resolved: false,
            outdated: false,
            can_reply: false,
            can_resolve: false,
            comments: Vec::new(),
        },
    };
    ThreadsSnapshot {
        scope: crate::notes::NoteScope {
            repo_path: PathBuf::from("/repo"),
            target: bcode_code_review_models::ReviewTarget::LastCommit,
            scope: None,
        },
        threads: threads
            .iter()
            .map(|id| {
                let mut record = template.clone();
                (*id).clone_into(&mut record.id);
                record
            })
            .collect(),
        read_at_ms,
    }
}

#[test]
fn thread_saves_run_one_at_a_time_and_the_newest_waits() {
    let mut saves = ThreadSaves::default();
    let cached = snapshot(&["a"], 1_000);
    assert_eq!(saves.offer(cached.clone()), Some(cached));
    // Read from the forge while the cached one is being saved: it waits, and only the newest
    // waits.
    assert_eq!(saves.offer(snapshot(&["a", "b"], 2_000)), None);
    let newest = snapshot(&["a", "b", "c"], 3_000);
    assert_eq!(saves.offer(newest.clone()), None);
    assert_eq!(saves.finished(true), Some(newest.clone()), "saved after");
    assert!(saves.is_saving());
    assert_eq!(saves.finished(true), None);
    assert!(!saves.is_saving());
    // The same threads again are not saved again.
    assert_eq!(saves.offer(newest), None);
}

#[test]
fn a_failed_thread_save_is_tried_again_on_the_next_offer() {
    let mut saves = ThreadSaves::default();
    let threads = snapshot(&["a"], 1_000);
    assert!(saves.offer(threads.clone()).is_some());
    assert_eq!(saves.finished(false), None);
    assert_eq!(saves.offer(threads.clone()), Some(threads));
}

#[test]
fn unchanged_threads_are_saved_again_once_their_read_time_is_stale() {
    let mut saves = ThreadSaves::default();
    assert!(saves.offer(snapshot(&["a"], 1_000)).is_some());
    saves.finished(true);
    assert_eq!(
        saves.offer(snapshot(&["a"], 1_000 + SNAPSHOT_REFRESH_MS - 1)),
        None
    );
    let read_again = snapshot(&["a"], 1_000 + SNAPSHOT_REFRESH_MS);
    assert_eq!(saves.offer(read_again.clone()), Some(read_again));
}

#[test]
fn conversations_are_as_fresh_as_their_oldest_read_and_the_cache_keeps_its_time() {
    let mut conversations = Conversations::default();
    assert_eq!(conversations.read_at_ms(), None);
    let then = std::time::UNIX_EPOCH + Duration::from_secs(1_000);
    conversations.read_cached(vec![
        (key(1), activity(1), then),
        (key(2), activity(2), then),
    ]);
    assert_eq!(conversations.read_at_ms(), Some(1_000_000));
    // Read from the provider: newer, but the snapshot is as old as its oldest pull request.
    conversations.read(&[key(1)], Ok(vec![activity(1)]), false);
    assert_eq!(conversations.read_at_ms(), Some(1_000_000));
    conversations.read(&[key(2)], Ok(vec![activity(2)]), false);
    assert!(conversations.read_at_ms() > Some(1_000_000));
    // The cache answering late does not make a read conversation older.
    conversations.read_cached(vec![(key(1), activity(1), then)]);
    assert!(conversations.read_at_ms() > Some(1_000_000));
}
