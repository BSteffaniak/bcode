use super::*;
use bcode_session_models::ModelTurnOutcome;
use std::time::Duration;

fn owner() -> FeedOwner {
    FeedOwner {
        build_fingerprint: "build-a".into(),
        artifact_id: Some("artifact-a".into()),
        protocol_version: 68,
        storage_writer_epoch: Some(28),
        daemon_instance_id: Some("instance-a".into()),
        pid: 42,
    }
}

fn event(session_id: SessionId, sequence: u64, kind: SessionEventKind) -> SessionEvent {
    SessionEvent {
        session_id,
        sequence,
        kind,
        schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
        timestamp_ms: 0,
        provenance: None,
    }
}

fn renamed(name: &str) -> SessionEventKind {
    SessionEventKind::SessionRenamed {
        name: Some(name.into()),
    }
}

async fn next(follower: &mut FeedFollower) -> FeedUpdate {
    tokio::time::timeout(Duration::from_secs(5), follower.next())
        .await
        .expect("an update arrives")
        .expect("the feed is watched")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_follower_sees_the_owner_and_each_committed_event_in_order() {
    let root = tempfile::tempdir().expect("root");
    let session = SessionId::new();
    let mut writer = FeedWriter::open(root.path(), session, owner()).expect("writer");
    writer
        .publish(&event(session, 0, renamed("one")))
        .expect("publish");
    let mut follower = FeedFollower::open(root.path(), session, None).expect("follower");
    let FeedUpdate::Marker(marker) = next(&mut follower).await else {
        panic!("the marker comes first");
    };
    assert!(marker.owning && !marker.busy && marker.readable());
    assert_eq!(marker.owner, owner());
    assert!(matches!(next(&mut follower).await, FeedUpdate::Event(event) if event.sequence == 0));
    // Published later: delivered by notification, without polling.
    writer
        .publish(&event(session, 1, renamed("two")))
        .expect("publish");
    assert!(matches!(next(&mut follower).await, FeedUpdate::Event(event) if event.sequence == 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn work_starting_and_stopping_rewrites_the_marker_and_release_ends_ownership() {
    let root = tempfile::tempdir().expect("root");
    let session = SessionId::new();
    let mut writer = FeedWriter::open(root.path(), session, owner()).expect("writer");
    writer
        .publish(&event(
            session,
            0,
            SessionEventKind::ModelTurnStarted {
                turn_id: "turn".into(),
            },
        ))
        .expect("publish");
    let marker = read_marker(root.path(), session)
        .expect("readable")
        .expect("marker");
    assert!(marker.busy);
    writer
        .publish(&event(
            session,
            1,
            SessionEventKind::ModelTurnFinished {
                turn_id: "turn".into(),
                outcome: ModelTurnOutcome::Completed,
                message: None,
            },
        ))
        .expect("publish");
    let idle = read_marker(root.path(), session)
        .expect("readable")
        .expect("marker");
    assert!(!idle.busy && idle.generation > marker.generation);
    writer.release().expect("release");
    let released = read_marker(root.path(), session)
        .expect("readable")
        .expect("marker");
    assert!(!released.owning && !released.busy);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seen_events_are_skipped_and_torn_or_damaged_lines_are_never_trusted() {
    let root = tempfile::tempdir().expect("root");
    let session = SessionId::new();
    let mut writer = FeedWriter::open(root.path(), session, owner()).expect("writer");
    for sequence in 0..3 {
        writer
            .publish(&event(session, sequence, renamed("name")))
            .expect("publish");
    }
    let path = feed_dir(root.path(), session).join(EVENTS_FILE);
    // A damaged record (checksum mismatch) and a torn tail (no newline yet).
    let mut file = OpenOptions::new().append(true).open(&path).expect("open");
    let json = serde_json::to_string(&event(session, 3, renamed("forged"))).expect("json");
    let damaged = Record {
        v: FEED_FORMAT_VERSION,
        seq: Some(3),
        sum: Some(checksum(b"something else")),
        event: Some(json),
        gap: false,
    };
    writeln!(file, "{}", serde_json::to_string(&damaged).expect("record")).expect("write");
    write!(file, "{{\"v\":1,\"seq\":4").expect("torn");
    drop(file);
    let mut follower = FeedFollower::open(root.path(), session, Some(1)).expect("follower");
    assert!(matches!(next(&mut follower).await, FeedUpdate::Marker(_)));
    assert!(matches!(next(&mut follower).await, FeedUpdate::Event(event) if event.sequence == 2));
    assert_eq!(next(&mut follower).await, FeedUpdate::Gap);
    // The torn tail is held until it completes; nothing more is delivered for it now.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), follower.next())
            .await
            .is_err()
    );
}

#[test]
fn a_newer_build_is_reported_not_guessed() {
    let root = tempfile::tempdir().expect("root");
    let session = SessionId::new();
    let dir = feed_dir(root.path(), session);
    fs::create_dir_all(&dir).expect("dir");
    fs::write(
        dir.join(MARKER_FILE),
        serde_json::json!({"format": FEED_FORMAT_VERSION + 1}).to_string(),
    )
    .expect("marker");
    assert_eq!(
        read_marker(root.path(), session),
        Err(FeedUnreadable::NewerBuild)
    );
    let mut marker = serde_json::to_value(ActivityMarker {
        format: FEED_FORMAT_VERSION,
        session_id: session,
        owner: owner(),
        owning: true,
        busy: true,
        generation: 0,
        event_schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION + 1,
        updated_at_ms: 0,
    })
    .expect("marker");
    fs::write(dir.join(MARKER_FILE), marker.to_string()).expect("marker");
    assert_eq!(
        read_marker(root.path(), session),
        Err(FeedUnreadable::NewerBuild)
    );
    marker["event_schema_version"] = CURRENT_SESSION_EVENT_SCHEMA_VERSION.into();
    fs::write(dir.join(MARKER_FILE), marker.to_string()).expect("marker");
    assert!(
        read_marker(root.path(), session)
            .expect("readable")
            .is_some()
    );
    assert_eq!(read_marker(root.path(), SessionId::new()), Ok(None));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rotated_feed_is_read_from_its_start_with_a_gap() {
    let root = tempfile::tempdir().expect("root");
    let session = SessionId::new();
    let mut writer = FeedWriter::open(root.path(), session, owner()).expect("writer");
    writer
        .publish(&event(session, 0, renamed("before")))
        .expect("publish");
    let mut follower = FeedFollower::open(root.path(), session, None).expect("follower");
    assert!(matches!(next(&mut follower).await, FeedUpdate::Marker(_)));
    assert!(matches!(next(&mut follower).await, FeedUpdate::Event(_)));
    writer.rotate().expect("rotate");
    writer
        .publish(&event(session, 1, renamed("after")))
        .expect("publish");
    assert_eq!(next(&mut follower).await, FeedUpdate::Gap);
    assert!(matches!(next(&mut follower).await, FeedUpdate::Event(event) if event.sequence == 1));
}
