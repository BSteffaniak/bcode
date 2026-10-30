// Transactional permission decisions, including failures at every commit boundary.
use super::*;

fn resolutions(id: SessionId, approved: bool) -> Vec<(SessionEvent, Option<u64>)> {
    ["first", "second"]
        .into_iter()
        .enumerate()
        .map(|(index, permission_id)| {
            (
                event(
                    id,
                    u64::try_from(index).unwrap() + 1,
                    SessionEventKind::PermissionResolved {
                        permission_id: permission_id.into(),
                        approved,
                    },
                ),
                Some(10),
            )
        })
        .collect()
}

async fn initialize(root: &std::path::Path, id: SessionId) -> SessionDb {
    let db = SessionDb::open_turso_in_root(id, root).await.unwrap();
    db.append_event(&event(
        id,
        0,
        SessionEventKind::SessionCreated {
            name: Some("permission batch".into()),
            working_directory: root.into(),
        },
    ))
    .await
    .unwrap();
    db
}

#[tokio::test]
async fn permission_batch_failure_rolls_back_each_written_prefix_and_reopens() {
    for (approved, fail_after) in [(true, 1), (true, 2), (false, 1), (false, 2)] {
        let root = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let db = initialize(root.path(), id).await;
        let result = db
            .append_event_batch_checked(&resolutions(id, approved), |written| {
                if written == fail_after {
                    Err(SessionDbError::InvalidData(
                        "injected permission commit failure".into(),
                    ))
                } else {
                    Ok(())
                }
            })
            .await;
        assert!(result.is_err());
        assert_eq!(db.all_events_strict().await.unwrap().len(), 1);
        validate_all_projection_checkpoints_at_tail(db.database(), Some(0))
            .await
            .unwrap();
        db.database().close().await.unwrap();
        drop(db);
        let db = SessionDb::open_existing_turso_in_root(id, root.path())
            .await
            .unwrap();
        assert_eq!(db.all_events_strict().await.unwrap().len(), 1);
        db.append_event_batch(&resolutions(id, approved))
            .await
            .unwrap();
        assert_eq!(db.all_events_strict().await.unwrap().len(), 3);
        validate_all_projection_checkpoints_at_tail(db.database(), Some(2))
            .await
            .unwrap();
        db.database().close().await.unwrap();
    }
}

// Invoked only by the crash-parent test. Exit bypasses transaction drop/rollback.
#[tokio::test]
async fn permission_batch_crash_child() {
    let Ok(root) = std::env::var("BCODE_PERMISSION_BATCH_CRASH_ROOT") else {
        return;
    };
    let id = std::env::var("BCODE_PERMISSION_BATCH_CRASH_ID")
        .unwrap()
        .parse()
        .unwrap();
    let phase: usize = std::env::var("BCODE_PERMISSION_BATCH_CRASH_PHASE")
        .unwrap()
        .parse()
        .unwrap();
    let approved = std::env::var("BCODE_PERMISSION_BATCH_CRASH_APPROVED").unwrap() == "true";
    let db = SessionDb::open_existing_turso_in_root(id, std::path::Path::new(&root))
        .await
        .unwrap();
    db.append_event_batch_checked(&resolutions(id, approved), |written| {
        if phase == written {
            std::process::exit(94);
        }
        Ok(())
    })
    .await
    .unwrap();
    // Phase 3 is after canonical commit but before response/waiter publication.
    std::process::exit(94);
}

#[tokio::test]
async fn permission_batch_process_crash_never_leaves_decided_prefix() {
    for (approved, phase) in [
        (true, 1),
        (true, 2),
        (true, 3),
        (false, 1),
        (false, 2),
        (false, 3),
    ] {
        let root = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let db = initialize(root.path(), id).await;
        db.database().close().await.unwrap();
        drop(db);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "db::tests::permission_batch_tests::permission_batch_crash_child",
            ])
            .env("BCODE_PERMISSION_BATCH_CRASH_ROOT", root.path())
            .env("BCODE_PERMISSION_BATCH_CRASH_ID", id.to_string())
            .env("BCODE_PERMISSION_BATCH_CRASH_PHASE", phase.to_string())
            .env(
                "BCODE_PERMISSION_BATCH_CRASH_APPROVED",
                approved.to_string(),
            )
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(94));
        let db = SessionDb::open_existing_turso_in_root(id, root.path())
            .await
            .unwrap();
        let history = db.all_events_strict().await.unwrap();
        let tail = if phase == 3 { 2 } else { 0 };
        assert_eq!(history.len(), usize::try_from(tail).unwrap() + 1);
        validate_all_projection_checkpoints_at_tail(db.database(), Some(tail))
            .await
            .unwrap();
        if phase != 3 {
            db.append_event_batch(&resolutions(id, approved))
                .await
                .unwrap();
        }
        let history = db.all_events_strict().await.unwrap();
        assert_eq!(history.len(), 3);
        for event in &history[1..] {
            assert!(
                matches!(event.kind, SessionEventKind::PermissionResolved { approved: value, .. } if value == approved)
            );
        }
        validate_all_projection_checkpoints_at_tail(db.database(), Some(2))
            .await
            .unwrap();
        db.database().close().await.unwrap();
    }
}
